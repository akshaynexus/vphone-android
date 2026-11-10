//! On-device iOS VM creation for the vphone Android app: no PC, no root.
//!
//! Ports of the desktop tools (`tools/`, `setup/create-vm.sh`, and vphone-cli's
//! Linux CFW installer) with the root-only parts replaced: SSH/SFTP in process
//! instead of ssh and sshfs, the APFS library instead of mounts, apple-codesign
//! instead of the rcodesign program.

/// The clock the steps' time limits are counted on: it stops while the creation is paused.
pub use vusb_common::active;

pub mod aea;
pub mod bundle;
pub mod cfw;
pub mod der;
pub mod display;
pub mod error;
pub mod firstboot;
pub mod guest;
pub mod guestfs;
pub mod host;
pub mod im4p;
pub mod manifest;
pub mod net;
pub mod nvram;
pub mod plist_py;
pub mod prefs;
pub mod progress;
pub mod ramdisk;
pub mod resources;
pub mod restore_options;
pub mod sign;
pub mod steps;
pub mod ssh;
pub mod tree;
pub mod trustcache;
