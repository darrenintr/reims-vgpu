# macOS Tahoe Release picker: what to test and how to clean old builds

## Current baseline and next candidate (2026-10-31)

- **Stable v0.1.2:** the user-accepted, prebuilt Native-v3 + Fat-LTO + codegen-units=1 bundle. Select it for regular macOS Tahoe use.
- **Next performance experiment:** QEMU `system/physmem.c` 2 MiB alignment for Linux memfd Transparent Huge Pages. See [experiment/qemu-thp-v0.1.2](https://github.com/darrenintr/reims-vgpu/tree/experiment/qemu-thp-v0.1.2). The build also uses Native-v3 + Fat-LTO. It is **not usable until its `exp-qemu-thp-*` GitHub Release is published**, and only then should it be compared against Stable v0.1.2.
- **Diagnostic-only:** latest Profiler release. Other older Profiler runs remain in History.
- **History:** original experimental Native-v3, standalone Fat-LTO, Testing-12/14, and the first combined experiment (its binary is identical to v0.1.2) remain available for investigation or rollback.

## KDE launcher menu

The top menu shows only the latest Stable and **new, actionable** experimental builds. The selected installation symlink is displayed in the prompt, but it does **not** prove which QEMU process is running.

Choose **诊断工具** for the newest Profiler; **历史及回滚** for all published versions; **清理本机缓存** to preview obsolete local downloads.

The launcher and release manager are separate scripts. After modifying both in the repo, install both updated files (and confirm the KDE `.desktop` entry points to the installed launcher).

## Cleanup security model

The release manager exposes:
```sh
~/.local/bin/reims-release --cleanup-preview
~/.local/bin/reims-release --cleanup-preview --json
# Destructive only with an unchanged preview plan and deliberate confirmation:
~/.local/bin/reims-release --cleanup-apply --confirm --plan-id '<preview-plan-id>'
```

The GUI first shows the exact candidates, estimated size, and asks for confirmation. There is **no headless auto-confirm for cleanup**.

Guaranteed protections:
- Do **not** delete any published GitHub Releases or remote assets.
- Do **not** delete VM disks, EFI/OpenCore, snapshots, logs or arbitrary directories.
- Do **not** delete any Stable bundles, the currently selected installation, the latest actionable Testing/experiment candidate, or the newest Profiler.
- Do **not** delete while any `qemu-system*` or `qemu-kvm` is running; fail closed when the process table is not accessible.
- Only remove installed folders inside the configured `reims-ctl/qemu` directory whose tags exactly match real published GitHub Releases; symlink directories are never removed.
- Recalculate the plan immediately before deletion; if tags/size/inode have changed, reject and request another preview.

**Important:** If your currently selected build is `exp-native-v3-fat-lto-1-c8838347f0e5`, it is protected even though it duplicates v0.1.2. Select and activate **Stable v0.1.2** (when the VM is stopped) before opening Cleanup a second time. Both binaries are identical; the tag changes for normal version management.

Local storage savings depend on which versions were actually downloaded; do not estimate them from GitHub Release archive sizes.

## Performance benchmark for QEMU THP

Test exactly the same Tahoe Guest state, CPU pinning, 1920x1080 display, host GPU driver and workload on v0.1.2 and `exp-qemu-thp-*`. Use 30 seconds active Finder window dragging, 30 seconds Safari scrolling and 10 Launchpad cycles. Observe frame intervals and stalls rather than peak FPS.

Check Linux THP policy:
```sh
cat /sys/kernel/mm/transparent_hugepage/shmem_enabled
cat /sys/kernel/mm/transparent_hugepage/enabled
```
THP is conditional and may not be enabled on the host. A build with 2 MiB mapping alignment does not guarantee that KVM uses huge pages, especially when dirty logging is active. Record the host policy and memory mapping observations before drawing a performance conclusion.
