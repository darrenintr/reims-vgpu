# Stable and Testing release tracks

## Stable (versioned; keep rollback)

- **Current official Stable:** [v0.1.2](https://github.com/darrenintr/reims-vgpu/releases/tag/v0.1.2) — exact, byte-identical promotion of the user-tested [Native-v3 + Fat-LTO experiment](https://github.com/darrenintr/reims-vgpu/releases/tag/exp-native-v3-fat-lto-1-c8838347f0e5).
- **Source:** `c8838347f0e5500c0950d2b15695a78a2ff165f8`; **Linux archive SHA256:** `35170d85a3a5fb00936b0209f7590d52d438b1e9b0399ef94a045185f28c0c57`.
- **Combined Rust compiler profile:** `RUSTFLAGS=-C target-cpu=x86-64-v3`, `CARGO_PROFILE_RELEASE_LTO=fat`, `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`; CPU requires x86-64-v3 support (e.g., Ryzen 5 3600).
- **Build and promotion:** successful [experiment run 41184582089](https://github.com/darrenintr/reims-vgpu/actions/runs/41184582089), with same bundle and `SHA256SUMS` verified during successful [promotion run 41186829117](https://github.com/darrenintr/reims-vgpu/actions/runs/41186829117); promotion did **not** recompile QEMU or Rust.
- **Pinned internals:** QEMU `d8b3a798430d70248f566a53aad2a8f732e4598b`; Metal2Vulkan `6d6f5d92326361562ac5d482eb017f41d80c82d9` — identical to v0.1.1. No QEMU THP, security backports, upstream dirty-page/FIFO, or video features in this release.
- **Rollback:** [v0.1.1](https://github.com/darrenintr/reims-vgpu/releases/tag/v0.1.1) (Native-v3 only) and [v0.1.0](https://github.com/darrenintr/reims-vgpu/releases/tag/v0.1.0) (generic x86-64) remain preserved, with tags and release assets unchanged.
- **Validation scope:** the owner reports the actual combined build performs well on Ryzen 5 3600 + RX 7600 running macOS Tahoe. Stable means the selected repeatable performance baseline; it does **not** establish sustained 120 FPS or a matched benchmark p95/p99 improvement.
- **Immutable release policy:** never retag, overwrite, or delete a previous Stable bundle to promote a new experiment; use a new semver release.

**Important:** a selected installation symlink is not proof of which QEMU process is running. For reproducible host comparison inspect `/proc/<pid>/exe`, the running binary SHA256, `BUILD-INFO.txt`, and guest settings.

## Testing

- `testing`: integration branch based on `feature/experimental-bundles-and-launcher` at `e4a42df79f1487a4f18d9ed1bad4782a3deaa05e`.
- Existing experimental branch includes a release picker, optional per-boot instrumentation, and `native-v3`, `fat-lto`, `profiler` build profiles.
- `experiment/qemu-thp`: isolated first candidate, using QEMU `4e9e3de2adfe6e333f594811d7bfc4a3a840ac64` (Linux file-backed RAM 2 MiB alignment; port of upstream `1b41f5f5`). This is unvalidated on RX 7600.
- The QEMU patch source lives in `darrenintr/qemu-reims-vgpu` branch `perf/file-backed-thp-align`.
- Neither `testing` nor experimental branches may move the stable tag, modify stable assets, or automatically promote to stable.

## Future performance experiments inherit v0.1.2

- For every QEMU upstream, Metal2Vulkan or Reims GPU runtime optimization candidate, **start from v0.1.2's combined Rust compiler profile**: `RUSTFLAGS=-C target-cpu=x86-64-v3`, `CARGO_PROFILE_RELEASE_LTO=fat`, `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`. Keep QEMU and Metal2Vulkan pins unchanged unless they are specifically the experimental variable.
- Older `exp-native-v3-*` and `exp-fat-lto-*` bundles are independent historical profiles, **not** extra performance fixes to add to v0.1.2.
- Published `exp-native-v3-fat-lto-1-c8838347f0e5` is **exactly the same bundle** as Stable v0.1.2. There is no need to download both unless retaining provenance or rollback history.
- QEMU THP `experiment/qemu-thp` currently exists as source-only. Rebuild on top of **the same combined v0.1.2 compilation flags** before testing; document host shmem THP policy and actual huge-page use, and never label that experiment as Stable automatically.
- The owner previously observed Fat-LTO on its own remaining near 120 FPS for longer intervals than Native-v3 alone, but those logs were different workloads and lengths. The combined build has a subjective user acceptance, not a quantified 120 FPS proof.
- A candidate must preserve macOS Tahoe boot, audio, networking, storage, WindowServer, Safari, GPU correctness, and stable operation. Compare matched active-motion frame-time distributions as well as CPU/GPU load before promoting.


## Promotion gate

For each candidate: build pinned QEMU and Reims on Linux; verify QEMU's `apple-gfx-pci`, USB, KVM, SMC, OpenCore and Tahoe boot; confirm desktop, audio, network, Safari/WindowServer and VM shutdown. Benchmark against the exact stable binary under the same CPU pinning, guest state, resolution, display settings, driver and workload. Report median and p95/p99 frame intervals, max stalls, guest/window FPS, CPU host and vCPU use, and GPU submission/present timings; verify graphics correctness.

Prefer 120 FPS *consistency* (8.33 ms frame budget) over peak FPS. Record regression risk and keep rollback artifacts. CI build-only success does NOT count as RX 7600 functional or performance validation. Experimental builds should remain pre-releases until tested.

## Next upstream topics

- Compare QEMU upstream fixes selectively, not a wholesale QEMU update that could break the Apple GPU/SMC fork.
- Compare Reims upstream async present and video acceleration separately; async present also depends on `reims-util` guest-side changes and is **not** a QEMU-only cherry-pick.
- Prioritize measuring shared present queue stalls, shadow textures, batching, shader compilation, and CPU/vCPU topology before tuning.
