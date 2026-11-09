//! On-device VM creation: a resumable port of `setup/create-vm.sh`.
//!
//! The UI and the running creation are in different processes and talk through files in the VM
//! directory, which is also what makes a creation survive an app kill or a reboot.

pub mod compat;
pub mod ctl;
pub mod download;
pub mod engine;
pub mod exec;
pub mod logview;
mod mirror;
pub mod state;
pub mod steps;
pub mod vm;

#[cfg(target_os = "android")]
mod jni;

use crate::meminfo::MemInfo;
use crate::paths::Paths;
use engine::free_bytes;
use serde::Serialize;
use state::{Spec, Status};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

const GIB: u64 = 1 << 30;
/// Helpers a creation execs from nativeLibraryDir; the packaging step refuses an APK without them.
pub const REQUIRED_LIBS: &[&str] = &[
    "libvphone_cli.so",
    "libusb-1.0.so",
    "libvusbd.so",
    "libusbmuxd_vphone.so",
    "libvphone_apfs.so",
    "libqemu-vphone.so",
];
const FIRST_RESTORE_MIN_GB: u64 = 25;
const COMFORTABLE_GB: u64 = 60;
const DELETE_MARKER: &str = ".create-delete-after";

fn active_file(paths: &Paths) -> PathBuf {
    paths.files.join("create").join("active")
}

fn vm_path(paths: &Paths, name: &str) -> PathBuf {
    paths.vms_internal().join(name)
}

/// A name usable as a directory and in QEMU's option strings.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("the name must be 1 to 64 characters".into());
    }
    if name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err("use letters, digits, '.', '_' and '-'; it must not start with a dot".into());
    }
    Ok(())
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub free_gb: u64,
    pub want_gb: u64,
    pub low_hours: u64,
    pub high_hours: u64,
}

pub fn preflight(paths: &Paths, spec: &Spec) -> Preflight {
    let mut p = Preflight::default();
    match compat::find_pairing(&spec.device, &spec.ios) {
        Ok(pairing) => {
            if let Err(e) = compat::resolve(&pairing) {
                p.errors.push(e);
            }
            if !pairing.linux_tested {
                p.warnings.push(
                    "This pairing is under test: it has been verified on macOS only, so a first run may need manual attention.".into(),
                );
            }
        }
        Err(e) => p.errors.push(e),
    }
    if let Err(e) = valid_name(&spec.name) {
        p.errors.push(e);
    }
    let dir = vm_path(paths, &spec.name);
    if (dir.exists() && !dir.join(state::SPEC_FILE).exists()) || paths.vms().join(&spec.name).exists() {
        p.errors.push(format!("A VM named {} already exists.", spec.name));
    }
    let mem = MemInfo::read();
    if mem.total_mib < 8 * 1024 {
        p.errors.push(format!(
            "This phone has {} GiB of RAM; creating a VM needs at least 8.",
            mem.total_mib / 1024
        ));
    } else if mem.usable_mib() < 7 * 1024 {
        p.warnings.push(format!(
            "Only {} GiB of memory is free right now. Close other apps first; a VM process that runs out of memory stops the creation.",
            mem.usable_mib() / 1024
        ));
    }
    let _ = fs::create_dir_all(&paths.files);
    let free = free_bytes(&paths.files);
    p.free_gb = free / GIB;
    p.want_gb = COMFORTABLE_GB;
    if let Some(fw) = compat::find_pairing(&spec.device, &spec.ios)
        .ok()
        .and_then(|pair| compat::resolve(&pair).ok())
    {
        if paths.fw_cache().join(&fw.ios.file).exists() {
            p.want_gb = p.want_gb.saturating_sub(12);
        }
    }
    if p.free_gb < FIRST_RESTORE_MIN_GB {
        p.errors.push(format!(
            "Only {} GB is free. Creating a VM needs at least {FIRST_RESTORE_MIN_GB} GB free during its restore, and about {COMFORTABLE_GB} GB in all.",
            p.free_gb
        ));
    } else if p.free_gb < p.want_gb {
        p.warnings.push(format!(
            "{} GB is free, about {} GB is needed in all. Free more space, or creation will pause when it runs short.",
            p.free_gb, p.want_gb
        ));
    }
    for lib in REQUIRED_LIBS {
        if !paths.lib(lib).is_file() {
            p.errors.push(format!("{lib} is missing from the app's native libraries"));
        }
    }
    let secs: u64 = steps::STEPS.iter().map(|s| s.est_secs).sum();
    p.low_hours = (secs as f64 * 0.7 / 3600.0).round() as u64;
    p.high_hours = (secs as f64 * 2.0 / 3600.0).round() as u64;
    p
}

