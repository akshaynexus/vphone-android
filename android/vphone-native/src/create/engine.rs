//! One running creation: its files, its status document, its helpers and its VM.

use super::compat::Firmware;
use super::download::{self, Gate};
use super::exec::{Background, Exec};
use super::mirror::{self, Mirror};
use super::state::{self, PhaseStatus, Spec, Status, StepStatus};
use super::steps::STEPS;
use super::vm::{display, vitals, Supervisor, VmKind, VmProcess};
use crate::paths::Paths;
use crate::platform::log;
use crate::vphoned::Client;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vphone_create_steps::active;
use vphone_create_steps::host::{BackgroundHelper, Host};
use vphone_create_steps::progress::Progress;

const TAG: &str = "vphone-create";
const GIB: u64 = 1 << 30;
pub const CANCEL_FILE: &str = ".create-cancel";
pub const PAUSE_FILE: &str = ".create-pause";
const CELLULAR_FILE: &str = ".create-cellular-ok";
/// QEMU's `-D` file, written in the boot's run directory.
const QEMU_LOG: &str = "qemu.log";
const MUX_LOG: &str = "usbmuxd.log";
/// How little free space a step may start with: 25 GiB for the restore (it writes the disk),
/// a few GiB elsewhere. Below these the creation waits for space, it does not fail.
pub const RESTORE_FREE_FLOOR: u64 = 25 * GIB;
const GENERAL_FREE_FLOOR: u64 = 3 * GIB;
const VM_FREE_FLOOR: u64 = 2 * GIB;
/// The step whose phases are timed.
pub const FIRST_BOOT_STEP: &str = "booted";

/// Where the steps' measured times are kept, for every VM made on this phone.
pub fn measured_file(paths: &Paths) -> PathBuf {
    paths.files.join("create").join("step-times.json")
}

