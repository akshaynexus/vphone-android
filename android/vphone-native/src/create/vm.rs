//! The VMs a creation boots: the DFU restore, the SSH ramdisk and the first boot.
//!
//! QEMU runs once per process, and creation needs several boots, so each runs in its own app
//! process: the `:createvm` Service, which reads a spec file and hands it to [`run_from_spec`].
//! [`Supervisor`] (in the `:create` process, which holds the foreground service) starts that
//! process, watches it through its pid, and stops it over QMP.

use crate::bundle::Bundle;
use crate::config::VmConfig;
use crate::launcher;
use crate::meminfo::MemInfo;
use crate::paths::Paths;
use crate::platform::log;
use crate::{qmp, runtime};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use vphone_create_steps::host::VmKind;

mod diag;
pub mod display;
pub mod vitals;

const TAG: &str = "vphone-create";
const QEMU_STACK: usize = 64 << 20;
/// What the VM's threads did, beside its serial log; followed into the step's log.
pub const THREADS_LOG: &str = "threads.log";
/// The latest per-thread list, for the VM activity screen.
pub const THREADS_NOW: &str = "vm-threads.txt";
const EXECUTABLE: &str = "libvphone_vm.so";
/// The restore runs a full iOS restoreOS; 4 GiB (a 12 GB phone's normal default) is not enough.
const MIN_CREATE_RAM_MIB: u32 = 6144;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RunSpec {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdout: String,
    pub stderr: String,
    pub pid_file: String,
    pub exit_file: String,
    pub gfx: bool,
    pub threads_log: String,
    /// The app's directories: files, cache, external, native libraries.
    pub paths: [String; 4],
    /// Written, with the reason, when this boot's display could not start.
    pub display_failed: String,
    /// The images whose guest flushes QEMU ignores in this boot: synced once it has ended.
    pub sync_on_exit: Vec<String>,
    /// How many vCPU threads to ask the system's performance hints for; 0 asks for none.
    pub hint_vcpus: u32,
}

fn spec_path(paths: &Paths) -> PathBuf {
    paths.create_sockets().join("vm-spec.json")
}

pub fn pid_path(paths: &Paths) -> PathBuf {
    paths.create_sockets().join("qemu.pid")
}

fn opt(p: &str) -> String {
    p.replace(',', ",,")
}

/// The VM's settings for a creation boot: as the normal defaults, but restore needs more RAM.
pub fn create_config() -> VmConfig {
    let mem = MemInfo::read();
    let mut cfg = VmConfig::for_phone(mem.total_mib, crate::config::online_cpus());
    cfg.ram_mib = cfg
        .ram_mib
        .max(MIN_CREATE_RAM_MIB)
        .min((mem.total_mib.saturating_sub(3072) as u32).max(MIN_CREATE_RAM_MIB));
    // Asked for in the VM's own process; whether the system grants them is logged there.
    cfg.performance_hints = true;
    cfg
}

