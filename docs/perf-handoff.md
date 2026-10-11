# Performance work — handoff (updated 2026-10-11)

## Status at 2026-10-11

- Everything below marked "uncommitted" is now committed: the compute seed read
  (`c809c35c`) and the QEMU page-view run mapping (`vendor/qemu` `4bb85e9c9e`,
  pushed to `darrenintr/qemu-reims-vgpu` `host-reims-vgpu-vmapple`).
- Later commits: `3790fe2c` (per-frame host work behind window animations,
  shader translation on up to four workers, `gva_store_census`), `a69f8482`
  (verified findings from a whole-tree audit), `4f2d92f1` (sampled image
  recycle pool bounded by memory).
- Live reading on macOS 13.7.8 / RX 7600 after those: 96% of GVA Stores defer,
  the drain thread is blocked ~0 ms/s, QEMU's device thread ~34% busy during
  window animations. The genie (minimize-to-Dock) still runs at 15-25 fps.
- Next planned step: rewrite the sampled-texture path as one owner — one GPU
  image per guest texture identity, refreshed only when the guest wrote its
  pages (the write ledger / dirty tracking already exist), released with the
  guest resource. During the genie ~900 sampled images/s are still created
  and filled from guest pages whose content did not change
  (`sampled_ref_backing route=guest_runs` dominates). Measure first: add a
  per-bind census of why a bind misses the identity cache.
- 3D: 4x MSAA passes are still refused (see below); a design plan exists in
  this session's notes and is summarised in that section.


Pathway: x86 macOS 13.7.8 guest, Linux KVM, PCI, Vulkan rail on RX 7600 / RADV.
Goal set by the user: at least 30 fps and response within 30 ms, iOS Simulator included.

## Done and committed

- Main repo `5853703c` — "perf: guest RAM as dma-buf, bounded write ledger, released packed views".
  It also contains the user's earlier uncommitted work; the user chose one combined commit.
- `vendor/qemu` `6506bdc426` — `guest_ram_fd` hook; page-view registry keyed by address.
  **This commit is on a detached HEAD.** Put it on a branch and push it to the `darrenintr` fork
  before pushing the main repo, because CI fetches `vendor/qemu` from there.
- Live result: a Safari animation holds 45–99 fps after ten minutes of uptime. Before this work
  it fell to 16–20 fps within six minutes.
- Outside the repo, at the user's request: `~/OSX-KVM-Ventura/start-reims.sh` no longer exports
  `REIMS_VGPU_GUEST_IMPORT=off`, and the `sim-fast` launcher profile now points at the in-tree build.

## Uncommitted work in progress

### 1. Compute storage-image seed read on the GPU (done, not live-tested; tree builds)

- `StorageImageSeed { Host, Guest(GuestRunSource), Resident }` replaces `bytes`/`seed_skipped`.
- `RailStage::defers_seed_read()` / `with_guest_seed(MappingSeedWindow)`: the Vulkan rail takes the
  mapper-ref storage window instead of a 14 MB CPU read; a served (resident) storage seed no longer
  allocates the zero placeholder either.
- `compute_exec/vulkan.rs` `storage_seed()`: Resident, then Guest (via the new
  `draw::vulkan::mapping_window_source`, which pays the debt and builds the runs), then CPU read.
- `validate_compute` checks a Guest seed's pitch, exact length and run coverage (3 new declines).
- Tests: `vulkan_storage_seed_*`, `every_storage_seed_source_is_validated_on_its_own_terms`,
  GPU `compute_storage_image_guest_seed_strides_the_window`. Full reims-vgpu suite serial and
  clippy `-D warnings` pass (backend-vulkan,host-window).
- Remaining: live check. `compute_phase` stage_us per full-screen Simulator pass should fall from
  ~8-10 ms; store routes `compute_seed_guest_runs` / `compute_seed_guest_cpu` show which arm ran.

### 2. Page views mapped one file run at a time (done, not live-tested)

`vendor/qemu/hw/display/reims-vgpu-pci.c` `reims_vgpu_pci_map_pages`: the alias fallback now issues
one `mmap(MAP_FIXED)` per contiguous file run instead of one per 4 KiB page. Surfaces use 16 KiB
granules, so this is about 4x fewer syscalls, kernel mappings and later unmaps. It was measured
at 18% (`unmap_pages`) + 12% (`map_pages`) of the device thread once views started being released.
It builds. It needs a live boot to confirm textures still render correctly before committing it in
the submodule.

## Not started: 4x MSAA passes in the Simulator

The Simulator renders 510x510 and 1092x510 BGRA8-sRGB layers with depth as 4x MSAA. Pass 1 is
Clear+Store, passes 2..N are Load+Store, then a final pass resolves into an IOSurface. Passes 1..N
are refused (`multisample_linear_transfer_unsupported`, `chain_resident_land_fail ...
read_target_multisample_image`). Target model: the samples live only in an engine-resident 4x
image, with no guest seed and no readback, which is lawful because a Metal MSAA texture is
GPU-private. The resolve pass reads that resident.

