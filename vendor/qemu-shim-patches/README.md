# Shim changes not yet in the `vendor/qemu` submodule pin

`vendor/qemu` is pinned to a commit of `steelbrain/qemu-reims-vgpu`
(`host-reims-vgpu-vmapple`). A change to the device shim has to land there
before the pin can move; until it does, the change is carried here as a
`git am` series against the pinned commit.

| Patch | Base | What |
|---|---|---|
| `0001` | `bd88218` | `reims-vgpu-dirty.c`: arm a tracked set from a main-loop bottom half instead of the next guest doorbell; `reims-vgpu-dirty-test/` model check |
| `0002` | `0001` | `trace-events` + a timed wrapper: `reims_vgpu_dirty_run` reports how long each harvest held the BQL (measurement only; the clock is read only while the event is on) |

Apply, test, and bump the pin once the submodule branch carries it:

```sh
git -C vendor/qemu am ../qemu-shim-patches/000*.patch
vendor/qemu/hw/display/reims-vgpu-dirty-test/run.sh        # no QEMU build needed
for m in arm_at_track no_unlogged no_hit_stamp no_sync; do
  vendor/qemu/hw/display/reims-vgpu-dirty-test/run.sh --mutant $m 6   # each must FAIL
done
```

The Rust crate needs no change to run against either side of this patch: the ABI
is unchanged, and an unpatched shim simply arms later.

## Measuring what a harvest costs the main loop

The arming harvest runs on QEMU's main loop, which also services the audio timer
(`audio_run_out`, 10 ms) and the xHCI isochronous timers. To see whether it can
delay them, with patch `0002` applied add to the launcher:

```sh
-D /tmp/qemu-trace.log -trace reims_vgpu_dirty_run
```

and read `us=` (how long the BQL was held), `forced=1` for the main-loop harvest
and `forced=0` for a doorbell one. `scripts/audio-triage/audio-triage.sh` measures
the other side: how long the main loop takes to answer a QMP request.
