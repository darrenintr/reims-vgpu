#!/usr/bin/env bash
# Builds RADV with amdgpu user queues (Mesa MR !40808, a draft) for an experiment on a Linux
# host with an RDNA3 GPU, and a control RADV from the same base without them, each into a
# prefix of its own under ~/.cache/mesa-userq. Nothing is installed system-wide: a VM takes one
# of them through VK_DRIVER_FILES. It also downloads the MR's kernel patches, which it does not
# apply (.claude/skills/userq-experiment/SKILL.md says what to do with them).
#
#   scripts/build-userq-mesa.sh            # both builds
#   scripts/build-userq-mesa.sh userq      # one of them: userq or control
#
# The two builds share a base (Mesa 26.2.0-devel, May 2026), older than the 26.2.4 from which
# metal2vulkan stands an RDNA3 dGPU in for the measured Radeon 780M (docs/qemu.md, "the Radeon
# RX 7600 and other RDNA3 GPUs"): under either build the GPU takes the runtime's exact paths.
# Compare the user-queue build with the control, never with the system's Mesa.
#
# Environment: MESA_USERQ_DIR (default ~/.cache/mesa-userq), JOBS (default nproc),
# EXTRA_MESON_ARGS (appended to meson setup).
set -euo pipefail

# The MR's head and the commit it is based on (the parent of its first commit), 2026-10-15.
USERQ_REPO=https://gitlab.freedesktop.org/openglfreak/mesa.git
USERQ_REV=05129e6f98e5a139504bd5044e537820ab1a255a
CONTROL_REV=b8fa4fd2581166e5fcbaef7471dad11556572d4f
# The kernel patches the MR's description links (on top of Linux 7.1-rc4).
KERNEL_PATCHES_URL=https://gitlab.freedesktop.org/-/project/176/uploads/2e872afb84551b14042e8f77f8e4afd5/userq-kernel-patches.tar.gz
# The libdrm_amdgpu the base's meson.build asks for.
LIBDRM_MIN=2.4.133
LIBDRM_REPO=https://gitlab.freedesktop.org/mesa/libdrm.git

DIR=${MESA_USERQ_DIR:-$HOME/.cache/mesa-userq}
JOBS=${JOBS:-$(nproc)}
WHICH=${1:-both}

say() { printf '\n== %s\n' "$*"; }
die() { printf 'build-userq-mesa: %s\n' "$*" >&2; exit 1; }

case "$WHICH" in
    both | userq | control) ;;
    *) die "usage: $0 [userq|control]" ;;
esac
[ "$(uname -s)" = Linux ] || die "a Linux host only"

say "Checking tools"
missing=()
for tool in git meson ninja pkg-config python3 glslangValidator bison flex curl; do
    command -v "$tool" >/dev/null || missing+=("$tool")
done
python3 -c 'import mako, yaml' 2>/dev/null || missing+=("python3-mako python3-yaml")
[ ${#missing[@]} -eq 0 ] || die "missing: ${missing[*]} (Ubuntu: sudo apt install meson ninja-build \
pkg-config glslang-tools bison flex python3-mako python3-yaml libxcb-dri3-dev libxcb-present-dev \
libxshmfence-dev libx11-xcb-dev libxrandr-dev libwayland-dev wayland-protocols libelf-dev zlib1g-dev \
libexpat1-dev libzstd-dev libudev-dev; Fedora: sudo dnf builddep mesa-vulkan-drivers)"
meson_version=$(meson --version)
[ "$(printf '%s\n1.4.0\n' "$meson_version" | sort -V | head -1)" = 1.4.0 ] ||
    die "meson $meson_version is older than 1.4.0 (pip install --user meson)"

mkdir -p "$DIR"
cd "$DIR"

say "libdrm_amdgpu >= $LIBDRM_MIN"
export PKG_CONFIG_PATH="$DIR/libdrm/install/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
have=$(pkg-config --modversion libdrm_amdgpu 2>/dev/null || echo 0)
if [ "$(printf '%s\n%s\n' "$have" "$LIBDRM_MIN" | sort -V | head -1)" != "$LIBDRM_MIN" ]; then
    echo "found $have: building libdrm into $DIR/libdrm/install"
    [ -d libdrm/src ] || git clone --depth 1 "$LIBDRM_REPO" libdrm/src
    meson setup --wipe libdrm/build libdrm/src --prefix="$DIR/libdrm/install" --libdir=lib \
        -Dbuildtype=release -Damdgpu=enabled -Dradeon=disabled -Dintel=disabled \
        -Dnouveau=disabled -Dvmwgfx=disabled -Dtests=false -Dman-pages=disabled
    ninja -C libdrm/build -j "$JOBS" install
    have=$(pkg-config --modversion libdrm_amdgpu)
fi
echo "libdrm_amdgpu $have"

say "Mesa sources"
if [ ! -d src/.git ]; then
    git init -q src
    git -C src remote add origin "$USERQ_REPO"
fi
for rev in "$USERQ_REV" "$CONTROL_REV"; do
    git -C src cat-file -e "$rev^{commit}" 2>/dev/null ||
        git -C src fetch --depth 1 origin "$rev" ||
        git -C src fetch origin feat/radv-userq
done
git -C src cat-file -e "$CONTROL_REV^{commit}" 2>/dev/null ||
    git -C src fetch --deepen 40 origin "$USERQ_REV"

build() {
    local name=$1 rev=$2
    say "Mesa $name ($rev)"
    local tree="$DIR/$name/src" prefix="$DIR/$name/install"
    if [ -d "$tree" ]; then
        git -C src worktree remove --force "$tree" 2>/dev/null || rm -rf "$tree"
    fi
    git -C src worktree prune
    git -C src worktree add --detach "$tree" "$rev"
    # shellcheck disable=SC2086 # EXTRA_MESON_ARGS is a list of words.
    meson setup --wipe "$DIR/$name/build" "$tree" --prefix="$prefix" --libdir=lib \
        -Dbuildtype=release -Dvulkan-drivers=amd -Dgallium-drivers= -Dplatforms=x11,wayland \
        -Dllvm=disabled -Dvideo-codecs= -Dglx=disabled -Degl=disabled -Dgbm=disabled \
        -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Dvulkan-layers= -Dtools= \
        -Dbuild-tests=false ${EXTRA_MESON_ARGS:-}
    ninja -C "$DIR/$name/build" -j "$JOBS" install
    local icd
    icd=$(ls "$prefix"/share/vulkan/icd.d/radeon_icd.*.json)
    echo "$icd" >"$DIR/$name/icd"
    echo "ICD: $icd"
}

[ "$WHICH" = control ] || build userq "$USERQ_REV"
[ "$WHICH" = userq ] || build control "$CONTROL_REV"

say "Kernel patches"
mkdir -p kernel
[ -s kernel/userq-kernel-patches.tar.gz ] ||
    curl -fsSL -o kernel/userq-kernel-patches.tar.gz "$KERNEL_PATCHES_URL"
echo "$DIR/kernel/userq-kernel-patches.tar.gz (not applied: the skill says how)"

say "The kernel's user queues now"
param=/sys/module/amdgpu/parameters/user_queue
if [ -r "$param" ]; then
    echo "amdgpu user_queue = $(cat "$param") (1 or 2: on; -1 or 0: off for this GPU)"
else
    echo "amdgpu has no user_queue parameter: this kernel has no user queues"
fi

say "Done"
for name in userq control; do
    if [ -s "$DIR/$name/icd" ]; then
        echo "$name: reims-ctl set <vm> --env VK_DRIVER_FILES=$(cat "$DIR/$name/icd")"
    fi
done
