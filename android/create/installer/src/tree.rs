//! Copying a directory tree into the guest as a tar stream to its `tar`.
//!
//! SFTP costs several round trips per file over an emulated dropbear; a cryptex
//! is hundreds of thousands of small files. `tar` over one exec channel carries
//! contents, modes and numeric owners in one stream. The ramdisk's tar is GNU
//! tar 1.17, so entries use GNU long-name records. Extended attributes cannot be
//! carried: neither that tar nor `sftp-server` can set them, the same limit the
//! sshfs path had. They are counted and reported.

use crate::active::ActiveInstant;
use crate::bail;
use crate::error::{Error, Result};
use crate::guestfs::shell_quote;
use crate::progress::{check_cancel, Progress};
use crate::ssh::GuestSsh;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tar::{EntryType, Header};
use tokio::sync::mpsc;
use vphone_apfs::container::VolumeSelector;
use vphone_apfs::volume::{self, FileKind, Inode, Volume};

/// The guest's tar (from sshtars).
pub const GUEST_TAR: &str = "/usr/bin/tar";

/// Mode and owner of a tree entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
}

/// What a copy carried.
#[derive(Clone, Debug, Default)]
pub struct TreeStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub bytes: u64,
    /// Extended attributes the source had that could not be carried, by name.
    pub xattrs_not_carried: BTreeMap<String, u64>,
}

struct ChunkWriter {
    tx: mpsc::Sender<Vec<u8>>,
    buf: Vec<u8>,
}

const CHUNK: usize = 1 << 20;

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK + 4096));
            self.tx
                .blocking_send(chunk)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the guest's tar stopped reading"))?;
        }
        Ok(())
    }
}

/// How a copy is getting on, for the screen: the share done, the speed over the last half minute
/// and the time left at that speed. Time is counted on the active clock, so a pause is not a
/// stall and does not slow the speed shown.
pub struct Meter {
    /// What the line starts with: "Copying the patched dyld cache to the VM".
    title: String,
    /// The part of the step's bar this meter fills, as (from, to) shares of it.
    bar: (f64, f64),
    total: u64,
    sent: u64,
    /// The files done and how many there are, where the copy counts them: a file costs the guest
    /// time beside its bytes, so the share done and the time left are then of both.
    files: Option<(u64, u64)>,
    started: ActiveInstant,
    reported: ActiveInstant,
    /// (seconds since the start, bytes sent then, work done then), over the window the speed is
    /// measured on.
    samples: VecDeque<(f64, u64, u64)>,
}

/// The window the speed is measured over, and the least of it that says anything.
const SPEED_WINDOW_SECS: f64 = 30.0;
const SPEED_MIN_SECS: f64 = 3.0;
const REPORT_EVERY: Duration = Duration::from_millis(500);

fn megabytes(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb < 10.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{mb:.0} MB")
    }
}

fn time_text(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    match secs {
        0..=89 => format!("{} s", secs.max(1)),
        90..=3599 => format!("{} min", secs.div_ceil(60)),
        _ => format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60),
    }
}

impl Meter {
    pub fn new(what: &str, total: u64) -> Meter {
        Meter::titled(format!("Copying {what} to the VM"), total, (0.0, 1.0))
    }

    /// A meter whose line starts with `title` and which fills the `bar` share of the step's bar.
    pub fn titled(title: String, total: u64, bar: (f64, f64)) -> Meter {
        let now = ActiveInstant::now();
        Meter { title, bar, total, sent: 0, files: None, started: now, reported: now, samples: VecDeque::new() }
    }

    /// Counting files too, `files` of them in all.
    fn counting_files(mut self, files: u64) -> Meter {
        self.files = Some((0, files));
        self
    }

    /// What is done and what there is, with each file counted at what making it costs the guest.
    fn work(&self) -> (u64, u64) {
        match self.files {
            Some((done, all)) => (self.sent.min(self.total) + done.min(all) * FILE_COST_BYTES, self.total + all * FILE_COST_BYTES),
            None => (self.sent.min(self.total), self.total),
        }
    }

    /// The line for `sent` bytes at `at` seconds, with what the samples so far say of the speed.
    fn line(&mut self, at: f64) -> String {
        let (work, all_work) = self.work();
        self.samples.push_back((at, self.sent, work));
        while self.samples.len() > 2 && self.samples[0].0 < at - SPEED_WINDOW_SECS {
            self.samples.pop_front();
        }
        let (first_at, first_sent, first_work) = self.samples[0];
        let mut text = format!("{}: ", self.title);
        if self.total > 0 {
            text += &format!("{}% ({} of {}", work * 100 / all_work.max(1), megabytes(self.sent.min(self.total)), megabytes(self.total));
            if let Some((done, all)) = self.files {
                text += &format!(", {} of {all} files", done.min(all));
            }
            text += ")";
        } else {
            text += &megabytes(self.sent);
        }
        let span = at - first_at;
        if span < SPEED_MIN_SECS || work <= first_work {
            return text + " \u{b7} measuring speed";
        }
        let per_sec = self.sent.saturating_sub(first_sent) as f64 / span;
        text += &if per_sec < 10_000_000.0 {
            format!(" \u{b7} {:.1} MB/s", per_sec / 1_000_000.0)
        } else {
            format!(" \u{b7} {:.0} MB/s", per_sec / 1_000_000.0)
        };
        if all_work > work {
            text += &format!(" \u{b7} about {} left", time_text((all_work - work) as f64 * span / (work - first_work) as f64));
        }
        text
    }

    fn add(&mut self, bytes: u64, progress: &dyn Progress) {
        self.add_with_files(bytes, 0, progress);
    }

    fn add_with_files(&mut self, bytes: u64, files: u64, progress: &dyn Progress) {
        self.sent += bytes;
        if let Some((done, _)) = self.files.as_mut() {
            *done += files;
        }
        if self.reported.elapsed() >= REPORT_EVERY {
            self.reported = ActiveInstant::now();
            let line = self.line(self.started.elapsed().as_secs_f64());
            progress.progress(self.bar_done(), self.total, &line);
        }
    }

    /// Where the step's bar is: this meter's share of it, filled as far as the meter has got.
    fn bar_done(&self) -> u64 {
        if self.total == 0 {
            return self.sent;
        }
        let (work, all_work) = self.work();
        let share = self.bar.0 + (self.bar.1 - self.bar.0) * work as f64 / all_work.max(1) as f64;
        (share * self.total as f64) as u64
    }

    /// The speed over all of it, in MB/s.
    fn average(&self) -> f64 {
        self.sent as f64 / self.started.elapsed().as_secs_f64().max(0.001) / 1_000_000.0
    }

    /// How long it took and the speed over all of it.
    fn summary(&self) -> String {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        format!("{} in {} (average {:.1} MB/s)", megabytes(self.sent), time_text(secs), self.sent as f64 / secs / 1_000_000.0)
    }
}

/// Writes GNU tar entries; `owner` overrides each entry's uid/gid when set.
pub struct TarSink<'a> {
    out: &'a mut dyn Write,
    owner: Option<(u32, u32)>,
    stats: TreeStats,
    progress: &'a dyn Progress,
    /// Shared by the streams of one copy.
    meter: Option<&'a Mutex<Meter>>,
    part: Part,
    /// How long the archive is so far.
    position: u64,
    /// Where in the archive each file so far ends, for a copy that follows its reader by them.
    marks: Option<Vec<u64>>,
}

/// Which entries of the tree one stream carries. A copy over several connections sends the
/// directories first, then each connection its share of the files, then the directories again
/// (so their times are the source's, not the copy's).
#[derive(Clone, Default)]
pub struct Part {
    /// Leave directory entries out.
    no_dirs: bool,
    /// Leave symbolic links out.
    no_links: bool,
    /// The only files to send; `None` sends every file.
    files: Option<Arc<HashSet<String>>>,
}

