# Linux backing-table lookup optimization

Branch: `steelbrain/tahoe-import-performance`, based on `c43cafd`.

## Scope

Linux buffer import and alias creation resolve ranges through the process's
backing table. A fragmented range previously performed a B-tree lookup for
every piece. Permission checks also allocated an owned piece list and cloned
and dropped a file reference for each piece, only to inspect its protection.

`files.rs` now borrows each piece during a single ordered traversal. Run
traversal costs O(log N + K), rather than O(K log N), for N table runs and K
visited runs. Registration searches retain their existing insertion order and
linear lookup. The public `pieces` operation still returns owned references;
the permission-only operation allocates no piece list or file references.

The visitor must finish checking coverage even after seeing an unreadable
piece: an unresolved tail returns `None`, allowing the existing kernel probe,
rather than `Some(false)`. Run precedence, piece boundaries, file offsets,
permissions, empty ranges and overflow results are preserved.

This changes neither vCPU pinning nor Vulkan submission synchronization. It
does not reduce the number of live dma-buf imports or fix RADV's per-import
submission overhead. Its target is host CPU work while resolving memory.

## CPU microbenchmark

Measured in the development container on 2026-10-13, Rust 1.98.1, release
profile. These are synthetic permission-query timings, **not Ryzen 3600 VM
measurements, boot-time improvements or FPS gains**.

Each table contains contiguous 4 KiB virtual runs with noncontiguous file
offsets. Each query checks the entire range. Six rounds of 100 queries
alternate old/new order; the table reports the upper median round. The old
path is the seek-per-piece reference implementation retained in the tests.

| Runs | Old query (microseconds) | New query (microseconds) | Ratio |
| ---: | ---: | ---: | ---: |
| 1 | 0.023 | 0.012 | 1.89x |
| 64 | 0.910 | 0.165 | 5.52x |
| 1,024 | 22.679 | 2.560 | 8.86x |
| 16,384 | 688.153 | 46.655 | 14.75x |

Reproduce from the repository root:

```sh
cargo test -p reims-vgpu-vulkan os::linux::files::tests --lib
cargo test -p reims-vgpu-vulkan fragmented_permission_walk_benchmark --release --lib -- --ignored --nocapture --test-threads=1
cargo clippy -p reims-vgpu-vulkan --lib --tests -- -D warnings
```

## Validation and remaining gates

- Seven backing-table tests passed, including 1,799 differential range queries
  comparing resolution, piece boundaries, file identity, offsets and permission
  results with the reference implementation. The manual benchmark is ignored
  during ordinary test runs and was run separately successfully.
- Clippy for the Vulkan library and its tests passed with `-D warnings`, and
  the changed Rust file passed rustfmt.
- The VM integration test `os::linux::vm::tests::readable_follows_the_protection`
  failed in this container. A separate probe confirmed that
  `process_vm_readv` of this process's own readable memory returns `EPERM` here;
  the integration test depends on that call for anonymous memory. This gate
  remains unverified on an unrestricted host; it was not disabled or loosened.
- No GPU, KVM device or Tahoe disk is available here. Full `cargo xtask check`,
  the applicable behavior suite and a real guest boot remain merge gates.

On the Ryzen 3600/RX 7600 host, compare the parent and patched release builds
with the same QEMU, four-vCPU configuration, resolution and guest-disk copies.
Keep the existing pinning configuration. Enable `REIMS_VGPU_STATS=1`, alternate
build order, and record boot-to-desktop time, application-launch pauses,
presentation rate and longest frame gap under the same workload. Keep cold and
warm cache runs separate. A microbenchmark improvement alone does not establish
a visible guest improvement.
