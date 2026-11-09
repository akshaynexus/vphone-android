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

pub mod display;
pub mod vitals;

const TAG: &str = "vphone-create";
const QEMU_STACK: usize = 64 << 20;
/// dfu-up.sh's sleep: AVPBooter reaches DFU. A phone needs no less.
const DFU_SETTLE: Duration = Duration::from_secs(20);
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
    /// Written, with the reason, when this boot's display could not start.
    pub display_failed: String,
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
    cfg.performance_hints = false;
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
    let mut args: Vec<String> = ["qemu-system-aarch64", "-M", &machine, "-accel"]
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
    finish_spec(args, env, run, paths, display)
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
    finish_spec(launch.args, env, run, paths, true)
}

/// Each switch turns off one fast path built and tuned on x86 hosts. Later values win.
/// `QEMU_TLB_UNSYNCED` stays at 2: with too few host cores, mode 0 kept a restore from reaching
/// its device at all, where mode 2 got through.
const SAFE_EMULATION: [(&str, &str); 5] = [
    ("VPHONE_NATIVE_FP", "0"),
    ("VPHONE_FAST_F32", "0"),
    ("VPHONE_INLINE_LOOKUP", "0"),
    ("VPHONE_XPAGE_LINKS", "0"),
    ("VPHONE_RET_PREFETCH", "0"),
];

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
        display_failed: at("display-failed"),
    }
}

/// The `:createvm` process: runs the spec's QEMU, once. A service that the system restarted
/// finds its spec already taken and returns an error instead of booting a VM twice.
pub fn run_from_spec(paths: &Paths) -> Result<(), String> {
    let path = spec_path(paths);
    let taken = path.with_extension("taken");
    fs::rename(&path, &taken).map_err(|_| "no VM spec is waiting".to_string())?;
    let spec: RunSpec = serde_json::from_slice(&fs::read(&taken).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;

    for d in launcher::runtime_dirs(paths) {
        let _ = fs::create_dir_all(d);
    }
    let write_err = |message: &str| {
        let _ = fs::write(&spec.stderr, format!("{message}\n"));
    };
    let qemu = runtime::load_qemu(paths).inspect_err(|e| write_err(e))?;
    if spec.gfx {
        let gipa = runtime::load_vulkan(paths).inspect_err(|e| {
            write_err(e);
            let _ = fs::write(&spec.display_failed, e);
        })?;
        // SAFETY: gipa is vkGetInstanceProcAddr of the loader just opened.
        unsafe { (qemu.set_gipa)(gipa) };
        // A view that was already open gave its Surface before QEMU's library was loaded.
        runtime::reapply_window();
    }
    vitals::spawn_self_stats(vitals::proc_path(paths));

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
    std::thread::Builder::new()
        .name("qemu-main".into())
        .stack_size(QEMU_STACK)
        .spawn(move || {
            let code = runtime::run_qemu(qemu, spec.args);
            let _ = fs::write(&spec.exit_file, code.to_string());
            std::process::exit(code);
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// What Android does for the Supervisor: a process per VM, and why one died.
pub trait VmProcess: Send + Sync {
    /// Starts the `:createvm` service. The VM's spec file is ready.
    fn start(&self) -> Result<(), String>;
    /// Releases it, after the process has ended.
    fn release(&self);
    /// Why Android says the last VM process ended, if it knows.
    fn exit_reason(&self) -> String;
}

#[derive(Debug, Default)]
struct Current {
    pid: i32,
    run: PathBuf,
}

pub struct Supervisor {
    safe_emulation: std::sync::atomic::AtomicBool,
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
    // The pid of a dead :createvm may be reused; only ours is ever signalled.
    fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|c| String::from_utf8_lossy(&c).contains(":createvm"))
}

#[cfg(not(target_os = "android"))]
fn is_our_vm_process(_pid: i32) -> bool {
    true
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
            safe_emulation: std::sync::atomic::AtomicBool::new(false),
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
    /// Boots after this leave out the fast paths written for x86 hosts: slower, but it rules
    /// them out where a boot has wedged.
    pub fn use_safe_emulation(&self) {
        self.safe_emulation.store(true, std::sync::atomic::Ordering::SeqCst);
    }

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

    fn stop_inner(&self) -> Result<(), String> {
        let Some(pid) = self.pid().filter(|p| alive(*p)) else {
            let _ = fs::remove_file(pid_path(&self.paths));
            self.process.release();
            *self.current.lock().unwrap() = None;
            return Ok(());
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
        self.process.release();
        let _ = fs::remove_file(pid_path(&self.paths));
        *self.current.lock().unwrap() = None;
        Ok(())
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
        let show = setting == display::Setting::On;
        self.display_attached.store(show, Ordering::SeqCst);
        *self.boot_failure.lock().unwrap() = None;
        *self.stall.lock().unwrap() = None;
        let note = match &setting {
            display::Setting::Headless(why) => why.clone(),
            display::Setting::Off => "The screen is switched off for this VM.".to_string(),
            display::Setting::On => String::new(),
        };
        *self.display_status.lock().unwrap() = (setting.name().to_string(), note);
        let spec = match kind {
            VmKind::Dfu | VmKind::Ramdisk => dfu_spec(&bundle, &cfg, &self.paths, run, vusb, show),
            VmKind::FirstBoot { gfx: true } => {
                first_boot_spec(&bundle, &cfg, &self.paths, run, show)
            }
            VmKind::FirstBoot { gfx: false } => {
                return Err("a first boot needs the GPU".into());
            }
        };
        let mut spec = spec;
        if self.safe_emulation.load(std::sync::atomic::Ordering::SeqCst) {
            spec.env.extend(SAFE_EMULATION.map(|(k, v)| (k.to_string(), v.to_string())));
        }
        let json = serde_json::to_vec_pretty(&spec).map_err(|e| e.to_string())?;
        fs::write(spec_path(&self.paths), json).map_err(|e| e.to_string())?;
        self.process.start()?;

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
        });

        if matches!(kind, VmKind::Dfu | VmKind::Ramdisk) {
            self.wait("QEMU's USB socket", Duration::from_secs(120), || {
                vusb.exists() || !alive(pid)
            })?;
            if !alive(pid) {
                return Err(self.fail_boot(self.died(run)));
            }
            // AVPBooter reaches DFU (dfu-up.sh's sleep).
            let end = Instant::now() + DFU_SETTLE;
            while Instant::now() < end {
                if (self.cancelled)() {
                    return Err("cancelled".into());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
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
                let _ = self.process.start();
                *self.current.lock().unwrap() = Some(Current {
                    pid,
                    run: run.to_path_buf(),
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
        // Spec present, no library: fails after being taken, and cannot start a second time.
        fs::write(spec_path(&p), serde_json::to_vec(&RunSpec {
            stdout: d.join("o").to_string_lossy().into_owned(),
            stderr: d.join("e").to_string_lossy().into_owned(),
            ..Default::default()
        }).unwrap()).unwrap();
        let _ = fs::File::create(d.join("o"));
        assert!(run_from_spec(&p).is_err());
        assert!(!spec_path(&p).exists());
        assert_eq!(run_from_spec(&p).unwrap_err(), "no VM spec is waiting");
    }
}