/// Counts what goes to the tar stream as it goes, so one file of gigabytes still shows progress.
struct Metered<'w, 'a> {
    inner: &'w mut dyn Write,
    meter: Option<&'a Mutex<Meter>>,
    progress: &'a dyn Progress,
}

impl Write for Metered<'_, '_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.progress.cancelled() {
            // Not `Interrupted`, which a copy loop would take as "try again".
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.write(data)?;
        if let Some(meter) = self.meter {
            meter.lock().unwrap().add(n as u64, self.progress);
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

const ZEROS: [u8; 512] = [0; 512];

impl TarSink<'_> {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.position += data.len() as u64;
        self.out.write_all(data).map_err(|e| Error::io("tar stream", e))
    }

    fn pad(&mut self, size: u64) -> Result<()> {
        let rem = (size % 512) as usize;
        if rem != 0 {
            self.write(&ZEROS[..512 - rem])?;
        }
        Ok(())
    }

    /// A GNU `././@LongLink` record carrying a long name (type L) or link target (type K).
    fn long_record(&mut self, kind: EntryType, value: &[u8]) -> Result<()> {
        let mut h = Header::new_gnu();
        h.as_mut_bytes()[..13].copy_from_slice(b"././@LongLink");
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(value.len() as u64 + 1);
        h.set_entry_type(kind);
        h.set_cksum();
        self.write(h.as_bytes())?;
        self.write(value)?;
        self.write(&[0])?;
        self.pad(value.len() as u64 + 1)
    }

    fn header(&mut self, path: &str, kind: EntryType, size: u64, meta: Meta, link: Option<&str>) -> Result<()> {
        check_cancel(self.progress)?;
        let mut h = Header::new_gnu();
        let name = path.as_bytes();
        if name.len() > 100 {
            self.long_record(EntryType::GNULongName, name)?;
            h.as_mut_bytes()[..100].copy_from_slice(&name[..100]);
        } else {
            h.as_mut_bytes()[..name.len()].copy_from_slice(name);
        }
        if let Some(link) = link {
            let bytes = link.as_bytes();
            if bytes.len() > 100 {
                self.long_record(EntryType::GNULongLink, bytes)?;
                h.as_mut_bytes()[157..257].copy_from_slice(&bytes[..100]);
            } else {
                h.as_mut_bytes()[157..157 + bytes.len()].copy_from_slice(bytes);
            }
        }
        h.set_entry_type(kind);
        h.set_size(size);
        h.set_mode(meta.mode & 0o7777);
        let (uid, gid) = self.owner.unwrap_or((meta.uid, meta.gid));
        h.set_uid(uid as u64);
        h.set_gid(gid as u64);
        h.set_mtime(meta.mtime);
        h.set_cksum();
        self.write(h.as_bytes())
    }

    fn name(relative: &str, dir: bool) -> String {
        let mut path = format!("./{relative}");
        if dir && !path.ends_with('/') {
            path.push('/');
        }
        path
    }

    /// A directory; `""` is the tree root.
    pub fn dir(&mut self, relative: &str, meta: Meta) -> Result<()> {
        if self.part.no_dirs {
            return check_cancel(self.progress);
        }
        self.header(&Self::name(relative, true), EntryType::Directory, 0, meta, None)?;
        self.stats.dirs += 1;
        Ok(())
    }

    pub fn symlink(&mut self, relative: &str, target: &str, meta: Meta) -> Result<()> {
        if self.part.no_links {
            return Ok(());
        }
        self.header(&Self::name(relative, false), EntryType::Symlink, 0, meta, Some(target))?;
        self.stats.symlinks += 1;
        Ok(())
    }

    /// A regular file whose `size` bytes `write_contents` produces.
    pub fn file(
        &mut self,
        relative: &str,
        size: u64,
        meta: Meta,
        write_contents: &mut dyn FnMut(&mut dyn Write) -> Result<u64>,
    ) -> Result<()> {
        if self.part.files.as_ref().is_some_and(|mine| !mine.contains(relative)) {
            return Ok(());
        }
        self.header(&Self::name(relative, false), EntryType::Regular, size, meta, None)?;
        let wrote = {
            let mut metered = Metered { inner: &mut *self.out, meter: self.meter, progress: self.progress };
            write_contents(&mut metered)
        };
        // A cancel stops the stream part-way through a file; it is a cancel, not a short file.
        check_cancel(self.progress)?;
        let wrote = wrote?;
        if wrote != size {
            bail!("{relative}: wrote {wrote} bytes, expected {size}");
        }
        self.position += size;
        self.pad(size)?;
        if let Some(marks) = self.marks.as_mut() {
            marks.push(self.position);
        }
        self.stats.files += 1;
        self.stats.bytes += size;
        if self.meter.is_none() && self.stats.files % 64 == 0 {
            self.progress.progress(self.stats.bytes, 0, relative);
        }
        Ok(())
    }

    pub fn note_xattr(&mut self, name: &str) {
        *self.stats.xattrs_not_carried.entry(name.to_owned()).or_default() += 1;
    }
}

/// Something that can list and stream a directory tree.
pub trait TreeSource: Send + Sync {
    fn emit(&self, sink: &mut TarSink<'_>, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<()>;
    /// The regular files `emit` would send, each with its size, without reading any of them.
    fn files(&self, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<Vec<(String, u64)>>;
    /// How many files that is, and their bytes.
    fn measure(&self, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<(u64, u64)> {
        let files = self.files(exclude)?;
        Ok((files.len() as u64, files.iter().map(|f| f.1).sum()))
    }
}

/// A local directory.
pub struct LocalTree {
    pub root: PathBuf,
    /// Per-path (relative) metadata overriding the local file's.
    pub meta: BTreeMap<String, Meta>,
}

impl LocalTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            meta: BTreeMap::new(),
        }
    }

    fn meta_of(&self, relative: &str, m: &fs::Metadata) -> Meta {
        use std::os::unix::fs::MetadataExt;
        self.meta.get(relative).copied().unwrap_or(Meta {
            mode: m.mode() & 0o7777,
            uid: m.uid(),
            gid: m.gid(),
            mtime: m.mtime().max(0) as u64,
        })
    }

    fn visit(&self, relative: &str, sink: &mut TarSink<'_>, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<()> {
        let dir = if relative.is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative)
        };
        let mut children = fs::read_dir(&dir)
            .map_err(|e| Error::path(&dir, e))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::path(&dir, e))?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let name = child.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| Error::msg(format!("non-UTF-8 name in {}", dir.display())))?;
            let rel = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            if exclude(&rel) {
                continue;
            }
            let path = child.path();
            let m = fs::symlink_metadata(&path).map_err(|e| Error::path(&path, e))?;
            let meta = self.meta_of(&rel, &m);
            let kind = m.file_type();
            if kind.is_dir() {
                sink.dir(&rel, meta)?;
                self.visit(&rel, sink, exclude)?;
            } else if kind.is_symlink() {
                let target = fs::read_link(&path).map_err(|e| Error::path(&path, e))?;
                let target = target.to_str().ok_or_else(|| Error::msg("non-UTF-8 symlink"))?.to_owned();
                sink.symlink(&rel, &target, meta)?;
            } else if kind.is_file() {
                sink.file(&rel, m.len(), meta, &mut |w| {
                    let mut f = fs::File::open(&path).map_err(|e| Error::path(&path, e))?;
                    io::copy(&mut f, w).map_err(|e| Error::path(&path, e))
                })?;
            } else {
                bail!("{}: not a file, directory or link", path.display());
            }
        }
        Ok(())
    }
}

