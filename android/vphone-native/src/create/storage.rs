//! What the app keeps on the phone, and removing it when the person asks.
//!
//! VMs and failed attempts take many gigabytes and the phone has no shell, so this is the only way
//! to get them back. Everything the screen shows is measured here in allocated blocks, and every
//! removal goes through directory descriptors opened with `O_NOFOLLOW` from the app's own storage:
//! a symlink cannot lead a delete out of it. The UI names things (a VM, a firmware file, a
//! leftover's id); paths are always worked out here, from a fresh look at the disk.
//!
//! Two kernel locks keep a delete from racing a creation. `.create-lock`, the run's own lock,
//! says whether a VM is in use and is taken for the length of the operation. [`GATE_FILE`] is held
//! exclusively while any cleanup runs: [`hold_off_cleanup`] refuses a new creation meanwhile, and
//! [`wait_for_cleanup`] holds back a run that was resumed. A VM's folder is renamed away before
//! its contents are removed, so whatever starts next finds no folder, not half of one.

use super::compat::Firmware;
use super::engine::{self, free_bytes};
use super::state::{self, Status};
use super::steps::STEPS;
use crate::paths::Paths;
use crate::runtime::{self, Phase};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const KIB: u64 = 1 << 10;

/// Exclusive while a cleanup runs. Empty, and the one file a reset leaves behind.
pub const GATE_FILE: &str = ".storage-lock";
/// Folders being removed. One still here after a crash shows up as a leftover.
const TRASH_DIR: &str = ".storage-trash";
const TEMP_VM_PREFIX: &str = "gpu-recovery-";
const RUNS_PREFIX: &str = ".runs-";
const GPU_EXTRACT: &str = ".gpu-driver-extract";
const RELEASE_DIR: &str = "release";
/// A download's own files, listed with the image they belong to.
const DOWNLOAD_SUFFIXES: &[&str] = &[".part.ok", ".part.tmp", ".part", ".meta"];
/// The helpers a creation starts, and what to call them.
const HELPERS: &[(&str, &str)] = &[
    ("libvphone_vm.so", "VM process"),
    ("libvusbd.so", "USB broker (vusbd)"),
    ("libusbmuxd_vphone.so", "usbmuxd"),
];
/// How much of a folder a confirmation spells out.
const LISTED_CHILDREN: usize = 8;
const STOP_WAIT: Duration = Duration::from_secs(3);

// ------------------------------------------------------------------ sizes

pub fn human(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.0} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{} KB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

fn os_error() -> io::Error {
    io::Error::last_os_error()
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn open_root(path: &Path) -> io::Result<OwnedFd> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    Ok(file.into())
}

/// A directory under `parent`, refusing to pass through a symlink.
fn open_dir(parent: &OwnedFd, name: &CStr) -> io::Result<OwnedFd> {
    // SAFETY: parent is an open descriptor and name a valid C string for the call.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(os_error());
    }
    // SAFETY: fd was just opened and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn lstat_at(parent: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: an all-zero stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor and name are valid and st is a valid out-pointer.
    if unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(os_error());
    }
    Ok(st)
}

fn device_of(fd: &OwnedFd) -> io::Result<u64> {
    // SAFETY: an all-zero stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fd is open and st a valid out-pointer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(os_error());
    }
    Ok(st.st_dev as u64)
}

fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn allocated(st: &libc::stat) -> u64 {
    st.st_blocks as u64 * 512
}

/// The names in a directory; a new description, so its offset is its own.
fn names_in(dir: &OwnedFd) -> io::Result<Vec<CString>> {
    // SAFETY: dir is an open directory descriptor.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c".".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(os_error());
    }
    // SAFETY: fd is a fresh directory descriptor, handed over to the stream.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let e = os_error();
        // SAFETY: fdopendir failed, so fd is still ours to close.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: stream stays open until the closedir below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: d_name is a NUL-terminated array inside the entry just returned.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    // SAFETY: stream is open and is not used after this.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

fn walk_size(dir: &OwnedFd, dev: u64, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let mut total = 0;
    for name in names_in(dir).unwrap_or_default() {
        let Ok(st) = lstat_at(dir, &name) else { continue };
        if st.st_dev as u64 != dev {
            continue;
        }
        if is_dir(&st) {
            total += allocated(&st);
            if let Ok(child) = open_dir(dir, &name) {
                total += walk_size(&child, dev, seen);
            }
        } else if st.st_nlink as u64 <= 1 || seen.insert((st.st_dev as u64, st.st_ino as u64)) {
            total += allocated(&st);
        }
    }
    total
}

/// What `path` takes on disk: allocated blocks, so a sparse disk counts for what it has written.
/// A symlink counts as itself; what it points at is not looked at.
pub fn size_of(path: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(path) else {
        return 0;
    };
    let own = md.blocks() * 512;
    if !md.is_dir() {
        return own;
    }
    match open_root(path) {
        Ok(dir) => own + walk_size(&dir, md.dev(), &mut HashSet::new()),
        Err(_) => own,
    }
}

fn device_total(dir: &Path) -> u64 {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return 0;
    };
    // SAFETY: an all-zero statvfs is a valid out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid path and st a valid out-pointer for the call.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    st.f_blocks as u64 * st.f_frsize as u64
}

// ------------------------------------------------------------------ staying inside the app

/// One name from the UI: a single path component, nothing that climbs or is empty.
fn safe_component(name: &str, what: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.len() > 255 || name.contains(['/', '\0']) {
        return Err(format!("{name:?} is not a valid {what}; nothing was removed"));
    }
    Ok(())
}

/// A place strictly inside filesDir or cacheDir, as the components below that root.
struct Scoped {
    root: PathBuf,
    rel: Vec<OsString>,
}

fn scoped(paths: &Paths, target: &Path) -> Result<Scoped, String> {
    for root in [&paths.files, &paths.cache] {
        let Ok(rest) = target.strip_prefix(root) else { continue };
        let rel: Option<Vec<OsString>> = rest
            .components()
            .map(|c| match c {
                Component::Normal(n) => Some(n.to_os_string()),
                _ => None,
            })
            .collect();
        if let Some(rel) = rel.filter(|r| !r.is_empty()) {
            return Ok(Scoped { root: root.clone(), rel });
        }
        break;
    }
    Err(format!("{} is not inside the app's own storage; nothing was removed", target.display()))
}

impl Scoped {
    /// The target's directory, reached without following a symlink, its name, and the filesystem.
    fn parent(&self) -> Result<(OwnedFd, CString, u64), String> {
        let shown = self.root.join(self.rel.iter().collect::<PathBuf>());
        let refuse = |e: io::Error| match e.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => {
                format!("{}: a folder on the way there is a symbolic link; nothing was removed", shown.display())
            }
            _ => format!("{}: {e}", shown.display()),
        };
        let mut dir = open_root(&self.root).map_err(refuse)?;
        let dev = device_of(&dir).map_err(refuse)?;
        let (last, ways) = self.rel.split_last().ok_or("nothing to remove")?;
        for part in ways {
            let name = c_name(part).map_err(refuse)?;
            dir = open_dir(&dir, &name).map_err(refuse)?;
        }
        Ok((dir, c_name(last).map_err(refuse)?, dev))
    }

    fn lstat(&self) -> Result<libc::stat, String> {
        let (parent, name, _) = self.parent()?;
        lstat_at(&parent, &name).map_err(|e| e.to_string())
    }
}

// ------------------------------------------------------------------ removing

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub path: String,
    pub error: String,
}

/// What one operation did.
#[derive(Default, Debug)]
struct Outcome {
    freed: u64,
    removed: Vec<String>,
    kept: Vec<String>,
    failures: Vec<Failure>,
}

impl Outcome {
    fn fail(&mut self, path: &Path, error: impl ToString) {
        self.failures.push(Failure { path: path.display().to_string(), error: error.to_string() });
    }
}

/// Removes one entry of `parent`; a symlink is removed as a link. Only freed blocks are counted.
fn remove_at(parent: &OwnedFd, name: &CStr, shown: &Path, dev: u64, out: &mut Outcome) -> bool {
    let shown = shown.join(OsStr::from_bytes(name.to_bytes()));
    let st = match lstat_at(parent, name) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return true,
        Err(e) => {
            out.fail(&shown, e);
            return false;
        }
    };
    if st.st_dev as u64 != dev {
        out.fail(&shown, "on another filesystem; not removed");
        return false;
    }
    let mut flags = 0;
    if is_dir(&st) {
        flags = libc::AT_REMOVEDIR;
        let dir = match open_dir(parent, name) {
            Ok(d) => d,
            Err(e) => {
                out.fail(&shown, e);
                return false;
            }
        };
        let names = match names_in(&dir) {
            Ok(n) => n,
            Err(e) => {
                out.fail(&shown, e);
                return false;
            }
        };
        let mut all = true;
        for child in &names {
            all &= remove_at(&dir, child, &shown, dev, out);
        }
        if !all {
            return false;
        }
    }
    note_current(&shown);
    // SAFETY: parent is open and name valid for the call.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        out.fail(&shown, os_error());
        return false;
    }
    if is_dir(&st) || st.st_nlink as u64 <= 1 {
        out.freed += allocated(&st);
        note_done(allocated(&st));
    }
    true
}

fn remove_path(paths: &Paths, target: &Path, out: &mut Outcome) -> bool {
    let s = match scoped(paths, target) {
        Ok(s) => s,
        Err(e) => {
            out.fail(target, e);
            return false;
        }
    };
    let (parent, name, dev) = match s.parent() {
        Ok(p) => p,
        // Nothing to remove when a folder on the way is already gone.
        Err(e) if gone(&e) => return true,
        Err(e) => {
            out.fail(target, e);
            return false;
        }
    };
    remove_at(&parent, &name, target.parent().unwrap_or(Path::new("")), dev, out)
}

fn open_folder(paths: &Paths, dir: &Path) -> Result<(OwnedFd, u64), String> {
    if dir == paths.files || dir == paths.cache {
        let fd = open_root(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let dev = device_of(&fd).map_err(|e| format!("{}: {e}", dir.display()))?;
        return Ok((fd, dev));
    }
    let (parent, name, dev) = scoped(paths, dir)?.parent()?;
    let fd = open_dir(&parent, &name).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok((fd, dev))
}

fn gone(e: &str) -> bool {
    e.contains("(os error 2)")
}

/// Removes what `dir` holds and leaves the folder, apart from `keep`.
fn remove_children(paths: &Paths, dir: &Path, keep: &[&str], out: &mut Outcome) -> bool {
    let (fd, dev) = match open_folder(paths, dir) {
        Ok(d) => d,
        Err(e) if gone(&e) => return true,
        Err(e) => {
            out.fail(dir, e);
            return false;
        }
    };
    let names = match names_in(&fd) {
        Ok(n) => n,
        Err(e) => {
            out.fail(dir, e);
            return false;
        }
    };
    let mut all = true;
    for name in names.iter().filter(|n| !keep.iter().any(|k| k.as_bytes() == n.to_bytes())) {
        all &= remove_at(&fd, name, dir, dev, out);
    }
    all
}

/// Renames a folder of the app's into the trash, so that it is gone at once from its old place.
fn to_trash(paths: &Paths, target: &Path) -> Result<PathBuf, String> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let s = scoped(paths, target)?;
    let (parent, name, _) = s.parent()?;
    let root = open_root(&paths.files).map_err(|e| format!("{}: {e}", paths.files.display()))?;
    let trash = c_name(OsStr::new(TRASH_DIR)).map_err(|e| e.to_string())?;
    // SAFETY: root is open and trash a valid C string.
    if unsafe { libc::mkdirat(root.as_raw_fd(), trash.as_ptr(), 0o700) } != 0 {
        let e = os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(format!("cannot make the trash folder: {e}"));
        }
    }
    let trash_fd = open_dir(&root, &trash).map_err(|e| format!("the trash folder cannot be used: {e}"))?;
    let stem = name.to_string_lossy().into_owned();
    let unique = format!("{}-{}-{stem}", state::now_ms(), SERIAL.fetch_add(1, Ordering::Relaxed));
    let new = CString::new(unique.as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: both descriptors are open and both names valid C strings.
    if unsafe { libc::renameat(parent.as_raw_fd(), name.as_ptr(), trash_fd.as_raw_fd(), new.as_ptr()) } != 0 {
        return Err(format!("cannot move {} aside: {}", target.display(), os_error()));
    }
    Ok(paths.files.join(TRASH_DIR).join(unique))
}

