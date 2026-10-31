# GitHub-managed Linux releases (experimental)

Use the GitHub Release bundle on Ubuntu x86_64 without keeping source trees, Rust toolchains or QEMU build intermediates. This feature remains experimental until CI and RX 7600 boot tests pass.

## Channels

- **stable**: newest non-prerelease release tagged `v*`. Pushing master does not create a stable release; a version tag does.
- **testing**: newest prerelease tagged `testing-<run>-<commit>`, built from development or feature/github-release-binaries.

Both channels publish `reims-vgpu-linux-x86_64.tar.gz` and `SHA256SUMS`. The archive contains the installed QEMU prefix, `bin/qemu-system-x86_64`, `bin/qemu-img`, `bin/reims-ctl`, `ReimsVgpu.rom`, its other QEMU runtime assets, and `BUILD-INFO.txt`.

## Usage on Linux x86_64

While this experimental branch has not merged, download the manager from the branch (inspect it before running):

```sh
mkdir -p ~/.local/bin
curl -fL https://raw.githubusercontent.com/darrenintr/reims-vgpu/feature/github-release-binaries/scripts/reims-release.py -o ~/.local/bin/reims-release
chmod 755 ~/.local/bin/reims-release
~/.local/bin/reims-release
```

Once the branch is merged, the raw URL can change to master.

```sh
reims-release --channel testing --install-only
reims-release --channel stable -- start mac
reims-release --channel testing -- start mac
reims-release --channel stable --no-check -- start mac
```

At interactive startup the manager asks which channel to use, checks for newer releases, and asks whether to download. The `--no-check` option uses a cached channel without contacting GitHub. After `--`, arguments pass directly to the bundled `reims-ctl`; without arguments its TUI opens.

New bundles install beside old bundles into `~/.local/share/reims-ctl/qemu/<tag>` and atomically switch the existing `qemu/current` symlink. Running QEMU binaries are not overwritten, and VM disk images and configuration are never deleted. The existing `reims-ctl update` **still builds from source**; use `reims-release` to avoid local compilation. Optionally create a shell alias or desktop shortcut to run the release manager.

The archive checksum verifies transfer integrity, not independent signing: the manifest and bundle come from the same GitHub release. The downloader rejects path-traversal and link members in archives. Release permissions should be limited to maintainers.

## CI and compatibility

`.github/workflows/linux-binaries.yml` builds the pinned metal2vulkan, Vulkan reims-vgpu, patched QEMU and `reims-ctl` on Ubuntu 24.04. It runs the reims-ctl tests plus binary and ROM smoke checks. Push to development or the integration branch publishes a testing prerelease; pushing a `v*` tag publishes stable. Manual dispatch is available when the workflow is registered on the default branch.

Builds on GitHub runners cannot validate performance, display correctness, or a macOS Tahoe boot on an RX 7600. Ubuntu 26.10 requires an actual host smoke test. Host dependencies (Vulkan/RADV, `/dev/kvm`, `/dev/udmabuf` permissions, OVMF/OpenCore and QEMU dynamic libraries) are still required.

Experimental Metal2Vulkan optimizations are now pinned in Cargo.toml and Cargo.lock at 6d6f5d92 (fork branch feature/warmcache-discrete-gpu). Changes are selective adaptations of upstream 8c09b7c (shader stage key cache) and b25f89d (discrete-GPU zero fill and adjacent tiled-color staging reuse), not blanket merges. Pipeline watchdog timeout d7d4870 is intentionally excluded. Treat these as experimental until GitHub builds and RX 7600 / macOS Tahoe checks pass.
This integration also includes the tested local Tahoe changes up to 26231fa7 and preserves the two unpublished QEMU EFI/AppleSMC fixes via the darrenintr/qemu-reims-vgpu fork, branch reims-local-applesmc-efi. Linux CI explicitly enables libusb, retaining the host USB pass-through used locally.