impl LocalTree {
    fn measure_dir(&self, relative: &str, exclude: &(dyn Fn(&str) -> bool + Sync), total: &mut Vec<(String, u64)>) -> Result<()> {
        let dir = if relative.is_empty() { self.root.clone() } else { self.root.join(relative) };
        for child in fs::read_dir(&dir).map_err(|e| Error::path(&dir, e))? {
            let child = child.map_err(|e| Error::path(&dir, e))?;
            let name = child.file_name();
            let Some(name) = name.to_str() else { continue };
            let rel = if relative.is_empty() { name.to_owned() } else { format!("{relative}/{name}") };
            if exclude(&rel) {
                continue;
            }
            let m = fs::symlink_metadata(child.path()).map_err(|e| Error::path(&child.path(), e))?;
            if m.file_type().is_dir() {
                self.measure_dir(&rel, exclude, total)?;
            } else if m.file_type().is_file() {
                total.push((rel, m.len()));
            }
        }
        Ok(())
    }
}

impl TreeSource for LocalTree {
    fn emit(&self, sink: &mut TarSink<'_>, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<()> {
        let m = fs::symlink_metadata(&self.root).map_err(|e| Error::path(&self.root, e))?;
        let meta = self.meta_of("", &m);
        sink.dir("", meta)?;
        self.visit("", sink, exclude)
    }

