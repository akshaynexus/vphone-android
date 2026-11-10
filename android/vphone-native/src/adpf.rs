//! Android's Performance Hint API for the vCPU threads: each period, the busiest vCPU's CPU time
//! is reported as the work done. A busy vCPU then exceeds the target and the system raises its
//! clocks and placement; idle ones report little and let the phone cool.
//!
//! Nothing here is taken for granted. `libandroid.so` is opened at run time, so a process it
//! does not load in goes without hints; the manager, the session and the first report can each
//! be refused. Whichever way it goes, `report` is told once, in one line that also names the
//! CPUs this process may run on.

use crate::config::Performance;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const PERIOD: Duration = Duration::from_millis(50);

/// The time a period's work is said to be due in. The work reported is the busiest vCPU's CPU
/// time in the period, at most [`PERIOD`], so the target is the share of a core above which the
/// system is told the work ran late and raises the clocks. TCG has no deadline of its own: a
/// guest that keeps a vCPU busy runs faster for every step of clock it gets.
///
/// Until 0.5.8 the target was 40 ms, 80% of a core. A guest at 75-85% then reported 37-43 ms
/// against it, which is "on time": the system held the clocks at 1.6-2.0 GHz of 3.6-4.6, as it
/// should for work that meets its deadline.
pub fn target_ns(performance: Performance) -> i64 {
    match performance {
        // Over a fifth of a core is late: a busy guest gets the highest clocks.
        Performance::Max => 10_000_000,
        // Over three fifths of a core is late: the clocks follow the load more slowly.
        Performance::Balanced => 30_000_000,
    }
}

/// The nice value the vCPU threads are given: Android's `THREAD_PRIORITY_URGENT_DISPLAY` (-8)
/// for Max, `THREAD_PRIORITY_DISPLAY` (-4) for Balanced. Both are values an app may give its own
/// threads.
pub fn vcpu_nice(performance: Performance) -> i32 {
    match performance {
        Performance::Max => -8,
        Performance::Balanced => -4,
    }
}

/// The vCPU threads' clocks, summed over the samples since the last reading: how many CPUs were
/// sampled, their clocks as thousandths of each CPU's highest, and how many ran at 95% or more.
static CLOCK_SAMPLES: AtomicU64 = AtomicU64::new(0);
static CLOCK_PERMILLE: AtomicU64 = AtomicU64::new(0);
static CLOCK_AT_MAX: AtomicU64 = AtomicU64::new(0);
/// The clocks the system allowed those CPUs at each sample (`scaling_max_freq`, which thermal
/// and power limits lower), as thousandths of each CPU's highest.
static CLOCK_ALLOWED_PERMILLE: AtomicU64 = AtomicU64::new(0);

/// One sample of a vCPU thread's CPU: its clock now, the highest the system allows it now, and
/// its highest, in kHz.
fn clock_sample(now_khz: u64, allowed_khz: u64, max_khz: u64) {
    if max_khz == 0 {
        return;
    }
    let permille = (now_khz.saturating_mul(1000) / max_khz).min(1000);
    let allowed = (allowed_khz.saturating_mul(1000) / max_khz).min(1000);
    CLOCK_SAMPLES.fetch_add(1, Ordering::Relaxed);
    CLOCK_PERMILLE.fetch_add(permille, Ordering::Relaxed);
    CLOCK_ALLOWED_PERMILLE.fetch_add(allowed, Ordering::Relaxed);
    CLOCK_AT_MAX.fetch_add(u64::from(permille >= 950), Ordering::Relaxed);
}

/// `vCPU clocks 54% of max (the system allows 100%), 12% of the time at max`, over the samples
/// since the last call; `None` where there were none (hints off, or clocks an app cannot read).
pub fn take_clock_summary() -> Option<String> {
    let samples = CLOCK_SAMPLES.swap(0, Ordering::Relaxed);
    let permille = CLOCK_PERMILLE.swap(0, Ordering::Relaxed);
    let allowed = CLOCK_ALLOWED_PERMILLE.swap(0, Ordering::Relaxed);
    let at_max = CLOCK_AT_MAX.swap(0, Ordering::Relaxed);
    clock_summary(samples, permille, allowed, at_max)
}

