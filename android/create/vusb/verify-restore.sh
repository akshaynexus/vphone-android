#!/bin/bash
# Drive a real restore on this x86 Linux host through the Android transport: no usbip, no vhci_hcd,
# no root. The stock cloudOS restore of tools/gpu-driver-from-cloudos.sh, over
#   QEMU vusb chardev -> vusbd -> libusb-1.0 replacement -> usbmuxd (32 KiB) + vphone-cli
#
#   android/create/vusb/verify-restore.sh <cloudOS.ipsw>     # the restore
#   PROBE_ONLY=1 android/create/vusb/verify-restore.sh       # stop after DFU answers
#
# Env: VC (a host vphone-cli linked dynamically against libusb-1.0.so.0), QEMU, OPENAVB, OPENSEP,
#      SMP (8), MEM (8G), KEEP=1 (keep the VM), VUSB_DEBUG=1 (log every mux packet).
#      FAST_MTREE=1: set the restore ramdisk's fast-mtree option first, as the app does before a
#        restore (needs android/scripts/build-create-steps.sh's host examples; see below).
#      PAUSE_AT=<seconds> PAUSE_FOR=<seconds>: that long into the image write (after the tool's
#        "Sending filesystem now..."; the write takes about a minute here), pause the way the app
#        does (QMP stop, then SIGSTOP of the broker, usbmuxd and the restore tool), and resume
#        after PAUSE_FOR. The restore must carry on and finish. The tool's own time
#        limits only leave the pause out in a vphone-cli built with android/create/swift's edits;
#        the broker, usbmuxd and glue here always do.
# A guest whose kernel writes to the serial line has its restore log in serial.log (serial=3 is
# asked for; the stock cloudOS restore kernel stays quiet).
# Everything is under android/build/vusb/verify (git-ignored). Only processes started here are stopped.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
cd "$ROOT"
IPSW=${1:-}
SIBLING=$ROOT/../vphone-linux
VC=${VC:-$SIBLING/linux-create/.build/out/out/Products/Debug-linux-x86_64/vphone-cli}
QEMU=${QEMU:-$ROOT/android/build/qemu-host/qemu-system-aarch64}
ROMS=$SIBLING/cpu-tree/vphone-cli-2.13.4/VPhoneExecutable/VPhoneVirtualization/Resources
export OPENAVB=${OPENAVB:-$ROMS/OpenAVB/OpenAVB.bin} OPENSEP=${OPENSEP:-$ROMS/OpenSEP/OpenSEP.bin}
HOST=$ROOT/android/build/vusb/host
WORK=$ROOT/android/build/vusb/verify
NAME=restore-$$
VM=$WORK/vms/$NAME
RUN=$WORK/runs/$NAME
QSOCK=$RUN/vusb.sock
QMP=$RUN/qmp.sock
BSOCK=$RUN/vusbd.sock
MSOCK=$RUN/usbmuxd.sock

OPTIONS_TOOL=$ROOT/android/build/create-steps/target/release/examples/restore_options
if [ "${FAST_MTREE:-0}" = 1 ] && [ ! -x "$OPTIONS_TOOL" ]; then
  echo "missing $OPTIONS_TOOL: (cd android/create/installer && CARGO_TARGET_DIR=\$ROOT/android/build/create-steps/target cargo build --release --examples)" >&2
  exit 1
fi
for f in "$VC" "$QEMU" "$HOST/vusbd" "$HOST/usbmuxd" "$HOST/libusb-1.0.so.0"; do
  [ -x "$f" ] || { echo "missing $f" >&2; exit 1; }
done
[ -n "${PROBE_ONLY:-}" ] || [ -f "$IPSW" ] || { echo "usage: $0 <cloudOS.ipsw>" >&2; exit 2; }
mkdir -p "$WORK/vms" "$RUN"

pids=()
cleanup() {
  for p in $(pgrep -x qemu-system-aar || true); do
    tr '\0' ' ' </proc/$p/cmdline 2>/dev/null | grep -q -- "$QSOCK" && kill "$p"
  done
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  if [ "${KEEP:-0}" != 1 ]; then rm -rf "$VM"; fi
}
trap cleanup EXIT

echo "=== temporary VM $VM"
tools/new-vm.sh "$VM" >/dev/null
if [ -z "${PROBE_ONLY:-}" ]; then
  echo "=== unzipping $(basename "$IPSW") as its restore tree"
  unzip -q "$IPSW" -d "$VM/iPhonePCC_Restore"
  python3 -I - "$VM/iPhonePCC_Restore/BuildManifest.plist" <<'PY'
import plistlib, sys
p = sys.argv[1]
m = plistlib.load(open(p, "rb"))
types = m.get("SupportedProductTypes", [])
if "iPhone99,11" not in types:
    m["SupportedProductTypes"] = types + ["iPhone99,11"]
    plistlib.dump(m, open(p, "wb"), fmt=plistlib.FMT_XML)
PY
  if [ "${FAST_MTREE:-0}" = 1 ]; then
    echo "=== restore options"
    "$OPTIONS_TOOL" "$VM"
  fi
fi

