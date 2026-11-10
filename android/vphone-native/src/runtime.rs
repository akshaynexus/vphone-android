//! The running VM: QEMU's library, its thread, its output, the boot sequence after it, and input.
//!
//! QEMU keeps process-global state and cannot be started twice in one process, so a process runs
//! at most one VM; [`State::needs_restart`] tells Kotlin to start a fresh process for the next.
//! QEMU can also end the whole process (its own `exit()`, or a crash): the last-run file, written
//! before it starts, is how the next process learns that this happened.

use crate::bundle::Bundle;
use crate::config::VmConfig;
use crate::launcher::{self, Launch};
use crate::meminfo::MemInfo;
use crate::paths::Paths;
use crate::platform::log;
use crate::preflight;
use crate::qmp;
use crate::vphoned::Client;
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::FromRawFd;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

const TAG: &str = "vphone";
const QEMU_STACK: usize = 64 << 20;
/// The thread QEMU's main loop runs on.
const QEMU_THREAD: &str = "qemu-main";
/// Longer than start-vm.sh's 15 minutes: a phone's cores run TCG several times slower.
const HEALTH_ATTEMPTS: u32 = 240;
const LOG_LIMIT: u64 = 64 << 20;
const BUTTON_LEFT: u32 = 1 << 0;
const BUTTON_MIDDLE: u32 = 1 << 2;

#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Preparing,
    Booting,
    Unlocking,
    Running,
    Stopping,
    Exited,
    Failed,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub phase: Phase,
    pub vm: String,
    pub message: String,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub width: u32,
    pub height: u32,
    /// QEMU has run in this process, so the next VM needs a new one.
    pub needs_restart: bool,
    /// When QEMU started, in ms since the epoch (0 before then), for the elapsed boot time.
    pub started_ms: u64,
    /// The VM's performance setting (`balanced` or `max`) and refresh rate, once it boots: the
    /// screen asks Android for that rate, and leaves sustained-performance mode off for `max`.
    pub performance: String,
    pub refresh_hz: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LastRun {
    pub vm: String,
    /// "running" means the process died with the VM in it.
    pub phase: String,
    pub code: Option<i32>,
    pub message: String,
    pub log_path: String,
}

static STATE: Mutex<State> = Mutex::new(State {
    phase: Phase::Idle,
    vm: String::new(),
    message: String::new(),
    exit_code: None,
    log_path: String::new(),
    width: 0,
    height: 0,
    needs_restart: false,
    started_ms: 0,
    performance: String::new(),
    refresh_hz: 0,
});
static NOTICE: Mutex<String> = Mutex::new(String::new());
static QEMU_DONE: AtomicBool = AtomicBool::new(false);
static WINDOW: Mutex<usize> = Mutex::new(0);
static LOG: Mutex<Option<LogFile>> = Mutex::new(None);
static CAPTURE: Once = Once::new();
static BUTTON_BUSY: AtomicBool = AtomicBool::new(false);

struct LogFile {
    file: File,
    written: u64,
}

pub(crate) struct Qemu {
    main: unsafe extern "C" fn(c_int, *mut *mut c_char) -> c_int,
    pub(crate) set_gipa: unsafe extern "C" fn(*mut c_void),
    pub(crate) set_window: unsafe extern "C" fn(*mut c_void),
    pointer: unsafe extern "C" fn(u16, u16, u32),
    /// `rvg_android_perf_line`: where the guest's frame time went since the last call.
    perf_line: unsafe extern "C" fn(u32, *mut u8, usize) -> usize,
    /// `vphone_android_cpu_line`: the guest's spin hints and the vCPU threads' waits since the
    /// last call; absent in a QEMU built before it.
    cpu_line: Option<unsafe extern "C" fn(*mut u8, usize) -> usize>,
    /// `vphone_android_touch_mark`: a numbered pointer event, when Android read the touch and
    /// when it is sent (`CLOCK_MONOTONIC` ns); and `vphone_android_touch_line`, where the
    /// events' time went on their way to the guest since the last call. Absent in an older QEMU.
    touch_mark: Option<unsafe extern "C" fn(u32, i64, i64)>,
    touch_line: Option<unsafe extern "C" fn(*mut u8, usize) -> usize>,
}
static QEMU: OnceLock<Qemu> = OnceLock::new();

#[derive(Clone, Copy, Default)]
struct Pointer {
    x: u16,
    y: u16,
    buttons: u32,
    view_w: f32,
    view_h: f32,
}
static POINTER: Mutex<Pointer> = Mutex::new(Pointer {
    x: 0,
    y: 0,
    buttons: 0,
    view_w: 0.0,
    view_h: 0.0,
});

pub fn state() -> State {
    STATE.lock().unwrap().clone()
}

fn update(f: impl FnOnce(&mut State)) {
    f(&mut STATE.lock().unwrap());
}

fn set_phase(phase: Phase, message: &str) {
    update(|s| {
        s.phase = phase;
        s.message = message.to_string();
    });
}

pub fn take_notice() -> String {
    std::mem::take(&mut *NOTICE.lock().unwrap())
}

fn notice(text: String) {
    log_line(&text);
    *NOTICE.lock().unwrap() = text;
}

pub(crate) fn log_line(line: &str) {
    log(TAG, line);
    write_log(format!("[vphone] {line}\n").as_bytes());
}

fn write_log(bytes: &[u8]) {
    if let Some(out) = LOG.lock().unwrap().as_mut() {
        if out.written < LOG_LIMIT {
            out.written += bytes.len() as u64;
            let _ = out.file.write_all(bytes);
            if out.written >= LOG_LIMIT {
                let _ = out
                    .file
                    .write_all(b"\n[vphone] log limit reached; the rest is only in logcat\n");
            }
        }
    }
}