/// The mean clock, the mean of what the system allowed, and the share of samples at the highest.
/// Clocks at what is allowed, and that below 100%, are a limit of the phone's (heat or power);
/// clocks below what is allowed are the governor's choice for this load.
fn clock_summary(samples: u64, permille: u64, allowed: u64, at_max: u64) -> Option<String> {
    (samples > 0).then(|| {
        let percent = |sum: u64| (sum + samples * 5) / (samples * 10);
        format!(
            "vCPU clocks {}% of max (the system allows {}%), {}% of the time at max",
            percent(permille),
            percent(allowed),
            (at_max * 100 + samples / 2) / samples
        )
    })
}

type GetManager = unsafe extern "C" fn() -> *mut c_void;
type CreateSession = unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void;
type ReportActual = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type CloseSession = unsafe extern "C" fn(*mut c_void);

/// `APerformanceHint_setPreferPowerEfficiency` (API 35).
type PreferEfficiency = unsafe extern "C" fn(*mut c_void, bool) -> c_int;
/// `APerformanceHint_notifyWorkloadIncrease` (API 36): session, cpu, gpu, a name for traces.
type WorkloadIncrease = unsafe extern "C" fn(*mut c_void, bool, bool, *const c_char) -> c_int;
/// `AThermal_acquireManager` (API 30).
type ThermalManager = unsafe extern "C" fn() -> *mut c_void;
/// `AThermal_getCurrentThermalStatus` (API 30): `AThermalStatus`, 0 none to 6 shutdown.
type ThermalStatus = unsafe extern "C" fn(*mut c_void) -> c_int;
/// `AThermal_getThermalHeadroom` (API 31): 1.0 is where the system throttles severely.
type ThermalHeadroom = unsafe extern "C" fn(*mut c_void, c_int) -> f32;

struct Api {
    get_manager: GetManager,
    create_session: CreateSession,
    report_actual: ReportActual,
    close_session: CloseSession,
    /// Newer calls, where this Android has them.
    prefer_efficiency: Option<PreferEfficiency>,
    workload_increase: Option<WorkloadIncrease>,
    /// The thermal service's manager, its status and its headroom, where this Android has them.
    thermal: Option<(ThermalManager, ThermalStatus, Option<ThermalHeadroom>)>,
}

fn dl_error() -> String {
    // SAFETY: dlerror returns null or a C string that stays valid until the next dl call here.
    unsafe {
        let e = libc::dlerror();
        if e.is_null() {
            "no reason given".into()
        } else {
            CStr::from_ptr(e).to_string_lossy().into_owned()
        }
    }
}

impl Api {
    fn load() -> Result<Api, String> {
        const LIBRARY: &CStr = c"libandroid.so";
        // SAFETY: a valid name; the handle is kept for the life of the process.
        let handle = unsafe { libc::dlopen(LIBRARY.as_ptr(), libc::RTLD_NOW) };
        if handle.is_null() {
            return Err(format!("libandroid.so did not load: {}", dl_error()));
        }
        let symbol = |name: &CStr| -> Result<*mut c_void, String> {
            // SAFETY: a live handle and a valid name.
            let found = unsafe { libc::dlsym(handle, name.as_ptr() as *const c_char) };
            if found.is_null() {
                Err(format!("libandroid.so has no {}", name.to_string_lossy()))
            } else {
                Ok(found)
            }
        };
        // SAFETY: each symbol is the NDK function of that name, whose C signature the type states.
        unsafe {
            Ok(Api {
                get_manager: std::mem::transmute::<*mut c_void, GetManager>(symbol(c"APerformanceHint_getManager")?),
                create_session: std::mem::transmute::<*mut c_void, CreateSession>(symbol(c"APerformanceHint_createSession")?),
                report_actual: std::mem::transmute::<*mut c_void, ReportActual>(symbol(
                    c"APerformanceHint_reportActualWorkDuration",
                )?),
                close_session: std::mem::transmute::<*mut c_void, CloseSession>(symbol(c"APerformanceHint_closeSession")?),
                prefer_efficiency: symbol(c"APerformanceHint_setPreferPowerEfficiency")
                    .ok()
                    .map(|f| std::mem::transmute::<*mut c_void, PreferEfficiency>(f)),
                workload_increase: symbol(c"APerformanceHint_notifyWorkloadIncrease")
                    .ok()
                    .map(|f| std::mem::transmute::<*mut c_void, WorkloadIncrease>(f)),
                thermal: match (symbol(c"AThermal_acquireManager"), symbol(c"AThermal_getCurrentThermalStatus")) {
                    (Ok(manager), Ok(status)) => Some((
                        std::mem::transmute::<*mut c_void, ThermalManager>(manager),
                        std::mem::transmute::<*mut c_void, ThermalStatus>(status),
                        symbol(c"AThermal_getThermalHeadroom")
                            .ok()
                            .map(|f| std::mem::transmute::<*mut c_void, ThermalHeadroom>(f)),
                    )),
                    _ => None,
                },
            })
        }
    }
}

