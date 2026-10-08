#!/usr/bin/env bash
# Bundle a built Linux QEMU with its runtime libraries, ROM data, and shader tools.
# Reims itself is a Rust staticlib linked into QEMU, not a separate .so.
set -euo pipefail

die() { echo "[qemu-package] ERROR: $*" >&2; exit 1; }
repo_root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$repo_root"

qemu="$repo_root/vendor/qemu/build/qemu-system-x86_64"
gop="$repo_root/crates/reims-vgpu-efi/out/reims-vgpu-gop.rom"
[ -x "$qemu" ] || die "Build QEMU first with scripts/qemu-build/qemu-build.sh --target x86_64 --backend vulkan"
[ -s "$gop" ] || die "Build GOP first with crates/reims-vgpu-efi/scripts/reims-vgpu-efi-rom/reims-vgpu-efi-rom.sh"
command -v zstd >/dev/null || die "zstd missing"
command -v ldconfig >/dev/null || die "ldconfig missing"

llvm_dis="$(command -v llvm-dis || true)"
if [ -z "$llvm_dis" ]; then
  llvm_dis="$(find /usr/bin -maxdepth 1 -name 'llvm-dis-[0-9]*' -print | sort -V | tail -n 1)"
fi
[ -n "$llvm_dis" ] && [ -x "$llvm_dis" ] || die "llvm-dis missing (install llvm)"
spirv_val="$(command -v spirv-val || true)"
[ -n "$spirv_val" ] && [ -x "$spirv_val" ] || die "spirv-val missing (install spirv-tools)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Install rather than copy only build/qemu-system-x86_64: the install supplies
# EFI option ROMs and firmware normally missing from a copied binary.
echo "[qemu-package] Installing QEMU into temporary DESTDIR ..."
DESTDIR="$work/install" ninja -C vendor/qemu/build install
installed_qemu="$(find "$work/install" -type f -name qemu-system-x86_64 -print -quit)"
efi_virtio="$(find "$work/install" -type f -name efi-virtio.rom -print -quit)"
[ -n "$installed_qemu" ] || die "QEMU install did not produce a binary"
[ -n "$efi_virtio" ] || die "QEMU install did not include efi-virtio.rom"

dist="$repo_root/dist"
bundle="$dist/reims-qemu-linux-x86_64"
mkdir -p "$dist"
rm -rf "$bundle"
mkdir -p "$bundle/bin" "$bundle/lib" "$bundle/share/qemu" "$bundle/firmware" "$bundle/licenses"
cp -L "$installed_qemu" "$bundle/bin/qemu-system-x86_64.real"
cp -a "$(dirname "$efi_virtio")/." "$bundle/share/qemu/"
cp -L "$llvm_dis" "$bundle/bin/llvm-dis"
cp -L "$spirv_val" "$bundle/bin/spirv-val"
cp -L "$gop" "$bundle/firmware/reims-vgpu-gop.rom"
cp -L LICENSE "$bundle/licenses/reims-vgpu-LICENSE"
cp -L vendor/qemu/COPYING "$bundle/licenses/qemu-COPYING"

# Do not ship glibc, the loader, libstdc++ or the host's graphics stack. The
# host must provide a Vulkan ICD + compatible Mesa/NVIDIA driver and libvulkan.
copy_lib() {
  local soname="$1" path="$2"
  case "$soname" in
    libc.so.*|libm.so.*|libpthread.so.*|libdl.so.*|librt.so.*|libresolv.so.*|\
    libutil.so.*|libanl.so.*|ld-linux-*.so.*|libgcc_s.so.*|libstdc++.so.*|\
    libGL*.so.*|libEGL.so.*|libvulkan.so.*|libdrm*.so.*|libgbm.so.*)
      return ;;
  esac
  [ -f "$path" ] || die "Missing shared library: $soname ($path)"
  if [ ! -e "$bundle/lib/$soname" ]; then
    cp -L "$path" "$bundle/lib/$soname"
  fi
}

collect_deps() {
  local elf="$1" deps
  deps="$(LC_ALL=C ldd "$elf" 2>&1)" || die "ldd failed for $elf: $deps"
  if grep -q 'not found' <<<"$deps"; then
    echo "$deps" >&2
    die "Unresolved dynamic dependencies for $elf"
  fi
  while IFS=$'\t' read -r soname path; do
    [ -n "$soname" ] || continue
    copy_lib "$soname" "$path"
  done < <(awk '$2 == "=>" && $3 ~ /^\// {print $1 "\t" $3}' <<<"$deps")
}

collect_deps "$bundle/bin/qemu-system-x86_64.real"
collect_deps "$bundle/bin/llvm-dis"
collect_deps "$bundle/bin/spirv-val"