// ------------------------------------------------------------------- the last run

fn write_last_run(run: &LastRun) {
    if let Ok(json) = serde_json::to_vec(run) {
        let _ = fs::write(Paths::get().last_run(), json);
    }
}

pub fn last_run() -> Option<LastRun> {
    let bytes = fs::read(Paths::get().last_run()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Kotlin found out why the process with the VM in it died.
pub fn record_death(reason: &str) {
    if let Some(mut run) = last_run() {
        run.phase = "died".into();
        run.message = reason.to_string();
        write_last_run(&run);
    }
}

pub fn log_tail(path: &str, bytes: u64) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map_or(0, |m| m.len());
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(bytes)));
    let mut out = Vec::new();
    let _ = f.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

// ------------------------------------------------------------------- loading

fn dl_error() -> String {
    // SAFETY: dlerror returns NULL or a valid thread-local string.
    let p = unsafe { libc::dlerror() };
    if p.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: non-NULL per the check above.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn dlopen(path: &Path) -> Result<*mut c_void, String> {
    let c = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: c is a valid path string.
    let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("cannot load {}: {}", path.display(), dl_error()));
    }
    Ok(handle)
}

fn dlsym(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: handle came from dlopen; name is NUL-terminated.
    let sym = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if sym.is_null() {
        return Err(format!(
            "{} is missing: {}",
            name.to_string_lossy(),
            dl_error()
        ));
    }
    Ok(sym)
}

/// What the thread QEMU makes as its library loads is called: its `call_rcu` thread, which frees
/// what the others have stopped using. QEMU names no thread that early, so it takes the name of
/// the thread that loads the library.
pub(crate) const LOADER_NAME: &CStr = c"qemu-rcu";

/// [`dlopen`] with the calling thread named `name` for as long as the library's constructors run.
fn dlopen_as(name: &CStr, path: &Path) -> Result<*mut c_void, String> {
    let mut own = [0u8; 16];
    // SAFETY: PR_GET_NAME writes at most 16 bytes; PR_SET_NAME reads a NUL-terminated string.
    unsafe {
        libc::prctl(libc::PR_GET_NAME, own.as_mut_ptr());
        libc::prctl(libc::PR_SET_NAME, name.as_ptr());
    }
    let handle = dlopen(path);
    // SAFETY: `own` is the NUL-terminated name read above.
    unsafe { libc::prctl(libc::PR_SET_NAME, own.as_ptr()) };
    handle
}

#[allow(clippy::missing_transmute_annotations)]
pub(crate) fn load_qemu(paths: &Paths) -> Result<&'static Qemu, String> {
    if let Some(q) = QEMU.get() {
        return Ok(q);
    }
    let h = dlopen_as(LOADER_NAME, &paths.lib("libqemu-vphone.so"))?;
    // SAFETY: these symbols have the signatures of the app's shared contract with libqemu-vphone.
    let q = unsafe {
        Qemu {
            main: std::mem::transmute::<*mut c_void, _>(dlsym(h, c"qemu_android_main")?),
            set_gipa: std::mem::transmute::<*mut c_void, _>(dlsym(
                h,
                c"rvg_android_set_vulkan_gipa",
            )?),
            set_window: std::mem::transmute::<*mut c_void, _>(dlsym(h, c"rvg_android_set_window")?),
            pointer: std::mem::transmute::<*mut c_void, _>(dlsym(h, c"rvg_android_pointer")?),
            perf_line: std::mem::transmute::<*mut c_void, _>(dlsym(h, c"rvg_android_perf_line")?),
            cpu_line: dlsym(h, c"vphone_android_cpu_line").ok().map(|f| std::mem::transmute::<*mut c_void, _>(f)),
            touch_mark: dlsym(h, c"vphone_android_touch_mark").ok().map(|f| std::mem::transmute::<*mut c_void, _>(f)),
            touch_line: dlsym(h, c"vphone_android_touch_line").ok().map(|f| std::mem::transmute::<*mut c_void, _>(f)),
        }
    };
    Ok(QEMU.get_or_init(|| q))
}

