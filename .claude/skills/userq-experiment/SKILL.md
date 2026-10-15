---
name: userq-experiment
description: On the owner's Linux host with the RX 7600, land the submission-cost work that waits for a GPU (metal2vulkan's chunks at offsets and scheduler tunings, the library's dma-buf windows), build the experimental RADV with amdgpu user queues and its control, and measure all of it on VM boots. Use when the owner asks to run the user-queue experiment, build the experimental driver, or finish the gpu-latency branch at home.
---

# The user-queue experiment and the submission-cost work

Written on 2026-10-15 in a cloud session with no GPU, on branch `steelbrain/gpu-latency`. That
session could compile and unit-test, but not run anything on a GPU, push to
`darrenintr/metal2vulkan`, or install packages. This skill finishes the work on the host it is
for: the Ryzen 5 3600 with the Radeon RX 7600 (RADV), Linux 7.3.

Follow `AGENTS.md` throughout: a worktree of your own, one check per merge, GPU and VM
discipline, stop only the pids you started, boot only disk copies. **Ask the owner before**
anything that needs root, installs or replaces a kernel, changes the boot loader, or reboots
the host. Ask once, with the exact commands, and do the work that needs none of that while
waiting.

## What is already on the branch

- `reims-ctl --confine llc` (`crates/reims-ctl/src/pin.rs`): QEMU's other threads kept within
  the vCPUs' L3 (one CCX). It is off by default and not yet measured.
- `scripts/build-userq-mesa.sh`: builds RADV from Mesa MR !40808 ("User queues in RADV", a draft)
  and a control RADV from the same base, each in its own prefix under `~/.cache/mesa-userq`. It
  also downloads the MR's kernel patches.
- `patches/metal2vulkan/`:
  - `0001-dma-buf-chunks-at-offsets.diff`: `DmaBufChunk::offset` and
    `DmaBufRequest::offset_alignment`. The runtime imports a chunk's dma-buf whole and binds the
    chunk at its offset.
  - `0002-scheduler-tunings.diff`: `M2V_DEFER_COUNT`, `M2V_DEFER_LIMIT_US` and `M2V_RUN_LIMIT`.
  - Both were written against the pinned runtime, `0107e942`. Only `reims-vgpu-vulkan`'s build
    against them was checked; none of the runtime's own tests were run.
- `patches/reims-vgpu/0001-dma-buf-windows.patch`: `REIMS_VGPU_DMA_BUF_WINDOWS=1`. A stretch of a
  request that one run of a file covers is answered as an offset into a dma-buf of the aligned
  64 MiB window of that file. Its unit tests passed against the patched runtime. It compiles only
  once the pin moves, which is why it is a patch and not a commit.

The frame readback item needs no code. The pinned runtime already keeps host-copied textures in
host-cached memory, or copies them on NVIDIA's staging queue (its `texture/host_copy.rs`). The
1.7 s read in `docs/qemu.md` predates that. Measure it again (step 3); don't build anything for it.

## 1. Land the runtime patches

1. Clone `git@github.com:darrenintr/metal2vulkan.git` under `~/.cache` and start branch
   `steelbrain/dma-buf-windows` at `0107e9424a7c8605ebc389daf54b5e311fcbb67d`.
2. Apply `patches/metal2vulkan/0001-dma-buf-chunks-at-offsets.diff`. Commit it as:
   ```
   Take a dma-buf source's chunks at offsets into their dma-bufs

   RADV passes every imported buffer object with each submission, about
   0.55 us each, and a source could share an import only for a chunk of
   exactly the same pages as another. A chunk now names where its bytes start
   in its dma-buf (DmaBufChunk::offset), at 0 or a multiple of the request's
   DmaBufRequest::offset_alignment: a sparse buffer's alignment where several
   chunks are bound, a plain buffer's where one is, and 0 (no offsets) for an
   image's dedicated import. The runtime imports a chunk's dma-buf whole and
   binds the chunk at its offset, so a source that answers windows of a file's
   pages lets every buffer over one window share one import.
   ```