/// Ports `tools/dfu-up.sh`: the normal arguments, the virtual USB chardev and logs. With `display`
/// the guest's GPU is attached, as `GFX=1 WINDOW=1` does: guest RAM becomes shared memfd.
pub fn dfu_spec(
    b: &Bundle,
    cfg: &VmConfig,
    paths: &Paths,
    run: &Path,
    vusb: &Path,
    display: bool,
) -> RunSpec {
    let mem = cfg.ram_arg();
    let file = |name: &str| opt(&b.path(name));
    let mut machine = format!("vresearch101,ecid={},sep-rom={}", b.ecid, file(&b.seprom));
    if let Some(serial) = &b.serial {
        machine += &format!(",serial={serial}");
    }
    let res = format!("{}x{}", b.res.0, b.res.1);
    if display {
        machine += &launcher::gfx_machine(&res);
    }
    let (nvram, disk) = (file("nvram.bin"), file("Disk.img"));
    let mut args: Vec<String> = ["qemu-system-aarch64", "-name", "vphone,debug-threads=on", "-M", &machine, "-accel"]
        .into_iter()
        .map(String::from)
        .collect();
    let mut push = |parts: &[&str]| args.extend(parts.iter().map(|s| s.to_string()));
    push(&[
        &format!("tcg,tb-size={}", cfg.tb_mib),
        "-smp",
        &format!("{},maxcpus={}", cfg.vcpus, cfg.vcpus + 1),
        "-m",
        &mem,
        "-bios",
        &b.path(&b.rom),
    ]);
    for drive in [
        format!("file={nvram},if=pflash,format=raw,offset=16384,file.locking=off"),
        format!("file={disk},if=pflash,format=raw,file.locking=off"),
        format!("file={disk},if=none,id=root-pci,format=raw,file.locking=off"),
        format!("file={nvram},if=none,id=aux-pci,format=raw,offset=16384,file.locking=off"),
    ] {
        push(&["-drive", &drive]);
    }
    push(&["-serial", "mon:stdio"]);
    if b.has_sep_storage {
        let sep = file("SEPStorage");
        push(&[
            "-drive",
            &format!("file={sep},if=none,id=sep-storage,format=raw,file.locking=off"),
        ]);
    }
    // The virtual network card's boot ROM is not in the app, and iOS does not use it.
    push(&["-global", "virtio-net-pci.romfile="]);
    if display {
        let gfx = launcher::gfx_args(&res, &mem);
        push(&gfx.iter().map(String::as_str).collect::<Vec<_>>());
    }
    push(&["-display", "none"]);
    let qmp = opt(&paths.qmp_socket().to_string_lossy());
    push(&["-qmp", &format!("unix:{qmp},server=on,wait=off")]);
    let vusb = opt(&vusb.to_string_lossy());
    push(&[
        "-chardev",
        &format!("socket,id=vusb,path={vusb},server=on,wait=off"),
    ]);
    push(&["-global", "vresearch-vusb.chardev=vusb"]);
    let log = run.join("qemu.log").to_string_lossy().into_owned();
    push(&["-D", &log, "-d", "guest_errors"]);

    let dir = |p: PathBuf| p.to_string_lossy().into_owned();
    let mut env = vec![
        ("VPHONE_TARGET_PAGE_BITS".to_string(), "14".to_string()),
        ("VPHONE_TLB_RANGE_PAGES".to_string(), "1".to_string()),
        ("VPHONE_XPAGE_LINKS".to_string(), "1".to_string()),
        ("VPHONE_RET_PREFETCH".to_string(), "2".to_string()),
        ("QEMU_TLB_UNSYNCED".to_string(), "2".to_string()),
        ("VPHONE_XHCI_NONREMOVABLE".to_string(), "0x1fe".to_string()),
        ("TMPDIR".to_string(), dir(paths.cache.join("tmp"))),
        ("HOME".to_string(), dir(paths.files.join("home"))),
    ];
    if display {
        env.extend(launcher::gfx_env(paths).into_iter().map(|(k, v)| (k.to_string(), v)));
    }
    let mut spec = finish_spec(args, env, run, paths, display);
    spec.hint_vcpus = if cfg.performance_hints { cfg.vcpus } else { 0 };
    spec
}

/// The first boot: the normal launcher's arguments. The GPU stays attached either way; without
/// `display` it draws nowhere (QEMU's `REIMS_VGPU_WINDOW=0`).
pub fn first_boot_spec(
    b: &Bundle,
    cfg: &VmConfig,
    paths: &Paths,
    run: &Path,
    display: bool,
) -> RunSpec {
    let launch = launcher::build(b, cfg, paths);
    let env: Vec<(String, String)> = launch
        .env
        .into_iter()
        .map(|(k, v)| {
            let v = if k == "REIMS_VGPU_WINDOW" && !display { "0".to_string() } else { v };
            (k.to_string(), v)
        })
        .collect();
    let mut spec = finish_spec(launch.args, env, run, paths, true);
    spec.hint_vcpus = if cfg.performance_hints { cfg.vcpus } else { 0 };
    spec
}

/// The file a `-drive` value opens, undoing [`opt`]'s comma doubling.
fn drive_file(drive: &str) -> Option<String> {
    let mut rest = drive.strip_prefix("file=")?.chars().peekable();
    let mut file = String::new();
    while let Some(c) = rest.next() {
        if c == ',' && rest.next_if_eq(&',').is_none() {
            break;
        }
        file.push(c);
    }
    Some(file)
}

/// The restore's DFU boot, with its drives `cache=unsafe`: each flush costs the guest a host fsync,
/// and the step is recorded only after QEMU exits and [`sync_images`] ran, so it can be redone.
/// The ramdisk and first boots keep the guest's flushes, as their steps record progress mid-boot.
fn with_unsafe_cache(mut spec: RunSpec) -> RunSpec {
    spec.sync_on_exit = use_unsafe_cache(&mut spec.args);
    spec
}

