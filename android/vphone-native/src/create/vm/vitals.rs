//! How the VM is doing, once a second, for the UI and the stall watchdog. The QEMU process
//! writes its own counters (only it may read /proc/self/io on Android), and the `:create` side
//! adds what only it knows (the USB stage, the guest's vCPUs over QMP, the display).
//!
//! `guest_pc` and `pcs` come from QMP's `info registers -a`: TCG updates a running vCPU's PC at
//! its exits (timer interrupts and the like), so the view is coarse, but a live guest's PCs
//! change within seconds, and a spinning or wedged one's do not.

use crate::paths::Paths;
use crate::qmp::Vcpu;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often vCPU PCs are read (in ticks of the 1-second vitals loop): `info registers -a` is
/// cheap, but it is the only QMP traffic here and need not run every second.
pub const PC_SAMPLE_TICKS: u32 = 3;
/// The guest counts as moving this long after its PCs last changed.
const MOVING_WINDOW: Duration = Duration::from_secs(10);
/// Not moving for this long goes into the step's log (once, until it moves again).
const STOPPED_REPORT: Duration = Duration::from_secs(60);
/// How long the guest's PCs, its output, USB traffic and the host helpers' output must all be
/// silent before the step fails.
///
/// Live guests' PCs change within seconds (timer interrupts alone), so what this must outlast is
/// a guest that is honestly idle while the host works in silence. The longest such gap on the
/// desktop is about a minute (fetching the signing tickets, preparing the first image), and
/// TCG on a phone runs about ten times slower (`VPHONE_RESTORE_TIMEOUT_SCALE=10`): ten minutes.
/// The limit is half as long again; a false stall costs hours, a late one costs minutes.
pub const STALL_LIMIT: Duration = Duration::from_secs(15 * 60);

/// What the QEMU process knows of itself: one small file, written from its own process.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ProcStats {
    pub cpu_ticks: u64,
    pub rss_kb: u64,
    pub write_bytes: u64,
    /// Set when `/proc/self/io` could be read.
    pub has_io: bool,
    pub ms: u64,
}

pub fn stats_path(paths: &Paths) -> PathBuf {
    paths.create_sockets().join("vm-stats.json")
}

pub fn proc_path(paths: &Paths) -> PathBuf {
    paths.create_sockets().join("vm-proc.json")
}

/// `/proc/self/stat`'s utime + stime, from the text after the command name's closing bracket.
pub fn parse_cpu_ticks(stat: &str) -> Option<u64> {
    let rest = stat.rsplit(')').next()?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

pub fn parse_rss_kb(status: &str) -> Option<u64> {
    let line = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?;
    line.split_whitespace().next()?.parse().ok()
}

pub fn parse_write_bytes(io: &str) -> Option<u64> {
    io.lines()
        .find_map(|l| l.strip_prefix("write_bytes:"))?
        .trim()
        .parse()
        .ok()
}

/// Reads this process's counters.
pub fn read_self() -> ProcStats {
    let io = fs::read_to_string("/proc/self/io").ok();
    ProcStats {
        cpu_ticks: fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| parse_cpu_ticks(&s))
            .unwrap_or(0),
        rss_kb: fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| parse_rss_kb(&s))
            .unwrap_or(0),
        write_bytes: io.as_deref().and_then(parse_write_bytes).unwrap_or(0),
        has_io: io.is_some(),
        ms: super::super::state::now_ms(),
    }
}

/// The `:createvm` side: its own counters, every second, while it lives.
pub fn spawn_self_stats(path: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("vm-self-stats".into())
        .spawn(move || loop {
            if let Ok(json) = serde_json::to_vec(&read_self()) {
                let tmp = path.with_extension("new");
                if fs::write(&tmp, json).is_ok() {
                    let _ = fs::rename(&tmp, &path);
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        });
}

/// `05ac:12a8` from vusbd's `USB device discovered: 05ac:12a8, ...` lines, with what it means.
pub fn usb_stage(broker_log_tail: &str) -> String {
    let Some(line) = broker_log_tail
        .lines()
        .rev()
        .find(|l| l.contains("USB device discovered:"))
    else {
        return String::new();
    };
    let id = line
        .split("USB device discovered:")
        .nth(1)
        .and_then(|t| t.split(',').next())
        .map_or("", str::trim);
    let what = match id.rsplit(':').next().unwrap_or("") {
        "1227" => "DFU",
        "1281" => "recovery / iBoot",
        "12ac" => "restore system",
        "12a8" => "booted system",
        _ => "USB device",
    };
    format!("{what} ({id})")
}

/// Everything the vitals file holds.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Vitals {
    pub alive: bool,
    pub paused: bool,
    pub cpu_percent: f64,
    pub rss_mb: u64,
    pub disk_written_mb: u64,
    pub usb_stage: String,
    pub guest_el: String,
    pub guest_pc: String,
    pub guest_moving: bool,
    pub pcs: Vec<String>,
    pub display: String,
    /// Why there is no display, when the boot has none.
    pub display_note: String,
    pub updated_ms: u64,
}

/// CPU use between two samples of the QEMU process, 100 = one core flat out.
pub fn cpu_percent(old: &ProcStats, new: &ProcStats, ticks_per_sec: u64) -> f64 {
    if new.ms <= old.ms || new.cpu_ticks < old.cpu_ticks || ticks_per_sec == 0 {
        return 0.0;
    }
    let secs = (new.ms - old.ms) as f64 / 1000.0;
    (new.cpu_ticks - old.cpu_ticks) as f64 / ticks_per_sec as f64 / secs * 100.0
}

fn hex_pc(vcpu: &Vcpu) -> String {
    format!("{:#x}", vcpu.pc)
}

/// What the watchdog and the log make of a sample.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    StoppedMoving,
    MovingAgain,
}

