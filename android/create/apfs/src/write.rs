//! Writing to a volume, through the `apfs-write` transaction engine.
//!
//! `apfs-write` owns allocation, B-tree copy-on-write, the object map, free
//! queues and the checkpoint commit. This module adapts it to image files
//! (including IM4P-wrapped ramdisks), resolves paths by reading the
//! transaction's staged state, and groups operations into checkpoints.
//!
//! Atomicity: changes reach the image only at [`MutVolume::commit`], or at an
//! automatic checkpoint between operations once a transaction grows large.
//! After any failed operation the transaction is discarded and this handle
//! refuses further use; the image stays at its last committed checkpoint.

use crate::btree::{KeyKind, Node, NodeSource, Tree};
use crate::container::{pick_volume, Container, VolumeSelector};
use crate::error::{corrupt, Error, Result};
use crate::image::Image;
use crate::raw::*;
use crate::volume::{CatView, DirEntry, Inode, Xattr};
use apfs_core::block_device::{BlockDevice, BlockError, WritableBlockDevice};
use apfs_write::file as ops;
use apfs_write::txn::{Transaction, TxnError};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Commit automatically once this much is staged in memory.
const COMMIT_STAGED_BYTES: usize = 64 << 20;
const COMMIT_OPS: u32 = 4096;

fn werr(context: &str, e: TxnError) -> Error {
    Error::Write(format!("{context}: {e}"))
}

/// An [`Image`] as the write engine's block device.
pub struct ImageDev {
    img: Image,
}

fn io_err(e: Error) -> BlockError {
    match e {
        Error::Io(e) => BlockError::Io(e),
        other => BlockError::Io(std::io::Error::other(other.to_string())),
    }
}

impl BlockDevice for ImageDev {
    fn size(&self) -> u64 {
        self.img.len()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::result::Result<(), BlockError> {
        self.img.read_at(buf, offset).map_err(io_err)
    }
}

impl WritableBlockDevice for ImageDev {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::result::Result<(), BlockError> {
        self.img.write_at(buf, offset).map_err(io_err)
    }
    fn flush_data(&mut self) -> std::result::Result<(), BlockError> {
        self.img.sync().map_err(io_err)
    }
}

/// Reads nodes as the open transaction sees them.
struct TxnSrc<'a> {
    txn: RefCell<&'a mut Transaction<ImageDev>>,
    bs: usize,
}

impl NodeSource for TxnSrc<'_> {
    fn node(&self, tree: &Tree, oid: u64) -> Result<Rc<Node>> {
        let paddr = match tree.virt {
            None => oid,
            Some((omap_root, xid)) => {
                let omap = Tree { kind: KeyKind::Omap, root: omap_root, virt: None };
                let mut key = [0u8; 16];
                put64(&mut key, 0, oid);
                put64(&mut key, 8, xid);
                match omap.find_le(self, &key)? {
                    Some((k, v)) if le64(&k, 0) == oid && v.len() >= 16 => {
                        if le32(&v, 0) & OMAP_VAL_DELETED != 0 {
                            return corrupt(format!("omap record for oid {oid:#x} is deleted"));
                        }
                        le64(&v, 8)
                    }
                    _ => return corrupt(format!("omap has no record for oid {oid:#x}")),
                }
            }
        };
        let mut b = vec![0u8; self.bs];
        self.txn
            .borrow_mut()
            .read_block(paddr, &mut b)
            .map_err(|e| werr("reading a staged block", e))?;
        Ok(Rc::new(Node::parse(b, tree.kind)?))
    }
}

/// Attributes for a created or changed node. `None` leaves a value alone.
#[derive(Clone, Copy, Debug, Default)]
pub struct Attrs {
    /// Permission bits including setuid/setgid/sticky (the type bits are ignored).
    pub mode: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mtime_ns: Option<u64>,
    pub ctime_ns: Option<u64>,
    pub atime_ns: Option<u64>,
    pub btime_ns: Option<u64>,
    pub bsd_flags: Option<u32>,
}

impl Attrs {
    fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.uid.is_none()
            && self.gid.is_none()
            && self.mtime_ns.is_none()
            && self.ctime_ns.is_none()
            && self.atime_ns.is_none()
            && self.btime_ns.is_none()
            && self.bsd_flags.is_none()
    }
}

