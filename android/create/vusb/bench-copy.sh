#!/bin/bash
# Measure the custom firmware install's copies on this x86 Linux host, against a real SSH ramdisk
# VM: a VM restored by verify-restore.sh (KEEP=1), booted into the SSH ramdisk the way the app's
# cfw step does it, over the Android transport (QEMU vusb chardev -> vusbd -> usbmuxd).
#
#   KEEP=1 MEM=6G SMP=6 android/create/vusb/verify-restore.sh <cloudOS.ipsw>   # leaves vms/restore-N
#   android/create/vusb/bench-copy.sh up <vm directory>      # SSH ramdisk up, volumes mounted
#   android/create/vusb/bench-copy.sh tree [<megabytes>]     # a test tree: large files and many small
#   android/create/vusb/bench-copy.sh ssh <vm> <streams>     # the copy as tar over SSH
#   android/create/vusb/bench-copy.sh disk <vm> <readers>    # the copy by the transfer disk
#   android/create/vusb/bench-copy.sh sh <vm> '<command>'    # a command in the ramdisk
#   android/create/vusb/bench-copy.sh down <vm>              # unmount, stop what `up` started
#   android/create/vusb/bench-copy.sh check <vm>             # the stopped VM's disk against the tree
#
# Env: BENCH_TREE (another tree to copy), QEMU, VC, SMP (6), MEM (6G), TRANSFER=0 (no transfer disk), CACHE_UNSAFE=1 (the VM's drives
#      with cache=unsafe), BENCH_PROGRESS=1 (print the progress lines).
# Needs the host examples: (cd android/create/installer &&
#   CARGO_TARGET_DIR=$ROOT/android/build/create-steps/target cargo build --release --example copy_bench)
# Everything is under android/build/vusb/bench (git-ignored). Only processes started here are stopped.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
cd "$ROOT"
SIBLING=$ROOT/../vphone-linux
VC=${VC:-$SIBLING/linux-create/.build/out/out/Products/Debug-linux-x86_64/vphone-cli}
QEMU=${QEMU:-$ROOT/android/build/qemu-host/qemu-system-aarch64}
HOST=$ROOT/android/build/vusb/host
BENCH=$ROOT/android/build/create-steps/target/release/examples/copy_bench
WORK=$ROOT/android/build/vusb/bench
TREE=${BENCH_TREE:-$WORK/tree}
GUEST_DEST=/mnt1/private/var/bench-copy
verb=${1:?usage: bench-copy.sh up|tree|ssh|disk|sh|down|check ...}
[ -x "$BENCH" ] || { echo "missing $BENCH (see the head of this script)" >&2; exit 1; }

vm_paths() {
  VM=$(cd "${1:?a VM directory}" && pwd)
  RUN=$WORK/run-$(basename "$VM")
  MSOCK=$RUN/usbmuxd.sock
  IMAGE=$VM/.cfw-transfer.img
}

case $verb in
tree)
  mb=${2:-2048}; set +o pipefail
  rm -rf "$TREE"; mkdir -p "$TREE/big" "$TREE/small"
  # Large files that do not compress, as a dyld cache's do not, and a system's own small ones.
  for i in 1 2 3 4; do
    # (openssl ends on a closed pipe: that is how it is stopped)
    { openssl enc -aes-128-ctr -nosalt -pass "pass:bench$i" -in /dev/zero 2>/dev/null || true; } | head -c $((mb * 1048576 / 4)) > "$TREE/big/cache.$i"
  done
  cp -r /usr/include "$TREE/small/include"
  mkdir "$TREE/small/lib"; find /usr/lib -maxdepth 1 -type f -name '*.so*' -size -4M | head -400 | xargs -I{} cp {} "$TREE/small/lib/"
  ln -s ../big/cache.1 "$TREE/small/link-to-cache"
  chmod 4755 "$TREE/small/lib/$(ls "$TREE/small/lib" | head -1)"
  echo "$TREE: $(find "$TREE" -type f | wc -l) files, $(du -sm "$TREE" | cut -f1) MB"
  ;;