/// The rolling picture of the guest: when its PCs last changed, when anything else last happened.
pub struct Watch {
    pid: Option<i32>,
    pcs: Option<Vec<Vcpu>>,
    moved_at: Instant,
    activity_at: Instant,
    signature: u64,
    stopped_reported: bool,
    now: Instant,
}

impl Watch {
    pub fn new(now: Instant) -> Watch {
        Watch {
            pid: None,
            pcs: None,
            moved_at: now,
            activity_at: now,
            signature: 0,
            stopped_reported: false,
            now,
        }
    }

    pub fn set_now(&mut self, now: Instant) {
        self.now = now;
    }

    /// A new VM process starts the clocks over.
    pub fn boot(&mut self, pid: Option<i32>) {
        if self.pid != pid {
            self.pid = pid;
            self.pcs = None;
            self.moved_at = self.now;
            self.activity_at = self.now;
            self.stopped_reported = false;
        }
    }

    /// While paused nothing runs, so nothing here may count. A resume starts the clocks over.
    pub fn paused(&mut self) {
        self.moved_at = self.now;
        self.activity_at = self.now;
        self.stopped_reported = false;
    }

    /// The combined size of the logs that show the guest, USB and the helpers working.
    pub fn observe_output(&mut self, signature: u64) {
        if signature != self.signature {
            self.signature = signature;
            self.activity_at = self.now;
        }
    }

    pub fn observe_pcs(&mut self, vcpus: Vec<Vcpu>) -> Option<Event> {
        let moved = self.pcs.as_ref().is_none_or(|old| {
            old.len() != vcpus.len() || old.iter().zip(&vcpus).any(|(a, b)| a.pc != b.pc || a.el != b.el)
        });
        self.pcs = Some(vcpus);
        if moved {
            self.moved_at = self.now;
            if self.stopped_reported {
                self.stopped_reported = false;
                return Some(Event::MovingAgain);
            }
            return None;
        }
        if !self.stopped_reported && self.now.duration_since(self.moved_at) >= STOPPED_REPORT {
            self.stopped_reported = true;
            return Some(Event::StoppedMoving);
        }
        None
    }

    pub fn moving(&self) -> bool {
        self.now.duration_since(self.moved_at) < MOVING_WINDOW
    }

    pub fn vcpus(&self) -> &[Vcpu] {
        self.pcs.as_deref().unwrap_or(&[])
    }

    /// `Some` once PCs and everything else have been still for `limit`.
    pub fn stalled_for(&self, limit: Duration) -> Option<Duration> {
        let still = self.now.duration_since(self.moved_at.max(self.activity_at));
        (still >= limit).then_some(still)
    }

    /// The facts of a stall: where every vCPU is, the USB stage, and what the guest last said.
    pub fn describe_stall(&self, still: Duration, usb_stage: &str, last_output: &str) -> String {
        let cpus: Vec<String> = self
            .vcpus()
            .iter()
            .map(|c| format!("  vCPU {} at EL{} pc {}", c.index, c.el, hex_pc(c)))
            .collect();
        format!(
            "The VM made no progress for {} minutes and was stopped.\n\
             No output from the guest, no USB traffic, and no vCPU moved.\n{}\nUSB: {}\nLast guest output:\n{}",
            still.as_secs() / 60,
            if cpus.is_empty() {
                "QEMU's monitor did not answer, so the vCPUs could not be read.".to_string()
            } else {
                cpus.join("\n")
            },
            if usb_stage.is_empty() { "no device yet" } else { usb_stage },
            if last_output.trim().is_empty() { "(none)" } else { last_output.trim() },
        )
    }
}

/// Fills the vCPU fields of `vitals` from the latest sample.
pub fn fill_guest(vitals: &mut Vitals, vcpus: &[Vcpu], moving: bool) {
    vitals.guest_moving = moving;
    vitals.pcs = vcpus.iter().map(hex_pc).collect();
    if let Some(cpu) = vcpus.first() {
        vitals.guest_pc = hex_pc(cpu);
        vitals.guest_el = format!("EL{}", cpu.el);
    }
}