/// A volume open for writing.
pub struct MutVolume {
    path: PathBuf,
    sel: VolumeSelector,
    txn: Option<Transaction<ImageDev>>,
    poisoned: bool,
    vsb: Vec<u8>,
    omap: Vec<u8>,
    bs: usize,
    case_insensitive: bool,
    norm_insensitive: bool,
    ops_in_txn: u32,
}

/// Reads the current raw volume superblock and object-map header.
fn read_volume_raw(c: &Container, sel: &VolumeSelector) -> Result<(Vec<u8>, Vec<u8>)> {
    let list = c.volumes()?;
    let s = &list[pick_volume(&list, sel)?];
    if s.sealed {
        return Err(Error::Unsupported(format!(
            "volume '{}' is sealed; sealed volumes are read-only here, as with linux-apfs-rw \
             (write them from the guest instead)",
            s.name
        )));
    }
    if s.encrypted {
        return Err(Error::Unsupported(format!("volume '{}' is encrypted", s.name)));
    }
    let bs = c.block_size as usize;
    let mut vsb = vec![0u8; bs];
    c.read_block(s.paddr, &mut vsb)?;
    if !verify_checksum(&vsb) {
        return corrupt("bad checksum on the volume superblock");
    }
    let mut omap = vec![0u8; bs];
    c.read_block(le64(&vsb, FS_OMAP_OID), &mut omap)?;
    Ok((vsb, omap))
}

impl MutVolume {
    /// Opens a writable volume of the image at `path`. Refuses sealed or
    /// encrypted volumes.
    pub fn open(path: &Path, sel: &VolumeSelector) -> Result<MutVolume> {
        let c = Container::open(Image::open(path, false)?)?;
        let (vsb, omap) = read_volume_raw(&c, sel)?;
        let bs = c.block_size as usize;
        let incompat = le64(&vsb, FS_INCOMPAT);
        let mut v = MutVolume {
            path: path.to_path_buf(),
            sel: sel.clone(),
            txn: None,
            poisoned: false,
            vsb,
            omap,
            bs,
            case_insensitive: incompat & INCOMPAT_CASE_INSENSITIVE != 0,
            norm_insensitive: incompat
                & (INCOMPAT_CASE_INSENSITIVE | INCOMPAT_NORM_INSENSITIVE)
                != 0,
            ops_in_txn: 0,
        };
        drop(c);
        v.begin()?;
        Ok(v)
    }

    fn begin(&mut self) -> Result<()> {
        let dev = ImageDev { img: Image::open(&self.path, true)? };
        self.txn = Some(Transaction::begin(dev).map_err(|e| werr("opening the transaction", e))?);
        self.ops_in_txn = 0;
        Ok(())
    }

    fn txn(&mut self) -> Result<&mut Transaction<ImageDev>> {
        if self.poisoned {
            return Err(Error::Invalid("an earlier write failed; reopen the image".into()));
        }
        self.txn.as_mut().ok_or_else(|| Error::Invalid("no open transaction".into()))
    }

    /// Runs a catalog query against the transaction's staged state.
    fn with_view<T>(
        &mut self,
        f: impl FnOnce(&CatView<'_, TxnSrc<'_>>) -> Result<T>,
    ) -> Result<T> {
        let (vsb, omap) = self.current_raw()?;
        let root_oid = le64(&vsb, FS_ROOT_TREE_OID);
        let omap_root = le64(&omap, OM_TREE_OID);
        let (bs, case_insensitive, norm_insensitive) =
            (self.bs, self.case_insensitive, self.norm_insensitive);
        let txn = self.txn()?;
        let xid = txn.xid;
        let src = TxnSrc { txn: RefCell::new(txn), bs };
        let view = CatView {
            src: &src,
            cat: Tree {
                kind: KeyKind::Catalog { hashed: norm_insensitive },
                root: root_oid,
                virt: Some((omap_root, xid)),
            },
            case_insensitive,
            norm_insensitive,
        };
        f(&view)
    }

    /// The volume superblock and object map as staged right now.
    fn current_raw(&mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        let (vsb, omap) = (self.vsb.clone(), self.omap.clone());
        self.txn()?
            .current_volume(&vsb, &omap)
            .map_err(|e| werr("reading the staged volume", e))
    }

