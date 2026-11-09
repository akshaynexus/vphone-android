//! The steps of `setup/create-vm.sh`, in order, with one state file per VM.

use super::engine::{Engine, RESTORE_FREE_FLOOR};
use super::state;
use super::vm::VmKind;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vphone_create_steps::display::WidthPolicy;
use vphone_create_steps::error::Error as InstallError;
use vphone_create_steps::host::Host;
use vphone_create_steps::{resources, steps as installer};

const GIB: u64 = 1 << 30;

pub struct StepDef {
    pub key: &'static str,
    pub title: &'static str,
    /// A rough time on a phone, not measured: emulated steps are scaled from the desktop's
    /// timings by how much slower TCG runs, and corrected as completed steps come in.
    pub est_secs: u64,
    /// Whether the time is spent running the emulated guest.
    pub emulated: bool,
    pub floor: u64,
    pub run: fn(&Arc<Engine>) -> Result<(), String>,
}

pub static STEPS: &[StepDef] = &[
    StepDef { key: "sanity", title: "Check the phone", est_secs: 30, emulated: false, floor: 2 * GIB, run: sanity },
    StepDef { key: "bundle", title: "Make the VM's empty disk and files", est_secs: 300, emulated: false, floor: 2 * GIB, run: bundle },
    StepDef { key: "firmware", title: "Download firmware from Apple", est_secs: 3600, emulated: false, floor: 2 * GIB, run: firmware },
    StepDef { key: "gpu-driver", title: "Extract the GPU driver from cloudOS", est_secs: 9000, emulated: true, floor: 5 * GIB, run: gpu_driver },
    StepDef { key: "prepare", title: "Merge the firmware", est_secs: 2400, emulated: false, floor: 16 * GIB, run: prepare },
    StepDef { key: "patch", title: "Patch the firmware", est_secs: 900, emulated: false, floor: 3 * GIB, run: patch },
    StepDef { key: "restore", title: "Restore iOS onto the VM's disk", est_secs: 10800, emulated: true, floor: RESTORE_FREE_FLOOR, run: restore },
    // Ends with the snapshot flip and the restore tree put back, as do_cfw does.
    StepDef { key: "cfw", title: "Install the custom firmware", est_secs: 10800, emulated: true, floor: 9 * GIB, run: cfw },
    StepDef { key: "booted", title: "First boot and setup", est_secs: 7200, emulated: true, floor: 3 * GIB, run: first_boot },
];

const CLI: &str = "libvphone_cli.so";
const PATCH_STARTED: &str = ".patch-started";

pub fn run(e: &Arc<Engine>) -> Result<(), String> {
    e.spawn_pause_watch();
    run_steps(e, STEPS)
}

fn run_steps(e: &Arc<Engine>, steps: &'static [StepDef]) -> Result<(), String> {
    // Whatever an earlier process left running of this VM goes first.
    if let Some(run) = stale_vm_run(e) {
        let _ = e.sup.adopt(&run);
        e.append_log("A VM from an earlier run was still alive; stopping it.");
        let _ = e.sup.stop();
    }
    let total = steps.len();
    let mut i = 0;
    let mut redone_after_pause: Option<&str> = None;
    while i < total {
        let def = &steps[i];
        if state::done(&e.vm, def.key) {
            e.append_log(&format!("  • {}  (already done)", def.key));
            i += 1;
            continue;
        }
        e.is_cancelled()?;
        if def.key == "patch" {
            // A failed patch leaves the tree part patched: prepare runs again first.
            if state::done(&e.vm, "prepare") && e.vm.join(PATCH_STARTED).exists() {
                e.append_log("  an earlier fw patch did not finish: preparing the firmware again first");
                state::unmark(&e.vm, "prepare")?;
                let _ = fs::remove_file(e.vm.join(PATCH_STARTED));
                i -= 1;
                continue;
            }
        }
        e.wait_while_paused()?;
        e.hold_until_clear(def.floor)?;
        e.take_paused_in_step();
        e.begin_step(def.key);
        e.append_log(&format!("▶ {} ({}/{})", def.key, i + 1, total));
        if def.key == "patch" {
            fs::write(e.vm.join(PATCH_STARTED), b"").map_err(|err| err.to_string())?;
        }
        let started = Instant::now();
        let result = (def.run)(e);
        match &result {
            Ok(()) => {
                state::mark(&e.vm, def.key)?;
                e.end_step(def.key, "done", started.elapsed());
            }
            Err(m) => {
                let cancelled = m == "cancelled";
                if !cancelled && e.take_paused_in_step() && redone_after_pause != Some(def.key) {
                    redone_after_pause = Some(def.key);
                    e.append_log(&format!("step {} stopped after a pause ({m}); running it again", def.key));
                    e.end_step(def.key, "pending", started.elapsed());
                    let _ = e.sup.stop();
                    e.usb_down();
                    continue;
                }
                e.append_log(&format!("step {} {}", def.key, if cancelled { "cancelled" } else { "failed" }));
                if !cancelled {
                    e.append_log(m);
                }
                e.end_step(def.key, if cancelled { "pending" } else { "failed" }, started.elapsed());
                let _ = e.sup.stop();
                e.usb_down();
                return result;
            }
        }
        i += 1;
    }
    let _ = e.sup.stop();
    e.usb_down();
    if e.spec.delete_firmware_after {
        e.delete_cached_firmware();
    }
    Ok(())
}