fn vcpu_threads() -> Vec<i32> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter(|t| {
            std::fs::read_to_string(t.path().join("comm"))
                .is_ok_and(|c| c.trim_end().ends_with("/TCG"))
        })
        .filter_map(|t| t.file_name().to_str()?.parse().ok())
        .collect()
}

/// The threads of the GPU device and its Vulkan runtime (reims-vgpu's FIFO and queue threads,
/// metal2vulkan's): the ones a guest frame waits on once its vCPU has asked for it.
fn gpu_threads() -> Vec<i32> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| is_gpu_thread(c.trim_end())))
        .filter_map(|t| t.file_name().to_str()?.parse().ok())
        .collect()
}

/// A thread name of the GPU device's, as the kernel keeps it (its first 15 bytes).
fn is_gpu_thread(comm: &str) -> bool {
    ["reims-vgpu", "metal2vulkan", "m2v-"].iter().any(|prefix| comm.starts_with(prefix))
}

fn cpu_ns(tid: i32) -> Option<i64> {
    let text = std::fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// CPU numbers as the kernel lists them: `0-3,6-7`.
fn cpu_list(cpus: &[usize]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < cpus.len() {
        let mut j = i;
        while j + 1 < cpus.len() && cpus[j + 1] == cpus[j] + 1 {
            j += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if j > i {
            out += &format!("{}-{}", cpus[i], cpus[j]);
        } else {
            out += &cpus[i].to_string();
        }
        i = j + 1;
    }
    out
}

/// The CPUs this process may run on, and the ones the phone has: `0-7 of 0-7`.
fn allowed_cpus() -> String {
    // SAFETY: the set is a plain bit array that sched_getaffinity fills for this process.
    let allowed: Option<Vec<usize>> = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        (libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0)
            .then(|| (0..libc::CPU_SETSIZE as usize).filter(|cpu| libc::CPU_ISSET(*cpu, &set)).collect())
    };
    let present = std::fs::read_to_string("/sys/devices/system/cpu/present").unwrap_or_default();
    match (allowed, present.trim()) {
        (Some(allowed), "") => cpu_list(&allowed),
        (Some(allowed), present) => format!("{} of {present}", cpu_list(&allowed)),
        (None, _) => "unknown (the kernel did not say)".to_owned(),
    }
}

fn status_line(result: &Result<usize, String>, cpus: &str) -> String {
    match result {
        Ok(threads) => format!(
            "performance hints: ON, the system accepted a session on {threads} vCPU threads and its first report; this process may run on CPUs {cpus}"
        ),
        Err(why) => format!("performance hints: OFF ({why}); this process may run on CPUs {cpus}"),
    }
}

/// Starts the reporting thread. `report` gets one line saying whether the hints took effect.
pub fn start(
    vcpus: u32,
    performance: Performance,
    done: impl Fn() -> bool + Send + 'static,
    report: impl Fn(&str) + Send + 'static,
) {
    let spawned = std::thread::Builder::new().name("vm-adpf".into()).spawn(move || {
        let said = std::cell::Cell::new(false);
        let say = |result: Result<usize, String>| {
            if !said.replace(true) {
                report(&status_line(&result, &allowed_cpus()));
            }
        };
        if let Err(why) = run(vcpus as usize, performance, &done, &say, &|line| report(line)) {
            if said.get() {
                report(&format!("performance hints stopped: {why}"));
            } else {
                say(Err(why));
            }
        }
    });
    // Without the thread nothing can say so later; QEMU's own start reports a process that
    // cannot make threads.
    drop(spawned);
}

/// How often the vCPU threads' placement is said.
const PLACEMENT_EVERY: Duration = Duration::from_secs(30);

/// The CPU a thread last ran on: field 39 of its `stat`.
fn last_cpu(tid: i32) -> Option<usize> {
    let text = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).ok()?;
    text[text.rfind(')')? + 2..].split(' ').nth(36)?.parse().ok()
}