    fn run<T>(
        &mut self,
        what: &str,
        op: impl FnOnce(&mut Transaction<ImageDev>, &[u8], &[u8]) -> std::result::Result<T, TxnError>,
    ) -> Result<T> {
        let (vsb, omap) = (self.vsb.clone(), self.omap.clone());
        let r = op(self.txn()?, &vsb, &omap);
        match r {
            Ok(v) => {
                self.ops_in_txn += 1;
                Ok(v)
            }
            Err(e) => {
                self.txn = None;
                self.poisoned = true;
                Err(werr(what, e))
            }
        }
    }

    /// Writes all staged changes as a new checkpoint.
    pub fn commit(&mut self) -> Result<()> {
        let txn = self.txn.take().ok_or_else(|| {
            Error::Invalid("no open transaction (an earlier write failed)".to_string())
        })?;
        match txn.commit() {
            Ok(_) => {}
            Err(e) => {
                self.poisoned = true;
                return Err(werr("committing", e));
            }
        }
        let c = Container::open(Image::open(&self.path, false)?)?;
        let (vsb, omap) = read_volume_raw(&c, &self.sel)?;
        self.vsb = vsb;
        self.omap = omap;
        self.begin()
    }

    /// Checkpoints between operations when the transaction has grown large.
    pub fn commit_if_large(&mut self) -> Result<()> {
        let big = self.ops_in_txn >= COMMIT_OPS
            || self.txn()?.staged_bytes() >= COMMIT_STAGED_BYTES;
        if big {
            self.commit()?;
        }
        Ok(())
    }

    /// Discards everything since the last commit.
    pub fn abort(mut self) {
        if let Some(t) = self.txn.take() {
            let _ = t.abort();
        }
    }

    // --- Queries (see uncommitted changes) ---

    pub fn lookup(&mut self, path: &str) -> Result<Inode> {
        self.with_view(|v| v.lookup(path))
    }

