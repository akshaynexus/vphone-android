#!/system/bin/sh
# vPhone's root boost: run as root (KernelSU, Magisk or APatch's su) for as long as a VM runs in
# the app's process, then everything it changed is given back.
#
#   sh root-boost.sh PID DIR
#
# PID is the app's process (QEMU runs in it), DIR the app's files directory: the stop file the
# app creates when the VM ends and the saved values live there.
#
# What it does, after DroidVM (tools/rig/phone-cpu-pin.sh, CrosvmBackendInstance's cpusets and
# real-time switch) and the performance modules emulator users run as root:
#   clocks   every CPU policy's limit back to its hardware highest, the files then read-only, so
#            a vendor's power daemon (OPLUS held the big cores at 1.7 GHz for DroidVM; the
#            OnePlus 15 held these vCPUs at 1.3-2.4 of 3.6-4.6) cannot pull them down again; the
#            governor still chooses within them. Qualcomm's core_ctl keeps every core awake.
#            Nothing is pinned high: the S24 Ultra's 0.7.1 log, with the lowest CPU, GPU and bus
#            clocks pinned at their highest, ran the vCPUs at 3.1 GHz for half a minute and then
#            at 2.0 for the rest of the boot. Heat the guest does not use takes clock from the
#            vCPUs.
#   threads  vCPU, GPU-device and main-loop threads in Android's top-app cpuset (all cores; its
#            foreground cpuset left the S24 Ultra's X4 out), at nice -20; the vCPUs kept off slow
#            little cores only where the other cores outnumber them. In 0.7.1 seven vCPUs were
#            kept on five cores: each ran two thirds of the time and the guest kernel spun on its
#            locks (200-570 million WFE a second) while the holders waited for a core. QEMU's
#            main loop is real-time (SCHED_FIFO 1): it runs in short bursts and every timer, disk
#            completion and touch passes through it. The vCPUs stay off real time: busy
#            real-time threads would starve Android's own (DroidVM refuses real time without a
#            cpuset for the same reason).
#   memory   the app is not ended by the low-memory killer.
# The kernel's own thermal limits stay in force. When the battery reaches HOT degrees the clocks
# are given back, and taken again below COOL.
set -u
PID=$1
DIR=$2
SYS=${VPHONE_SYS:-/sys}
PROC=${VPHONE_PROC:-/proc}
CPUSET=${VPHONE_CPUSET:-/dev/cpuset}
HOT=${VPHONE_HOT_C:-44}
COOL=${VPHONE_COOL_C:-40}
TICK=${VPHONE_TICK:-2}
STOP=$DIR/root-boost.stop
# The app may be gone before this script: nothing it prints may end it.
trap '' PIPE HUP

say() { echo "root boost: $*" 2>/dev/null; }

# Out of the app's process group, so that Android ending the app does not end this script before
# it has given everything back.
echo $$ > $SYS/fs/cgroup/cgroup.procs 2>/dev/null
echo $$ > $CPUSET/tasks 2>/dev/null

started() { sed 's/.*) //' $PROC/$PID/stat 2>/dev/null | cut -d' ' -f20; }
START=$(started)
if [ -z "$START" ]; then
  say "OFF (process $PID is not running)"
  exit 1
fi
alive() { [ "$(started)" = "$START" ]; }

FAILED=0
FAILS=""
# put GROUP FILE VALUE: the file's value and mode saved once in DIR/root-boost.GROUP, then VALUE.
put() {
  [ -e "$2" ] || return 1
  saved=$DIR/root-boost.$1
  if ! grep -q "^$2 " "$saved" 2>/dev/null; then
    old=$(cat "$2" 2>/dev/null)
    case $old in *\[*) old=$(echo "$old" | sed 's/.*\[\(.*\)\].*/\1/') ;; esac
    echo "$2 $(stat -c %a "$2" 2>/dev/null || echo 644) $old" >> "$saved"
  fi
  chmod 644 "$2" 2>/dev/null
  if ! echo "$3" > "$2" 2>/dev/null; then
    FAILED=$((FAILED + 1))
    [ $FAILED -le 3 ] && FAILS="$FAILS $2"
    return 1
  fi
}