    fn files(&self, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<Vec<(String, u64)>> {
        let mut total = Vec::new();
        self.measure_dir("", exclude, &mut total)?;
        Ok(total)
    }
}

fn apfs_err(e: vphone_apfs::Error) -> Error {
    Error::Apfs(e.to_string())
}

fn inode_meta(ino: &Inode) -> Meta {
    Meta {
        mode: ino.mode as u32 & 0o7777,
        uid: ino.uid,
        gid: ino.gid,
        mtime: ino.mod_time / 1_000_000_000,
    }
}

/// A tree inside an APFS image, read through the APFS library.
pub struct ApfsTree {
    pub image: PathBuf,
    pub selector: VolumeSelector,
    /// Absolute path inside the volume.
    pub root: String,
}

/// Attributes that are how APFS stores the file, not metadata of it.
fn is_internal_xattr(name: &str) -> bool {
    name == "com.apple.decmpfs" || name == "com.apple.ResourceFork" || name == "com.apple.fs.symlink"
}

impl ApfsTree {
    /// Visit every entry; `f` gets the relative path ("" for the root).
    fn walk(&self, f: &mut dyn FnMut(&Volume<'_>, &str, &Inode) -> Result<()>) -> Result<()> {
        let container = vphone_apfs::open_image(&self.image).map_err(apfs_err)?;
        let vol = container.open_volume(&self.selector).map_err(apfs_err)?;
        let root = vol.lookup(&self.root).map_err(apfs_err)?;
        let mut failure = None;
        let walked = volume::walk(&vol, &root, &mut |rel: &str, ino: &Inode| {
            match f(&vol, rel, ino) {
                Ok(()) => Ok(()),
                Err(e) => {
                    failure = Some(e);
                    Err(vphone_apfs::Error::NotFound("stopped".into()))
                }
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        walked.map_err(apfs_err)
    }

    /// Copy the tree to a local directory; returns each entry's guest metadata.
    pub fn extract(&self, dest: &Path, progress: &dyn Progress) -> Result<BTreeMap<String, Meta>> {
        let mut metas = BTreeMap::new();
        fs::create_dir_all(dest).map_err(|e| Error::path(dest, e))?;
        self.walk(&mut |vol, rel, ino| {
            check_cancel(progress)?;
            let out = if rel.is_empty() { dest.to_path_buf() } else { dest.join(rel) };
            metas.insert(rel.to_owned(), inode_meta(ino));
            match ino.kind() {
                FileKind::Dir => fs::create_dir_all(&out).map_err(|e| Error::path(&out, e)),
                FileKind::Symlink => {
                    let target = vol.read_symlink(ino).map_err(apfs_err)?;
                    let _ = fs::remove_file(&out);
                    std::os::unix::fs::symlink(target, &out).map_err(|e| Error::path(&out, e))
                }
                FileKind::File => {
                    let mut f = fs::File::create(&out).map_err(|e| Error::path(&out, e))?;
                    vol.write_file_to(ino, &mut f).map_err(apfs_err)?;
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&out, fs::Permissions::from_mode(0o644 | (ino.mode as u32 & 0o111)))
                        .map_err(|e| Error::path(&out, e))
                }
                other => bail!("{rel}: unsupported file type {}", other.as_str()),
            }
        })?;
        Ok(metas)
    }
}

impl TreeSource for ApfsTree {
    fn files(&self, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<Vec<(String, u64)>> {
        let mut excluded: BTreeSet<String> = BTreeSet::new();
        let mut total = Vec::new();
        self.walk(&mut |vol, rel, ino| {
            if !rel.is_empty() {
                if excluded.iter().any(|p| rel.starts_with(p.as_str())) {
                    return Ok(());
                }
                if exclude(rel) {
                    excluded.insert(format!("{rel}/"));
                    return Ok(());
                }
            }
            if ino.kind() == FileKind::File {
                total.push((rel.to_owned(), vol.file_size(ino).map_err(apfs_err)?));
            }
            Ok(())
        })?;
        Ok(total)
    }

    fn emit(&self, sink: &mut TarSink<'_>, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<()> {
        // `walk` lists a directory's children before descending, so an excluded
        // subtree is filtered by prefix.
        let mut excluded: BTreeSet<String> = BTreeSet::new();
        self.walk(&mut |vol, rel, ino| {
            if !rel.is_empty() {
                if excluded.iter().any(|p| rel.starts_with(p.as_str())) {
                    return Ok(());
                }
                if exclude(rel) {
                    excluded.insert(format!("{rel}/"));
                    return Ok(());
                }
            }
            for x in vol.xattrs(ino.ino).map_err(apfs_err)? {
                if !is_internal_xattr(&x.name) {
                    sink.note_xattr(&x.name);
                }
            }
            let meta = inode_meta(ino);
            match ino.kind() {
                FileKind::Dir => sink.dir(rel, meta),
                FileKind::Symlink => {
                    let target = vol.read_symlink(ino).map_err(apfs_err)?;
                    sink.symlink(rel, &target, meta)
                }
                FileKind::File => {
                    let size = vol.file_size(ino).map_err(apfs_err)?;
                    sink.file(rel, size, meta, &mut |w| {
                        let mut counting = CountingWriter { inner: w, count: 0 };
                        vol.write_file_to(ino, &mut counting).map_err(apfs_err)?;
                        Ok(counting.count)
                    })
                }
                other => bail!("{rel}: unsupported file type {}", other.as_str()),
            }
        })
    }
}

struct CountingWriter<'w> {
    inner: &'w mut dyn Write,
    count: u64,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(data)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Stream `source` into `dest` (an existing guest directory) through the guest's tar.
pub fn upload_tree(
    ssh: &GuestSsh,
    dest: &str,
    source: &dyn TreeSource,
    owner: Option<(u32, u32)>,
    exclude: &(dyn Fn(&str) -> bool + Sync),
    progress: &dyn Progress,
) -> Result<TreeStats> {
    let name = dest.trim_end_matches('/').rsplit('/').next().unwrap_or(dest);
    upload_tree_as(name, ssh, dest, source, owner, exclude, progress)
}

/// One tar stream of `part` of the tree, to the guest's tar on `ssh`.
fn stream(
    ssh: &GuestSsh,
    dest: &str,
    source: &dyn TreeSource,
    owner: Option<(u32, u32)>,
    exclude: &(dyn Fn(&str) -> bool + Sync),
    part: Part,
    meter: Option<&Mutex<Meter>>,
    progress: &dyn Progress,
) -> Result<TreeStats> {
    let command = format!("{GUEST_TAR} -xpf - --numeric-owner -C {}", shell_quote(dest));
    let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::scope(|scope| {
        let producer = scope.spawn(move || -> Result<TreeStats> {
            let mut out = ChunkWriter { tx, buf: Vec::with_capacity(CHUNK + 4096) };
            let stats = {
                let mut sink = TarSink { out: &mut out, owner, stats: TreeStats::default(), progress, meter, part, position: 0, marks: None };
                source.emit(&mut sink, exclude)?;
                sink.stats
            };
            // End of archive: two zero blocks, then pad to a 10 KiB record.
            out.write_all(&[0u8; 10240]).map_err(|e| Error::io("tar stream", e))?;
            out.flush().map_err(|e| Error::io("tar stream", e))?;
            Ok(stats)
        });
        let output = ssh.exec_with_stdin(&command, rx, progress);
        let stats = producer.join().map_err(|_| Error::msg("tar producer panicked"))?;
        let output = output?;
        if output.status != 0 {
            bail!("guest tar failed with status {}: {}", output.status, output.stderr_text());
        }
        stats
    })
}

/// More connections to the guest's SSH for a large copy: each is its own server process in the
/// guest, so each stream's decryption runs on a vCPU of its own.
pub struct MoreLinks<'a> {
    /// Opens one more authenticated connection.
    pub open: &'a (dyn Fn() -> Result<GuestSsh> + Sync),
    /// The most streams a copy may use, the first connection included.
    pub streams: usize,
}

/// A copy smaller than this goes over the one connection: opening more costs a key exchange
/// each, which the emulated guest computes slowly.
pub const PARALLEL_MIN_BYTES: u64 = 256 << 20;

/// Shares `files` out over `bins` lists of nearly equal bytes: the largest first, each to the
/// list that has least so far.
fn shares(files: Vec<(String, u64)>, bins: usize) -> Vec<HashSet<String>> {
    shares_weighed(files, bins, 0)
}

/// As [`shares`], with each file counting as `per_file` bytes more than its size: what making a
/// file costs the guest beside writing its contents.
fn shares_weighed(mut files: Vec<(String, u64)>, bins: usize, per_file: u64) -> Vec<HashSet<String>> {
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut out = vec![(0u64, HashSet::new()); bins.max(1)];
    for (name, size) in files {
        let least = out.iter_mut().min_by_key(|(bytes, _)| *bytes).expect("at least one list");
        least.0 += size + per_file;
        least.1.insert(name);
    }
    out.into_iter().map(|(_, names)| names).filter(|names| !names.is_empty()).collect()
}

/// As [`upload_tree`], naming what is copied for the log and the screen. The source is measured
/// first, so the copy shows its share done, its speed and the time left from its first second,
/// and the log has a line when it starts (files and size) and one when it ends (time and speed).
pub fn upload_tree_as(
    what: &str,
    ssh: &GuestSsh,
    dest: &str,
    source: &dyn TreeSource,
    owner: Option<(u32, u32)>,
    exclude: &(dyn Fn(&str) -> bool + Sync),
    progress: &dyn Progress,
) -> Result<TreeStats> {
    upload_tree_over(what, ssh, None, None, dest, source, owner, exclude, progress)
}

/// As [`upload_tree_as`], by the fastest way there is for a copy of this size. With a transfer
/// disk, a large copy goes to the guest as tar archives on that disk, which the guest's tar reads
/// from the block device: nothing of it passes the guest's SSH server or its USB. Otherwise, and
/// for a small copy, it is the tar stream over SSH, over several connections when `more` offers
/// them and the copy is large: the guest's SSH server is one process a connection and the slowest
/// part of that path. Which way is decided here, once, and logged; a copy that fails does not
/// fall back to the other.
#[allow(clippy::too_many_arguments)]
pub fn upload_tree_over(
    what: &str,
    ssh: &GuestSsh,
    more: Option<&MoreLinks<'_>>,
    disk: Option<&TransferDisk>,
    dest: &str,
    source: &dyn TreeSource,
    owner: Option<(u32, u32)>,
    exclude: &(dyn Fn(&str) -> bool + Sync),
    progress: &dyn Progress,
) -> Result<TreeStats> {
    let files = source.files(exclude)?;
    let (count, bytes) = (files.len() as u64, files.iter().map(|f| f.1).sum::<u64>());
    if let Some(disk) = disk {
        match disk.refuses(&files) {
            None => return disk.copy(what, ssh, dest, source, owner, exclude, files, progress),
            Some(why) => progress.log(&format!("[*] {what}: not by the transfer disk ({why})")),
        }
    }
    // How many streams is decided here, once, from the copy's size and the connections there are.
    let mut extra: Vec<GuestSsh> = Vec::new();
    if let Some(more) = more.filter(|m| bytes >= PARALLEL_MIN_BYTES && m.streams > 1) {
        for _ in 1..more.streams.min(files.len()) {
            check_cancel(progress)?;
            match (more.open)() {
                Ok(link) => extra.push(link),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    progress.log(&format!("no further connection to the VM's SSH ({e}): copying over {}", extra.len() + 1));
                    break;
                }
            }
        }
    }
    let streams = extra.len() + 1;
    progress.log(&format!(
        "[*] copying {what} to the VM: {count} files, {} (tar over SSH, {})",
        megabytes(bytes),
        if streams == 1 { "one stream".to_owned() } else { format!("{streams} streams, one connection each") }
    ));
    let meter = Mutex::new(Meter::new(what, bytes));
    progress.progress(0, bytes, &format!("Copying {what} to the VM: 0% (0 of {}) \u{b7} measuring speed", megabytes(bytes)));
    let stats = if streams == 1 {
        stream(ssh, dest, source, owner, exclude, Part::default(), Some(&meter), progress)?
    } else {
        let only_dirs = Part { no_dirs: false, no_links: false, files: Some(Arc::new(HashSet::new())) };
        // Directories and links first, so every stream finds its files' directories as they should be.
        let mut stats = stream(ssh, dest, source, owner, exclude, only_dirs.clone(), None, progress)?;
        let parts: Vec<Part> = shares(files, streams)
            .into_iter()
            .map(|names| Part { no_dirs: true, no_links: true, files: Some(Arc::new(names)) })
            .collect();
        let links: Vec<&GuestSsh> = std::iter::once(ssh).chain(extra.iter()).collect();
        let done: Vec<Result<TreeStats>> = std::thread::scope(|scope| {
            let running: Vec<_> = parts
                .into_iter()
                .zip(links)
                .map(|(part, link)| {
                    let meter = &meter;
                    scope.spawn(move || stream(link, dest, source, owner, exclude, part, Some(meter), progress))
                })
                .collect();
            running.into_iter().map(|t| t.join().unwrap_or_else(|_| Err(Error::msg("a copy stream panicked")))).collect()
        });
        for part in done {
            let part = part?;
            stats.files += part.files;
            stats.bytes += part.bytes;
            for (name, n) in part.xattrs_not_carried {
                *stats.xattrs_not_carried.entry(name).or_default() += n;
            }
        }
        // Adding files changed the directories' times: the directory entries again set them right.
        stream(ssh, dest, source, owner, exclude, Part { no_links: true, ..only_dirs }, None, progress)?;
        stats
    };
    for link in extra {
        link.disconnect();
    }
    // Every guest tar has exited: everything sent is on its disk.
    progress.log(&format!("[+] copied {what}: {}", meter.lock().unwrap().summary()));
    progress.progress(bytes, bytes, &format!("Copied {what} to the VM"));
    Ok(stats)
}

/// The transfer disk's file in a VM's directory, while the SSH ramdisk runs.
pub const TRANSFER_IMAGE: &str = ".cfw-transfer.img";
/// What the guest sees as the disk's size. The file is sparse: it takes the space of the archive
/// being carried, and gives it back after each copy.
pub const TRANSFER_CAPACITY: u64 = 64 << 30;
/// A copy smaller than this stays on SSH: writing the archive and starting the guest's reader
/// cost more than they save.
pub const DISK_MIN_BYTES: u64 = 32 << 20;
/// Space this side keeps free beyond the archive itself.
const DISK_SPARE_BYTES: u64 = 512 << 20;
/// The archives are laid out on the disk in units of this size.
const RECORD: u64 = 1 << 20;
/// The guest's tar reads the disk this much at a time: its default of 10 KiB took four times as
/// long for the same archive.
const TAR_RECORD: u64 = 4 << 20;
/// The guest's tar says how far it is every this many of its records.
const CHECKPOINT_RECORDS: u64 = 4;
/// Making one file costs the guest about what writing this much of a large one does (measured
/// under emulation: some 1800 files a second, against more than 1 GB/s of contents). The readers'
/// shares are weighed with it, so many small files are spread over all of them.
const FILE_COST_BYTES: u64 = 512 << 10;
/// A copy of fewer files than this has one reader.
const DISK_PARALLEL_MIN_FILES: usize = 2000;
const GUEST_DD: &str = "/bin/dd";

/// A disk of the SSH ramdisk VM that is a file on this side: what is written to the file here,
/// the guest reads from its block device, at the speed of its disk driver. The VM is started with
/// it attached read-only (see `VmKind::Ramdisk` in the app's VM code).
pub struct TransferDisk {
    /// The file on this side.
    pub image: PathBuf,
    /// The guest's raw device for it.
    pub device: String,
    /// How many of the guest's tars read it at once, each on a vCPU of its own.
    pub streams: usize,
    file: fs::File,
}

/// A region of the transfer disk holding one archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    offset: u64,
    len: u64,
}

/// An archive written to the transfer disk.
struct Archive {
    slot: Slot,
    /// Where in it each of its files ends, in the order the guest's tar meets them.
    file_ends: Vec<u64>,
    /// The bytes of its files' contents.
    contents: u64,
}

/// Up to the next record boundary.
fn records(bytes: u64) -> u64 {
    bytes.div_ceil(RECORD) * RECORD
}

/// What one file takes in the archive: its header, a long-name record when its name needs one,
/// and its contents in whole blocks.
fn archived_size(relative: &str, size: u64) -> u64 {
    let name = relative.len() as u64 + 2;
    let long = if name > 100 { 512 + (name + 1).div_ceil(512) * 512 } else { 0 };
    long + 512 + size.div_ceil(512) * 512
}

/// The end of an archive: two zero blocks, padded to a 10 KiB record.
const ARCHIVE_END: u64 = 10240;

/// Writes one archive into its slot of the image, and refuses to leave it.
struct SlotWriter<'f> {
    file: &'f fs::File,
    at: u64,
    end: u64,
    buf: Vec<u8>,
}

impl SlotWriter<'_> {
    fn new(file: &fs::File, slot: Slot) -> SlotWriter<'_> {
        SlotWriter { file, at: slot.offset, end: slot.offset + slot.len, buf: Vec::with_capacity(CHUNK + 4096) }
    }
}