pub fn preflight_json(paths: &Paths, spec_json: &str) -> String {
    let spec: Spec = serde_json::from_str(spec_json).unwrap_or_default();
    serde_json::to_string(&preflight(paths, &spec)).unwrap_or_default()
}

/// Records the wizard's choice; the creation itself runs in the service.
pub fn begin(paths: &Paths, spec_json: &str) -> Result<(), String> {
    let mut spec: Spec = serde_json::from_str(spec_json).map_err(|e| e.to_string())?;
    valid_name(&spec.name)?;
    if let Some(other) = pending(paths) {
        if other != spec.name {
            return Err(format!("{other} is still being created. Finish or delete it first."));
        }
    }
    let pairing = compat::find_pairing(&spec.device, &spec.ios)?;
    let scale = spec.scale.clamp(1, 3).max(compat::min_scale(&spec.ios));
    let firmware = compat::resolve(&pairing)?;
    let check = preflight(paths, &spec);
    if let Some(e) = check.errors.first() {
        return Err(e.clone());
    }

    let dir = vm_path(paths, &spec.name);
    fs::create_dir_all(dir.join(state::LOG_DIR)).map_err(|e| format!("cannot create the VM: {e}"))?;
    spec.build = pairing.build;
    spec.cloud_os = pairing.cloud_os;
    spec.preset = pairing.preset;
    spec.scale = scale;
    spec.linux_tested = pairing.linux_tested;
    spec.firmware = Some(firmware);
    state::write_spec(&dir, &spec)?;
    fs::create_dir_all(paths.files.join("create")).map_err(|e| e.to_string())?;
    state::write_atomic(&active_file(paths), spec.name.as_bytes()).map_err(|e| e.to_string())?;
    state::write_status(
        &dir,
        &Status {
            name: spec.name.clone(),
            device: spec.device.clone(),
            ios: spec.ios.clone(),
            state: "running".into(),
            message: "Starting".into(),
            updated_ms: state::now_ms(),
            ..Default::default()
        },
    );
    Ok(())
}

/// The creation in progress (running, failed, cancelled or interrupted), if any.
pub fn pending(paths: &Paths) -> Option<String> {
    let name = fs::read_to_string(active_file(paths)).ok()?;
    let name = name.trim().to_string();
    let vm = vm_path(paths, &name);
    (!name.is_empty() && vm.join(state::SPEC_FILE).is_file()).then_some(name)
}

/// The status the UI shows: a "running" one with no process behind it is "interrupted".
fn ui_status(paths: &Paths, name: &str) -> Status {
    let vm = vm_path(paths, name);
    let mut status = state::read_status(&vm).unwrap_or_default();
    status.paused = vm.join(engine::PAUSE_FILE).exists();
    if status.state == "running" && !state::run_is_live(&vm) {
        status.state = "interrupted".into();
        status.message = "It stopped while the app or the phone restarted; tap Resume.".into();
    }
    logview::apply_step_times(&vm, &mut status);
    status
}

pub fn status_json(paths: &Paths, name: &str) -> String {
    serde_json::to_string(&ui_status(paths, name)).unwrap_or_default()
}

/// The facts for the progress screen's panels; reads the step's log, so not for the main thread.
pub fn live_json(paths: &Paths, name: &str) -> String {
    logview::live_json(paths, &ui_status(paths, name))
}

pub fn request_cancel(paths: &Paths, name: &str) {
    let vm = vm_path(paths, name);
    let _ = fs::write(vm.join(engine::CANCEL_FILE), b"");
    let _ = fs::remove_file(vm.join(engine::PAUSE_FILE));
}

/// Pauses or resumes a running creation; its engine applies this within a second.
pub fn request_pause(paths: &Paths, name: &str, paused: bool) {
    let file = vm_path(paths, name).join(engine::PAUSE_FILE);
    if paused {
        let _ = fs::write(file, b"");
    } else {
        let _ = fs::remove_file(file);
    }
}

pub fn set_cellular(paths: &Paths, name: &str, allow: bool) {
    let file = vm_path(paths, name).join(".create-cellular-ok");
    if allow {
        let _ = fs::write(file, b"");
    } else {
        let _ = fs::remove_file(file);
    }
}

fn running_now(vm: &std::path::Path) -> bool {
    state::read_status(vm).is_some_and(|s| s.state == "running") && state::run_is_live(vm)
}