/// Appends `cache=unsafe` to each `-drive`. Returns their files.
fn use_unsafe_cache(args: &mut [String]) -> Vec<String> {
    let mut files = Vec::new();
    for at in 1..args.len() {
        if args[at - 1] != "-drive" {
            continue;
        }
        args[at].push_str(",cache=unsafe");
        if let Some(file) = drive_file(&args[at]) {
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    files
}

/// `cache=unsafe` left these only in the page cache: a phone restart would lose them, after the
/// step that wrote them was recorded. Gone files have nothing left to keep.
fn sync_images(files: &[PathBuf]) -> Result<(), String> {
    for file in files {
        let image = match fs::OpenOptions::new().write(true).open(file) {
            Ok(image) => image,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("cannot open {} to sync it: {e}", file.display())),
        };
        let started = Instant::now();
        image.sync_all().map_err(|e| format!("cannot sync {}: {e}", file.display()))?;
        log(TAG, &format!("synced {} in {:?}", file.display(), started.elapsed()));
    }
    Ok(())
}

/// Every caller is a setup boot: normal boots go through `launcher::build` alone.
fn finish_spec(
    args: Vec<String>,
    env: Vec<(String, String)>,
    run: &Path,
    paths: &Paths,
    gfx: bool,
) -> RunSpec {
    let at = |n: &str| run.join(n).to_string_lossy().into_owned();
    RunSpec {
        args,
        env,
        stdout: at("serial.log"),
        stderr: at("stderr.log"),
        pid_file: pid_path(paths).to_string_lossy().into_owned(),
        exit_file: at("qemu.exit"),
        gfx,
        threads_log: at(THREADS_LOG),
        paths: [&paths.files, &paths.cache, &paths.external, &paths.native_lib]
            .map(|p| p.to_string_lossy().into_owned()),
        display_failed: at("display-failed"),
        sync_on_exit: Vec::new(),
        hint_vcpus: 0,
    }
}

/// The VM process's body, in the `libvphone_vm.so` executable: loads QEMU and runs the boot's
/// spec. QEMU ends the process itself.
pub fn run_spec_file(file: &Path) -> Result<(), String> {
    let spec: RunSpec = serde_json::from_slice(&fs::read(file).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let paths = Paths {
        files: spec.paths[0].clone().into(),
        cache: spec.paths[1].clone().into(),
        external: spec.paths[2].clone().into(),
        native_lib: spec.paths[3].clone().into(),
    };
    for d in launcher::runtime_dirs(&paths) {
        let _ = fs::create_dir_all(d);
    }
    let write_err = |message: &str| {
        let _ = fs::write(&spec.stderr, format!("{message}\n"));
    };
    let qemu = runtime::load_qemu(&paths).inspect_err(|e| write_err(e))?;
    if spec.gfx {
        let gipa = runtime::load_vulkan(&paths).inspect_err(|e| {
            write_err(e);
            let _ = fs::write(&spec.display_failed, e);
        })?;
        // SAFETY: gipa is vkGetInstanceProcAddr of the loader just opened.
        unsafe { (qemu.set_gipa)(gipa) };
    }
    vitals::spawn_self_stats(vitals::proc_path(&paths));
    diag::spawn(spec.threads_log.clone().into(), paths.create_sockets().join(THREADS_NOW), paths.files.clone());

    let open = |p: &str| fs::File::create(p).map_err(|e| format!("cannot create {p}: {e}"));
    let (out, err) = (open(&spec.stdout)?, open(&spec.stderr)?);
    // SAFETY: stdout/stderr are replaced with files this process just opened.
    unsafe {
        libc::dup2(out.as_raw_fd(), 1);
        libc::dup2(err.as_raw_fd(), 2);
    }
    for (k, v) in &spec.env {
        std::env::set_var(k, v);
    }
    // Written last: past here, this process dying is the VM's.
    fs::write(&spec.pid_file, std::process::id().to_string()).map_err(|e| e.to_string())?;
    log(TAG, &format!("starting QEMU: {}", spec.args.join(" ")));
    if spec.hint_vcpus > 0 {
        // One line, in the log the step's own follows as [vm]: whether the hints took effect.
        let threads_log = spec.threads_log.clone();
        crate::adpf::start(spec.hint_vcpus, || false, move |line| {
            log(TAG, line);
            if let Ok(mut out) = fs::OpenOptions::new().append(true).open(&threads_log) {
                use std::io::Write;
                let _ = writeln!(out, "{line}");
            }
        });
    } else {
        log(TAG, "performance hints: not asked for in this boot");
    }
    let qemu_thread = std::thread::Builder::new()
        .name("qemu-main".into())
        .stack_size(QEMU_STACK)
        .spawn(move || {
            let code = runtime::run_qemu(qemu, spec.args);
            let _ = fs::write(&spec.exit_file, code.to_string());
            // QEMU's threads are still alive: exit()'s atexit handlers would run under them.
            // SAFETY: fflush(NULL) writes out every C stream, and _exit does not return.
            unsafe {
                libc::fflush(std::ptr::null_mut());
                libc::_exit(code)
            }
        })
        .map_err(|e| e.to_string())?;
    let _ = qemu_thread.join();
    Ok(())
}

/// The entry point of the `libvphone_vm.so` executable.
#[no_mangle]
pub extern "C" fn vphone_vm_main(spec: *const std::os::raw::c_char) -> std::os::raw::c_int {
    // SAFETY: the executable passes a valid C string.
    let spec = unsafe { std::ffi::CStr::from_ptr(spec) }.to_string_lossy().into_owned();
    match run_spec_file(Path::new(&spec)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("VM process: {e}");
            1
        }
    }
}

/// What Android does for the Supervisor: a process per VM, and why one died.
pub trait VmProcess: Send + Sync {
    /// Starts the `:createvm` service. The VM's spec file is ready.
    fn start(&self, spec: &Path, run: &Path) -> Result<(), String>;
    /// Releases it, after the process has ended.
    fn release(&self);
    /// Why Android says the last VM process ended, if it knows.
    fn exit_reason(&self) -> String;
}

#[derive(Debug, Default)]
struct Current {
    pid: i32,
    run: PathBuf,
    /// [`RunSpec::sync_on_exit`] of this boot.
    sync: Vec<PathBuf>,
}

pub struct Supervisor {
    paths: Paths,
    process: Arc<dyn VmProcess>,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The person's pause: a wait inside a boot does not time out while it holds.
    paused: Arc<dyn Fn() -> bool + Send + Sync>,
    current: Mutex<Option<Current>>,
    /// The current boot has a display attached.
    display_attached: AtomicBool,
    /// `stop` is ending the VM, so its going away is not a death.
    stopping: AtomicBool,
    /// Why the current boot failed, for [`Supervisor::take_display_failure`].
    boot_failure: Mutex<Option<String>>,
    /// The watchdog's facts, when it is what stopped the VM.
    stall: Mutex<Option<String>>,
    /// What the vitals file says of the display.
    display_status: Mutex<(String, String)>,
}

fn alive(pid: i32) -> bool {
    let Ok(text) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    let state = text
        .lines()
        .find_map(|l| l.strip_prefix("State:"))
        .map(str::trim)
        .and_then(|s| s.chars().next())
        .unwrap_or('X');
    if state == 'Z' || state == 'X' {
        return false;
    }
    is_our_vm_process(pid)
}

#[cfg(target_os = "android")]
fn is_our_vm_process(pid: i32) -> bool {
    // The pid of a dead VM process may be reused; only ours is ever signalled.
    fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|c| String::from_utf8_lossy(&c).contains(EXECUTABLE))
}

