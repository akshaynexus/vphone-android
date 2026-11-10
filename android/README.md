# vphone for Android

An unrooted, arm64 debug app that boots existing vphone VM bundles on a phone: QEMU (TCG) and
reims-vgpu run inside the app's process, and the iOS screen is drawn with the bundled Turnip driver.
Targets: Galaxy S24 Ultra and OnePlus 15, Android 15 or later (minSdk 35, targetSdk 37).

## Build

    android/scripts/build-apk.sh

It needs the SDK in `~/Android/Sdk` (platform 37, build-tools 37, a CMake) with NDK 28.2 (`android/env.sh`), and
two libraries built first: `android/build/out/lib/libqemu-vphone.so` and
`android/build/out/libvulkan_freedreno.so`. It stops and names what is missing rather than packaging
without them. It builds libadrenotools at a pinned commit, the Rust library in `vphone-native/`, and
leaves `android/build/out/vphone-android-debug.apk`, signed with the SDK's debug key.

Rust tests run on the build machine:

    . android/env.sh; cd android/vphone-native
    CARGO_TARGET_DIR=../build/vphone-native-target cargo test --target x86_64-unknown-linux-gnu

## Install

    adb install -r android/build/out/vphone-android-debug.apk

Start the app once so Android creates its storage folder.

## Getting a VM onto the phone

Besides creating one on the phone (below), you can bring one from a computer. A VM is a folder holding `Disk.img`, `nvram.bin`, `ecid`, its two ROMs, and optionally `config.plist`, `serial`,
`gfx-res` and `SEPStorage`: the bundle `run-vphone.sh` boots. Bundles are 20-25 GB.

**Import in the app.** Copy the folder to the phone's shared storage, for example under `Download/`, tap
*Import a VM folder* and pick it. Android does not let the picker choose the top of `Download/` itself or of
internal storage, so pick the VM's own folder. Runs of zeros stay holes. Shared storage must hold
the source and the copy at once while this runs.

**adb push.** The direct route, needing no second copy:

    adb push my-vm /sdcard/Android/data/com.vphone.android/files/vms/

The folder's name is the VM's name. If the app then reports a file as not writable, use Import instead.

## Creating a VM on the phone

*Create a VM on this phone* runs the whole of `setup/create-vm.sh` with no computer and no root: it downloads
the firmware from Apple over Wi-Fi (resumably; cellular only if you confirm), patches it, restores it in
QEMU, installs the CFW, and does the first boot, ending on a running iOS home screen. Choose the device
and iOS version; the wizard defaults to the tested iPhone17,3 + iOS 26.6.2 at 1x and marks pairings the
Linux flow has not verified as under test.

Expect it to take many hours. The wizard gives a time only from what this phone has measured: each step's
running time is kept in `<filesDir>/create/step-times.json` when it finishes, and until every step left has
a time there the screen says there is no estimate. It keeps going with the screen off and survives
the phone restarting: creation resumes from the first unfinished step, with `.create-state` per VM. Charging
is recommended; a hot phone slows down, and at critical temperature the guest pauses until it cools. It
needs about 60 GB free in the app's internal storage (at least 25 GB free during the restore, it waits
below that), and 8 GB of RAM or more. VMs and their restore trees live in internal storage (`<filesDir>/vms`,
real sparse files); downloaded firmware is in `<filesDir>/fw-cache` and is reused for later VMs unless
you ask to delete it.

What the steps do, where it differs from the desktop flow:

- **GPU driver.** The cloudOS system image in the IPSW is an encrypted archive (AEA profile 1) that carries
  its own key in its header. `gpu-driver` reads that member from the IPSW, decrypts it on up to eight
  threads (`installer/src/aea.rs`: every segment's HMAC and checksum is checked), reads the bundle from the
  APFS volume inside and checks the driver's SHA-1; the decrypted image exists only in `fw-cache` while the
  step runs. A cloudOS whose image has no key field is restored into a temporary VM instead, as on the
  desktop. `cargo run --release --example gpu_driver -- <cloudOS.ipsw> <out dir> [threads]` runs it on a PC.
- **Pause.** Pause stops the guest (QMP `stop`) and the helpers (SIGSTOP), for as long as you like; a hot
  phone or low storage does the same. Nothing counts that time: the restore's USB device stays attached
  across a stop (QEMU's `vusb-pci`), and the helpers' time limits run on a clock that leaves stopped time
  out (`vusb/common/src/active.rs`, the same in the patched libimobiledevice-glue, which the restore tool's
  and usbmuxd's waits use). A step that fails after a pause is reported; it is not run again by itself.
  A pause does not survive the app's process ending: the step then starts again.
- **Fast mtree.** The guest's restore hashes the whole system volume on one CPU unless its ramdisk's
  `/usr/local/share/restore/options.plist` sets `SealedVolumeUseFastMtree`. That is made sure of before
  every restore (`installer/src/restore_options.rs`), so a VM patched by an older build has it too; the
  step's log says `Fast mtree: ON` or `OFF` with the reason, and repeats the guest's own word on it.
- **Copies into the guest.** The large copies of `cfw` (iOS's system files, the patched dyld cache) go by a
  transfer disk: a sparse file in the VM's folder (`.cfw-transfer.img`) that the SSH ramdisk VM has as a
  second, read-only disk. The app writes tar archives into the file and the guest's tar reads them from the
  block device, so nothing of them passes the guest's SSH server or its USB. They say how much there is
  before they start, name the way they go, and show their share, speed and time left. A copy under 32 MiB,
  one whose archive does not fit the free storage, or a boot without the disk goes as tar over SSH (three
  connections above 256 MiB); which way is decided once and logged, not retried. Measured on a PC against a
  real SSH ramdisk (`create/vusb/bench-copy.sh`): 2 GB of large files 8.6 MB/s over SSH and 426 MB/s by the
  disk; 44,000 small files 10.5 and 27 MB/s (the guest makes about 2,000 files a second either way).
- **Performance hints.** Setup boots ask Android's performance hints for the vCPU threads from the VM's
  own process. One line in the step's log (`[vm] performance hints: ON` or `OFF (why)`) says whether the
  system granted them and which CPUs the process may run on.

Each QEMU boot runs as its own executable, `libvphone_vm.so`, a child of the `:create` foreground service, so an app kill or crash costs at most the step in progress. Step logs are in
`<vm>/.create-logs/` and reachable from the wizard. Nothing here has been run on a phone yet:
see the build report.

## Using it

*Boot* checks the bundle, the app's libraries and free memory, then starts the VM. Boot takes several
minutes on a phone: the screen shows the phase until iOS is up, and the app unlocks the screen once `vphoned`
answers. *Settings* sets each VM's vCPUs, RAM, TCG code cache, performance hints, iOS's background services, the network, whether its screen is drawn, and two GPU switches (see *A wrong picture*). Defaults: 6 vCPUs, 4 GiB
RAM and a 1024 MiB code cache on a 12 GB phone; 6 GiB and 2048 MiB from 16 GB; 8 GiB and 2048 MiB from 24 GB.

| Control | What it does |
|---|---|
| Touch | One finger, as the desktop window's pointer. A press and a release go to the guest at once; moves go at most once per refresh of the guest's display, the newest place each time (`vphone-native/src/touch.rs`) |
| Home | A middle click on the guest's touchscreen, which QEMU sends as Cmd+H |
| Lock | vphoned's `input.button lock` |
| Power | iOS's own shutdown, then QMP `quit` if the guest does not answer |
| Back gesture or key | Asks whether to leave the VM running, shut it down, or stay |

The VM keeps running with the app in the background, in a foreground service (type `specialUse`) holding a
partial wake lock. A notification brings it back or stops it. QEMU can run once per process, so the app restarts
itself after a VM ends.

## Network

iOS reaches the internet through QEMU's user-mode network (libslirp 4.9.7, built by
`scripts/build-deps.sh` and enabled in `scripts/build-qemu.sh`): a NAT made of the app's own sockets, so
it follows whatever connection the phone uses, Wi-Fi, mobile data or a VPN. The machine's one network card
(`virtio-net-pci`) is attached to it with `-nic user,model=virtio-net-pci,ipv6=off`. iOS gets `10.0.2.15`
by DHCP, the gateway `10.0.2.2` and the resolver `10.0.2.3`. *Settings > Network* turns it off for a VM;
iOS then has the card with nothing on the other end (`-net nic`), as every VM had before 0.6.4. The boots
of a creation (restore, SSH ramdisk, first boot) are always without it.

- **DNS.** libslirp forwards the guest's lookups to the first IPv4 server of a resolv.conf. Android has
  none, so `patches/libslirp` makes it read the file named in `VPHONE_RESOLV_CONF`, and the app keeps
  `files/resolv.conf`: the IPv4 DNS servers of the phone's active network (`VmNetwork.kt`, from
  `LinkProperties`), then `1.1.1.1` and `8.8.8.8`, which answer when the network lists only IPv6 resolvers.
  The file is written before the boot and again whenever the active network changes. libslirp looks at
  it again when it has changed, at most once a second, so a switch of network reaches a running VM without
  a restart. Only the first server is asked: if the network's own IPv4 server does not answer, the public
  ones are not tried.
- **Limits.** IPv4 only. TCP and UDP; `ping` (ICMP) is not tested on a phone. Nothing
  outside reaches iOS: there are no forwarded ports. Android's Private DNS does not apply to the guest's
  lookups, which go as plain DNS to the servers above.
- **The log.** A boot's log has `network: on ... DNS servers ...` at the start, a `network: DNS servers
  for the VM: ...` line at each change, and, after the unlock, one line of what iOS itself sees, asked of
  vphoned (`device.network`, then `network.resolve` of `apple.com` with a connection to port 443):

      [vphone] network: en0 10.0.2.15 (QEMU's NAT); DNS apple.com -> 17.253.144.10 in 180 ms; TCP 17.253.144.10:443 connected in 95 ms

  `NO en0` there means iOS's driver did not bind the card; `NO ADDRESS` that it did and DHCP did not
  answer; `DNS FAILED` and `TCP ... FAILED` name the step that failed. libslirp's own errors go to
  QEMU's stderr, which is in the same log. A VM that came from a Mac with its address pinned by
  vphone-cli is set back to DHCP (`network.ipv4.set`), and the log says so.
- The guest's agent (vsock) and usbmuxd (virtual USB) do not use this network.

## Logs and failures

QEMU's stdout and stderr, and the guest's serial console, go to `<vm>/serial.log` (the previous run's is
`serial.log.1`) and to logcat under the tags `qemu-out`, `qemu-err` and `vphone`. If the process
dies with a VM in it, the next launch shows why, from Android's `ApplicationExitInfo`, with the log one tap away.

    adb logcat -s vphone qemu-out qemu-err hook_impl vphone-adpf

No helper processes run, so Android's limit on phantom processes does not apply: `process-isolation` runs each
GPU task on threads inside QEMU.

### A wrong picture

*Settings* has these for each VM's picture:

| Setting | Default | What it sets |
|---|---|---|
| Performance | Max | How Android is asked to clock the vCPU threads (`adpf.rs`). Max: a vCPU busier than 20% of a core is reported late to the Performance Hint service, the threads run at nice -8, and sustained-performance mode is off. Balanced: late above 60%, nice -4, sustained-performance mode on. |
| Refresh rate | 120 Hz | The one display mode iOS is offered (`gfx-modes=<res>@<hz>`), and the rate asked of the phone's screen for the VM's window (`Surface.setFrameRate`, the window's preferred display mode). |
| GPU image compression (UBWC) | off | Off runs Turnip with `TU_DEBUG=noubwc`, for the driver's own images. |
| Screen buffer layout | Auto | `TU_ANDROID_SWAPCHAIN_LAYOUT=auto\|linear\|ubwc`: how Turnip lays out the swapchain's images, which gralloc allocates and the compositor reads. Auto is what gralloc says of each buffer, and linear where it says nothing. Linear is linear whatever gralloc says. Compressed asks gralloc for UBWC buffers (vendor usage bit `0x10000000`) and lays the images out as gralloc then says, UBWC where it says nothing. |
| Draw the screen on the CPU | off | `REIMS_VGPU_SOFTWARE_DISPLAY=1`: the GPU device makes no swapchain; for each frame it reads the guest's surface as a frame dump does and copies it into the window's buffer (`ANativeWindow_lock`, at the buffer's stride; `reims-vgpu`'s `window/vulkan/software.rs`). No driver is between the surface and the screen. It costs a conversion and a copy of the frame on the CPU per frame. |
| GPU diagnostics | off | `TU_DEBUG=layout`: the driver prints each image's format, size, pitch, tile mode and UBWC sizes to `serial.log`. `REIMS_VGPU_FRAME_DUMP=<files>/runs/frames`: every 15 s at most, the GPU device saves the bytes of the guest surface behind the frame on screen, read by the CPU from the guest's pages with no GPU command between (`reims-vgpu`'s `window/vulkan/dump.rs`). The log screen's *Share* sends the last four as `guest-surface-N.png` with the log. No dumps are made while the screen is drawn on the CPU. |

**The scrambled screen of 0.5.6 and 0.5.7** (Adreno 840, Android 17) was the swapchain's layout. The
frame dumps of 0.5.7 showed the guest's surface right while the screen showed every 16 rows smeared
across its width, and the `layout` lines showed the swapchain's `R8G8B8A8_UNORM` 864x1864 images `tiled`.
The cause, in Mesa at the pin: built with `-Dandroid-stub=true` the driver reads a buffer's layout from
its handle alone (`u_gralloc_fallback.c`), and only where the handle starts with the `'gmsm'` magic of
Qualcomm's older gralloc. This phone's gralloc (SnapAlloc) has no magic, so the modifier stayed
`DRM_FORMAT_MOD_INVALID`, and Turnip then picks a layout of its own (`tu_image.cc`: tiled, with UBWC
where allowed) for a buffer gralloc allocated, and the compositor reads, as linear. The fix is patches
`0007` to `0009` of `android/patches/mesa` (`android/mesa/README.md`). The driver says what it chose
once per swapchain, in `serial.log`:

    turnip: swapchain layout: 864x1864 VK_FORMAT_R8G8B8A8_UNORM, gralloc format 1 stride 896 px, handle numFds=2 numInts=34 ints[0..]= ...: gralloc (IMapper 5 metadata) says LINEAR, TU_ANDROID_SWAPCHAIN_LAYOUT=auto, laid out LINEAR (UBWC no), row pitch 3584 bytes

None of this has run on a phone as of 0.5.8. If the picture is still wrong: *Screen buffer layout*
Linear, then Compressed; then *Draw the screen on the CPU*, which needs no driver to be right.

A screenshot beside a `guest-surface-N.png` tells the two halves apart. The PNG scrambled like the screen:
the GPU wrote the scramble into the guest's pages (the render or the copy back). The PNG right and the
screen wrong: the fault is after the surface (the copy into the image, the draw, or the swapchain image
as the compositor reads it; the swapchain image's `layout` line says how Turnip laid it out).

Android 17 adds a per-app memory limit based on the phone's RAM. A VM it ends is reported as such;
lower the RAM or the code cache.

## Making it faster on the phone

Everything iOS does here waits on emulated CPUs, so what the phone gives the vCPU threads decides the frame
rate. In the first full log (0.6.0, OnePlus 15, Performance = Max) the phone held their clocks at 1.6 to
2.4 GHz of 3.6 while Android's thermal status read "none", and the guest had 5.2 to 5.9 of 6 CPUs busy
with background work.

**On the phone** (not verified on a device by this repository; the next log's `performance hints` and
`game mode` lines say whether the allowed clock rose):

- The app declares itself a game (`android:appCategory="game"`, a game-mode config that supports the
  performance mode, and `GameManager.setGameState` while the VM is on screen), because phones apply their
  performance settings to games. On a OnePlus: open the **Games** app (Game Assistant), add *vphone* if it is
  not listed, and choose **Pro Gamer mode** (or the performance setting it offers) for it.
- **Settings > Battery > Power mode (or More settings) > High performance mode** on a OnePlus lifts the
  clocks for every app.
- A game overlay opens with a swipe in from a top corner of the screen, which is also where iOS's
  Notification Center and Control Center start. If it gets in the way, turn the overlay's gesture off in
  the Games app's settings (*Game Assistant* / *Gaming tools*), or remove vphone from the Games list.
- Keep the phone cool and off the charger's heat: a case off and a fan do more than any setting once
  the phone is warm.

**In the VM's settings:** *iOS background services*.

- *Trimmed* applies the guest agent's `trimmed` profile (the about 137 daemons creation's option disables:
  Siri and the intelligence services, Find My, Wallet, Watch, Home, iCloud and Mail sync, analytics, media
  analysis, the App Store) and `com.apple.momentsd`.
- *Fewest* (experimental, not run on a guest) also disables `com.apple.sharingd` (AirDrop, Handoff) and
  `com.apple.chronod` (every widget). If `guest top` then shows a process busier than before, go back.
- The app asks the guest's agent once after the unlock, whenever the setting differs from what it last
  applied (`<vm>/services.json`), and the log says `[vphone] services: ...`. iOS honours it from its next
  start, so shut the VM down and start it again. *As they are* turns back on what the app turned off.
- The labels could not be checked against an iOS 27 `launchd.plist` here. BackgroundShortcutRunner is an XPC
  service started for widgets' App Intents and locationd is one the agent refuses to disable; neither can
  be switched off this way.

**In iOS:** remove the Weather, Clock and other widgets from the Home Screen and Today view (they ran at
0.2 to 0.4 of a guest CPU each, and their App Intents kept BackgroundShortcutRunner at 0.6 to 0.8), and turn
on Settings > Accessibility > Motion > Reduce Motion and Display & Text Size > Reduce Transparency.

What 0.6.2 changed in the app itself, and what was measured:

| Change | Measured |
|---|---|
| Counter reads (`MRS CNTVCT_EL0`, which is iOS's `mach_absolute_time` in user code) without QEMU's global lock and without ending their block (`VPHONE_FAST_CNT=1`, checked by `tests/tcg/vphone-cnt`) | On the build PC (x86-64, one vCPU, lock uncontended): 4 million reads in 0.23 s instead of 0.47 s, about 50 ns a read instead of 105. On the phone the vCPU threads waited for that lock 25-43 thousand times a second, 7-9% of their time; how much of that the reads were is what the next log's `cpu` line says |
| QEMU's main loop thread at nice -10 (the vCPU and GPU threads are at -8) | Not measured. It delivers the guest's timers, I/O completions and every touch, and ran at the default priority behind them |
| Touch: only the newest place of each Android event, moves at most once a refresh | Unit tests; fewer reports for the guest's HID driver and backboardd. The `touch` line measures the rest |
| `WFE` left a NOP | On the build PC a spin iteration with `WFE` costs 5.6 ns (200 million in 1.1 s). The log's 1.6-4.8 million a second is then about 1-2% of the six vCPU threads' time on the phone, scaled by its clocks; the same rate as the desktop's 14,000 waits of 11 us (docs/PERFORMANCE.md). Yielding the host CPU could win back no more than that |
| Whole-cache flushes | The log's long frame gaps do not follow them (11.9, 10.9, 6.2 and 5.0 s in periods without a flush; 6.5 and 9.8 s in periods with one), and a gap also counts a screen that did not change. After each flush the cache grew by about 12 MiB/s for a minute against 3-5 MiB/s otherwise: about 500 MiB translated again. The `cpu` line now gives the translation's share of vCPU time |

## Reading a boot log for speed

Every 30 s `serial.log` has these lines. Measure a few minutes after the unlock: iOS's own work after
a boot keeps every vCPU busy for minutes, and a period that began before the first frame gives a low mean
rate (`best second` is the burst's own).

- `[vphone] status ...`: memory, the busiest threads, where each vCPU is, and
  `vCPU clocks N% of max (the system allows A%), M% of the time at max`, sampled five times a second from the
  CPUs the vCPU threads last ran on. Clocks at what is allowed, and that below 100%, are the phone's limit
  (heat or power: `scaling_max_freq`); clocks below what is allowed are the governor's choice for this load.
- `[vphone] performance hints: ...`: the CPUs the vCPU threads last ran on, each cluster's clock now with
  what the system allows and its highest, Android's thermal status and headroom (1.00 is where it throttles
  severely), the scheduler groups of a vCPU thread (`top-app` while the VM is on screen), and how many of the
  GPU device's live threads have the vCPUs' priority.
- `[vphone] game mode: ...`: whether the phone counts the app as a game and in which of Android's game
  modes, each time the VM comes on screen. Compare the allowed clocks before and after switching the phone's
  performance or game settings.
- `[vphone] touch ...`: the touch events since the last line and how many went to the guest, then the
  median, 95th percentile and longest of each leg: Android's read of the touch to the app's call, the app
  to QEMU (reims-vgpu's input thread), QEMU's main loop and lock, and the wait until the guest's USB
  transfer took the report; and the sum. All of that is the host's side. What iOS does after its driver has
  the report (backboardd, the app, the frame) is not visible from the host: a delay you feel with these
  figures at a few milliseconds is the guest's, and follows `guest top` and the `gpu` line's frame time.
- `[vphone] services: ...`: what the VM's *iOS background services* setting asked of the guest's agent.
- `[vphone] cpu ...` (QEMU's `vphone_android_cpu_line`): how often the guest ran `WFE` and `YIELD`, which
  are NOPs under multi-threaded TCG, so a guest that spins on them burns host cores: tens of thousands a
  second is iOS at rest, millions is a spin; the share of the vCPU threads' time spent halted (the guest
  idle), waiting for QEMU's global lock, in exclusive sections, and spinning for other vCPUs' part of a TLB
  flush, and translating guest code (with the blocks made a second); how often they took QEMU's lock at all
  and how often the guest read its counter, which takes no lock since 0.6.2; and the translated-code
  cache's fill and how often it was flushed whole (each flush translates everything the guest runs again;
  the settings allow at most 2048 MiB, which is the phone's RAM to give, not a limit of QEMU's on arm64).
- `[vphone] guest top ...`: which of iOS's processes used its CPUs since the last line (vphoned's
  `processes.list`), from the unlock on: `5.61 guest CPUs busy: mediaanalysisd 1.92, ...`.
- `[vphone] guest code ...`: when at least half the vCPUs are in one 16 KiB page of user code, 48
  instruction words around the address most of them share, once a page. TCG keeps a vCPU's PC as of its
  last indirect branch, so vCPUs in one loop show the same few addresses. `d503205f` is `WFE`, `d503203f`
  `YIELD`: a loop of those and a branch back is a spin; vector instructions are work. The shared cache's
  slide has been the same in every boot logged, so with the guest's dyld shared cache in `dsc/`
  `tools/dsc-symbolize.py` names the image and function (docs/PERFORMANCE.md).
- `[vphone] gpu ...`: the guest's presents a second against the refresh rate, the most in one second, and
  per guest frame, in milliseconds and as a share of the refresh rate's frame budget (8.3 ms at 120 Hz):
  the GPU device's FIFO threads waiting for the guest (`guest`), waiting for an earlier command of another
  FIFO (`barriers`: the guest orders commands across its queues, and a FIFO thread waits until the other
  queue's thread has run the command it names, so this follows `host`) and running the guest's commands
  (`host`, with the three opcodes that took most), the window's acquire, encode and present, and the Metal
  runtime's `vkQueueSubmit`, pipeline makings, host steps, readings of `/proc/self/maps` with the checks
  answered from the kept reading instead, and the shadow copies with the texels they moved
  (`reims-vgpu`'s `perf.rs`). The FIFO figures are sums over every FIFO thread. A `host` far below the frame
  time means the frame waits on the emulated CPU, not on the GPU path.

## Sending a VM to another phone

On the VM list, **Send** packs a VM into one `<name>.vphone` file:

- **Share…** packs it into the app's cache (kept for a day), then opens the share sheet: Quick Share, Files, or any app that takes a file.
- **Save to…** writes it straight to a place you pick, such as Downloads or a USB drive.

To receive one, open the `.vphone` file from Quick Share's notification or a file manager, or use **Import a VM → A .vphone file**. A name that is already taken gets a number after it.

What goes in: only what a boot reads, which is `Disk.img`, `nvram.bin`, the two boot ROMs, `SEPStorage`, `config.plist`, `ecid`, `serial` and `gfx-res`. Logs, setup leftovers (`iPhone_Restore/`, the transfer image, `.create-*`) and this phone's `android.json` stay behind, so the receiving phone uses its own defaults. A VM can't be sent while it's running or being created.

The format (`vphone-native/src/share.rs`, version 1):

- **Header:** magic `VPHONEVM`, then a JSON header listing the files and their sizes.
- **Body:** one zstd frame of `offset | length | bytes` records per file. Holes in the disk and all-zero 1 MiB chunks are left out.
- **Trailer:** a JSON trailer with each file's BLAKE3 hash, then `VPHNEND1`.

The file reads front to back. Importing checks every hash and boots nothing until the VM folder is complete. It needs up to the VM's data size, plus 1 GiB, free on the receiving phone.

### Over a USB cable

A phone can receive a VM straight from another phone over USB-C, with no cloud copy and no file staged on the receiving phone:

1. On the sending phone, tap **Send → Save to…** and choose Downloads.
2. Connect the phones. On the sending phone, tap the USB notification and choose **File transfer**, then keep it unlocked.
3. On the receiving phone, choose **USB controlled by: This device** if Android asks, then tap **Import a VM → From a phone plugged in by USB**.

vphone reads the other phone's shared folders over MTP with Android's USB host API (`UsbVmSource.kt`), looking up to three folders deep and skipping `Android/`. It streams the picked file through a pipe into the same importer, so the checksums and holes are handled the same way. Android doesn't let an app act as the MTP device itself, so the sending side is the system's own File transfer mode.
