//! Copying a directory tree into the guest as a tar stream to its `tar`.
//!
//! SFTP costs several round trips per file over an emulated dropbear; a cryptex
//! is hundreds of thousands of small files. `tar` over one exec channel carries
//! contents, modes and numeric owners in one stream. The ramdisk's tar is GNU
//! tar 1.17, so entries use GNU long-name records. Extended attributes cannot be
//! carried: neither that tar nor `sftp-server` can set them, the same limit the
//! sshfs path had. They are counted and reported.

use crate::bail;
use crate::error::{Error, Result};
use crate::guestfs::shell_quote;
use crate::progress::{check_cancel, Progress};
use crate::ssh::GuestSsh;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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

/// Writes GNU tar entries; `owner` overrides each entry's uid/gid when set.
pub struct TarSink<'a> {
    out: &'a mut dyn Write,
    owner: Option<(u32, u32)>,
    stats: TreeStats,
    progress: &'a dyn Progress,
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
        self.header(&Self::name(relative, true), EntryType::Directory, 0, meta, None)?;
        self.stats.dirs += 1;
        Ok(())
    }

    pub fn symlink(&mut self, relative: &str, target: &str, meta: Meta) -> Result<()> {
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
        self.header(&Self::name(relative, false), EntryType::Regular, size, meta, None)?;
        let wrote = write_contents(&mut *self.out)?;
        if wrote != size {
            bail!("{relative}: wrote {wrote} bytes, expected {size}");
        }
        self.pad(size)?;
        self.stats.files += 1;
        self.stats.bytes += size;
        if self.stats.files % 64 == 0 {
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

impl TreeSource for LocalTree {
    fn emit(&self, sink: &mut TarSink<'_>, exclude: &(dyn Fn(&str) -> bool + Sync)) -> Result<()> {
        let m = fs::symlink_metadata(&self.root).map_err(|e| Error::path(&self.root, e))?;
        let meta = self.meta_of("", &m);
        sink.dir("", meta)?;
        self.visit("", sink, exclude)
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
    let command = format!("{GUEST_TAR} -xpf - --numeric-owner -C {}", shell_quote(dest));
    let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::scope(|scope| {
        let producer = scope.spawn(move || -> Result<TreeStats> {
            let mut out = ChunkWriter { tx, buf: Vec::with_capacity(CHUNK + 4096) };
            let stats = {
                let mut sink = TarSink { out: &mut out, owner, stats: TreeStats::default(), progress };
                source.emit(&mut sink, exclude)?;
                sink.stats
            };
            // End of archive: two zero blocks, then pad to a 10 KiB record.
            out.write_all(&[0u8; 10240]).map_err(|e| Error::io("tar stream", e))?;
            out.flush().map_err(|e| Error::io("tar stream", e))?;
            Ok(stats)
        });
        let output = ssh.exec_with_stdin(&command, rx, progress);
        let stats = producer
            .join()
            .map_err(|_| Error::msg("tar producer panicked"))?;
        let output = output?;
        if output.status != 0 {
            bail!("guest tar failed with status {}: {}", output.status, output.stderr_text());
        }
        stats
    })
}
