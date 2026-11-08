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

A VM is a folder holding `Disk.img`, `nvram.bin`, `ecid`, its two ROMs, and optionally `config.plist`, `serial`,
`gfx-res` and `SEPStorage`: the bundle `run-vphone.sh` boots. Bundles are 20-25 GB.

**Import in the app.** Copy the folder to the phone's shared storage, for example under `Download/`, tap
*Import a VM folder* and pick it. Android does not let the picker choose the top of `Download/` itself or of
internal storage, so pick the VM's own folder. Runs of zeros stay holes. Shared storage must hold
the source and the copy at once while this runs.

**adb push.** The direct route, needing no second copy:

    adb push my-vm /sdcard/Android/data/com.vphone.android/files/vms/

The folder's name is the VM's name. If the app then reports a file as not writable, use Import instead.

## Using it

*Boot* checks the bundle, the app's libraries and free memory, then starts the VM. Boot takes several
minutes on a phone: the screen shows the phase until iOS is up, and the app unlocks the screen once `vphoned`
answers. *Settings* sets each VM's vCPUs, RAM, TCG code cache and performance hints. Defaults: 6 vCPUs, 4 GiB
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

Android 17 adds a per-app memory limit based on the phone's RAM. A VM it ends is reported as such;
lower the RAM or the code cache.
