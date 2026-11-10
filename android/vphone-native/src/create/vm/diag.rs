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
/// The sampled thread's stack, `[low, high)`, for the frame walk; 0 when it was not found.
static STACK_LOW: AtomicUsize = AtomicUsize::new(0);
static STACK_HIGH: AtomicUsize = AtomicUsize::new(0);

/// A thread's writable stack: bionic names each thread's mapping `stack_and_tls:<tid>`, and the
/// process's first thread has `[stack]`.
fn thread_stack(tid: i32) -> Option<(usize, usize)> {
    stack_in_maps(&fs::read_to_string("/proc/self/maps").ok()?, tid)
}

fn stack_in_maps(maps: &str, tid: i32) -> Option<(usize, usize)> {
    let name = format!("[anon:stack_and_tls:{tid}]");
    // SAFETY: getpid has no preconditions.
    let main = tid == unsafe { libc::getpid() };
    maps.lines().find_map(|l| {
        let mut f = l.split_ascii_whitespace();
        let range = f.next()?;
        if !f.next()?.starts_with("rw") {
            return None;
        }
        let path = l.split_ascii_whitespace().nth(5).unwrap_or("");
        if path != name && !(main && path == "[stack]") {
            return None;
        }
        let (a, b) = range.split_once('-')?;
        Some((usize::from_str_radix(a, 16).ok()?, usize::from_str_radix(b, 16).ok()?))
    })
}
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

/// Text built in a fixed buffer: all a crash handler can rely on in a process that has faulted.
struct Line {
    buf: [u8; 512],
    len: usize,
}

impl Line {
    fn new() -> Line {
        Line { buf: [0; 512], len: 0 }
    }

    fn add(&mut self, bytes: &[u8]) -> &mut Line {
        let n = bytes.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
        self
    }

    fn text(&mut self, text: &str) -> &mut Line {
        self.add(text.as_bytes())
    }