fn stale_vm_run(e: &Engine) -> Option<PathBuf> {
    super::vm::read_pid(&e.paths).map(|_| e.vm.join(".runs-dfu"))
}

fn sanity(e: &Arc<Engine>) -> Result<(), String> {
    let report = super::preflight(&e.paths, &e.spec);
    for w in &report.warnings {
        e.append_log(&format!("warning: {w}"));
    }
    if !report.errors.is_empty() {
        return Err(report.errors.join("\n"));
    }
    let _ = fs::create_dir_all(e.paths.fw_cache());
    let _ = fs::create_dir_all(e.paths.files.join("home"));
    let _ = fs::create_dir_all(e.paths.cache.join("tmp"));
    Ok(())
}

fn compat_json(e: &Engine) -> Result<PathBuf, String> {
    let path = e.paths.files.join("create").join("compatibility.json");
    fs::write(&path, super::compat::json_text()).map_err(|err| err.to_string())?;
    Ok(path)
}

/// What the installer crate reports is text here, with a cancel kept distinct.
fn outcome<T>(e: &Engine, r: std::result::Result<T, InstallError>) -> Result<T, String> {
    r.map_err(|err| if matches!(err, InstallError::Cancelled) || e.is_cancelled().is_err() {
        "cancelled".to_string()
    } else {
        err.to_string()
    })
}

fn assets(e: &Engine) -> PathBuf {
    e.paths.fw_cache()
}

fn release(e: &Arc<Engine>) -> Result<resources::Release, String> {
    outcome(e, resources::fetch_release(&assets(e), e.as_ref()))
}

/// Waits for an allowed network: the bundle step downloads vphone-cli's release.
fn wait_network(e: &Engine) -> Result<(), String> {
    use super::download::Gate;
    while let Some(reason) = e.blocked() {
        e.is_cancelled()?;
        e.hold(&reason);
        std::thread::sleep(Duration::from_secs(3));
    }
    e.hold("");
    Ok(())
}

fn bundle(e: &Arc<Engine>) -> Result<(), String> {
    wait_network(e)?;
    let compat = compat_json(e)?;
    let params = installer::CreateBundle {
        vm: &e.vm,
        device: &e.spec.device,
        ios: &e.spec.ios,
        scale: e.spec.scale,
        width_policy: WidthPolicy::AlignTo16,
        compat_json: &compat,
        assets: &assets(e),
        ecid: None,
    };
    outcome(e, installer::create_bundle(&params, e.as_ref())).map(drop)
}

fn firmware(e: &Arc<Engine>) -> Result<(), String> {
    let fw = e.spec.firmware.as_ref().ok_or("the VM has no firmware plan")?;
    e.fetch_firmware(fw)
}