fn cpu_khz(cpu: usize, file: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/{file}")).ok()?.trim().parse().ok()
}

/// `vCPU threads last ran on CPUs 2-3,6-7, at 2.9-3.6 GHz of 3.6-4.3; 598 work reports accepted`.
/// `speeds` is each of those CPUs' clock now and its highest, in kHz; empty where the phone does
/// not let an app read them.
fn placement_line(cpus: &[usize], speeds: &[(u64, u64)], reports: u64) -> String {
    let mut on: Vec<usize> = cpus.to_vec();
    on.sort_unstable();
    on.dedup();
    let ghz = |khz: u64| format!("{:.1}", khz as f64 / 1e6);
    let range = |values: Vec<u64>| match (values.iter().min(), values.iter().max()) {
        (Some(low), Some(high)) if low != high => format!("{}-{}", ghz(*low), ghz(*high)),
        (Some(one), _) => ghz(*one),
        _ => String::new(),
    };
    let clock = if speeds.is_empty() {
        "their clocks are not readable here".to_owned()
    } else {
        format!("at {} GHz of {}", range(speeds.iter().map(|s| s.0).collect()), range(speeds.iter().map(|s| s.1).collect()))
    };
    format!("performance hints: vCPU threads last ran on CPUs {}, {clock}; {reports} work reports accepted in all", cpu_list(&on))
}

/// One cluster of CPUs that share a clock: its CPUs as the kernel lists them (`0-5`), its clock
/// now, the highest the system allows now and its highest, in kHz.
type Cluster = (String, u64, u64, u64);

/// The phone's clusters, from each CPU's `cpufreq/related_cpus`; empty where an app cannot read
/// them.
fn clusters() -> Vec<Cluster> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for cpu in 0..crate::config::online_cpus() as usize {
        let Ok(related) = std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/related_cpus")) else {
            continue;
        };
        if seen.contains(&related) {
            continue;
        }
        let cpus: Vec<usize> = related.split_whitespace().filter_map(|c| c.parse().ok()).collect();
        if let (Some(now), Some(max)) = (cpu_khz(cpu, "scaling_cur_freq"), cpu_khz(cpu, "cpuinfo_max_freq")) {
            out.push((cpu_list(&cpus), now, cpu_khz(cpu, "scaling_max_freq").unwrap_or(max), max));
        }
        seen.push(related);
    }
    out
}

/// `clusters: CPUs 0-5 at 2.4 GHz, allowed 3.6 of 3.6; CPUs 6-7 at 2.5 GHz, allowed 3.9 of 4.6`:
/// a clock below what is allowed is the governor's choice, an allowance below the highest is the
/// phone's limit (heat or power).
fn clusters_line(clusters: &[Cluster]) -> String {
    if clusters.is_empty() {
        return "clusters not readable here".to_owned();
    }
    let ghz = |khz: u64| format!("{:.1}", khz as f64 / 1e6);
    let each: Vec<String> = clusters
        .iter()
        .map(|(cpus, now, allowed, max)| format!("CPUs {cpus} at {} GHz, allowed {} of {}", ghz(*now), ghz(*allowed), ghz(*max)))
        .collect();
    format!("clusters: {}", each.join("; "))
}

