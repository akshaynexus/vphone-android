# Swift firmware tools for Android

vphone-cli's firmware tooling (`fw`, `restore`, `cfw` verbs, `sign`, ...) built for aarch64 Android as
`libvphone_cli.so`, for on-device VM creation without root. Layered over `linux-create/` (unmodified) and the
pinned vphone-cli 2.14.2 (`83197152`), so nothing is forked:

- `sync-android.sh`: copies `linux-create/` to `android/build/create/tree`, runs its `sync-sources.sh`, adds `overlay/`.
- `androidize.py`: makes the `os(Linux)` paths apply to Android and rewrites Glibc imports, then applies `edits.txt`
  (checked regex edits; a miss fails the build).
- `overlay/`: Android replacements. `RestoreRamDiskSealPatcher` patches the ramdisk with `libvphone_apfs.so` instead of a
  mount; `VPhoneCustomFirmwareInstallerLinux` has no sshfs/sudo path (`cfw install` is the app's installer);
  `VPhoneAndroidCatalogue` reads AppleDB with JSONDecoder, because JSONSerialization runs out bionic's allocator;
  `main.swift` and `VPhoneAndroidTrustStore` set the home directory and TLS roots.
- `build-swift.sh`: `swift build` for one architecture.

Build everything with `android/scripts/build-create-tools.sh` (Swift 6.4.1 from swiftly, swift.org's Android SDK for
6.4.1, NDK 28.2). C dependencies are in `android/deps-create/` (`pins.txt`, upstream releases with SHA-256). Output and
the runtime contract: `android/build/out/create/` and `android/build/COORDINATION-create.md`.

`VPHONE_CREATE_ARCH=x86_64` builds the same tree natively for the x86_64 Android emulator, for verification only.
