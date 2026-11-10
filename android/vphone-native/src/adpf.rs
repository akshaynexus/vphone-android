//! Android's Performance Hint API for the vCPU threads: each period, the busiest vCPU's CPU time
//! is reported as the work done. A busy vCPU then exceeds the target and the system raises its
//! clocks and placement; idle ones report little and let the phone cool.
//!
//! Nothing here is taken for granted. `libandroid.so` is opened at run time, so a process it
//! does not load in goes without hints; the manager, the session and the first report can each
//! be refused. Whichever way it goes, `report` is told once, in one line that also names the
//! CPUs this process may run on.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::time::{Duration, Instant};

const PERIOD: Duration = Duration::from_millis(50);
const TARGET_NS: i64 = 40_000_000;

type GetManager = unsafe extern "C" fn() -> *mut c_void;
type CreateSession = unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void;
type ReportActual = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type CloseSession = unsafe extern "C" fn(*mut c_void);

struct Api {
    get_manager: GetManager,
    create_session: CreateSession,
    report_actual: ReportActual,
    close_session: CloseSession,
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
pub fn start(vcpus: u32, done: impl Fn() -> bool + Send + 'static, report: impl Fn(&str) + Send + 'static) {
    let spawned = std::thread::Builder::new().name("vm-adpf".into()).spawn(move || {
        let said = std::cell::Cell::new(false);
        let say = |result: Result<usize, String>| {
            if !said.replace(true) {
                report(&status_line(&result, &allowed_cpus()));
            }
        };
        if let Err(why) = run(vcpus as usize, &done, &say, &|line| report(line)) {
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

fn run(vcpus: usize, done: &dyn Fn() -> bool, say: &dyn Fn(Result<usize, String>), say_more: &dyn Fn(&str)) -> Result<(), String> {
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
    if tids.iter().any(|t| cpu_ns(*t).is_none()) {
        return Err("this kernel has no schedstat to measure the vCPU threads by".into());
    }
    let api = Api::load()?;
    // SAFETY: the manager is process-wide.
    let manager = unsafe { (api.get_manager)() };
    if manager.is_null() {
        return Err("this device has no performance hint manager".into());
    }
    // SAFETY: tids are this process's threads and outlive the call.
    let session = unsafe { (api.create_session)(manager, tids.as_ptr(), tids.len(), TARGET_NS) };
    if session.is_null() {
        return Err("the system refused a hint session for the vCPU threads".into());
    }
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
            say_more(&placement_line(&cpus, &speeds, reports));
        }
        let now: Vec<i64> = tids.iter().map(|t| cpu_ns(*t).unwrap_or(0)).collect();
        let busiest = now.iter().zip(&last).map(|(a, b)| a - b).max().unwrap_or(0);
        last = now;
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
        start(2, || true, move |line| sink.lock().unwrap().push(line.to_owned()));
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
