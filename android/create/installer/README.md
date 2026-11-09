# vphone-create-steps

The on-device VM creation steps of the Android app: no PC and no root.

`steps.rs` is the entry point (`create_bundle`, `gpu_driver`, `cfw`, `first_boot`), in the order of
`setup/create-vm.sh`. The orchestrator implements `host::Host`. Steps run again safely after a
failure (`<vm>/.create-steps`).

Libraries do the heavy lifting: russh and russh-sftp (SSH, SFTP), apple-codesign (Mach-O and bundle
signing, cdhashes), the plist crate, `vphone-apfs` (reads images; rebuilds the ramdisk),
`vusb-portfwd` (the usbmux tunnel). AEA decryption and the `patch-*` verbs are
vphone-cli's, run as `libvphone_cli.so`. `plist_py` and `im4p` stay hand-written because they must
match the Python tools' bytes.

Cryptex trees go to the guest through its `tar` over one SSH channel: SFTP costs several round trips
per file. The dyld cache is taken out of the OS cryptex, patched locally, and uploaded once.

Limits (none of these are faked): iPhone and iPad guests only (`cfw::device_named` refuses others);
extended attributes are not carried into the guest, because neither the ramdisk's GNU tar 1.17 nor its
sftp-server can set them (the sshfs path could not either); install-mode `environmentOnly` is not
ported.

`android/scripts/build-create-steps.sh` builds it for Android; with `test`, runs the host tests.