3. Apply `0002-scheduler-tunings.diff`. Commit it as:
   ```
   Let M2V_DEFER_COUNT, M2V_DEFER_LIMIT_US and M2V_RUN_LIMIT tune submissions

   The deferred submissions' limits (32 commits in a row, 1 ms, 64 batches a
   run) were constants. Each can now be set for a measurement, read once, a
   value out of range keeping the default: they change when work is submitted,
   never what it does, and M2V_SCHED_STATS shows their effect.
   ```
4. Test on the RX 7600, following that repository's `docs/VALIDATION.md`:
   - its `dma_buf` and `host_memory` unit tests, including the new
     `a_chunk_starts_into_its_dma_buf_at_the_request_s_alignment`;
   - the runtime lane `dma_buf_imports`;
   - its scheduler tests.
   The import path was rewritten blind: a chunk at an offset binds with
   `vkBindBufferMemory(.., offset)`, or a sparse bind's `memoryOffset`. Look hard at failures
   there. Fix what fails in a commit of its own.
5. Ask the owner before pushing the branch to the fork, then push.

## 2. Move the pin, apply the library patch, check

1. In a `reims-vgpu` worktree on `steelbrain/gpu-latency`, move both `metal2vulkan` crates in
   `Cargo.toml` to the fork's new head, then run `cargo update -p metal2vulkan -p metal2vulkan-runtime`.
2. Apply the library patch with `git am patches/reims-vgpu/0001-dma-buf-windows.patch`. Commit
   the pin move first, its own commit: `:racehorse: Pin metal2vulkan at chunks at offsets`.
3. Delete `patches/` in a commit of its own once both patches have landed.
4. Run `cargo nextest run -p reims-vgpu-vulkan -E 'test(/dma_buf/)'`, then `cargo xtask check`
   once (the GPU tests that fail under load run again alone). Then `make test`, since the pin
   reaches what the suite drives.

## 3. Measure what landed (no new kernel needed)

Use the macOS 26 guest at 8 vCPUs, 8 GiB and 1920x1080 at 100 Hz, on copies of one disk, cold
caches apart from warm. Follow `docs/qemu.md`, "Measuring it", and alternate build orders.
Always set:

```sh
reims-ctl set mac --env REIMS_VGPU_STATS=1 --env M2V_SCHED_STATS=1
```

Record, for each case: presents a second, the longest gap, the window's GPU time,
submissions a second, batches per submission, time per submission, and the runtime's
`WorkCounts::dma_buf_imports` against `dma_buf_shared_imports` (live imports). Use Safari with
a CSS animation, the Finder genie, and Chess's launch time. Do each idle, and again under a
12-job release build.

1. **Baseline**: the parent pin, no settings.
2. **Windows**: `--env REIMS_VGPU_DMA_BUF_WINDOWS=1`. Expect far fewer live imports and a
   shorter time per submission. Watch QEMU's RSS too: udmabuf pins every page of a window.
3. **Deferral limits** (each read once at start, so restart for each value):
   `M2V_DEFER_LIMIT_US` = 250, 1000 (default) and 4000; `M2V_DEFER_COUNT` = 16, 32 and 64;
   `M2V_RUN_LIMIT` = 64 and 256. Keep what improves presents without lengthening the longest
   gap. A default changes only in a runtime commit that cites the numbers.
4. **Confine**: `reims-ctl set mac --cpus 4 --threads-per-core 2 --pin auto --confine llc`
   against the same without `--confine`, under the build. 8 vCPUs span both CCXs, so confine
   changes nothing there. QEMU's log says what it confined.
5. **Readback**: with no window (`-display none` and VNC), time `rvg_frame_target_read_pixels`
   for 1920x1080, using the `apple_gfx` trace or a debug line. The goal is milliseconds, not the
   1.7 s recorded.