pub fn read_measured(paths: &Paths) -> BTreeMap<String, u64> {
    fs::read(measured_file(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// A step that ended at once had its work done already (a cached download, a driver read before):
/// that says nothing new about how long the work takes, so an earlier, longer time is kept.
fn kept_measurement(before: Option<u64>, now: u64) -> u64 {
    const NO_WORK_SECS: u64 = 5;
    match before {
        Some(before) if now < NO_WORK_SECS && before >= NO_WORK_SECS => before,
        _ => now,
    }
}

/// The time left and, when there is none to give, why. `running_secs` is how long the step
/// `current` has run.
pub fn estimate(steps: &[StepStatus], current: &str, running_secs: u64) -> (u64, String) {
    let left: Vec<&StepStatus> = steps.iter().filter(|st| st.state != "done").collect();
    if left.is_empty() {
        return (0, String::new());
    }
    let unmeasured = left.iter().filter(|st| !st.measured).count();
    if unmeasured > 0 {
        let which = match (unmeasured, left.len()) {
            (1, 1) => "the step left has never finished on this phone".to_string(),
            (u, l) if u == l => format!("none of the {l} steps left has finished on this phone before"),
            (1, l) => format!("1 of the {l} steps left has never finished on this phone"),
            (u, l) => format!("{u} of the {l} steps left have never finished on this phone"),
        };
        return (0, format!("No time estimate yet: {which}"));
    }
    let secs = left
        .iter()
        .map(|st| if st.key == current { st.est_secs.saturating_sub(running_secs) } else { st.est_secs })
        .sum();
    (secs, String::new())
}

/// The key a phase's time is kept under in the measured times, beside the steps' own.
pub fn phase_key(step: &str, phase: &str) -> String {
    format!("{step}.{phase}")
}

/// The phases a step starts with, each with what it last took on this phone.
fn phases_of(step: &str, measured: &BTreeMap<String, u64>) -> Vec<PhaseStatus> {
    if step != FIRST_BOOT_STEP {
        return Vec::new();
    }
    vphone_create_steps::firstboot::PHASES
        .iter()
        .map(|(key, title)| {
            let last = measured.get(&phase_key(step, key)).copied();
            PhaseStatus {
                key: (*key).into(),
                title: (*title).into(),
                state: "pending".into(),
                est_secs: last.unwrap_or(0),
                measured: last.is_some(),
                ..Default::default()
            }
        })
        .collect()
}

/// The time the phases still take, from what each last took on this phone, with `running_secs`
/// of the running one gone; `None` where one of them has never finished here, or none is left.
pub fn phases_left(phases: &[PhaseStatus], running_secs: u64) -> Option<u64> {
    let left: Vec<&PhaseStatus> = phases.iter().filter(|p| matches!(p.state.as_str(), "pending" | "running")).collect();
    if left.is_empty() || left.iter().any(|p| !p.measured) {
        return None;
    }
    Some(left.iter().map(|p| if p.state == "running" { p.est_secs.saturating_sub(running_secs) } else { p.est_secs }).sum())
}

/// `key` begins: the phase running until now is done after `ran_secs`, and those listed before
/// `key` that never began are passed over (the guest was not seen entering them). The phase that
/// ended and its time, for the measured record.
pub fn advance_phases(phases: &mut [PhaseStatus], key: &str, ran_secs: u64, now_ms: u64) -> Option<(String, u64)> {
    let at = phases.iter().position(|p| p.key == key)?;
    if phases[at].state != "pending" {
        return None;
    }
    let mut ended = None;
    for phase in phases.iter_mut().take(at) {
        match phase.state.as_str() {
            "running" => {
                phase.state = "done".into();
                phase.elapsed_secs = ran_secs;
                ended = Some((phase.key.clone(), ran_secs));
            }
            "pending" => phase.state = "skipped".into(),
            _ => {}
        }
    }
    phases[at].state = "running".into();
    phases[at].started_ms = now_ms;
    ended
}

/// Where the vCPUs were over the samples since the last line: how many were in user code (EL0)
/// and in the kernel (EL1 and above), and the kernel address seen most, which is the idle loop
/// when the guest is idle and a hot spot when it is not.
#[derive(Default)]
pub struct VcpuWindow {
    samples: u32,
    user: u32,
    kernel_pcs: BTreeMap<u64, u32>,
    /// A vCPU of this boot was seen in user code.
    pub userspace_seen: bool,
}

impl VcpuWindow {
    pub fn observe(&mut self, vcpus: &[crate::qmp::Vcpu]) {
        for vcpu in vcpus {
            self.samples += 1;
            if vcpu.el == 0 {
                self.user += 1;
                self.userspace_seen = true;
            } else {
                *self.kernel_pcs.entry(vcpu.pc).or_insert(0) += 1;
            }
        }
    }

    /// The window in words, and a new window; empty where nothing was sampled.
    pub fn take_note(&mut self) -> String {
        if self.samples == 0 {
            return String::new();
        }
        let kernel = self.samples - self.user;
        let percent = |n: u32| (u64::from(n) * 100 + u64::from(self.samples) / 2) / u64::from(self.samples);
        let mut note = format!("vCPU samples {}: user {}% kernel {}%", self.samples, percent(self.user), percent(kernel));
        if let Some((pc, count)) = self.kernel_pcs.iter().max_by_key(|(pc, count)| (**count, std::cmp::Reverse(**pc))) {
            note += &format!(" (most at {pc:#x}: {}%)", percent(*count));
        }
        let seen = self.userspace_seen;
        *self = VcpuWindow { userspace_seen: seen, ..Default::default() };
        note
    }
}

pub fn free_bytes(dir: &Path) -> u64 {
    let Ok(c) = std::ffi::CString::new(dir.to_string_lossy().as_bytes()) else {
        return 0;
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: st is a valid out-pointer and c a valid path for the call's duration.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    st.f_bavail as u64 * st.f_frsize as u64
}

pub struct Engine {
    pub paths: Paths,
    pub vm: PathBuf,
    pub name: String,
    pub spec: Spec,
    pub sup: Supervisor,
    status: Mutex<Status>,
    status_written: Mutex<Instant>,
    step_started: Mutex<active::ActiveInstant>,
    times: Mutex<BTreeMap<String, u64>>,
    /// How long each step last took on this phone, in running time; what estimates are made of.
    measured: Mutex<BTreeMap<String, u64>>,
    step_log: Mutex<Option<File>>,
    usb: Mutex<Vec<Background>>,
    pause: Mutex<bool>,
    mirror: Mutex<Vec<Followed>>,
    restore_rate: Mutex<mirror::RestoreRate>,
    /// The logs [`Engine::mirror_log`] follows, for the watchdog's "any output yet?".
    followed_paths: Mutex<Vec<PathBuf>>,
    /// The VM is held (pause, thermal, storage): nothing it does counts against a time limit.
    vm_held: Arc<AtomicBool>,
    /// When the running phase began, on the active clock.
    phase_started: Mutex<Option<active::ActiveInstant>>,
    /// The vCPU samples since the first boot's last load line.
    vcpu_window: Mutex<VcpuWindow>,
}

/// A log being followed into the step's log, with what marks its lines there.
struct Followed {
    mirror: Mirror,
    prefix: &'static str,
}

impl Engine {
    pub fn new(paths: Paths, name: &str, process: Arc<dyn VmProcess>) -> Result<Arc<Engine>, String> {
        let vm = paths.vms_internal().join(name);
        let spec = state::read_spec(&vm).ok_or_else(|| format!("{name} has no plan to resume"))?;
        let cancel_vm = vm.clone();
        let cancelled = Arc::new(move || super::ctl::cancelled() || cancel_vm.join(CANCEL_FILE).exists());
        let pause_vm = vm.clone();
        let vm_held = Arc::new(AtomicBool::new(false));
        let held = vm_held.clone();
        let paused = Arc::new(move || held.load(Ordering::SeqCst) || pause_vm.join(PAUSE_FILE).exists());
        let sup = Supervisor::new(paths.clone(), process, cancelled, paused);
        let times = fs::read(vm.join(".create-times.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let measured = read_measured(&paths);
        let steps = STEPS
            .iter()
            .map(|d| StepStatus {
                key: d.key.into(),
                title: d.title.into(),
                state: if state::done(&vm, d.key) { "done".into() } else { "pending".into() },
                est_secs: measured.get(d.key).copied().unwrap_or(0),
                measured: measured.contains_key(d.key),
                log: state::step_log(&vm, d.key).to_string_lossy().into_owned(),
                ..Default::default()
            })
            .collect();
        let status = Status {
            name: name.to_string(),
            device: spec.device.clone(),
            ios: spec.ios.clone(),
            state: "running".into(),
            steps,
            ..Default::default()
        };
        let e = Arc::new(Engine {
            paths,
            vm,
            name: name.to_string(),
            spec,
            sup,
            status: Mutex::new(status),
            status_written: Mutex::new(Instant::now() - Duration::from_secs(60)),
            step_started: Mutex::new(active::ActiveInstant::now()),
            times: Mutex::new(times),
            measured: Mutex::new(measured),
            step_log: Mutex::new(None),
            usb: Mutex::new(Vec::new()),
            pause: Mutex::new(false),
            mirror: Mutex::new(Vec::new()),
            restore_rate: Mutex::new(mirror::RestoreRate::new()),
            followed_paths: Mutex::new(Vec::new()),
            vm_held,
            phase_started: Mutex::new(None),
            vcpu_window: Mutex::new(VcpuWindow::default()),
        });
        Ok(e)
    }

    fn pause_requested(&self) -> bool {
        self.vm.join(PAUSE_FILE).exists()
    }

    /// Holds a step from starting while the person has it paused.
    pub fn wait_while_paused(&self) -> Result<(), String> {
        while self.pause_requested() {
            self.is_cancelled()?;
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    fn cancelled_now(&self) -> bool {
        super::ctl::cancelled() || self.vm.join(CANCEL_FILE).exists()
    }

    /// Updates what the UI shows; written to disk at most once a second unless `force`.
    pub fn status(&self, force: bool, f: impl FnOnce(&mut Status)) {
        let mut s = self.status.lock().unwrap();
        f(&mut s);
        s.updated_ms = state::now_ms();
        s.thermal = super::ctl::thermal();
        let mut last = self.status_written.lock().unwrap();
        if force || last.elapsed() >= Duration::from_millis(1000) {
            *last = Instant::now();
            state::write_status(&self.vm, &s);
        }
    }

    pub fn snapshot(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn begin_step(&self, key: &str) {
        let log_path = state::step_log(&self.vm, key);
        let _ = fs::create_dir_all(self.vm.join(state::LOG_DIR));
        *self.step_log.lock().unwrap() = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        *self.step_started.lock().unwrap() = active::ActiveInstant::now();
        let path = log_path.to_string_lossy().into_owned();
        *self.phase_started.lock().unwrap() = None;
        let phases = phases_of(key, &self.measured.lock().unwrap());
        self.status(true, |s| {
            s.phases = phases;
            s.step = key.to_string();
            s.step_started_ms = state::now_ms();
            s.state = "running".into();
            s.message.clear();
            s.hold.clear();
            s.done_bytes = 0;
            s.total_bytes = 0;
            s.bytes_per_sec = 0;
            s.error.clear();
            s.log = path.clone();
            for st in &mut s.steps {
                if st.key == key {
                    st.state = "running".into();
                }
            }
        });
    }

    pub fn end_step(&self, key: &str, state_text: &str, elapsed: Duration) {
        self.pump_mirror();
        self.mirror.lock().unwrap().clear();
        let total = {
            let mut t = self.times.lock().unwrap();
            let e = t.entry(key.to_string()).or_insert(0);
            *e += elapsed.as_secs();
            let total = *e;
            if let Ok(json) = serde_json::to_vec(&*t) {
                let _ = state::write_atomic(&self.vm.join(".create-times.json"), &json);
            }
            total
        };
        // The phase running when the step ended ran until now; only a finished step's is a
        // measurement.
        let last_phase = self.phase_started.lock().unwrap().take().map(|began| began.elapsed().as_secs());
        let running_phase = self.snapshot().phases.iter().find(|p| p.state == "running").map(|p| p.key.clone());
        // A step that finished is a measurement of this phone, for the next estimate.
        let measured = (state_text == "done").then(|| {
            let mut m = self.measured.lock().unwrap();
            let secs = kept_measurement(m.get(key).copied(), elapsed.as_secs());
            m.insert(key.to_string(), secs);
            if let (Some(phase), Some(ran)) = (&running_phase, last_phase) {
                m.insert(phase_key(key, phase), ran);
            }
            if let Ok(json) = serde_json::to_vec(&*m) {
                let file = measured_file(&self.paths);
                let _ = fs::create_dir_all(file.parent().unwrap_or(&self.paths.files));
                let _ = state::write_atomic(&file, &json);
            }
            secs
        });
        self.status(true, |s| {
            for phase in &mut s.phases {
                if phase.state == "running" {
                    phase.elapsed_secs = last_phase.unwrap_or(0);
                    phase.state = if state_text == "done" { "done".into() } else { "stopped".into() };
                }
            }
            for st in &mut s.steps {
                if st.key == key {
                    st.state = state_text.to_string();
                    st.elapsed_secs = total;
                    if let Some(secs) = measured {
                        st.est_secs = secs;
                        st.measured = true;
                    }
                }
            }
            self.set_eta(s);
        });
    }

    /// The time left, from how long each step still to run last took on this phone. With a step
    /// among them that has never finished here there is no estimate, and the note says so:
    /// numbers that were not measured would only mislead.
    fn set_eta(&self, s: &mut Status) {
        let running = self.step_started.lock().unwrap().elapsed().as_secs();
        let (mut secs, mut note) = estimate(&s.steps, &s.step, running);
        // The first boot's own phases say more than its total: what is left of them replaces the
        // step's share. Every step after the first boot is none, so nothing else is in the sum.
        if s.step == FIRST_BOOT_STEP && s.steps.last().is_some_and(|last| last.key == FIRST_BOOT_STEP) {
            let in_phase = self.phase_started.lock().unwrap().as_ref().map_or(0, |began| began.elapsed().as_secs());
            // Where a phase has never finished here, the step's own total (or its reason for
            // having none) stands.
            if let Some(left) = phases_left(&s.phases, in_phase) {
                (secs, note) = (left, String::new());
            }
        }
        s.eta_secs = secs;
        s.eta_note = note;
    }

    /// A phase of the running step began (`Progress::phase`).
    fn begin_phase(&self, key: &str) {
        let ran = {
            let mut began = self.phase_started.lock().unwrap();
            let ran = began.as_ref().map_or(0, |b| b.elapsed().as_secs());
            *began = Some(active::ActiveInstant::now());
            ran
        };
        let step = self.snapshot().step;
        let mut ended = None;
        self.status(true, |s| {
            ended = advance_phases(&mut s.phases, key, ran, state::now_ms());
            self.set_eta(s);
        });
        if let Some((phase, secs)) = ended {
            self.append_log(&format!("phase {phase} took {}", super::logview::duration_text(secs)));
            let mut m = self.measured.lock().unwrap();
            m.insert(phase_key(&step, &phase), secs);
            if let Ok(json) = serde_json::to_vec(&*m) {
                let file = measured_file(&self.paths);
                let _ = fs::create_dir_all(file.parent().unwrap_or(&self.paths.files));
                let _ = state::write_atomic(&file, &json);
            }
        }
    }

    /// Follows a helper's own log into the step's, with what the VM and the USB link say beside
    /// it: a restore's stages show as the step's progress, and a guest that panics shows why.
    pub fn mirror_log(&self, path: &Path) {
        // What was followed until now has its last lines delivered before it is replaced.
        self.pump_mirror();
        let follow = |path: &Path, prefix| Followed { mirror: Mirror::new(path), prefix };
        let logs = self.vm.join(state::LOG_DIR);
        let usb = logs.join("usb-broker.log");
        let serial = self.sup.serial_log();
        *self.followed_paths.lock().unwrap() = vec![path.to_path_buf(), serial.clone(), usb.clone()];
        *self.mirror.lock().unwrap() = vec![
            follow(path, ""),
            follow(&serial, "[guest] "),
            follow(&usb, "[usb] "),
            follow(&self.sup.threads_log(), "[vm] "),
            follow(&self.qemu_log(), "[qemu] "),
            follow(&logs.join(MUX_LOG), "[mux] "),
        ];
    }

    /// The end of QEMU's log of this boot, without its per-event trace.
    fn qemu_log_tail(&self) -> String {
        const KEEP: usize = 1500;
        let log = crate::runtime::log_tail(&self.qemu_log().to_string_lossy(), 512 * 1024);
        let lines: Vec<&str> = log.lines().filter(|l| !mirror::is_qemu_noise(l)).collect();
        let text = lines.join("\n");
        let start = text.len().saturating_sub(KEEP);
        let start = (start..=text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(start);
        text[start..].trim().to_string()
    }

    /// QEMU's own log of this boot, beside its serial log.
    fn qemu_log(&self) -> PathBuf {
        self.sup.serial_log().with_file_name(QEMU_LOG)
    }

    fn pump_mirror(&self) {
        let mut guard = self.mirror.lock().unwrap();
        for followed in guard.iter_mut() {
            for line in followed.mirror.drain() {
                if followed.prefix == "[qemu] " && mirror::is_qemu_noise(&line) {
                    continue;
                }
                if followed.prefix.is_empty() {
                    if let Some((stage, text)) = followed.mirror.advance(&line) {
                        self.status(true, |s| {
                            s.done_bytes = stage;
                            s.total_bytes = mirror::STAGE_COUNT;
                            s.bytes_per_sec = 0;
                            s.message = text.to_string();
                        });
                    }
                } else if let Some(sent) = mirror::sent_mib(&line) {
                    self.status(true, |s| s.message = format!("Sent {sent} MB to the VM"));
                }
                if let Some(percent) = mirror::asr_percent(&line) {
                    self.show_restore_progress(percent);
                }
                if let Some(on) = mirror::fast_mtree_said(&line) {
                    self.append_log(if on {
                        "Fast mtree: the guest's restore confirms it is ON (the system volume is sealed from the manifest)"
                    } else {
                        "Fast mtree: the guest's restore says it is NOT enabled; it hashes every system file now, on one CPU"
                    });
                    if on {
                        self.status(true, |s| {
                            s.done_bytes = 0;
                            s.total_bytes = 0;
                            s.bytes_per_sec = 0;
                            s.message = "Sealing the system volume (fast path)".into();
                        });
                    }
                }
                if let Some(text) = mirror::guest_stage(&line) {
                    self.status(true, |s| {
                        s.done_bytes = 0;
                        s.total_bytes = 0;
                        s.bytes_per_sec = 0;
                        s.message = text.to_string();
                    });
                }
                self.append_log(&format!("{}{line}", followed.prefix));
            }
        }
    }

    /// The bar, speed and time left of the image restore. The speed is what the VM's disk image
    /// actually grew by; the time left is the recent rate carried forward.
    fn show_restore_progress(&self, percent: u64) {
        let disk = self
            .sup
            .serial_log()
            .parent()
            .and_then(Path::parent)
            .and_then(|vm| fs::metadata(vm.join("Disk.img")).ok())
            .map(|m| m.blocks() * 512);
        let mut rate = self.restore_rate.lock().unwrap();
        if self.pause_requested() {
            rate.clear();
        }
        if let Some(message) = rate.update(percent, disk) {
            self.status(true, |s| {
                s.done_bytes = percent;
                s.total_bytes = 100;
                s.bytes_per_sec = 0;
                s.message = message;
            });
        }
    }

    pub fn append_log(&self, line: &str) {
        log(TAG, line);
        if let Some(f) = self.step_log.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn is_cancelled(&self) -> Result<(), String> {
        if self.cancelled_now() {
            Err("cancelled".into())
        } else {
            Ok(())
        }
    }

    /// Environment every helper needs (worker notes in COORDINATION-create.md).
    pub fn helper_env(&self) -> Vec<(String, String)> {
        let dir = |p: PathBuf| p.to_string_lossy().into_owned();
        let cv = self.paths.create_sockets();
        vec![
            ("LD_LIBRARY_PATH".into(), dir(self.paths.native_lib.clone())),
            ("HOME".into(), dir(self.paths.files.join("home"))),
            ("TMPDIR".into(), dir(self.paths.cache.join("tmp"))),
            // The vphone-cli release the bundle step unpacks (`resources::fetch_release`).
            ("VPHONE_RESOURCES".into(), dir(self.paths.fw_cache().join("release"))),
            ("VPHONE_VUSB_SOCKET".into(), dir(cv.join("vusbd.sock"))),
            (
                "USBMUXD_SOCKET_ADDRESS".into(),
                format!("UNIX:{}", cv.join("usbmuxd.sock").display()),
            ),
            // XNU's own output on the serial line, which the step's log follows as [guest].
            ("VPHONE_RESTORE_BOOT_ARGS_EXTRA".into(), "serial=3".into()),
        ]
    }

    pub fn exec(&self, lib: &str, args: Vec<String>, env: Vec<(String, String)>, log_path: &Path, timeout: Option<Duration>) -> Exec {
        let mut full = self.helper_env();
        full.extend(env);
        Exec {
            program: self.paths.lib(lib),
            args,
            env: full,
            cwd: self.vm.clone(),
            log: log_path.to_path_buf(),
            timeout,
            done_when: None,
        }
    }

    pub fn current_log(&self) -> PathBuf {
        PathBuf::from(self.status.lock().unwrap().log.clone())
    }

    /// Runs a CLI helper; a non-zero exit is an error naming it and its log.
    pub fn run_lib(&self, exec: Exec, what: &str) -> Result<(), String> {
        let code = exec.run(&|| self.cancelled_now())?;
        if code != 0 {
            return Err(format!("{what} failed with status {code}; see its log"));
        }
        Ok(())
    }

    /// The restore's helpers: the USB broker and usbmuxd (`tools/usbmuxd-32k.sh` and the USB/IP
    /// attach loop on a PC). Started once; restarted if they have died.
    pub fn usb_up(&self) -> Result<(), String> {
        let mut bg = self.usb.lock().unwrap();
        bg.retain_mut(|b| b.exited().is_none());
        if bg.len() == 2 {
            return Ok(());
        }
        bg.clear();
        let cv = self.paths.create_sockets();
        let _ = fs::create_dir_all(&cv);
        let _ = fs::create_dir_all(self.paths.files.join("lockdown"));
        for s in ["vusbd.sock", "usbmuxd.sock", "usbmuxd.pid"] {
            let _ = fs::remove_file(cv.join(s));
        }
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        let logdir = self.vm.join(state::LOG_DIR);
        let _ = fs::create_dir_all(&logdir);
        let broker = self.exec(
            "libvusbd.so",
            vec![
                "--chardev".into(),
                s(cv.join("vusb.sock")),
                "--listen".into(),
                s(cv.join("vusbd.sock")),
            ],
            // The phone's stages take far longer than the desktop's five minutes.
            vec![("VUSB_STAGE_TIMEOUT".into(), "3600".into()), ("VUSB_CTRL_TIMEOUT".into(), "30".into())],
            &logdir.join("usb-broker.log"),
            None,
        );
        let mux = self.exec(
            "libusbmuxd_vphone.so",
            vec![
                "-f".into(),
                "-v".into(),
                "-S".into(),
                s(cv.join("usbmuxd.sock")),
                "-P".into(),
                s(cv.join("usbmuxd.pid")),
                "-C".into(),
                s(self.paths.files.join("lockdown")),
                "-l".into(),
                s(logdir.join(MUX_LOG)),
            ],
            vec![],
            &logdir.join("usbmuxd-out.log"),
            None,
        );
        bg.push(broker.spawn()?);
        // The broker owns the chardev before anything talks through it.
        std::thread::sleep(Duration::from_millis(500));
        bg.push(mux.spawn()?);
        Ok(())
    }

    pub fn usb_down(&self) {
        self.usb.lock().unwrap().clear();
    }

    pub fn vusb_socket(&self) -> PathBuf {
        self.paths.create_sockets().join("vusb.sock")
    }

    /// Blocks while storage or thermal conditions say to wait.
    pub fn hold_until_clear(&self, floor: u64) -> Result<(), String> {
        loop {
            self.is_cancelled()?;
            let free = free_bytes(&self.paths.files);
            let thermal = super::ctl::thermal();
            let held = if free < floor {
                Some(("storage", format!("Waiting for storage: {} GB free, this step needs {} GB. Free some space to continue.", free / GIB, floor / GIB)))
            } else if thermal >= super::ctl::THERMAL_CRITICAL {
                Some(("thermal", super::ctl::thermal_text(thermal).to_string()))
            } else {
                None
            };
            match held {
                Some((hold, message)) => {
                    self.status(true, |s| {
                        s.hold = hold.into();
                        s.message = message.clone();
                    });
                    std::thread::sleep(Duration::from_secs(5));
                }
                None => {
                    self.status(true, |s| {
                        s.hold.clear();
                        s.message.clear();
                    });
                    return Ok(());
                }
            }
        }
    }

    pub fn general_floor() -> u64 {
        GENERAL_FREE_FLOOR
    }

    /// The person's pause stops the guest and every helper. While a VM runs, a critically hot
    /// phone or a nearly full disk does the same until it clears.
    pub fn spawn_pause_watch(self: &Arc<Self>) {
        self.spawn_vitals();
        let me = self.clone();
        let _ = std::thread::Builder::new().name("create-pause".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                me.pump_mirror();
                let mut paused = me.pause.lock().unwrap();
                if matches!(me.snapshot().state.as_str(), "done" | "failed" | "cancelled") {
                    if *paused {
                        super::exec::freeze(false);
                        active::hold(false);
                    }
                    return;
                }
                let alive = me.sup.alive();
                let by_person = me.pause_requested();
                let free = free_bytes(&me.paths.files);
                let hot = alive && super::ctl::thermal() >= super::ctl::THERMAL_CRITICAL;
                let low = alive && free < VM_FREE_FLOOR;
                // A cancel lets everything run again, so that what is waiting can end.
                let want = !me.cancelled_now() && (by_person || hot || low);
                if want && !*paused {
                    if alive && crate::qmp::command(&me.paths.qmp_socket(), "stop").is_err() {
                        continue;
                    }
                    super::exec::freeze(true);
                    // This process keeps running: its own waits on the guest (SSH commands, the
                    // guest's RPC) stop counting here, as the stopped helpers' do by themselves.
                    active::hold(true);
                    *paused = true;
                    me.vm_held.store(true, Ordering::SeqCst);
                    let (hold, message) = if by_person {
                        ("", "Paused".to_string())
                    } else if low {
                        ("storage", format!("Paused: only {} GB free. Free some space to continue.", free / GIB))
                    } else {
                        ("thermal", super::ctl::thermal_text(super::ctl::thermal()).to_string())
                    };
                    me.append_log(&format!("[pause] {message}"));
                    me.status(true, |s| {
                        s.hold = hold.into();
                        s.message = message.clone();
                    });
                } else if !want && *paused {
                    if alive && crate::qmp::command(&me.paths.qmp_socket(), "cont").is_err() {
                        continue;
                    }
                    active::hold(false);
                    super::exec::freeze(false);
                    *paused = false;
                    me.vm_held.store(false, Ordering::SeqCst);
                    me.append_log("[pause] resumed");
                    me.status(true, |s| {
                        s.hold.clear();
                        s.message.clear();
                    });
                }
            }
        });
    }

    /// The vitals thread: `<cv>/vm-stats.json` once a second, and the stall watchdog.
    fn spawn_vitals(self: &Arc<Self>) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("create-vitals".into()).spawn(move || {
            let mut sampler = Sampler::new();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if matches!(me.snapshot().state.as_str(), "done" | "failed" | "cancelled") {
                    sampler.finish(&me);
                    return;
                }
                sampler.tick(&me);
            }
        });
    }

    /// Fails the running boot when nothing at all has happened for too long.
    fn stop_stalled(&self, facts: String) {
        self.append_log("[vm] the VM has stalled:");
        self.append_log(&facts);
        self.sup.stop_stalled(facts);
    }

    /// A boot with the display failed; later boots of this VM go without one, and why.
    pub fn display_failed(&self, why: &str) {
        self.append_log(&format!("[display] the boot with the display failed: {why}"));
        self.append_log("[display] redoing this step without the display, and keeping it off for this VM");
        display::turn_off_after_failure(&self.vm, why);
    }

    pub fn fetch_firmware(&self, fw: &Firmware) -> Result<(), String> {
        let cache = self.paths.fw_cache();
        fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
        // The cloudOS image first, as in the desktop flow: it is small and the GPU step needs it.
        for image in [&fw.cloud, &fw.ios] {
            self.append_log(&format!("firmware: {} from {}", image.file, image.url));
            let job = download::Fetch {
                url: image.url.clone(),
                dest: cache.join(&image.file),
                expected_size: image.size,
            };
            download::fetch(&job, self)?;
        }
        Ok(())
    }

    pub fn delete_cached_firmware(&self) {
        if let Some(fw) = &self.spec.firmware {
            let cache = self.paths.fw_cache();
            for image in [&fw.cloud, &fw.ios] {
                download::remove_cached(&cache.join(&image.file));
            }
        }
    }

    pub fn vphoned(&self) -> Client {
        Client::new(&self.paths.vsock_socket())
    }
}

impl Gate for Engine {
    fn cancelled(&self) -> bool {
        self.cancelled_now()
    }
    fn blocked(&self) -> Option<String> {
        if self.pause_requested() {
            return Some("Paused".to_string());
        }
        super::ctl::set_cellular_ok(self.spec.allow_cellular || self.vm.join(CELLULAR_FILE).exists());
        super::ctl::network_block().map(str::to_string)
    }
    fn free_bytes(&self) -> u64 {
        free_bytes(&self.paths.files)
    }
    fn report(&self, done: u64, total: u64, speed: u64, message: &str) {
        let message = message.to_string();
        self.status(false, |s| {
            s.done_bytes = done;
            s.total_bytes = total;
            s.bytes_per_sec = speed;
            s.message = message;
            s.hold.clear();
            self.set_eta(s);
        });
    }
    fn hold(&self, message: &str) {
        let hold = if message.is_empty() || message == "Paused" {
            ""
        } else if message.contains("Wi-Fi") {
            "wifi"
        } else if message.contains("storage") {
            "storage"
        } else {
            "network"
        };
        let (hold, message) = (hold.to_string(), message.to_string());
        self.status(true, |s| {
            s.hold = hold;
            if !message.is_empty() {
                s.message = message;
            }
        });
    }
}

impl Progress for Engine {
    fn log(&self, line: &str) {
        self.append_log(line);
        // A new stage: what the last one reported is over.
        if let Some(text) = line.strip_prefix("=== ") {
            let text = if text == "DFU" { "Starting the VM" } else { text }.to_string();
            self.status(true, |s| {
                s.done_bytes = 0;
                s.total_bytes = 0;
                s.bytes_per_sec = 0;
                s.message = text;
            });
        }
    }
    fn progress(&self, done: u64, total: u64, message: &str) {
        let message = message.to_string();
        self.status(false, |s| {
            s.done_bytes = done;
            s.total_bytes = total;
            s.bytes_per_sec = 0;
            s.message = message;
            self.set_eta(s);
        });
    }
    fn phase(&self, key: &str, _title: &str) {
        self.begin_phase(key);
    }
    /// Every long in-process job asks this often, so a pause stops them here.
    fn cancelled(&self) -> bool {
        while self.pause_requested() && !self.cancelled_now() {
            std::thread::sleep(Duration::from_millis(250));
        }
        self.cancelled_now()
    }
}

/// One second's worth of the VM's condition, from the logs, the process's own counters and QMP.
struct Sampler {
    ticks: u32,
    watch: vitals::Watch,
    last_proc: Option<vitals::ProcStats>,
    was_alive: bool,
    /// Ticks per second of `/proc/*/stat`'s CPU times.
    hz: u64,
    vcpus: u32,
}

impl Sampler {
    fn new() -> Sampler {
        // SAFETY: sysconf only reads a system constant.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        Sampler {
            ticks: 0,
            watch: vitals::Watch::new(Instant::now()),
            last_proc: None,
            was_alive: false,
            hz,
            vcpus: super::vm::create_config().vcpus,
        }
    }

    fn base(&self, e: &Engine) -> vitals::Vitals {
        let (display, display_note) = e.sup.display_status();
        vitals::Vitals { display, display_note, updated_ms: state::now_ms(), ..Default::default() }
    }

    fn finish(&mut self, e: &Engine) {
        if self.was_alive {
            vitals::write(&vitals::stats_path(&e.paths), &self.base(e));
        }
    }

    fn output_signature(e: &Engine) -> u64 {
        let logs = e.vm.join(state::LOG_DIR);
        let mut files = e.followed_paths.lock().unwrap().clone();
        files.push(e.sup.serial_log());
        files.push(logs.join(MUX_LOG));
        files.iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum()
    }

    fn tick(&mut self, e: &Engine) {
        let alive = e.sup.alive();
        if !alive {
            self.finish(e);
            self.was_alive = false;
            self.last_proc = None;
            return;
        }
        let now = Instant::now();
        self.watch.set_now(now);
        self.watch.boot(e.sup.pid());
        self.was_alive = true;
        let mut out = self.base(e);
        out.alive = true;

        if let Ok(bytes) = fs::read(vitals::proc_path(&e.paths)) {
            if let Ok(stats) = serde_json::from_slice::<vitals::ProcStats>(&bytes) {
                if let Some(old) = &self.last_proc {
                    out.cpu_percent = vitals::cpu_percent(old, &stats, self.hz);
                }
                out.rss_mb = stats.rss_kb / 1024;
                out.disk_written_mb = if stats.has_io {
                    stats.write_bytes >> 20
                } else {
                    use std::os::unix::fs::MetadataExt;
                    // Without /proc/self/io: how much of the disk has been written at all.
                    fs::metadata(e.vm.join("Disk.img")).map_or(0, |m| (m.blocks() * 512) >> 20)
                };
                self.last_proc = Some(stats);
            }
        }

        let broker = e.vm.join(state::LOG_DIR).join("usb-broker.log");
        out.usb_stage = vitals::usb_stage(&crate::runtime::log_tail(&broker.to_string_lossy(), 8192));

        out.paused = e.vm_held.load(Ordering::SeqCst) || e.pause_requested();
        if out.paused {
            self.watch.paused();
        } else {
            self.watch.observe_output(Self::output_signature(e));
            self.ticks += 1;
            if self.ticks.is_multiple_of(vitals::PC_SAMPLE_TICKS) {
                if let Ok(vcpus) = crate::qmp::sample_vcpus(&e.paths.qmp_socket(), self.vcpus) {
                    e.vcpu_window.lock().unwrap().observe(&vcpus);
                    match self.watch.observe_pcs(vcpus) {
                        Some(vitals::Event::StoppedMoving) => e.append_log(&format!(
                            "[vm] the guest has stopped moving: {}",
                            self.watch.vcpus().iter().map(|c| format!("{}:EL{}:{:#x}", c.index, c.el, c.pc)).collect::<Vec<_>>().join(" ")
                        )),
                        Some(vitals::Event::MovingAgain) => e.append_log("[vm] the guest is moving again"),
                        None => {}
                    }
                }
            }
            if let Some(still) = self.watch.stalled_for(vitals::STALL_LIMIT) {
                let last = crate::runtime::log_tail(&e.sup.serial_log().to_string_lossy(), 1500);
                let facts = self.watch.describe_stall(still, &out.usb_stage, &last);
                e.stop_stalled(facts);
                return;
            }
        }
        vitals::fill_guest(&mut out, self.watch.vcpus(), self.watch.moving());
        vitals::write(&vitals::stats_path(&e.paths), &out);
    }
}

struct Bg(Background);

impl BackgroundHelper for Bg {
    fn finished(&mut self) -> Option<i32> {
        self.0.exited()
    }
    fn stop(self: Box<Self>) {
        drop(self.0);
    }
}

impl Host for Engine {
    fn start_vm(&self, kind: VmKind, vm: &Path, run: &Path) -> Result<(), String> {
        if matches!(kind, VmKind::Dfu | VmKind::Ramdisk) {
            self.usb_up()?;
        }
        let logs = self.vm.join(state::LOG_DIR);
        let usb = Followed { mirror: Mirror::new(&logs.join("usb-broker.log")), prefix: "[usb] " };
        let mux = Followed { mirror: Mirror::new(&logs.join(MUX_LOG)), prefix: "[mux] " };
        self.sup.start(kind, vm, run, &self.vusb_socket(), display::setting(&self.vm))?;
        *self.vcpu_window.lock().unwrap() = VcpuWindow::default();
        // From here the step's log shows the guest and the USB link, before any tool runs.
        let guest = Followed { mirror: Mirror::from_start(&self.sup.serial_log()), prefix: "[guest] " };
        let threads = Followed { mirror: Mirror::from_start(&run.join(super::vm::THREADS_LOG)), prefix: "[vm] " };
        // Started over by each boot's QEMU, which has it open by now.
        let qemu = Followed { mirror: Mirror::from_start(&run.join(QEMU_LOG)), prefix: "[qemu] " };
        *self.mirror.lock().unwrap() = vec![guest, usb, threads, qemu, mux];
        Ok(())
    }
    fn stop_vm(&self, _vm: &Path) -> Result<(), String> {
        self.sup.stop()
    }
    fn vm_alive(&self) -> bool {
        self.sup.alive()
    }
    fn serial_log(&self) -> PathBuf {
        self.sup.serial_log()
    }
    fn diagnostics(&self) -> String {
        if let Some(facts) = self.sup.stall_facts() {
            return facts;
        }
        let tail = |path: PathBuf| crate::runtime::log_tail(&path.to_string_lossy(), 1500).trim().to_string();
        let logs = self.vm.join(state::LOG_DIR);
        format!(
            "How the VM ended: {}\n\nThread log:\n{}\n\nVM output:\n{}\n\nQEMU log:\n{}\n\nUSB link:\n{}\n\nusbmuxd:\n{}",
            if self.sup.alive() { "it is still running".to_string() } else { self.sup.exit_detail() },
            tail(self.sup.threads_log()),
            tail(self.sup.serial_log()),
            self.qemu_log_tail(),
            tail(logs.join("usb-broker.log")),
            tail(logs.join(MUX_LOG))
        )
    }
    fn vcpu_note(&self) -> String {
        self.vcpu_window.lock().unwrap().take_note()
    }
    fn guest_userspace_seen(&self) -> bool {
        self.vcpu_window.lock().unwrap().userspace_seen
    }
    fn vphoned_rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.vphoned().rpc(method, params, timeout)
    }
    fn run_helper(
        &self,
        lib: &str,
        args: &[String],
        env: &[(String, String)],
        log_path: &Path,
        timeout: Option<Duration>,
    ) -> Result<i32, String> {
        self.exec(lib, args.to_vec(), env.to_vec(), log_path, timeout)
            .run(&|| self.cancelled_now())
    }
    fn spawn_helper(
        &self,
        lib: &str,
        args: &[String],
        env: &[(String, String)],
        log_path: &Path,
    ) -> Result<Box<dyn BackgroundHelper>, String> {
        self.mirror_log(log_path);
        let bg = self
            .exec(lib, args.to_vec(), env.to_vec(), log_path, None)
            .spawn()?;
        Ok(Box::new(Bg(bg)))
    }
    fn usbmux_socket(&self) -> PathBuf {
        self.paths.create_sockets().join("usbmuxd.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_estimate_is_only_made_of_measured_steps() {
        let step = |key: &str, state: &str, secs: u64, measured: bool| StepStatus {
            key: key.into(),
            state: state.into(),
            est_secs: secs,
            measured,
            ..Default::default()
        };
        // Everything left was measured: the running step's own time comes off its measurement.
        let all = [step("patch", "done", 60, true), step("restore", "running", 3000, true), step("cfw", "pending", 1800, true)];
        assert_eq!(estimate(&all, "restore", 1000), (3800, String::new()));
        assert_eq!(estimate(&all, "restore", 9000), (1800, String::new()), "a step that overruns leaves the rest");
        // One never finished here: no number at all, and the reason.
        let some = [step("restore", "running", 3000, true), step("cfw", "pending", 0, false), step("booted", "pending", 900, true)];
        assert_eq!(estimate(&some, "restore", 10), (0, "No time estimate yet: 1 of the 3 steps left has never finished on this phone".into()));
        let none = [step("restore", "running", 0, false), step("cfw", "pending", 0, false)];
        assert_eq!(estimate(&none, "restore", 10).1, "No time estimate yet: none of the 2 steps left has finished on this phone before");
        let last = [step("patch", "done", 60, true), step("booted", "running", 0, false)];
        assert_eq!(estimate(&last, "booted", 10).1, "No time estimate yet: the step left has never finished on this phone");
        assert_eq!(estimate(&[step("a", "done", 1, true)], "", 0), (0, String::new()));
    }

    #[test]
    fn the_first_boots_phases_are_timed_and_estimated_from_this_phones_own_runs() {
        let none = BTreeMap::new();
        let mut phases = phases_of(FIRST_BOOT_STEP, &none);
        assert_eq!(phases.len(), 6);
        assert!(phases_of("restore", &none).is_empty());
        assert_eq!(phases_left(&phases, 0), None, "nothing was ever measured here");
        assert_eq!(advance_phases(&mut phases, "kernel", 0, 1000), None);
        assert_eq!(advance_phases(&mut phases, "launchd", 40, 2000), Some(("kernel".into(), 40)));
        assert_eq!(advance_phases(&mut phases, "launchd", 9, 2500), None, "a phase begins once");
        assert_eq!(advance_phases(&mut phases, "migration", 60, 3000), Some(("launchd".into(), 60)));
        assert_eq!((phases[0].state.as_str(), phases[0].elapsed_secs), ("done", 40));
        assert_eq!((phases[2].state.as_str(), phases[2].started_ms), ("running", 3000));
        // A guest never seen in user code before its agent answered: launchd is passed over.
        let mut direct = phases_of(FIRST_BOOT_STEP, &none);
        advance_phases(&mut direct, "kernel", 0, 1);
        assert_eq!(advance_phases(&mut direct, "migration", 90, 2), Some(("kernel".into(), 90)));
        assert_eq!(direct[1].state, "skipped");

        // With every phase measured: what is left, the running one's own time taken off.
        let measured: BTreeMap<String, u64> =
            [("kernel", 40), ("launchd", 60), ("migration", 3000), ("setup", 20), ("unlock", 30), ("finish", 400)]
                .into_iter()
                .map(|(k, v)| (phase_key(FIRST_BOOT_STEP, k), v))
                .collect();
        let mut phases = phases_of(FIRST_BOOT_STEP, &measured);
        assert_eq!(phases_left(&phases, 0), Some(3550));
        advance_phases(&mut phases, "kernel", 0, 1);
        advance_phases(&mut phases, "launchd", 35, 2);
        advance_phases(&mut phases, "migration", 70, 3);
        assert_eq!(phases_left(&phases, 1000), Some(2000 + 20 + 30 + 400));
        assert_eq!(phases_left(&phases, 5000), Some(450), "a phase that overruns leaves the rest");
        // One phase never finished here: no number.
        let mut some = measured.clone();
        some.remove(&phase_key(FIRST_BOOT_STEP, "finish"));
        assert_eq!(phases_left(&phases_of(FIRST_BOOT_STEP, &some), 0), None);
    }

    #[test]
    fn vcpu_samples_are_summed_into_one_note() {
        use crate::qmp::Vcpu;
        let mut window = VcpuWindow::default();
        assert_eq!(window.take_note(), "");
        window.observe(&[Vcpu { index: 0, pc: 0xfffffe0000001000, el: 1 }, Vcpu { index: 1, pc: 0xfffffe0000002000, el: 1 }]);
        assert!(!window.userspace_seen);
        window.observe(&[Vcpu { index: 0, pc: 0xfffffe0000001000, el: 1 }, Vcpu { index: 1, pc: 0x100004000, el: 0 }]);
        assert!(window.userspace_seen);
        assert_eq!(window.take_note(), "vCPU samples 4: user 25% kernel 75% (most at 0xfffffe0000001000: 50%)");
        assert_eq!(window.take_note(), "", "a new window");
        assert!(window.userspace_seen, "which still knows the boot reached user code");
    }

    #[test]
    fn a_step_with_nothing_to_do_does_not_replace_a_real_measurement() {
        assert_eq!(kept_measurement(None, 0), 0);
        assert_eq!(kept_measurement(Some(3240), 0), 3240, "the driver was cached this time");
        assert_eq!(kept_measurement(Some(3240), 2), 3240);
        assert_eq!(kept_measurement(Some(3240), 2700), 2700, "a real run replaces the last one");
        assert_eq!(kept_measurement(Some(1), 2), 2);
    }

    struct NoVm;
    impl VmProcess for NoVm {
        fn start(&self, _: &Path, _: &Path) -> Result<(), String> { Ok(()) }
        fn release(&self) {}
        fn exit_reason(&self) -> String { String::new() }
    }

    fn engine(name: &str) -> Arc<Engine> {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/create-engine").join(name);
        let _ = fs::remove_dir_all(&d);
        let paths = Paths { files: d.join("f"), cache: d.join("c"), external: d.join("e"), native_lib: d.join("l") };
        let vm = paths.vms_internal().join("vm");
        fs::create_dir_all(vm.join(state::LOG_DIR)).unwrap();
        fs::create_dir_all(paths.create_sockets()).unwrap();
        state::write_spec(&vm, &Spec { name: "vm".into(), ..Default::default() }).unwrap();
        Engine::new(paths, "vm", Arc::new(NoVm)).unwrap()
    }

    fn append(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn qemu_and_usbmuxd_logs_reach_the_step_log_and_the_diagnostics() {
        let e = engine("follow");
        e.begin_step("restore");
        let tool = e.vm.join("tool.log");
        let qemu = e.qemu_log();
        let mux = e.vm.join(state::LOG_DIR).join(MUX_LOG);
        append(&qemu, "earlier boot\n");
        append(&mux, "earlier line\n");
        e.mirror_log(&tool);
        append(&qemu, "Invalid read at addr 0x10\n");
        append(&mux, "Connected to v2.0 client\n");
        e.pump_mirror();
        let log = fs::read_to_string(state::step_log(&e.vm, "restore")).unwrap();
        assert!(log.contains("[qemu] Invalid read at addr 0x10"), "{log}");
        assert!(log.contains("[mux] Connected to v2.0 client"), "{log}");
        assert!(!log.contains("earlier"), "{log}");
        let facts = e.diagnostics();
        assert!(facts.contains("QEMU log:\nearlier boot\nInvalid read at addr 0x10"), "{facts}");
        assert!(facts.contains("usbmuxd:\nearlier line\nConnected to v2.0 client"), "{facts}");
    }
}