# winit loads these via dlopen, so they may not be in QEMU's ldd output.
for soname in \
  libwayland-client.so.0 libwayland-cursor.so.0 libwayland-egl.so.1 \
  libxkbcommon.so.0 libxkbcommon-x11.so.0 \
  libX11.so.6 libX11-xcb.so.1 libXcursor.so.1 libXi.so.6 \
  libXrandr.so.2 libXinerama.so.1 libXext.so.6 libXfixes.so.3 \
  libxcb.so.1 libxcb-randr.so.0 libxcb-xfixes.so.0; do
  path="$(ldconfig -p | awk -v wanted="$soname" '$1 == wanted && /x86-64/ {if (!found) found=$NF} END {if (found) print found}')"
  if [ -n "$path" ]; then
    copy_lib "$soname" "$path"
    collect_deps "$path"
  fi
done

cat > "$bundle/bin/qemu-system-x86_64" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd -P)"
export LD_LIBRARY_PATH="$here/../lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export PATH="$here:$PATH"
# Resolve installed QEMU ROMs relative to the bundle, not the original build.
exec "$here/qemu-system-x86_64.real" -L "$here/../share/qemu" "$@"
SH
chmod 755 "$bundle/bin/qemu-system-x86_64"

cat > "$bundle/README.txt" <<'EOF'
Reims-vGPU QEMU: Linux x86_64 + Vulkan + linked Rust host-window backend

bin/qemu-system-x86_64       Wrapper to execute (not the .real ELF)
bin/qemu-system-x86_64.real  Compiled QEMU with reims-vgpu-pci
bin/llvm-dis                 Shader translator tool
bin/spirv-val                Shader validation tool
lib/                         Non-graphics runtime shared libraries
share/qemu/                  QEMU BIOS/EFI ROMs and installed data
firmware/reims-vgpu-gop.rom  Reims PCI UEFI GOP option ROM

Example, with the snapshot-aware VM launcher in the same repository:
  QEMU_BIN=/absolute/path/to/reims-qemu-linux-x86_64/bin/qemu-system-x86_64 \
    vm/boot-x86.sh --interactive --device reims-vgpu-pci --rail macos-13

The VM launcher detects the GOP ROM and included shader tools automatically.
For another QEMU launcher use the wrapper, add its bin/ to PATH if required,
and attach firmware/reims-vgpu-gop.rom to the reims-vgpu-pci device.

Host requirements: Linux x86_64 compatible with Ubuntu 24.04+ glibc, KVM
permission, Vulkan-capable GPU and ICD/driver, libvulkan.so.1, and X11/Wayland.
GPU drivers, macOS disks, OpenCore and private OVMF vars are NOT included.
Compiled-QEMU/device/audio/display smoke checks are NOT a live macOS boot test.
EOF

{
  echo "Reims source commit: $(git rev-parse HEAD)"
  echo "Pinned QEMU source commit: $(git -C vendor/qemu rev-parse HEAD)"
  echo "QEMU: $("$bundle/bin/qemu-system-x86_64" --version | head -n 1)"
  echo "Rust compiler: $(rustc --version)"
  echo "Build host: $(uname -a)"
  echo "Backend: Vulkan + host-window; Reims Rust staticlib linked into QEMU"
} > "$bundle/build-info.txt"

echo "[qemu-package] Smoke-testing bundled runtime ..."
LD_LIBRARY_PATH="$bundle/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  ldd "$bundle/bin/qemu-system-x86_64.real" > "$work/ldd.txt"
if grep -q 'not found' "$work/ldd.txt"; then cat "$work/ldd.txt" >&2; die "Missing runtime library"; fi
"$bundle/bin/qemu-system-x86_64" --version
"$bundle/bin/qemu-system-x86_64" -device reims-vgpu-pci,help > "$work/device.txt" 2>&1 || {
  cat "$work/device.txt" >&2
  die "Reims PCI device missing"
}
"$bundle/bin/qemu-system-x86_64" -accel help | grep -qi kvm || die "KVM support missing"
"$bundle/bin/qemu-system-x86_64" -audiodev help > "$work/audio.txt"
for driver in pipewire pa alsa sdl; do
  grep -Eq "(^|[[:space:]])$driver([[:space:]]|$)" "$work/audio.txt" || {
    cat "$work/audio.txt" >&2
    die "QEMU audio driver $driver was not built"
  }
done
"$bundle/bin/qemu-system-x86_64" -display help > "$work/display.txt"
for backend in gtk sdl; do
  grep -Eq "(^|[[:space:]])$backend([[:space:]]|$)" "$work/display.txt" || {
    cat "$work/display.txt" >&2
    die "QEMU display backend $backend was not built"
  }
done

(cd "$bundle" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
tar -C "$dist" -I zstd -cf "$dist/reims-qemu-linux-x86_64.tar.zst" reims-qemu-linux-x86_64
sha256sum "$dist/reims-qemu-linux-x86_64.tar.zst"
echo "[qemu-package] Ready: $dist/reims-qemu-linux-x86_64.tar.zst"