Write the numbers into `docs/qemu.md`, in the sections those settings belong to. Make window
imports the default only if the numbers carry it and the pinned memory is acceptable to the
owner; that is the owner's call, so ask.

## 4. Build the experimental driver

1. Run `scripts/build-userq-mesa.sh`. It names the packages it needs and stops if they are
   missing. Installing them takes `sudo apt install ...`, so ask the owner. It builds:
   - `~/.cache/mesa-userq/userq/install`: RADV with user queues;
   - `~/.cache/mesa-userq/control/install`: the same base without them.
   It also prints each ICD's `VK_DRIVER_FILES` line.
2. **Compare against the control only.** Both builds are Mesa 26.2.0-devel, older than the
   26.2.4 that metal2vulkan's RDNA3 stand-in needs, so neither gets the stand-in. The control
   isolates the user queues; the system's Mesa would not.
3. Check both builds load before any kernel work:
   `VK_DRIVER_FILES=<icd> vulkaninfo --summary` should name the RX 7600 under RADV.

## 5. A kernel with user queues

The host's kernel offers none to this GPU until `amdgpu.user_queue` is set, and the RADV branch
also needs the MR's kernel patches (`~/.cache/mesa-userq/kernel/userq-kernel-patches.tar.gz`,
written on top of 7.1-rc4).

1. Unpack the tarball into a directory of its own, and read every patch before using it.
2. Get the source of the running kernel's release (7.3, or newer).
3. Patch the tree:
   - Check each patch with `git apply --check`, and with `-R` to find any already in the tree.
     The signal ioctl's `syncobj_points` patch was reviewed in August 2026 and may have landed.
   - Apply the rest, resolving conflicts against the newer tree.
4. Configure:
   - Start from the running kernel's config.
   - Enable amdgpu's user-queue option: grep `drivers/gpu/drm/amd/amdgpu/Kconfig` for `USERQ`.
   - Check `modinfo amdgpu | grep -A2 user_queue` on the built module for what each value means.
5. **Ask the owner** before installing the kernel, adding `amdgpu.user_queue=1` to the boot
   entry, and rebooting. Give the exact commands for their distribution, and keep the old kernel
   bootable.
6. After the reboot:
   - `/sys/module/amdgpu/parameters/user_queue` should read 1 or 2.
   - `dmesg | grep -i -E 'userq|mes'` should show no MES errors. A gfx1102 MES failure to evict a
     queue was reported in 2024; any hang there ends the experiment, so report it.
   - Prove RADV uses the queues: under the user-queue ICD, `strace -f -c -e trace=ioctl` of
     `vkcube` should show almost no `DRM_IOCTL_AMDGPU_CS` calls. Under the control ICD it shows
     one or more a frame.

## 6. Measure user queues

1. Boot copies of the same disk, alternating the two Mesa builds and the build order:
   ```sh
   reims-ctl set mac --env VK_DRIVER_FILES=$(cat ~/.cache/mesa-userq/userq/icd)
   reims-ctl set mac --env VK_DRIVER_FILES=$(cat ~/.cache/mesa-userq/control/icd)
   reims-ctl set mac --env VK_DRIVER_FILES=     # back to the system's Mesa
   ```
2. Record what step 3 records, the time per submission most of all. The branch's own TODO list
   says it still passes every buffer object in the wait ioctl
   (`drm_amdgpu_userq_wait::bo_read_handles`), so the per-object cost may stay.
3. Run with windows on as well: fewer objects should help either build.
4. Fail on any `reims-vgpu: fatal` line, and keep QEMU's logs.

## 7. Report

- Write the results in `docs/qemu.md` (a section "Linux hosts: amdgpu user queues"): the
  kernel, the patches applied, the Mesa revisions, the numbers for both builds, and whether
  user queues are worth waiting for upstream.
- Don't make a user-queue kernel or Mesa a requirement of anything: users have neither.
- Push the branch (ask first) and open a draft PR.
- Tell the owner, in a few lines, what changed and what the numbers say.
