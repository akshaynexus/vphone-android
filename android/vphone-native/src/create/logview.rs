//! Reading a step's log for the screen: named stages from the markers real tools print, a view
//! of a log of any size, and the facts the progress screen shows in place of guesses.
//!
//! The log files hold no times, so while a creation runs a [`Stamper`] notes how far each one had
//! grown and when, in `<log>.stamps`. Everything here is derived from those two files, so the UI
//! process needs nothing from the process doing the work.

use super::engine::free_bytes;
use super::mirror;
use super::state::{now_ms, Status};
use super::steps::STEPS;
use crate::meminfo::MemInfo;
use crate::paths::Paths;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A line-offset is kept for every this many lines; a line is found from the nearest one.
const BLOCK: u32 = 32;
const CHUNK: usize = 1 << 18;
/// How much of one line is kept for classifying, searching and showing.
const PEEK: usize = 8192;
const SHOW_CHARS: usize = 2000;
/// A guest in a reboot loop must not grow the list of stages without limit.
const MAX_STAGES: usize = 4000;
const PAGE_MAX: u32 = 512;
/// How long a tool may be silent before the screen points it out. Nothing is concluded from it.
const SILENT_WARN_SECS: u64 = 300;
const GUEST_SILENT_WARN_SECS: u64 = 600;
/// How old `vm-stats.json` may be and still count as live: it is rewritten about once a second.
const STATS_STALE_SECS: u64 = 10;
const COPY_CAP: usize = 128 << 10;
const EXPORT_CAP: usize = 256 << 20;
const STATS_FILE: &str = "vm-stats.json";

pub const SRC_TOOL: u8 = 0;
pub const SRC_GUEST: u8 = 1;
pub const SRC_USB: u8 = 2;
const SRC_MASK: u8 = 3;
const ERROR_BIT: u8 = 4;

pub const FILTER_ALL: i32 = 0;
pub const FILTER_TOOL: i32 = 1;
pub const FILTER_VM: i32 = 2;
pub const FILTER_USB: i32 = 3;
pub const FILTER_ERRORS: i32 = 4;

/// Who wrote a line, and the line without the mark the step log gave it.
pub fn split_source(line: &str) -> (u8, &str) {
    if let Some(body) = line.strip_prefix("[guest] ").or_else(|| line.strip_prefix("[vm] ")) {
        (SRC_GUEST, body)
    } else if let Some(body) = line.strip_prefix("[usb] ") {
        (SRC_USB, body)
    } else {
        (SRC_TOOL, line)
    }
}

/// Whether a line reports something going wrong.
pub fn is_error(body: &str) -> bool {
    const SAYS: [&str; 9] =
        ["error", "fail", "panic", "fatal", "unable to", "cannot", "timed out", "refused", "denied"];
    const BUT_NOT: [&str; 7] =
        ["no error", "0 error", "error: 0", "error=0", "err=0", "failsafe", "fail-safe"];
    let lower = body.to_ascii_lowercase();
    SAYS.iter().any(|s| lower.contains(s)) && !BUT_NOT.iter().any(|s| lower.contains(s))
}

/// A line as it should be read: a progress bar's last redraw, no escape sequences.
pub fn clean(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\r', '\n']);
    let text = text.rsplit('\r').next().unwrap_or("");
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `Sending RestoreRamDisk (232789583 bytes)...`: what is being sent, and its size.
pub fn sending_of(body: &str) -> Option<(String, u64)> {
    let rest = body.strip_prefix("Sending ")?;
    let (name, size) = rest.rsplit_once(" (")?;
    let (bytes, unit) = size.split_once(' ')?;
    if !unit.starts_with("bytes") {
        return None;
    }
    Some((name.trim_end_matches(" now").to_string(), bytes.parse().ok()?))
}

/// `Mounting filesystems (16)`: a step restored reports, with its operation number.
fn restored_operation(body: &str) -> Option<&str> {
    let (name, code) = body.strip_suffix(')')?.rsplit_once(" (")?;
    let first_upper = name.chars().next()?.is_ascii_uppercase();
    let plain = name.chars().all(|c| c.is_alphanumeric() || c == ' ');
    let numbered = !code.is_empty() && code.len() <= 3 && code.bytes().all(|b| b.is_ascii_digit());
    (first_upper && plain && numbered && !name.starts_with("Unknown")).then_some(name)
}

/// What the VM's serial output says it is doing. The first four are in the logs of real restores;
/// the kernel's are XNU's own messages, which appear once a boot has the serial console on.
const GUEST_MARKS: [(&str, &str); 7] = [
    ("OpenBootROM:", "VM boot ROM"),
    ("======== Start of iBoot", "VM iBoot"),
    ("Entering recovery mode", "VM iBoot recovery mode"),
    ("======== End of iBoot", "VM iBoot finished; no VM output until the next stage prints"),
    ("Darwin Kernel Version", "VM kernel started"),
    ("BSD root:", "VM kernel mounting its root"),
    ("Still waiting for root device", "VM kernel waiting for its root device"),
];

#[derive(Debug, PartialEq, Clone)]
pub struct Mark {
    pub title: String,
    /// 0 for the tool's own stages, 1 for finer detail inside one.
    pub level: u8,
    pub kind: &'static str,
    /// A `▶` line: the step started again in this log.
    pub attempt: bool,
}

/// Finds where a new stage begins, line by line.
#[derive(Default)]
pub struct Tracker {
    restore_rank: u64,
    attempts: u32,
    guest: Option<&'static str>,
}

impl Tracker {
    pub fn mark(&mut self, source: u8, body: &str) -> Option<Mark> {
        match source {
            SRC_GUEST => {
                let (needle, title) = GUEST_MARKS.iter().find(|(n, _)| body.contains(n))?;
                if self.guest == Some(*needle) {
                    return None;
                }
                self.guest = Some(*needle);
                Some(Mark { title: (*title).to_string(), level: 1, kind: "vm", attempt: false })
            }
            SRC_TOOL => self.tool_mark(body),
            _ => None,
        }
    }

    fn tool_mark(&mut self, body: &str) -> Option<Mark> {
        let plain = |title: String, kind, level| Some(Mark { title, level, kind, attempt: false });
        if let Some(step) = body.strip_prefix("▶ ") {
            self.attempts += 1;
            self.restore_rank = 0;
            self.guest = None;
            let title = if self.attempts > 1 {
                format!("Attempt {}: {step}", self.attempts)
            } else {
                format!("Step {step}")
            };
            return Some(Mark { title, level: 0, kind: "step", attempt: true });
        }
        if let Some(text) = body.strip_prefix("=== ") {
            self.restore_rank = 0;
            self.guest = None;
            let title = if text == "DFU" { "Start the VM in DFU mode" } else { text };
            return plain(title.to_string(), "tool", 0);
        }
        if let Some((rank, title)) = mirror::stage_of(body) {
            if rank > self.restore_rank {
                self.restore_rank = rank;
                return plain(title.to_string(), "restore", 0);
            }
        }
        if let Some(status) = body.strip_prefix("Status: ") {
            return plain(status.to_string(), "status", 0);
        }
        if let Some((name, bytes)) = sending_of(body) {
            return plain(format!("Sending {name} ({})", size_text(bytes)), "restore", 1);
        }
        restored_operation(body).and_then(|name| plain(name.to_string(), "restore", 1))
    }
}

pub fn size_text(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.1} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.0} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

