# Combined upstream testing candidate - 2026-11-03

This branch is an experiment based on Stable v0.1.2, not a Stable promotion.
Do not install a source-only checkout as a validated binary.

## Intended compatible-guest stack

- QEMU b9d3bd5a9dd427ac42f6a2249c26a0b342f8f16e: the retained Tahoe fork,
  existing 2 MiB file-backed RAM alignment, plus xHCI cfa70f3a and USB-host
  lifetime fixes 59cb5109/a8633856. Local AppleSMC and Apple GPU changes stay.
- Reims 43b7caa2: faster resource lookup, shorter bind-list allocations,
  cached per-segment capabilities and aligned guest-memory reads.
- Reims 422eab5b: FIFO in-thread task execution/replies. Keep the fork's
  existing trace events, VecDeque and before-block submission hook.
- Metal2Vulkan: start from the tested RDNA3 fork at 6d6f5d92 and backport
  debug metadata support. Universal-metallib and Core Image changes are
  attempted transactionally; incompatible ports are explicitly deferred.
- Keep Rust 1.98.1, x86-64-v3, Fat LTO and codegen-units=1, as in v0.1.2.

The generated `upstream-stack-20261103.json` is authoritative for what was
actually assembled, including exact inputs and any deferred commits.
Translator sources are retained in vendor/metal2vulkan so the binary's
contents do not depend on a moving branch or a private patch checkout.

## Explicit exclusions

APV guest-driver/IOSurface migration, video devices, dirty-page copying,
pipeline timeout behavior and general Rust/SDL upgrades are not silently
mixed into this compatible-guest candidate. They require separate integration
work; this branch must not be described as containing those features.

## Gates and output

The dedicated workflow assembles sources, commits only to this experimental
branch, runs core/ctl and targeted translator tests, builds the library and
QEMU, performs binary smoke tests, and publishes a new testing prerelease
only after all gates succeed. No existing tag or release asset is replaced.
A failed workflow publishes no binary release.

The runtime bundle keeps the existing `reims-vgpu-linux-x86_64.tar.gz` name
for release-picker compatibility. It includes BUILD-INFO.txt and
UPSTREAM-STACK.json. A source archive and SHA256SUMS accompany it.

CI is not an RX 7600/Tahoe hardware test. Preserve v0.1.2 for rollback and
compare matched Finder dragging, Safari scrolling and animated workloads.
Check guest boot, rendering, input, audio, network and clean shutdown before
any Stable promotion. QEMU qtests and hardware performance remain unverified.

THP has no effect when the host's shmem policy forbids it. No host setting is
changed by the build. The translator cache version is isolated from upstream;
a first boot may recompile shaders. No guest disk, OpenCore or current symlink
is modified automatically.
