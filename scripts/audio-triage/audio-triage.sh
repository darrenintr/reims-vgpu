#!/usr/bin/env bash
# audio-triage.sh — which layer of the guest-audio path is broken?
#
# "No sound" has at least eight different causes that look identical from the
# desk, and each wants a different repair. This walks them in order, prints the
# evidence for each, and names the FIRST layer that fails. It changes nothing: it
# reads the QEMU binary, its build tree, the running process, the host sound
# server and (optionally) the guest, and it never writes to any of them.
#
#   L1 host        the sound server has a default sink, unmuted, with volume
#   L2 backend     this QEMU binary was built with a native host backend, and the
#                  launcher's run-time choice does not silently fall through to
#                  `none` (it does, with no error: vm/boot-x86.sh picks the first
#                  of `pipewire pa alsa sdl none` that `-audiodev help` lists)
#   L3 device      the running VM has the audio device and its `audiodev=` names a
#                  backend that exists
#   L4 enumeration the device is on the bus the guest enumerates
#   L5 guest       macOS has an output device                       (needs --guest)
#   L6 stream      playing in the guest creates a host stream
#   L7 playback    samples reach the host sink (not silence)
#   L8 scheduling  QEMU's main loop answers within the audio timer period
#
# L8 is a QMP round-trip: the monitor is served from the same main loop that runs
# `audio_run_out` (10 ms) and the xHCI isochronous timers, so a round-trip that
# takes tens of milliseconds is a main loop that audio is waiting behind. It is a
# measurement of the thread, not of any one cause; run it with the guest idle and
# again under a graphics workload, and compare.
#
# Usage:
#   scripts/audio-triage/audio-triage.sh [--qemu BIN] [--qmp SOCK] [--guest SSH]
#                                        [--seconds N]
#
#   --qemu     the binary the VM launches (default vendor/qemu/build/qemu-system-x86_64)
#   --qmp      the running VM's QMP socket (default vm/disks/run/qmp.sock); without
#              one L3/L4/L8 are skipped
#   --guest    an ssh alias for the guest (as audio-crackle-probe uses); enables L5
#              and makes L6/L7 play a sound instead of waiting for you to
#   --seconds  how long L6/L7/L8 observe (default 10)
#
# Exit: 0 no layer failed, 1 a layer failed (the first is named on the last line),
# 2 setup error.
set -uo pipefail
export LC_ALL=C

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QEMU_BIN="$ROOT/vendor/qemu/build/qemu-system-x86_64"
QMP_SOCK="$ROOT/vm/disks/run/qmp.sock"
GUEST=""
SECS=10

while [ $# -gt 0 ]; do
  case "$1" in
    --qemu) QEMU_BIN="$2"; shift 2 ;;
    --qmp) QMP_SOCK="$2"; shift 2 ;;
    --guest) GUEST="$2"; shift 2 ;;
    --seconds) SECS="$2"; shift 2 ;;
    -h|--help) sed -n '2,38p' "$0"; exit 0 ;;
    *) echo "audio-triage: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -x "$QEMU_BIN" ] || { echo "audio-triage: no executable QEMU at $QEMU_BIN (use --qemu)" >&2; exit 2; }
command -v python3 >/dev/null || { echo "audio-triage: python3 is required" >&2; exit 2; }

FIRST_FAIL=""
note() { printf '%-4s %-12s %-5s %s\n' "$1" "$2" "$3" "$4"; }
ok()   { note "$1" "$2" PASS "$3"; }
warn() { note "$1" "$2" WARN "$3"; }
skip() { note "$1" "$2" SKIP "$3"; }
bad()  { note "$1" "$2" FAIL "$3"; [ -n "$FIRST_FAIL" ] || FIRST_FAIL="$1 $2: $3"; }

