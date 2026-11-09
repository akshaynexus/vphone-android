//! What each thread of the VM process is doing, written where the app can show it: CPU use per
//! thread every few seconds, and periodically the native stacks of the busy ones. On a phone with
//! no debugger attached this is what says which thread is stuck, and where.

#![cfg_attr(not(target_arch = "aarch64"), allow(dead_code, unused_imports))]

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SAMPLE: Duration = Duration::from_secs(5);
/// Stacks every this many samples.
const STACK_EVERY: u32 = 6;
const BUSY_PERCENT: f64 = 5.0;
const STACK_THREADS: usize = 6;
const FRAMES: usize = 20;

/// Where a crash is written: the thread log, already open.
static CRASH_FD: AtomicI32 = AtomicI32::new(-1);
static STACK_TID: AtomicI32 = AtomicI32::new(0);
static STACK_DONE: AtomicBool = AtomicBool::new(false);
static STACK_LEN: AtomicUsize = AtomicUsize::new(0);
static mut STACK: [usize; FRAMES] = [0; FRAMES];

#[derive(Clone)]
struct Thread {
    tid: i32,
    name: String,
    state: char,
    ticks: u64,
}

fn parse_stat(text: &str) -> Option<(char, u64)> {
    let rest = &text[text.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    Some((f.first()?.chars().next()?, f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?))
}

fn threads() -> Vec<Thread> {
    let Ok(entries) = fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let tid = e.file_name().to_string_lossy().parse().ok()?;
            let name = fs::read_to_string(e.path().join("comm")).ok()?.trim().to_string();
            let (state, ticks) = parse_stat(&fs::read_to_string(e.path().join("stat")).ok()?)?;
            Some(Thread { tid, name, state, ticks })
        })
        .collect()
}

#[cfg(target_arch = "aarch64")]
mod stack {
    use super::*;
    use std::os::raw::{c_int, c_void};

    #[repr(C)]
    struct Unwinder {
        _private: [u8; 0],
    }
    type Trace = extern "C" fn(*mut Unwinder, *mut c_void) -> c_int;
    extern "C" {
        fn _Unwind_Backtrace(trace: Trace, arg: *mut c_void) -> c_int;
        fn _Unwind_GetIP(context: *mut Unwinder) -> usize;
    }

    fn push(pc: usize) -> bool {
        let n = STACK_LEN.load(Ordering::Relaxed);
        if n >= FRAMES {
            return false;
        }
        // SAFETY: one thread at a time fills STACK, chosen by STACK_TID.
        unsafe { *std::ptr::addr_of_mut!(STACK[n]) = pc };
        STACK_LEN.store(n + 1, Ordering::Relaxed);
        true
    }

    extern "C" fn trace(context: *mut Unwinder, _: *mut c_void) -> c_int {
        // SAFETY: context is the unwinder's, valid during this call.
        if push(unsafe { _Unwind_GetIP(context) }) { 0 } else { 5 }
    }

    /// SIGSYS sent by this monitor (`tgkill`) asks for a stack: QEMU's threads block every signal
    /// but the fatal ones, so it is one of the few that reach them. Any other arrival is a crash.
    extern "C" fn handler(signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
        // SAFETY: the kernel passes valid siginfo and ucontext for an SA_SIGINFO handler.
        let (pc, lr, regs, code) = unsafe {
            let m = &(*(context as *const libc::ucontext_t)).uc_mcontext;
            (m.pc as usize, m.regs[30] as usize, [m.regs[0], m.regs[1], m.regs[2], m.regs[3]], (*info).si_code)
        };
        if signal == libc::SIGSYS && code == libc::SI_TKILL {
            // SAFETY: gettid has no preconditions.
            if unsafe { libc::gettid() } == STACK_TID.load(Ordering::SeqCst) {
                STACK_LEN.store(0, Ordering::SeqCst);
                push(pc);
                push(lr);
                // SAFETY: trace only writes to static memory.
                unsafe { _Unwind_Backtrace(trace, std::ptr::null_mut()) };
                STACK_DONE.store(true, Ordering::SeqCst);
            }
            return;
        }
        crash(signal, code, unsafe { (*info).si_addr() } as usize, pc, lr, regs);
    }

