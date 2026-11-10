# vphone-create-steps

The on-device VM creation steps of the Android app: no PC and no root.

`steps.rs` is the entry point (`create_bundle`, `gpu_driver_from_image` and its fallback `gpu_driver`, `cfw`, `first_boot`), in the order of
`setup/create-vm.sh`. The orchestrator implements `host::Host`. Steps run again safely after a
failure (`<vm>/.create-steps`).

Libraries do the heavy lifting: russh and russh-sftp (SSH, SFTP), apple-codesign (Mach-O and bundle
signing, cdhashes), the plist crate, `vphone-apfs` (reads images; rebuilds the ramdisk),
`vusb-portfwd` (the usbmux tunnel), and for the cloudOS image's archive (`aea.rs`) the RustCrypto
hkdf, hmac, aes and ctr crates with lzfse_rust and rayon. The `patch-*` verbs are vphone-cli's, run as
`libvphone_cli.so`; its own AEA decryption (the iPhone image, in `fw prepare`) is the Android overlay's
multi-threaded one. `plist_py` and `im4p` stay hand-written because they must
match the Python tools' bytes.

Cryptex trees and the patched dyld cache go to the guest by the transfer disk
(`tree::TransferDisk`): `cfw` makes a sparse file, the app attaches it to the SSH ramdisk VM as a
second read-only disk, the archives are written into the file here and the guest's GNU tar reads
them from the raw device (`tar -xpf - -b 8192 < /dev/rdiskN`, after a `dd` on the same open file
for an archive's offset). It is the same tar with the same options as over SSH, so modes, numeric
owners and times arrive the same. Many files use three readers; a few large ones use one, which
is faster. With the disk attached the VM's container is no longer `disk1`, so
`steps::mount_guest_volumes` finds it and runs the ramdisk's `mount_filesystems` with its device
names rewritten. A small copy, one that does not fit, or a boot without the disk is tar over SSH
(`tree::upload_tree_over` decides, once): SFTP costs several round trips per file, and above
256 MiB three connections are used, as the guest's dropbear is one process a connection. The dyld
cache is taken out of the OS cryptex, patched locally, and uploaded once.
`android/create/vusb/bench-copy.sh` measures both ways against a real SSH ramdisk VM on a PC.

Limits (none of these are faked): iPhone and iPad guests only (`cfw::device_named` refuses others);
extended attributes are not carried into the guest, because neither the ramdisk's GNU tar 1.17 nor its
sftp-server can set them (the sshfs path could not either); install-mode `environmentOnly` is not
ported.

`android/scripts/build-create-steps.sh` builds it for Android; with `test`, runs the host tests.