pub fn write(path: &Path, vitals: &Vitals) {
    if let Ok(json) = serde_json::to_vec(vitals) {
        let tmp = path.with_extension("new");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu(index: u32, pc: u64, el: u8) -> Vcpu {
        Vcpu { index, pc, el }
    }

    #[test]
    fn reads_the_process_s_counters() {
        // A command name with spaces and a bracket, as /proc allows.
        let stat = "1234 (qemu main) x) S 1 2 3 4 5 6 7 8 9 10 20 3 4 5 6 7 8 9";
        assert_eq!(parse_cpu_ticks(stat), Some(23));
        assert_eq!(parse_rss_kb("Name:\tx\nVmRSS:\t  12345 kB\n"), Some(12345));
        assert_eq!(parse_write_bytes("rchar: 1\nwrite_bytes: 4096\ncancelled_write_bytes: 0\n"), Some(4096));
        assert!(read_self().cpu_ticks > 0 || read_self().rss_kb > 0);
    }

    #[test]
    fn names_the_usb_stage() {
        assert_eq!(
            usb_stage("vusbd: USB device discovered: 05ac:1227, 1 configuration(s), active 1\nvusbd: sent 3 MiB in 1 s\n"),
            "DFU (05ac:1227)"
        );
        assert_eq!(
            usb_stage("x\nvusbd: USB device discovered: 05ac:1227, 1\nvusbd: USB device discovered: 05ac:12ac, 2\n"),
            "restore system (05ac:12ac)",
            "the last one"
        );
        assert_eq!(usb_stage(""), "");
    }

    #[test]
    fn computes_cpu_percent() {
        let a = ProcStats { cpu_ticks: 100, ms: 1000, ..Default::default() };
        let b = ProcStats { cpu_ticks: 800, ms: 2000, ..Default::default() };
        assert_eq!(cpu_percent(&a, &b, 100), 700.0, "seven cores flat out");
        assert_eq!(cpu_percent(&b, &a, 100), 0.0);
    }

    #[test]
    fn a_moving_guest_is_not_stalled_and_a_frozen_one_is() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        w.boot(Some(1));
        w.observe_pcs(vec![cpu(0, 0x100, 1), cpu(1, 0x200, 0)]);
        for s in 1..=40u64 {
            w.set_now(t0 + Duration::from_secs(s * 30));
            w.observe_pcs(vec![cpu(0, 0x100 + s, 1), cpu(1, 0x200, 0)]);
            w.observe_output(0);
        }
        assert!(w.moving());
        assert_eq!(w.stalled_for(STALL_LIMIT), None, "any vCPU changing counts");

        let frozen = t0 + Duration::from_secs(1200);
        let pcs = || vec![cpu(0, 0x128, 1), cpu(1, 0x200, 0)];
        w.observe_pcs(pcs()); // now equal for the rest of the test
        let mut report = None;
        for s in 1..=20 * 60u64 {
            w.set_now(frozen + Duration::from_secs(s));
            match w.observe_pcs(pcs()) {
                Some(e) => {
                    assert_eq!(e, Event::StoppedMoving);
                    report.get_or_insert(s);
                }
                None => {}
            }
        }
        assert_eq!(report, Some(60), "reported to the log once, after a minute");
        assert!(!w.moving());
        let still = w.stalled_for(STALL_LIMIT).expect("a frozen guest stalls");
        let facts = w.describe_stall(still, "DFU (05ac:1227)", "guest said this\n");
        for needle in ["vCPU 0 at EL1 pc 0x128", "DFU (05ac:1227)", "guest said this"] {
            assert!(facts.contains(needle), "{facts}");
        }
    }

    #[test]
    fn output_alone_holds_off_a_stall() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        w.boot(Some(1));
        for s in 0..20 * 60u64 {
            w.set_now(t0 + Duration::from_secs(s));
            w.observe_pcs(vec![cpu(0, 0x100, 1)]);
            w.observe_output(s);
        }
        assert_eq!(w.stalled_for(STALL_LIMIT), None);
    }

    #[test]
    fn a_new_boot_and_a_pause_start_the_clocks_over() {
        let t0 = Instant::now();
        let mut w = Watch::new(t0);
        w.boot(Some(1));
        w.observe_pcs(vec![cpu(0, 0x100, 1)]);
        w.set_now(t0 + STALL_LIMIT + Duration::from_secs(1));
        w.observe_pcs(vec![cpu(0, 0x100, 1)]);
        assert!(w.stalled_for(STALL_LIMIT).is_some());
        w.paused();
        assert_eq!(w.stalled_for(STALL_LIMIT), None, "a pause is not a stall");
        w.set_now(t0 + STALL_LIMIT * 3);
        w.observe_pcs(vec![cpu(0, 0x100, 1)]);
        assert!(w.stalled_for(STALL_LIMIT).is_some());
        w.boot(Some(2));
        assert_eq!(w.stalled_for(STALL_LIMIT), None, "another VM process is another boot");
    }

    #[test]
    fn says_so_when_the_monitor_never_answered() {
        let w = Watch::new(Instant::now());
        let text = w.describe_stall(STALL_LIMIT, "", "");
        assert!(text.contains("did not answer"), "{text}");
        assert!(text.contains("no device yet"));
    }
}