impl Write for SlotWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        if self.at + self.buf.len() as u64 > self.end {
            return Err(io::Error::other("the archive is larger than its place on the transfer disk"));
        }
        self.file.write_all_at(&self.buf, self.at)?;
        self.at += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }
}

/// The checkpoint number in the last complete line of what the guest's tar has said so far
/// (`tar: Read checkpoint 48`). `pending` keeps the unfinished line between pieces.
fn last_checkpoint(pending: &mut Vec<u8>, piece: &[u8]) -> Option<u64> {
    pending.extend_from_slice(piece);
    let end = pending.iter().rposition(|b| *b == b'\n')?;
    let complete: Vec<u8> = pending.drain(..=end).collect();
    String::from_utf8_lossy(&complete)
        .lines()
        .rev()
        .find_map(|line| line.trim().rsplit_once("checkpoint ").and_then(|(_, n)| n.trim().parse().ok()))
}

impl TransferDisk {
    fn label_offset() -> u64 {
        TRANSFER_CAPACITY - RECORD
    }

    /// Makes the VM's transfer disk file, empty but for a label the guest's device is known by.
    /// Before the SSH ramdisk VM starts. An earlier boot's file is replaced.
    pub fn create(image: &Path) -> Result<()> {
        use std::os::unix::fs::FileExt;
        let _ = fs::remove_file(image);
        let file = fs::File::create(image).map_err(|e| Error::path(image, e))?;
        file.set_len(TRANSFER_CAPACITY).map_err(|e| Error::path(image, e))?;
        let label = format!("vphone-transfer-{:032x}\n", rand::random::<u128>());
        file.write_all_at(label.as_bytes(), Self::label_offset()).map_err(|e| Error::path(image, e))?;
        Ok(())
    }

    /// The guest's device for the transfer disk file `image`: the whole disk whose last record
    /// carries the file's label. `None`, with the reason logged, when there is no such file or the
    /// guest shows no such disk; the copies then go over SSH.
    pub fn find(ssh: &GuestSsh, image: &Path, progress: &dyn Progress) -> Result<Option<TransferDisk>> {
        use std::os::unix::fs::FileExt;
        let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(image) else {
            progress.log("[*] no transfer disk in this VM: the large copies go over SSH");
            return Ok(None);
        };
        let mut label = [0u8; 49];
        let size = file.metadata().map_err(|e| Error::path(image, e))?.len();
        if size != TRANSFER_CAPACITY || file.read_exact_at(&mut label, Self::label_offset()).is_err() || !label.starts_with(b"vphone-transfer-") {
            progress.log("[*] the transfer disk's file is not as it was made: the large copies go over SSH");
            return Ok(None);
        }
        let label = String::from_utf8_lossy(&label).trim().to_owned();
        let script = format!(
            "for d in /dev/rdisk[0-9] /dev/rdisk[0-9][0-9]; do [ -e \"$d\" ] || continue; \
             l=$({GUEST_DD} if=\"$d\" bs={RECORD} skip={} count=1 2>/dev/null | /usr/bin/head -n 1 2>/dev/null); \
             [ \"$l\" = {label} ] && echo \"$d\"; done 2>/dev/null; true",
            Self::label_offset() / RECORD
        );
        let found = ssh.exec(&script, Some(Duration::from_secs(300)))?.check("looking for the transfer disk")?.stdout_text();
        match found.lines().next().map(str::trim).filter(|d| d.starts_with("/dev/rdisk")) {
            Some(device) => {
                progress.log(&format!("[*] transfer disk: {} is the VM's {device}", image.display()));
                Ok(Some(TransferDisk { image: image.to_path_buf(), device: device.to_owned(), streams: 1, file }))
            }
            None => {
                progress.log("[*] the VM shows no transfer disk: the large copies go over SSH");
                Ok(None)
            }
        }
    }