    /// The process is ending, so ordinary allocation is acceptable; the alarm bounds a deadlock.
    fn crash(signal: c_int, code: c_int, fault: usize, pc: usize, lr: usize, regs: [u64; 4]) {
        // SAFETY: alarm, gettid and the signal calls have no preconditions.
        let tid = unsafe {
            libc::alarm(5);
            libc::gettid()
        };
        STACK_LEN.store(0, Ordering::SeqCst);
        // SAFETY: trace only writes to static memory.
        unsafe { _Unwind_Backtrace(trace, std::ptr::null_mut()) };
        let maps = read_maps();
        let frames: Vec<String> = (0..STACK_LEN.load(Ordering::SeqCst).min(FRAMES))
            // SAFETY: filled just above, by this thread alone.
            .map(|i| symbol(&maps, unsafe { *std::ptr::addr_of!(STACK[i]) }))
            .collect();
        let name = fs::read_to_string(format!("/proc/self/task/{tid}/comm")).unwrap_or_default();
        let text = format!(
            "CRASH {} code {code} in thread {} (tid {tid}) at fault address 0x{fault:x}\n  pc {}\n  lr {}\n  x0 0x{:x} x1 0x{:x} x2 0x{:x} x3 0x{:x}\n  stack: {}\n",
            signal_name(signal),
            name.trim(),
            symbol(&maps, pc),
            symbol(&maps, lr),
            regs[0], regs[1], regs[2], regs[3],
            frames.join(" < "),
        );
        let fd = CRASH_FD.load(Ordering::SeqCst);
        // SAFETY: writes bytes this function owns, then restores the default action and re-raises.
        unsafe {
            libc::write(fd, text.as_ptr().cast(), text.len());
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    fn signal_name(signal: c_int) -> &'static str {
        match signal {
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGILL => "SIGILL",
            libc::SIGABRT => "SIGABRT",
            libc::SIGFPE => "SIGFPE",
            libc::SIGSYS => "SIGSYS",
            _ => "signal",
        }
    }

    pub fn install() -> c_int {
        // SAFETY: sigaction with a zeroed, then filled, struct.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            for signal in [libc::SIGSYS, libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGABRT, libc::SIGFPE] {
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
            libc::SIGSYS
        }
    }

    pub fn capture(signal: c_int, tid: i32) -> Vec<usize> {
        STACK_DONE.store(false, Ordering::SeqCst);
        STACK_TID.store(tid, Ordering::SeqCst);
        // SAFETY: tgkill to a thread of this process.
        if unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, signal) } != 0 {
            return Vec::new();
        }
        let end = Instant::now() + Duration::from_millis(500);
        while !STACK_DONE.load(Ordering::SeqCst) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        STACK_TID.store(0, Ordering::SeqCst);
        let n = STACK_LEN.load(Ordering::SeqCst).min(FRAMES);
        // SAFETY: the handler has finished or will not write again for tid 0.
        (0..n).map(|i| unsafe { *std::ptr::addr_of!(STACK[i]) }).collect()
    }
}

/// Memory and storage, for telling a crash from Android ending the process.
fn resources(files: &std::path::Path) -> String {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |text: &str, key: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.split_ascii_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    };
    let free = crate::create::engine::free_bytes(files) >> 30;
    format!(
        "rss {}M avail {}M disk {}G",
        kb(&status, "VmRSS:") / 1024,
        kb(&meminfo, "MemAvailable:") / 1024,
        free
    )
}

/// `libname+0xfile_offset`, or a note for memory with no file (QEMU's generated code).
fn symbol(maps: &[(usize, usize, usize, String)], pc: usize) -> String {
    match maps.iter().find(|m| pc >= m.0 && pc < m.1) {
        Some((start, _, offset, path)) if !path.is_empty() => {
            let name = path.rsplit('/').next().unwrap_or(path);
            format!("{name}+0x{:x}", pc - start + offset)
        }
        Some(_) => format!("jit:0x{pc:x}"),
        None => format!("?:0x{pc:x}"),
    }
}

