# audio-triage

Names the first broken layer of the guest-audio path, from evidence, without
changing anything. "No sound" has several causes that look identical and want
different repairs; this separates them.

```sh
scripts/audio-triage/audio-triage.sh \
  --qemu vendor/qemu/build/qemu-system-x86_64 \
  --qmp  vm/disks/run/qmp.sock \
  --guest macos-vm --seconds 10
```

| layer | question | failure means |
|---|---|---|
| L1 host | default sink exists, unmuted | the host has no sound to give; QEMU is not the problem |
| L2 backend | this binary lists a native backend, and the launcher's choice is not `none` | the binary was built without pipewire/pa/alsa (configure found no dev package) and `vm/boot-x86.sh` fell through to `none` **silently** |
| L3 device | the running VM's `info qtree` has an audio device bound to a backend | the device is missing from the command line, or has no `audiodev=` |
| L4 enumeration | it is on the USB/PCI bus | nothing for the guest to find |
| L5 guest | macOS lists an output device (needs `--guest`) | device on the bus, no driver bound |
| L6 stream | playing creates a QEMU stream on the host | the guest never started output, or the backend could not open one |
| L7 playback | the sink monitor carries signal | a stream exists but is corked, misrouted, or carries zeroes |
| L8 scheduling | QMP `query-status` round trip stays under the 10 ms audio timer period | the main loop audio runs on is being held up |

Run L8 twice, idle and under a graphics workload. With the shim trace patch
(`vendor/qemu-shim-patches/0002`) and `-trace reims_vgpu_dirty_run` the log then
says whether the dirty-bitmap harvest is what held it.

The audio device in `vm/boot-x86.sh` is `usb-audio` on xHCI, not HD Audio (see the
AUDIO note there for why `ich9-intel-hda` never worked with AppleHDA). The triage
recognises both so a local launcher that differs is still read correctly.

Exit 0: no layer failed. Exit 1: one did; the last line names the first. Exit 2:
setup error.