# ---------------------------------------------------------------- QMP helper --
# `qmp CMD [JSON-ARGS]` prints the "return" value; `qmp-hmp LINE` prints the text
# of a monitor command. One connection per call: these run a handful of times.
qmp_ready() { [ -S "$QMP_SOCK" ] && python3 - "$QMP_SOCK" <<'PY' 2>/dev/null
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(sys.argv[1])
f = s.makefile("rw"); f.readline()
f.write(json.dumps({"execute": "qmp_capabilities"}) + "\n"); f.flush()
sys.exit(0 if "return" in json.loads(f.readline()) else 1)
PY
}
qmp_hmp() { python3 - "$QMP_SOCK" "$1" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(5); s.connect(sys.argv[1])
f = s.makefile("rw"); f.readline()
def call(cmd, args=None):
    f.write(json.dumps({"execute": cmd, **({"arguments": args} if args else {})}) + "\n"); f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            return r
call("qmp_capabilities")
r = call("human-monitor-command", {"command-line": sys.argv[2]})
print(r.get("return", "") if "return" in r else "ERROR " + json.dumps(r["error"]))
PY
}
# Round-trip latency of `query-status`, in milliseconds, one per line.
qmp_latency() { python3 - "$QMP_SOCK" "$1" <<'PY'
import json, socket, sys, time
s = socket.socket(socket.AF_UNIX); s.settimeout(5); s.connect(sys.argv[1])
f = s.makefile("rw"); f.readline()
f.write(json.dumps({"execute": "qmp_capabilities"}) + "\n"); f.flush(); f.readline()
end = time.monotonic() + float(sys.argv[2])
while time.monotonic() < end:
    t = time.monotonic()
    f.write(json.dumps({"execute": "query-status"}) + "\n"); f.flush()
    while "return" not in json.loads(f.readline()):
        pass
    print("%.3f" % ((time.monotonic() - t) * 1000.0))
    time.sleep(0.002)
PY
}

echo "audio-triage: qemu=$QEMU_BIN"
"$QEMU_BIN" --version 2>/dev/null | head -1

# --------------------------------------------------------------------- L1 host --
HOST_SINK=""
if command -v pactl >/dev/null; then
  INFO="$(pactl info 2>&1)"
  if printf '%s' "$INFO" | grep -q "^Server Name"; then
    HOST_SINK="$(pactl get-default-sink 2>/dev/null)"
    SERVER="$(printf '%s' "$INFO" | sed -n 's/^Server Name: //p')"
    if [ -z "$HOST_SINK" ]; then
      bad L1 host "sound server answers ($SERVER) but there is no default sink"
    else
      MUTE="$(pactl get-sink-mute "$HOST_SINK" 2>/dev/null | sed 's/.*: //')"
      VOL="$(pactl get-sink-volume "$HOST_SINK" 2>/dev/null | grep -o '[0-9]*%' | head -1)"
      if [ "$MUTE" = "yes" ]; then
        bad L1 host "default sink $HOST_SINK is muted ($SERVER)"
      else
        ok L1 host "server=$SERVER default_sink=$HOST_SINK mute=${MUTE:-?} volume=${VOL:-?}"
      fi
    fi
  else
    bad L1 host "pactl cannot reach a sound server: $(printf '%s' "$INFO" | head -1)"
  fi
else
  skip L1 host "pactl not installed"
fi

# ----------------------------------------------------------------- L2 backend --
BUILD_DIR="$(dirname "$QEMU_BIN")"
BACKENDS="$("$QEMU_BIN" -audiodev help 2>&1 | sed '1d' | sed 's/^ *//' | tr '\n' ' ')"
# The launcher's own choice (vm/boot-x86.sh, "Pick the audio backend"), replayed
# against this binary so the answer is the one a boot would get.
CHOSEN=""
for cand in pipewire pa alsa sdl none; do
  if "$QEMU_BIN" -audiodev help 2>/dev/null | grep -qx " *$cand"; then CHOSEN="$cand"; break; fi
done
CHOSEN="${CHOSEN:-none}"
HOST_LIBS=""
for lib in libpipewire-0.3 libpulse libasound; do
  v="$(pkg-config --modversion "$lib" 2>/dev/null)" && HOST_LIBS="$HOST_LIBS $lib=$v"