#[cfg(not(target_os = "android"))]
fn is_our_vm_process(_pid: i32) -> bool {
    true
}

/// The VM as an ordinary child process of the creation service.
pub struct ExecVm {
    native_lib: PathBuf,
    child: Mutex<Option<std::process::Child>>,
    launch_log: PathBuf,
}

impl ExecVm {
    pub fn new(paths: &Paths) -> ExecVm {
        ExecVm {
            native_lib: paths.native_lib.clone(),
            child: Mutex::new(None),
            launch_log: paths.create_sockets().join("vm-launch.log"),
        }
    }
}

impl VmProcess for ExecVm {
    fn start(&self, spec: &Path, _run: &Path) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let log = fs::File::create(&self.launch_log).map_err(|e| e.to_string())?;
        let mut command = Command::new(self.native_lib.join(EXECUTABLE));
        command
            .arg(spec)
            .env("LD_LIBRARY_PATH", &self.native_lib)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        // SAFETY: setsid is async-signal-safe and allowed between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| format!("cannot start the VM process: {e}"))?;
        *self.child.lock().unwrap() = Some(child);
        Ok(())
    }

    fn release(&self) {
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.try_wait();
        }
    }

    fn exit_reason(&self) -> String {
        use std::os::unix::process::ExitStatusExt;
        let mut guard = self.child.lock().unwrap();
        let Some(child) = guard.as_mut() else {
            return String::new();
        };
        let status = match child.try_wait() {
            Ok(Some(status)) => status,
            _ => return String::new(),
        };
        let launch = fs::read_to_string(&self.launch_log).unwrap_or_default();
        let launch = launch.trim();
        let what = match (status.code(), status.signal()) {
            (Some(code), _) => format!("QEMU exited with status {code}."),
            (_, Some(signal)) => format!("QEMU was killed by {}.", signal_name(signal)),
            _ => "QEMU ended.".to_string(),
        };
        if launch.is_empty() { what } else { format!("{what}\n{launch}") }
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGSEGV => "SIGSEGV (invalid memory access)".into(),
        libc::SIGBUS => "SIGBUS (bad memory access)".into(),
        libc::SIGILL => "SIGILL (illegal instruction)".into(),
        libc::SIGABRT => "SIGABRT (aborted)".into(),
        libc::SIGKILL => "SIGKILL (Android or the app ended it)".into(),
        libc::SIGTERM => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}