pub fn duration_text(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

/// How `<log>.stamps` is named: beside the log, one `<bytes> <ms>` line per growth.
fn stamps_path(log: &Path) -> PathBuf {
    let mut name = log.as_os_str().to_os_string();
    name.push(".stamps");
    PathBuf::from(name)
}

/// Notes when each log grew, every half second, until dropped.
pub struct Stamper {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stamper {
    pub fn spawn(dir: PathBuf) -> Stamper {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("create-stamps".into())
            .spawn(move || {
                let mut seen = HashMap::new();
                while !flag.load(Ordering::SeqCst) {
                    stamp_once(&dir, &mut seen);
                    std::thread::sleep(Duration::from_millis(500));
                }
                stamp_once(&dir, &mut seen);
            })
            .ok();
        Stamper { stop, thread }
    }
}

impl Drop for Stamper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn append_stamp(path: &Path, offset: u64, ms: u64) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{offset} {ms}");
    }
}

fn last_stamped(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| t.lines().last()?.split(' ').next()?.parse().ok())
        .unwrap_or(0)
}

fn stamp_once(dir: &Path, seen: &mut HashMap<PathBuf, u64>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "log") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let size = meta.len();
        let stamps = stamps_path(&path);
        let last = match seen.get(&path) {
            Some(last) => *last,
            None => {
                // What a log already holds was written before anyone was timing it.
                let stamped = last_stamped(&stamps);
                if size > stamped {
                    append_stamp(&stamps, size, 0);
                }
                seen.insert(path.clone(), size);
                size
            }
        };
        if size < last {
            let _ = File::create(&stamps);
            seen.insert(path, size);
        } else if size > last {
            append_stamp(&stamps, size, now_ms());
            seen.insert(path, size);
        }
    }
}

#[derive(Default)]
struct Stamps {
    entries: Vec<(u64, u64)>,
    read: u64,
    partial: Vec<u8>,
}

impl Stamps {
    fn refresh(&mut self, path: &Path) {
        let Ok(file) = File::open(path) else {
            return;
        };
        let len = file.metadata().map_or(0, |m| m.len());
        if len < self.read {
            *self = Stamps::default();
        }
        let mut buf = vec![0u8; (len - self.read).min(1 << 20) as usize];
        let Ok(n) = file.read_at(&mut buf, self.read) else {
            return;
        };
        self.read += n as u64;
        self.partial.extend_from_slice(&buf[..n]);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return;
        };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        for line in String::from_utf8_lossy(&complete).lines() {
            if let Some((o, m)) = line.split_once(' ') {
                if let (Ok(o), Ok(m)) = (o.parse(), m.parse()) {
                    self.entries.push((o, m));
                }
            }
        }
    }

    /// When the byte at `offset` was written; `None` when that was not recorded.
    fn time_at(&self, offset: u64) -> Option<u64> {
        let i = self.entries.partition_point(|e| e.0 <= offset);
        self.entries.get(i).map(|e| e.1).filter(|ms| *ms != 0)
    }

    /// Whether `offset` is past everything stamped, so it was written moments ago.
    fn is_recent(&self, offset: u64) -> bool {
        self.entries.last().is_some_and(|e| e.0 <= offset)
    }
}

/// How a stage ended, when the log says so.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ended {
    Failed,
    Stopped,
}

#[derive(Clone, Debug)]
pub struct Stage {
    pub title: String,
    pub level: u8,
    pub kind: &'static str,
    pub attempt: bool,
    pub first_line: u32,
    pub first_offset: u64,
    pub ended: Option<Ended>,
    pub errors: u32,
}

/// One log file, indexed as far as it has been read.
pub struct LogIndex {
    path: PathBuf,
    stamps_path: PathBuf,
    file: Option<File>,
    ino: u64,
    stamps: Stamps,
    scanned: u64,
    line_start: u64,
    carry: Vec<u8>,
    blocks: Vec<u64>,
    classes: Vec<u8>,
    tracker: Tracker,
    stages: Vec<Stage>,
    mtime_ms: u64,
    last_guest: Option<u64>,
    sent: Option<(u64, f64)>,
    sending: Option<(String, u64)>,
    /// Bumped when the file was replaced or cut short and reading began again.
    resets: u64,
}

impl LogIndex {
    pub fn new(path: &Path) -> LogIndex {
        LogIndex {
            path: path.to_path_buf(),
            stamps_path: stamps_path(path),
            file: None,
            ino: 0,
            stamps: Stamps::default(),
            scanned: 0,
            line_start: 0,
            carry: Vec::new(),
            blocks: Vec::new(),
            classes: Vec::new(),
            tracker: Tracker::default(),
            stages: Vec::new(),
            mtime_ms: 0,
            last_guest: None,
            sent: None,
            sending: None,
            resets: 0,
        }
    }

    fn start_over(&mut self) {
        let resets = self.resets + 1;
        *self = LogIndex::new(&self.path.clone());
        self.resets = resets;
    }

    fn lines(&self) -> u32 {
        self.classes.len() as u32
    }

    fn push_line(&mut self, start: u64, raw: &[u8]) {
        let n = self.classes.len() as u32;
        if n % BLOCK == 0 {
            self.blocks.push(start);
        }
        let text = clean(raw);
        let (source, body) = split_source(&text);
        let error = is_error(body);
        self.classes.push(source | if error { ERROR_BIT } else { 0 });
        match source {
            SRC_GUEST => self.last_guest = Some(start),
            SRC_USB => {
                if let Some(sent) = sent_line(body) {
                    self.sent = Some(sent);
                }
            }
            _ => self.tool_line(body),
        }
        let mark = self.tracker.mark(source, body);
        if self.stages.is_empty() && mark.is_none() {
            self.add_stage(start, n, "Start of the log".into(), 0, "tool", false);
        }
        if let Some(mark) = mark {
            let same_as_last = self.stages.last().is_some_and(|s| s.title == mark.title);
            if !same_as_last && self.stages.len() < MAX_STAGES {
                if mark.attempt {
                    if let Some(last) = self.stages.last_mut() {
                        if last.ended.is_none() && last.kind != "step" {
                            last.ended = Some(Ended::Stopped);
                        }
                    }
                }
                self.add_stage(start, n, mark.title, mark.level, mark.kind, mark.attempt);
            }
        }
        if error {
            if let Some(last) = self.stages.last_mut() {
                last.errors += 1;
            }
        }
    }

    fn tool_line(&mut self, body: &str) {
        if let Some(sending) = sending_of(body) {
            self.sending = Some(sending);
        }
        if let Some(rest) = body.strip_prefix("step ") {
            let ended = if rest.ends_with(" failed") {
                Some(Ended::Failed)
            } else if rest.ends_with(" cancelled") || rest.contains(" stopped after a pause") {
                Some(Ended::Stopped)
            } else {
                None
            };
            if let (Some(ended), Some(last)) = (ended, self.stages.last_mut()) {
                last.ended = Some(ended);
            }
        }
        if body.starts_with("▶ ") {
            self.sent = None;
            self.sending = None;
            self.last_guest = None;
        }
    }

    fn add_stage(&mut self, offset: u64, line: u32, title: String, level: u8, kind: &'static str, attempt: bool) {
        self.stages.push(Stage {
            title,
            level,
            kind,
            attempt,
            first_line: line,
            first_offset: offset,
            ended: None,
            errors: 0,
        });
    }