    fn dec(&mut self, value: i64) -> &mut Line {
        if value < 0 {
            self.add(b"-");
        }
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        let mut rest = value.unsigned_abs();
        loop {
            i -= 1;
            digits[i] = b'0' + (rest % 10) as u8;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        self.add(&digits[i..])
    }

    fn hex(&mut self, value: u64) -> &mut Line {
        let mut digits = [0u8; 16];
        let mut i = digits.len();
        let mut rest = value;
        loop {
            i -= 1;
            digits[i] = b"0123456789abcdef"[(rest & 15) as usize];
            rest >>= 4;
            if rest == 0 {
                break;
            }
        }
        self.add(b"0x").add(&digits[i..])
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// A crashed thread's registers, as they were at the fault.
struct Registers {
    pc: u64,
    lr: u64,
    fp: u64,
    x8: u64,
    x: [u64; 4],
}

/// The first thing a crash writes, needing nothing but the buffer.
fn crash_header(out: &mut Line, signal: &str, code: i32, thread: &[u8], tid: i32, fault: u64, r: &Registers) {
    out.text("CRASH ").text(signal).text(" code ").dec(code.into());
    out.text(" in thread ").add(thread).text(" (tid ").dec(tid.into()).text(") at fault address ").hex(fault);
    out.text("\n  pc ").hex(r.pc).text(" lr ").hex(r.lr).text(" x8 ").hex(r.x8);
    out.text("\n  x0 ").hex(r.x[0]).text(" x1 ").hex(r.x[1]).text(" x2 ").hex(r.x[2]).text(" x3 ").hex(r.x[3]).text("\n");
}

// On a PC only for the tests, which fault for real in a child process.
#[cfg(any(target_arch = "aarch64", all(test, target_arch = "x86_64")))]
mod stack {
    use super::*;
    use std::os::raw::{c_int, c_void};

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

    /// Whether a crash has got as far as its stack walk: a fault during it ends the process.
    static CRASHED: AtomicBool = AtomicBool::new(false);
    #[cfg(test)]
    pub static FAULT_IN_WALK: AtomicBool = AtomicBool::new(false);

    #[cfg(target_arch = "aarch64")]
    unsafe fn registers(context: *const libc::ucontext_t) -> Registers {
        let m = &(*context).uc_mcontext;
        Registers { pc: m.pc, lr: m.regs[30], fp: m.regs[29], x8: m.regs[8], x: [m.regs[0], m.regs[1], m.regs[2], m.regs[3]] }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn registers(context: *const libc::ucontext_t) -> Registers {
        let g = &(*context).uc_mcontext.gregs;
        let at = |i: i32| g[i as usize] as u64;
        Registers { pc: at(libc::REG_RIP), lr: 0, fp: 0, x8: at(libc::REG_R8), x: [at(libc::REG_RDI), at(libc::REG_RSI), at(libc::REG_RDX), at(libc::REG_RCX)] }
    }

    /// SIGSYS sent by this monitor (`tgkill`) asks for a stack: QEMU's threads block every signal
    /// but the fatal ones, so it is one of the few that reach them. Any other arrival is a crash.
    extern "C" fn handler(signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
        // SAFETY: the kernel passes valid siginfo and ucontext for an SA_SIGINFO handler.
        let (r, code) = unsafe { (registers(context as *const libc::ucontext_t), (*info).si_code) };
        if signal == libc::SIGSYS && code == libc::SI_TKILL {
            // SAFETY: gettid has no preconditions.
            if unsafe { libc::gettid() } == STACK_TID.load(Ordering::SeqCst) {
                STACK_LEN.store(0, Ordering::SeqCst);
                // Not libunwind: it authenticates the return addresses it reads, and one that
                // fails on a live thread is a SIGILL that ends the VM. The registers, then the
                // frame records, read only inside the thread's own stack.
                push(r.pc as usize);
                push(strip_pac(r.lr) as usize);
                walk_frames(r.fp);
                STACK_DONE.store(true, Ordering::SeqCst);
            }
            return;
        }
        if APP_ONLY.load(Ordering::SeqCst) {
            // In the app's process the crash signals stay Android's; a SIGSYS that is not a
            // sample (seccomp's) goes where it went before.
            // SAFETY: the kernel's arguments, passed on unchanged.
            unsafe { pass_on(signal, info, context) };
            return;
        }
        crash(signal, code, unsafe { (*info).si_addr() } as u64, &r);
    }

    /// The frame records from `fp` (x29) up: each is the caller's x29, then the return address.
    /// Every read is checked against the sampled thread's stack, so a frame without a record
    /// (a leaf, or JIT code) ends the walk instead of faulting.
    fn walk_frames(mut fp: u64) {
        let (low, high) = (STACK_LOW.load(Ordering::SeqCst) as u64, STACK_HIGH.load(Ordering::SeqCst) as u64);
        if low == 0 {
            return;
        }
        while fp >= low && fp % 16 == 0 && fp + 16 <= high {
            // SAFETY: [fp, fp + 16) lies inside the thread's mapped, writable stack.
            let (next, ret) = unsafe { (*(fp as *const u64), *((fp + 8) as *const u64)) };
            if ret == 0 || !push(strip_pac(ret) as usize) || next <= fp {
                break;
            }
            fp = next;
        }
    }

    static APP_ONLY: AtomicBool = AtomicBool::new(false);
    static mut PREVIOUS: Option<libc::sigaction> = None;

    /// The handler SIGSYS had before [`install_sampler`].
    unsafe fn pass_on(signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
        // SAFETY: written once, before the handler that reads it was installed.
        let previous = unsafe { *std::ptr::addr_of!(PREVIOUS) };
        match previous {
            Some(old) if old.sa_flags & libc::SA_SIGINFO != 0 && old.sa_sigaction > 1 => {
                let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                    // SAFETY: an SA_SIGINFO handler has this signature.
                    unsafe { std::mem::transmute(old.sa_sigaction) };
                f(signal, info, context);
            }
            Some(old) if old.sa_sigaction > 1 => {
                // SAFETY: a plain handler has this signature.
                let f: extern "C" fn(c_int) = unsafe { std::mem::transmute(old.sa_sigaction) };
                f(signal);
            }
            _ => {
                // SAFETY: restores the default action and re-raises.
                unsafe {
                    libc::signal(signal, libc::SIG_DFL);
                    libc::raise(signal);
                }
            }
        }
    }

    /// Only the sampling signal, for the app's own process: ART owns the crash signals there.
    pub fn install_sampler() -> c_int {
        // SAFETY: sigaction with a zeroed, then filled, struct; PREVIOUS is written before the
        // handler that reads it can run.
        unsafe {
            APP_ONLY.store(true, Ordering::SeqCst);
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGSYS, std::ptr::null(), &mut old);
            *std::ptr::addr_of_mut!(PREVIOUS) = Some(old);
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART | libc::SA_ONSTACK;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGSYS, &action, std::ptr::null_mut());
            libc::SIGSYS
        }
    }

    /// The header goes out first and needs nothing but this stack, so a process too damaged for
    /// what follows still says what happened. The process ends either way, and the signal it ends
    /// with is this one.
    fn crash(signal: c_int, code: c_int, fault: u64, r: &Registers) {
        let mut name = [0u8; 16];
        // SAFETY: gettid has no preconditions; PR_GET_NAME writes at most 16 bytes to `name`.
        let tid = unsafe {
            libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr());
            libc::gettid()
        };
        let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
        let mut header = Line::new();
        crash_header(&mut header, signal_name(signal), code, name, tid, fault, r);
        let fd = CRASH_FD.load(Ordering::SeqCst);
        // SAFETY: writes bytes this function owns.
        unsafe { libc::write(fd, header.bytes().as_ptr().cast(), header.bytes().len()) };
        if !CRASHED.swap(true, Ordering::SeqCst) {
            walk(fd, r);
        }
        // SAFETY: restores the default action and re-raises.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// The stack and the names of the libraries, which take allocation and an unwinder working in
    /// a process that has already faulted; the alarm bounds a deadlock in either.
    fn walk(fd: c_int, r: &Registers) {
        // SAFETY: alarm has no preconditions.
        unsafe { libc::alarm(5) };
        #[cfg(test)]
        if FAULT_IN_WALK.load(Ordering::SeqCst) {
            // SAFETY: not safe, and meant not to be: a second fault.
            unsafe { std::ptr::write_volatile(0x20 as *mut u8, 1) };
        }
        let maps = read_maps();
        let text = format!(
            "  pc {}\n  lr {}\n",
            symbol(&maps, r.pc as usize),
            symbol(&maps, strip_pac(r.lr) as usize),
        );
        // SAFETY: writes bytes this function owns.
        unsafe { libc::write(fd, text.as_ptr().cast(), text.len()) };
    }

    /// A return address without its pointer authentication code: user addresses have 39 bits.
    fn strip_pac(address: u64) -> u64 {
        address & ((1 << 39) - 1)
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
        let (low, high) = thread_stack(tid).unwrap_or((0, 0));
        STACK_LOW.store(low, Ordering::SeqCst);
        STACK_HIGH.store(high, Ordering::SeqCst);
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

/// The kernel's limit of mappings a process may hold. An app may not read
/// `/proc/sys/vm/max_map_count` (SELinux) and nothing can raise it there, so where it does not
/// read this is the kernel's default, which reims-vgpu's budget of views assumes too.
fn max_map_count() -> u64 {
    fs::read_to_string("/proc/sys/vm/max_map_count").ok().and_then(|l| l.trim().parse().ok()).unwrap_or(65_530)
}

/// How many mappings `/proc/self/maps` lists, and the bytes of them that are the GPU's memory
/// (KGSL's device node, which RSS does not count).
fn maps_summary(maps: &str) -> (usize, u64) {
    let mut gpu = 0u64;
    let mut count = 0;
    for line in maps.lines() {
        count += 1;
        if !line.contains("kgsl") {
            continue;
        }
        let span = line.split_ascii_whitespace().next().and_then(|r| r.split_once('-'));
        if let Some((Ok(low), Ok(high))) = span.map(|(a, b)| (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16))) {
            gpu += high.saturating_sub(low);
        }
    }
    (count, gpu)
}

/// The memory line: what the process holds by kind, what the phone has left, and the mappings
/// against their limit. `anon` is QEMU's own memory and its translated code, `shm` the guest RAM
/// it has touched (a memfd), `gpu` the GPU memory mapped into the process.
fn resources_line(status: &str, meminfo: &str, maps: &str, limit: u64, free_gb: u64) -> String {
    let kb = |text: &str, key: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.split_ascii_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    };
    let (count, gpu) = maps_summary(maps);
    format!(
        "rss {}M (anon {}M shm {}M) gpu {}M avail {}M disk {free_gb}G maps {count}/{limit}",
        kb(status, "VmRSS:") / 1024,
        kb(status, "RssAnon:") / 1024,
        kb(status, "RssShmem:") / 1024,
        gpu >> 20,
        kb(meminfo, "MemAvailable:") / 1024,
    )
}

/// Memory, storage and mappings, for telling a crash from Android ending the process, and for
/// seeing the mappings near their limit before the GPU library's views are refused there.
fn resources(files: &std::path::Path, limit: u64) -> String {
    let read = |path: &str| fs::read_to_string(path).unwrap_or_default();
    let free = crate::create::engine::free_bytes(files) >> 30;
    resources_line(&read("/proc/self/status"), &read("/proc/meminfo"), &read("/proc/self/maps"), limit, free)
}

/// What Android has this process confined to, for telling a kill from a crash.
fn startup_line(status: &str, cgroup: &str, oom_score_adj: &str) -> String {
    let cpus = status.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:")).map_or("?", str::trim);
    format!(
        "startup cpus {cpus} cgroup {} oom_score_adj {}",
        cgroup.trim().replace('\n', "; "),
        oom_score_adj.trim()
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

/// Whose stacks to read: the busiest `threads` of `busy`, then each of `all` named in `always`
/// that is not among them, at 0%.
fn stack_targets(busy: &[(Thread, f64)], all: &[Thread], threads: usize, always: &[&str]) -> Vec<(Thread, f64)> {
    let mut out: Vec<(Thread, f64)> = busy.iter().take(threads).cloned().collect();
    for t in all.iter().filter(|t| always.contains(&t.name.as_str())) {
        if !out.iter().any(|(o, _)| o.tid == t.tid) {
            out.push((t.clone(), 0.0));
        }
    }
    out
}

/// CPU use per thread since the last sample, with the process's memory and mappings. Reads only
/// `/proc`, so it can run in the app's own process, where the crash handler of [`spawn`] must not
/// go (ART handles SIGSEGV itself).
pub struct Sampler {
    hz: f64,
    limit: u64,
    files: PathBuf,
    last: Vec<Thread>,
    at: Instant,
    /// The threads [`Sampler::line`] found busy, for [`Sampler::stacks`].
    busy: Vec<(Thread, f64)>,
    /// The sampling signal, once [`Sampler::with_stacks`] installed its handler.
    signal: Option<std::os::raw::c_int>,
}

impl Sampler {
    pub fn new(files: PathBuf) -> Sampler {
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        Sampler { hz, limit: max_map_count(), files, last: threads(), at: Instant::now(), busy: Vec::new(), signal: None }
    }

    /// Also able to read busy threads' stacks: installs a handler for the sampling signal only,
    /// which is safe beside ART (the crash signals stay Android's).
    pub fn with_stacks(mut self) -> Sampler {
        #[cfg(target_arch = "aarch64")]
        {
            self.signal = Some(stack::install_sampler());
        }
        self
    }

    /// The busy threads, busiest first, with their share of a core since the last call.
    fn rows(&mut self) -> Vec<(Thread, f64)> {
        let current = threads();
        let secs = self.at.elapsed().as_secs_f64().max(0.001);
        let mut rows: Vec<(Thread, f64)> = current
            .iter()
            .map(|t| {
                let before = self.last.iter().find(|o| o.tid == t.tid).map_or(t.ticks, |o| o.ticks);
                (t.clone(), t.ticks.saturating_sub(before) as f64 / self.hz / secs * 100.0)
            })
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        self.last = current;
        self.at = Instant::now();
        rows
    }

    /// `rss ... maps N/65530 total 412%: CPU 0/TCG 98% R, ...`, the busiest `top` threads.
    pub fn line(&mut self, top: usize) -> String {
        let rows = self.rows();
        let total: f64 = rows.iter().map(|r| r.1).sum();
        let shown: Vec<(Thread, f64)> = rows.into_iter().take(top).collect();
        self.busy = shown.iter().filter(|(_, p)| *p >= BUSY_PERCENT).cloned().collect();
        format!("{} total {:.0}%: {}", resources(&self.files, self.limit), total, busy_list(&shown))
    }

    /// `stack qemu-rcu (tid 123, 100%): libqemu-vphone.so+0x1a2b < ...` for the busiest `threads`
    /// of the last [`Sampler::line`], and for every thread named in `always` however idle: the
    /// pc, the return address and the frame records above.
    pub fn stacks(&self, threads: usize, always: &[&str]) -> Vec<String> {
        let Some(_signal) = self.signal else {
            return Vec::new();
        };
        let _targets = stack_targets(&self.busy, &self.last, threads, always);
        #[cfg(not(target_arch = "aarch64"))]
        return Vec::new();
        #[cfg(target_arch = "aarch64")]
        {
            let maps = read_maps();
            _targets
                .iter()
                .map(|(t, p)| {
                    let frames: Vec<String> = stack::capture(_signal, t.tid).iter().map(|pc| symbol(&maps, *pc)).collect();
                    if frames.is_empty() {
                        format!("stack {} (tid {}, {:.0}%): could not be read", t.name, t.tid, p)
                    } else {
                        format!("stack {} (tid {}, {:.0}%): {}", t.name, t.tid, p, frames.join(" < "))
                    }
                })
                .collect()
        }
    }
}

/// Starts the monitor: `log` keeps the history, `now` the latest list, for the app to read.
pub fn spawn(log: PathBuf, now: PathBuf, files: PathBuf) {
    // Appending, as the performance-hint line is written to this file from another handle.
    let Ok(mut out) = File::create(&log).and_then(|_| fs::OpenOptions::new().append(true).open(&log)) else {
        return;
    };
    CRASH_FD.store(out.as_raw_fd(), Ordering::SeqCst);
    let read = |path: &str| fs::read_to_string(path).unwrap_or_default();
    let _ = writeln!(out, "{}", startup_line(&read("/proc/self/status"), &read("/proc/self/cgroup"), &read("/proc/self/oom_score_adj")));
    #[cfg(target_arch = "aarch64")]
    let signal = stack::install();
    let _ = std::thread::Builder::new().name("vm-monitor".into()).spawn(move || {
        let started = Instant::now();
        let mut sampler = Sampler::new(files);
        let mut tick = 0u32;
        loop {
            std::thread::sleep(SAMPLE);
            tick += 1;
            let rows = sampler.rows();
            let total: f64 = rows.iter().map(|r| r.1).sum();
            let line = format!(
                "t={}s {} total {:.0}%: {}",
                started.elapsed().as_secs(),
                resources(&sampler.files, sampler.limit),
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
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_thread_s_writable_stack_by_its_bionic_name() {
        let maps = "\
7000000000-7000001000 ---p 00000000 00:00 0                              [anon:stack_and_tls:4321]
7000001000-7000101000 rw-p 00000000 00:00 0                              [anon:stack_and_tls:4321]
7000101000-7000201000 rw-p 00000000 00:00 0                              [anon:stack_and_tls:43210]";
        assert_eq!(stack_in_maps(maps, 4321), Some((0x7000001000, 0x7000101000)));
        assert_eq!(stack_in_maps(maps, 999), None);
    }

    #[test]
    fn stacks_are_read_of_the_busiest_threads_and_of_the_ones_asked_for_by_name() {
        let t = |tid, name: &str| Thread { tid, name: name.into(), state: 'S', ticks: 0 };
        let all = [t(1, "main"), t(2, "qemu-main"), t(3, "qemu-rcu"), t(4, "CPU 0/TCG"), t(5, "CPU 1/TCG")];
        let busy = [(t(3, "qemu-rcu"), 100.0), (t(4, "CPU 0/TCG"), 60.0), (t(5, "CPU 1/TCG"), 50.0)];
        let names = |v: Vec<(Thread, f64)>| v.into_iter().map(|(t, p)| (t.tid, p as u32)).collect::<Vec<_>>();
        assert_eq!(names(stack_targets(&busy, &all, 2, &[])), [(3, 100), (4, 60)]);
        // A blocked main loop is read too, and a thread already listed is listed once.
        assert_eq!(names(stack_targets(&busy, &all, 2, &["qemu-main", "qemu-rcu"])), [(3, 100), (4, 60), (2, 0)]);
        assert_eq!(names(stack_targets(&[], &all, 2, &["qemu-main"])), [(2, 0)]);
        assert!(stack_targets(&[], &all, 2, &["absent"]).is_empty());
    }

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

    #[test]
    fn a_crash_header_needs_only_its_buffer_and_keeps_the_format_shown() {
        let mut out = Line::new();
        let r = Registers { pc: 0x1234, lr: 0xabc, fp: 0, x8: 0xdd, x: [0, 1, 2, u64::MAX] };
        crash_header(&mut out, "SIGSEGV", -6, b"CPU 0/TCG", 77, 0x10, &r);
        assert_eq!(
            String::from_utf8_lossy(out.bytes()),
            "CRASH SIGSEGV code -6 in thread CPU 0/TCG (tid 77) at fault address 0x10\n  pc 0x1234 lr 0xabc x8 0xdd\n  x0 0x0 x1 0x1 x2 0x2 x3 0xffffffffffffffff\n"
        );
    }

    #[test]
    fn a_full_buffer_cuts_the_text_short() {
        let mut out = Line::new();
        for _ in 0..40 {
            out.hex(u64::MAX).dec(i64::MIN);
        }
        assert_eq!(out.bytes().len(), 512);
    }

    #[test]
    fn the_memory_line_counts_mappings_against_their_limit_and_the_gpus_memory() {
        let status = "VmRSS:\t 5427200 kB\nRssAnon:\t 1843200 kB\nRssFile:\t 10240 kB\nRssShmem:\t 3573760 kB\n";
        let meminfo = "MemTotal: 15728640 kB\nMemAvailable:   1228800 kB\n";
        let maps = "7000000000-7000100000 rw-s 00000000 00:05 12 /dev/kgsl-3d0\n\
                    7000100000-7000104000 rw-p 00000000 00:00 0 [anon:scudo]\n\
                    7000200000-7000a00000 rw-s 00100000 00:05 12 /dev/kgsl-3d0\n";
        assert_eq!(maps_summary(maps), (3, 9 << 20));
        assert_eq!(
            resources_line(status, meminfo, maps, 65_530, 31),
            "rss 5300M (anon 1800M shm 3490M) gpu 9M avail 1200M disk 31G maps 3/65530"
        );
        assert_eq!(resources_line("", "", "", 65_530, 0), "rss 0M (anon 0M shm 0M) gpu 0M avail 0M disk 0G maps 0/65530");
    }

    #[test]
    fn startup_says_what_confines_the_process() {
        let line = startup_line("Name:\tx\nCpus_allowed_list:\t0-7\nVmRSS: 1 kB\n", "0::/uid_10123/pid_456\n1:cpu:/x\n", "905\n");
        assert_eq!(line, "startup cpus 0-7 cgroup 0::/uid_10123/pid_456; 1:cpu:/x oom_score_adj 905");
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    mod crashes {
        use super::super::*;
        use std::os::unix::process::ExitStatusExt;
        use std::process::Command;

        /// The body of the child process the tests below start: the handler, then a real fault.
        #[test]
        #[ignore = "run by the tests below, in a process of its own"]
        fn crash_child() {
            let Ok(log) = std::env::var("VPHONE_CRASH_LOG") else {
                return;
            };
            let out = File::create(log).unwrap();
            CRASH_FD.store(out.as_raw_fd(), Ordering::SeqCst);
            std::mem::forget(out);
            stack::install();
            stack::FAULT_IN_WALK.store(std::env::var_os("VPHONE_CRASH_TWICE").is_some(), Ordering::SeqCst);
            // SAFETY: not safe, and meant not to be: this process is here to crash.
            unsafe { std::ptr::write_volatile(0x10 as *mut u8, 1) };
        }

        fn crash(name: &str, twice: bool) -> (Option<i32>, String) {
            let log = std::env::temp_dir().join(format!("vphone-crash-{}-{name}", std::process::id()));
            let mut child = Command::new(std::env::current_exe().unwrap());
            child.args(["--ignored", "--exact", "create::vm::diag::tests::crashes::crash_child"]).env("VPHONE_CRASH_LOG", &log);
            if twice {
                child.env("VPHONE_CRASH_TWICE", "1");
            }
            let status = child.output().unwrap().status;
            let text = fs::read_to_string(&log).unwrap_or_default();
            let _ = fs::remove_file(&log);
            (status.signal(), text)
        }

        fn assert_header(text: &str) {
            assert!(text.starts_with("CRASH SIGSEGV code 1 in thread "), "{text}");
            assert!(text.contains("at fault address 0x10\n  pc 0x"), "{text}");
            assert!(text.contains(" x8 0x"), "{text}");
            assert!(text.contains("\n  x0 0x"), "{text}");
        }

        #[test]
        fn a_fault_is_recorded_and_ends_the_process_by_its_own_signal() {
            let (signal, text) = crash("once", false);
            assert_eq!(signal, Some(libc::SIGSEGV), "{text}");
            assert_header(&text);
            assert!(!text.contains("stack: "), "a live stack is never unwound: {text}");
        }

        #[test]
        fn a_second_fault_during_the_walk_still_leaves_the_header() {
            let (signal, text) = crash("twice", true);
            assert_eq!(signal, Some(libc::SIGSEGV), "{text}");
            assert_header(&text);
            assert!(!text.contains("stack: "), "the walk faulted: {text}");
        }
    }
}