qmp() { # command: one QMP command to this run's QEMU
  python3 -I - "$QMP" "$1" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(30); s.connect(sys.argv[1])
f = s.makefile("rw")
f.readline()
for command in ("qmp_capabilities", sys.argv[2]):
    f.write(json.dumps({"execute": command}) + "\n"); f.flush()
    while True:
        answer = json.loads(f.readline())
        if "return" in answer or "error" in answer:
            break
    if "error" in answer:
        sys.exit(f"QMP {command}: {answer['error']}")
PY
}

# The restore tool's process group (restore-wait.sh starts it with setsid), found by this run's VM name.
restore_tool_group() {
  local p
  for p in $(pgrep -x vphone-cli || true); do
    tr '\0' ' ' </proc/"$p"/cmdline 2>/dev/null | grep -q -- " $NAME " && { echo "$p"; return; }
  done
}

# The app's pause, PAUSE_AT seconds into the image write, for PAUSE_FOR seconds.
pause_during_the_write() {
  until grep -q "Sending filesystem now" "$RUN/restore.log" 2>/dev/null; do
    sleep 1
    kill -0 $$ 2>/dev/null || return 0
  done
  sleep "$PAUSE_AT"
  if grep -q "Done sending filesystem" "$RUN/restore.log"; then
    echo "=== the image write ended before PAUSE_AT=$PAUSE_AT s: not paused" >&2
    return 0
  fi
  local tool; tool=$(restore_tool_group)
  echo "=== pausing $PAUSE_AT s into the image write for $PAUSE_FOR s ($(date +%T))"
  qmp stop
  kill -STOP "${pids[@]}"
  [ -z "$tool" ] || kill -STOP -- -"$tool"
  sleep "$PAUSE_FOR"
  echo "=== resuming ($(date +%T))"
  [ -z "$tool" ] || kill -CONT -- -"$tool"
  kill -CONT "${pids[@]}"
  qmp cont
}

echo "=== QEMU in DFU ($QEMU)"
(QEMU=$QEMU CID=0 SMP=${SMP:-8} GFX=0 WINDOW=0 DFLAGS=guest_errors LOG=$RUN/qemu.log \
  timeout 14400 ./run-vphone.sh "$VM" -chardev "socket,id=vusb,path=$QSOCK,server=on,wait=off" \
  -global vresearch-vusb.chardev=vusb -qmp "unix:$QMP,server=on,wait=off" >"$RUN/serial.log" 2>"$RUN/stderr.log" &)
for _ in $(seq 1 30); do [ -S "$QSOCK" ] && break; sleep 1; done
[ -S "$QSOCK" ] || { tail "$RUN/stderr.log" >&2; echo "QEMU did not start" >&2; exit 1; }

echo "=== broker and usbmuxd"
"$HOST/vusbd" --chardev "$QSOCK" --listen "$BSOCK" >"$RUN/vusbd.log" 2>&1 &
pids+=($!)
export VPHONE_VUSB_SOCKET=$BSOCK
LD_LIBRARY_PATH=$HOST "$HOST/usbmuxd" -f -v -S "$MSOCK" -P "$RUN/usbmuxd.pid" -C "$RUN/lockdown" \
  >"$RUN/usbmuxd.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 20); do [ -S "$MSOCK" ] && break; sleep 0.5; done
[ -S "$MSOCK" ] || { tail "$RUN/usbmuxd.log" >&2; echo "usbmuxd did not start" >&2; exit 1; }
export USBMUXD_SOCKET_ADDRESS=UNIX:$MSOCK
export LD_LIBRARY_PATH=$HOST

echo "=== waiting for DFU"
"$VC" recovery-probe --timeout 180 || { tail "$RUN/vusbd.log" >&2; exit 1; }
if [ -n "${PROBE_ONLY:-}" ]; then
  echo "=== DFU answered"; cat "$RUN/vusbd.log"; exit 0
fi

echo "=== restoring (log: $RUN/restore.log)"
if [ -n "${PAUSE_AT:-}" ]; then
  pause_during_the_write &
fi
started=$SECONDS
VPHONE_RESTORE_BOOT_ARGS_EXTRA=${VPHONE_RESTORE_BOOT_ARGS_EXTRA-serial=3} \
VPHONE_RESTORE_TIMEOUT_SCALE=${VPHONE_RESTORE_TIMEOUT_SCALE:-10} \
  tools/restore-wait.sh "$RUN/restore.log" "$VC" restore -l "$WORK/vms" "$NAME" -v \
  || { tail -30 "$RUN/restore.log" >&2; echo "restore failed; run dir $RUN" >&2; KEEP=1; exit 1; }
grep -qi 'restore finished' "$RUN/restore.log" || { tail -30 "$RUN/restore.log" >&2; echo "restore did not finish; run dir $RUN" >&2; exit 1; }
mtree=$(grep -a -o 'Fast mtree path [a-z ]*enabled' "$RUN/serial.log" | tail -1 || true)
echo "=== the guest on its fast mtree path: ${mtree:-it did not say (no guest log on the serial line)}"
echo "=== restore finished in $((SECONDS - started)) s; firmware store magic at 0x4000 of nvram.bin:"
xxd -s 0x4000 -l 16 "$VM/nvram.bin"