    fn open_current(&mut self) -> bool {
        let Ok(meta) = fs::metadata(&self.path) else {
            return self.file.is_some();
        };
        if self.file.is_some() && meta.ino() != self.ino {
            self.start_over();
        }
        if meta.len() < self.scanned {
            self.start_over();
        }
        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(f) => {
                    self.ino = meta.ino();
                    self.file = Some(f);
                }
                Err(_) => return false,
            }
        }
        self.mtime_ms = meta.mtime() as u64 * 1000 + meta.mtime_nsec() as u64 / 1_000_000;
        true
    }

    /// Reads what is new, for at most `budget`. True when there is more to read.
    pub fn refresh(&mut self, budget: Duration) -> bool {
        self.stamps.refresh(&self.stamps_path);
        if !self.open_current() {
            return false;
        }
        let started = Instant::now();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let Some(file) = self.file.as_ref() else {
                return false;
            };
            let Ok(n) = file.read_at(&mut buf, self.scanned) else {
                return false;
            };
            if n == 0 {
                return false;
            }
            self.consume(&buf[..n]);
            if started.elapsed() >= budget {
                return true;
            }
        }
    }

    fn consume(&mut self, data: &[u8]) {
        let base = self.scanned;
        let mut start = 0;
        while let Some(i) = data[start..].iter().position(|b| *b == b'\n') {
            let end = start + i;
            if self.carry.is_empty() {
                let upto = end.min(start + PEEK);
                self.push_line(self.line_start, &data[start..upto]);
            } else {
                let room = PEEK.saturating_sub(self.carry.len());
                let piece = &data[start..end];
                self.carry.extend_from_slice(&piece[..piece.len().min(room)]);
                let line = std::mem::take(&mut self.carry);
                self.push_line(self.line_start, &line);
            }
            start = end + 1;
            self.line_start = base + start as u64;
        }
        let rest = &data[start..];
        let room = PEEK.saturating_sub(self.carry.len());
        self.carry.extend_from_slice(&rest[..rest.len().min(room)]);
        // A line longer than the peek keeps something in `carry`, so its end joins it.
        if self.carry.is_empty() && !rest.is_empty() {
            self.carry.push(rest[0]);
        }
        self.scanned = base + data.len() as u64;
    }

    fn time_of(&self, offset: u64) -> Option<u64> {
        self.stamps.time_at(offset).or_else(|| {
            (self.stamps.is_recent(offset) && self.mtime_ms != 0).then_some(self.mtime_ms)
        })
    }

    fn last_line_ms(&self) -> Option<u64> {
        (self.mtime_ms != 0 && self.lines() > 0).then_some(self.mtime_ms)
    }

    fn stage_start(&self, stage: &Stage) -> Option<u64> {
        self.time_of(stage.first_offset)
    }
}

fn sent_line(body: &str) -> Option<(u64, f64)> {
    let mib = mirror::sent_mib(body)?;
    let secs = body.split(" in ").nth(1)?.split(' ').next()?.parse().unwrap_or(0.0);
    Some((mib, secs))
}

struct Block {
    index: u32,
    lines: Vec<Vec<u8>>,
}

#[derive(PartialEq, Clone, Copy, Debug)]
struct RowStage {
    stage: u32,
    first_row: u32,
    /// Lines shown, not counting the stage's own row.
    shown: u32,
    /// Lines belonging to the stage under the current filter.
    total: u32,
    /// Where this stage starts in `matches`, when a filter is on.
    first_match: u32,
}

/// What the log screen is showing of one log: a filter, a search, and which stages are open.
pub struct LogView {
    index: LogIndex,
    filter: i32,
    search: String,
    matches: Option<Vec<u32>>,
    match_cursor: u32,
    opened: HashMap<u32, bool>,
    all_open: Option<bool>,
    layout: Vec<RowStage>,
    rows: u32,
    generation: u64,
    block: Option<Block>,
    run_state: String,
    paused: bool,
    known_resets: u64,
}

impl LogView {
    pub fn new(path: &Path) -> LogView {
        LogView {
            index: LogIndex::new(path),
            filter: FILTER_ALL,
            search: String::new(),
            matches: None,
            match_cursor: 0,
            opened: HashMap::new(),
            all_open: None,
            layout: Vec::new(),
            rows: 0,
            generation: 0,
            block: None,
            run_state: String::new(),
            paused: false,
            known_resets: 0,
        }
    }

    fn filtered(&self) -> bool {
        self.filter != FILTER_ALL || !self.search.is_empty()
    }

    fn wanted(&self, class: u8) -> bool {
        match self.filter {
            FILTER_TOOL => class & SRC_MASK == SRC_TOOL,
            FILTER_VM => class & SRC_MASK == SRC_GUEST,
            FILTER_USB => class & SRC_MASK == SRC_USB,
            FILTER_ERRORS => class & ERROR_BIT != 0,
            _ => true,
        }
    }

    pub fn set_filter(&mut self, filter: i32, search: &str) {
        self.filter = filter;
        self.search = search.trim().to_lowercase();
        self.matches = self.filtered().then(Vec::new);
        self.match_cursor = 0;
        self.generation += 1;
        self.rebuild();
    }

    pub fn toggle(&mut self, stage: u32) {
        let open = self.is_open(stage);
        self.opened.insert(stage, !open);
        self.generation += 1;
        self.rebuild();
    }

    pub fn open_all(&mut self, open: bool) {
        self.opened.clear();
        self.all_open = Some(open);
        self.generation += 1;
        self.rebuild();
    }

    fn is_open(&self, stage: u32) -> bool {
        if let Some(open) = self.opened.get(&stage) {
            return *open;
        }
        if let Some(open) = self.all_open {
            return open;
        }
        // Finished stages fold away unless a filter is looking for lines in them.
        self.filtered() || stage as usize + 1 == self.index.stages.len()
    }

    /// Reads, indexes and searches for at most `budget`; true when there is more to do.
    pub fn sync(&mut self, run_state: &str, paused: bool, budget: Duration) -> bool {
        self.run_state = run_state.to_string();
        self.paused = paused;
        let more = self.index.refresh(budget);
        if self.index.resets != self.known_resets {
            self.known_resets = self.index.resets;
            self.opened.clear();
            self.block = None;
            self.matches = self.filtered().then(Vec::new);
            self.match_cursor = 0;
            self.generation += 1;
        }
        let searching = self.advance_matches(budget);
        self.rebuild();
        more || searching
    }

    fn advance_matches(&mut self, budget: Duration) -> bool {
        if self.matches.is_none() {
            return false;
        }
        let started = Instant::now();
        let total = self.index.lines();
        let needle = self.search.clone();
        while self.match_cursor < total {
            let line = self.match_cursor;
            let hit = self.wanted(self.index.classes[line as usize])
                && (needle.is_empty()
                    || self.read_line(line).is_some_and(|raw| {
                        clean(raw).to_lowercase().contains(&needle)
                    }));
            if hit {
                if let Some(m) = self.matches.as_mut() {
                    m.push(line);
                }
            }
            self.match_cursor += 1;
            if self.match_cursor % 256 == 0 && started.elapsed() >= budget {
                break;
            }
        }
        self.match_cursor < total
    }