    /// The transfer disk file `image`, which the guest reads as `device`. [`TransferDisk::find`]
    /// is how the install gets one; this is for a guest whose device is known.
    pub fn open(image: &Path, device: &str) -> Result<TransferDisk> {
        let file = fs::OpenOptions::new().read(true).write(true).open(image).map_err(|e| Error::path(image, e))?;
        Ok(TransferDisk { image: image.to_path_buf(), device: device.to_owned(), streams: 1, file })
    }

    /// With this many of the guest's tars reading at once.
    pub fn with_readers(mut self, readers: usize) -> TransferDisk {
        self.streams = readers.max(1);
        self
    }

    /// Why a copy of `files` does not go by this disk, if it does not.
    fn refuses(&self, files: &[(String, u64)]) -> Option<String> {
        let bytes: u64 = files.iter().map(|f| f.1).sum();
        if bytes < DISK_MIN_BYTES {
            return Some(format!("{} is a small copy", megabytes(bytes)));
        }
        // Directories and links take a header each; they are few beside this.
        let archive = records(files.iter().map(|(name, size)| archived_size(name, *size)).sum::<u64>()) + (64 << 20);
        if archive + TAR_RECORD > Self::label_offset() {
            return Some(format!("an archive of {} is larger than the disk", megabytes(archive)));
        }
        let free = free_space(self.image.parent().unwrap_or(Path::new(".")));
        match free {
            Some(free) if free < archive + DISK_SPARE_BYTES => {
                Some(format!("the archive needs {} here, {} is free", megabytes(archive + DISK_SPARE_BYTES), megabytes(free)))
            }
            _ => None,
        }
    }

    /// Gives the file's space back; its size, which the guest was told, stays.
    fn empty(&self, used: u64) {
        use std::os::fd::AsRawFd;
        // SAFETY: a plain system call on a file this owns.
        let punched = unsafe {
            libc::fallocate(self.file.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 0, used.min(Self::label_offset()) as libc::off_t)
        };
        if punched != 0 {
            // A filesystem without holes: the space is given back when the file goes, at the step's end.
            log::debug!("transfer disk: no hole punched: {}", io::Error::last_os_error());
        }
    }

    /// The guest's command that unpacks the archive in `slot` into `dest`. Its tar reads the raw
    /// device itself, as its standard input, from where a `dd` on the same open file left off:
    /// through a pipe from `dd` the same archive took three times as long.
    fn unpack_command(&self, slot: Slot, dest: &str) -> String {
        let start = slot.offset / RECORD;
        let seek = if start == 0 { String::new() } else { format!("{GUEST_DD} bs={RECORD} skip={} count=1 of=/dev/null 2>/dev/null; ", start - 1) };
        format!(
            "( {seek}exec {GUEST_TAR} -xpf - -b {} --checkpoint={CHECKPOINT_RECORDS} --numeric-owner -C {} ) < {}",
            TAR_RECORD / 512,
            shell_quote(dest),
            self.device
        )
    }

    /// Writes `part` of the tree as an archive into `slot`.
    #[allow(clippy::too_many_arguments)]
    fn pack(
        &self,
        slot: Slot,
        source: &dyn TreeSource,
        owner: Option<(u32, u32)>,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        part: Part,
        meter: Option<&Mutex<Meter>>,
        progress: &dyn Progress,
    ) -> Result<(TreeStats, Archive)> {
        let mut out = SlotWriter::new(&self.file, slot);
        let (stats, file_ends) = {
            let mut sink = TarSink { out: &mut out, owner, stats: TreeStats::default(), progress, meter, part, position: 0, marks: Some(Vec::new()) };
            source.emit(&mut sink, exclude)?;
            (sink.stats, sink.marks.unwrap_or_default())
        };
        let disk = |e| Error::io(format!("writing the transfer disk {}", self.image.display()), e);
        out.write_all(&[0u8; ARCHIVE_END as usize]).map_err(disk)?;
        // To a whole record: the guest reads whole records.
        let written = out.at + out.buf.len() as u64 - slot.offset;
        out.write_all(&vec![0u8; (records(written) - written) as usize]).map_err(disk)?;
        out.flush().map_err(disk)?;
        let contents = stats.bytes;
        Ok((stats, Archive { slot: Slot { offset: slot.offset, len: out.at - slot.offset }, file_ends, contents }))
    }

    /// Has the guest unpack `archive`; what its tar says of its progress goes to `meter`, as the
    /// bytes it has read and the files that end within them.
    fn unpack(&self, ssh: &GuestSsh, archive: &Archive, dest: &str, meter: Option<&Mutex<Meter>>, progress: &dyn Progress) -> Result<()> {
        let slot = archive.slot;
        let contents: u64 = archive.contents;
        // (the unfinished line, the bytes this reader has reported, its files)
        let state = Mutex::new((Vec::new(), 0u64, 0u64));
        let report = |read: u64| {
            let mut state = state.lock().unwrap();
            let files = archive.file_ends.partition_point(|end| *end <= read) as u64;
            // In the files' own bytes, which is what the copy was announced as: an archive is a
            // little more (headers, padding).
            let read = (read as u128 * contents as u128 / slot.len.max(1) as u128) as u64;
            if let Some(meter) = meter.filter(|_| read > state.1 || files > state.2) {
                meter.lock().unwrap().add_with_files(read.saturating_sub(state.1), files.saturating_sub(state.2), progress);
                (state.1, state.2) = (read.max(state.1), files.max(state.2));
            }
        };
        let output = ssh.exec_watching(
            &self.unpack_command(slot, dest),
            &|piece| {
                let checkpoint = last_checkpoint(&mut state.lock().unwrap().0, piece);
                if let Some(checkpoint) = checkpoint {
                    report((checkpoint * TAR_RECORD).min(slot.len));
                }
            },
            progress,
        )?;
        if output.status != 0 {
            let said = output.stderr_text();
            let why: Vec<&str> = said.lines().filter(|l| !l.contains("checkpoint")).collect();
            bail!("guest tar failed with status {}: {}", output.status, why.join("\n"));
        }
        report(slot.len);
        Ok(())
    }

