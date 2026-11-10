#!/system/bin/sh
# vPhone's root boost: run as root (KernelSU, Magisk or APatch's su) for as long as a VM runs in
# the app's process, then everything it changed is given back.
#
#   sh root-boost.sh PID DIR
#
# PID is the app's process (QEMU runs in it), DIR the app's files directory: the stop file the
# app creates when the VM ends and the saved values live there.
#
# What it does, after DroidVM (tools/rig/phone-cpu-pin.sh, CrosvmBackendInstance's GPU worker
# cpuset and real-time switch) and the performance modules emulator users run as root:
#   clocks   every CPU policy's limit back to its hardware highest, and the big clusters' lowest
#            pinned there too; the files then read-only, so a vendor's power daemon (OPLUS held
#            the big cores at 1.7 GHz for DroidVM; the OnePlus 15 held these vCPUs at 1.3-2.4 of
#            3.6-4.6) cannot pull them down again. The GPU's lowest clock at its highest, its
#            clock kept on between frames, the memory buses (DDR, LLCC, L3) at their highest,
#            Qualcomm's core_ctl keeping every core awake, WALT's sched_boost.
#   threads  vCPU and GPU-device threads at nice -20, kept off the little cores where the phone
#            has slow ones; QEMU's main loop real-time (SCHED_FIFO 1): it runs in short bursts and
#            every timer, disk completion and touch of the guest passes through it. The vCPUs
#            stay off real time: six busy real-time threads would starve Android's own (DroidVM
#            refuses real time without a cpuset for the same reason).
#   memory   the app is not ended by the low-memory killer; transparent huge pages for memory
#            QEMU asks them for (the guest's RAM), where the kernel has them.
# The kernel's own thermal limits (the cooling devices' frequency limits) stay in force. When the
# battery reaches HOT degrees the clocks are given back, and taken again below COOL.
set -u
PID=$1
DIR=$2
SYS=${VPHONE_SYS:-/sys}
PROC=${VPHONE_PROC:-/proc}
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
echo $$ > /dev/cpuset/tasks 2>/dev/null

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

# Left by a run that was ended before it gave everything back.
for g in clocks memory; do give $g; done

khz_ghz() { awk -v k="$1" 'BEGIN { printf "%.1f", k / 1e6 }'; }

# The clusters: each policy's CPUs and highest clock. A cluster below 70% of the fastest one's
# clock (the SM8650's A520s at 2.3 of 3.4 GHz) is a little one; the OnePlus 15's 3.6 of 4.6 GHz
# cores are not.
TOP=0
for p in $SYS/devices/system/cpu/cpufreq/policy*; do
  [ -f $p/cpuinfo_max_freq ] || continue
  m=$(cat $p/cpuinfo_max_freq)
  [ "$m" -gt "$TOP" ] && TOP=$m
done
BIG=""
LITTLE=""
for p in $SYS/devices/system/cpu/cpufreq/policy*; do
  [ -f $p/cpuinfo_max_freq ] || continue
  m=$(cat $p/cpuinfo_max_freq)
  if [ $((m * 10)) -lt $((TOP * 7)) ]; then LITTLE="$LITTLE $(cat $p/related_cpus)"
  else BIG="$BIG $(cat $p/related_cpus)"; fi
done
[ -z "$(echo $BIG)" ] && BIG=$LITTLE && LITTLE=""
MASK=0
for c in $BIG; do MASK=$((MASK | (1 << c))); done
MASK=$(printf '%x' $MASK)
CPUS=$(echo $BIG | tr ' ' ',')