up)
  vm_paths "${2:-}"
  for f in "$VC" "$QEMU" "$HOST/vusbd" "$HOST/usbmuxd"; do [ -x "$f" ] || { echo "missing $f" >&2; exit 1; }; done
  mkdir -p "$RUN"; : > "$RUN/pids"
  export LD_LIBRARY_PATH=$HOST
  # A VM restored by verify-restore.sh has the stock cloudOS tree: its boot chain takes no changed
  # ramdisk, and its manifest has an identity for every board. Once: the boot chain patched as the
  # app's `patch` step does it, and the manifest cut down to the VM's own identity with the
  # runtime parts the patched chain boots (the research vphone600 kernel and its device tree), as
  # the hybrid manifest of a created VM has them.
  if [ ! -f "$VM/.bench-tree-ready" ]; then
    echo "=== patching the stock tree's boot chain (log: $RUN/fw-patch.log)"
    "$VC" fw patch -l "$(dirname "$VM")" "$(basename "$VM")" >"$RUN/fw-patch.log" 2>&1 \
      || { tail -5 "$RUN/fw-patch.log" >&2; echo "fw patch failed" >&2; exit 1; }
    python3 -I - "$VM"/*_Restore/BuildManifest.plist <<'PY'
import plistlib, shutil, sys
p = sys.argv[1]
shutil.copy(p, p + ".stock")
m = plistlib.load(open(p, "rb"))
own = [b for b in m["BuildIdentities"]
       if b["Info"]["DeviceClass"] == "vresearch101ap" and "Research" not in b["Info"].get("Variant", "")]
if len(own) != 1:
    sys.exit(f"{p}: {len(own)} vresearch101ap identities")
manifest = own[0]["Manifest"]
manifest["KernelCache"]["Info"]["Path"] = "kernelcache.research.vphone600"
for key in ("DeviceTree", "RestoreDeviceTree"):
    manifest[key]["Info"]["Path"] = "Firmware/all_flash/DeviceTree.vphone600ap.im4p"
m["BuildIdentities"] = own
plistlib.dump(m, open(p, "wb"), fmt=plistlib.FMT_XML)
PY
    touch "$VM/.bench-tree-ready"
  fi
  echo "=== SSH ramdisk and tree"
  "$BENCH" prep "$VM" "$VC"
  # A stock cloudOS VM's installed iBoot does not come up to its recovery prompt on this machine
  # (it crashes after iLLB), so the ramdisk is booted from DFU instead: OpenAVB drops into DFU when
  # nvram.bin holds no firmware. The restored one is kept beside it.
  if [ "${BLANK_NVRAM:-1}" = 1 ]; then
    [ -f "$VM/nvram.bin.restored" ] || cp "$VM/nvram.bin" "$VM/nvram.bin.restored"
    rm -f "$VM/nvram.bin"; truncate -s 33570816 "$VM/nvram.bin"
  fi
  extra=()
  if [ "${TRANSFER:-1}" = 1 ]; then
    rm -f "$IMAGE"; truncate -s 64G "$IMAGE"
    printf 'vphone-transfer-%s\n' "$(head -c 16 /dev/urandom | xxd -p)" | dd of="$IMAGE" bs=1M seek=65535 conv=notrunc status=none
    extra+=(-drive "file=$IMAGE,if=none,id=transfer,format=raw,readonly=on,file.locking=off"
            -device "vmapple-virtio-blk-pci,variant=root,drive=transfer")
  else
    rm -f "$IMAGE"
  fi
  runner=$ROOT/run-vphone.sh
  if [ "${CACHE_UNSAFE:-0}" = 1 ]; then
    runner=$RUN/run-vphone-unsafe.sh
    sed 's/file.locking=off"/file.locking=off,cache=unsafe"/' "$ROOT/run-vphone.sh" > "$runner"; chmod +x "$runner"
  fi
  echo "=== QEMU in DFU"
  (QEMU=$QEMU CID=0 SMP=${SMP:-6} MEM=${MEM:-6G} GFX=0 WINDOW=0 DFLAGS=guest_errors LOG=$RUN/qemu.log \
    setsid "$runner" "$VM" -chardev "socket,id=vusb,path=$RUN/vusb.sock,server=on,wait=off" \
    -global vresearch-vusb.chardev=vusb -qmp "unix:$RUN/qmp.sock,server=on,wait=off" "${extra[@]}" \
    >"$RUN/serial.log" 2>"$RUN/stderr.log" & echo $! > "$RUN/qemu.group")
  for _ in $(seq 1 30); do [ -S "$RUN/vusb.sock" ] && break; sleep 1; done
  [ -S "$RUN/vusb.sock" ] || { tail "$RUN/stderr.log" >&2; echo "QEMU did not start" >&2; exit 1; }
  "$HOST/vusbd" --chardev "$RUN/vusb.sock" --listen "$RUN/vusbd.sock" >"$RUN/vusbd.log" 2>&1 &
  echo $! >> "$RUN/pids"
  export VPHONE_VUSB_SOCKET=$RUN/vusbd.sock
  "$HOST/usbmuxd" -f -v -S "$MSOCK" -P "$RUN/usbmuxd.pid" -C "$RUN/lockdown" >"$RUN/usbmuxd.log" 2>&1 &
  echo $! >> "$RUN/pids"
  for _ in $(seq 1 20); do [ -S "$MSOCK" ] && break; sleep 0.5; done
  export USBMUXD_SOCKET_ADDRESS=UNIX:$MSOCK
  "$VC" recovery-probe --timeout 180
  echo "=== booting the SSH ramdisk (log: $RUN/boot-ramdisk.log)"
  VPHONE_RESTORE_BOOT_ARGS_EXTRA=serial=3 VPHONE_RESTORE_TIMEOUT_SCALE=10 \
    setsid "$VC" restore -l "$(dirname "$VM")" "$(basename "$VM")" --boot-ramdisk -v >"$RUN/boot-ramdisk.log" 2>&1 </dev/null &
  tool=$!
  started=$SECONDS
  "$BENCH" sh "$MSOCK" id
  kill -- -"$tool" 2>/dev/null || kill "$tool" 2>/dev/null || true
  echo "=== SSH answered after $((SECONDS - started)) s; mounting"
  "$BENCH" mount "$MSOCK" "$VM"
  "$BENCH" sh "$MSOCK" 'ls /dev/disk*; /sbin/mount'
  ;;
sh)
  vm_paths "${2:-}"
  "$BENCH" sh "$MSOCK" "${3:?a command}"
  ;;
ssh)
  vm_paths "${2:-}"
  "$BENCH" copy "$MSOCK" "$TREE" "$GUEST_DEST" "${3:-1}"
  ;;
disk)
  vm_paths "${2:-}"
  BENCH_DISK_READERS=${3:-1} "$BENCH" copy "$MSOCK" "$TREE" "$GUEST_DEST" 1 "$IMAGE"
  ;;
down)
  vm_paths "${2:-}"
  timeout 120 "$BENCH" sh "$MSOCK" 'sync; /sbin/umount /mnt2; /sbin/umount /mnt6; /sbin/umount /mnt1; sync' || true
  if [ -f "$RUN/qemu.group" ]; then kill -- -"$(cat "$RUN/qemu.group")" 2>/dev/null || true; fi
  while read -r p; do kill "$p" 2>/dev/null || true; done < "$RUN/pids"
  rm -f "$IMAGE" "$RUN/qemu.group"; : > "$RUN/pids"
  ;;
check)
  vm_paths "${2:-}"
  "$BENCH" check "$VM/Disk.img" "${3:-System}" "${GUEST_DEST#/mnt1}" "$TREE"
  ;;
*)
  echo "unknown verb $verb" >&2; exit 2
  ;;
esac
