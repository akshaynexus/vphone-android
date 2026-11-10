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
    what: String,
    total: u64,
    sent: u64,
    started: ActiveInstant,
    reported: ActiveInstant,
    /// (seconds since the start, bytes sent then), over the window the speed is measured on.
    samples: VecDeque<(f64, u64)>,
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
        let now = ActiveInstant::now();
        Meter { what: what.to_owned(), total, sent: 0, started: now, reported: now, samples: VecDeque::new() }
    }

    /// The line for `sent` bytes at `at` seconds, with what the samples so far say of the speed.
    fn line(&mut self, at: f64) -> String {
        self.samples.push_back((at, self.sent));
        while self.samples.len() > 2 && self.samples[0].0 < at - SPEED_WINDOW_SECS {
            self.samples.pop_front();
        }
        let (first_at, first_sent) = self.samples[0];
        let mut text = format!("Copying {} to the VM: ", self.what);
        if self.total > 0 {
            text += &format!("{}% ({} of {})", self.sent.min(self.total) * 100 / self.total, megabytes(self.sent), megabytes(self.total));
        } else {
            text += &megabytes(self.sent);
        }
        let span = at - first_at;
        if span < SPEED_MIN_SECS || self.sent <= first_sent {
            return text + " \u{b7} measuring speed";
        }
        let per_sec = (self.sent - first_sent) as f64 / span;
        text += &if per_sec < 10_000_000.0 {
            format!(" \u{b7} {:.1} MB/s", per_sec / 1_000_000.0)
        } else {
            format!(" \u{b7} {:.0} MB/s", per_sec / 1_000_000.0)
        };
        if self.total > self.sent {
            text += &format!(" \u{b7} about {} left", time_text((self.total - self.sent) as f64 / per_sec));
        }
        text
    }

    fn add(&mut self, bytes: u64, progress: &dyn Progress) {
        self.sent += bytes;
        if self.reported.elapsed() >= REPORT_EVERY {
            self.reported = ActiveInstant::now();
            let line = self.line(self.started.elapsed().as_secs_f64());
            progress.progress(self.sent.min(self.total.max(self.sent)), self.total, &line);
        }
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
        self.pad(size)?;
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
                let mut sink = TarSink { out: &mut out, owner, stats: TreeStats::default(), progress, meter, part };
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
fn shares(mut files: Vec<(String, u64)>, bins: usize) -> Vec<HashSet<String>> {
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut out = vec![(0u64, HashSet::new()); bins.max(1)];
    for (name, size) in files {
        let least = out.iter_mut().min_by_key(|(bytes, _)| *bytes).expect("at least one list");
        least.0 += size;
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
    upload_tree_over(what, ssh, None, dest, source, owner, exclude, progress)
}

/// As [`upload_tree_as`], over several connections when `more` offers them and the copy is large:
/// the guest's SSH server is one process a connection and the slowest part of the path.
#[allow(clippy::too_many_arguments)]
pub fn upload_tree_over(
    what: &str,
    ssh: &GuestSsh,
    more: Option<&MoreLinks<'_>>,
    dest: &str,
    source: &dyn TreeSource,
    owner: Option<(u32, u32)>,
    exclude: &(dyn Fn(&str) -> bool + Sync),
    progress: &dyn Progress,
) -> Result<TreeStats> {
    let files = source.files(exclude)?;
    let (count, bytes) = (files.len() as u64, files.iter().map(|f| f.1).sum::<u64>());
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
    fn times_and_sizes_read_plainly() {
        assert_eq!((time_text(0.2), time_text(45.0), time_text(90.0), time_text(3599.0), time_text(3700.0)), ("1 s".into(), "45 s".into(), "2 min".into(), "60 min".into(), "1 h 01 min".into()));
        assert_eq!((megabytes(1_500_000), megabytes(2_345_000_000)), ("1.5 MB".into(), "2345 MB".into()));
    }
}