Map from the investigation (file:line in `crates/reims-vgpu/src`):
- Request side, `runtime/draw/vulkan.rs`:
  - the readback is `skip_readback = !store_is_store` (around line 8854);
  - deferral goes through `gva_store_defer_eligible` (around line 11023). Its
    `row_stride >= tight_row_bytes * sample_count` term likely rejects 4x;
  - the identity is `gva_chain_identity` (around line 10638). It has no sample count, so a 1x and
    a 4x resident over one GVA share a key.
- Landing: `ResidentGvaStore` → `writeback_debt::arm_gva` (`writeback_debt.rs` around line 1322) →
  `resident_read_snapshot` refuses MSAA (`engine/mod.rs` around line 6032). Never arm a debt for an
  MSAA resident.
- Engine: `load_from_target` on a missing or not-ready resident is a refusal
  (`LoadTargetContentNotReady`, `exec.rs` around line 3644). `gva_resident_ready` and
  `resident_content_ready` do not check sample count.
- Suspected existing defect to check first: `exec.rs` around line 3574, `color_only =
  PassKey::single(...)` (sample_count 1) is used to build the framebuffer of a 4x resident when
  there is a depth attachment.
- The resolve pass always uses the transient `acquire_multisample_target` image (`exec.rs` around
  line 3727) and cannot load a resident. It needs the source identity and generation plumbed
  through. Today `ColorRtRequest.multisample_source_ref` is only a u32. Then bind the 4x resident
  as the MS attachment, and never `registry_ensure(sample=1)` that identity.
- Existing tests to extend: `reason.rs` around line 864; `runtime/exec/tests.rs` around lines 5605
  and 5755; `runtime/draw/tests.rs` around lines 1309, 2874 and 4380–4520.

## Other known items

- The Earth lock-screen wallpaper (`AegirPoster`) crashes on macOS 13. The guest's paravirtual
  Metal plugin hard-codes `isFramebufferReadSupported` / `deviceSupportsFeatureSet:` to NO; it was
  disassembled locally and not committed. The device cannot fix this on Ventura; macOS 15+ read
  device-info keys 37/44.
- In the user's Simulator, `com.apple.findmy.findmylocated` and `com.apple.managedappdistributiond`
  were disabled with `simctl spawn booted launchctl disable` because they crash in a loop on
  iOS 17.2.
- Rarer page-view leaks the subagent found, not fixed:
  - `draw/vulkan.rs` `coalesce_pages_to_runs` never unmaps a cross-region alias;
  - failure-path `?` returns after `map_pages` in `ensure_packed_resource`.
- A virtio-blk boot disk was 3x faster for I/O in a read-only test. It is not applied to the
  user's launcher; that needs their go-ahead.
- Observation lines added and kept: `stamp_latency`, `guest_write_ledger`.

## How to verify

- Build: `scripts/qemu-build/qemu-build.sh --backend vulkan`.
- Tests: `cargo test -p reims-vgpu --no-default-features --features backend-vulkan,host-window --
  --test-threads=1`.
- Live: the user launches `~/OSX-KVM-Ventura/start-reims.sh`. Open a requestAnimationFrame page in
  guest Safari and watch these lines in `/tmp/reims-vgpu-fail.log` for at least 10 minutes:
  `host_window_cadence` present_hz, `stamp_latency`, `guest_write_ledger`, `compute_phase`, and
  QEMU's `/proc/<pid>/maps` count.

## Audit 2026-10-11 — verified but deferred (not fixed yet)

Fixed in `3790fe2c` and `a69f8482`. Still open, each confirmed against the code:

- REPLACE_PHYSICAL device work runs twice per packet: at arrival (`drain/mod.rs`
  `repoint_at_arrival`) and again in `process_child_packet`'s arm. Needs a decision on
  which owns the lifecycle op before deleting either.
- Vulkan engine error paths leak a transient depth image, a descriptor set, or an
  occlusion query pool (`engine/exec.rs` `create_transient_depth`, `alloc_descriptor_set`,
  `create_query_pool`; `exec_compute.rs` descriptor set). Only on refused draws.
- Present submit failure leaves the acquire semaphore signalled
  (`engine/window_present.rs` ~1503-1518); arm a rebuild on error.
- vblank thread is never joined at device destroy (`device/mod.rs` 284-317, QEMU
  `reims-vgpu-pci.c` 952-970): shutdown-only use-after-free risk.
- `device_poll` holds the device lock across the early-frame paint (`device/mod.rs` 652-704).
- Per-frame work still to cut: mapped-surface revalidate per bind (`mapper/mod.rs` 1972),
  per-row fragmented writes (`mapping_write/mod.rs` 1754/1855), per-row blit copies
  (`blit_exec/mod.rs` 1904-1929, 2737-2967), `pump_translations`/`observe_awaited_stamps`
  per packet (`drain/mod.rs` 2511-2696), compute `retire_all` on readback
  (`exec_compute.rs` 1639), single-slot packed-alias cache per texture_ref
  (`draw/vulkan.rs` 3229-3260), whole-AIR hash per shader lookup (`m2v_cache.rs` 718).
- Guest-sized allocations without a cap: `objects/mod.rs` 1330 (descriptor length),
  3532 (backing page count).
- ICB rail findings (`runtime/icb`, `draw/metal/icb.rs`) are latent: ICB command memory
  is never bound outside tests.