// ------------------------------------------------------------------ the gate

fn gate_path(paths: &Paths) -> PathBuf {
    paths.files.join(GATE_FILE)
}

fn open_gate(paths: &Paths) -> io::Result<File> {
    fs::create_dir_all(&paths.files)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(gate_path(paths))
}

fn try_flock(file: &File, operation: i32) -> bool {
    // SAFETY: the descriptor is valid for the call's duration.
    unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) == 0 }
}

/// A forked child holds a copy of a lock for the instant before it execs, so one try is not an answer.
fn settle_flock(file: &File, operation: i32) -> bool {
    for _ in 0..20 {
        if try_flock(file, operation) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn take_gate(paths: &Paths) -> Result<File, String> {
    let gate = open_gate(paths).map_err(|e| format!("cannot take the storage lock: {e}"))?;
    if settle_flock(&gate, libc::LOCK_EX) {
        Ok(gate)
    } else {
        Err("Another cleanup is still running.".into())
    }
}

/// Held by [`super::begin`]: a new creation is refused while a cleanup is running.
pub fn hold_off_cleanup(paths: &Paths) -> Result<Option<File>, String> {
    let Ok(gate) = open_gate(paths) else {
        return Ok(None);
    };
    if settle_flock(&gate, libc::LOCK_SH) {
        Ok(Some(gate))
    } else {
        Err("Storage is being cleaned up. Try again when that has finished.".into())
    }
}

/// A resumed run waits for a cleanup to finish; a cleanup that dies lets go by itself.
pub fn wait_for_cleanup(paths: &Paths) {
    let Ok(gate) = open_gate(paths) else {
        return;
    };
    while !try_flock(&gate, libc::LOCK_SH) {
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ------------------------------------------------------------------ what is in use

/// A helper process of the app's, identified as `is_our_vm_process` in vm.rs does: by the
/// executable it was started from. `started` tells it apart from a later process with its pid.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HelperProcess {
    pub pid: i32,
    pub name: String,
    #[serde(skip)]
    started: u64,
}

/// `pid`, if it is alive, this app's user's, and one of the app's helpers.
fn helper_at(paths: &Paths, pid: i32) -> Option<HelperProcess> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |key: &str| status.lines().find_map(|l| l.strip_prefix(key)).map(str::trim);
    if matches!(field("State:")?.chars().next(), Some('Z' | 'X') | None) {
        return None;
    }
    // SAFETY: getuid has no failure mode and no arguments.
    let me = unsafe { libc::getuid() };
    if field("Uid:")?.split_whitespace().next()?.parse::<u32>().ok()? != me {
        return None;
    }
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    // Argument 1 as well: where a program runs through binfmt_misc, argument 0 is the interpreter.
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).take(2).collect();
    let (_, name) = HELPERS
        .iter()
        .find(|(lib, _)| args.iter().any(|a| *a == paths.lib(lib).as_os_str().as_bytes()))?;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let started = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()?;
    Some(HelperProcess { pid, name: name.to_string(), started })
}

fn helper_processes(paths: &Paths) -> Vec<HelperProcess> {
    let mut found: Vec<HelperProcess> = fs::read_dir("/proc")
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
                .filter_map(|pid| helper_at(paths, pid))
                .collect()
        })
        .unwrap_or_default();
    found.sort_by_key(|p| p.pid);
    found
}

/// The running VM in this process, from the same state the VM screen uses.
fn normal_vm_up() -> Option<String> {
    let s = runtime::state();
    matches!(s.phase, Phase::Preparing | Phase::Booting | Phase::Unlocking | Phase::Running | Phase::Stopping)
        .then_some(s.vm)
}

/// What a cleanup needs from the app.
pub struct Ctx<'a> {
    pub paths: &'a Paths,
    /// The VM running in this process, if any. Normal VMs have no creation lock.
    pub normal_vm: fn() -> Option<String>,
}

impl<'a> Ctx<'a> {
    pub fn new(paths: &'a Paths) -> Ctx<'a> {
        Ctx { paths, normal_vm: normal_vm_up }
    }
}

fn folders_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// What is alive right now, from the kernel's locks and the VM's own state.
struct Live {
    creations: Vec<String>,
    normal: Option<String>,
    stale: Vec<HelperProcess>,
}

impl Live {
    fn read(ctx: &Ctx) -> Live {
        let vms = ctx.paths.vms_internal();
        let creations: Vec<String> =
            folders_in(&vms).into_iter().filter(|n| state::run_is_live(&vms.join(n))).collect();
        // A live creation owns its helpers; without one, any that are alive are from a dead run.
        let stale = if creations.is_empty() { helper_processes(ctx.paths) } else { Vec::new() };
        Live { creations, normal: (ctx.normal_vm)(), stale }
    }

    fn vm_block(&self, name: &str) -> String {
        if self.creations.iter().any(|n| n == name) {
            "Its setup is running now. Stop it on its progress screen first.".into()
        } else if self.normal.as_deref() == Some(name) {
            "This VM is running now. Shut it down first.".into()
        } else if !self.stale.is_empty() {
            "A process from an earlier run is still alive. Stop it under Leftovers first.".into()
        } else {
            String::new()
        }
    }

    /// Why shared state (sockets, temporary files) must be left alone; "" when it can go.
    fn idle_block(&self) -> String {
        if let Some(name) = self.creations.first() {
            format!("{name} is being set up right now.")
        } else if let Some(name) = &self.normal {
            format!("{name} is running right now. Shut it down first.")
        } else if !self.stale.is_empty() {
            "A process from an earlier run is still alive. Stop it first.".into()
        } else {
            String::new()
        }
    }
}

/// The run's own lock, for the length of an operation on its VM.
fn lock_vm(vm: &Path) -> Result<File, String> {
    let lock = vm.join(state::LOCK_FILE);
    if fs::symlink_metadata(&lock).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{}: is a symbolic link; nothing was removed", lock.display()));
    }
    state::acquire_lock(vm).ok_or_else(|| "A setup holds this VM's lock, so it is in use; nothing was removed.".to_string())
}

/// `<vms>/<name>`, once it is a real folder of the library.
fn vm_folder(paths: &Paths, name: &str) -> Result<PathBuf, String> {
    safe_component(name, "VM name")?;
    if name.starts_with('.') {
        return Err(format!("{name:?} is not a VM; nothing was removed"));
    }
    let vm = paths.vms_internal().join(name);
    let st = scoped(paths, &vm)?.lstat().map_err(|e| format!("{name}: {e}"))?;
    if !is_dir(&st) {
        return Err(format!("{name} is not a real folder (it may be a symbolic link); nothing was removed"));
    }
    Ok(vm)
}

// ------------------------------------------------------------------ the screen's items

#[derive(Serialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VmItem {
    pub name: String,
    pub device: String,
    pub ios: String,
    /// "finished", "running", "paused", "failed", "stopped", "interrupted", "incomplete", "ready".
    pub state: String,
    pub state_text: String,
    pub bytes: u64,
    pub size: String,
    pub log_bytes: u64,
    pub log_size: String,
    /// Why it cannot be touched now; "" when it can.
    pub blocked: String,
    pub reset_blocked: String,
    pub external: bool,
    /// The failing step's log, to read before deciding.
    pub log: String,
}

#[derive(Serialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareItem {
    pub name: String,
    pub bytes: u64,
    pub size: String,
    pub partial: bool,
    pub note: String,
    pub blocked: String,
    #[serde(skip)]
    members: Vec<OsString>,
}

#[derive(Serialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LeftoverItem {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub bytes: u64,
    pub size: String,
    pub blocked: String,
    #[serde(skip)]
    targets: Vec<Target>,
}

#[derive(Clone, Debug)]
enum Target {
    Whole(PathBuf),
    Children(PathBuf),
}

#[derive(Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Scan {
    pub free_bytes: u64,
    pub free_size: String,
    pub total_bytes: u64,
    pub total_size: String,
    pub app_bytes: u64,
    pub app_size: String,
    pub vms_size: String,
    pub firmware_size: String,
    pub leftovers_size: String,
    pub vms: Vec<VmItem>,
    pub firmware: Vec<FirmwareItem>,
    pub leftovers: Vec<LeftoverItem>,
    pub processes: Vec<HelperProcess>,
    pub processes_blocked: String,
    /// Why "Reset everything" is unavailable; "" when it can run.
    pub reset_blocked: String,
    pub reset_bytes: u64,
    pub reset_size: String,
}

fn step_title(key: &str) -> String {
    STEPS.iter().find(|s| s.key == key).map_or_else(|| "the start".to_string(), |s| s.title.to_string())
}

/// `(state, text, failing step's log)` of a VM in the library.
fn vm_state(vm: &Path, live: bool) -> (String, String, String) {
    let paused = vm.join(engine::PAUSE_FILE).exists();
    let status: Option<Status> = state::read_status(vm);
    let step = status.as_ref().map_or_else(String::new, |s| s.step.clone());
    let title = step_title(&step);
    if live {
        return if paused {
            ("paused".into(), format!("Paused at: {title}"), String::new())
        } else {
            ("running".into(), format!("Being set up: {title}"), String::new())
        };
    }
    if state::done(vm, "booted") {
        return ("finished".into(), "Finished".into(), String::new());
    }
    if state::read_spec(vm).is_none() {
        return if crate::bundle::Bundle::open(vm).is_ok() {
            ("ready".into(), "Ready; made outside the app's setup".into(), String::new())
        } else {
            ("incomplete".into(), "Incomplete: not a usable VM".into(), String::new())
        };
    }
    let log = |s: &Status| {
        s.steps
            .iter()
            .find(|st| st.key == s.step)
            .map_or_else(|| state::step_log(vm, &s.step).to_string_lossy().into_owned(), |st| st.log.clone())
    };
    match status.as_ref().map(|s| s.state.as_str()) {
        Some("failed") => ("failed".into(), format!("Failed at: {title}"), status.as_ref().map_or_else(String::new, log)),
        Some("cancelled") if step.is_empty() => ("stopped".into(), "Reset; it starts over when continued".into(), String::new()),
        Some("cancelled") => ("stopped".into(), format!("Stopped at: {title}"), String::new()),
        Some("running") if paused => ("paused".into(), format!("Paused at: {title}"), String::new()),
        Some("running") => ("interrupted".into(), format!("Interrupted at: {title}"), String::new()),
        _ => ("incomplete".into(), "Incomplete: setup has not finished".into(), String::new()),
    }
}

/// A folder's own logs: `.create-logs`, each `.runs-*`, and the serial logs of normal boots.
fn log_targets(vm: &Path) -> Vec<Target> {
    let mut targets = vec![Target::Children(vm.join(state::LOG_DIR))];
    for name in folders_in(vm) {
        if name.starts_with(RUNS_PREFIX) {
            targets.push(Target::Whole(vm.join(name)));
        }
    }
    for f in ["serial.log", "serial.log.1"] {
        targets.push(Target::Whole(vm.join(f)));
    }
    targets
}

