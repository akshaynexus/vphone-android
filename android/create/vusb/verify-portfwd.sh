#!/bin/bash
# Exercise vphone_portfwd against a live guest: boot the RestoreOS of a VM that verify-restore.sh kept
# (KEEP=1), forward its usbmux port 62078 (restored's lockdown-style service), and ask it QueryType.
#
#   android/create/vusb/verify-portfwd.sh <vm-dir>
#
# Zeroes the VM's nvram.bin and SEPStorage so that it enters DFU; the VM's disk is not restored again.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
cd "$ROOT"
VM=$(realpath "${1:?usage: verify-portfwd.sh <vm-dir>}")
NAME=$(basename "$VM")
LIB=$(dirname "$VM")
VC=${VC:-$ROOT/../vphone-linux/linux-create/.build/out/out/Products/Debug-linux-x86_64/vphone-cli}
QEMU=${QEMU:-$ROOT/android/build/qemu-host/qemu-system-aarch64}
HOST=$ROOT/android/build/vusb/host
RUN=$ROOT/android/build/vusb/verify/runs/pf-$$
QSOCK=$RUN/vusb.sock BSOCK=$RUN/vusbd.sock MSOCK=$RUN/usbmuxd.sock PF=$RUN/pf.sock
mkdir -p "$RUN"
pids=()
cleanup() {
  for p in $(pgrep -x qemu-system-aar || true); do
    tr '\0' ' ' </proc/$p/cmdline 2>/dev/null | grep -q -- "$QSOCK" && kill "$p"
  done
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

truncate -s 0 "$VM/nvram.bin" "$VM/SEPStorage"
truncate -s 33570816 "$VM/nvram.bin"
truncate -s 512K "$VM/SEPStorage"

(QEMU=$QEMU CID=0 SMP=${SMP:-8} GFX=0 WINDOW=0 timeout 3600 ./run-vphone.sh "$VM" \
  -chardev "socket,id=vusb,path=$QSOCK,server=on,wait=off" -global vresearch-vusb.chardev=vusb \
  >"$RUN/serial.log" 2>"$RUN/stderr.log" &)
for _ in $(seq 1 30); do [ -S "$QSOCK" ] && break; sleep 1; done
"$HOST/vusbd" --chardev "$QSOCK" --listen "$BSOCK" >"$RUN/vusbd.log" 2>&1 &
pids+=($!)
export VPHONE_VUSB_SOCKET=$BSOCK
LD_LIBRARY_PATH=$HOST "$HOST/usbmuxd" -f -v -S "$MSOCK" -P "$RUN/usbmuxd.pid" -C "$RUN/lockdown" >"$RUN/usbmuxd.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 20); do [ -S "$MSOCK" ] && break; sleep 0.5; done
"$HOST/vphone_portfwd" "$MSOCK" "unix:$PF" 62078 >"$RUN/portfwd.log" 2>&1 &
pids+=($!)
export USBMUXD_SOCKET_ADDRESS=UNIX:$MSOCK
LD_LIBRARY_PATH=$HOST "$VC" recovery-probe --timeout 180
LD_LIBRARY_PATH=$HOST VPHONE_RESTORE_TIMEOUT_SCALE=10 setsid "$VC" restore -l "$LIB" "$NAME" --boot-ramdisk -v \
  >"$RUN/restore.log" 2>&1 &
pids+=($!)
echo "waiting for the RestoreOS device"
for _ in $(seq 1 600); do grep -q '05ac:12a[8c]' "$RUN/vusbd.log" && break; sleep 2; done
grep -E 'discovered' "$RUN/vusbd.log"
python3 -I - "$PF" <<'PY'
import plistlib, socket, struct, sys, time
req = plistlib.dumps({"Request": "QueryType"}, fmt=plistlib.FMT_XML)
deadline = time.time() + 300
last = None
while time.time() < deadline:
    try:
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(30)
        s.connect(sys.argv[1])
        s.sendall(struct.pack(">I", len(req)) + req)
        n = struct.unpack(">I", s.recv(4, socket.MSG_WAITALL))[0]
        body = s.recv(n, socket.MSG_WAITALL)
        print("port 62078 answered:", plistlib.loads(body))
        sys.exit(0)
    except Exception as e:
        last = e
        time.sleep(3)
print("no answer:", last)
sys.exit(1)
PY