fn read_maps() -> Vec<(usize, usize, usize, String)> {
    let Ok(text) = fs::read_to_string("/proc/self/maps") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_ascii_whitespace();
            let (a, b) = f.next()?.split_once('-')?;
            let offset = {
                f.next()?;
                usize::from_str_radix(f.next()?, 16).ok()?
            };
            f.next()?;
            f.next()?;
            Some((
                usize::from_str_radix(a, 16).ok()?,
                usize::from_str_radix(b, 16).ok()?,
                offset,
                f.next().unwrap_or("").to_string(),
            ))
        })
        .collect()
}

/// One sample's busy threads, for the log: `CPU 0/TCG 98% R`.
fn busy_list(rows: &[(Thread, f64)]) -> String {
    let mut out: Vec<String> = rows
        .iter()
        .filter(|(_, p)| *p >= 1.0)
        .map(|(t, p)| format!("{} {:.0}% {}", t.name, p, t.state))
        .collect();
    if out.is_empty() {
        out.push("every thread idle".to_string());
    }
    out.join(", ")
}

/// Starts the monitor: `log` keeps the history, `now` the latest list, for the app to read.
pub fn spawn(log: PathBuf, now: PathBuf, files: PathBuf) {
    let Ok(mut out) = File::create(&log) else {
        return;
    };
    CRASH_FD.store(out.as_raw_fd(), Ordering::SeqCst);
    #[cfg(target_arch = "aarch64")]
    let signal = stack::install();
    let _ = std::thread::Builder::new().name("vm-monitor".into()).spawn(move || {
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        let started = Instant::now();
        let mut last = threads();
        let mut tick = 0u32;
        loop {
            std::thread::sleep(SAMPLE);
            tick += 1;
            let current = threads();
            let mut rows: Vec<(Thread, f64)> = current
                .iter()
                .map(|t| {
                    let before = last.iter().find(|o| o.tid == t.tid).map_or(t.ticks, |o| o.ticks);
                    let percent = t.ticks.saturating_sub(before) as f64 / hz / SAMPLE.as_secs_f64() * 100.0;
                    (t.clone(), percent)
                })
                .collect();
            rows.sort_by(|a, b| b.1.total_cmp(&a.1));
            let total: f64 = rows.iter().map(|r| r.1).sum();
            let line = format!(
                "t={}s {} total {:.0}%: {}",
                started.elapsed().as_secs(),
                resources(&files),
                total,
                busy_list(&rows)
            );
            let _ = writeln!(out, "{line}");
            let list: Vec<String> = rows
                .iter()
                .map(|(t, p)| format!("{:<18} {:>4.0}%  {}", t.name, p, t.state))
                .collect();
            let tmp = now.with_extension("new");
            if fs::write(&tmp, list.join("\n")).is_ok() {
                let _ = fs::rename(&tmp, &now);
            }
            #[cfg(target_arch = "aarch64")]
            if tick % STACK_EVERY == 0 {
                let maps = read_maps();
                for (t, p) in rows.iter().filter(|(t, p)| *p >= BUSY_PERCENT && t.name != "vm-monitor").take(STACK_THREADS) {
                    let frames: Vec<String> = stack::capture(signal, t.tid).iter().map(|pc| symbol(&maps, *pc)).collect();
                    if frames.is_empty() {
                        let _ = writeln!(out, "stack {} ({:.0}%): could not be read", t.name, p);
                    } else {
                        let _ = writeln!(out, "stack {} ({:.0}%): {}", t.name, p, frames.join(" < "));
                    }
                }
            }
            let _ = out.flush();
            last = current;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_thread_s_state_and_cpu_past_a_name_with_spaces() {
        let stat = "123 (CPU 0/TCG) R 1 2 3 4 5 6 7 8 9 10 700 300 14 15";
        assert_eq!(parse_stat(stat), Some(('R', 1000)));
    }

    #[test]
    fn names_a_frame_by_library_and_file_offset() {
        let maps = vec![(0x1000, 0x2000, 0x500, "/app/lib/libqemu-vphone.so".to_string()), (0x8000, 0x9000, 0, String::new())];
        assert_eq!(symbol(&maps, 0x1010), "libqemu-vphone.so+0x510");
        assert_eq!(symbol(&maps, 0x8010), "jit:0x8010");
        assert_eq!(symbol(&maps, 0x7000), "?:0x7000");
    }
}
