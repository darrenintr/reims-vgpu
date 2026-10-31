# Stable and Testing release tracks

## Stable (frozen)

- Published baseline: [v0.1.0](https://github.com/darrenintr/reims-vgpu/releases/tag/v0.1.0)
- Exact source: `f36b720da5d1c44f172a6aa662e9ab11d1081b5f`
- QEMU submodule: `d8b3a798430d70248f566a53aad2a8f732e4598b`
- Metal2Vulkan pin: `6d6f5d92326361562ac5d482eb017f41d80c82d9`
- Keep binary artifacts and SHA256SUMS; do not rebuild or replace this tag just to update documentation.

**Important:** the binary currently running on the owner's Ubuntu host has NOT yet been SHA256-verified against this published release. Do not label that running binary `v0.1.0` without checking `/proc/<qemu-pid>/exe`, its SHA256, and the installed `BUILD-INFO.txt`.

## Testing

- `testing`: integration branch based on `feature/experimental-bundles-and-launcher` at `e4a42df79f1487a4f18d9ed1bad4782a3deaa05e`.
- Existing experimental branch includes a release picker, optional per-boot instrumentation, and `native-v3`, `fat-lto`, `profiler` build profiles.
- `experiment/qemu-thp`: isolated first candidate, using QEMU `4e9e3de2adfe6e333f594811d7bfc4a3a840ac64` (Linux file-backed RAM 2 MiB alignment; port of upstream `1b41f5f5`). This is unvalidated on RX 7600.
- The QEMU patch source lives in `darrenintr/qemu-reims-vgpu` branch `perf/file-backed-thp-align`.
- Neither `testing` nor experimental branches may move the stable tag, modify stable assets, or automatically promote to stable.

## Promotion gate

For each candidate: build pinned QEMU and Reims on Linux; verify QEMU's `apple-gfx-pci`, USB, KVM, SMC, OpenCore and Tahoe boot; confirm desktop, audio, network, Safari/WindowServer and VM shutdown. Benchmark against the exact stable binary under the same CPU pinning, guest state, resolution, display settings, driver and workload. Report median and p95/p99 frame intervals, max stalls, guest/window FPS, CPU host and vCPU use, and GPU submission/present timings; verify graphics correctness.

Prefer 120 FPS *consistency* (8.33 ms frame budget) over peak FPS. Record regression risk and keep rollback artifacts. CI build-only success does NOT count as RX 7600 functional or performance validation. Experimental builds should remain pre-releases until tested.

## Next upstream topics

- Compare QEMU upstream fixes selectively, not a wholesale QEMU update that could break the Apple GPU/SMC fork.
- Compare Reims upstream async present and video acceleration separately; async present also depends on `reims-util` guest-side changes and is **not** a QEMU-only cherry-pick.
- Prioritize measuring shared present queue stalls, shadow textures, batching, shader compilation, and CPU/vCPU topology before tuning.