clocks() {
  FAILED=0
  FAILS=""
  said=""
  for p in $SYS/devices/system/cpu/cpufreq/policy*; do
    [ -f $p/cpuinfo_max_freq ] || continue
    hw=$(cat $p/cpuinfo_max_freq)
    cpus=$(cat $p/related_cpus | tr ' ' ',')
    put clocks $p/scaling_max_freq $hw
    case " $LITTLE " in
      *" $(cat $p/related_cpus | cut -d' ' -f1) "*) said="$said CPUs $cpus up to $(khz_ghz $hw) GHz;" ;;
      *) put clocks $p/scaling_min_freq $hw; said="$said CPUs $cpus at $(khz_ghz $hw) GHz;" ;;
    esac
    # Read-only: the vendor's daemons cannot lower them while the VM runs.
    chmod 444 $p/scaling_max_freq $p/scaling_min_freq 2>/dev/null
  done
  # The GPU (KGSL): lowest clock at the highest, clocks and bus kept on between frames, no nap.
  kg=$SYS/class/kgsl/kgsl-3d0
  if [ -d $kg ]; then
    gmax=$(cat $kg/devfreq/max_freq 2>/dev/null || cat $kg/max_gpuclk 2>/dev/null)
    [ -n "$gmax" ] && put clocks $kg/devfreq/min_freq $gmax
    put clocks $kg/min_pwrlevel 0
    put clocks $kg/force_clk_on 1
    put clocks $kg/force_bus_on 1
    put clocks $kg/force_rail_on 1
    put clocks $kg/force_no_nap 1
    put clocks $kg/idle_timer 1000000
    [ -n "$gmax" ] && said="$said GPU at $((gmax / 1000000)) MHz, kept on;"
  fi
  # The memory buses: devfreq devices for bandwidth and the memory-latency governors, and the
  # bus_dcvs votes of newer Qualcomm kernels.
  buses=0
  for d in $SYS/class/devfreq/*; do
    case ${d##*/} in
      *kgsl*|*bw*|*ddr*|*llcc*|*l3*|*memlat*|*cpubw*|*gpubw*)
        top=$(tr ' ' '\n' < $d/available_frequencies 2>/dev/null | sort -n | tail -1)
        [ -z "$top" ] && top=$(cat $d/max_freq 2>/dev/null)
        [ -n "$top" ] && put clocks $d/min_freq $top && buses=$((buses + 1)) ;;
    esac
  done
  for b in $SYS/devices/system/cpu/bus_dcvs/*; do
    [ -f $b/boost_freq ] && [ -f $b/hw_max_freq ] && put clocks $b/boost_freq $(cat $b/hw_max_freq) && buses=$((buses + 1))
  done
  [ $buses -gt 0 ] && said="$said $buses memory bus clocks at their highest;"
  # Qualcomm's core_ctl parks cores it thinks idle: every core of each cluster awake.
  parked=0
  for c in $SYS/devices/system/cpu/cpu*/core_ctl; do
    [ -f $c/min_cpus ] || continue
    n=$(cat ${c%/core_ctl}/cpufreq/related_cpus 2>/dev/null | wc -w)
    [ "$n" -gt 0 ] && put clocks $c/min_cpus $n && parked=$((parked + 1))
  done
  [ $parked -gt 0 ] && said="$said core_ctl keeps every core awake;"
  # WALT's boost (Qualcomm): 1 places busy tasks on the big cores and raises their clocks.
  for f in $PROC/sys/walt/sched_boost $PROC/sys/kernel/sched_boost; do
    [ -f $f ] && put clocks $f 1 && said="$said sched_boost on;" && break
  done
  say "clocks:${said%;}"
  [ $FAILED -gt 0 ] && say "$FAILED settings were refused:$FAILS"
}

memory() {
  thp=$SYS/kernel/mm/transparent_hugepage/enabled
  if [ -f $thp ]; then
    case $(cat $thp) in
      *"[never]"*) put memory $thp madvise && say "transparent huge pages for the guest's RAM (they were off)" ;;
      *) say "transparent huge pages were already on: $(cat $thp)" ;;
    esac
  fi
}

DONE_TIDS=" "
# Shell builtins only, every tick: the app's own priority code (adpf.rs) gives the vCPUs nice -8
# when they appear, which may come after this script's -20.
threads() {
  VCPUS=0
  GPUS=0
  for t in $PROC/$PID/task/*; do
    tid=${t##*/}
    read -r comm < $t/comm 2>/dev/null || continue
    case $comm in
      "CPU "*"/TCG"|reims-vgpu*|metal2vulkan*|m2v-*)
        read -r st < $t/stat 2>/dev/null || continue
        set -- ${st##*) }
        [ "${17}" != -20 ] && renice -n $((-20 - ${17})) -p $tid >/dev/null 2>&1
        case $comm in "CPU "*) VCPUS=$((VCPUS + 1)) ;; *) GPUS=$((GPUS + 1)) ;; esac
        case "$DONE_TIDS" in *" $tid "*) continue ;; esac
        taskset -p $MASK $tid >/dev/null 2>&1 ;;
      qemu-main)
        case "$DONE_TIDS" in *" $tid "*) continue ;; esac
        chrt -f -p 1 $tid >/dev/null 2>&1 && MAIN="real-time (SCHED_FIFO 1)"
        taskset -p $MASK $tid >/dev/null 2>&1 ;;
      *) continue ;;
    esac
    DONE_TIDS="$DONE_TIDS$tid "
  done
}

VCPUS=0
GPUS=0
MAIN="not found yet"
# What root can see of the phone's hypervisor: the VM uses neither (QEMU emulates the CPU), and
# Gunyah's VMs need a kernel built with DroidVM's patches on the SM8650.
hv=""
for d in /dev/gunyah /dev/kvm; do [ -e $d ] && hv="$hv $d"; done
if [ -n "$hv" ]; then say "hardware virtualization: present at$hv (this VM still runs on QEMU's emulated CPUs)"
else say "hardware virtualization: no /dev/gunyah or /dev/kvm on this kernel; QEMU emulates the CPUs"; fi
clocks
memory
CLOCKS=on
SAID_THREADS=""
TICKS=0
while alive && [ ! -e "$STOP" ]; do
  # Android sets the app's low-memory score again as it moves between foreground and background.
  echo -1000 > $PROC/$PID/oom_score_adj 2>/dev/null
  threads
  line="threads: $VCPUS vCPU threads at nice -20 on CPUs $CPUS; QEMU's main loop $MAIN; $GPUS GPU threads at nice -20"
  if [ "$line" != "$SAID_THREADS" ] && [ $((TICKS % 15)) -eq 0 ]; then
    say "$line"
    SAID_THREADS=$line
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