# give GROUP: the saved values back, twice (a lowest clock cannot go above a highest, whichever
# was first), then the files' modes.
give() {
  saved=$DIR/root-boost.$1
  [ -f "$saved" ] || return 0
  for pass in 1 2; do
    while read -r path mode value; do
      chmod 644 "$path" 2>/dev/null
      echo "$value" > "$path" 2>/dev/null
    done < "$saved"
  done
  while read -r path mode value; do chmod "$mode" "$path" 2>/dev/null; done < "$saved"
  rm -f "$saved"
}

# Left by a run that was ended before it gave everything back (0.7.1's memory group included).
for g in clocks memory; do give $g; done

khz_ghz() { awk -v k="$1" 'BEGIN { printf "%.1f", k / 1e6 }'; }

# The clusters: a cluster below 70% of the fastest one's clock (the SM8650's A520s at 2.3 of
# 3.4 GHz) is a little one; the OnePlus 15's 3.6 of 4.6 GHz cores are not.
TOP=0
for p in $SYS/devices/system/cpu/cpufreq/policy*; do
  [ -f $p/cpuinfo_max_freq ] || continue
  m=$(cat $p/cpuinfo_max_freq)
  [ "$m" -gt "$TOP" ] && TOP=$m
done
BIG=""
ALLCPUS=""
for p in $SYS/devices/system/cpu/cpufreq/policy*; do
  [ -f $p/cpuinfo_max_freq ] || continue
  m=$(cat $p/cpuinfo_max_freq)
  ALLCPUS="$ALLCPUS $(cat $p/related_cpus)"
  [ $((m * 10)) -ge $((TOP * 7)) ] && BIG="$BIG $(cat $p/related_cpus)"
done
mask() { m=0; for c in $*; do m=$((m | (1 << c))); done; printf '%x' $m; }
NBIG=$(echo $BIG | wc -w)
BIGMASK=$(mask $BIG)
ALLMASK=$(mask $ALLCPUS)

clocks() {
  FAILED=0
  FAILS=""
  said=""
  for p in $SYS/devices/system/cpu/cpufreq/policy*; do
    [ -f $p/cpuinfo_max_freq ] || continue
    hw=$(cat $p/cpuinfo_max_freq)
    was=$(cat $p/scaling_max_freq)
    put clocks $p/scaling_max_freq $hw
    # Read-only: the vendor's daemons cannot lower it while the VM runs.
    chmod 444 $p/scaling_max_freq 2>/dev/null
    said="$said CPUs $(cat $p/related_cpus | tr ' ' ',') up to $(khz_ghz $hw) GHz (was $(khz_ghz $was));"
  done
  # Qualcomm's core_ctl parks cores it thinks idle: every core of each cluster awake.
  parked=0
  for c in $SYS/devices/system/cpu/cpu*/core_ctl; do
    [ -f $c/min_cpus ] || continue
    n=$(cat ${c%/core_ctl}/cpufreq/related_cpus 2>/dev/null | wc -w)
    [ "$n" -gt 0 ] && put clocks $c/min_cpus $n && parked=$((parked + 1))
  done
  [ $parked -gt 0 ] && said="$said core_ctl keeps every core awake;"
  say "clocks:${said%;}"
  [ $FAILED -gt 0 ] && say "$FAILED settings were refused:$FAILS"
}