done
note L2 backend INFO "compiled in: ${BACKENDS:-none listed}"
note L2 backend INFO "launcher would choose: $CHOSEN"
note L2 backend INFO "dev libraries visible to pkg-config now:${HOST_LIBS:- none}"
for log in "$BUILD_DIR/meson-logs/meson-log.txt" "$BUILD_DIR/../build/meson-logs/meson-log.txt"; do
  if [ -f "$log" ]; then
    note L2 backend INFO "configure log ($log):"
    grep -iE "dependency (libpipewire|libpulse|alsa)[^ ]* .*(found|not found)" "$log" | sort -u | head -6 | sed 's/^/                       /'
    break
  fi
done
if [ "$CHOSEN" = "none" ]; then
  if [ -n "$HOST_SINK" ] || [ -n "$HOST_LIBS" ]; then
    bad L2 backend "this binary has no native audio backend (only: ${BACKENDS:-none}) but the host has one — the launcher falls through to 'none' and the guest is silent. Reconfigure with the dev packages installed (see configure log above)"
  else
    warn L2 backend "launcher resolves to 'none'; host sound not detected either"
  fi
else
  ok L2 backend "binary has '$CHOSEN'; the launcher would use it"
fi

# --------------------------------------------------------- live VM (L3,L4,L8) --
LIVE=0
if qmp_ready; then LIVE=1; fi
if [ "$LIVE" = 1 ]; then
  # The QEMU that serves this socket: same binary name, and its command line
  # names the socket (or the symlink's target, as vm/boot-x86.sh's qmp.sock is).
  SOCK_REAL="$(basename "$(readlink -f "$QMP_SOCK")")"
  PID=""
  for cand in $(pgrep -x "$(basename "$QEMU_BIN")" 2>/dev/null); do
    if tr '\0' '\n' < "/proc/$cand/cmdline" 2>/dev/null | grep -q "$SOCK_REAL"; then PID="$cand"; break; fi
  done
  if [ -n "$PID" ] && [ -r "/proc/$PID/cmdline" ]; then
    CMD="$(tr '\0' '\n' < "/proc/$PID/cmdline")"
    note L3 device INFO "running pid=$PID binary=$(readlink "/proc/$PID/exe" 2>/dev/null)"
    printf '%s\n' "$CMD" | grep -A1 -E '^-audiodev$' | grep -v -- '^--$' | sed 's/^/                       cmdline: /'
    printf '%s\n' "$CMD" | grep -E 'usb-audio|intel-hda|hda-|ich9-intel|AC97|ac97|sb16|es1370' | sed 's/^/                       cmdline: /'
  fi
  # One line per audio device: "TYPE audiodev=VALUE", read from that device's own
  # block of `info qtree` (other devices carry an `audiodev` property too, and an
  # unbound one elsewhere is not this device's problem). A codec carries the
  # property for an HDA controller, so the controller alone is not "bound".
  AUDIO_DEVS="$(qmp_hmp 'info qtree' | python3 -c '
import re, sys
types = ("usb-audio", "hda-duplex", "hda-output", "hda-micro", "AC97", "sb16",
         "es1370", "virtio-sound-pci", "intel-hda", "ich9-intel-hda")
lines = sys.stdin.read().replace("\r", "").split("\n")
for i, line in enumerate(lines):
    m = re.match(r"^( *)dev: ([^,]+),", line)
    if not m or m.group(2) not in types:
        continue
    indent = len(m.group(1)); value = "<absent>"
    for follow in lines[i + 1:]:
        if len(follow) - len(follow.lstrip()) <= indent and follow.strip():
            break
        v = re.match(r"^ *audiodev = \"(.*)\"$", follow)
        if v:
            value = v.group(1)
            break
    print("%s audiodev=%s" % (m.group(2), value if value else "<empty>"))
