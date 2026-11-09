//! What the steps need from the app: the VM process and helper executables.
//! The orchestrator implements this; see android/build/COORDINATION-create.md.

use crate::progress::Progress;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How the VM is started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmKind {
    /// DFU, for a restore (or, with `auto-boot=false`, for `restore --boot-ramdisk`).
    Dfu,
    /// Same start as DFU, kept separate so a run directory can be told apart.
    Ramdisk,
    /// A normal boot.
    FirstBoot { gfx: bool },
}

/// A helper the host started in the background; `stop` kills it (and its
/// children) and waits for it.
pub trait BackgroundHelper: Send {
    /// `Some(exit status)` once it has exited.
    fn finished(&mut self) -> Option<i32>;
    fn stop(self: Box<Self>);
}

/// The host side of the steps.
pub trait Host: Progress {
    fn start_vm(&self, kind: VmKind, vm: &Path, run: &Path) -> Result<(), String>;
    fn stop_vm(&self, vm: &Path) -> Result<(), String>;
    fn vm_alive(&self) -> bool;
    fn serial_log(&self) -> PathBuf;
    /// `POST /v1/rpc` on the guest's vphoned; the result object.
    fn vphoned_rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String>;
    /// Run a `lib*.so` helper to completion. Exit status, Err only when it could not be run.
    fn run_helper(
        &self,
        lib: &str,
        args: &[String],
        env: &[(String, String)],
        log: &Path,
        timeout: Option<Duration>,
    ) -> Result<i32, String>;
    /// Start a helper in the background (`restore --boot-ramdisk` never ends on its own).
    fn spawn_helper(
        &self,
        lib: &str,
        args: &[String],
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Box<dyn BackgroundHelper>, String>;
    /// `libvphone_portfwd`-equivalent: a byte stream to `device_port` on the guest
    /// over usbmux, for SSH. Default: `vusb_portfwd::connect` on the host's socket.
    fn usbmux_socket(&self) -> PathBuf;
}