# Per thread, in shell builtins (no process per thread every tick). The app's own priority code
# (adpf.rs) gives the vCPUs nice -8 when they appear, which may come after this script's -20, and
# Android moves the whole app between cpusets as it goes between foreground and background: both
# are put right every tick.
DONE_TIDS=" "
VMASK=""
threads() {
  VCPUS=0
  GPUS=0
  NICE20=0
  for t in $PROC/$PID/task/*; do
    read -r comm < $t/comm 2>/dev/null || continue
    case $comm in "CPU "*"/TCG") VCPUS=$((VCPUS + 1)) ;; esac
  done
  # Off the little cores only where the big ones outnumber the vCPUs; else on every core.
  want=$ALLMASK
  [ $VCPUS -gt 0 ] && [ $NBIG -gt $VCPUS ] && want=$BIGMASK
  [ "$want" != "$VMASK" ] && DONE_TIDS=" " && VMASK=$want
  for t in $PROC/$PID/task/*; do
    tid=${t##*/}
    read -r comm < $t/comm 2>/dev/null || continue
    case $comm in
      "CPU "*"/TCG"|reims-vgpu*|metal2vulkan*|m2v-*|qemu-main) ;;
      *) continue ;;
    esac
    [ -d $CPUSET/top-app ] && echo $tid > $CPUSET/top-app/tasks 2>/dev/null
    read -r st < $t/stat 2>/dev/null || continue
    set -- ${st##*) }
    case $comm in
      qemu-main)
        # toybox's order: chrt -p PID PRIORITY. Field 41 of stat is the policy (1 is FIFO).
        [ "${39}" != 1 ] && chrt -f -p $tid 1 >/dev/null 2>&1
        read -r st < $t/stat 2>/dev/null && set -- ${st##*) }
        if [ "${39}" = 1 ]; then MAIN="real-time (SCHED_FIFO 1)"; else MAIN="refused real time (policy ${39})"; fi
        ;;
      *)
        [ "${17}" != -20 ] && renice -n $((-20 - ${17})) -p $tid >/dev/null 2>&1
        read -r st < $t/stat 2>/dev/null && set -- ${st##*) }
        [ "${17}" = -20 ] && NICE20=$((NICE20 + 1))
        case $comm in "CPU "*) ;; *) GPUS=$((GPUS + 1)); continue ;; esac
        case "$DONE_TIDS" in *" $tid "*) continue ;; esac
        taskset -p $VMASK $tid >/dev/null 2>&1
        DONE_TIDS="$DONE_TIDS$tid " ;;
    esac
  done
  [ $VMASK = $BIGMASK ] && ON="the big cores ($(echo $BIG | tr ' ' ','))" || ON="every core"
}

VCPUS=0
GPUS=0
NICE20=0
ON="every core"
MAIN="not found yet"
# What root can see of the phone's hypervisor: the VM uses neither (QEMU emulates the CPU), and
# Gunyah's VMs need a kernel built with DroidVM's patches on the SM8650.
hv=""
for d in /dev/gunyah /dev/kvm; do [ -e $d ] && hv="$hv $d"; done
if [ -n "$hv" ]; then say "hardware virtualization: present at$hv (this VM still runs on QEMU's emulated CPUs)"
else say "hardware virtualization: no /dev/gunyah or /dev/kvm on this kernel; QEMU emulates the CPUs"; fi
clocks
CLOCKS=on
SAID_THREADS=""
TICKS=0
while alive && [ ! -e "$STOP" ]; do
  # Android sets the app's low-memory score again as it moves between foreground and background.
  echo -1000 > $PROC/$PID/oom_score_adj 2>/dev/null
  threads
  line="threads: $VCPUS vCPU threads on $ON, in top-app; $NICE20 vCPU and GPU threads at nice -20 ($GPUS GPU); QEMU's main loop $MAIN"
  if [ "$line" != "$SAID_THREADS" ] && [ $((TICKS % 15)) -eq 0 ]; then
    say "$line"
    SAID_THREADS=$line
    [ $NBIG -gt 0 ] && [ $VCPUS -gt $((NBIG + 1)) ] && say "$VCPUS vCPU threads for $NBIG fast cores: fewer vCPUs (VM settings) leave a core to Android and QEMU's other threads, and the guest spins less on its locks"
  fi
  t=$(cat $SYS/class/power_supply/battery/temp 2>/dev/null)
  if [ -n "$t" ]; then
    t=$((t / 10))
    if [ $CLOCKS = on ] && [ $t -ge $HOT ]; then
      give clocks
      CLOCKS=off
      say "the battery is at $t °C: the clocks are given back to the phone until it is below $COOL °C"
    elif [ $CLOCKS = off ] && [ $t -lt $COOL ]; then
      CLOCKS=on
      say "the battery is at $t °C again: the clocks are taken again"
      clocks
    fi
  fi
  TICKS=$((TICKS + 1))
  sleep $TICK
done
give clocks
give memory
rm -f "$STOP"
say "the VM ended: every setting it changed is given back"