fn gpu_bundle(e: &Engine) -> PathBuf {
    e.paths.fw_cache().join("AppleParavirtGPUMetalIOGPUFamily.bundle")
}

fn fw_paths(e: &Engine) -> Result<(PathBuf, PathBuf), String> {
    let fw = e.spec.firmware.as_ref().ok_or("the VM has no firmware plan")?;
    let cache = e.paths.fw_cache();
    Ok((cache.join(&fw.ios.file), cache.join(&fw.cloud.file)))
}

fn library(e: &Engine) -> String {
    e.paths.vms_internal().to_string_lossy().into_owned()
}

fn ecid(e: &Engine) -> Result<String, String> {
    fs::read_to_string(e.vm.join("ecid"))
        .map(|t| t.trim().to_string())
        .map_err(|err| format!("the VM has no ECID: {err}"))
}

fn prepare(e: &Arc<Engine>) -> Result<(), String> {
    let (ipsw, cloud) = fw_paths(e)?;
    let gpu = gpu_bundle(e);
    if !gpu.is_dir() {
        return Err("the GPU driver bundle is not there; run the gpu-driver step".into());
    }
    if let Ok(entries) = fs::read_dir(&e.vm) {
        for entry in entries.flatten() {
            let n = entry.file_name().to_string_lossy().into_owned();
            if n.ends_with("_Restore") || n == "FirmwareOriginals" {
                e.append_log(&format!("  removing the unfinished {n} of an earlier run"));
                let _ = if entry.path().is_dir() {
                    fs::remove_dir_all(entry.path())
                } else {
                    fs::remove_file(entry.path())
                };
            }
        }
    }
    let args = vec![
        "fw".into(), "prepare".into(), "-l".into(), library(e), e.name.clone(),
        "-i".into(), ipsw.to_string_lossy().into_owned(),
        "-c".into(), cloud.to_string_lossy().into_owned(),
        "--gpu-driver-bundle".into(), gpu.to_string_lossy().into_owned(),
    ];
    let log = e.current_log();
    let exec = e.exec(CLI, args, vec![], &log, Some(Duration::from_secs(4 * 3600)));
    e.run_lib(exec, "fw prepare")
}

fn patch(e: &Arc<Engine>) -> Result<(), String> {
    let args = vec!["fw".into(), "patch".into(), "-l".into(), library(e), e.name.clone()];
    let log = e.current_log();
    let exec = e.exec(CLI, args, vec![], &log, Some(Duration::from_secs(2 * 3600)));
    e.run_lib(exec, "fw patch")
}

/// `fw restore` of a fresh target: a restore writes the disk from the start, and a VM's SEP
/// storage must be zero for it (a VM restored repeatedly panicked in the SEP key store).
fn reset_targets(e: &Engine) -> Result<(), String> {
    for (file, len) in [("nvram.bin", 33_570_816u64), ("SEPStorage", 524_288), ("Disk.img", 64 << 30)] {
        let path = e.vm.join(file);
        let f = fs::OpenOptions::new().write(true).open(&path).map_err(|err| format!("{file}: {err}"))?;
        f.set_len(0).map_err(|err| format!("{file}: {err}"))?;
        f.set_len(len).map_err(|err| format!("{file}: {err}"))?;
    }
    Ok(())
}

fn probe(e: &Engine, run: &std::path::Path) -> Result<(), String> {
    let id = ecid(e)?;
    let log = run.join("probe.log");
    let exec = e.exec(CLI, vec!["recovery-probe".into(), "--ecid".into(), id, "--timeout".into(), "600".into()], vec![], &log, Some(Duration::from_secs(900)));
    e.run_lib(exec, "recovery-probe")
}

/// Runs `exec` while the VM must stay up: a dead QEMU would otherwise leave the tool waiting.
fn run_with_vm(e: &Arc<Engine>, exec: super::exec::Exec, what: &str) -> Result<(), String> {
    let vm_gone = std::cell::Cell::new(false);
    let code = exec.run(&|| {
        let alive = e.sup.alive();
        if !alive {
            vm_gone.set(true);
        }
        e.is_cancelled().is_err() || !alive
    });
    if vm_gone.get() {
        return Err(format!("the VM stopped during {what}: {}", e.sup.exit_detail()));
    }
    match code? {
        0 => Ok(()),
        c => Err(format!("{what} failed with status {c}; see its log")),
    }
}