fn targets_bytes(targets: &[Target]) -> u64 {
    targets
        .iter()
        .map(|t| match t {
            Target::Whole(p) => size_of(p),
            Target::Children(p) => children_of(p).iter().map(|c| size_of(c)).sum(),
        })
        .sum()
}

fn children_of(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

fn vm_item(ctx: &Ctx, live: &Live, name: &str, external: bool) -> VmItem {
    let library = if external { ctx.paths.vms() } else { ctx.paths.vms_internal() };
    let vm = library.join(name);
    let spec = state::read_spec(&vm);
    let (state_name, state_text, log) = vm_state(&vm, !external && live.creations.iter().any(|n| n == name));
    let bytes = size_of(&vm);
    let log_bytes = targets_bytes(&log_targets(&vm));
    let (device, ios) = spec.map_or_else(Default::default, |s| (s.device, s.ios));
    let blocked = if external {
        "Imported VM on shared storage: this screen only removes things in the app's own storage.".to_string()
    } else {
        live.vm_block(name)
    };
    let reset_blocked = if !blocked.is_empty() {
        blocked.clone()
    } else if state::read_spec(&vm).is_none() {
        "It has no saved setup plan to start over from. Delete it instead.".into()
    } else {
        match super::pending(ctx.paths) {
            Some(other) if other != name => format!("{other} is still being created. Finish or delete it first."),
            _ => String::new(),
        }
    };
    VmItem {
        name: name.to_string(),
        device,
        ios,
        state: state_name,
        state_text,
        bytes,
        size: human(bytes),
        log_bytes,
        log_size: human(log_bytes),
        blocked,
        reset_blocked,
        external,
        log,
    }
}

fn vm_items(ctx: &Ctx, live: &Live) -> Vec<VmItem> {
    let internal: Vec<String> = folders_in(&ctx.paths.vms_internal())
        .into_iter()
        .filter(|n| !n.starts_with('.') && !n.starts_with(TEMP_VM_PREFIX))
        .collect();
    let mut items: Vec<VmItem> = internal.iter().map(|n| vm_item(ctx, live, n, false)).collect();
    for name in folders_in(&ctx.paths.vms()) {
        if !name.starts_with('.') && !internal.contains(&name) {
            items.push(vm_item(ctx, live, &name, true));
        }
    }
    items
}

/// The images each VM still needs, with their sizes where the catalog has them.
fn firmware_needs(paths: &Paths) -> (HashMap<String, u64>, HashMap<String, Vec<String>>, HashSet<String>) {
    let mut sizes = HashMap::new();
    let mut needed: HashMap<String, Vec<String>> = HashMap::new();
    let mut known = HashSet::new();
    let vms = paths.vms_internal();
    for name in folders_in(&vms) {
        let vm = vms.join(&name);
        let Some(spec) = state::read_spec(&vm) else { continue };
        let Some(Firmware { cloud, ios, .. }) = spec.firmware else { continue };
        for image in [cloud, ios] {
            if let Some(size) = image.size {
                sizes.insert(image.file.clone(), size);
            }
            known.insert(image.file.clone());
            if !state::done(&vm, "booted") {
                needed.entry(image.file).or_default().push(name.clone());
            }
        }
    }
    (sizes, needed, known)
}

/// The files a running creation has planned, plus those of the shared assets it uses.
fn in_use_firmware(paths: &Paths, live: &Live, item_name: &str, known: &HashSet<String>) -> String {
    let Some(creating) = live.creations.first() else {
        return String::new();
    };
    let vm = paths.vms_internal().join(creating);
    let planned = state::read_spec(&vm)
        .and_then(|s| s.firmware)
        .is_some_and(|f| f.cloud.file == item_name || f.ios.file == item_name);
    // Anything but an image another VM is known to want may be a shared asset of the running step.
    if planned || !(known.contains(item_name) || item_name.ends_with(".ipsw")) {
        format!("{creating} is using this while it is being set up.")
    } else {
        String::new()
    }
}

fn firmware_items(ctx: &Ctx, live: &Live) -> Vec<FirmwareItem> {
    let cache = ctx.paths.fw_cache();
    let mut groups: BTreeMap<String, Vec<OsString>> = BTreeMap::new();
    for path in children_of(&cache) {
        let Some(name) = path.file_name() else { continue };
        let text = name.to_string_lossy().into_owned();
        let base = DOWNLOAD_SUFFIXES
            .iter()
            .find_map(|s| text.strip_suffix(s))
            .filter(|b| !b.is_empty())
            .unwrap_or(&text)
            .to_string();
        groups.entry(base).or_default().push(name.to_os_string());
    }
    let (sizes, needed, known) = firmware_needs(ctx.paths);
    let mut items = Vec::new();
    for (name, members) in groups {
        let bytes: u64 = members.iter().map(|m| size_of(&cache.join(m))).sum();
        let whole = cache.join(&name);
        let part = cache.join(format!("{name}.part"));
        let unfinished_tree = name == GPU_EXTRACT
            || (name == RELEASE_DIR && whole.is_dir() && !whole.join(".unpacked").exists());
        let partial = (part.exists() && !whole.is_file()) || unfinished_tree;
        let mut notes = Vec::new();
        if partial && part.exists() {
            let have = fs::symlink_metadata(&part).map_or(0, |m| m.len());
            notes.push(match sizes.get(&name) {
                Some(total) => format!("Partial download: {} of {}; it can resume.", human(have), human(*total)),
                None => format!("Partial download: {} so far; it can resume.", human(have)),
            });
        } else if unfinished_tree {
            notes.push("Unfinished: a setup stopped partway through unpacking this.".into());
        }
        if let Some(vms) = needed.get(&name) {
            notes.push(format!("Needed by {}: it would be fetched again.", vms.join(", ")));
        }
        let blocked = in_use_firmware(ctx.paths, live, &name, &known);
        items.push(FirmwareItem { name, bytes, size: human(bytes), partial, note: notes.join(" "), blocked, members });
    }
    items
}

fn leftover(id: String, title: String, detail: String, blocked: String, targets: Vec<Target>) -> LeftoverItem {
    let bytes = targets_bytes(&targets);
    LeftoverItem { id, title, detail, bytes, size: human(bytes), blocked, targets }
}

fn count_files(dir: &Path) -> usize {
    children_of(dir).len()
}

fn files_detail(dir: &Path) -> String {
    match count_files(dir) {
        1 => "1 item".to_string(),
        n => format!("{n} items"),
    }
}

/// Everything not in use that is a VM's by name only: created by runs that are over.
fn leftover_items(ctx: &Ctx, live: &Live) -> Vec<LeftoverItem> {
    let paths = ctx.paths;
    let idle = live.idle_block();
    let mut items = Vec::new();
    let library = paths.vms_internal();

    for path in children_of(&library) {
        let Some(name) = path.file_name().and_then(OsStr::to_str).map(str::to_string) else { continue };
        let real_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        if real_dir && name.starts_with(TEMP_VM_PREFIX) {
            items.push(leftover(
                format!("temp-vm:{name}"),
                format!("Temporary VM {name}"),
                "Left behind by a GPU driver step that did not finish.".into(),
                live.creations.first().map_or_else(|| idle.clone(), |c| format!("{c} is using this while it is being set up.")),
                vec![Target::Whole(path.clone())],
            ));
        } else if name.starts_with('.') || !real_dir {
            items.push(leftover(
                format!("orphan:{name}"),
                format!("Stray item {name}"),
                "In the VM library, but not a VM.".into(),
                String::new(),
                vec![Target::Whole(path.clone())],
            ));
        }
    }
    for name in folders_in(&library) {
        let vm = library.join(&name);
        if name.starts_with('.') || name.starts_with(TEMP_VM_PREFIX) || !state::done(&vm, "booted") {
            continue;
        }
        let runs: Vec<Target> = folders_in(&vm)
            .into_iter()
            .filter(|n| n.starts_with(RUNS_PREFIX))
            .map(|n| Target::Whole(vm.join(n)))
            .collect();
        if !runs.is_empty() {
            items.push(leftover(
                format!("vm-runs:{name}"),
                format!("Old run folders of {name}"),
                "Temporary boot logs of its finished setup.".into(),
                live.vm_block(&name),
                runs,
            ));
        }
    }
    let shared = [
        ("runs", "Old run folders", "GPU records from past VM runs.", paths.files.join("runs")),
        ("sockets", "Stale sockets and run state", "Left by helpers of a run that is over.", paths.create_sockets()),
        ("vsock", "Stale guest socket", "Left by a VM that is not running.", paths.files.join("vsock")),
        ("home", "Helper home folder", "Scratch space of the setup tools.", paths.files.join("home")),
        ("cache", "Temporary files", "The app's cache folder.", paths.cache.clone()),
        ("trash", "Unfinished deletes", "A delete that was interrupted.", paths.files.join(TRASH_DIR)),
    ];
    for (id, title, detail, dir) in shared {
        if count_files(&dir) > 0 {
            let blocked = match (id, super::pending(paths)) {
                ("trash", _) => String::new(),
                // A stopped creation may still want what its tools keep here when it continues.
                ("home", Some(name)) if idle.is_empty() => format!("{name} may need this to continue. Finish or delete it first."),
                _ => idle.clone(),
            };
            let detail = format!("{detail} {}.", files_detail(&dir));
            items.push(leftover(id.to_string(), title.to_string(), detail, blocked, vec![Target::Children(dir)]));
        }
    }
    if paths.qmp_socket().exists() {
        items.push(leftover(
            "qmp".into(),
            "Stale VM control socket".into(),
            "Left by a VM that is not running.".into(),
            idle.clone(),
            vec![Target::Whole(paths.qmp_socket())],
        ));
    }
    let active = super::active_file(paths);
    if active.is_file() && super::pending(paths).is_none() {
        items.push(leftover(
            "active".into(),
            "Record of a removed creation".into(),
            "Points at a VM that no longer exists.".into(),
            String::new(),
            vec![Target::Whole(active)],
        ));
    }
    let last_run = paths.last_run();
    if let Some(vm) = last_run_vm(&last_run).filter(|n| !paths.vm_dir(n).exists()) {
        items.push(leftover(
            "last-run".into(),
            "Record of a removed VM's last run".into(),
            format!("About {vm}, which no longer exists."),
            String::new(),
            vec![Target::Whole(last_run)],
        ));
    }
    items
}

fn last_run_vm(file: &Path) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    value.get("vm")?.as_str().filter(|n| !n.is_empty()).map(str::to_string)
}

pub fn scan(ctx: &Ctx) -> Scan {
    let paths = ctx.paths;
    let live = Live::read(ctx);
    let vms = vm_items(ctx, &live);
    let firmware = firmware_items(ctx, &live);
    let leftovers = leftover_items(ctx, &live);
    let firmware_bytes = size_of(&paths.fw_cache());
    let leftover_bytes: u64 = leftovers.iter().map(|l| l.bytes).sum();
    let free = free_bytes(&paths.files);
    let total = device_total(&paths.files);
    let app = size_of(&paths.files) + size_of(&paths.cache) + size_of(&paths.external);
    let all_vms: u64 = vms.iter().map(|v| v.bytes).sum();
    let reset_blocked = live.idle_block();
    let processes_blocked = live.creations.first().map_or_else(String::new, |c| format!("{c} is using these."));
    let reset_bytes = size_of(&paths.files) + size_of(&paths.cache);
    Scan {
        free_bytes: free,
        free_size: human(free),
        total_bytes: total,
        total_size: human(total),
        app_bytes: app,
        app_size: human(app),
        vms_size: human(all_vms),
        firmware_size: human(firmware_bytes),
        leftovers_size: human(leftover_bytes),
        vms,
        firmware,
        leftovers,
        processes: live.stale,
        processes_blocked,
        reset_blocked,
        reset_bytes,
        reset_size: human(reset_bytes),
    }
}