    fn rebuild(&mut self) {
        let lines = self.index.lines();
        let count = self.index.stages.len();
        let mut layout = Vec::with_capacity(count);
        let mut row = 0u32;
        for i in 0..count {
            let first = self.index.stages[i].first_line;
            let end = self.index.stages.get(i + 1).map_or(lines, |s| s.first_line);
            let (total, first_match) = match &self.matches {
                Some(m) => {
                    let a = m.partition_point(|l| *l < first);
                    let b = m.partition_point(|l| *l < end);
                    ((b - a) as u32, a as u32)
                }
                None => (end - first, 0),
            };
            if self.filtered() && total == 0 {
                continue;
            }
            let shown = if self.is_open(i as u32) { total } else { 0 };
            layout.push(RowStage { stage: i as u32, first_row: row, shown, total, first_match });
            row += 1 + shown;
        }
        let only_grew = self.layout.len() <= layout.len()
            && self.layout.iter().zip(&layout).enumerate().all(|(i, (a, b))| {
                let last = i + 1 == self.layout.len();
                (a.stage, a.first_row, a.first_match) == (b.stage, b.first_row, b.first_match)
                    && if last { a.shown <= b.shown } else { a.shown == b.shown }
            });
        if !only_grew {
            self.generation += 1;
        }
        self.layout = layout;
        self.rows = row;
    }

    fn read_line(&mut self, line: u32) -> Option<&[u8]> {
        let b = line / BLOCK;
        let want = (line % BLOCK) as usize;
        if self.block.as_ref().is_none_or(|k| k.index != b || k.lines.len() <= want) {
            self.block = Some(Block { index: b, lines: self.load_block(b)? });
        }
        self.block.as_ref()?.lines.get(want).map(|l| l.as_slice())
    }

    fn load_block(&self, b: u32) -> Option<Vec<Vec<u8>>> {
        let file = self.index.file.as_ref()?;
        let mut pos = *self.index.blocks.get(b as usize)?;
        let count = (self.index.lines() - b * BLOCK).min(BLOCK) as usize;
        let mut lines = Vec::with_capacity(count);
        let mut current = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        while lines.len() < count {
            let n = file.read_at(&mut buf, pos).ok()?;
            if n == 0 {
                break;
            }
            let mut start = 0;
            while lines.len() < count {
                let Some(i) = buf[start..n].iter().position(|c| *c == b'\n') else {
                    break;
                };
                let piece = &buf[start..start + i];
                let room = PEEK.saturating_sub(current.len());
                current.extend_from_slice(&piece[..piece.len().min(room)]);
                lines.push(std::mem::take(&mut current));
                start += i + 1;
            }
            if lines.len() < count {
                let rest = &buf[start..n];
                let room = PEEK.saturating_sub(current.len());
                current.extend_from_slice(&rest[..rest.len().min(room)]);
            }
            pos += n as u64;
        }
        Some(lines)
    }

    fn line_of(&self, entry: &RowStage, k: u32) -> Option<u32> {
        match &self.matches {
            Some(m) => m.get((entry.first_match + k) as usize).copied(),
            None => Some(self.index.stages[entry.stage as usize].first_line + k),
        }
    }

    fn stage_state(&self, i: usize) -> &'static str {
        let stage = &self.index.stages[i];
        match stage.ended {
            Some(Ended::Failed) => return "failed",
            Some(Ended::Stopped) => return "stopped",
            None => {}
        }
        if i + 1 < self.index.stages.len() {
            return "done";
        }
        match self.run_state.as_str() {
            "running" if self.paused => "paused",
            "running" => "running",
            "failed" => "failed",
            "done" => "done",
            _ => "stopped",
        }
    }

    fn header_json(&self, entry: &RowStage, now: u64) -> Value {
        let i = entry.stage as usize;
        let stage = &self.index.stages[i];
        let start = self.index.stage_start(stage);
        let state = self.stage_state(i);
        let end = match self.index.stages.get(i + 1) {
            Some(next) => self.index.stage_start(next),
            None if state == "running" || state == "paused" => Some(now),
            None => self.index.last_line_ms(),
        };
        let secs = match (start, end) {
            (Some(s), Some(e)) => (e.saturating_sub(s) / 1000) as i64,
            _ => -1,
        };
        json!({
            "k": "h",
            "stage": entry.stage,
            "title": stage.title,
            "level": stage.level,
            "kind": stage.kind,
            "state": state,
            "startMs": start.unwrap_or(0),
            "secs": secs,
            "lines": entry.total,
            "errors": stage.errors,
            "open": entry.shown > 0 || entry.total == 0,
        })
    }

    fn line_json(&mut self, line: u32) -> Value {
        let class = self.index.classes.get(line as usize).copied().unwrap_or(0);
        let raw = self.read_line(line).map(<[u8]>::to_vec).unwrap_or_default();
        let mut text = clean(&raw);
        if text.chars().count() > SHOW_CHARS {
            text = text.chars().take(SHOW_CHARS).collect();
            text.push_str(" …");
        }
        json!({"k": "l", "line": line, "text": text, "src": class & SRC_MASK, "err": class & ERROR_BIT != 0})
    }

    /// `count` rows from `first`, stage headings and lines together, as JSON.
    pub fn rows_json(&mut self, first: u32, count: u32) -> String {
        let now = now_ms();
        let mut out = Vec::new();
        let end = first.saturating_add(count.min(PAGE_MAX)).min(self.rows);
        let mut row = first;
        while row < end {
            let i = self.layout.partition_point(|e| e.first_row <= row).saturating_sub(1);
            let Some(entry) = self.layout.get(i).copied() else {
                break;
            };
            if row == entry.first_row {
                out.push(self.header_json(&entry, now));
            } else if let Some(line) = self.line_of(&entry, row - entry.first_row - 1) {
                out.push(self.line_json(line));
            }
            row += 1;
        }
        Value::Array(out).to_string()
    }

    pub fn summary_json(&self, busy: bool) -> String {
        json!({
            "generation": self.generation,
            "rows": self.rows,
            "lines": self.index.lines(),
            "matches": self.matches.as_ref().map(Vec::len),
            "stages": self.index.stages.len(),
            "busy": busy,
        })
        .to_string()
    }

    /// The match-space range of what would be copied or exported.
    fn range(&self, stage: Option<u32>) -> (usize, usize) {
        let total = self.index.lines() as usize;
        let span = stage.map(|s| {
            let first = self.index.stages[s as usize].first_line as usize;
            let end = self.index.stages.get(s as usize + 1).map_or(total, |n| n.first_line as usize);
            (first, end)
        });
        match (&self.matches, span) {
            (Some(m), Some((a, b))) => {
                (m.partition_point(|l| (*l as usize) < a), m.partition_point(|l| (*l as usize) < b))
            }
            (Some(m), None) => (0, m.len()),
            (None, Some(span)) => span,
            (None, None) => (0, total),
        }
    }

    fn line_at(&self, i: usize) -> u32 {
        self.matches.as_ref().map_or(i as u32, |m| m[i])
    }

    /// Writes lines, the latest `cap` bytes of them, under their stage names. Returns the lines
    /// written and left out.
    fn write_view(&mut self, out: &mut dyn Write, stage: Option<u32>, cap: usize) -> std::io::Result<(usize, usize)> {
        let (lo, hi) = self.range(stage);
        let mut start = hi;
        let mut size = 0usize;
        while start > lo && size < cap {
            start -= 1;
            let line = self.line_at(start);
            size += self.read_line(line).map_or(0, |r| r.len() + 1);
        }
        let mut current = None;
        for i in start..hi {
            let line = self.line_at(i);
            if stage.is_none() {
                let s = self.index.stages.partition_point(|s| s.first_line <= line).saturating_sub(1);
                if current != Some(s) {
                    current = Some(s);
                    if let Some(st) = self.index.stages.get(s) {
                        writeln!(out, "--- {} ---", st.title)?;
                    }
                }
            }
            let text = self.read_line(line).map(clean).unwrap_or_default();
            writeln!(out, "{text}")?;
        }
        Ok((hi - start, start - lo))
    }

    pub fn copy_text(&mut self, stage: i32) -> String {
        let mut out = Vec::new();
        let stage = (stage >= 0).then_some(stage as u32);
        let omitted = self.write_view(&mut out, stage, COPY_CAP).map_or(0, |r| r.1);
        let text = String::from_utf8_lossy(&out).into_owned();
        if omitted > 0 {
            format!("({omitted} earlier lines left out; use Share for the whole log)\n{text}")
        } else {
            text
        }
    }

    /// Writes the current view to `dest` for sharing; "" or the error.
    pub fn export(&mut self, dest: &Path) -> Result<String, String> {
        let mut file = fs::File::create(dest).map_err(|e| e.to_string())?;
        let (written, omitted) = self
            .write_view(&mut file, None, EXPORT_CAP)
            .map_err(|e| format!("cannot write the log: {e}"))?;
        Ok(if omitted > 0 {
            format!("{written} lines; the {omitted} before them are left out (over 256 MiB)")
        } else {
            format!("{written} lines")
        })
    }
}