fn restore(e: &Arc<Engine>) -> Result<(), String> {
    e.usb_up()?;
    e.sup.stop()?;
    reset_targets(e)?;
    let run = e.vm.join(".runs-dfu");
    e.start_vm(VmKind::Dfu, &e.vm, &run)?;
    e.status(true, |s| s.message = "Waiting for the VM's DFU mode".into());
    probe(e, &run)?;
    e.status(true, |s| s.message = "Restoring iOS onto the virtual disk".into());
    e.mirror_log(&run.join("restore.log"));
    let mut exec = e.exec(
        CLI,
        vec!["restore".into(), "-l".into(), library(e), e.name.clone(), "--ecid".into(), ecid(e)?, "-v".into()],
        vec![("VPHONE_RESTORE_TIMEOUT_SCALE".into(), "10".into())],
        &run.join("restore.log"),
        Some(Duration::from_secs(8 * 3600)),
    );
    // restore-wait.sh: a device that does not come back after a finished restore is not a failure.
    exec.done_when = Some((vec!["status: restore finished".into(), "restore finished".into()], Duration::from_secs(120)));
    run_with_vm(e, exec, "the restore")?;
    e.sup.stop()
}

/// Temporary VMs from a killed run, which `gpu_driver` names by process id.
fn clear_stale_gpu_vms(e: &Engine) {
    if let Ok(entries) = fs::read_dir(e.paths.vms_internal()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("gpu-recovery-") {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn gpu_driver(e: &Arc<Engine>) -> Result<(), String> {
    clear_stale_gpu_vms(e);
    let existing = gpu_bundle(e).join("AppleParavirtGPUMetalIOGPUFamily");
    if existing.is_file() {
        e.append_log(&format!("GPU driver already cached: {}", gpu_bundle(e).display()));
        return Ok(());
    }
    e.usb_up()?;
    let rel = release(e)?;
    let (_, cloud) = fw_paths(e)?;
    let log = e.current_log();
    let params = installer::GpuParams {
        host: e.as_ref(),
        library: &e.paths.vms_internal(),
        cloud_ipsw: &cloud,
        out_dir: &assets(e),
        release: &rel,
        log: &log,
    };
    e.status(true, |s| s.message = "Restoring a temporary cloudOS VM to read its GPU driver".into());
    let driver = outcome(e, installer::gpu_driver(&params))?;
    e.append_log(&format!(
        "GPU driver: {} (sha1 {}{})",
        driver.bundle.display(),
        driver.sha1,
        if driver.matches_known { ", the tested driver" } else { ", NOT the tested driver" }
    ));
    Ok(())
}

fn cfw(e: &Arc<Engine>) -> Result<(), String> {
    e.usb_up()?;
    let rel = release(e)?;
    let ecid_value = outcome(e, vphone_create_steps::bundle::read_ecid(&e.vm))?;
    let log = e.current_log();
    let params = installer::CfwParams {
        host: e.as_ref(),
        library: &e.paths.vms_internal(),
        name: &e.name,
        release: &rel,
        assets: &assets(e),
        scale: e.spec.scale,
        ecid: ecid_value,
        log: &log,
    };
    e.status(true, |s| s.message = "Installing the custom firmware".into());
    outcome(e, installer::cfw(&params)).map(drop)
}

fn first_boot(e: &Arc<Engine>) -> Result<(), String> {
    e.status(true, |s| s.message = "First boot: iOS migrates its data; the logo and progress bar can stay up for a long time".into());
    let params = installer::FirstBoot {
        host: e.as_ref(),
        vm: &e.vm,
        scale: e.spec.scale,
        gfx: true,
    };
    outcome(e, installer::first_boot(&params))?;
    // The VM leaves through a clean shutdown; the person launches it in the app's VM screen.
    e.status(true, |s| s.message = "Shutting the VM down cleanly".into());
    let _ = e.vphoned_rpc("system.shutdown", json!({"force": true}), Duration::from_secs(60));
    if e.sup.wait_exit(Duration::from_secs(300)).is_err() {
        e.append_log("the guest did not shut down in five minutes; stopping it");
    }
    e.sup.stop()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::create::vm::VmProcess;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct NoVm;
    impl VmProcess for NoVm {
        fn start(&self) -> Result<(), String> { Ok(()) }
        fn release(&self) {}
        fn exit_reason(&self) -> String { String::new() }
    }

    fn engine(name: &str) -> Arc<Engine> {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/create-steps").join(name);
        let _ = fs::remove_dir_all(&d);
        let paths = Paths { files: d.join("f"), cache: d.join("c"), external: d.join("e"), native_lib: d.join("l") };
        let vm = paths.vms_internal().join("vm");
        fs::create_dir_all(vm.join(state::LOG_DIR)).unwrap();
        state::write_spec(&vm, &state::Spec { name: "vm".into(), ..Default::default() }).unwrap();
        Engine::new(paths, "vm", Arc::new(NoVm)).unwrap()
    }

    macro_rules! def {
        ($key:expr, $run:expr) => {
            StepDef { key: $key, title: $key, est_secs: 1, emulated: false, floor: 0, run: $run }
        };
    }

    static A: AtomicU32 = AtomicU32::new(0);
    static B: AtomicU32 = AtomicU32::new(0);
    static RESUME: [StepDef; 3] = [
        def!("a", |_| { A.fetch_add(1, Ordering::SeqCst); Ok(()) }),
        def!("b", |_| if B.fetch_add(1, Ordering::SeqCst) == 0 { Err("boom".into()) } else { Ok(()) }),
        def!("c", |_| Ok(())),
    ];

    #[test]
    fn a_failure_stops_there_and_a_resume_runs_only_the_rest() {
        let e = engine("resume");
        assert_eq!(run_steps(&e, &RESUME).unwrap_err(), "boom");
        assert!(state::done(&e.vm, "a") && !state::done(&e.vm, "b") && !state::done(&e.vm, "c"));
        // A new engine, as after an app kill.
        let again = Engine::new(e.paths.clone(), "vm", Arc::new(NoVm)).unwrap();
        run_steps(&again, &RESUME).unwrap();
        assert_eq!(A.load(Ordering::SeqCst), 1, "a done step is not repeated");
        assert_eq!(B.load(Ordering::SeqCst), 2);
        assert!(state::done(&e.vm, "c"));
    }

    static P_PREP: AtomicU32 = AtomicU32::new(0);
    static P_PATCH: AtomicU32 = AtomicU32::new(0);
    static PATCH_FLOW: [StepDef; 2] = [
        def!("prepare", |_| { P_PREP.fetch_add(1, Ordering::SeqCst); Ok(()) }),
        def!("patch", |_| if P_PATCH.fetch_add(1, Ordering::SeqCst) == 0 { Err("half patched".into()) } else { Ok(()) }),
    ];

    #[test]
    fn a_failed_patch_prepares_again_before_the_next_patch() {
        let e = engine("patch");
        assert!(run_steps(&e, &PATCH_FLOW).is_err());
        assert_eq!(P_PREP.load(Ordering::SeqCst), 1);
        run_steps(&e, &PATCH_FLOW).unwrap();
        assert_eq!(P_PREP.load(Ordering::SeqCst), 2, "prepare runs again");
        assert_eq!(P_PATCH.load(Ordering::SeqCst), 2);
        assert!(state::done(&e.vm, "patch"));
    }

    static CANCELS: [StepDef; 1] = [def!("x", |_| Err("cancelled".to_string()))];

    #[test]
    fn a_cancelled_step_is_not_recorded_and_the_status_says_pending() {
        let e = engine("cancel");
        assert_eq!(run_steps(&e, &CANCELS).unwrap_err(), "cancelled");
        assert!(!state::done(&e.vm, "x"));
    }
}