#[allow(clippy::missing_transmute_annotations)]
/// The system's Vulkan loader, hooked by libadrenotools to load the bundled Turnip.
pub(crate) fn load_vulkan(paths: &Paths) -> Result<*mut c_void, String> {
    type OpenVulkan = unsafe extern "C" fn(
        c_int,
        c_int,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut *mut c_void,
    ) -> *mut c_void;
    const DRIVER_CUSTOM: c_int = 1 << 0;

    let tools = dlopen(&paths.lib("libadrenotools.so"))?;
    // SAFETY: the signature of adrenotools_open_libvulkan in adrenotools/driver.h.
    let open: OpenVulkan =
        unsafe { std::mem::transmute(dlsym(tools, c"adrenotools_open_libvulkan")?) };
    // libadrenotools concatenates the directory and the file name.
    let dir =
        CString::new(format!("{}/", paths.native_lib.display())).map_err(|e| e.to_string())?;
    // SAFETY: the strings outlive the call; the optional arguments are NULL as the flags require.
    let vulkan = unsafe {
        open(
            libc::RTLD_NOW,
            DRIVER_CUSTOM,
            std::ptr::null(),
            dir.as_ptr(),
            dir.as_ptr(),
            c"libvulkan_freedreno.so".as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if vulkan.is_null() {
        return Err("libadrenotools could not hook the system Vulkan loader (details under the hook_impl tag in logcat)".into());
    }
    dlsym(vulkan, c"vkGetInstanceProcAddr")
}

// ------------------------------------------------------------------- QEMU's output

fn capture(fd: c_int, tag: &'static str) -> Result<(), String> {
    let mut pipe = [0; 2];
    // SAFETY: pipe has room for two descriptors; dup2 and close act on ones just made.
    unsafe {
        if libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(format!("pipe: {}", std::io::Error::last_os_error()));
        }
        libc::fcntl(pipe[1], libc::F_SETPIPE_SZ, 1 << 20);
        if libc::dup2(pipe[1], fd) < 0 {
            return Err(format!("dup2: {}", std::io::Error::last_os_error()));
        }
        libc::close(pipe[1]);
    }
    // SAFETY: the read end was just created and nothing else owns it.
    let mut reader = unsafe { File::from_raw_fd(pipe[0]) };
    std::thread::Builder::new()
        .name(format!("{tag}-log"))
        .spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            let mut partial = Vec::new();
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                write_log(&buf[..n]);
                partial.extend_from_slice(&buf[..n]);
                let mut rest = partial.as_slice();
                while let Some(end) = rest.iter().position(|b| *b == b'\n') {
                    log(tag, String::from_utf8_lossy(&rest[..end]).trim_end());
                    rest = &rest[end + 1..];
                }
                partial = rest.to_vec();
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn open_log(vm_dir: &Path) -> Result<PathBuf, String> {
    let path = vm_dir.join("serial.log");
    let _ = fs::rename(&path, vm_dir.join("serial.log.1"));
    let file = File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    *LOG.lock().unwrap() = Some(LogFile { file, written: 0 });
    let mut result = Ok(());
    CAPTURE.call_once(|| {
        result = capture(1, "qemu-out").and_then(|()| capture(2, "qemu-err"));
    });
    result.map(|()| path)
}

// ------------------------------------------------------------------- boot

/// What Boot would refuse or warn about, for the dialog before it.
pub fn preflight(name: &str) -> preflight::Report {
    let paths = Paths::get();
    let dir = paths.vm_dir(name);
    preflight::check(&dir, &VmConfig::load(&dir), paths, MemInfo::read()).0
}

pub fn boot(name: &str) -> Result<(), String> {
    {
        let mut s = STATE.lock().unwrap();
        if s.needs_restart {
            return Err("QEMU has already run in this process; the app must restart first".into());
        }
        if !matches!(s.phase, Phase::Idle | Phase::Failed) {
            return Err(format!("a VM is already {:?}", s.phase).to_lowercase());
        }
        s.phase = Phase::Preparing;
        s.vm = name.to_string();
        s.message.clear();
        s.exit_code = None;
        s.log_path.clear();
        s.started_ms = 0;
    }
    let name = name.to_string();
    std::thread::Builder::new()
        .name("vm-boot".into())
        .spawn(move || {
            if let Err(message) = start(&name) {
                log_line(&format!("boot failed: {message}"));
                set_phase(Phase::Failed, &message);
                write_last_run(&LastRun {
                    vm: name,
                    phase: "failed".into(),
                    code: None,
                    message,
                    log_path: state().log_path,
                });
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn start(name: &str) -> Result<(), String> {
    let paths = Paths::get();
    let dir = paths.vm_dir(name);
    let cfg = VmConfig::load(&dir);
    let (report, bundle) = preflight::check(&dir, &cfg, paths, MemInfo::read());
    if !report.errors.is_empty() {
        return Err(report.errors.join("\n"));
    }
    let bundle: Bundle = bundle.ok_or("the VM folder cannot be read")?;

    for d in launcher::runtime_dirs(paths) {
        fs::create_dir_all(&d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let _ = fs::remove_file(paths.vsock_socket());
    let _ = fs::remove_file(paths.qmp_socket());

    let log_path = open_log(&dir)?;
    let log_path = log_path.to_string_lossy().into_owned();
    update(|s| {
        s.log_path = log_path.clone();
        (s.width, s.height) = bundle.res;
        s.performance = format!("{:?}", cfg.performance).to_lowercase();
        s.refresh_hz = cfg.refresh_hz;
    });
    for w in report.warnings {
        notice(w);
    }
    if !cfg.display {
        notice("This VM's screen is off in its settings: iOS runs, and nothing is drawn".into());
    }
    log_line(&if cfg.network {
        crate::net::boot_line(paths)
    } else {
        "network: off in this VM's settings; iOS has its network card with nothing on the other end".to_string()
    });

    let Launch { args, env } = launcher::build(&bundle, &cfg, paths);
    log_line(&format!("starting QEMU: {}", args.join(" ")));
    // Dumps of an earlier boot would pass for this one's.
    if let Ok(old) = fs::read_dir(launcher::frame_dump_dir(paths)) {
        for file in old.flatten() {
            let _ = fs::remove_file(file.path());
        }
    }
    for (key, value) in env {
        if matches!(key, "TU_DEBUG"
                | "TU_ANDROID_SWAPCHAIN_LAYOUT"
                | "REIMS_VGPU_SOFTWARE_DISPLAY"
                | "REIMS_VGPU_STATS"
                | "REIMS_VGPU_FRAME_DUMP") {
            log_line(&format!("GPU setting: {key}={value}"));
        }
        std::env::set_var(key, value);
    }

    let gipa = load_vulkan(paths)?;
    let qemu = load_qemu(paths)?;
    // SAFETY: gipa is vkGetInstanceProcAddr of the loader just opened.
    unsafe { (qemu.set_gipa)(gipa) };
    {
        let window = WINDOW.lock().unwrap();
        // SAFETY: the window, if any, is held by this library until it is replaced.
        unsafe { (qemu.set_window)(*window as *mut c_void) };
    }

    // The marker goes down last: past here, a process death is the VM's.
    write_last_run(&LastRun {
        vm: name.to_string(),
        phase: "running".into(),
        code: None,
        message: String::new(),
        log_path: log_path.clone(),
    });
    update(|s| {
        s.needs_restart = true;
        s.phase = Phase::Booting;
        s.started_ms = crate::create::state::now_ms();
    });

    let run_name = name.to_string();
    let run_log = log_path;
    std::thread::Builder::new()
        .name(QEMU_THREAD.into())
        .stack_size(QEMU_STACK)
        .spawn(move || {
            let code = run_qemu(qemu, args);
            QEMU_DONE.store(true, Ordering::SeqCst);
            log_line(&format!("QEMU returned {code}"));
            let message = if code == 0 {
                String::new()
            } else {
                format!("QEMU ended with status {code}")
            };
            update(|s| {
                s.phase = Phase::Exited;
                s.exit_code = Some(code);
                s.message = message.clone();
            });
            write_last_run(&LastRun {
                vm: run_name,
                phase: "exited".into(),
                code: Some(code),
                message,
                log_path: run_log,
            });
        })
        .map_err(|e| format!("cannot start QEMU's thread: {e}"))?;

    let (vcpus, refresh_hz) = (cfg.vcpus, cfg.refresh_hz);
    std::thread::Builder::new()
        .name("vm-status".into())
        .spawn(move || status_lines(vcpus, refresh_hz))
        .map_err(|e| e.to_string())?;
    if cfg.performance_hints {
        #[cfg(target_os = "android")]
        crate::adpf::start(cfg.vcpus, cfg.performance, || QEMU_DONE.load(Ordering::SeqCst), log_line);
    }
    let services = cfg.services;
    let network = cfg.network;
    let vm_dir = dir.clone();
    std::thread::Builder::new()
        .name("vphoned-watch".into())
        .spawn(move || {
            let client = Client::new(&Paths::get().vsock_socket());
            if watch(client.clone()) {
                if network {
                    network_line(&client);
                }
                // After the unlock, which the user waits for; the guest's agent is up.
                crate::services::apply(&client, &vm_dir, services, &|line| {
                    log_line(line);
                    if line.contains("next starts") {
                        notice(line.trim_start_matches("services: ").to_string());
                    }
                });
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn run_qemu(qemu: &Qemu, args: Vec<String>) -> c_int {
    let owned: Vec<CString> = args
        .into_iter()
        .map(|a| CString::new(a).unwrap_or_default())
        .collect();
    let mut argv: Vec<*mut c_char> = owned.iter().map(|a| a.as_ptr() as *mut c_char).collect();
    argv.push(std::ptr::null_mut());
    // SAFETY: argv is a NULL-terminated array of valid strings that outlive the call.
    unsafe { (qemu.main)(owned.len() as c_int, argv.as_mut_ptr()) }
}

/// False when the VM ended or is being stopped: whoever waits should give up.
fn sleep_while_up(d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if QEMU_DONE.load(Ordering::SeqCst) || state().phase == Phase::Stopping {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !QEMU_DONE.load(Ordering::SeqCst) && state().phase != Phase::Stopping
}

/// How often the boot log says how the VM is doing.
const STATUS_EVERY: Duration = Duration::from_secs(30);

/// Every [`STATUS_EVERY`] until QEMU ends: the phase, the process's CPU and memory, its busiest
/// threads, and where the guest's vCPUs are. A black screen is then a guest still in its kernel
/// (EL1), one running its daemons (EL0), or one that has stopped moving.
/// The GPU device's line of where the guest's frame time went since the last call, for a display
/// mode of `cap_hz`; empty where the library gives none.
fn gpu_line(qemu: &Qemu, cap_hz: u32) -> String {
    let mut buffer = vec![0u8; 2048];
    // SAFETY: the buffer holds the capacity given; the call writes at most that.
    let written = unsafe { (qemu.perf_line)(cap_hz, buffer.as_mut_ptr(), buffer.len()) };
    buffer.truncate(written.min(buffer.len()));
    String::from_utf8_lossy(&buffer).into_owned()
}

/// QEMU's line of the guest's spin hints and the vCPU threads' waits since the last call; empty
/// where the library gives none.
fn cpu_line(qemu: &Qemu) -> String {
    let Some(line) = qemu.cpu_line else {
        return String::new();
    };
    let mut buffer = vec![0u8; 1024];
    // SAFETY: the buffer holds the capacity given; the call writes at most that.
    let written = unsafe { line(buffer.as_mut_ptr(), buffer.len()) };
    buffer.truncate(written.min(buffer.len()));
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The touches since the last line: how many came and went on (`touch::Counts`), and QEMU's
/// account of their way to the guest; empty when nobody touched the screen.
fn touch_line(qemu: &Qemu) -> String {
    let counts = TOUCH.lock().unwrap().pacer.take_counts();
    if counts == crate::touch::Counts::default() {
        return String::new();
    }
    let mut buffer = vec![0u8; 1024];
    let written = match qemu.touch_line {
        // SAFETY: the buffer holds the capacity given; the call writes at most that.
        Some(line) => unsafe { line(buffer.as_mut_ptr(), buffer.len()) },
        None => 0,
    };
    buffer.truncate(written.min(buffer.len()));
    let stages = String::from_utf8_lossy(&buffer).into_owned();
    if stages.is_empty() {
        counts.words()
    } else {
        format!("{}; {stages}", counts.words())
    }
}

/// How many 16 KiB pages of guest code are dumped in one run: each is a line of the log.
const CODE_PAGES_SAID: usize = 12;
/// The words dumped before a PC and from it on.
const CODE_WORDS: (u64, u32) = (16, 48);

/// The PC most of the guest's vCPUs share in user space, where at least half of them (and three)
/// are in one 16 KiB page of it: the vCPU to ask, and the PC. TCG keeps a vCPU's PC as of its
/// last indirect branch or helper call, so vCPUs in one loop show the same few addresses: the
/// sign of a spin, or of one piece of work spread over every core.
fn shared_user_pc(vcpus: &[qmp::Vcpu]) -> Option<(u32, u64)> {
    let user: Vec<&qmp::Vcpu> = vcpus.iter().filter(|v| v.el == 0).collect();
    let need = 3.max(vcpus.len().div_ceil(2));
    let mut best: Option<(usize, u32, u64)> = None;
    for v in &user {
        let together = user.iter().filter(|o| o.pc >> 14 == v.pc >> 14).count();
        let same = user.iter().filter(|o| o.pc == v.pc).count();
        if together >= need && best.is_none_or(|(most, _, _)| same > most) {
            best = Some((same, v.index, v.pc));
        }
    }
    best.map(|(_, cpu, pc)| (cpu, pc))
}

/// `guest code 3:00 at 0x2c0eaca3c (cpu0, 4 of 6 vCPUs in this page), from 0x2c0eac9fc: d503201f ...`
fn code_line(at: u64, pc: u64, cpu: u32, words: &[u32]) -> String {
    let words: Vec<String> = words.iter().map(|w| format!("{w:08x}")).collect();
    format!("at {pc:#x} (asked of cpu{cpu}), words from {at:#x}: {}", words.join(" "))
}

/// The stack sampler's cost is two signals and one reading of the process's mappings a sample,
/// on this thread: the first four status lines carry one, then every fourth (two minutes).
fn status_lines(vcpus: u32, refresh_hz: u32) {
    let paths = Paths::get();
    let mut sampler = crate::create::vm::diag::Sampler::new(paths.files.clone()).with_stacks();
    let mut tick = 0u32;
    let mut last: Vec<qmp::Vcpu> = Vec::new();
    let mut still = 0u32;
    // The guest's processes' CPU time at the last line, once its agent answers.
    let mut last_top: Option<(Instant, vphone_create_steps::firstboot::CpuTimes)> = None;
    let mut code_said: Vec<u64> = Vec::new();
    let agent = Client::new(&paths.vsock_socket());
    while sleep_until_done(STATUS_EVERY) {
        let s = state();
        let elapsed = crate::create::state::now_ms().saturating_sub(s.started_ms) / 1000;
        let sample = qmp::sample_vcpus(&paths.qmp_socket(), vcpus);
        let answered = sample.is_ok();
        let mut shared = None;
        let guest = match sample {
            Ok(now) => {
                still = if !now.is_empty() && now == last { still + 1 } else { 0 };
                let text = guest_summary(&now, still, STATUS_EVERY);
                shared = shared_user_pc(&now);
                last = now;
                text
            }
            Err(e) => format!("guest not readable: {e}"),
        };
        let window = if *WINDOW.lock().unwrap() != 0 { "on screen" } else { "no window" };
        let clocks = crate::adpf::take_clock_summary().map_or(String::new(), |c| format!("; {c}"));
        log_line(&format!(
            "status {}:{:02} {}, {window}: {}{clocks}; {guest}",
            elapsed / 60,
            elapsed % 60,
            format!("{:?}", s.phase).to_lowercase(),
            sampler.line(4),
        ));
        // Where a guest frame's time goes, against the refresh rate's budget.
        if let Some(line) = QEMU.get().map(|q| gpu_line(q, refresh_hz)).filter(|l| !l.is_empty()) {
            log_line(&format!("gpu {elapsed_min}:{elapsed_sec:02} {line}", elapsed_min = elapsed / 60, elapsed_sec = elapsed % 60));
        }
        let at = format!("{}:{:02}", elapsed / 60, elapsed % 60);
        // The guest's spin hints, and what the vCPU threads waited on instead of running it.
        if let Some(line) = QEMU.get().map(cpu_line).filter(|l| !l.is_empty()) {
            log_line(&format!("cpu {at} {line}"));
        }
        // Where the touches' time went between the screen and the guest's read of them.
        if let Some(line) = QEMU.get().map(touch_line).filter(|l| !l.is_empty()) {
            log_line(&format!("touch {at} {line}"));
        }
        // Which of the guest's processes used its CPUs since the last line, once its agent is up.
        if matches!(s.phase, Phase::Unlocking | Phase::Running) {
            let now = Instant::now();
            let times = agent
                .rpc("processes.list", serde_json::json!({}), Duration::from_secs(20))
                .ok()
                .as_ref()
                .and_then(vphone_create_steps::firstboot::cpu_times);
            match times {
                Some(times) => {
                    if let Some((then, before)) = &last_top {
                        let top = vphone_create_steps::firstboot::top_line(before, &times, now.duration_since(*then).as_secs_f64());
                        log_line(&format!("guest top {at} {top}"));
                    }
                    last_top = Some((now, times));
                }
                None => log_line(&format!("guest top {at} the guest's agent gave no process list")),
            }
        }
        // Most vCPUs in one page of user code: its instructions, once a page, to tell a spin
        // loop (WFE, YIELD, a compare and a branch back) from work.
        if let Some((cpu, pc)) = shared.filter(|(_, pc)| code_said.len() < CODE_PAGES_SAID && !code_said.contains(&(pc >> 14))) {
            let from = pc.saturating_sub(CODE_WORDS.0 * 4);
            if let Ok(words) = qmp::read_words(&paths.qmp_socket(), cpu, from, CODE_WORDS.1) {
                if !words.is_empty() {
                    code_said.push(pc >> 14);
                    log_line(&format!("guest code {at} {}", code_line(from, pc, cpu, &words)));
                }
            }
        }
        // A boot that is not moving says where it is stuck: the first minutes, then every few.
        // QEMU's main loop not answering is a boot not moving whatever the CPU use, so its
        // thread's stack and the reclaimer's are read even when they are blocked.
        tick += 1;
        if tick <= 4 || tick % 4 == 0 {
            let stuck: &[&str] = if answered { &[] } else { &[QEMU_THREAD, LOADER_NAME.to_str().unwrap_or("")] };
            for line in sampler.stacks(2, stuck) {
                log_line(&line);
            }
        }
    }
}

/// False once QEMU has ended; stopping still counts, as a stuck shutdown is worth logging.
fn sleep_until_done(d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if QEMU_DONE.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !QEMU_DONE.load(Ordering::SeqCst)
}

/// `guest cpu0 EL1 0xfffffe0007a1b2c0, cpu1 EL0 0x1a2b3c`, and how long all of it has not moved.
fn guest_summary(vcpus: &[qmp::Vcpu], still_samples: u32, every: Duration) -> String {
    if vcpus.is_empty() {
        return "guest: no vCPU registers".into();
    }
    let list: Vec<String> = vcpus.iter().map(|v| format!("cpu{} EL{} {:#x}", v.index, v.el, v.pc)).collect();
    let user = vcpus.iter().filter(|v| v.el == 0).count();
    let mut out = format!("guest {} ({user} of {} in user space)", list.join(", "), vcpus.len());
    if still_samples > 0 {
        let secs = u64::from(still_samples) * every.as_secs();
        out.push_str(&format!("; no vCPU has moved in {secs} s"));
    }
    out
}

/// start-vm.sh's wait: vphoned's health, then `screen.unlock` until it reports unlocked. True
/// when the guest's agent answered and the VM is still up.
fn watch(client: Client) -> bool {
    let mut healthy = false;
    for _ in 0..HEALTH_ATTEMPTS {
        if client.healthy(Duration::from_secs(10)) {
            healthy = true;
            break;
        }
        if !sleep_while_up(Duration::from_secs(10)) {
            return false;
        }
    }
    if !healthy {
        update(|s| s.message = "iOS did not answer in 40 minutes; see the log".into());
        return false;
    }
    log_line("vphoned is up; unlocking");
    set_phase(Phase::Unlocking, "");
    for _ in 0..20 {
        if !sleep_while_up(Duration::ZERO) {
            return false;
        }
        match client.unlock(Duration::from_secs(120)) {
            Ok(true) => {
                set_phase(Phase::Running, "");
                log_line("unlocked");
                return true;
            }
            Ok(false) => log_line("screen.unlock: still locked"),
            Err(e) => log_line(&e),
        }
        if !sleep_while_up(Duration::from_secs(15)) {
            return false;
        }
    }
    set_phase(
        Phase::Running,
        "iOS is up but the screen did not unlock; swipe up",
    );
    true
}

/// How long iOS is given to get its address after the unlock, in steps of ten seconds.
const NETWORK_TRIES: u32 = 6;

/// After the unlock: the one `network: ...` line that says whether iOS has its address from
/// QEMU's NAT, resolves a name and connects (`crate::net::report`), asked of the guest's agent.
fn network_line(client: &Client) {
    use crate::net::{self, Link};
    let rpc = |method: &str, params: serde_json::Value, timeout: Duration| client.rpc(method, params, timeout);
    let read = || net::link(&rpc("device.network", serde_json::json!({}), Duration::from_secs(30)));
    let mut link = read();
    // An address the Mac host of an imported VM pinned is not one QEMU's NAT routes.
    if matches!(&link, Link::Other(a) if !a.starts_with("169.254.")) {
        if let Some(line) = net::release_managed_address(&rpc) {
            log_line(&line);
            link = Link::NoAddress;
        }
    }
    for _ in 0..NETWORK_TRIES {
        if !link.may_settle() || !sleep_while_up(Duration::from_secs(10)) {
            break;
        }
        link = read();
    }
    if QEMU_DONE.load(Ordering::SeqCst) {
        return;
    }
    log_line(&net::report(&link, &rpc));
}

// ------------------------------------------------------------------- stop

fn wait_done(limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if QEMU_DONE.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// iOS's own shutdown when vphoned answers, as start-vm.sh --stop does; QMP's quit otherwise.
pub fn stop() {
    {
        let mut s = STATE.lock().unwrap();
        if !matches!(s.phase, Phase::Booting | Phase::Unlocking | Phase::Running) {
            return;
        }
        s.phase = Phase::Stopping;
        s.message.clear();
    }
    let _ = std::thread::Builder::new()
        .name("vm-stop".into())
        .spawn(|| {
            let paths = Paths::get();
            let client = Client::new(&paths.vsock_socket());
            if client.healthy(Duration::from_secs(5)) {
                match client.shutdown() {
                    Ok(()) => {
                        if wait_done(Duration::from_secs(120)) {
                            return;
                        }
                        log_line("iOS did not power off in 2 minutes");
                    }
                    Err(e) => log_line(&e),
                }
            }
            if let Err(e) = qmp::quit(&paths.qmp_socket()) {
                log_line(&format!("QMP quit: {e}"));
            }
            if !wait_done(Duration::from_secs(30)) {
                log_line("QEMU did not stop; ending the process");
                let s = state();
                write_last_run(&LastRun {
                    vm: s.vm,
                    phase: "failed".into(),
                    code: None,
                    message: "QEMU did not stop when asked; the app ended it".into(),
                    log_path: s.log_path,
                });
                // SAFETY: _exit ends the process without running QEMU's or ART's handlers.
                unsafe { libc::_exit(1) };
            }
        });
}

// ------------------------------------------------------------------- display and input

/// The surface for the guest's screen (an acquired ANativeWindow, or 0). Returns the previous one,
/// for the caller to release now that the renderer has let go of it.
pub fn swap_window(window: usize) -> usize {
    let mut current = WINDOW.lock().unwrap();
    let old = std::mem::replace(&mut *current, window);
    if let Some(q) = QEMU.get() {
        // SAFETY: the window stays acquired until it is replaced here.
        unsafe { (q.set_window)(window as *mut c_void) };
    }
    old
}

/// Hands the window a Surface already gave [`swap_window`] to QEMU's library, once it is loaded:
/// the Surface can arrive first (the view is open when a creation boot starts).
pub(crate) fn reapply_window() {
    let window = *WINDOW.lock().unwrap();
    if let (Some(q), true) = (QEMU.get(), window != 0) {
        // SAFETY: the window stays acquired until it is replaced by swap_window.
        unsafe { (q.set_window)(window as *mut c_void) };
    }
}

/// The guest's screen fitted inside `(w, h)`: `(left, top, width, height)`, centred.
pub fn fit(w: i32, h: i32, guest_w: u32, guest_h: u32) -> [i32; 4] {
    let (gw, gh) = (i64::from(guest_w.max(1)), i64::from(guest_h.max(1)));
    let (w64, h64) = (i64::from(w.max(0)), i64::from(h.max(0)));
    let (fw, fh) = if w64 * gh <= h64 * gw {
        (w64, w64 * gh / gw)
    } else {
        (h64 * gw / gh, h64)
    };
    [
        ((w64 - fw) / 2) as i32,
        ((h64 - fh) / 2) as i32,
        fw as i32,
        fh as i32,
    ]
}

/// A point in a view of the image's aspect, in guest pixels. Off the view holds at its edge.
pub fn to_guest(
    x: f32,
    y: f32,
    view_w: f32,
    view_h: f32,
    guest_w: u32,
    guest_h: u32,
) -> (u16, u16) {
    let axis = |v: f32, size: f32, guest: u32| -> u16 {
        let top = guest.saturating_sub(1) as f32;
        (((v / size.max(1.0)) * guest as f32).clamp(0.0, top)) as u16
    };
    (axis(x, view_w, guest_w), axis(y, view_h, guest_h))
}

pub fn set_view(w: i32, h: i32) {
    let mut p = POINTER.lock().unwrap();
    p.view_w = w as f32;
    p.view_h = h as f32;
}

fn qemu_running() -> Option<&'static Qemu> {
    let phase = state().phase;
    matches!(
        phase,
        Phase::Booting | Phase::Unlocking | Phase::Running | Phase::Stopping
    )
    .then(|| QEMU.get())
    .flatten()
}

/// The touch being paced on its way to the guest (`crate::touch`), and the last number given to
/// an event.
struct Touch {
    pacer: crate::touch::Pacer,
    tag: u32,
    flusher: bool,
}
static TOUCH: Mutex<Touch> = Mutex::new(Touch { pacer: crate::touch::Pacer::new(0), tag: 0, flusher: false });
/// Wakes the thread that sends a held move when its time comes.
static TOUCH_HELD: std::sync::Condvar = std::sync::Condvar::new();

/// `CLOCK_MONOTONIC` in ns: the clock of Android's event times and of QEMU's stamps.
fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Sends the guest a pointer state, numbered so that QEMU can time its way (`touch_line`).
/// Called with `TOUCH` held, which keeps the events in order.
fn send_pointer(q: &Qemu, t: &mut Touch, x: u16, y: u16, buttons: u32, event_ns: i64) {
    let mut word = buttons;
    if let Some(mark) = q.touch_mark {
        t.tag = crate::touch::next_tag(t.tag);
        word |= t.tag << crate::touch::TAG_SHIFT;
        // SAFETY: plain values to a function of the shared contract.
        unsafe { mark(t.tag, event_ns, monotonic_ns()) };
    }
    // SAFETY: plain values to a function of the shared contract.
    unsafe { (q.pointer)(x, y, word) };
}

/// The thread that sends a held move once its interval is over, so that a finger that stops
/// between two refreshes is still where the guest thinks it is.
fn touch_flusher() {
    let mut t = TOUCH.lock().unwrap();
    loop {
        let Some(due) = t.pacer.due_at() else {
            t = TOUCH_HELD.wait(t).unwrap();
            continue;
        };
        let now = monotonic_ns();
        if now < due {
            t = TOUCH_HELD.wait_timeout(t, Duration::from_nanos((due - now) as u64)).unwrap().0;
            continue;
        }
        if let (Some(sample), Some(q)) = (t.pacer.due(now), qemu_running()) {
            let buttons = POINTER.lock().unwrap().buttons;
            // A release since then has already said where the finger is.
            if buttons & BUTTON_LEFT != 0 {
                send_pointer(q, &mut t, sample.x, sample.y, buttons, sample.event_ns);
            }
        }
    }
}

/// A MotionEvent's primary pointer: action 0 down, 1 up, 2 move, 3 cancel; `event_ns` is when
/// Android read it (`MotionEvent.getEventTimeNanos`). Moves reach the guest at most once per
/// refresh of its display (`crate::touch`); presses and releases at once.
pub fn touch(action: i32, x: f32, y: f32, event_ns: i64) {
    use crate::touch::{Kind, Sample};
    let Some(q) = qemu_running() else { return };
    let (gw, gh, hz) = {
        let s = STATE.lock().unwrap();
        (s.width, s.height, s.refresh_hz)
    };
    let mut t = TOUCH.lock().unwrap();
    t.pacer.interval_ns = crate::touch::interval_ns(hz);
    if !t.flusher {
        t.flusher = std::thread::Builder::new().name("vm-touch".into()).spawn(touch_flusher).is_ok();
    }
    let (px, py, buttons) = {
        let mut p = POINTER.lock().unwrap();
        (p.x, p.y) = to_guest(x, y, p.view_w, p.view_h, gw, gh);
        match action {
            0 | 2 => p.buttons |= BUTTON_LEFT,
            _ => p.buttons &= !BUTTON_LEFT,
        }
        (p.x, p.y, p.buttons)
    };
    let kind = match action {
        0 => Kind::Down,
        2 => Kind::Move,
        _ => Kind::Up,
    };
    if t.pacer.offer(kind, Sample { x: px, y: py, event_ns }, monotonic_ns()) {
        send_pointer(q, &mut t, px, py, buttons, event_ns);
    } else {
        TOUCH_HELD.notify_one();
    }
}

/// A middle click, which QEMU's touchscreen turns into Cmd+H, iOS's Home shortcut.
pub fn home() {
    let Some(q) = qemu_running() else { return };
    let _ = std::thread::Builder::new()
        .name("home".into())
        .spawn(move || {
            for pressed in [true, false] {
                {
                    let mut p = POINTER.lock().unwrap();
                    if pressed {
                        p.buttons |= BUTTON_MIDDLE;
                    } else {
                        p.buttons &= !BUTTON_MIDDLE;
                    }
                    // SAFETY: plain values to a function of the shared contract.
                    unsafe { (q.pointer)(p.x, p.y, p.buttons) };
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        });
}

/// The side button, through vphoned as the desktop window's Lock does. One press at a time.
pub fn lock() {
    if qemu_running().is_none() || BUTTON_BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("lock".into()).spawn(|| {
        if let Err(e) = Client::new(&Paths::get().vsock_socket()).button("lock") {
            notice(format!("Lock needs iOS to be up: {e}"));
        }
        BUTTON_BUSY.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_summary_counts_user_space_and_stillness() {
        let v = |index, el, pc| qmp::Vcpu { index, pc, el };
        let text = guest_summary(&[v(0, 1, 0xfffffe0007a1b2c0), v(1, 0, 0x1a2b)], 0, STATUS_EVERY);
        assert_eq!(text, "guest cpu0 EL1 0xfffffe0007a1b2c0, cpu1 EL0 0x1a2b (1 of 2 in user space)");
        // The 0.5.9 log's 3:00: six vCPUs in one page of user code, four at one PC.
        let cpu = |index, el, pc| qmp::Vcpu { index, pc, el };
        let spin = [cpu(0, 0, 0x2c0eaca3c), cpu(1, 0, 0x2c0eac8cc), cpu(2, 0, 0x2c0eaca3c), cpu(3, 0, 0x2c0eaca3c), cpu(4, 0, 0x2c0eaca3c), cpu(5, 0, 0x2c0eac8d4)];
        assert_eq!(shared_user_pc(&spin), Some((0, 0x2c0eaca3c)));
        // Its 2:00: four in user space, each elsewhere; and kernel PCs never count.
        let apart = [cpu(0, 0, 0x1a000083c), cpu(1, 0, 0x184e9bb78), cpu(2, 0, 0x184438a9c), cpu(3, 0, 0x181cab53c), cpu(4, 1, 0xfffffe003023a224), cpu(5, 1, 0xfffffe003023f340)];
        assert_eq!(shared_user_pc(&apart), None);
        let kernel: Vec<qmp::Vcpu> = (0..6).map(|i| cpu(i, 1, 0xfffffe0030a39340)).collect();
        assert_eq!(shared_user_pc(&kernel), None);
        assert_eq!(
            code_line(0x2c0eac9fc, 0x2c0eaca3c, 0, &[0xd503205f, 0x17ffffff]),
            "at 0x2c0eaca3c (asked of cpu0), words from 0x2c0eac9fc: d503205f 17ffffff"
        );
        assert!(guest_summary(&[v(0, 1, 4)], 2, STATUS_EVERY).ends_with("no vCPU has moved in 60 s"));
        assert_eq!(guest_summary(&[], 0, STATUS_EVERY), "guest: no vCPU registers");
    }

    /// The loading thread lends its name only while the library loads, and gets it back even
    /// when the library is not there.
    #[test]
    fn the_loader_keeps_its_own_name() {
        let name = || {
            let mut buf = [0u8; 16];
            // SAFETY: PR_GET_NAME writes at most 16 bytes.
            unsafe { libc::prctl(libc::PR_GET_NAME, buf.as_mut_ptr()) };
            CStr::from_bytes_until_nul(&buf).unwrap().to_owned()
        };
        std::thread::Builder::new()
            .name("vm-boot".into())
            .spawn(move || {
                assert!(dlopen_as(LOADER_NAME, Path::new("/nonexistent/libqemu-vphone.so")).is_err());
                assert_eq!(name().as_c_str(), c"vm-boot");
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(LOADER_NAME.to_bytes().len() < 16);
    }

    #[test]
    fn letterboxes_the_narrower_axis() {
        assert_eq!(fit(1440, 3000, 1290, 2796), [28, 0, 1384, 3000]);
        assert_eq!(fit(1000, 3000, 1000, 2000), [0, 500, 1000, 2000]);
    }

    #[test]
    fn touch_maps_and_clamps() {
        assert_eq!(to_guest(0.0, 0.0, 1384.0, 3000.0, 1290, 2796), (0, 0));
        assert_eq!(
            to_guest(692.0, 1500.0, 1384.0, 3000.0, 1290, 2796),
            (645, 1398)
        );
        assert_eq!(
            to_guest(5000.0, -5.0, 1384.0, 3000.0, 1290, 2796),
            (1289, 0)
        );
        assert_eq!(
            to_guest(1384.0, 3000.0, 1384.0, 3000.0, 1290, 2796),
            (1289, 2795)
        );
    }
}