    pub fn exists(&mut self, path: &str) -> Result<bool> {
        match self.lookup(path) {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn read_dir(&mut self, ino: u64) -> Result<Vec<DirEntry>> {
        self.with_view(|v| v.read_dir(ino))
    }

    pub fn inode(&mut self, ino: u64) -> Result<Inode> {
        self.with_view(|v| v.inode(ino))
    }

    pub fn xattrs(&mut self, ino: u64) -> Result<Vec<Xattr>> {
        self.with_view(|v| v.xattrs(ino))
    }

    /// (parent directory inode, final component).
    fn split(&mut self, path: &str) -> Result<(u64, String)> {
        let trimmed = path.trim_end_matches('/');
        let (dir, name) = match trimmed.rfind('/') {
            Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        if name.is_empty() || name == "." {
            return Err(Error::Invalid(format!("'{path}' has no final component")));
        }
        let parent = self.lookup(if dir.is_empty() { "/" } else { dir })?;
        if parent.mode & S_IFMT != S_IFDIR {
            return Err(Error::Invalid(format!("{dir}: not a directory")));
        }
        Ok((parent.ino, name.to_string()))
    }

    fn child(&mut self, parent: u64, name: &str) -> Result<Option<DirEntry>> {
        self.with_view(|v| v.lookup_child(parent, name))
    }

    // --- Mutations ---

    fn apply_attrs(&mut self, parent: u64, name: &str, a: &Attrs) -> Result<()> {
        if a.is_empty() {
            return Ok(());
        }
        let mode_set = a.mode.map(|m| m & 0o7777);
        let mode_clear = a.mode.map(|_| 0o7777);
        // set_inode_attrs ORs in `bsd_flags_set` after clearing `bsd_flags_clear`.
        let (flags_set, flags_clear) = match a.bsd_flags {
            Some(f) => (Some(f), Some(!0u32)),
            None => (None, None),
        };
        self.run("setting attributes", |t, v, o| {
            ops::set_inode_attrs(
                t, v, o, parent, name, mode_set, mode_clear, a.mtime_ns, a.ctime_ns, a.atime_ns,
                a.btime_ns, flags_set, flags_clear, a.uid, a.gid,
            )
        })
    }

    pub fn set_attrs(&mut self, path: &str, a: &Attrs) -> Result<()> {
        let (parent, name) = self.split(path)?;
        if self.child(parent, &name)?.is_none() {
            return Err(Error::NotFound(path.to_string()));
        }
        self.apply_attrs(parent, &name, a)
    }

    /// Creates a directory. With `parents`, also creates missing ancestors
    /// (with `a`'s attributes) and accepts an existing directory.
    pub fn mkdir(&mut self, path: &str, a: &Attrs, parents: bool) -> Result<u64> {
        if parents {
            let mut cur = String::new();
            let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
            for (i, comp) in comps.iter().enumerate() {
                cur.push('/');
                cur.push_str(comp);
                let (parent, name) = self.split(&cur)?;
                match self.child(parent, &name)? {
                    Some(de) => {
                        let ino = self.inode(de.ino)?;
                        if ino.mode & S_IFMT != S_IFDIR {
                            return Err(Error::Invalid(format!("{cur}: not a directory")));
                        }
                        if i + 1 == comps.len() {
                            return Ok(de.ino);
                        }
                    }
                    None => {
                        let ino = self.mkdir_one(parent, &name, a)?;
                        if i + 1 == comps.len() {
                            return Ok(ino);
                        }
                    }
                }
            }
            return Ok(ROOT_DIR_INO);
        }
        let (parent, name) = self.split(path)?;
        if self.child(parent, &name)?.is_some() {
            return Err(Error::Invalid(format!("{path}: already exists")));
        }
        self.mkdir_one(parent, &name, a)
    }

    fn mkdir_one(&mut self, parent: u64, name: &str, a: &Attrs) -> Result<u64> {
        let ino = self.run("mkdir", |t, v, o| ops::mkdir(t, v, o, parent, name))?;
        self.apply_attrs(parent, name, a)?;
        Ok(ino)
    }

    /// Creates a regular file, replacing any existing non-directory.
    pub fn create_file(&mut self, path: &str, data: &[u8], a: &Attrs) -> Result<u64> {
        let (parent, name) = self.split(path)?;
        self.replace_with_file(parent, &name, data, a)
    }

    fn replace_with_file(&mut self, parent: u64, name: &str, data: &[u8], a: &Attrs) -> Result<u64> {
        if let Some(de) = self.child(parent, name)? {
            if self.inode(de.ino)?.mode & S_IFMT == S_IFDIR {
                return Err(Error::Invalid(format!("'{name}' is a directory")));
            }
            self.run("unlinking the replaced file", |t, v, o| ops::unlink(t, v, o, parent, name))?;
        }
        let ino = if data.is_empty() {
            self.run("create", |t, v, o| ops::create_file(t, v, o, parent, name))?
        } else {
            self.run("write", |t, v, o| ops::write_file(t, v, o, parent, name, data))?
        };
        self.apply_attrs(parent, name, a)?;
        Ok(ino)
    }

    /// Replaces a file's contents, keeping its mode, owner, BSD flags and
    /// user xattrs (the compressed-file machinery is dropped, since the new
    /// contents are stored uncompressed).
    pub fn replace_keep_metadata(&mut self, path: &str, data: &[u8]) -> Result<u64> {
        let (parent, name) = self.split(path)?;
        let de = self.child(parent, &name)?.ok_or_else(|| Error::NotFound(path.to_string()))?;
        let old = self.inode(de.ino)?;
        let xattrs: Vec<(String, Vec<u8>)> = {
            let mut out = Vec::new();
            for x in self.xattrs(old.ino)? {
                if x.name == XATTR_DECMPFS || x.name == XATTR_RSRC || x.flags & XATTR_FS_OWNED != 0 {
                    continue;
                }
                let value = match (&x.data, x.stream) {
                    (Some(d), _) => d.clone(),
                    (None, Some((obj, size))) => self.read_stream_staged(obj, size)?,
                    _ => continue,
                };
                out.push((x.name, value));
            }
            out
        };
        let a = Attrs {
            mode: Some(old.mode & 0o7777),
            uid: Some(old.uid),
            gid: Some(old.gid),
            bsd_flags: Some(old.bsd_flags & !BSD_COMPRESSED),
            ..Default::default()
        };
        let ino = self.replace_with_file(parent, &name, data, &a)?;
        for (k, v) in xattrs {
            self.setxattr_ino(ino, &k, &v)?;
        }
        Ok(ino)
    }

    /// Reads a data stream through the transaction (for xattr streams).
    fn read_stream_staged(&mut self, obj: u64, size: u64) -> Result<Vec<u8>> {
        let bs = self.bs as u64;
        let extents: Vec<(u64, u64, u64)> = self.with_view(|v| {
            let mut start = [0u8; 16];
            put64(&mut start, 0, crate::volume::key_hdr(J_FILE_EXTENT, obj));
            let mut out = Vec::new();
            v.cat.scan_from(v.src, &start, &mut |k, val| {
                if (le64(k, 0) & OBJ_ID_MASK) != obj
                    || (le64(k, 0) >> OBJ_TYPE_SHIFT) as u8 != J_FILE_EXTENT
                {
                    return Ok(false);
                }
                out.push((le64(k, 8), le64(val, 0) & FE_LEN_MASK, le64(val, 8)));
                Ok(true)
            })?;
            Ok(out)
        })?;
        let mut out = vec![0u8; size as usize];
        let txn = self.txn()?;
        for (logical, len, phys) in extents {
            if phys == 0 || logical >= size {
                continue;
            }
            let n = (len.min(size - logical)) as usize;
            let mut buf = vec![0u8; (n as u64).div_ceil(bs) as usize * bs as usize];
            for (i, chunk) in buf.chunks_mut(bs as usize).enumerate() {
                txn.read_block(phys + i as u64, chunk).map_err(|e| werr("reading a stream", e))?;
            }
            out[logical as usize..logical as usize + n].copy_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    pub fn symlink(&mut self, target: &str, path: &str, a: &Attrs) -> Result<u64> {
        let (parent, name) = self.split(path)?;
        let ino = self.run("symlink", |t, v, o| {
            ops::create_symlink(t, v, o, parent, &name, target.as_bytes(), false)
        })?;
        self.apply_attrs(parent, &name, a)?;
        Ok(ino)
    }

    pub fn link(&mut self, existing: &str, new: &str) -> Result<()> {
        let (sp, sn) = self.split(existing)?;
        let (dp, dn) = self.split(new)?;
        self.run("link", |t, v, o| ops::link(t, v, o, sp, &sn, dp, &dn))
    }

    /// Removes one directory entry (a file, symlink or empty directory).
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let (parent, name) = self.split(path)?;
        if self.child(parent, &name)?.is_none() {
            return Err(Error::NotFound(path.to_string()));
        }
        self.run("unlink", |t, v, o| ops::unlink(t, v, o, parent, &name))
    }

    /// Removes a tree, children first.
    pub fn remove_all(&mut self, path: &str) -> Result<()> {
        let ino = self.lookup(path)?;
        if ino.mode & S_IFMT == S_IFDIR {
            let mut names: Vec<String> =
                self.read_dir(ino.ino)?.into_iter().map(|d| d.name).collect();
            names.sort();
            let base = path.trim_end_matches('/').to_string();
            for n in names {
                self.remove_all(&format!("{base}/{n}"))?;
            }
        }
        self.remove(path)?;
        self.commit_if_large()
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        let (op, on) = self.split(old)?;
        let (np, nn) = self.split(new)?;
        if self.child(op, &on)?.is_none() {
            return Err(Error::NotFound(old.to_string()));
        }
        if op == np {
            self.run("rename", |t, v, o| ops::rename(t, v, o, op, &on, &nn, true))
        } else {
            self.run("move", |t, v, o| ops::move_entry(t, v, o, op, &on, np, &nn, true))
        }
    }

    fn setxattr_ino(&mut self, ino: u64, name: &str, value: &[u8]) -> Result<()> {
        self.run("setxattr", |t, v, o| ops::set_xattr(t, v, o, ino, name, value))
    }

    pub fn setxattr(&mut self, path: &str, name: &str, value: &[u8]) -> Result<()> {
        if name == XATTR_DECMPFS || name == XATTR_RSRC || name == XATTR_SYMLINK {
            return Err(Error::Invalid(format!("'{name}' is managed by the filesystem")));
        }
        let ino = self.lookup(path)?.ino;
        self.setxattr_ino(ino, name, value)
    }

    pub fn rmxattr(&mut self, path: &str, name: &str) -> Result<()> {
        let ino = self.lookup(path)?.ino;
        self.run("rmxattr", |t, v, o| ops::delete_xattr(t, v, o, ino, name))
    }
}

impl Drop for MutVolume {
    /// Dropping without [`MutVolume::commit`] discards the staged changes.
    fn drop(&mut self) {
        if let Some(t) = self.txn.take() {
            let _ = t.abort();
        }
    }
}