    /// The copy: the archives written to the file here, then unpacked by the guest from its device.
    #[allow(clippy::too_many_arguments)]
    fn copy(
        &self,
        what: &str,
        ssh: &GuestSsh,
        dest: &str,
        source: &dyn TreeSource,
        owner: Option<(u32, u32)>,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        files: Vec<(String, u64)>,
        progress: &dyn Progress,
    ) -> Result<TreeStats> {
        let (count, bytes) = (files.len() as u64, files.iter().map(|f| f.1).sum::<u64>());
        // Several readers are for many files. A few large ones are written fastest one after
        // the other (measured: 2 GB in four files unpacked in 4 s by one reader, 7 s by three).
        let streams = if files.len() >= DISK_PARALLEL_MIN_FILES { self.streams.max(1) } else { 1 };
        progress.log(&format!(
            "[*] copying {what} to the VM: {count} files, {} (transfer disk {}, {})",
            megabytes(bytes),
            self.device,
            if streams == 1 { "one reader".to_owned() } else { format!("{streams} readers") }
        ));
        // The bar: a tenth for writing the archive here, the rest for the guest unpacking it.
        const PACKED: f64 = 0.1;
        let packing = Mutex::new(Meter::titled(format!("Packing {what} for the VM's transfer disk"), bytes, (0.0, PACKED)));
        progress.progress(0, bytes, &format!("Packing {what} for the VM's transfer disk: 0% (0 of {})", megabytes(bytes)));
        let began = ActiveInstant::now();
        // The guest's tar reads whole records of its own: none may reach the label.
        let whole = Slot { offset: 0, len: Self::label_offset() - TAR_RECORD };
        // Each archive with what unpacks it: `true` for those the guest reads at the same time.
        let mut stats;
        let mut first: Vec<Archive> = Vec::new();
        let mut together: Vec<Archive> = Vec::new();
        let mut last: Vec<Archive> = Vec::new();
        if streams == 1 {
            let (packed, archive) = self.pack(whole, source, owner, exclude, Part::default(), Some(&packing), progress)?;
            stats = packed;
            together.push(archive);
        } else {
            // As over several connections: the directories and links, then each reader its share
            // of the files, then the directories again for their times.
            let only_dirs = Part { no_dirs: false, no_links: false, files: Some(Arc::new(HashSet::new())) };
            let (packed, dirs) = self.pack(whole, source, owner, exclude, only_dirs.clone(), None, progress)?;
            stats = packed;
            let mut at = dirs.slot.len;
            let dirs_len = dirs.slot.len;
            first.push(dirs);
            let mut parts = Vec::new();
            for names in shares_weighed(files.clone(), streams, FILE_COST_BYTES) {
                let size: u64 = files.iter().filter(|f| names.contains(&f.0)).map(|f| archived_size(&f.0, f.1)).sum();
                let slot = Slot { offset: at, len: records(size + ARCHIVE_END) };
                at += slot.len;
                parts.push((Part { no_dirs: true, no_links: true, files: Some(Arc::new(names)) }, slot));
            }
            if at + dirs_len > whole.len {
                bail!("the archives of {what} do not fit the transfer disk");
            }
            let done: Vec<Result<(TreeStats, Archive)>> = std::thread::scope(|scope| {
                let running: Vec<_> = parts
                    .iter()
                    .map(|(part, slot)| {
                        let (part, slot, packing) = (part.clone(), *slot, &packing);
                        scope.spawn(move || self.pack(slot, source, owner, exclude, part, Some(packing), progress))
                    })
                    .collect();
                running.into_iter().map(|t| t.join().unwrap_or_else(|_| Err(Error::msg("an archive writer panicked")))).collect()
            });
            for (part, (_, slot)) in done.into_iter().zip(&parts) {
                let (part, archive) = part?;
                if archive.slot != *slot {
                    bail!("an archive of {what} came to {} bytes, not the {} it was given", archive.slot.len, slot.len);
                }
                stats.files += part.files;
                stats.bytes += part.bytes;
                for (name, n) in part.xattrs_not_carried {
                    *stats.xattrs_not_carried.entry(name).or_default() += n;
                }
                together.push(archive);
            }
            let (_, dirs) = self.pack(Slot { offset: at, len: whole.len - at }, source, owner, exclude, Part { no_links: true, ..only_dirs }, None, progress)?;
            last.push(dirs);
        }
        let used = first.iter().chain(&together).chain(&last).map(|a| a.slot.offset + a.slot.len).max().unwrap_or(0);
        let packed_in = began.elapsed().as_secs_f64();
        let packed_at = packing.lock().unwrap().average();
        check_cancel(progress)?;

        let unpacking = Mutex::new(Meter::titled(format!("Copying {what} to the VM from its transfer disk"), bytes, (PACKED, 1.0)).counting_files(count));
        let unpack_began = ActiveInstant::now();
        let unpacked = (|| -> Result<()> {
            for archive in &first {
                self.unpack(ssh, archive, dest, None, progress)?;
            }
            let done: Vec<Result<()>> = std::thread::scope(|scope| {
                let running: Vec<_> = together
                    .iter()
                    .map(|archive| {
                        let unpacking = &unpacking;
                        scope.spawn(move || self.unpack(ssh, archive, dest, Some(unpacking), progress))
                    })
                    .collect();
                running.into_iter().map(|t| t.join().unwrap_or_else(|_| Err(Error::msg("a copy stream panicked")))).collect()
            });
            for part in done {
                part?;
            }
            for archive in &last {
                self.unpack(ssh, archive, dest, None, progress)?;
            }
            Ok(())
        })();
        self.empty(used);
        unpacked?;
        let unpacked_in = unpack_began.elapsed().as_secs_f64();
        let all = began.elapsed().as_secs_f64().max(0.001);
        // Every guest tar has exited: everything sent is on its disk.
        progress.log(&format!(
            "[+] copied {what}: {} in {} (average {:.1} MB/s; packed here in {} at {:.0} MB/s, unpacked by the VM in {} at {:.1} MB/s)",
            megabytes(bytes),
            time_text(all),
            bytes as f64 / all / 1_000_000.0,
            time_text(packed_in),
            packed_at,
            time_text(unpacked_in),
            bytes as f64 / unpacked_in.max(0.001) / 1_000_000.0
        ));
        progress.progress(bytes, bytes, &format!("Copied {what} to the VM"));
        Ok(stats)
    }
}