pub fn read_pid(paths: &Paths) -> Option<i32> {
    fs::read_to_string(pid_path(paths)).ok()?.trim().parse().ok()
}

impl Supervisor {
    pub fn new(
        paths: Paths,
        process: Arc<dyn VmProcess>,
        cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
        paused: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Supervisor {
            paths,
            process,
            cancelled,
            paused,
            current: Mutex::new(None),
            display_attached: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            boot_failure: Mutex::new(None),
            stall: Mutex::new(None),
            display_status: Mutex::new(("no-gpu".into(), String::new())),
        }
    }

    pub fn alive(&self) -> bool {
        let Some(pid) = self.pid() else {
            return false;
        };
        let up = alive(pid);
        if !up {
            self.note_death();
        }
        up
    }

    /// A boot we did not stop has ended.
    fn note_death(&self) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let run = self.current.lock().unwrap().as_ref().map(|c| c.run.clone());
        if let Some(run) = run {
            let detail = self.died(&run);
            self.boot_failure.lock().unwrap().get_or_insert(detail);
        }
    }

    /// Why the current boot failed, once, if it had the display attached: the caller redoes the
    /// boot headless. Clears the record either way.
    pub fn take_display_failure(&self) -> Option<String> {
        let reason = self.boot_failure.lock().unwrap().take();
        if self.display_attached.swap(false, Ordering::SeqCst) {
            reason
        } else {
            None
        }
    }

    pub fn display_status(&self) -> (String, String) {
        self.display_status.lock().unwrap().clone()
    }

    /// The watchdog's last resort: stops the VM and makes these facts its failure.
    pub fn stop_stalled(&self, facts: String) {
        *self.stall.lock().unwrap() = Some(facts.clone());
        self.boot_failure.lock().unwrap().get_or_insert(facts);
        let _ = self.stop();
    }

    pub fn stall_facts(&self) -> Option<String> {
        self.stall.lock().unwrap().clone()
    }

    pub fn pid(&self) -> Option<i32> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.pid)
            .or_else(|| read_pid(&self.paths))
    }

    pub fn threads_log(&self) -> PathBuf {
        self.serial_log().with_file_name(THREADS_LOG)
    }

    pub fn serial_log(&self) -> PathBuf {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map_or_else(|| self.paths.create_sockets().join("none"), |c| c.run.join("serial.log"))
    }

    /// `tools/dfu-stop.sh`: this creation's VM only, over QMP, then signals.
    pub fn stop(&self) -> Result<(), String> {
        self.stopping.store(true, Ordering::SeqCst);
        let result = self.stop_inner();
        self.stopping.store(false, Ordering::SeqCst);
        result
    }

    /// The VM process has ended, by its own exit or ours. The record of the boot is cleared
    /// after the sync, so a second `stop` meanwhile waits for the images too.
    fn ended(&self) -> Result<(), String> {
        let sync = self.current.lock().unwrap().as_ref().map(|c| c.sync.clone()).unwrap_or_default();
        let synced = sync_images(&sync);
        self.process.release();
        let _ = fs::remove_file(pid_path(&self.paths));
        *self.current.lock().unwrap() = None;
        synced
    }

    fn stop_inner(&self) -> Result<(), String> {
        let Some(pid) = self.pid().filter(|p| alive(*p)) else {
            return self.ended();
        };
        let _ = qmp::quit(&self.paths.qmp_socket());
        for (signal, wait) in [(0, 10), (libc::SIGTERM, 10), (libc::SIGKILL, 5)] {
            if signal != 0 {
                // SAFETY: pid is our :createvm process, checked by `alive`.
                unsafe { libc::kill(pid, signal) };
            }
            let end = Instant::now() + Duration::from_secs(wait);
            while Instant::now() < end && alive(pid) {
                std::thread::sleep(Duration::from_millis(250));
            }
            if !alive(pid) {
                break;
            }
        }
        if alive(pid) {
            return Err(format!("the VM process {pid} did not stop"));
        }
        self.ended()
    }

    fn wait(&self, what: &str, limit: Duration, ready: impl Fn() -> bool) -> Result<(), String> {
        const TICK: Duration = Duration::from_millis(500);
        let mut end = Instant::now() + limit;
        while Instant::now() < end {
            if (self.cancelled)() {
                return Err("cancelled".into());
            }
            if ready() {
                return Ok(());
            }
            // A paused VM makes no progress: the time does not count.
            if (self.paused)() {
                end += TICK;
            }
            std::thread::sleep(TICK);
        }
        Err(format!("timed out waiting for {what}"))
    }

    fn died(&self, run: &Path) -> String {
        if let Some(facts) = self.stall.lock().unwrap().as_ref() {
            return facts.clone();
        }
        // A GPU boot's stderr is mostly the translator's notes about how each shader differs.
        let tail = crate::runtime::log_tail(&run.join("stderr.log").to_string_lossy(), 16384);
        let tail: Vec<&str> = tail.lines().filter(|l| !l.contains("[metal2vulkan-")).collect();
        let tail = tail[tail.len().saturating_sub(20)..].join("\n");
        let tail = tail.trim();
        let reason = self.process.exit_reason();
        format!(
            "{}{}",
            if reason.is_empty() { "QEMU exited." } else { reason.as_str() },
            if tail.is_empty() {
                String::new()
            } else {
                format!("\nIts last output:\n{tail}")
            }
        )
    }

    /// This boot failed: the reason is kept for [`Supervisor::take_display_failure`].
    fn fail_boot(&self, detail: String) -> String {
        self.boot_failure.lock().unwrap().get_or_insert_with(|| detail.clone());
        detail
    }

    /// `tools/dfu-up.sh` / the first boot's start: blocks until the VM's device is up.
    pub fn start(
        &self,
        kind: VmKind,
        vm: &Path,
        run: &Path,
        vusb: &Path,
        setting: display::Setting,
    ) -> Result<(), String> {
        fs::create_dir_all(run).map_err(|e| e.to_string())?;
        fs::create_dir_all(self.paths.create_sockets()).map_err(|e| e.to_string())?;
        self.stop()?;
        for p in [vusb, &self.paths.qmp_socket(), &self.paths.vsock_socket()] {
            let _ = fs::remove_file(p);
        }
        let _ = fs::remove_file(run.join("qemu.exit"));
        let display_failed = run.join("display-failed");
        let _ = fs::remove_file(&display_failed);
        let _ = fs::remove_file(vitals::proc_path(&self.paths));
        let bundle = Bundle::open(vm).map_err(|p| p.join("; "))?;
        let cfg = create_config();
        // A separate process has no Surface: until frames can reach the app, setup is headless.
        let _ = setting;
        let setting = display::Setting::Off;
        let show = false;
        self.display_attached.store(show, Ordering::SeqCst);
        *self.boot_failure.lock().unwrap() = None;
        *self.stall.lock().unwrap() = None;
        let note = match &setting {
            display::Setting::Headless(why) => why.clone(),
            display::Setting::Off => "Setup runs without a live picture.".to_string(),
            display::Setting::On => String::new(),
        };
        *self.display_status.lock().unwrap() = (setting.name().to_string(), note);
        let spec = match kind {
            VmKind::Dfu => with_unsafe_cache(dfu_spec(&bundle, &cfg, &self.paths, run, vusb, show)),
            VmKind::Ramdisk => dfu_spec(&bundle, &cfg, &self.paths, run, vusb, show),
            VmKind::FirstBoot { gfx: true } => {
                first_boot_spec(&bundle, &cfg, &self.paths, run, show)
            }
            VmKind::FirstBoot { gfx: false } => {
                return Err("a first boot needs the GPU".into());
            }
        };
        let json = serde_json::to_vec_pretty(&spec).map_err(|e| e.to_string())?;
        fs::write(spec_path(&self.paths), json).map_err(|e| e.to_string())?;
        self.process.start(&spec_path(&self.paths), run)?;

        // The pid is written just before QEMU starts, so one that is already gone died at startup.
        self.wait("the VM process", Duration::from_secs(90), || {
            read_pid(&self.paths).is_some() || display_failed.exists()
        })
        .map_err(|e| self.fail_boot(format!("{e}: {}", self.died(run))))?;
        if display_failed.exists() {
            let why = fs::read_to_string(&display_failed).unwrap_or_default();
            return Err(self.fail_boot(format!("The display could not start: {}", why.trim())));
        }
        let pid = read_pid(&self.paths).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(500));
        if !alive(pid) {
            return Err(self.fail_boot(self.died(run)));
        }
        *self.current.lock().unwrap() = Some(Current {
            pid,
            run: run.to_path_buf(),
            sync: spec.sync_on_exit.iter().map(PathBuf::from).collect(),
        });

        if matches!(kind, VmKind::Dfu | VmKind::Ramdisk) {
            self.wait("QEMU's USB socket", Duration::from_secs(120), || {
                vusb.exists() || !alive(pid)
            })?;
            if !alive(pid) {
                return Err(self.fail_boot(self.died(run)));
            }
            // The probe that follows waits for DFU; the broker keeps reconnecting to this socket.
        }
        Ok(())
    }

    pub fn wait_exit(&self, limit: Duration) -> Result<(), String> {
        let Some(pid) = self.pid() else {
            return Ok(());
        };
        self.wait("the VM to shut down", limit, || !alive(pid))
    }

    /// Re-adopts a VM process that outlived the service (it keeps running through an app kill).
    pub fn adopt(&self, run: &Path) -> bool {
        match read_pid(&self.paths).filter(|p| alive(*p)) {
            Some(pid) => {
                // The spec on disk is the boot this process was started from.
                let sync = fs::read(spec_path(&self.paths))
                    .ok()
                    .and_then(|json| serde_json::from_slice::<RunSpec>(&json).ok())
                    .map_or_else(Vec::new, |spec| spec.sync_on_exit);
                *self.current.lock().unwrap() = Some(Current {
                    pid,
                    run: run.to_path_buf(),
                    sync: sync.iter().map(PathBuf::from).collect(),
                });
                true
            }
            None => false,
        }
    }

    pub fn exit_detail(&self) -> String {
        let current = self.current.lock().unwrap().as_ref().map(|c| c.run.clone());
        match current {
            Some(run) => self.died(&run),
            None => self.stall_facts().unwrap_or_else(|| "the VM is not running".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::tests::fixture;

    fn paths() -> Paths {
        Paths {
            files: "/data/files".into(),
            cache: "/data/cache".into(),
            external: "/sdcard/ext".into(),
            native_lib: "/lib".into(),
        }
    }

    fn cfg() -> VmConfig {
        VmConfig {
            vcpus: 6,
            ram_mib: 6144,
            tb_mib: 1024,
            performance_hints: false,
        }
    }

    #[test]
    fn dfu_arguments_port_dfu_up() {
        let dir = fixture("dfu-args");
        fs::write(dir.join("SEPStorage"), b"x").unwrap();
        let b = Bundle::open(&dir).unwrap();
        let run = dir.join(".runs-dfu");
        let spec = dfu_spec(&b, &cfg(), &paths(), &run, Path::new("/data/files/cv/vusb.sock"), false);
        let line = spec.args.join(" ");
        let v = dir.to_string_lossy();
        assert!(line.contains(&format!("-M vresearch101,ecid=deadbeef,sep-rom={v}/AVPSEPBooter.vresearch1.bin")));
        assert!(line.contains("-accel tcg,tb-size=1024 -smp 6,maxcpus=7 -m 6G"));
        assert!(line.contains("-chardev socket,id=vusb,path=/data/files/cv/vusb.sock,server=on,wait=off"));
        assert!(line.contains("-global vresearch-vusb.chardev=vusb"));
        assert!(line.contains("-qmp unix:/data/files/qmp.sock,server=on,wait=off"));
        assert!(line.contains(&format!("-drive file={v}/SEPStorage,if=none,id=sep-storage,format=raw,file.locking=off")));
        assert!(!line.contains("apple-gfx"));
        assert!(spec.env.iter().any(|(k, _)| k == "VPHONE_TARGET_PAGE_BITS"));
    }

    #[test]
    fn a_creation_boot_with_a_display_attaches_the_gpu_as_gfx_1_window_1() {
        let dir = fixture("dfu-gfx");
        fs::write(dir.join("gfx-res"), "860x1864\n").unwrap();
        let b = Bundle::open(&dir).unwrap();
        let spec = dfu_spec(&b, &cfg(), &paths(), &dir.join("r"), Path::new("/cv/vusb.sock"), true);
        let line = spec.args.join(" ");
        assert!(line.contains("gfx-device=apple-gfx-mmio,memory-backend=ram0,gfx-modes=860x1864@60"), "{line}");
        assert!(line.contains("-object memory-backend-memfd,id=ram0,size=6G,share=on"), "{line}");
        assert!(line.contains("-global apple-gfx-mmio.resolution=860x1864"), "{line}");
        assert!(line.contains("vresearch-vusb.chardev=vusb"), "still the DFU machine: {line}");
        assert!(spec.gfx, "it loads Vulkan");
        assert_eq!(
            spec.env.iter().find(|(k, _)| k == "REIMS_VGPU_WINDOW").map(|(_, v)| v.as_str()),
            Some("1")
        );
        let headless = dfu_spec(&b, &cfg(), &paths(), &dir.join("r"), Path::new("/cv/vusb.sock"), false);
        assert!(!headless.args.join(" ").contains("memory-backend"), "no shared RAM without a display");
        assert!(!headless.gfx);
    }

    #[test]
    fn first_boot_is_headless_with_gpu() {
        let dir = fixture("fb-args");
        let b = Bundle::open(&dir).unwrap();
        let spec = first_boot_spec(&b, &cfg(), &paths(), &dir.join(".runs-boot"), false);
        assert!(spec.gfx);
        assert!(spec.args.join(" ").contains("apple-gfx-mmio"));
        assert_eq!(
            spec.env.iter().find(|(k, _)| k == "REIMS_VGPU_WINDOW").map(|(_, v)| v.as_str()),
            Some("0")
        );
    }

    #[test]
    fn setup_drives_are_unsafe_and_listed_for_sync_and_normal_boots_are_untouched() {
        let dir = fixture("drive-cache");
        fs::write(dir.join("SEPStorage"), b"x").unwrap();
        let b = Bundle::open(&dir).unwrap();
        let v = dir.to_string_lossy();
        let drives = |args: &[String]| -> Vec<String> {
            args.windows(2).filter(|w| w[0] == "-drive").map(|w| w[1].clone()).collect()
        };
        let want: Vec<String> = ["nvram.bin", "Disk.img", "SEPStorage"].iter().map(|f| format!("{v}/{f}")).collect();
        let boot = dfu_spec(&b, &cfg(), &paths(), &dir.join("r"), Path::new("/cv/vusb.sock"), false);
        let restore = with_unsafe_cache(boot.clone());
        let restore_drives = drives(&restore.args);
        assert_eq!(restore_drives.len(), 5, "{restore_drives:?}");
        assert!(restore_drives.iter().all(|d| d.ends_with(",cache=unsafe")), "{restore_drives:?}");
        assert_eq!(restore.sync_on_exit, want);
        for spec in [boot, first_boot_spec(&b, &cfg(), &paths(), &dir.join("r"), false)] {
            let drives = drives(&spec.args);
            assert_eq!(drives.len(), 5, "{drives:?}");
            assert!(drives.iter().all(|d| !d.contains("cache=")), "{drives:?}");
            assert!(spec.sync_on_exit.is_empty());
        }
        let normal = launcher::build(&b, &cfg(), &paths());
        let drives = drives(&normal.args);
        assert_eq!(drives.len(), 5, "{drives:?}");
        assert!(drives.iter().all(|d| !d.contains("cache=")), "{drives:?}");
    }

    #[test]
    fn a_drive_files_commas_are_undoubled() {
        assert_eq!(drive_file("file=/a,,b/Disk.img,if=none,format=raw").as_deref(), Some("/a,b/Disk.img"));
        assert_eq!(drive_file("file=/a/Disk.img").as_deref(), Some("/a/Disk.img"));
        assert_eq!(drive_file("if=none,file=/a"), None);
    }

    struct NoProcess;
    impl VmProcess for NoProcess {
        fn start(&self, _: &Path, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn release(&self) {}
        fn exit_reason(&self) -> String {
            String::new()
        }
    }

    fn supervisor(name: &str) -> (Supervisor, PathBuf) {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures").join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let p = Paths {
            files: d.clone(),
            cache: d.join("c"),
            external: d.join("e"),
            native_lib: "/nonexistent".into(),
        };
        (Supervisor::new(p, Arc::new(NoProcess), Arc::new(|| false), Arc::new(|| false)), d)
    }

    #[test]
    fn stopping_an_ended_setup_boot_syncs_its_images() {
        let (sup, d) = supervisor("vm-sync-ok");
        let image = d.join("Disk.img");
        fs::write(&image, b"disk").unwrap();
        let gone = d.join("gone.img");
        *sup.current.lock().unwrap() = Some(Current { pid: 0, run: d.clone(), sync: vec![image, gone] });
        assert_eq!(sup.stop(), Ok(()));
        assert!(sup.current.lock().unwrap().is_none());
    }

    #[test]
    fn an_image_that_cannot_be_synced_fails_the_stop() {
        let (sup, d) = supervisor("vm-sync-bad");
        // A directory cannot be opened for writing, which reaches the sync's error path.
        *sup.current.lock().unwrap() = Some(Current { pid: 0, run: d.clone(), sync: vec![d.clone()] });
        let error = sup.stop().unwrap_err();
        assert!(error.contains("cannot open"), "{error}");
        assert!(sup.current.lock().unwrap().is_none(), "the boot is over either way");
    }

    #[test]
    fn a_spec_is_used_once() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/vm-once");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("cv")).unwrap();
        let p = Paths {
            files: d.clone(),
            cache: d.join("c"),
            external: d.join("e"),
            native_lib: "/nonexistent".into(),
        };
        fs::write(spec_path(&p), serde_json::to_vec(&RunSpec {
            stdout: d.join("o").to_string_lossy().into_owned(),
            stderr: d.join("e").to_string_lossy().into_owned(),
            paths: ["/nonexistent".to_string(), "/nonexistent".to_string(), "/nonexistent".to_string(), "/nonexistent".to_string()],
            ..Default::default()
        }).unwrap()).unwrap();
        // No QEMU library at that path: reports it and returns, never starts a VM.
        assert!(run_spec_file(&spec_path(&p)).is_err());
    }
}