// ------------------------------------------------------------------ plans

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct Op {
    pub op: String,
    /// A VM's name, a firmware file's name, or a leftover's id.
    pub name: String,
}

/// What a confirmation says: exactly what goes, measured now.
#[derive(Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub title: String,
    pub text: String,
    pub action: String,
    pub bytes: u64,
    pub size: String,
    /// Non-empty: this cannot run now, and why.
    pub blocked: String,
    /// When set, the person must type this word to go ahead.
    pub confirm_word: String,
}

fn list_children(dir: &Path, keep: &[&str]) -> Vec<String> {
    let mut sized: Vec<(String, u64)> = children_of(dir)
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            (!keep.contains(&name.as_str())).then(|| (name, size_of(p)))
        })
        .collect();
    sized.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
    let mut lines: Vec<String> =
        sized.iter().take(LISTED_CHILDREN).map(|(n, b)| format!("  • {n} ({})", human(*b))).collect();
    if sized.len() > LISTED_CHILDREN {
        let rest: u64 = sized[LISTED_CHILDREN..].iter().map(|(_, b)| b).sum();
        lines.push(format!("  • and {} more ({})", sized.len() - LISTED_CHILDREN, human(rest)));
    }
    lines
}

fn describe_targets(targets: &[Target]) -> Vec<String> {
    targets
        .iter()
        .filter(|t| !matches!(t, Target::Whole(p) if fs::symlink_metadata(p).is_err()))
        .map(|t| match t {
            Target::Whole(p) => format!("  • {} ({})", p.display(), human(size_of(p))),
            Target::Children(p) => format!(
                "  • everything inside {} ({})",
                p.display(),
                human(children_of(p).iter().map(|c| size_of(c)).sum())
            ),
        })
        .collect()
}

fn find_vm(ctx: &Ctx, live: &Live, name: &str) -> Result<VmItem, String> {
    vm_folder(ctx.paths, name)?;
    Ok(vm_item(ctx, live, name, false))
}