/// Deletes a VM (and, optionally, its cached firmware). A running creation stops first and the
/// service finishes the removal when it has.
pub fn delete(paths: &Paths, name: &str, also_firmware: bool) -> Result<(), String> {
    let vm = vm_path(paths, name);
    if running_now(&vm) {
        fs::write(vm.join(DELETE_MARKER), if also_firmware { "firmware" } else { "" })
            .map_err(|e| e.to_string())?;
        request_cancel(paths, name);
        return Ok(());
    }
    if also_firmware {
        if let Some(spec) = state::read_spec(&vm) {
            if let Some(fw) = spec.firmware {
                for image in [fw.cloud, fw.ios] {
                    download::remove_cached(&paths.fw_cache().join(image.file));
                }
            }
        }
    }
    if pending(paths).as_deref() == Some(name) {
        let _ = fs::remove_file(active_file(paths));
    }
    fs::remove_dir_all(&vm).map_err(|e| format!("cannot delete {name}: {e}"))
}

fn finish(paths: &Paths, name: &str, keep_active: bool) {
    let vm = vm_path(paths, name);
    if vm.join(DELETE_MARKER).exists() {
        let also = fs::read_to_string(vm.join(DELETE_MARKER)).unwrap_or_default() == "firmware";
        let _ = fs::remove_file(vm.join(DELETE_MARKER));
        // Not "running" anymore, so delete removes it now.
        let _ = delete(paths, name, also);
        return;
    }
    if !keep_active {
        let _ = fs::remove_file(active_file(paths));
    }
}

