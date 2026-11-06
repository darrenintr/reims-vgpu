# Combined Reims upstream testing + Ventura release picker (2026-11-06)

This integration is based on `testing`, with the source-only upstream-stack experiment transplanted in a **single candidate**. It is **not** a Stable promotion. Stable v0.1.3 and all previous GitHub Releases remain unchanged.

## Integrated and pinned

- QEMU `b9d3bd5a9dd427ac42f6a2249c26a0b342f8f16e`: retains the Tahoe fork, 2 MiB memfd alignment, xHCI reentrancy protection, libusb host lifetime fixes and local AppleSMC/Apple GPU behavior.
- Reims core: fast guest-record lookup/decoding (`43b7caa2`), FIFO in-thread execution (`422eab5b`) and the existing fork-specific wake hooks.
- Metal2Vulkan: the RX 7600 fork at `6d6f5d92` and manually adapted debug information, Universal metallib and Core Image / aggregate-access ports. Source is pinned inside `vendor/metal2vulkan`, not fetched from a moving branch.
- Linux compile profile: Rust 1.98.1, x86-64-v3, Fat LTO, codegen-units 1.
- New independent `scripts/ventura-launch-remote`: interactive online/offline selector, exact release tag, selected release activation, launch, and stop.

Exact adapted sources and ported commits are recorded in [upstream-stack-20261103.json](upstream-stack-20261103.json). The previous stack's full-core CI aborted after a 32 GiB allocation and one lifecycle test failure. The new workflow gates focused integration tests and clearly states that the *full* core suite has not passed. A green CI build is not an RX 7600 or macOS guest hardware validation.

## Opt-in features not silently included

- KVM dirty-page tracking and its paired Metal2Vulkan CPU-copy synchronization changes: dependent C ABI, QEMU, Rust, and runtime changes must land together.
- `reims-util` x86 async-present guest kexts: not a host-only patch. Requires a verified guest-driver/OpenCore upgrade with recovery and rollback, and cannot be included simply by changing the release bundle.
- Advanced `/dev/udmabuf` mapping-budget redesign: separate host driver/kernel compatibility regression tests needed.
- VideoToolbox / VA-API scaler, NVMe boot-controller changes and pipeline timeout behavior: deliberately isolated due to guest compatibility and stability risks.

These are candidates for separate, reversible follow-up builds. Do not describe this integrated build as containing them.

## Installing the Ventura launcher on Ubuntu / KDE

From this checkout, copy both scripts to your local launcher directory:

```bash
install -Dm755 scripts/reims-release.py "$HOME/.local/bin/reims-release"
install -Dm755 scripts/ventura-launch-remote "$HOME/.local/bin/ventura-launch-remote"
```

Use `reims-ctl list` to find the **existing Reims-managed** Ventura VM name. By default it is `ventura`; override using `VENTURA_VM=your-vm-name` if needed. The script will not create or overwrite a VM disk or OpenCore image.

```bash
ventura-launch-remote                # GUI on KDE (kdialog), Zenity, or terminal menu
ventura-launch-remote --list-remote  # inspect published GitHub builds
ventura-launch-remote --list-local   # inspect already downloaded bundles offline
ventura-launch-remote --tag testing-combined-RUN-SHA  # exact release from GitHub
ventura-launch-remote --local v0.1.3                 # use an existing bundle, zero network
ventura-launch-remote --stop          # stop Ventura; keep the selected binary
```

For an existing VM called `mac`, launch with:

```bash
VENTURA_VM=mac ventura-launch-remote
```

Cached bundles are discovered at `~/.local/share/reims-ctl/qemu/<release-tag>/` (or under `REIMS_CTL_DATA_DIR/qemu`). Both local and remote selection validate safe tags and the required executable filenames. The downloader validates the release archive SHA256 before installing. Local selection uses `--no-check` and does not contact GitHub.

The release manager changes the shared `qemu/current` symlink, so do not change builds while the Ventura VM is running. An already-running Tahoe VM keeps the executable with which it was launched, but both launches share the default install selection. The startup log is at `~/.local/state/ventura-launch.log` by default.

**Compatibility boundary:** This script launches a VM already registered with `reims-ctl`. It does not mutate or automatically replace an unrelated `~/OSX-KVM-Ventura/boot-macOS*.sh` that starts QEMU directly. Such a script may contain hard-coded binary paths and must be inspected on the actual PC first.

## CI and first host test

The `combined-testing-20261106.yml` workflow builds the pinned library, ROM, QEMU and reims-ctl; runs the Ventura picker tests and focused core/translator checks; and saves an artifact. Only a successful `testing` branch push publishes a new `testing-combined-...` prerelease. Old Stable binaries and their tags are never overwritten.

Before considering Stable, A/B with v0.1.3 on the same Ventura/Tahoe VM state, identical CPU pinning, resolution and caches. Verify boot, graphics, clipboard/input, audio, network, shutdown and Finder/Safari frame times (median, p95 and p99). The host's shmem THP policy must permit huge pages for the QEMU alignment optimization to have an effect.