/// `scheduler groups cpuset:/top-app, cpu:/top-app`: the CPU and cpuset control groups Android
/// put a thread in, from its `cgroup` file (`4:cpuset:/top-app`); the groups without a CPU
/// controller are left out, and a thread with none (cgroup v2 alone) shows its one path. Android
/// gives the app on screen `top-app`, whose threads the scheduler places and clocks first.
fn scheduler_groups(cgroup: &str) -> String {
    let mut groups: Vec<String> = Vec::new();
    let mut unified = None;
    for line in cgroup.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if controllers.is_empty() {
            unified = Some(path);
        } else if controllers.split(',').any(|c| c == "cpu" || c == "cpuset") {
            groups.push(format!("{controllers}:{path}"));
        }
    }
    match (groups.is_empty(), unified) {
        (false, _) => format!("scheduler groups {}", groups.join(", ")),
        (true, Some(path)) => format!("scheduler group {path}"),
        (true, None) => "scheduler group not readable".to_owned(),
    }
}

/// `thermal status light, headroom 0.82 of 1.00`: Android's thermal status (`AThermalStatus`) and
/// its headroom, where 1.00 is the temperature at which it throttles severely; a headroom the
/// phone does not give (NaN) is left out.
fn thermal_line(status: c_int, headroom: Option<f32>) -> String {
    let name = match status {
        0 => "none",
        1 => "light",
        2 => "moderate",
        3 => "severe",
        4 => "critical",
        5 => "emergency",
        6 => "shutdown",
        _ => "not given",
    };
    match headroom.filter(|h| h.is_finite()) {
        Some(headroom) => format!("thermal status {name}, headroom {headroom:.2} of 1.00"),
        None => format!("thermal status {name}"),
    }
}

/// Gives each of `tids` the nice value `nice`: how many took it, and the first refusal.
fn raise_priority(tids: &[i32], nice: i32) -> (usize, Option<String>) {
    let mut refusal = None;
    let mut raised = 0;
    for tid in tids {
        // SAFETY: setpriority on a thread of this process; a refusal is an error code.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, *tid as libc::id_t, nice) } == 0 {
            raised += 1;
        } else if refusal.is_none() {
            refusal = Some(std::io::Error::last_os_error().to_string());
        }
    }
    (raised, refusal)
}

fn priority_line(nice: i32, threads: usize, raised: usize, refusal: Option<&str>) -> String {
    match refusal {
        None => format!("performance: {raised} vCPU threads run at nice {nice}"),
        Some(why) => format!("performance: {raised} of {threads} vCPU threads took nice {nice}; the system refused the rest ({why})"),
    }
}