// ---------------------------------------------------------------- the views the UI has open

static VIEWS: OnceLock<Mutex<HashMap<i64, Arc<Mutex<LogView>>>>> = OnceLock::new();
static NEXT_VIEW: AtomicI64 = AtomicI64::new(1);

fn views() -> &'static Mutex<HashMap<i64, Arc<Mutex<LogView>>>> {
    VIEWS.get_or_init(Default::default)
}

pub fn open_view(path: &str) -> i64 {
    let id = NEXT_VIEW.fetch_add(1, Ordering::SeqCst);
    views().lock().unwrap().insert(id, Arc::new(Mutex::new(LogView::new(Path::new(path)))));
    id
}

pub fn close_view(id: i64) {
    views().lock().unwrap().remove(&id);
}

pub fn with_view<T>(id: i64, f: impl FnOnce(&mut LogView) -> T) -> Option<T> {
    let view = views().lock().unwrap().get(&id)?.clone();
    let mut view = view.lock().unwrap();
    Some(f(&mut view))
}

/// One read of a view: at most `budget`, then what the screen needs to know.
pub fn sync_view(id: i64, run_state: &str, paused: bool) -> String {
    with_view(id, |v| {
        let busy = v.sync(run_state, paused, Duration::from_millis(40));
        v.summary_json(busy)
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------- the progress screen's facts

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Fact {
    pub label: String,
    pub value: String,
    pub warn: bool,
    /// Takes the panel's whole width.
    pub wide: bool,
}

fn fact(label: &str, value: impl Into<String>) -> Fact {
    Fact { label: label.into(), value: value.into(), warn: false, wide: false }
}

fn ago(now: u64, then: u64) -> u64 {
    now.saturating_sub(then) / 1000
}

fn is_live(status: &Status) -> bool {
    status.state == "running" && !status.paused && status.hold.is_empty()
}

/// The facts about the step in progress, all of them read from the log.
pub fn progress_facts(status: &Status, index: &LogIndex, now: u64) -> Vec<Fact> {
    let mut out = Vec::new();
    let live = is_live(status);
    let attempt = index.stages.iter().rposition(|s| s.attempt).unwrap_or(0);
    let current = index.stages[attempt.min(index.stages.len().saturating_sub(1))..].iter();
    let tool = current.clone().rfind(|s| s.level == 0 && s.kind != "step");
    let guest = current.clone().rfind(|s| s.kind == "vm");
    let detail = current.clone().rfind(|s| s.level == 1 && s.kind != "vm");

    let mut stage = fact(
        "Stage",
        tool.map(|s| s.title.clone())
            .or_else(|| (!status.message.is_empty()).then(|| status.message.clone()))
            .unwrap_or_else(|| "Starting".to_string()),
    );
    stage.wide = true;
    out.push(stage);
    if let Some(g) = guest {
        let mut f = fact("VM says", g.title.clone());
        f.wide = true;
        out.push(f);
    }
    if let Some(d) = detail {
        let mut f = fact("Latest step", d.title.clone());
        f.wide = true;
        out.push(f);
    }
    let since = |s: Option<&Stage>| -> String {
        match s.and_then(|s| index.stage_start(s)) {
            Some(start) => duration_text(ago(now, start)),
            None => "not recorded".to_string(),
        }
    };
    out.push(fact("In this stage", if live || status.paused { since(tool) } else { "stopped".to_string() }));
    let step_start = index.stages.get(attempt);
    out.push(fact("This attempt", since(step_start)));

    let mut line = fact("Last log line", "none yet");
    if let Some(ms) = index.last_line_ms() {
        let secs = ago(now, ms);
        line.value = format!("{} ago", duration_text(secs));
        line.warn = live && secs >= SILENT_WARN_SECS;
    }
    out.push(line);
    let emulated = STEPS.iter().any(|d| d.key == status.step && d.emulated);
    if emulated {
        let mut vm = fact("Last VM output", "none yet in this step");
        if let Some(ms) = index.last_guest.and_then(|o| index.time_of(o)) {
            let secs = ago(now, ms);
            vm.value = format!("{} ago", duration_text(secs));
            vm.warn = live && secs >= GUEST_SILENT_WARN_SECS;
        } else if index.last_guest.is_some() {
            vm.value = "time not recorded".to_string();
        }
        out.push(vm);
    }
    if let Some((mib, secs)) = index.sent {
        let speed = if secs > 0.0 { format!(", {:.1} MiB/s", mib as f64 / secs) } else { String::new() };
        out.push(fact("Sent over USB", format!("{mib} MiB{speed}")));
    }
    if let Some((name, bytes)) = &index.sending {
        out.push(fact("Sending", format!("{name}, {}", size_text(*bytes))));
    }
    // Below a megabyte these counters are the restore's stage number or a file count.
    if status.total_bytes == mirror::STAGE_COUNT {
        out.push(fact("Restore stage", format!("{} of {}", status.done_bytes, status.total_bytes)));
    } else if status.total_bytes >= 1 << 20 {
        let speed = if status.bytes_per_sec > 0 {
            format!(", {}/s", size_text(status.bytes_per_sec))
        } else {
            String::new()
        };
        out.push(fact(
            "Transferred",
            format!("{} of {}{speed}", size_text(status.done_bytes), size_text(status.total_bytes)),
        ));
    }
    out
}

/// A number or text from the VM's stats, as text.
fn plain_value(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// `vm-stats.json` as panel rows. `age` is how old the file is; `None` when it does not exist.
pub fn vm_facts(stats: Option<&str>, age: Option<u64>) -> Vec<Fact> {
    let only = |value: String, warn: bool| {
        vec![Fact { label: "VM".into(), value, warn, wide: true }]
    };
    let Some(text) = stats else {
        return only("Nothing is running".into(), false);
    };
    let age = age.unwrap_or(0);
    if age > STATS_STALE_SECS {
        return only(format!("Nothing new from the VM for {}", duration_text(age)), true);
    }
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return only("Its stats file cannot be read".into(), true);
    };
    if v.get("alive").and_then(Value::as_bool) != Some(true) {
        return only("Nothing is running".into(), false);
    }
    let paused = v.get("paused").and_then(Value::as_bool).unwrap_or(false);
    let number = |key: &str| v.get(key).and_then(Value::as_f64);
    let mut out = vec![fact("VM", if paused { "paused" } else { "running" })];
    if let Some(c) = number("cpuPercent") {
        out.push(fact("CPU", format!("{c:.0}%")));
    }
    if let Some(m) = number("rssMb") {
        out.push(fact("VM memory", format!("{:.1} GB", m / 1024.0)));
    }
    if let Some(m) = number("diskWrittenMb") {
        out.push(fact("Disk written", size_text((m * 1024.0 * 1024.0) as u64)));
    }
    if let Some(u) = plain_value(v.get("usbStage")) {
        out.push(fact("USB", u));
    }
    let el = plain_value(v.get("guestEl"));
    let pc = match v.get("guestPc") {
        Some(Value::Number(n)) => n.as_u64().map(|p| format!("{p:#x}")),
        other => plain_value(other),
    };
    if el.is_some() || pc.is_some() {
        let mut f = fact(
            "Guest CPU",
            [el.map(|e| format!("EL{e}")), pc].into_iter().flatten().collect::<Vec<_>>().join(" "),
        );
        f.wide = true;
        out.push(f);
    }
    if let Some(moving) = v.get("guestMoving").and_then(Value::as_bool) {
        let mut f = fact("Guest moving", if moving { "yes" } else { "no" });
        f.warn = !moving && !paused;
        out.push(f);
    }
    match v.get("display") {
        Some(Value::String(s)) => {
            let shown = match s.as_str() {
                "on" => "on",
                "off" => "off",
                "headless" => "headless",
                "no-gpu" => "no GPU this boot",
                other => other,
            };
            out.push(fact("Display", shown));
        }
        Some(Value::Bool(b)) => out.push(fact("Display", if *b { "on" } else { "off" })),
        _ => {}
    }
    out
}

fn vm_stats_facts(paths: &Paths, creation_running: bool) -> Vec<Fact> {
    let file = paths.create_sockets().join(STATS_FILE);
    let age = fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs());
    // A VM killed with its creation leaves its last stats behind; that is not a stalled VM.
    if !creation_running && age.is_some_and(|a| a > STATS_STALE_SECS) {
        return vm_facts(None, None);
    }
    let text = fs::read_to_string(&file).ok();
    vm_facts(text.as_deref(), age)
}

fn phone_facts(paths: &Paths) -> Vec<Fact> {
    const LOW_STORAGE: u64 = 5 << 30;
    const LOW_MEMORY_MIB: u64 = 1024;
    let free = free_bytes(&paths.files);
    let mem = MemInfo::read();
    let mut storage = fact("Free storage", size_text(free));
    storage.warn = free < LOW_STORAGE;
    let mut memory = fact(
        "Available memory",
        format!("{:.1} of {:.1} GB", mem.available_mib as f64 / 1024.0, mem.total_mib as f64 / 1024.0),
    );
    memory.warn = mem.available_mib < LOW_MEMORY_MIB;
    vec![storage, memory]
}

/// Each step's own time, from what the log and the step records say.
fn step_times(status: &Status, index: &LogIndex, now: u64) -> HashMap<String, String> {
    let attempt = index.stages.iter().rfind(|s| s.attempt);
    status
        .steps
        .iter()
        .map(|st| {
            let took = duration_text(st.elapsed_secs);
            let text = match st.state.as_str() {
                "done" if st.elapsed_secs > 0 => format!("took {took}"),
                "done" => "done".to_string(),
                "failed" if st.elapsed_secs > 0 => format!("failed after {took}"),
                "failed" => "failed".to_string(),
                "running" if st.key == status.step => match attempt.and_then(|s| index.stage_start(s)) {
                    Some(start) => format!("running for {}", duration_text(ago(now, start))),
                    None => "running".to_string(),
                },
                "running" => "stopped".to_string(),
                _ => "not started".to_string(),
            };
            (st.key.clone(), text)
        })
        .collect()
}

static LIVE: Mutex<Option<LogIndex>> = Mutex::new(None);

/// The status screen's live panel for this creation, as JSON. Called off the main thread.
pub fn live_json(paths: &Paths, status: &Status) -> String {
    let now = now_ms();
    let path = PathBuf::from(&status.log);
    let mut guard = LIVE.lock().unwrap();
    if guard.as_ref().is_none_or(|i| i.path != path) {
        *guard = Some(LogIndex::new(&path));
    }
    let index = guard.as_mut().expect("just set");
    if !status.log.is_empty() {
        while index.refresh(Duration::from_millis(250)) {}
    }
    let facts = progress_facts(status, index, now);
    let times = step_times(status, index, now);
    json!({
        "facts": facts,
        "vm": vm_stats_facts(paths, status.state == "running"),
        "phone": phone_facts(paths),
        "stepTimes": times,
    })
    .to_string()
}

/// The time each finished step has spent, from `.create-times.json`.
pub fn apply_step_times(vm: &Path, status: &mut Status) {
    let Some(times) = fs::read(vm.join(".create-times.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<HashMap<String, u64>>(&b).ok())
    else {
        return;
    };
    for step in &mut status.steps {
        if let Some(t) = times.get(&step.key) {
            step.elapsed_secs = *t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as real restores and boots wrote them to a step log.
    const REAL: &str = "▶ gpu-driver (4/9)\n=== DFU\n[usb] vusbd: listening on /x\nTrying to fetch new SHSH blob\nSending iBSS (380445 bytes)...\n[guest] OpenBootROM: SEP RX ready\n[guest] OpenBootROM: SEP RX empty\n[guest] ======== Start of iBoot serial output. ========\n[guest] Entering recovery mode, starting command prompt\n[guest] ======== End of iBoot serial output. ========\nSending iBEC (380475 bytes)...\nSending RestoreRamDisk (232789583 bytes)...\n[usb] vusbd: sent 8 MiB in 7.0 s\nSending RestoreSEP (3298945 bytes)...\nWaiting for device to enter restore mode...\nDevice 0000FE01 has successfully entered restore mode\nMounting filesystems (16)\nMounting filesystems (16)\nUnknown operation (88)\nStatus: Restore Finished\n";

    fn stage_titles(text: &str) -> Vec<String> {
        let mut t = Tracker::default();
        text.lines()
            .filter_map(|l| {
                let (source, body) = split_source(l);
                t.mark(source, body).map(|m| m.title)
            })
            .collect()
    }

    fn fixture(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/logview").join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
    }

    fn append(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    fn drain(v: &mut LogView) {
        while v.sync("running", false, Duration::from_millis(100)) {}
    }

    fn rows(v: &mut LogView) -> Vec<Value> {
        let total = v.rows;
        serde_json::from_str(&v.rows_json(0, total.max(1))).unwrap()
    }

    #[test]
    fn real_lines_become_named_stages_in_order() {
        assert_eq!(
            stage_titles(REAL),
            [
                "Step gpu-driver (4/9)",
                "Start the VM in DFU mode",
                "Asking Apple to sign the restore",
                "Loading the first boot stage",
                "VM boot ROM",
                "VM iBoot",
                "VM iBoot recovery mode",
                "VM iBoot finished; no VM output until the next stage prints",
                "Loading the second boot stage",
                "Loading the restore system",
                "Sending RestoreSEP (3 MiB)",
                "Waiting for the restore system to boot",
                "The restore system is running",
                "Mounting filesystems",
                "Mounting filesystems",
                "Restore finished",
            ]
        );
    }

    #[test]
    fn a_second_attempt_restarts_the_restores_stages_and_is_named() {
        let titles = stage_titles("▶ restore (7/9)\nSending iBSS (1 bytes)...\nstep restore failed\n▶ restore (7/9)\nSending iBSS (1 bytes)...\n");
        assert_eq!(titles, ["Step restore (7/9)", "Loading the first boot stage", "Attempt 2: restore (7/9)", "Loading the first boot stage"]);
    }

    #[test]
    fn a_boot_that_reboots_shows_its_stages_again_but_not_every_repeat() {
        let titles = stage_titles("[guest] OpenBootROM: a\n[guest] OpenBootROM: b\n[guest] ======== Start of iBoot serial output. ========\n[guest] OpenBootROM: c\n");
        assert_eq!(titles, ["VM boot ROM", "VM iBoot", "VM boot ROM"]);
    }

    #[test]
    fn sources_and_errors_are_told_apart() {
        assert_eq!(split_source("[guest] OpenBootROM: x"), (SRC_GUEST, "OpenBootROM: x"));
        assert_eq!(split_source("[usb] vusbd: sent 8 MiB in 7.0 s"), (SRC_USB, "vusbd: sent 8 MiB in 7.0 s"));
        assert_eq!(split_source("Waiting for device to enter restore mode...").0, SRC_TOOL);
        for bad in ["ERROR: Unable to connect", "step restore failed", "panic(cpu 0 caller 0x1): x", "the restore timed out"] {
            assert!(is_error(bad), "{bad}");
        }
        for fine in ["Waiting for device to enter restore mode...", "Finished with no error", "OpenBootROM: SEP RX ready", "Sending iBSS (380445 bytes)..."] {
            assert!(!is_error(fine), "{fine}");
        }
    }

    #[test]
    fn a_line_is_cleaned_for_reading() {
        assert_eq!(clean(b"\x1b[31mred\x1b[0m"), "red");
        assert_eq!(clean(b"[==   ] 10%\r[==== ] 50%\r\n"), "[==== ] 50%");
        assert_eq!(clean(b"a\tb"), "a    b");
    }

    #[test]
    fn sends_are_parsed_with_either_form() {
        assert_eq!(sending_of("Sending RestoreRamDisk (232789583 bytes)..."), Some(("RestoreRamDisk".into(), 232789583)));
        assert_eq!(sending_of("Sending Ap,SecurePageTableMonitor now (115812 bytes)..."), Some(("Ap,SecurePageTableMonitor".into(), 115812)));
        assert_eq!(sending_of("Sending filesystem now..."), None);
    }

    #[test]
    fn times_come_from_the_stamps() {
        let mut s = Stamps::default();
        s.entries = vec![(100, 0), (200, 5000), (300, 9000)];
        assert_eq!(s.time_at(50), None, "bytes written before timing began");
        assert_eq!(s.time_at(150), Some(5000));
        assert_eq!(s.time_at(250), Some(9000));
        assert_eq!(s.time_at(300), None);
        assert!(s.is_recent(300) && !s.is_recent(250));
    }

    #[test]
    fn the_stamper_times_new_lines_and_not_old_ones() {
        let d = fixture("stamper");
        let log = d.join("restore.log");
        write(&log, "from an earlier run\n");
        let mut seen = HashMap::new();
        stamp_once(&d, &mut seen);
        append(&log, "new line\n");
        stamp_once(&d, &mut seen);
        let mut stamps = Stamps::default();
        stamps.refresh(&stamps_path(&log));
        assert_eq!(stamps.entries.len(), 2);
        assert_eq!(stamps.time_at(0), None, "written before anyone was timing it");
        assert!(stamps.time_at(25).is_some());
        // A log cut short starts its record again.
        write(&log, "x\n");
        stamp_once(&d, &mut seen);
        assert_eq!(fs::metadata(stamps_path(&log)).unwrap().len(), 0);
    }

    #[test]
    fn view_follows_a_growing_log_and_counts_stay_consistent() {
        let d = fixture("grow");
        let log = d.join("s.log");
        write(&log, REAL);
        let mut v = LogView::new(&log);
        drain(&mut v);
        assert_eq!(v.index.lines() as usize, REAL.lines().count());
        let first_rows = v.rows;
        let gen = v.generation;
        append(&log, "half a li");
        drain(&mut v);
        assert_eq!(v.rows, first_rows, "half a line is not a line");
        append(&log, "ne\n");
        drain(&mut v);
        assert_eq!(v.rows, first_rows + 1);
        assert_eq!(v.generation, gen, "only growth: the screen appends");
        let all = rows(&mut v);
        assert_eq!(all.last().unwrap()["text"], "half a line");
    }

    #[test]
    fn a_new_stage_folds_the_last_one_away_and_says_so() {
        let d = fixture("fold");
        let log = d.join("s.log");
        write(&log, "▶ restore (7/9)\none\ntwo\n");
        let mut v = LogView::new(&log);
        drain(&mut v);
        assert_eq!(v.rows, 4);
        let gen = v.generation;
        append(&log, "=== next\nthree\n");
        drain(&mut v);
        assert_ne!(v.generation, gen);
        let all = rows(&mut v);
        assert_eq!(all[0]["k"], "h");
        assert_eq!(all[0]["open"], false);
        assert_eq!(all[0]["lines"], 3);
        assert_eq!(all[1]["title"], "next");
        assert_eq!(all[2]["text"], "=== next");
        v.toggle(0);
        assert_eq!(rows(&mut v)[1]["text"], "▶ restore (7/9)");
    }

    #[test]
    fn filters_and_search_pick_the_right_lines() {
        let d = fixture("filter");
        let log = d.join("s.log");
        write(&log, REAL);
        let text = |v: &mut LogView| -> Vec<String> {
            rows(v).iter().filter(|r| r["k"] == "l").map(|r| r["text"].as_str().unwrap().to_string()).collect()
        };
        let mut v = LogView::new(&log);
        drain(&mut v);
        v.set_filter(FILTER_USB, "");
        drain(&mut v);
        assert_eq!(text(&mut v), ["[usb] vusbd: listening on /x", "[usb] vusbd: sent 8 MiB in 7.0 s"]);
        v.set_filter(FILTER_VM, "recovery");
        drain(&mut v);
        assert_eq!(text(&mut v), ["[guest] Entering recovery mode, starting command prompt"]);
        v.set_filter(FILTER_ALL, "RESTOREramdisk");
        drain(&mut v);
        assert_eq!(text(&mut v), ["Sending RestoreRamDisk (232789583 bytes)..."]);
        v.set_filter(FILTER_ALL, "");
        drain(&mut v);
        assert_eq!(text(&mut v).len(), 1, "unfiltered, only the last stage stays open");
    }

    #[test]
    fn errors_are_found_and_counted_on_their_stage() {
        let d = fixture("errors");
        let log = d.join("s.log");
        write(&log, "▶ restore (7/9)\nall fine\nstep restore failed\nthe restore failed with status 1\n");
        let mut v = LogView::new(&log);
        drain(&mut v);
        v.set_filter(FILTER_ERRORS, "");
        drain(&mut v);
        let all = rows(&mut v);
        let lines: Vec<_> = all.iter().filter(|r| r["k"] == "l").collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|r| r["err"] == true));
        assert_eq!(all[0]["errors"], 2);
        assert_eq!(all[0]["state"], "failed");
    }

    #[test]
    fn a_huge_log_is_indexed_and_read_from_the_middle() {
        let d = fixture("huge");
        let log = d.join("s.log");
        let mut f = fs::File::create(&log).unwrap();
        writeln!(f, "▶ restore (7/9)").unwrap();
        for i in 0..200_000 {
            writeln!(f, "[guest] line number {i}").unwrap();
        }
        drop(f);
        let mut v = LogView::new(&log);
        drain(&mut v);
        assert_eq!(v.index.lines(), 200_001);
        assert!(v.index.blocks.len() < 7000, "only a sparse index is kept");
        let page: Vec<Value> = serde_json::from_str(&v.rows_json(150_000, 3)).unwrap();
        assert_eq!(page[0]["text"], "[guest] line number 149998");
    }

    #[test]
    fn a_line_too_long_to_keep_is_still_one_line() {
        let d = fixture("long");
        let log = d.join("s.log");
        write(&log, &format!("first\n{}\nthird\n", "x".repeat(600_000)));
        let mut v = LogView::new(&log);
        drain(&mut v);
        assert_eq!(v.index.lines(), 3);
        let all = rows(&mut v);
        assert_eq!(all[3]["text"], "third");
        assert!(all[2]["text"].as_str().unwrap().ends_with('…'));
    }

    #[test]
    fn a_replaced_log_is_read_again_from_the_start() {
        let d = fixture("replaced");
        let log = d.join("s.log");
        write(&log, "one\ntwo\nthree\n");
        let mut v = LogView::new(&log);
        drain(&mut v);
        let gen = v.generation;
        fs::remove_file(&log).unwrap();
        write(&log, "only\n");
        drain(&mut v);
        assert_eq!(v.index.lines(), 1);
        assert_ne!(v.generation, gen);
    }

    #[test]
    fn copying_takes_a_stage_or_the_current_view() {
        let d = fixture("copy");
        let log = d.join("s.log");
        write(&log, "▶ restore (7/9)\n=== one\na line\n=== two\nb line\n");
        let mut v = LogView::new(&log);
        drain(&mut v);
        assert_eq!(v.copy_text(1), "=== one\na line\n");
        assert!(v.copy_text(-1).contains("--- two ---\n=== two\nb line\n"));
        let out = d.join("export.txt");
        assert_eq!(v.export(&out).unwrap(), "5 lines");
        assert!(fs::read_to_string(out).unwrap().ends_with("b line\n"));
    }

    fn status(step: &str) -> Status {
        Status {
            state: "running".into(),
            step: step.into(),
            ..Default::default()
        }
    }

    #[test]
    fn progress_facts_state_the_stage_and_warn_of_silence() {
        let d = fixture("facts");
        let log = d.join("s.log");
        write(&log, "▶ gpu-driver (4/9)\nSending RestoreRamDisk (232789583 bytes)...\n[usb] vusbd: sent 8 MiB in 7.0 s\nWaiting for device to enter restore mode...\n[guest] OpenBootROM: SEP RX ready\n");
        let mut index = LogIndex::new(&log);
        index.refresh(Duration::from_secs(1));
        let last = index.last_line_ms().unwrap();
        let get = |facts: &[Fact], label: &str| facts.iter().find(|f| f.label == label).cloned();
        let facts = progress_facts(&status("gpu-driver"), &index, last + 2000);
        assert_eq!(get(&facts, "Stage").unwrap().value, "Waiting for the restore system to boot");
        assert_eq!(get(&facts, "VM says").unwrap().value, "VM boot ROM");
        assert_eq!(get(&facts, "Sent over USB").unwrap().value, "8 MiB, 1.1 MiB/s");
        assert_eq!(get(&facts, "Sending").unwrap().value, "RestoreRamDisk, 222 MiB");
        assert_eq!(get(&facts, "Last log line").unwrap().value, "2 s ago");
        assert!(!get(&facts, "Last log line").unwrap().warn);
        let quiet = progress_facts(&status("gpu-driver"), &index, last + 400_000);
        assert!(get(&quiet, "Last log line").unwrap().warn, "silence is pointed out");
        let mut paused = status("gpu-driver");
        paused.paused = true;
        let held = progress_facts(&paused, &index, last + 400_000);
        assert!(!get(&held, "Last log line").unwrap().warn, "a paused run is not silent");
    }

    #[test]
    fn vm_output_from_an_earlier_attempt_does_not_count_for_this_one() {
        let d = fixture("attempt");
        let log = d.join("s.log");
        write(&log, "▶ restore (7/9)\n[guest] OpenBootROM: a\nstep restore failed\n▶ restore (7/9)\n");
        let mut index = LogIndex::new(&log);
        index.refresh(Duration::from_secs(1));
        let facts = progress_facts(&status("restore"), &index, 1);
        let vm = facts.iter().find(|f| f.label == "Last VM output").unwrap();
        assert_eq!(vm.value, "none yet in this step");
    }

    #[test]
    fn a_counter_that_is_not_bytes_is_not_shown_as_bytes() {
        let d = fixture("bytes");
        let log = d.join("s.log");
        write(&log, "▶ firmware (3/9)\n");
        let mut index = LogIndex::new(&log);
        index.refresh(Duration::from_secs(1));
        let mut s = status("firmware");
        s.done_bytes = 5;
        s.total_bytes = mirror::STAGE_COUNT;
        assert!(progress_facts(&s, &index, 1).iter().any(|f| f.label == "Restore stage" && f.value == "5 of 12"));
        s.done_bytes = 3 << 30;
        s.total_bytes = 12 << 30;
        s.bytes_per_sec = 32 << 20;
        let f = progress_facts(&s, &index, 1);
        assert!(f.iter().any(|f| f.label == "Transferred" && f.value == "3.0 GiB of 12.0 GiB, 32 MiB/s"), "{f:?}");
    }

    #[test]
    fn the_vm_panel_never_invents_numbers() {
        assert_eq!(vm_facts(None, None)[0].value, "Nothing is running");
        let dead = r#"{"alive":false,"cpuPercent":500}"#;
        assert_eq!(vm_facts(Some(dead), Some(0))[0].value, "Nothing is running");
        let stale = r#"{"alive":true,"cpuPercent":500}"#;
        let f = vm_facts(Some(stale), Some(120));
        assert_eq!(f.len(), 1);
        assert!(f[0].warn && f[0].value.contains("2 min"));
    }

    #[test]
    fn a_live_vm_is_shown_as_it_reports_itself() {
        let text = r#"{"alive":true,"cpuPercent":612.4,"rssMb":3072,"diskWrittenMb":512,"usbStage":"recovery","guestEl":1,"guestPc":18446741874686296064,"guestMoving":false,"display":"no-gpu"}"#;
        let f = vm_facts(Some(text), Some(1));
        let get = |l: &str| f.iter().find(|x| x.label == l).unwrap().clone();
        assert_eq!(get("CPU").value, "612%");
        assert_eq!(get("VM memory").value, "3.0 GB");
        assert_eq!(get("Disk written").value, "512 MiB");
        assert_eq!(get("USB").value, "recovery");
        assert_eq!(get("Guest CPU").value, "EL1 0xfffffe0000000000");
        assert!(get("Guest moving").warn, "a guest that stopped is pointed out");
        assert_eq!(get("Display").value, "no GPU this boot");
    }

    #[test]
    fn step_rows_say_what_happened_not_how_long_it_might_take() {
        let d = fixture("times");
        let log = d.join("s.log");
        write(&log, "▶ restore (7/9)\n");
        let mut index = LogIndex::new(&log);
        index.refresh(Duration::from_secs(1));
        let step = |key: &str, state: &str, secs| super::super::state::StepStatus {
            key: key.into(),
            state: state.into(),
            elapsed_secs: secs,
            ..Default::default()
        };
        let mut s = status("restore");
        s.steps = vec![step("patch", "done", 3700), step("restore", "running", 0), step("cfw", "pending", 0)];
        let times = step_times(&s, &index, 1);
        assert_eq!(times["patch"], "took 1 h 01 min");
        assert_eq!(times["cfw"], "not started");
        assert!(times["restore"].starts_with("running"));
    }
}
