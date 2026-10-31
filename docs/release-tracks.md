# Stable and Testing release tracks

## Stable (versioned; keep rollback)

- Current official Stable: [v0.1.1](https://github.com/darrenintr/reims-vgpu/releases/tag/v0.1.1), the exact prebuilt x86-64-v3 Rust optimization promoted from [exp-native-v3-3-e4a42df79f14](https://github.com/darrenintr/reims-vgpu/releases/tag/exp-native-v3-3-e4a42df79f14). It is intended for Ryzen 5 3600 or other x86-64-v3-capable hosts; **not** a general x86-64 binary.
- Exact source commit: e4a42df79f1487a4f18d9ed1bad4782a3deaa05e; verified archived bundle SHA256: d2a7edf04b5d6621e353342d94301cb9533ab10cf7e25867dc74f0833f44cb1b.
- Successful native-v3 build: [GitHub Actions run 41170050601](https://github.com/darrenintr/reims-vgpu/actions/runs/41170050601); promotion [run 41183204162](https://github.com/darrenintr/reims-vgpu/actions/runs/41183204162).
- Original portable Stable [v0.1.0](https://github.com/darrenintr/reims-vgpu/releases/tag/v0.1.0) remains preserved exactly, with source f36b720da5d1c44f172a6aa662e9ab11d1081b5f, as the rollback for other CPUs or performance regressions.
- Both builds pin QEMU d8b3a798430d70248f566a53aad2a8f732e4598b and Metal2Vulkan 6d6f5d92326361562ac5d482eb017f41d80c82d9. v0.1.1 changes Rust code generation, not QEMU/Metal2Vulkan GPU algorithms.
- v0.1.1 is a user-selected **performance baseline**, not a claim of sustained 120 FPS. The user's 2026-10-31 sample includes a short 116–120 FPS stretch and many lower-FPS intervals; subjective improvement is encouraging but no controlled, matched A/B improvement is established.
- Release assets are immutable by policy: never retag or overwrite an existing Stable tag or bundle. A change requires a new semver release.

**Important:** a chosen installation symlink is not proof of which QEMU process is running. A reproducible host comparison also checks `/proc/<pid>/exe`, the running binary's SHA256 and installed `BUILD-INFO.txt`.


## Testing

- `testing`: integration branch based on `feature/experimental-bundles-and-launcher` at `e4a42df79f1487a4f18d9ed1bad4782a3deaa05e`.
- Existing experimental branch includes a release picker, optional per-boot instrumentation, and `native-v3`, `fat-lto`, `profiler` build profiles.
- `experiment/qemu-thp`: isolated first candidate, using QEMU `4e9e3de2adfe6e333f594811d7bfc4a3a840ac64` (Linux file-backed RAM 2 MiB alignment; port of upstream `1b41f5f5`). This is unvalidated on RX 7600.
- The QEMU patch source lives in `darrenintr/qemu-reims-vgpu` branch `perf/file-backed-thp-align`.
- Neither `testing` nor experimental branches may move the stable tag, modify stable assets, or automatically promote to stable.

## Future performance experiments inherit v0.1.1

- For an apples-to-apples QEMU/Metal2Vulkan A/B run on the Ryzen 5 3600, compile Rust and its dependencies with `RUSTFLAGS=-C target-cpu=x86-64-v3`, Thin LTO, 16 codegen units; keep host, RAM, CPU pinning, resolution, GPU driver, disk state, and test scene fixed.
- The earlier `exp-fat-lto-3-e4a42df79f14` bundle used default Rust target flags (not Native-v3). It is an independent experiment, **not** a Fat-LTO patch applied on top of v0.1.1.
- The owner reports a subjective improvement on Fat-LTO. Their `vmlog3.txt` shows roughly 28 consecutive samples at >=115 FPS versus five consecutive samples in `vmlog2.txt`; the logs have different lengths and are not matched workloads, and high-FPS samples still exhibit >8.33 ms max gaps. These logs do not embed binary SHA or build identity, so the test attribution relies on which build the owner selected.
- **Next candidate, not yet Stable:** compile Native-v3 + Fat-LTO + one codegen unit on the same QEMU and Metal2Vulkan pins as v0.1.1. Flags: `RUSTFLAGS=-C target-cpu=x86-64-v3`, `CARGO_PROFILE_RELEASE_LTO=fat`, `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`. Publish a distinct `exp-native-v3-fat-lto-*` prerelease with SHA256SUMS. Do not promote to v0.1.2 unless the actual combined binary passes real Tahoe/RX 7600 tests and comparison against v0.1.1.
- The QEMU THP experimental branch is source-only until a new built bundle is published. Build that candidate with the same Native-v3 flags before comparing to the new Stable baseline.
- Never infer a renderer improvement from compiler flags or a GitHub Actions success alone; compare matched runtime frame intervals and check correctness.

## Promotion gate

For each candidate: build pinned QEMU and Reims on Linux; verify QEMU's `apple-gfx-pci`, USB, KVM, SMC, OpenCore and Tahoe boot; confirm desktop, audio, network, Safari/WindowServer and VM shutdown. Benchmark against the exact stable binary under the same CPU pinning, guest state, resolution, display settings, driver and workload. Report median and p95/p99 frame intervals, max stalls, guest/window FPS, CPU host and vCPU use, and GPU submission/present timings; verify graphics correctness.

Prefer 120 FPS *consistency* (8.33 ms frame budget) over peak FPS. Record regression risk and keep rollback artifacts. CI build-only success does NOT count as RX 7600 functional or performance validation. Experimental builds should remain pre-releases until tested.

## Next upstream topics

- Compare QEMU upstream fixes selectively, not a wholesale QEMU update that could break the Apple GPU/SMC fork.
- Compare Reims upstream async present and video acceleration separately; async present also depends on `reims-util` guest-side changes and is **not** a QEMU-only cherry-pick.
- Prioritize measuring shared present queue stalls, shadow textures, batching, shader compilation, and CPU/vCPU topology before tuning.
