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

struct Qemu {
    main: unsafe extern "C" fn(c_int, *mut *mut c_char) -> c_int,
    set_gipa: unsafe extern "C" fn(*mut c_void),
    set_window: unsafe extern "C" fn(*mut c_void),
    pointer: unsafe extern "C" fn(u16, u16, u32),
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

fn log_line(line: &str) {
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

#[allow(clippy::missing_transmute_annotations)]
fn load_qemu(paths: &Paths) -> Result<&'static Qemu, String> {
    if let Some(q) = QEMU.get() {
        return Ok(q);
    }
    let h = dlopen(&paths.lib("libqemu-vphone.so"))?;
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
        }
    };
    Ok(QEMU.get_or_init(|| q))
}

#[allow(clippy::missing_transmute_annotations)]
/// The system's Vulkan loader, hooked by libadrenotools to load the bundled Turnip.
fn load_vulkan(paths: &Paths) -> Result<*mut c_void, String> {
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
    let dir = paths.vms().join(name);
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
    let dir = paths.vms().join(name);
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
    });
    for w in report.warnings {
        notice(w);
    }

    let Launch { args, env } = launcher::build(&bundle, &cfg, paths);
    log_line(&format!("starting QEMU: {}", args.join(" ")));
    for (key, value) in env {
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
    });

    let run_name = name.to_string();
    let run_log = log_path;
    std::thread::Builder::new()
        .name("qemu-main".into())
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

    if cfg.performance_hints {
        #[cfg(target_os = "android")]
        crate::adpf::start(cfg.vcpus, || QEMU_DONE.load(Ordering::SeqCst));
    }
    std::thread::Builder::new()
        .name("vphoned-watch".into())
        .spawn(move || watch(Client::new(&Paths::get().vsock_socket())))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn run_qemu(qemu: &Qemu, args: Vec<String>) -> c_int {
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

/// start-vm.sh's wait: vphoned's health, then `screen.unlock` until it reports unlocked.
fn watch(client: Client) {
    let mut healthy = false;
    for _ in 0..HEALTH_ATTEMPTS {
        if client.healthy(Duration::from_secs(10)) {
            healthy = true;
            break;
        }
        if !sleep_while_up(Duration::from_secs(10)) {
            return;
        }
    }
    if !healthy {
        update(|s| s.message = "iOS did not answer in 40 minutes; see the log".into());
        return;
    }
    log_line("vphoned is up; unlocking");
    set_phase(Phase::Unlocking, "");
    for _ in 0..20 {
        if !sleep_while_up(Duration::ZERO) {
            return;
        }
        match client.unlock(Duration::from_secs(120)) {
            Ok(true) => {
                set_phase(Phase::Running, "");
                log_line("unlocked");
                return;
            }
            Ok(false) => log_line("screen.unlock: still locked"),
            Err(e) => log_line(&e),
        }
        if !sleep_while_up(Duration::from_secs(15)) {
            return;
        }
    }
    set_phase(
        Phase::Running,
        "iOS is up but the screen did not unlock; swipe up",
    );
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

/// A MotionEvent's primary pointer: action 0 down, 1 up, 2 move, 3 cancel.
pub fn touch(action: i32, x: f32, y: f32) {
    let Some(q) = qemu_running() else { return };
    let (gw, gh) = {
        let s = STATE.lock().unwrap();
        (s.width, s.height)
    };
    let mut p = POINTER.lock().unwrap();
    (p.x, p.y) = to_guest(x, y, p.view_w, p.view_h, gw, gh);
    match action {
        0 | 2 => p.buttons |= BUTTON_LEFT,
        _ => p.buttons &= !BUTTON_LEFT,
    }
    // SAFETY: plain values to a function of the shared contract.
    unsafe { (q.pointer)(p.x, p.y, p.buttons) };
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