/// The bytes free for this user on the filesystem of `dir`; `None` when it does not say.
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid C string and `stat` is a zeroed structure the call fills.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_s_line_gives_the_share_the_speed_and_the_time_left() {
        let mut m = Meter::new("the patched dyld cache", 2_000_000_000);
        m.sent = 100_000_000;
        assert_eq!(m.line(1.0), "Copying the patched dyld cache to the VM: 5% (100 MB of 2000 MB) \u{b7} measuring speed");
        // 10 s later, 100 MB more: 10 MB/s, and 1800 MB left is 3 minutes.
        m.sent = 200_000_000;
        assert_eq!(m.line(11.0), "Copying the patched dyld cache to the VM: 10% (200 MB of 2000 MB) \u{b7} 10 MB/s \u{b7} about 3 min left");
        // Slow, as on a phone: one decimal, hours spelled out.
        m.samples.clear();
        m.sent = 200_000_000;
        m.line(100.0);
        m.sent = 205_000_000;
        assert_eq!(m.line(110.0), "Copying the patched dyld cache to the VM: 10% (205 MB of 2000 MB) \u{b7} 0.5 MB/s \u{b7} about 60 min left");
        // The speed is the last half minute's, not the average since the start.
        for (at, sent) in [(120.0, 300_000_000u64), (135.0, 600_000_000), (150.0, 900_000_000)] {
            m.sent = sent;
            m.line(at);
        }
        m.sent = 1_200_000_000;
        assert!(m.line(165.0).contains("\u{b7} 20 MB/s \u{b7} about 40 s left"), "{}", m.line(165.0));
        // Done: nothing is left to estimate.
        m.sent = 2_000_000_000;
        assert!(m.line(200.0).starts_with("Copying the patched dyld cache to the VM: 100% (2000 MB of 2000 MB) \u{b7} "));
        assert!(!m.line(201.0).contains("left"));
    }

    #[test]
    fn a_copy_that_counts_files_gives_the_share_and_the_time_left_of_both() {
        // 1000 MB in 2000 files: making the files is about as much work for the guest as the bytes.
        let mut m = Meter::titled("Copying the system to the VM from its transfer disk".into(), 1_000_000_000, (0.1, 1.0)).counting_files(2000);
        m.line(0.0);
        // Every byte read and no file made yet (the large files came first): not nearly done.
        m.sent = 1_000_000_000;
        assert_eq!(
            m.line(10.0),
            "Copying the system to the VM from its transfer disk: 48% (1000 MB of 1000 MB, 0 of 2000 files) \u{b7} 100 MB/s \u{b7} about 10 s left"
        );
        // The bar: a tenth was the packing, and 48% of the rest is done.
        assert_eq!(m.bar_done(), 539_329_563);
        m.files = Some((2000, 2000));
        assert_eq!(m.bar_done(), 1_000_000_000);
        assert!(!m.line(20.0).contains("left"));
    }

    #[test]
    fn a_local_tree_is_measured_as_it_would_be_sent() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/one"), vec![1u8; 1000]).unwrap();
        fs::write(dir.path().join("a/b/two"), vec![2u8; 234]).unwrap();
        fs::write(dir.path().join("skipped"), vec![3u8; 5000]).unwrap();
        std::os::unix::fs::symlink("a/one", dir.path().join("link")).unwrap();
        let tree = LocalTree::new(dir.path());
        assert_eq!(tree.measure(&|rel| rel == "skipped").unwrap(), (2, 1234));
        assert_eq!(tree.measure(&|rel| rel == "a").unwrap(), (1, 5000), "an excluded directory takes its files with it");
    }

    #[test]
    fn files_are_shared_out_evenly_by_size_and_each_goes_once() {
        let files: Vec<(String, u64)> = [900u64, 500, 400, 300, 200, 100, 1].iter().enumerate().map(|(i, s)| (format!("f{i}"), *s)).collect();
        let bins = shares(files.clone(), 3);
        assert_eq!(bins.len(), 3);
        let size = |bin: &HashSet<String>| files.iter().filter(|f| bin.contains(&f.0)).map(|f| f.1).sum::<u64>();
        let sizes: Vec<u64> = bins.iter().map(size).collect();
        assert_eq!(sizes.iter().sum::<u64>(), 2401);
        assert!(sizes.iter().max().unwrap() - sizes.iter().min().unwrap() <= 200, "{sizes:?}");
        assert_eq!(bins.iter().map(HashSet::len).sum::<usize>(), 7, "no file twice, none left out");
        assert_eq!(shares(vec![("only".into(), 5)], 4).len(), 1, "no empty stream");
    }

    #[test]
    fn many_small_files_are_spread_over_the_readers_beside_a_few_large_ones() {
        // By bytes alone the three large files take a reader each and one of them all the small.
        let mut files: Vec<(String, u64)> = (0..3).map(|i| (format!("big{i}"), 500 << 20)).collect();
        files.extend((0..3000).map(|i| (format!("small{i}"), 1000)));
        let bins = shares_weighed(files, 3, FILE_COST_BYTES);
        let counts: Vec<usize> = bins.iter().map(HashSet::len).collect();
        assert_eq!(counts.iter().sum::<usize>(), 3003);
        assert!(counts.iter().all(|c| (990..=1012).contains(c)), "{counts:?}");
    }

    #[test]
    fn an_archive_s_size_is_known_before_it_is_written() {
        struct Quiet;
        impl Progress for Quiet {
            fn log(&self, _: &str) {}
            fn progress(&self, _: u64, _: u64, _: &str) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        let long = format!("deep/{}", "n".repeat(120));
        fs::create_dir_all(root.join("deep")).unwrap();
        // Sizes on and off a block boundary, a name of exactly the 100 bytes a header holds
        // (with its "./"), one a byte longer, and one that needs a long-name record of two blocks.
        let exact = "e".repeat(98);
        let over = "o".repeat(99);
        let two_blocks = "t".repeat(600);
        for (name, size) in [("empty", 0usize), ("one", 1), ("block", 512), ("more", 513), (long.as_str(), 70_000), (exact.as_str(), 5), (over.as_str(), 5), (two_blocks.as_str(), 1025)] {
            if name.len() < 255 {
                fs::write(root.join(name), vec![7u8; size]).unwrap();
            }
        }
        let tree = LocalTree::new(&root);
        let files = tree.files(&|_| false).unwrap();
        assert_eq!(files.len(), 7, "the 600-byte name is no file name here");
        // The files alone, as a reader's share is: no directories, no links.
        let mut out = Vec::new();
        let part = Part { no_dirs: true, no_links: true, files: None };
        let mut sink = TarSink { out: &mut out, owner: None, stats: TreeStats::default(), progress: &Quiet, meter: None, part, position: 0, marks: None };
        tree.emit(&mut sink, &|_| false).unwrap();
        assert_eq!(out.len() as u64, files.iter().map(|(name, size)| archived_size(name, *size)).sum::<u64>());
        assert_eq!(archived_size(&two_blocks, 1025), 512 + 1024 + 512 + 1536, "a long name's record is whole blocks too");
        assert_eq!((records(0), records(1), records(RECORD), records(RECORD + 1)), (0, RECORD, RECORD, 2 * RECORD));
    }

    #[test]
    fn the_guest_s_tar_reads_the_device_itself_from_its_archive_s_place() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("t.img");
        fs::write(&image, b"").unwrap();
        let disk = TransferDisk::open(&image, "/dev/rdisk0").unwrap();
        assert_eq!(
            disk.unpack_command(Slot { offset: 0, len: 3 * RECORD }, "/mnt1/a b"),
            "( exec /usr/bin/tar -xpf - -b 8192 --checkpoint=4 --numeric-owner -C '/mnt1/a b' ) < /dev/rdisk0"
        );
        // From 5 MiB on: a dd on the same open device reads the record before, and tar goes on from there.
        assert_eq!(
            disk.unpack_command(Slot { offset: 5 * RECORD, len: RECORD }, "/mnt1/x"),
            "( /bin/dd bs=1048576 skip=4 count=1 of=/dev/null 2>/dev/null; exec /usr/bin/tar -xpf - -b 8192 --checkpoint=4 --numeric-owner -C '/mnt1/x' ) < /dev/rdisk0"
        );
    }

    #[test]
    fn the_guest_tar_s_checkpoints_are_read_across_pieces() {
        let mut pending = Vec::new();
        assert_eq!(last_checkpoint(&mut pending, b"tar: Read check"), None, "no whole line yet");
        assert_eq!(last_checkpoint(&mut pending, b"point 4\ntar: Read checkpoint 8\ntar: Read chec"), Some(8));
        assert_eq!(last_checkpoint(&mut pending, b"kpoint 12\n"), Some(12));
        assert_eq!(last_checkpoint(&mut pending, b"tar: ./x: Cannot open: No such file\n"), None);
        assert_eq!(last_checkpoint(&mut pending, b"/usr/bin/tar: Read checkpoint 16\n"), Some(16));
    }

    #[test]
    fn the_disk_takes_large_copies_that_fit_and_says_why_not_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("t.img");
        TransferDisk::create(&image).unwrap();
        assert_eq!(fs::metadata(&image).unwrap().len(), TRANSFER_CAPACITY);
        let disk = TransferDisk::open(&image, "/dev/rdisk0").unwrap();
        assert_eq!(disk.refuses(&[("small".into(), 5 << 20)]).as_deref(), Some("5.2 MB is a small copy"));
        assert_eq!(disk.refuses(&[("dyld".into(), 100 << 20)]), None);
        let too_large = disk.refuses(&[("huge".into(), TRANSFER_CAPACITY)]).unwrap();
        assert!(too_large.contains("larger than the disk"), "{too_large}");
        // What no filesystem here has free.
        let no_room = disk.refuses(&[("a".into(), 30 << 30), ("b".into(), 30 << 30)]);
        if free_space(dir.path()).is_some_and(|free| free < 60 << 30) {
            assert!(no_room.unwrap().contains("is free"));
        }
        // An archive never leaves its place.
        let mut w = SlotWriter::new(&disk.file, Slot { offset: RECORD, len: 10 });
        w.write_all(&[1u8; 11]).unwrap();
        assert!(w.flush().is_err());
    }

    #[test]
    fn times_and_sizes_read_plainly() {
        assert_eq!((time_text(0.2), time_text(45.0), time_text(90.0), time_text(3599.0), time_text(3700.0)), ("1 s".into(), "45 s".into(), "2 min".into(), "60 min".into(), "1 h 01 min".into()));
        assert_eq!((megabytes(1_500_000), megabytes(2_345_000_000)), ("1.5 MB".into(), "2345 MB".into()));
    }
}