')"
  if [ -z "$AUDIO_DEVS" ]; then
    bad L3 device "the running VM has no audio device in 'info qtree' (looked for usb-audio, hda-*, intel-hda, AC97, sb16, es1370, virtio-sound)"
  else
    BOUND="$(printf '%s\n' "$AUDIO_DEVS" | grep -v '^\(intel-hda\|ich9-intel-hda\) ' | grep -v 'audiodev=<\(empty\|absent\)>')"
    if [ -z "$BOUND" ]; then
      bad L3 device "audio device(s) exist but none is bound to a backend ($(printf '%s' "$AUDIO_DEVS" | tr '\n' ';')): they would play to nothing"
    else
      ok L3 device "$(printf '%s' "$AUDIO_DEVS" | tr '\n' ';')"
    fi
  fi
  USB="$(qmp_hmp 'info usb')"; PCI="$(qmp_hmp 'info pci')"
  if printf '%s\n%s\n' "$USB" "$PCI" | grep -qiE 'audio|multimedia audio|HDA|Intel 82801'; then
    ok L4 enumeration "on the bus: $(printf '%s\n%s\n' "$USB" "$PCI" | grep -iE 'audio|HDA|82801' | head -2 | tr -s ' ' | tr '\n' ';')"
  else
    bad L4 enumeration "'info usb' / 'info pci' list no audio device, so the guest cannot enumerate one"
  fi
else
  skip L3 device "no QMP socket at $QMP_SOCK (start the VM, or pass --qmp)"
  skip L4 enumeration "needs the running VM"
fi

# ------------------------------------------------------------------- L5 guest --
if [ -n "$GUEST" ]; then
  if timeout 30 ssh -o BatchMode=yes "$GUEST" true 2>/dev/null; then
    PROF="$(timeout 60 ssh -o BatchMode=yes "$GUEST" 'system_profiler SPAudioDataType 2>/dev/null' 2>/dev/null)"
    ENG="$(timeout 30 ssh -o BatchMode=yes "$GUEST" 'ioreg -rc IOAudioEngine 2>/dev/null | grep -c IOAudioEngine' 2>/dev/null)"
    if printf '%s' "$PROF" | grep -qiE 'Output|Devices:'; then
      ok L5 guest "macOS lists an audio device; IOAudioEngine count=${ENG:-?}: $(printf '%s' "$PROF" | grep -E '^    [A-Za-z]' | head -2 | tr -s ' ' | tr '\n' ';')"
    else
      bad L5 guest "macOS lists no audio device (system_profiler SPAudioDataType empty; IOAudioEngine count=${ENG:-?}) — the device is on the bus but no driver bound"
    fi
  else
    warn L5 guest "ssh $GUEST failed; cannot inspect the guest"
    GUEST=""
  fi
else
  skip L5 guest "no --guest"
fi

