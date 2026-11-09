//! One running creation: its files, its status document, its helpers and its VM.

use super::compat::Firmware;
use super::download::{self, Gate};
use super::exec::{Background, Exec};
use super::state::{self, Spec, Status, StepStatus};
use super::steps::STEPS;
use super::vm::{Supervisor, VmKind, VmProcess};
use crate::paths::Paths;
use crate::platform::log;
use crate::vphoned::Client;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vphone_create_steps::host::{BackgroundHelper, Host};
use vphone_create_steps::progress::Progress;

const TAG: &str = "vphone-create";
const GIB: u64 = 1 << 30;
pub const CANCEL_FILE: &str = ".create-cancel";
const CELLULAR_FILE: &str = ".create-cellular-ok";
/// How little free space a step may start with: 25 GiB for the restore (it writes the disk),
/// a few GiB elsewhere. Below these the creation waits for space, it does not fail.
pub const RESTORE_FREE_FLOOR: u64 = 25 * GIB;
const GENERAL_FREE_FLOOR: u64 = 3 * GIB;
const VM_FREE_FLOOR: u64 = 2 * GIB;

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
    step_started: Mutex<Instant>,
    times: Mutex<BTreeMap<String, u64>>,
    step_log: Mutex<Option<File>>,
    usb: Mutex<Vec<Background>>,
    pause: Mutex<bool>,
}

impl Engine {
    pub fn new(paths: Paths, name: &str, process: Arc<dyn VmProcess>) -> Result<Arc<Engine>, String> {
        let vm = paths.vms_internal().join(name);
        let spec = state::read_spec(&vm).ok_or_else(|| format!("{name} has no plan to resume"))?;
        let cancel_vm = vm.clone();
        let cancelled = Arc::new(move || super::ctl::cancelled() || cancel_vm.join(CANCEL_FILE).exists());
        let sup = Supervisor::new(paths.clone(), process, cancelled);
        let times = fs::read(vm.join(".create-times.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let steps = STEPS
            .iter()
            .map(|d| StepStatus {
                key: d.key.into(),
                title: d.title.into(),
                state: if state::done(&vm, d.key) { "done".into() } else { "pending".into() },
                est_secs: d.est_secs,
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
            step_started: Mutex::new(Instant::now()),
            times: Mutex::new(times),
            step_log: Mutex::new(None),
            usb: Mutex::new(Vec::new()),
            pause: Mutex::new(false),
        });
        Ok(e)
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
        *self.step_started.lock().unwrap() = Instant::now();
        let path = log_path.to_string_lossy().into_owned();
        self.status(true, |s| {
            s.step = key.to_string();
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
        self.status(true, |s| {
            for st in &mut s.steps {
                if st.key == key {
                    st.state = state_text.to_string();
                    st.elapsed_secs = total;
                }
            }
            s.eta_secs = self.eta(s);
        });
    }

    /// Remaining seconds. Steps yet to run use their estimate, scaled by how the emulated
    /// steps have compared with theirs so far; a step in progress subtracts its elapsed time.
    fn eta(&self, s: &Status) -> u64 {
        let times = self.times.lock().unwrap();
        let mut ratio = 1.0f64;
        let (mut ran, mut est) = (0u64, 0u64);
        for def in STEPS.iter().filter(|d| d.emulated) {
            if let Some(t) = times.get(def.key) {
                if state::done(&self.vm, def.key) && *t > 0 {
                    ran += *t;
                    est += def.est_secs;
                }
            }
        }
        if est > 0 {
            ratio = (ran as f64 / est as f64).clamp(0.3, 4.0);
        }
        let mut left = 0f64;
        for (def, st) in STEPS.iter().zip(&s.steps) {
            if st.state == "done" {
                continue;
            }
            let mut this = def.est_secs as f64;
            if def.emulated {
                this *= ratio;
            }
            if st.key == s.step {
                this -= self.step_started.lock().unwrap().elapsed().as_secs_f64();
            }
            left += this.max(0.0);
        }
        left as u64
    }

    pub fn tick_eta(&self) {
        self.status(false, |s| s.eta_secs = self.eta(s));
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
                s(logdir.join("usbmuxd.log")),
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

    /// While a VM runs, a critically hot phone or a nearly full disk pauses the guest over QMP
    /// (a guest writing into a full disk would fail) and resumes it when cleared.
    pub fn spawn_pause_watch(self: &Arc<Self>) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("create-pause".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let alive = me.sup.alive();
                let mut paused = me.pause.lock().unwrap();
                if !alive {
                    *paused = false;
                    if matches!(me.snapshot().state.as_str(), "done" | "failed" | "cancelled") {
                        return;
                    }
                    continue;
                }
                let free = free_bytes(&me.paths.files);
                let hot = super::ctl::thermal() >= super::ctl::THERMAL_CRITICAL;
                let low = free < VM_FREE_FLOOR;
                let want = hot || low;
                if want && !*paused {
                    if crate::qmp::command(&me.paths.qmp_socket(), "stop").is_ok() {
                        *paused = true;
                        let (hold, message) = if low {
                            ("storage", format!("Paused: only {} GB free. Free some space to continue.", free / GIB))
                        } else {
                            ("thermal", super::ctl::thermal_text(super::ctl::thermal()).to_string())
                        };
                        me.append_log(&format!("[pause] {message}"));
                        me.status(true, |s| {
                            s.hold = hold.into();
                            s.message = message.clone();
                        });
                    }
                } else if !want && *paused {
                    if crate::qmp::command(&me.paths.qmp_socket(), "cont").is_ok() {
                        *paused = false;
                        me.append_log("[pause] resumed");
                        me.status(true, |s| {
                            s.hold.clear();
                            s.message.clear();
                        });
                    }
                }
            }
        });
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
            s.eta_secs = self.eta(s);
        });
    }
    fn hold(&self, message: &str) {
        let hold = if message.is_empty() {
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
    }
    fn progress(&self, done: u64, total: u64, message: &str) {
        let message = message.to_string();
        self.status(false, |s| {
            s.done_bytes = done;
            s.total_bytes = total;
            s.bytes_per_sec = 0;
            s.message = message;
            s.eta_secs = self.eta(s);
        });
    }
    fn cancelled(&self) -> bool {
        self.cancelled_now()
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
        self.sup.start(kind, vm, run, &self.vusb_socket())
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
        let bg = self
            .exec(lib, args.to_vec(), env.to_vec(), log_path, None)
            .spawn()?;
        Ok(Box::new(Bg(bg)))
    }
    fn usbmux_socket(&self) -> PathBuf {
        self.paths.create_sockets().join("usbmuxd.sock")
    }
}

