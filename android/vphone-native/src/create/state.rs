//! One creation's state on disk: the same one-key-per-line state file as `create-vm.sh`, the
//! spec the wizard chose, and a status document the UI reads across processes. A creation is
//! defined by these files, so it survives an app kill or a reboot.

use super::compat::Firmware;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `path` then replaces the old one in one step, so a reader never sees half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("new");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// What the wizard chose.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Spec {
    pub name: String,
    pub device: String,
    pub ios: String,
    pub build: String,
    pub cloud_os: String,
    pub preset: String,
    pub scale: u32,
    pub linux_tested: bool,
    /// Downloads may use a metered network. Off by default.
    pub allow_cellular: bool,
    /// Delete this VM's firmware from the cache when the VM is finished.
    pub delete_firmware_after: bool,
    pub firmware: Option<Firmware>,
    /// Leave every background daemon of iOS on. Off by default: the first boot then applies
    /// vphoned's `trimmed` service profile (the installer's `firstboot` module).
    pub keep_all_services: bool,
}

pub const STATE_FILE: &str = ".create-state";
pub const SPEC_FILE: &str = ".create-spec.json";
pub const STATUS_FILE: &str = ".create-status.json";
pub const LOG_DIR: &str = ".create-logs";

pub fn read_spec(vm: &Path) -> Option<Spec> {
    serde_json::from_slice(&fs::read(vm.join(SPEC_FILE)).ok()?).ok()
}

pub fn write_spec(vm: &Path, spec: &Spec) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(spec).map_err(|e| e.to_string())?;
    write_atomic(&vm.join(SPEC_FILE), &json).map_err(|e| format!("cannot save the VM's plan: {e}"))
}

pub fn done(vm: &Path, step: &str) -> bool {
    fs::read_to_string(vm.join(STATE_FILE))
        .is_ok_and(|t| t.lines().any(|l| l == step))
}

pub fn mark(vm: &Path, step: &str) -> Result<(), String> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(vm.join(STATE_FILE))
        .map_err(|e| e.to_string())?;
    writeln!(f, "{step}").map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())
}

pub fn unmark(vm: &Path, step: &str) -> Result<(), String> {
    let text = fs::read_to_string(vm.join(STATE_FILE)).unwrap_or_default();
    let kept: String = text
        .lines()
        .filter(|l| *l != step)
        .map(|l| format!("{l}\n"))
        .collect();
    write_atomic(&vm.join(STATE_FILE), kept.as_bytes()).map_err(|e| e.to_string())
}

pub fn step_log(vm: &Path, step: &str) -> PathBuf {
    vm.join(LOG_DIR).join(format!("{step}.log"))
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct StepStatus {
    pub key: String,
    pub title: String,
    /// "pending", "running", "done", "failed"
    pub state: String,
    pub elapsed_secs: u64,
    /// How long this step last took on this phone, in running time; meaningful with `measured`.
    pub est_secs: u64,
    /// Whether this step has ever finished on this phone.
    pub measured: bool,
    pub log: String,
}

/// One phase of the running step (the first boot's: kernel boot, data migration, ...).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PhaseStatus {
    pub key: String,
    pub title: String,
    /// "pending", "running", "done"
    pub state: String,
    /// How long it took, once done.
    pub elapsed_secs: u64,
    /// When it began, for the screen's "running for"; 0 before it has.
    pub started_ms: u64,
    /// How long it last took on this phone; meaningful with `measured`.
    pub est_secs: u64,
    pub measured: bool,
}

/// What the UI shows; written by the running creation, read by any process.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    pub name: String,
    pub device: String,
    pub ios: String,
    /// "preparing", "running", "paused", "failed", "cancelled", "done"
    pub state: String,
    pub step: String,
    pub steps: Vec<StepStatus>,
    pub message: String,
    /// Why progress is on hold: "", "wifi", "network", "storage", "thermal".
    pub hold: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_sec: u64,
    /// Remaining seconds for the whole creation; 0 when unknown.
    pub eta_secs: u64,
    pub thermal: i32,
    pub error: String,
    pub log: String,
    pub updated_ms: u64,
    /// When the running step's current attempt began; 0 when none has.
    pub step_started_ms: u64,
    /// Why there is no `eta_secs`, in words for the screen; empty when there is one.
    pub eta_note: String,
    /// Whether the person paused it. Read from the pause file when the status is asked for.
    pub paused: bool,
    /// The running step's phases, where it has any.
    pub phases: Vec<PhaseStatus>,
}

/// Held by the process running a creation. The kernel drops it when that process ends, however it
/// ends, so it says whether a run is alive without guessing from how recent its status is.
pub const LOCK_FILE: &str = ".create-lock";

fn flock(file: &File, operation: i32) -> bool {
    // SAFETY: the descriptor is valid for the call's duration.
    unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) == 0 }
}

/// Takes the run's lock; `None` when another process holds it.
pub fn acquire_lock(vm: &Path) -> Option<File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(vm.join(LOCK_FILE))
        .ok()?;
    // A status check holds a shared lock for an instant; only a real run keeps it.
    for _ in 0..40 {
        if flock(&file, libc::LOCK_EX) {
            return Some(file);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

/// Whether a process is running this creation now.
pub fn run_is_live(vm: &Path) -> bool {
    let Ok(file) = File::open(vm.join(LOCK_FILE)) else {
        return false;
    };
    !flock(&file, libc::LOCK_SH)
}

pub fn read_status(vm: &Path) -> Option<Status> {
    serde_json::from_slice(&fs::read(vm.join(STATUS_FILE)).ok()?).ok()
}

pub fn write_status(vm: &Path, status: &Status) {
    if let Ok(json) = serde_json::to_vec(status) {
        let _ = write_atomic(&vm.join(STATUS_FILE), &json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../build/test-fixtures/create-state")
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn steps_survive_and_unmark() {
        let d = dir("steps");
        assert!(!done(&d, "bundle"));
        mark(&d, "bundle").unwrap();
        mark(&d, "firmware").unwrap();
        assert!(done(&d, "firmware") && done(&d, "bundle"));
        unmark(&d, "bundle").unwrap();
        assert!(!done(&d, "bundle") && done(&d, "firmware"));
    }

    #[test]
    fn status_round_trips_atomically() {
        let d = dir("status");
        let s = Status {
            name: "x".into(),
            state: "running".into(),
            ..Default::default()
        };
        write_status(&d, &s);
        assert_eq!(read_status(&d).unwrap().name, "x");
        assert!(fs::read_dir(&d).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".new")));
    }
}
