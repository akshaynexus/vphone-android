//! Android's Performance Hint API for the vCPU threads: each period, the busiest vCPU's CPU time
//! is reported as the work done. A busy vCPU then exceeds the target and the system raises its
//! clocks and placement; idle ones report little and let the phone cool.

use crate::platform::log;
use std::os::raw::c_void;
use std::time::{Duration, Instant};

const TAG: &str = "vphone-adpf";
const PERIOD: Duration = Duration::from_millis(50);
const TARGET_NS: i64 = 40_000_000;

#[link(name = "android")]
extern "C" {
    fn APerformanceHint_getManager() -> *mut c_void;
    fn APerformanceHint_createSession(
        m: *mut c_void,
        tids: *const i32,
        n: usize,
        target_ns: i64,
    ) -> *mut c_void;
    fn APerformanceHint_reportActualWorkDuration(s: *mut c_void, ns: i64) -> i32;
    fn APerformanceHint_closeSession(s: *mut c_void);
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

pub fn start(vcpus: u32, done: impl Fn() -> bool + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("vm-adpf".into())
        .spawn(move || run(vcpus as usize, done));
}

fn run(vcpus: usize, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tids = vcpu_threads();
    while tids.len() < vcpus {
        if done() || Instant::now() > deadline {
            log(TAG, "the vCPU threads did not appear; no performance hints");
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
        tids = vcpu_threads();
    }
    if tids.iter().any(|t| cpu_ns(*t).is_none()) {
        log(TAG, "this kernel has no schedstat; no performance hints");
        return;
    }
    // SAFETY: the manager is process-wide; tids are this process's and outlive the call.
    let session = unsafe {
        let manager = APerformanceHint_getManager();
        if manager.is_null() {
            std::ptr::null_mut()
        } else {
            APerformanceHint_createSession(manager, tids.as_ptr(), tids.len(), TARGET_NS)
        }
    };
    if session.is_null() {
        log(TAG, "this device offers no performance hint session");
        return;
    }
    log(TAG, &format!("hint session on {} vCPU threads", tids.len()));
    let mut last: Vec<i64> = tids.iter().map(|t| cpu_ns(*t).unwrap_or(0)).collect();
    while !done() {
        std::thread::sleep(PERIOD);
        let now: Vec<i64> = tids.iter().map(|t| cpu_ns(*t).unwrap_or(0)).collect();
        let busiest = now.iter().zip(&last).map(|(a, b)| a - b).max().unwrap_or(0);
        last = now;
        if busiest > 0 {
            // SAFETY: session is live until the close below.
            unsafe { APerformanceHint_reportActualWorkDuration(session, busiest) };
        }
    }
    // SAFETY: no other user of the session remains.
    unsafe { APerformanceHint_closeSession(session) };
}