fn run(
    vcpus: usize,
    performance: Performance,
    done: &dyn Fn() -> bool,
    say: &dyn Fn(Result<usize, String>),
    say_more: &dyn Fn(&str),
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tids = vcpu_threads();
    while tids.len() < vcpus {
        if done() {
            return Err("the VM ended before its vCPU threads appeared".into());
        }
        if Instant::now() > deadline {
            return Err(format!("only {} of {vcpus} vCPU threads appeared in a minute", tids.len()));
        }
        std::thread::sleep(Duration::from_millis(250));
        tids = vcpu_threads();
    }
    // QEMU makes its vCPU threads one after another, the SEP's among them: a list read part way
    // through would leave the late ones out of the session for the whole boot. Read until it
    // stands still.
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let again = vcpu_threads();
        if again.len() == tids.len() {
            break;
        }
        tids = again;
        if done() || Instant::now() > deadline {
            break;
        }
    }
    // The priority does not need the hint service, so it is set before anything can refuse.
    let nice = vcpu_nice(performance);
    let (raised, refusal) = raise_priority(&tids, nice);
    say_more(&priority_line(nice, tids.len(), raised, refusal.as_deref()));
    if tids.iter().any(|t| cpu_ns(*t).is_none()) {
        return Err("this kernel has no schedstat to measure the vCPU threads by".into());
    }
    let api = Api::load()?;
    // SAFETY: the manager is process-wide.
    let manager = unsafe { (api.get_manager)() };
    if manager.is_null() {
        return Err("this device has no performance hint manager".into());
    }
    let target = target_ns(performance);
    // SAFETY: tids are this process's threads and outlive the call.
    let session = unsafe { (api.create_session)(manager, tids.as_ptr(), tids.len(), target) };
    if session.is_null() {
        return Err("the system refused a hint session for the vCPU threads".into());
    }
    if let (Performance::Max, Some(prefer)) = (performance, api.prefer_efficiency) {
        // SAFETY: session is live; the call only sets a flag of it.
        unsafe { prefer(session, false) };
    }
    say_more(&format!(
        "performance: {performance:?}; a vCPU busier than {}% of a core is reported late{}{}",
        target * 100 / PERIOD.as_nanos() as i64,
        if api.prefer_efficiency.is_some() { ", power efficiency not preferred" } else { "" },
        if api.workload_increase.is_some() { ", workload increases notified" } else { "" },
    ));
    // SAFETY: the manager is process-wide; a null one is never asked.
    let thermal = api.thermal.map(|(acquire, status, headroom)| (unsafe { acquire() }, status, headroom)).filter(|t| !t.0.is_null());
    // The GPU device's threads, given the vCPUs' priority as they appear: a frame's vCPU waits
    // on them, and at the default priority they queue behind the vCPU threads of other work.
    let mut gpu_raised: Vec<i32> = Vec::new();
    let mut ticks = 0u64;
    let mut last: Vec<i64> = tids.iter().map(|t| cpu_ns(*t).unwrap_or(0)).collect();
    let mut outcome = Ok(());
    let (mut reports, mut placed_at) = (0u64, Instant::now());
    while !done() {
        std::thread::sleep(PERIOD);
        // What the hints are for, checked: which CPUs the vCPU threads run on, and how fast those
        // are clocked.
        if placed_at.elapsed() >= PLACEMENT_EVERY {
            placed_at = Instant::now();
            let cpus: Vec<usize> = tids.iter().filter_map(|t| last_cpu(*t)).collect();
            let speeds: Vec<(u64, u64)> = cpus.iter().filter_map(|c| Some((cpu_khz(*c, "scaling_cur_freq")?, cpu_khz(*c, "cpuinfo_max_freq")?))).collect();
            let heat = thermal.map_or(String::new(), |(manager, status, headroom)| {
                // SAFETY: a live manager; both calls only read it.
                let (status, headroom) = unsafe { (status(manager), headroom.map(|h| h(manager, 0))) };
                format!("; {}", thermal_line(status, headroom))
            });
            let groups = tids.first().and_then(|t| std::fs::read_to_string(format!("/proc/self/task/{t}/cgroup")).ok()).unwrap_or_default();
            say_more(&format!(
                "{}; {}{heat}; {}; {} GPU threads at nice {nice}",
                placement_line(&cpus, &speeds, reports),
                clusters_line(&clusters()),
                scheduler_groups(&groups),
                gpu_raised.len()
            ));
        }
        ticks += 1;
        // The clocks the vCPU threads get, for the status line: every fifth of a second.
        if ticks % 4 == 0 {
            for cpu in tids.iter().filter_map(|t| last_cpu(*t)) {
                if let (Some(now), Some(max)) = (cpu_khz(cpu, "scaling_cur_freq"), cpu_khz(cpu, "cpuinfo_max_freq")) {
                    clock_sample(now, cpu_khz(cpu, "scaling_max_freq").unwrap_or(max), max);
                }
            }
        }
        // The GPU device makes its threads as the guest makes its queues: every two seconds.
        if ticks % 40 == 1 {
            let new: Vec<i32> = gpu_threads().into_iter().filter(|t| !gpu_raised.contains(t)).collect();
            if !new.is_empty() {
                let first = gpu_raised.is_empty();
                let (raised, refusal) = raise_priority(&new, nice);
                gpu_raised.extend(new.iter().copied());
                if first {
                    say_more(&match refusal {
                        None => format!("performance: {raised} GPU threads run at nice {nice}, and the ones made later will"),
                        Some(why) => format!("performance: {raised} of {} GPU threads took nice {nice}; the system refused the rest ({why})", new.len()),
                    });
                }
            }
        }
        let now: Vec<i64> = tids.iter().map(|t| cpu_ns(*t).unwrap_or(0)).collect();
        let busiest = now.iter().zip(&last).map(|(a, b)| a - b).max().unwrap_or(0);
        last = now;
        // A guest that stays late, every two seconds: the system may raise the clocks at once
        // rather than step by step.
        if let (Performance::Max, Some(increase), true) = (performance, api.workload_increase, busiest > target && ticks % 40 == 0) {
            // SAFETY: session is live; the name is a static C string.
            unsafe { increase(session, true, false, c"vphone vCPU".as_ptr()) };
        }
        if busiest > 0 {
            // SAFETY: session is live until the close below.
            let status = unsafe { (api.report_actual)(session, busiest) };
            if status != 0 {
                outcome = Err(format!("the system refused a work report (error {status})"));
                break;
            }
            reports += 1;
            say(Ok(tids.len()));
        }
    }
    // SAFETY: no other user of the session remains.
    unsafe { (api.close_session)(session) };
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_read_as_the_kernels_do() {
        assert_eq!(cpu_list(&[]), "");
        assert_eq!(cpu_list(&[3]), "3");
        assert_eq!(cpu_list(&[0, 1, 2, 3, 6, 7]), "0-3,6-7");
        assert_eq!(cpu_list(&[0, 2, 4, 5]), "0,2,4-5");
    }

    #[test]
    fn the_placement_line_names_the_cpus_and_their_clocks() {
        assert_eq!(
            placement_line(&[7, 2, 3, 6, 7, 6], &[(3_600_000, 4_320_000), (2_900_000, 3_630_000)], 598),
            "performance hints: vCPU threads last ran on CPUs 2-3,6-7, at 2.9-3.6 GHz of 3.6-4.3; 598 work reports accepted in all"
        );
        assert_eq!(
            placement_line(&[0], &[], 0),
            "performance hints: vCPU threads last ran on CPUs 0, their clocks are not readable here; 0 work reports accepted in all"
        );
        // This test's own thread has a stat line of the same shape.
        // SAFETY: gettid has no preconditions.
        let me = unsafe { libc::gettid() };
        assert!(last_cpu(me).is_some());
    }

    #[test]
    fn a_busy_vcpu_is_always_late_and_an_idle_one_never() {
        let period = PERIOD.as_nanos() as i64;
        // The 0.5.7 log's vCPUs: 70-90% of a core, 35-45 ms of a 50 ms period.
        for busy in [35_000_000, 40_000_000, 45_000_000] {
            assert!(busy > target_ns(Performance::Max) * 3, "far past the target, not at it");
            assert!(busy > target_ns(Performance::Balanced));
        }
        // An idle guest's few percent is on time in both, so the clocks may fall.
        assert!(period / 10 < target_ns(Performance::Max));
        assert!(target_ns(Performance::Max) < target_ns(Performance::Balanced));
        assert!(target_ns(Performance::Balanced) < period);
        assert!(vcpu_nice(Performance::Max) < vcpu_nice(Performance::Balanced));
        // Nothing below what an app may ask for its own threads (-20 is the kernel's floor).
        assert!(vcpu_nice(Performance::Max) >= -19);
    }

    #[test]
    fn the_clock_summary_is_a_mean_and_a_share() {
        assert_eq!(clock_summary(0, 0, 0, 0), None);
        // Four samples: 1.8 of 3.6 GHz twice, allowed 2.7 (a limit of the phone's), and 4.6 of
        // 4.6 twice.
        assert_eq!(
            clock_summary(4, 500 + 500 + 1000 + 1000, 750 + 750 + 1000 + 1000, 2).as_deref(),
            Some("vCPU clocks 75% of max (the system allows 88%), 50% of the time at max")
        );
        clock_sample(1_800_000, 2_700_000, 3_600_000);
        clock_sample(4_600_000, 4_600_000, 4_600_000);
        clock_sample(1, 1, 0);
        let line = take_clock_summary().unwrap();
        assert!(line.starts_with("vCPU clocks 75% of max (the system allows 88%), 50%"), "{line}");
        assert_eq!(take_clock_summary(), None);
    }

    #[test]
    fn the_clusters_and_the_heat_are_said_as_the_phone_gives_them() {
        let clusters = vec![("0-5".to_owned(), 2_400_000, 3_628_800, 3_628_800), ("6-7".to_owned(), 2_500_000, 3_900_000, 4_608_000)];
        assert_eq!(
            clusters_line(&clusters),
            "clusters: CPUs 0-5 at 2.4 GHz, allowed 3.6 of 3.6; CPUs 6-7 at 2.5 GHz, allowed 3.9 of 4.6"
        );
        assert_eq!(clusters_line(&[]), "clusters not readable here");
        assert_eq!(thermal_line(1, Some(0.816)), "thermal status light, headroom 0.82 of 1.00");
        assert_eq!(thermal_line(0, Some(f32::NAN)), "thermal status none");
        assert_eq!(thermal_line(3, None), "thermal status severe");
        assert_eq!(thermal_line(42, None), "thermal status not given");
        assert_eq!(
            scheduler_groups("5:cpuset:/top-app\n4:cpu,cpuacct:/top-app\n2:memory:/\n0::/uid_10123/pid_456\n"),
            "scheduler groups cpuset:/top-app, cpu,cpuacct:/top-app"
        );
        assert_eq!(scheduler_groups("0::/uid_10123/pid_456\n"), "scheduler group /uid_10123/pid_456");
        assert_eq!(scheduler_groups(""), "scheduler group not readable");
        // Reading this machine's clusters never fails, whatever it lets a process read.
        let _ = clusters_line(&super::clusters());
    }

    #[test]
    fn the_gpu_devices_threads_are_known_by_their_names() {
        // The kernel keeps 15 bytes of a name.
        for name in ["reims-vgpu.fifo", "reims-vgpu.time", "metal2vulkan-wa", "m2v-host-ram"] {
            assert!(is_gpu_thread(name), "{name}");
        }
        for name in ["CPU 0/TCG", "qemu-main", "vm-status", "RenderThread", ""] {
            assert!(!is_gpu_thread(name), "{name}");
        }
        // This process has none; reading its threads is not a failure.
        assert!(gpu_threads().is_empty());
    }

    #[test]
    fn the_priority_line_says_what_took() {
        assert_eq!(priority_line(-8, 7, 7, None), "performance: 7 vCPU threads run at nice -8");
        let refused = priority_line(-8, 7, 0, Some("Permission denied"));
        assert!(refused.starts_with("performance: 0 of 7 vCPU threads took nice -8") && refused.ends_with("(Permission denied)"), "{refused}");
        // A thread that is not there is a refusal, not a crash.
        let (raised, refusal) = raise_priority(&[i32::MAX - 7], 0);
        assert_eq!(raised, 0);
        assert!(refusal.is_some());
    }

    #[test]
    fn the_line_says_plainly_which_way_it_went() {
        let on = status_line(&Ok(7), "0-7 of 0-7");
        assert!(on.starts_with("performance hints: ON") && on.contains("7 vCPU threads") && on.ends_with("CPUs 0-7 of 0-7"), "{on}");
        let off = status_line(&Err("the system refused a hint session for the vCPU threads".into()), "0-5 of 0-7");
        assert!(off.starts_with("performance hints: OFF (the system refused a hint session") && off.ends_with("CPUs 0-5 of 0-7"), "{off}");
    }

    /// On a PC there are no vCPU threads and no libandroid: every refusal must come back as a
    /// reason, never as a crash or a claim.
    #[test]
    fn without_vcpu_threads_it_reports_off_once() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        start(2, Performance::Max, || true, move |line| sink.lock().unwrap().push(line.to_owned()));
        let end = Instant::now() + Duration::from_secs(10);
        while lines.lock().unwrap().is_empty() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("performance hints: OFF (the VM ended before its vCPU threads appeared)"), "{lines:?}");
        assert!(lines[0].contains("this process may run on CPUs "), "{lines:?}");
    }

    #[test]
    fn a_library_that_is_not_there_is_a_reason() {
        if cfg!(target_os = "android") {
            return;
        }
        let why = Api::load().err().expect("a PC has no libandroid.so");
        assert!(why.starts_with("libandroid.so did not load"), "{why}");
    }
}
