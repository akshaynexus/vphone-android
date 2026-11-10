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
answers. *Settings* sets each VM's vCPUs, RAM, TCG code cache, performance hints, whether its screen is drawn, and two GPU switches (see *A wrong picture*). Defaults: 6 vCPUs, 4 GiB
RAM and a 1024 MiB code cache on a 12 GB phone; 6 GiB and 2048 MiB from 16 GB; 8 GiB and 2048 MiB from 24 GB.

| Control | What it does |
|---|---|
| Touch | One finger, as the desktop window's pointer |
| Home | A middle click on the guest's touchscreen, which QEMU sends as Cmd+H |
| Lock | vphoned's `input.button lock` |
| Power | iOS's own shutdown, then QMP `quit` if the guest does not answer |
| Back gesture or key | Asks whether to leave the VM running, shut it down, or stay |

The VM keeps running with the app in the background, in a foreground service (type `specialUse`) holding a
partial wake lock. A notification brings it back or stops it. QEMU can run once per process, so the app restarts
itself after a VM ends.

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
| GPU image compression (UBWC) | off | Off runs Turnip with `TU_DEBUG=noubwc`, for the driver's own images. |
| Screen buffer layout | Auto | `TU_ANDROID_SWAPCHAIN_LAYOUT=auto\|linear\|ubwc`: how Turnip lays out the swapchain's images, which gralloc allocates and the compositor reads. Auto is what gralloc says of each buffer, and linear where it says nothing. Linear is linear whatever gralloc says. Compressed asks gralloc for UBWC buffers (vendor usage bit `0x10000000`) and lays the images out as gralloc then says, UBWC where it says nothing. |
| Draw the screen on the CPU | off | `REIMS_VGPU_SOFTWARE_DISPLAY=1`: the GPU device makes no swapchain; for each frame it reads the guest's surface as a frame dump does and copies it into the window's buffer (`ANativeWindow_lock`, at the buffer's stride; `reims-vgpu`'s `window/vulkan/software.rs`). No driver is between the surface and the screen. It costs a conversion and a copy of the frame on the CPU per frame. |
| GPU diagnostics | on | `TU_DEBUG=layout`: the driver prints each image's format, size, pitch, tile mode and UBWC sizes to `serial.log`. `REIMS_VGPU_FRAME_DUMP=<files>/runs/frames`: every 15 s at most, the GPU device saves the bytes of the guest surface behind the frame on screen, read by the CPU from the guest's pages with no GPU command between (`reims-vgpu`'s `window/vulkan/dump.rs`). The log screen's *Share* sends the last four as `guest-surface-N.png` with the log. No dumps are made while the screen is drawn on the CPU. |

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