/// Runs the creation to its end (blocking). Any error text is in the status too.
pub fn run(paths: &Paths, name: &str, process: Arc<dyn vm::VmProcess>) -> Result<(), String> {
    // Held until this returns: a second start of the same creation stops here.
    let Some(_lock) = state::acquire_lock(&vm_path(paths, name)) else {
        return Ok(());
    };
    let e = engine::Engine::new(paths.clone(), name, process)?;
    // Log files hold no times of their own; this notes when each one grew.
    let _stamper = logview::Stamper::spawn(e.vm.join(state::LOG_DIR));
    let _ = fs::remove_file(e.vm.join(engine::CANCEL_FILE));
    let _ = fs::remove_file(e.vm.join(engine::PAUSE_FILE));
    ctl::clear_cancel();
    let result = steps::run(&e);
    let state_text = match &result {
        Ok(()) => "done",
        Err(m) if m == "cancelled" => "cancelled",
        Err(_) => "failed",
    };
    let error = result.clone().err().filter(|m| m != "cancelled").unwrap_or_default();
    e.status(true, |s| {
        s.state = state_text.into();
        s.hold.clear();
        if state_text != "running" {
            s.done_bytes = 0;
            s.total_bytes = 0;
        }
        s.error = error;
    });
    finish(paths, name, state_text != "done");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(name: &str) -> Paths {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../build/test-fixtures/create-mod")
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("lib")).unwrap();
        for lib in REQUIRED_LIBS {
            fs::write(d.join("lib").join(lib), b"x").unwrap();
        }
        Paths {
            files: d.join("files"),
            cache: d.join("cache"),
            external: d.join("ext"),
            native_lib: d.join("lib"),
        }
    }

    fn spec_json(name: &str) -> String {
        serde_json::json!({"name": name, "device": "iPhone17,3", "ios": "26.6.2", "scale": 1}).to_string()
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_name("vphone-iPhone17-3-2662").is_ok());
        assert!(valid_name(".hidden").is_err());
        assert!(valid_name("a b").is_err());
        assert!(valid_name("a,b").is_err());
    }

    #[test]
    fn preflight_reports_time_and_refuses_a_bad_pairing() {
        let p = paths("preflight");
        let spec: Spec = serde_json::from_str(&spec_json("x")).unwrap();
        let r = preflight(&p, &spec);
        assert!(r.low_hours >= 1 && r.high_hours > r.low_hours, "{}", r.low_hours);
        assert!(r.errors.iter().all(|e| !e.contains("missing")), "{:?}", r.errors);
        let bad: Spec = serde_json::from_str(&serde_json::json!({"name": "x", "device": "iPhone11,6", "ios": "18.6"}).to_string()).unwrap();
        assert!(preflight(&p, &bad).errors.iter().any(|e| e.contains("macOS kmutil")));
    }

    /// Whatever the test machine's disk has free, the rest of the plan holds.
    fn begin_ignoring_storage(p: &Paths, name: &str) {
        match begin(p, &spec_json(name)) {
            Ok(()) => {}
            Err(e) if e.contains("free") || e.contains("RAM") => {
                let dir = vm_path(p, name);
                fs::create_dir_all(dir.join(state::LOG_DIR)).unwrap();
                let mut s: Spec = serde_json::from_str(&spec_json(name)).unwrap();
                s.firmware = compat::resolve(&compat::find_pairing("iPhone17,3", "26.6.2").unwrap()).ok();
                state::write_spec(&dir, &s).unwrap();
                fs::create_dir_all(p.files.join("create")).unwrap();
                fs::write(active_file(p), name).unwrap();
                state::write_status(&dir, &Status { name: name.into(), state: "running".into(), updated_ms: state::now_ms(), ..Default::default() });
            }
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_stale_running_status_reads_as_interrupted_and_pending_survives() {
        let p = paths("stale");
        begin_ignoring_storage(&p, "vm1");
        assert_eq!(pending(&p).as_deref(), Some("vm1"));
        let vm = vm_path(&p, "vm1");
        let mut s = state::read_status(&vm).unwrap();
        s.updated_ms = state::now_ms() - 10 * 60 * 1000;
        state::write_status(&vm, &s);
        let out: serde_json::Value = serde_json::from_str(&status_json(&p, "vm1")).unwrap();
        assert_eq!(out["state"], "interrupted");
    }

    #[test]
    fn a_running_status_is_live_only_while_its_process_holds_the_lock() {
        let p = paths("lock");
        begin_ignoring_storage(&p, "vm1");
        let vm = vm_path(&p, "vm1");
        let state_now = |p: &Paths| -> String {
            let out: serde_json::Value = serde_json::from_str(&status_json(p, "vm1")).unwrap();
            out["state"].as_str().unwrap().to_string()
        };
        let lock = state::acquire_lock(&vm).unwrap();
        // However old its last status, a run that is alive is not "interrupted".
        let mut s = state::read_status(&vm).unwrap();
        s.updated_ms = state::now_ms() - 60 * 60 * 1000;
        state::write_status(&vm, &s);
        assert_eq!(state_now(&p), "running");
        assert!(state::acquire_lock(&vm).is_none(), "a second run must not start");
        drop(lock);
        assert_eq!(state_now(&p), "interrupted");
    }

    #[test]
    fn pausing_shows_in_the_status_and_a_stop_clears_it() {
        let p = paths("pause");
        begin_ignoring_storage(&p, "vm1");
        let paused = |p: &Paths| -> bool {
            let out: serde_json::Value = serde_json::from_str(&status_json(p, "vm1")).unwrap();
            out["paused"].as_bool().unwrap()
        };
        assert!(!paused(&p));
        request_pause(&p, "vm1", true);
        assert!(paused(&p));
        request_pause(&p, "vm1", false);
        assert!(!paused(&p));
        request_pause(&p, "vm1", true);
        request_cancel(&p, "vm1");
        assert!(!paused(&p));
    }

    #[test]
    fn a_second_creation_is_refused_until_the_first_is_gone() {
        let p = paths("second");
        begin_ignoring_storage(&p, "vm1");
        let e = begin(&p, &spec_json("vm2")).unwrap_err();
        assert!(e.contains("still being created") || e.contains("free") || e.contains("RAM"), "{e}");
        let vm = vm_path(&p, "vm1");
        let mut s = state::read_status(&vm).unwrap();
        s.state = "failed".into();
        state::write_status(&vm, &s);
        delete(&p, "vm1", false).unwrap();
        assert!(pending(&p).is_none());
        assert!(!vm_path(&p, "vm1").exists());
    }

    #[test]
    fn deleting_a_running_creation_defers_to_the_service() {
        let p = paths("defer");
        begin_ignoring_storage(&p, "vm1");
        let vm = vm_path(&p, "vm1");
        let lock = state::acquire_lock(&vm).unwrap();
        delete(&p, "vm1", true).unwrap();
        drop(lock);
        assert!(vm.join(DELETE_MARKER).exists() && vm.join(engine::CANCEL_FILE).exists());
        // Once the run has ended and written that it stopped, the marker finishes the removal.
        let mut s = state::read_status(&vm).unwrap();
        s.state = "cancelled".into();
        state::write_status(&vm, &s);
        finish(&p, "vm1", true);
        assert!(!vm.exists());
    }
}