# ----------------------------------------------------------------- L6/L7 flow --
if command -v pactl >/dev/null && [ -n "$HOST_SINK" ]; then
  CAP=""
  if command -v parec >/dev/null && command -v ffmpeg >/dev/null; then
    CAP="$(mktemp -d)/cap.wav"
    parec --device="$HOST_SINK.monitor" --format=s16le --rate=44100 --channels=2 \
      --file-format=wav "$CAP" 2>/dev/null &
    CAP_PID=$!
  fi
  if [ -n "$GUEST" ]; then
    timeout $((SECS + 15)) ssh -o BatchMode=yes "$GUEST" \
      "for i in 1 2 3 4 5 6; do afplay /System/Library/Sounds/Glass.aiff; done" >/dev/null 2>&1 &
    PLAY_PID=$!
  else
    echo "audio-triage: play a sound in the guest now (observing for ${SECS}s)"
  fi
  SEEN=0; STATES=""
  END=$((SECONDS + SECS))
  while [ "$SECONDS" -lt "$END" ]; do
    LIST="$(pactl list sink-inputs 2>/dev/null)"
    if printf '%s' "$LIST" | grep -qi 'qemu'; then
      SEEN=1
      STATES="$STATES $(printf '%s' "$LIST" | awk '/Sink Input/{c=0} /Corked:/{c=$2} /application.name = .*[Qq][Ee][Mm][Uu]/{print "corked=" c}' | head -1)"
    fi
    sleep 0.5
  done
  [ -n "${PLAY_PID:-}" ] && { kill "$PLAY_PID" 2>/dev/null; timeout 20 ssh -o BatchMode=yes "$GUEST" 'pkill afplay' >/dev/null 2>&1; }
  if [ "$SEEN" = 1 ]; then
    ok L6 stream "QEMU created a host stream ($(printf '%s' "$STATES" | tr -s ' ' | sed 's/^ //'| awk '{print $1}'))"
  else
    bad L6 stream "no QEMU stream appeared on the host during ${SECS}s of $([ -n "$GUEST" ] && echo guest playback || echo observation) — the guest never started output, or QEMU's backend could not open one"
  fi
  if [ -n "$CAP" ]; then
    kill "$CAP_PID" 2>/dev/null; wait "$CAP_PID" 2>/dev/null
    MEAN="$(ffmpeg -hide_banner -nostats -i "$CAP" -af volumedetect -f null - 2>&1 | sed -n 's/.*max_volume: \(-\?[0-9.]*\) dB.*/\1/p' | tail -1)"
    if [ -z "$MEAN" ] || [ "${MEAN%.*}" -lt -60 ] 2>/dev/null; then
      if [ "$SEEN" = 1 ]; then
        bad L7 playback "a stream exists but the sink monitor carries silence (max ${MEAN:-none} dB): the stream is corked, routed to another sink, or the guest is sending zeroes"
      else
        skip L7 playback "no stream, so nothing to hear (max ${MEAN:-none} dB)"
      fi
    else
      ok L7 playback "sink monitor carried signal (max ${MEAN} dB)"
    fi
  else
    skip L7 playback "needs parec and ffmpeg"
  fi
else
  skip L6 stream "needs pactl and a default sink"
  skip L7 playback "needs pactl and a default sink"
fi

# ------------------------------------------------------------- L8 scheduling --
if [ "$LIVE" = 1 ]; then
  qmp_latency "$SECS" | sort -n > "${TMPDIR:-/tmp}/audio-triage-lat.$$"
  N="$(wc -l < "${TMPDIR:-/tmp}/audio-triage-lat.$$")"
  if [ "$N" -lt 10 ]; then
    warn L8 scheduling "only $N QMP round trips completed in ${SECS}s: the main loop was not answering"
  else
    pick() { awk -v q="$1" -v n="$N" 'NR==int((n-1)*q)+1{print; exit}' "${TMPDIR:-/tmp}/audio-triage-lat.$$"; }
    P50="$(pick 0.50)"; P95="$(pick 0.95)"; P99="$(pick 0.99)"; MAX="$(tail -1 "${TMPDIR:-/tmp}/audio-triage-lat.$$")"
    MSG="n=$N p50=${P50}ms p95=${P95}ms p99=${P99}ms max=${MAX}ms (audio timer period 10ms)"
    # A QMP round trip is a floor on what the main loop can promise any timer.
    if awk -v p="$P99" -v m="$MAX" 'BEGIN{exit !(p>10 || m>50)}'; then
      warn L8 scheduling "main loop is slower than the audio timer period: $MSG"
      FIRST_SCHED="$MSG"
    else
      ok L8 scheduling "$MSG"
    fi
  fi
  rm -f "${TMPDIR:-/tmp}/audio-triage-lat.$$"
else
  skip L8 scheduling "needs the running VM's QMP socket"
fi

echo
if [ -n "$FIRST_FAIL" ]; then
  echo "VERDICT first failing layer -> $FIRST_FAIL"
  exit 1
elif [ -n "${FIRST_SCHED:-}" ]; then
  echo "VERDICT no layer failed outright; L8 scheduling is slow -> $FIRST_SCHED"
  echo "         capture who is holding the BQL: add  -D /tmp/qemu-trace.log -trace reims_vgpu_dirty_run  to the launcher"
  exit 1
fi
echo "VERDICT no layer failed"