fn plan_for(ctx: &Ctx, op: &Op) -> Result<Plan, String> {
    let paths = ctx.paths;
    let live = Live::read(ctx);
    match op.op.as_str() {
        "deleteVm" => {
            let vm = find_vm(ctx, &live, &op.name)?;
            let dir = paths.vms_internal().join(&op.name);
            let mut lines = vec![format!("Removes the folder of {} ({}), with everything in it:", vm.name, vm.size)];
            lines.extend(list_children(&dir, &[]));
            lines.push("\nThe shared firmware downloads are kept.".into());
            Ok(Plan {
                title: format!("Delete {}?", vm.name),
                text: lines.join("\n"),
                action: "Delete".into(),
                bytes: vm.bytes,
                size: vm.size,
                blocked: vm.blocked,
                confirm_word: String::new(),
            })
        }
        "resetVm" => {
            let vm = find_vm(ctx, &live, &op.name)?;
            let dir = paths.vms_internal().join(&op.name);
            let keep = [state::SPEC_FILE, state::LOCK_FILE];
            let bytes: u64 = children_of(&dir)
                .iter()
                .filter(|p| p.file_name().is_none_or(|n| !keep.iter().any(|k| OsStr::new(k) == n)))
                .map(|p| size_of(p))
                .sum();
            let mut lines = vec![format!("Removes the setup of {} ({}):", vm.name, human(bytes))];
            lines.extend(list_children(&dir, &keep));
            lines.push("\nKeeps its plan (device and iOS version) and the shared firmware downloads. It starts over from the first step when you continue it.".into());
            Ok(Plan {
                title: format!("Reset {}?", vm.name),
                text: lines.join("\n"),
                action: "Reset".into(),
                bytes,
                size: human(bytes),
                blocked: vm.reset_blocked,
                confirm_word: String::new(),
            })
        }
        "clearVmLogs" => {
            let vm = find_vm(ctx, &live, &op.name)?;
            let targets = log_targets(&paths.vms_internal().join(&op.name));
            let bytes = targets_bytes(&targets);
            let mut lines = vec![format!("Removes the logs and run folders of {} ({}):", vm.name, human(bytes))];
            lines.extend(describe_targets(&targets));
            lines.push("\nIts disk and setup progress are kept.".into());
            Ok(Plan {
                title: format!("Clear logs of {}?", vm.name),
                text: lines.join("\n"),
                action: "Clear".into(),
                bytes,
                size: human(bytes),
                blocked: vm.blocked,
                confirm_word: String::new(),
            })
        }
        "deleteFirmware" => {
            safe_component(&op.name, "firmware file")?;
            let item = firmware_items(ctx, &live)
                .into_iter()
                .find(|f| f.name == op.name)
                .ok_or_else(|| format!("{} is not in the firmware cache", op.name))?;
            let mut text = format!("Removes {} ({}){} from the firmware cache.",
                item.name, item.size, if item.partial { ", a partial download," } else { "" });
            if !item.note.is_empty() {
                text += &format!("\n\n{}", item.note);
            }
            Ok(Plan {
                title: format!("Delete {}?", item.name),
                text,
                action: "Delete".into(),
                bytes: item.bytes,
                size: item.size,
                blocked: item.blocked,
                confirm_word: String::new(),
            })
        }
        "deleteAllFirmware" => {
            let items = firmware_items(ctx, &live);
            let (free, in_use): (Vec<_>, Vec<_>) = items.iter().partition(|f| f.blocked.is_empty());
            let bytes: u64 = free.iter().map(|f| f.bytes).sum();
            let mut lines = vec![format!("Removes {} item(s) from the firmware cache ({}):", free.len(), human(bytes))];
            lines.extend(free.iter().map(|f| format!("  • {} ({})", f.name, f.size)));
            if !in_use.is_empty() {
                lines.push(format!("\nKept, as they are in use: {}.", in_use.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")));
            }
            let needed: Vec<&str> = free.iter().filter(|f| f.note.contains("Needed by")).map(|f| f.note.as_str()).collect();
            if !needed.is_empty() {
                lines.push(format!("\n{}", needed.join("\n")));
            }
            Ok(Plan {
                title: "Delete the firmware cache?".into(),
                text: lines.join("\n"),
                action: "Delete".into(),
                bytes,
                size: human(bytes),
                blocked: if free.is_empty() { "Nothing here can be deleted now.".into() } else { String::new() },
                confirm_word: String::new(),
            })
        }
        "clearLeftover" => {
            let item = leftover_items(ctx, &live)
                .into_iter()
                .find(|l| l.id == op.name)
                .ok_or_else(|| "That leftover is no longer there.".to_string())?;
            let mut lines = vec![format!("Removes {} ({}):", item.title, item.size)];
            lines.extend(describe_targets(&item.targets));
            Ok(Plan {
                title: format!("Clear {}?", item.title),
                text: lines.join("\n"),
                action: "Clear".into(),
                bytes: item.bytes,
                size: item.size,
                blocked: item.blocked,
                confirm_word: String::new(),
            })
        }
        "clearAllLeftovers" => {
            let items = leftover_items(ctx, &live);
            let (free, in_use): (Vec<_>, Vec<_>) = items.iter().partition(|l| l.blocked.is_empty());
            let bytes: u64 = free.iter().map(|l| l.bytes).sum();
            let mut lines = vec![format!("Removes {} leftover(s) ({}):", free.len(), human(bytes))];
            lines.extend(free.iter().map(|l| format!("  • {} ({})", l.title, l.size)));
            if !in_use.is_empty() {
                lines.push(format!("\nKept, as they are in use: {}.", in_use.iter().map(|l| l.title.as_str()).collect::<Vec<_>>().join(", ")));
            }
            Ok(Plan {
                title: "Clear the leftovers?".into(),
                text: lines.join("\n"),
                action: "Clear".into(),
                bytes,
                size: human(bytes),
                blocked: if free.is_empty() { "Nothing here can be cleared now.".into() } else { String::new() },
                confirm_word: String::new(),
            })
        }
        "stopProcesses" => {
            let mut lines = vec!["Stops these helpers from a run that is over. They are this app's own; nothing else is signalled:".to_string()];
            lines.extend(live.stale.iter().map(|p| format!("  • {} (process {})", p.name, p.pid)));
            lines.push("\nThis frees no space itself; it makes what they hold removable.".into());
            Ok(Plan {
                title: "Stop leftover processes?".into(),
                text: lines.join("\n"),
                action: "Stop".into(),
                bytes: 0,
                size: human(0),
                blocked: if live.stale.is_empty() { "No leftover processes are alive.".into() } else { live.creations.first().map_or_else(String::new, |c| format!("{c} is using these.")) },
                confirm_word: String::new(),
            })
        }
        "resetEverything" => {
            let bytes = size_of(&paths.files) + size_of(&paths.cache);
            let vms = vm_items(ctx, &live);
            let internal: Vec<String> = vms.iter().filter(|v| !v.external).map(|v| format!("  • {} ({})", v.name, v.size)).collect();
            let imported: Vec<&str> = vms.iter().filter(|v| v.external).map(|v| v.name.as_str()).collect();
            let mut lines = vec![format!("Removes ALL of this app's data in its own storage ({}), and returns it to its first-run state:", human(bytes))];
            lines.push(format!("\nEvery VM ({}):", internal.len()));
            lines.extend(internal);
            lines.push(format!("\nThe firmware cache ({}), which is about 12 GB to download again.", human(size_of(&paths.fw_cache()))));
            lines.push("Every leftover, log, setup record and pairing record.".into());
            if !imported.is_empty() {
                lines.push(format!("\nNot removed: imported VMs on shared storage ({}).", imported.join(", ")));
            }
            lines.push("\nThis cannot be undone.".into());
            Ok(Plan {
                title: "Reset everything?".into(),
                text: lines.join("\n"),
                action: "Erase everything".into(),
                bytes,
                size: human(bytes),
                blocked: live.idle_block(),
                confirm_word: "RESET".into(),
            })
        }
        other => Err(format!("unknown storage action {other:?}")),
    }
}

pub fn plan(ctx: &Ctx, op: &Op) -> Plan {
    plan_for(ctx, op).unwrap_or_else(|e| Plan {
        title: "Cannot do this".into(),
        text: e.clone(),
        action: String::new(),
        blocked: e,
        size: human(0),
        ..Default::default()
    })
}

// ------------------------------------------------------------------ doing it

fn forget_records(paths: &Paths, name: &str) {
    if super::pending(paths).as_deref() == Some(name) {
        let _ = fs::remove_file(super::active_file(paths));
    }
    if last_run_vm(&paths.last_run()).as_deref() == Some(name) {
        let _ = fs::remove_file(paths.last_run());
    }
}

fn apply_target(paths: &Paths, target: &Target, out: &mut Outcome) -> bool {
    match target {
        Target::Whole(p) => remove_path(paths, p, out),
        Target::Children(p) => remove_children(paths, p, &[], out),
    }
}

/// Renames the VM away and hands back where it went, once no boot has taken it in the meantime.
fn claim_vm(ctx: &Ctx, name: &str, vm: &Path) -> Result<PathBuf, String> {
    let trashed = to_trash(ctx.paths, vm)?;
    // A boot that began before the rename shows in the VM's state now; one that begins after it
    // finds no folder and stops with an error.
    if (ctx.normal_vm)().is_some_and(|n| n == name) {
        return match fs::rename(&trashed, vm) {
            Ok(()) => Err(format!("{name} started while this ran. Nothing was removed.")),
            Err(e) => Err(format!("{name} started while this ran, and could not be put back: {e}; it is at {}", trashed.display())),
        };
    }
    Ok(trashed)
}

fn delete_vm(ctx: &Ctx, name: &str, out: &mut Outcome) -> Result<(), String> {
    let vm = vm_folder(ctx.paths, name)?;
    let why = Live::read(ctx).vm_block(name);
    if !why.is_empty() {
        return Err(why);
    }
    let _lock = lock_vm(&vm)?;
    let size = size_of(&vm);
    note_total(size);
    let trashed = claim_vm(ctx, name, &vm)?;
    forget_records(ctx.paths, name);
    if remove_path(ctx.paths, &trashed, out) {
        out.removed.push(format!("VM {name} ({})", human(size)));
    } else {
        out.kept.push(format!("{}: what could not be removed is listed under Leftovers", trashed.display()));
    }
    Ok(())
}

fn reset_vm(ctx: &Ctx, name: &str, out: &mut Outcome) -> Result<(), String> {
    let vm = vm_folder(ctx.paths, name)?;
    let live = Live::read(ctx);
    let why = live.vm_block(name);
    if !why.is_empty() {
        return Err(why);
    }
    if let Some(other) = super::pending(ctx.paths).filter(|o| o != name) {
        return Err(format!("{other} is still being created. Finish or delete it first."));
    }
    let spec = state::read_spec(&vm).ok_or("It has no saved setup plan to start over from. Delete it instead.")?;
    let _lock = lock_vm(&vm)?;
    let size = size_of(&vm);
    note_total(size);

    // The fresh folder is made first and takes the old one's place in one step.
    let fresh = ctx.paths.vms_internal().join(format!(".reset-{name}"));
    let _ = fs::remove_dir_all(&fresh);
    fs::create_dir_all(fresh.join(state::LOG_DIR)).map_err(|e| format!("cannot prepare the reset: {e}"))?;
    state::write_spec(&fresh, &spec)?;
    state::write_status(
        &fresh,
        &Status {
            name: name.to_string(),
            device: spec.device.clone(),
            ios: spec.ios.clone(),
            state: "cancelled".into(),
            message: "Reset. Tap Resume to start over from the beginning.".into(),
            updated_ms: state::now_ms(),
            ..Default::default()
        },
    );
    let trashed = claim_vm(ctx, name, &vm).inspect_err(|_| {
        let _ = fs::remove_dir_all(&fresh);
    })?;
    fs::rename(&fresh, &vm).map_err(|e| format!("cannot put the fresh folder in place: {e}; the old one is at {}", trashed.display()))?;
    forget_records(ctx.paths, name);
    fs::create_dir_all(ctx.paths.files.join("create")).map_err(|e| e.to_string())?;
    state::write_atomic(&super::active_file(ctx.paths), name.as_bytes()).map_err(|e| e.to_string())?;

    if remove_path(ctx.paths, &trashed, out) {
        out.removed.push(format!("the setup of {name} ({})", human(size)));
    } else {
        out.kept.push(format!("{}: what could not be removed is listed under Leftovers", trashed.display()));
    }
    out.kept.push(format!("the plan of {name} and the shared firmware downloads"));
    Ok(())
}

fn clear_vm_logs(ctx: &Ctx, name: &str, out: &mut Outcome) -> Result<(), String> {
    let vm = vm_folder(ctx.paths, name)?;
    let why = Live::read(ctx).vm_block(name);
    if !why.is_empty() {
        return Err(why);
    }
    let _lock = lock_vm(&vm)?;
    let targets = log_targets(&vm);
    let size = targets_bytes(&targets);
    note_total(size);
    let mut all = true;
    for t in &targets {
        all &= apply_target(ctx.paths, t, out);
    }
    if all {
        out.removed.push(format!("the logs and run folders of {name} ({})", human(size)));
    }
    out.kept.push(format!("the disk and setup progress of {name}"));
    Ok(())
}

fn remove_firmware_item(paths: &Paths, item: &FirmwareItem, out: &mut Outcome) {
    let cache = paths.fw_cache();
    let mut all = true;
    for member in &item.members {
        all &= remove_path(paths, &cache.join(member), out);
    }
    if all {
        out.removed.push(format!("{} ({})", item.name, item.size));
    } else {
        out.kept.push(item.name.clone());
    }
}

fn delete_firmware(ctx: &Ctx, name: &str, out: &mut Outcome) -> Result<(), String> {
    safe_component(name, "firmware file")?;
    let live = Live::read(ctx);
    let item = firmware_items(ctx, &live)
        .into_iter()
        .find(|f| f.name == name)
        .ok_or_else(|| format!("{name} is not in the firmware cache"))?;
    if !item.blocked.is_empty() {
        return Err(item.blocked);
    }
    note_total(item.bytes);
    remove_firmware_item(ctx.paths, &item, out);
    Ok(())
}

fn delete_all_firmware(ctx: &Ctx, out: &mut Outcome) -> Result<(), String> {
    let live = Live::read(ctx);
    let items = firmware_items(ctx, &live);
    note_total(items.iter().filter(|f| f.blocked.is_empty()).map(|f| f.bytes).sum());
    for item in &items {
        if item.blocked.is_empty() {
            remove_firmware_item(ctx.paths, item, out);
        } else {
            out.kept.push(format!("{} ({})", item.name, item.blocked));
        }
    }
    Ok(())
}

fn clear_leftover_item(paths: &Paths, item: &LeftoverItem, out: &mut Outcome) {
    let mut all = true;
    for target in &item.targets {
        all &= apply_target(paths, target, out);
    }
    if all {
        out.removed.push(format!("{} ({})", item.title, item.size));
    } else {
        out.kept.push(item.title.clone());
    }
}

fn clear_leftover(ctx: &Ctx, id: &str, out: &mut Outcome) -> Result<(), String> {
    let live = Live::read(ctx);
    let item = leftover_items(ctx, &live)
        .into_iter()
        .find(|l| l.id == id)
        .ok_or("That leftover is no longer there.")?;
    if !item.blocked.is_empty() {
        return Err(item.blocked);
    }
    note_total(item.bytes);
    clear_leftover_item(ctx.paths, &item, out);
    Ok(())
}

fn clear_all_leftovers(ctx: &Ctx, out: &mut Outcome) -> Result<(), String> {
    let live = Live::read(ctx);
    let items = leftover_items(ctx, &live);
    note_total(items.iter().filter(|l| l.blocked.is_empty()).map(|l| l.bytes).sum());
    for item in &items {
        if item.blocked.is_empty() {
            clear_leftover_item(ctx.paths, item, out);
        } else {
            out.kept.push(format!("{} ({})", item.title, item.blocked));
        }
    }
    Ok(())
}

fn wait_gone(paths: &Paths, list: &[HelperProcess], limit: Duration) -> Vec<HelperProcess> {
    let still = || list.iter().filter(|p| helper_at(paths, p.pid).is_some_and(|q| q.started == p.started)).cloned().collect::<Vec<_>>();
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if still().is_empty() {
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    still()
}

/// SIGTERM, then SIGKILL. Each process is re-identified just before it is signalled.
fn stop_processes(ctx: &Ctx, out: &mut Outcome) -> Result<(), String> {
    let live = Live::read(ctx);
    if let Some(c) = live.creations.first() {
        return Err(format!("{c} is using these."));
    }
    let list = live.stale;
    if list.is_empty() {
        return Err("No leftover processes are alive.".into());
    }
    let send = |procs: &[HelperProcess], signal: i32| {
        for p in procs {
            if helper_at(ctx.paths, p.pid).is_some_and(|q| q.started == p.started) {
                // SAFETY: pid was just confirmed to be this app's own helper process.
                unsafe { libc::kill(p.pid, signal) };
            }
        }
    };
    note_total(list.len() as u64);
    send(&list, libc::SIGTERM);
    let mut remaining = wait_gone(ctx.paths, &list, STOP_WAIT);
    if !remaining.is_empty() {
        send(&remaining, libc::SIGKILL);
        remaining = wait_gone(ctx.paths, &remaining, STOP_WAIT);
    }
    for p in &list {
        if remaining.iter().any(|r| r.pid == p.pid) {
            out.failures.push(Failure { path: format!("{} (process {})", p.name, p.pid), error: "did not stop".into() });
        } else {
            out.removed.push(format!("{} (process {})", p.name, p.pid));
        }
    }
    Ok(())
}

fn reset_everything(ctx: &Ctx, out: &mut Outcome) -> Result<(), String> {
    let paths = ctx.paths;
    let live = Live::read(ctx);
    let why = live.idle_block();
    if !why.is_empty() {
        return Err(why);
    }
    note_total(size_of(&paths.files) + size_of(&paths.cache));

    let library = paths.vms_internal();
    // Every lock first: if any VM is in use, nothing has been touched yet.
    let mut locks = Vec::new();
    for name in folders_in(&library) {
        locks.push(lock_vm(&library.join(&name)).map_err(|e| format!("{name}: {e}"))?);
    }

    let mut claimed: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in children_of(&library) {
        let real_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        if !real_dir {
            remove_path(paths, &path, out);
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match to_trash(paths, &path) {
            Ok(t) => {
                if (ctx.normal_vm)().is_some() {
                    let _ = fs::rename(&t, &path);
                    for (from, to) in &claimed {
                        let _ = fs::rename(to, from);
                    }
                    return Err(format!("{name}: a VM started while this ran. Nothing was removed."));
                }
                claimed.push((path, t));
            }
            Err(e) => out.fail(&path, e),
        }
    }
    drop(locks);

    for (from, trashed) in &claimed {
        if remove_path(paths, trashed, out) {
            out.removed.push(format!("VM {}", from.file_name().unwrap_or_default().to_string_lossy()));
        }
    }
    remove_children(paths, &paths.files, &["vms", GATE_FILE, TRASH_DIR], out);
    remove_children(paths, &paths.cache, &[], out);
    remove_children(paths, &paths.files.join(TRASH_DIR), &[], out);
    remove_path(paths, &paths.files.join(TRASH_DIR), out);
    out.removed.push("the firmware cache, logs, sockets, temporary files and setup records".into());
    let imported = folders_in(&paths.vms());
    if !imported.is_empty() {
        out.kept.push(format!("imported VMs on shared storage: {}", imported.join(", ")));
    }
    Ok(())
}

fn perform(ctx: &Ctx, op: &Op, out: &mut Outcome) -> Result<(), String> {
    match op.op.as_str() {
        "deleteVm" => delete_vm(ctx, &op.name, out),
        "resetVm" => reset_vm(ctx, &op.name, out),
        "clearVmLogs" => clear_vm_logs(ctx, &op.name, out),
        "deleteFirmware" => delete_firmware(ctx, &op.name, out),
        "deleteAllFirmware" => delete_all_firmware(ctx, out),
        "clearLeftover" => clear_leftover(ctx, &op.name, out),
        "clearAllLeftovers" => clear_all_leftovers(ctx, out),
        "stopProcesses" => stop_processes(ctx, out),
        "resetEverything" => reset_everything(ctx, out),
        other => Err(format!("unknown storage action {other:?}")),
    }
}

// ------------------------------------------------------------------ results and progress

/// What the screen reports when an operation ends.
#[derive(Serialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub title: String,
    /// Nothing failed and nothing was refused.
    pub ok: bool,
    /// False when the operation was refused before anything was touched.
    pub attempted: bool,
    pub freed_bytes: u64,
    pub freed: String,
    pub free_before: u64,
    pub free_after: u64,
    pub removed: Vec<String>,
    pub kept: Vec<String>,
    pub failures: Vec<Failure>,
    /// The whole outcome, to show and to copy.
    pub report: String,
}

fn report_text(r: &JobResult) -> String {
    let mut lines = vec![r.title.clone(), String::new()];
    if !r.attempted {
        lines.push("Nothing was removed.".into());
    } else {
        lines.push(format!("Freed {}, measured from what was removed.", r.freed));
        lines.push(format!(
            "Free space on the phone: {} before, {} after.",
            human(r.free_before),
            human(r.free_after)
        ));
    }
    if !r.removed.is_empty() {
        lines.push("\nRemoved:".into());
        lines.extend(r.removed.iter().map(|m| format!("  • {m}")));
    }
    if !r.kept.is_empty() {
        lines.push("\nKept:".into());
        lines.extend(r.kept.iter().map(|m| format!("  • {m}")));
    }
    if !r.failures.is_empty() {
        lines.push(format!("\nCould not remove ({}):", r.failures.len()));
        lines.extend(r.failures.iter().map(|f| format!("  • {}: {}", f.path, f.error)));
    }
    lines.join("\n")
}

fn execute(ctx: &Ctx, op: &Op, title: &str) -> JobResult {
    let free_before = free_bytes(&ctx.paths.files);
    let gate = take_gate(ctx.paths);
    let mut out = Outcome::default();
    let outcome = gate.and_then(|_gate| perform(ctx, op, &mut out));
    let refused = outcome.as_ref().err().cloned();
    let attempted = refused.is_none() || out.freed > 0 || !out.removed.is_empty() || !out.failures.is_empty();
    if let Some(reason) = &refused {
        out.failures.push(Failure { path: String::new(), error: reason.clone() });
    }
    let free_after = free_bytes(&ctx.paths.files);
    let mut result = JobResult {
        title: title.to_string(),
        ok: refused.is_none() && out.failures.is_empty(),
        attempted,
        freed_bytes: out.freed,
        freed: human(out.freed),
        free_before,
        free_after,
        removed: out.removed,
        kept: out.kept,
        failures: out.failures,
        report: String::new(),
    };
    result.report = report_text(&result);
    result
}

#[derive(Serialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct Progress {
    active: bool,
    title: String,
    done_bytes: u64,
    total_bytes: u64,
    current: String,
    result: Option<JobResult>,
}

static JOB: Mutex<Option<Progress>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn with_job(f: impl FnOnce(&mut Progress)) {
    if let Some(p) = JOB.lock().unwrap().as_mut() {
        f(p);
    }
}

fn note_total(total: u64) {
    with_job(|p| p.total_bytes = total);
}

fn note_current(path: &Path) {
    with_job(|p| p.current = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
}

fn note_done(bytes: u64) {
    with_job(|p| p.done_bytes = p.done_bytes.saturating_add(bytes));
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

fn op_title(op: &Op) -> String {
    match op.op.as_str() {
        "deleteVm" => format!("Deleting {}", op.name),
        "resetVm" => format!("Resetting {}", op.name),
        "clearVmLogs" => format!("Clearing the logs of {}", op.name),
        "deleteFirmware" => format!("Deleting {}", op.name),
        "deleteAllFirmware" => "Deleting the firmware cache".into(),
        "clearLeftover" | "clearAllLeftovers" => "Clearing leftovers".into(),
        "stopProcesses" => "Stopping leftover processes".into(),
        "resetEverything" => "Erasing everything".into(),
        _ => "Cleaning up".into(),
    }
}

/// Runs an operation on its own thread. It outlives the screen; its result waits to be picked up.
pub fn start(paths: &'static Paths, op_json: &str) -> Result<(), String> {
    let op: Op = serde_json::from_str(op_json).map_err(|e| e.to_string())?;
    let title = op_title(&op);
    {
        let mut job = JOB.lock().unwrap();
        if job.as_ref().is_some_and(|p| p.active) {
            return Err("Another cleanup is still running.".into());
        }
        *job = Some(Progress { active: true, title: title.clone(), ..Default::default() });
    }
    std::thread::Builder::new()
        .name("storage-job".into())
        .spawn(move || {
            let ctx = Ctx::new(paths);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&ctx, &op, &title)))
                .unwrap_or_else(|_| {
                    let mut r = JobResult {
                        title: title.clone(),
                        attempted: true,
                        failures: vec![Failure { path: String::new(), error: "internal error; see logcat".into() }],
                        ..Default::default()
                    };
                    r.report = report_text(&r);
                    r
                });
            GENERATION.fetch_add(1, Ordering::SeqCst);
            with_job(|p| {
                p.active = false;
                p.result = Some(result);
            });
        })
        .map(drop)
        .map_err(|e| e.to_string())
}

pub fn scan_json(paths: &Paths) -> String {
    serde_json::to_string(&scan(&Ctx::new(paths))).unwrap_or_default()
}

pub fn plan_json(paths: &Paths, op_json: &str) -> String {
    let op: Op = serde_json::from_str(op_json).unwrap_or_default();
    serde_json::to_string(&plan(&Ctx::new(paths), &op)).unwrap_or_default()
}

pub fn progress_json() -> String {
    let job = JOB.lock().unwrap();
    serde_json::to_string(&job.clone().unwrap_or_default()).unwrap_or_default()
}

pub fn take_result() {
    let mut job = JOB.lock().unwrap();
    if job.as_ref().is_some_and(|p| !p.active) {
        *job = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create::compat;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::process::ExitStatusExt;

    fn fixture(name: &str) -> Paths {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/create-storage").join(name);
        let _ = fs::remove_dir_all(&d);
        let p = Paths { files: d.join("files"), cache: d.join("cache"), external: d.join("ext"), native_lib: d.join("lib") };
        for dir in [p.vms_internal(), p.fw_cache(), p.create_sockets(), p.cache.join("tmp"), p.vms(), p.native_lib.clone()] {
            fs::create_dir_all(dir).unwrap();
        }
        p
    }

    fn no_vm() -> Option<String> {
        None
    }

    fn ctx(p: &Paths) -> Ctx<'_> {
        Ctx { paths: p, normal_vm: no_vm }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn sparse(path: &Path, length: u64, data: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = File::create(path).unwrap();
        f.set_len(length).unwrap();
        f.seek(SeekFrom::Start(length / 2)).unwrap();
        f.write_all(&vec![0x5au8; data as usize]).unwrap();
        f.sync_all().unwrap();
    }

    fn made_vm(p: &Paths, name: &str) -> PathBuf {
        let vm = p.vms_internal().join(name);
        fs::create_dir_all(vm.join(state::LOG_DIR)).unwrap();
        let mut spec = state::Spec { name: name.into(), device: "iPhone17,3".into(), ios: "26.6.2".into(), ..Default::default() };
        spec.firmware = compat::resolve(&compat::find_pairing("iPhone17,3", "26.6.2").unwrap()).ok();
        state::write_spec(&vm, &spec).unwrap();
        state::write_status(&vm, &Status { name: name.into(), device: spec.device, ios: spec.ios, state: "failed".into(), step: "restore".into(), error: "boom".into(), ..Default::default() });
        sparse(&vm.join("Disk.img"), 64 << 30, 2 * MIB);
        write(&vm.join(state::LOG_DIR).join("restore.log"), &vec![b'x'; 64 << 10]);
        write(&vm.join(".runs-dfu/serial.log"), b"serial\n");
        write(&vm.join("iPhone_Restore/kernel"), &vec![1u8; (MIB) as usize]);
        vm
    }

    fn finished_vm(p: &Paths, name: &str) -> PathBuf {
        let vm = made_vm(p, name);
        state::mark(&vm, "booted").unwrap();
        vm
    }

    fn set_active(p: &Paths, name: &str) {
        fs::create_dir_all(p.files.join("create")).unwrap();
        fs::write(crate::create::active_file(p), name).unwrap();
    }

    fn run(c: &Ctx, op: &str, name: &str) -> JobResult {
        execute(c, &Op { op: op.into(), name: name.into() }, op)
    }

    #[test]
    fn sparse_files_count_for_allocated_blocks() {
        let p = fixture("sparse");
        let file = p.files.join("Disk.img");
        sparse(&file, 64 << 30, 4 * MIB);
        assert!(fs::metadata(&file).unwrap().len() == 64 << 30);
        let size = size_of(&file);
        assert!((4 * MIB..8 * MIB).contains(&size), "{size}");
        let vm = made_vm(&p, "vm1");
        let before = size_of(&vm);
        assert!(before < 32 * MIB, "a 64 GB sparse disk is not counted at its length: {before}");
    }

    #[test]
    fn a_hard_link_is_counted_once_and_a_symlink_counts_as_itself() {
        let p = fixture("links");
        let dir = p.files.join("d");
        write(&dir.join("a"), &vec![1u8; (2 * MIB) as usize]);
        fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        let outside = p.files.join("big");
        write(&outside, &vec![1u8; (8 * MIB) as usize]);
        symlink(&outside, dir.join("link")).unwrap();
        let size = size_of(&dir);
        assert!((2 * MIB..4 * MIB).contains(&size), "{size}");
    }

    #[test]
    fn a_locked_vm_is_refused_and_a_free_one_deleted() {
        let p = fixture("locked");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        set_active(&p, "vm1");
        let held = state::acquire_lock(&vm).unwrap();
        for op in ["deleteVm", "resetVm", "clearVmLogs"] {
            let r = run(&c, op, "vm1");
            assert!(!r.ok && r.failures[0].error.contains("setup is running"), "{r:?}");
            assert_eq!(r.freed_bytes, 0);
        }
        let blocked = plan(&c, &Op { op: "deleteVm".into(), name: "vm1".into() }).blocked;
        assert!(!blocked.is_empty());
        assert!(vm.join("Disk.img").exists(), "a live VM's files are untouched");
        released(&vm, held);
        let r = run(&c, "deleteVm", "vm1");
        assert!(r.ok, "{r:?}");
        assert!(r.freed_bytes > 3 * MIB, "{r:?}");
        assert!(!vm.exists());
        assert!(crate::create::pending(&p).is_none(), "the pending record goes with it");
    }

    fn vm_up() -> Option<String> {
        Some("vm1".to_string())
    }

    #[test]
    fn a_running_normal_vm_is_refused() {
        let p = fixture("normal");
        let c = Ctx { paths: &p, normal_vm: vm_up };
        let vm = made_vm(&p, "vm1");
        let r = run(&c, "deleteVm", "vm1");
        assert!(!r.ok && r.failures[0].error.contains("running now"), "{r:?}");
        assert!(vm.exists());
        assert!(run(&c, "resetEverything", "").failures[0].error.contains("running right now"));
        let vm2 = made_vm(&p, "vm2");
        assert!(run(&c, "deleteVm", "vm2").ok, "another VM is not held by this one");
        assert!(!vm2.exists());
    }

    #[test]
    fn a_boot_that_starts_during_the_delete_puts_the_folder_back() {
        let p = fixture("race-boot");
        // normal_vm says "not running" for the pre-check and "running" just after the rename.
        static CALLS: AtomicU64 = AtomicU64::new(0);
        fn second_call_up() -> Option<String> {
            (CALLS.fetch_add(1, Ordering::SeqCst) > 0).then(|| "vm1".to_string())
        }
        let c = Ctx { paths: &p, normal_vm: second_call_up };
        let vm = made_vm(&p, "vm1");
        let r = run(&c, "deleteVm", "vm1");
        assert!(!r.ok && r.failures[0].error.contains("started while"), "{r:?}");
        assert!(vm.join("Disk.img").exists(), "back in its place");
    }

    #[test]
    fn symlinks_and_traversal_are_refused() {
        let p = fixture("symlinks");
        let c = ctx(&p);
        let outside = p.files.parent().unwrap().join("outside");
        write(&outside.join("treasure"), b"keep me");

        symlink(&outside, p.vms_internal().join("evil")).unwrap();
        let r = run(&c, "deleteVm", "evil");
        assert!(!r.ok && r.failures[0].error.contains("symbolic link"), "{r:?}");

        for bad in ["", ".", "..", "../outside", "a/b", ".hidden"] {
            let r = run(&c, "deleteVm", bad);
            assert!(!r.ok, "{bad:?}: {r:?}");
        }
        for bad in ["../outside/treasure", "/etc/passwd", ""] {
            let r = run(&c, "deleteFirmware", bad);
            assert!(!r.ok, "{bad:?}: {r:?}");
        }
        assert!(scoped(&p, Path::new("/etc/passwd")).is_err());
        assert!(scoped(&p, &p.files).is_err(), "not the root itself");
        assert!(scoped(&p, &p.files.join("../outside")).is_err());
        assert!(scoped(&p, &p.external.join("vms/x")).is_err(), "external storage is not removed from");
        let mut out = Outcome::default();
        assert!(!remove_path(&p, Path::new("/etc/passwd"), &mut out));
        assert!(!out.failures.is_empty());

        // A symlink inside a VM is removed as a link; what it points at survives.
        let vm = made_vm(&p, "vm1");
        symlink(outside.join("treasure"), vm.join("link")).unwrap();
        symlink(&outside, vm.join("linkdir")).unwrap();
        assert!(run(&c, "deleteVm", "vm1").ok);
        assert_eq!(fs::read(outside.join("treasure")).unwrap(), b"keep me");

        // A trash folder replaced by a symlink is not used.
        fs::remove_dir_all(p.files.join(TRASH_DIR)).unwrap();
        symlink(&outside, p.files.join(TRASH_DIR)).unwrap();
        let vm2 = made_vm(&p, "vm2");
        assert!(!run(&c, "deleteVm", "vm2").ok);
        assert!(vm2.exists() && outside.join("treasure").exists());
    }

    #[test]
    fn a_folder_on_the_way_being_a_symlink_is_refused() {
        let p = fixture("symlink-way");
        let outside = p.files.parent().unwrap().join("other");
        write(&outside.join("x"), b"keep");
        symlink(&outside, p.files.join("linked")).unwrap();
        let mut out = Outcome::default();
        assert!(!remove_path(&p, &p.files.join("linked").join("x"), &mut out));
        assert!(out.failures[0].error.contains("symbolic link"), "{out:?}");
        assert!(outside.join("x").exists());
    }

    #[test]
    fn a_reset_starts_over_and_keeps_the_firmware() {
        let p = fixture("reset");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        let fw = p.fw_cache().join("iPhone17,3_26.6.2_23G90_Restore.ipsw");
        write(&fw, b"firmware");
        write(&p.fw_cache().join("cloud.part"), b"partial");
        state::mark(&vm, "firmware").unwrap();
        let r = run(&c, "resetVm", "vm1");
        assert!(r.ok, "{r:?}");
        assert!(r.freed_bytes > 3 * MIB, "{r:?}");
        assert_eq!(fs::read(&fw).unwrap(), b"firmware");
        assert_eq!(fs::read(p.fw_cache().join("cloud.part")).unwrap(), b"partial");
        assert!(!vm.join("Disk.img").exists() && !vm.join("iPhone_Restore").exists() && !vm.join(".runs-dfu").exists());
        assert!(!vm.join(state::STATE_FILE).exists());
        let spec = state::read_spec(&vm).unwrap();
        assert_eq!(spec.ios, "26.6.2");
        assert!(spec.firmware.is_some());
        let status = state::read_status(&vm).unwrap();
        assert_eq!(status.state, "cancelled");
        assert_eq!(crate::create::pending(&p).as_deref(), Some("vm1"));
        assert!(!state::run_is_live(&vm), "the operation's lock is released");
    }

    #[test]
    fn a_reset_is_refused_while_another_creation_is_pending_or_without_a_plan() {
        let p = fixture("reset-other");
        let c = ctx(&p);
        made_vm(&p, "vm1");
        made_vm(&p, "vm2");
        set_active(&p, "vm2");
        let r = run(&c, "resetVm", "vm1");
        assert!(r.failures[0].error.contains("still being created"), "{r:?}");
        assert!(p.vms_internal().join("vm1/Disk.img").exists());
        fs::remove_file(crate::create::active_file(&p)).unwrap();
        let bare = p.vms_internal().join("bare");
        fs::create_dir_all(&bare).unwrap();
        assert!(run(&c, "resetVm", "bare").failures[0].error.contains("no saved setup plan"));
    }

    #[test]
    fn clearing_logs_keeps_the_disk_and_progress() {
        let p = fixture("logs");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        state::mark(&vm, "firmware").unwrap();
        write(&vm.join("serial.log"), b"x");
        let r = run(&c, "clearVmLogs", "vm1");
        assert!(r.ok, "{r:?}");
        assert!(!vm.join(".runs-dfu").exists() && !vm.join("serial.log").exists());
        assert!(!vm.join(state::LOG_DIR).join("restore.log").exists());
        assert!(vm.join(state::LOG_DIR).is_dir(), "the folder stays for the next run");
        assert!(state::done(&vm, "firmware") && vm.join("Disk.img").exists() && vm.join(state::SPEC_FILE).exists());
    }

    #[test]
    fn partial_failure_reports_what_was_and_was_not_removed() {
        let p = fixture("partial");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        // A folder this process may not change: its contents cannot be removed.
        let stuck = vm.join(".runs-dfu");
        let original = fs::metadata(&stuck).unwrap().permissions();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();
        let r = run(&c, "clearVmLogs", "vm1");
        fs::set_permissions(&stuck, original).unwrap();
        assert!(!r.ok && r.attempted, "{r:?}");
        assert!(r.failures.iter().any(|f| f.path.contains(".runs-dfu")), "{r:?}");
        assert!(r.report.contains("Could not remove"), "{}", r.report);
        assert!(r.freed_bytes > 0, "the log file before it was removed and counted: {r:?}");
        assert!(!vm.join(state::LOG_DIR).join("restore.log").exists());
        assert!(stuck.join("serial.log").exists());
    }

    #[test]
    fn a_partly_failed_delete_leaves_its_rest_as_a_leftover() {
        let p = fixture("partial-delete");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        let stuck = vm.join(".runs-dfu");
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();
        let r = run(&c, "deleteVm", "vm1");
        assert!(!r.ok && r.attempted && !r.failures.is_empty(), "{r:?}");
        assert!(r.kept.iter().any(|k| k.contains("Leftovers")), "{r:?}");
        assert!(!vm.exists(), "gone from its place at once");
        for path in children_of(&p.files.join(TRASH_DIR)) {
            fs::set_permissions(path.join(".runs-dfu"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let s = scan(&c);
        let item = s.leftovers.iter().find(|l| l.id == "trash").expect("listed as a leftover");
        assert!(item.bytes > 0);
        assert!(run(&c, "clearLeftover", "trash").ok);
    }

    #[test]
    fn leftovers_are_found_and_cleared() {
        let p = fixture("leftovers");
        let c = ctx(&p);
        let temp = p.vms_internal().join("gpu-recovery-1234");
        sparse(&temp.join("Disk.img"), 8 << 30, MIB);
        write(&p.create_sockets().join("usbmuxd.pid"), b"99");
        write(&p.cache.join("tmp/leftover"), &vec![1u8; 1024 * 64]);
        write(&p.files.join("home/.scratch"), b"x");
        write(&p.vms_internal().join(".reset-old/x"), b"x");
        fs::create_dir_all(p.files.join("create")).unwrap();
        fs::write(crate::create::active_file(&p), "gone").unwrap();
        fs::write(p.last_run(), r#"{"vm":"also-gone","phase":"failed"}"#).unwrap();
        let done = finished_vm(&p, "done-vm");

        let s = scan(&c);
        let ids: Vec<&str> = s.leftovers.iter().map(|l| l.id.as_str()).collect();
        for want in ["temp-vm:gpu-recovery-1234", "sockets", "cache", "home", "orphan:.reset-old", "active", "last-run", "vm-runs:done-vm"] {
            assert!(ids.contains(&want), "{want} missing from {ids:?}");
        }
        assert!(s.vms.iter().all(|v| v.name != "gpu-recovery-1234"), "a temporary VM is not a VM");
        let temp_item = s.leftovers.iter().find(|l| l.id.starts_with("temp-vm")).unwrap();
        assert!((MIB..2 * MIB).contains(&temp_item.bytes), "measured sparsely: {}", temp_item.bytes);
        assert_eq!(s.vms[0].state, "finished");

        let r = run(&c, "clearAllLeftovers", "");
        assert!(r.ok, "{r:?}");
        assert!(!temp.exists() && !p.create_sockets().join("usbmuxd.pid").exists() && !p.cache.join("tmp/leftover").exists());
        assert!(p.create_sockets().is_dir(), "the shared folders themselves stay");
        assert!(!crate::create::active_file(&p).exists() && !p.last_run().exists());
        assert!(done.join("Disk.img").exists() && !done.join(".runs-dfu").exists(), "a finished VM keeps its disk");
        let after = scan(&c);
        assert!(after.leftovers.iter().all(|l| l.id == "trash" && l.bytes == 0), "{:?}", after.leftovers);
    }

    #[test]
    fn a_stopped_creations_tool_files_are_kept_until_it_is_gone() {
        let p = fixture("home-pending");
        let c = ctx(&p);
        made_vm(&p, "vm1");
        set_active(&p, "vm1");
        write(&p.files.join("home/.state"), b"needed");
        let s = scan(&c);
        assert!(s.leftovers.iter().find(|l| l.id == "home").unwrap().blocked.contains("vm1"));
        assert!(run(&c, "clearAllLeftovers", "").ok);
        assert!(p.files.join("home/.state").exists());
        assert!(crate::create::pending(&p).is_some(), "the stopped creation still continues");
    }

    #[test]
    fn leftovers_in_use_by_a_live_creation_are_left_alone() {
        let p = fixture("leftovers-live");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        let held = state::acquire_lock(&vm).unwrap();
        let temp = p.vms_internal().join("gpu-recovery-7");
        write(&temp.join("x"), b"in use");
        write(&p.create_sockets().join("qemu.pid"), b"1");
        write(&p.cache.join("tmp/f"), b"1");
        let s = scan(&c);
        assert!(s.leftovers.iter().all(|l| l.id == "trash" || !l.blocked.is_empty()), "{:?}", s.leftovers);
        assert!(s.processes.is_empty(), "a live creation's helpers are its own");
        let r = run(&c, "clearAllLeftovers", "");
        assert!(r.ok, "{r:?}");
        assert!(temp.exists() && p.cache.join("tmp/f").exists());
        assert!(!r.kept.is_empty());
        assert!(run(&c, "resetEverything", "").failures[0].error.contains("being set up"));
        drop(held);
    }

    #[test]
    fn firmware_is_listed_flagged_and_removed_with_its_bookkeeping() {
        let p = fixture("firmware");
        let c = ctx(&p);
        let cache = p.fw_cache();
        write(&cache.join("a.ipsw"), &vec![1u8; (2 * MIB) as usize]);
        write(&cache.join("a.ipsw.meta"), b"{}");
        write(&cache.join("b.ipsw.part"), &vec![1u8; (MIB) as usize]);
        write(&cache.join("b.ipsw.part.ok"), b"1024");
        write(&cache.join("release/lib"), b"x");
        let s = scan(&c);
        assert_eq!(s.firmware.len(), 3, "{:?}", s.firmware);
        let a = s.firmware.iter().find(|f| f.name == "a.ipsw").unwrap();
        assert!(!a.partial && a.bytes >= 2 * MIB, "{a:?}");
        let b = s.firmware.iter().find(|f| f.name == "b.ipsw").unwrap();
        assert!(b.partial && b.note.contains("Partial download"), "{b:?}");
        assert!(s.firmware.iter().find(|f| f.name == "release").unwrap().partial);

        assert!(run(&c, "deleteFirmware", "a.ipsw").ok);
        assert!(!cache.join("a.ipsw").exists() && !cache.join("a.ipsw.meta").exists());
        assert!(cache.join("b.ipsw.part").exists(), "a resumable download is not touched by another's delete");
        assert!(cache.join("b.ipsw.part.ok").exists());
        let r = run(&c, "deleteAllFirmware", "");
        assert!(r.ok, "{r:?}");
        assert!(children_of(&cache).is_empty());
    }

    #[test]
    fn firmware_a_live_creation_plans_is_kept_and_others_can_go() {
        let p = fixture("firmware-live");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        let spec = state::read_spec(&vm).unwrap();
        let ios = spec.firmware.as_ref().unwrap().ios.file.clone();
        write(&p.fw_cache().join(&ios), b"in use");
        write(&p.fw_cache().join("old-iPhone.ipsw"), b"old");
        write(&p.fw_cache().join("release/x"), b"used by the step");
        let held = state::acquire_lock(&vm).unwrap();
        let r = run(&c, "deleteFirmware", &ios);
        assert!(!r.ok && r.failures[0].error.contains("using this"), "{r:?}");
        assert!(p.fw_cache().join(&ios).exists());
        assert!(run(&c, "deleteFirmware", "old-iPhone.ipsw").ok, "storage can be freed during a storage hold");
        let r = run(&c, "deleteAllFirmware", "");
        assert!(r.ok, "{r:?}");
        assert!(p.fw_cache().join(&ios).exists() && p.fw_cache().join("release/x").exists());
        assert!(r.kept.iter().any(|k| k.contains("using this")), "{r:?}");
        drop(held);
    }

    #[test]
    fn a_creation_cannot_start_during_a_cleanup() {
        let p = fixture("gate");
        let gate = take_gate(&p).unwrap();
        assert!(hold_off_cleanup(&p).is_err(), "begin refuses");
        let stopped = {
            let p2 = p.clone();
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done2 = done.clone();
            let handle = std::thread::spawn(move || {
                wait_for_cleanup(&p2);
                done2.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(300));
            assert!(!done.load(Ordering::SeqCst), "a resumed run waits");
            (handle, done)
        };
        let r = run(&ctx(&p), "deleteAllFirmware", "");
        assert!(r.failures[0].error.contains("Another cleanup"), "a second cleanup is refused: {r:?}");
        drop(gate);
        stopped.0.join().unwrap();
        assert!(stopped.1.load(Ordering::SeqCst));
        let hold = hold_off_cleanup(&p).unwrap().unwrap();
        assert!(take_gate(&p).is_err(), "a cleanup does not start during a begin");
        drop(hold);
        assert!(take_gate(&p).is_ok());
    }

    #[test]
    fn begin_is_refused_during_a_cleanup() {
        let p = fixture("begin-gate");
        let gate = take_gate(&p).unwrap();
        let spec = serde_json::json!({"name": "vm1", "device": "iPhone17,3", "ios": "26.6.2", "scale": 1}).to_string();
        let e = crate::create::begin(&p, &spec).unwrap_err();
        assert!(e.contains("being cleaned up"), "{e}");
        assert!(!p.vms_internal().join("vm1").exists());
        drop(gate);
    }

    /// A process that can be mistaken for nothing else: this test's own copy of sleep.
    fn spawn_as(p: &Paths, lib: &str) -> std::process::Child {
        let exe = p.lib(lib);
        fs::copy("/bin/sleep", &exe).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        // Another test's fork can hold the copy's write descriptor for an instant before its exec.
        for _ in 0..100 {
            match std::process::Command::new(&exe).arg("60").spawn() {
                Ok(child) => return child,
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("{e}"),
            }
        }
        panic!("the helper's copy stayed busy");
    }

    /// As in `settle_flock`: a run's lock is really gone only once forks in flight have exec'd.
    fn released(vm: &Path, held: File) {
        drop(held);
        for _ in 0..200 {
            if !state::run_is_live(vm) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the lock is still held");
    }

    #[test]
    fn only_the_apps_own_helpers_are_stopped() {
        let p = fixture("procs");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        let mut helper = spawn_as(&p, "libvusbd.so");
        let mut stranger = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let s = scan(&c);
        assert_eq!(s.processes.len(), 1, "{:?}", s.processes);
        assert_eq!(s.processes[0].pid, helper.id() as i32);
        assert!(!s.reset_blocked.is_empty(), "reset waits for it");
        assert!(!s.vms[0].blocked.is_empty(), "a helper from a dead run may hold a VM's files");
        assert!(run(&c, "deleteVm", "vm1").failures[0].error.contains("earlier run"));
        assert!(vm.exists());

        let r = run(&c, "stopProcesses", "");
        assert!(r.ok, "{r:?}");
        assert!(helper.wait().unwrap().signal().is_some());
        assert!(stranger.try_wait().unwrap().is_none(), "another process is never signalled");
        let _ = stranger.kill();
        let _ = stranger.wait();
        assert!(scan(&c).processes.is_empty());
    }

    #[test]
    fn a_live_creations_helpers_are_not_offered() {
        let p = fixture("procs-live");
        let c = ctx(&p);
        let vm = made_vm(&p, "vm1");
        let mut helper = spawn_as(&p, "libusbmuxd_vphone.so");
        std::thread::sleep(Duration::from_millis(100));
        let held = state::acquire_lock(&vm).unwrap();
        assert!(scan(&c).processes.is_empty());
        assert!(run(&c, "stopProcesses", "").failures[0].error.contains("using these"));
        assert!(helper.try_wait().unwrap().is_none());
        drop(held);
        let _ = helper.kill();
        let _ = helper.wait();
    }

    #[test]
    fn states_are_read_from_the_vm() {
        let p = fixture("states");
        let c = ctx(&p);
        made_vm(&p, "failed-vm");
        finished_vm(&p, "done-vm");
        let paused = made_vm(&p, "paused-vm");
        let mut s = state::read_status(&paused).unwrap();
        s.state = "running".into();
        state::write_status(&paused, &s);
        fs::write(paused.join(engine::PAUSE_FILE), b"").unwrap();
        let stopped = made_vm(&p, "stopped-vm");
        let mut s = state::read_status(&stopped).unwrap();
        s.state = "cancelled".into();
        state::write_status(&stopped, &s);
        fs::create_dir_all(p.vms_internal().join("bad")).unwrap();
        fs::create_dir_all(p.vms().join("imported")).unwrap();
        let live_vm = made_vm(&p, "live-vm");
        let held = state::acquire_lock(&live_vm).unwrap();

        let scanned = scan(&c);
        let vm = |n: &str| scanned.vms.iter().find(|v| v.name == n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(vm("failed-vm").state, "failed");
        assert!(vm("failed-vm").state_text.contains("Restore iOS"), "{}", vm("failed-vm").state_text);
        assert!(vm("failed-vm").log.ends_with("restore.log"), "{}", vm("failed-vm").log);
        assert_eq!(vm("failed-vm").device, "iPhone17,3");
        assert_eq!(vm("done-vm").state, "finished");
        assert_eq!(vm("paused-vm").state, "paused");
        assert_eq!(vm("stopped-vm").state, "stopped");
        assert_eq!(vm("bad").state, "incomplete");
        assert_eq!(vm("live-vm").state, "running");
        assert!(!vm("live-vm").blocked.is_empty());
        assert!(vm("imported").external && !vm("imported").blocked.is_empty());
        assert!(vm("failed-vm").bytes > 3 * MIB && vm("failed-vm").bytes < 32 * MIB);
        assert!(!vm("failed-vm").size.is_empty());
        drop(held);
    }

    #[test]
    fn a_plan_measures_what_goes_and_names_it() {
        let p = fixture("plan");
        let c = ctx(&p);
        made_vm(&p, "vm1");
        let plan = plan(&c, &Op { op: "deleteVm".into(), name: "vm1".into() });
        assert!(plan.blocked.is_empty(), "{plan:?}");
        assert!(plan.bytes > 3 * MIB, "{plan:?}");
        assert!(plan.text.contains("vm1") && plan.text.contains("Disk.img") && plan.text.contains(&plan.size), "{}", plan.text);
        assert!(plan.confirm_word.is_empty());
        let reset = super::plan(&c, &Op { op: "resetEverything".into(), name: String::new() });
        assert_eq!(reset.confirm_word, "RESET");
        assert!(reset.text.contains("vm1") && reset.text.contains("cannot be undone"), "{}", reset.text);
        assert!(super::plan(&c, &Op { op: "nope".into(), name: String::new() }).blocked.contains("unknown"));
        let traversal = super::plan(&c, &Op { op: "deleteVm".into(), name: "../x".into() });
        assert!(!traversal.blocked.is_empty());
    }

    #[test]
    fn reset_everything_returns_to_first_run() {
        let p = fixture("reset-all");
        let c = ctx(&p);
        made_vm(&p, "vm1");
        finished_vm(&p, "vm2");
        set_active(&p, "vm1");
        write(&p.vms_internal().join("gpu-recovery-3/x"), b"x");
        write(&p.fw_cache().join("a.ipsw"), &vec![1u8; (MIB) as usize]);
        write(&p.create_sockets().join("usbmuxd.pid"), b"1");
        write(&p.files.join("lockdown/pair.plist"), b"x");
        write(&p.cache.join("tmp/t"), b"x");
        write(&p.vms().join("imported/Disk.img"), b"x");
        let r = run(&c, "resetEverything", "");
        assert!(r.ok, "{r:?}");
        assert!(r.freed_bytes > 3 * MIB, "{r:?}");
        assert!(children_of(&p.vms_internal()).is_empty());
        let rest: Vec<_> = children_of(&p.files).iter().map(|x| x.file_name().unwrap().to_owned()).collect();
        assert!(rest.iter().all(|n| n == "vms" || n == GATE_FILE), "{rest:?}");
        assert!(children_of(&p.cache).is_empty(), "the cache is empty");
        assert!(crate::create::pending(&p).is_none());
        assert!(p.vms().join("imported").exists(), "external storage is left alone");
        assert!(r.kept.iter().any(|k| k.contains("imported")), "{r:?}");
    }

    #[test]
    fn a_job_reports_progress_and_a_result() {
        let p: &'static Paths = Box::leak(Box::new(fixture("job")));
        made_vm(p, "vm1");
        start(p, r#"{"op":"deleteVm","name":"vm1"}"#).unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        let result = loop {
            let done = JOB.lock().unwrap().as_ref().is_some_and(|j| !j.active);
            if done {
                break JOB.lock().unwrap().clone().unwrap().result.unwrap();
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(result.ok, "{result:?}");
        assert!(result.freed_bytes > 3 * MIB && generation() > 0, "{result:?}");
        assert!(!p.vms_internal().join("vm1").exists());
        take_result();
        assert!(JOB.lock().unwrap().is_none());
    }
}
