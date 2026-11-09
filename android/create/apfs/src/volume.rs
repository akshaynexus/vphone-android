//! A volume: filesystem trees, inodes, directories, files, xattrs, snapshots.

use crate::btree::{KeyKind, NodeSource, Tree};
use crate::container::{Container, VolumeSummary};
use crate::decmpfs;
use crate::error::{corrupt, Error, Result};
use crate::names;
use crate::raw::*;

/// What a directory entry or inode is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Char,
    Block,
    Fifo,
    Socket,
    Other,
}

impl FileKind {
    pub fn from_mode(mode: u16) -> FileKind {
        match mode & S_IFMT {
            S_IFREG => FileKind::File,
            S_IFDIR => FileKind::Dir,
            S_IFLNK => FileKind::Symlink,
            S_IFCHR => FileKind::Char,
            S_IFBLK => FileKind::Block,
            S_IFIFO => FileKind::Fifo,
            S_IFSOCK => FileKind::Socket,
            _ => FileKind::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Dir => "dir",
            FileKind::Symlink => "symlink",
            FileKind::Char => "char",
            FileKind::Block => "block",
            FileKind::Fifo => "fifo",
            FileKind::Socket => "socket",
            FileKind::Other => "other",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub ino: u64,
    pub dtype: u16,
    pub sibling_id: Option<u64>,
    pub date_added: u64,
}

/// An inode record, decoded.
#[derive(Clone, Debug)]
pub struct Inode {
    pub ino: u64,
    pub parent: u64,
    pub private_id: u64,
    pub create_time: u64,
    pub mod_time: u64,
    pub change_time: u64,
    pub access_time: u64,
    pub internal_flags: u64,
    pub nchildren_or_nlink: u32,
    pub bsd_flags: u32,
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
    pub uncompressed_size: u64,
    pub name: Option<String>,
    pub size: u64,
    pub alloced: u64,
    pub sparse_bytes: u64,
    pub has_dstream: bool,
    pub rdev: Option<u32>,
}

impl Inode {
    pub fn kind(&self) -> FileKind {
        FileKind::from_mode(self.mode)
    }

    pub fn is_compressed(&self) -> bool {
        self.bsd_flags & BSD_COMPRESSED != 0
    }

    pub fn nlink(&self) -> u32 {
        if self.kind() == FileKind::Dir {
            1
        } else {
            self.nchildren_or_nlink
        }
    }
}

/// An extended attribute's location.
#[derive(Clone, Debug)]
pub struct Xattr {
    pub name: String,
    pub flags: u16,
    /// Embedded data, if any.
    pub data: Option<Vec<u8>>,
    /// (object id, size) for a stream.
    pub stream: Option<(u64, u64)>,
}

/// A file extent: `len` bytes at `logical`; `phys == 0` is a hole.
#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub logical: u64,
    pub len: u64,
    pub phys: u64,
    pub crypto: u64,
}

pub struct Volume<'c> {
    pub c: &'c Container,
    pub info: VolumeSummary,
    pub sb: Vec<u8>,
    pub xid: u64,
    pub omap_root: u64,
    pub cat: Tree,
    pub fext: Option<Tree>,
    pub case_insensitive: bool,
    pub norm_insensitive: bool,
    pub snapshot: Option<String>,
}

pub(crate) fn check_incompat(incompat: u64, rocompat: u64) -> Result<()> {
    let unknown = incompat & !INCOMPAT_SUPPORTED;
    if unknown != 0 {
        return Err(Error::Unsupported(format!("volume incompatible features {unknown:#x}")));
    }
    // Read-only-compatible features do not stop reads, whatever they are;
    // writes are checked separately.
    let _ = rocompat;
    Ok(())
}

impl<'c> Volume<'c> {
    pub(crate) fn open(
        c: &'c Container,
        s: &VolumeSummary,
        snapshot: Option<&str>,
    ) -> Result<Volume<'c>> {
        let bs = c.block_size as usize;
        let mut sb = vec![0u8; bs];
        c.read_block(s.paddr, &mut sb)?;
        check_incompat(s.incompat, s.rocompat)?;
        if s.encrypted {
            return Err(Error::Unsupported(format!(
                "volume '{}' is encrypted: this implementation has no key material",
                s.name
            )));
        }
        let incompat = s.incompat;
        let hashed = incompat & (INCOMPAT_CASE_INSENSITIVE | INCOMPAT_NORM_INSENSITIVE) != 0;
        let case_insensitive = incompat & INCOMPAT_CASE_INSENSITIVE != 0;
        let norm_insensitive = hashed;
        let kind = KeyKind::Catalog { hashed };

        // The volume's own object map.
        let mut om = vec![0u8; bs];
        c.read_block(le64(&sb, FS_OMAP_OID), &mut om)?;
        if !verify_checksum(&om) {
            return corrupt("bad checksum on volume object map");
        }
        let omap_root = le64(&om, OM_TREE_OID);

        let mut xid = le64(&sb, 0x10);
        let mut root_oid = le64(&sb, FS_ROOT_TREE_OID);
        let mut snap_name = None;
        let mut snapshots_of = sb.clone();
        if let Some(name) = snapshot {
            let (snap_xid, meta) = lookup_snapshot(c, &sb, name)?;
            // The snapshot keeps its own frozen volume superblock, which
            // names its catalog root; virtual objects are resolved in the
            // current object map as of the snapshot's transaction.
            let sblock_bno = le64(&meta, 8);
            let mut snap_sb = vec![0u8; bs];
            c.read_block(sblock_bno, &mut snap_sb)?;
            if !verify_checksum(&snap_sb) || &snap_sb[32..36] != FS_MAGIC {
                return corrupt(format!("snapshot '{name}' has an invalid superblock"));
            }
            root_oid = le64(&snap_sb, FS_ROOT_TREE_OID);
            xid = snap_xid;
            snapshots_of = snap_sb;
            snap_name = Some(name.to_string());
        }

        let cat = Tree { kind, root: root_oid, virt: Some((omap_root, xid)) };
        let sealed = incompat & INCOMPAT_SEALED != 0;
        let fext = if sealed {
            Some(Tree { kind: KeyKind::Fext, root: le64(&snapshots_of, FS_FEXT_OID), virt: None })
        } else {
            None
        };
        let mut info = s.clone();
        info.snapshots = list_snapshots(c, &sb)?;
        Ok(Volume {
            c,
            info,
            sb,
            xid,
            omap_root,
            cat,
            fext,
            case_insensitive,
            norm_insensitive,
            snapshot: snap_name,
        })
    }

    /// The catalog-query half of this volume, shared with the write path.
    pub(crate) fn view(&self) -> CatView<'_, Container> {
        CatView {
            src: self.c,
            cat: self.cat,
            case_insensitive: self.case_insensitive,
            norm_insensitive: self.norm_insensitive,
        }
    }

    pub fn inode(&self, ino: u64) -> Result<Inode> {
        self.view().inode(ino)
    }

    /// Resolves an absolute path without following a final symlink.
    pub fn lookup(&self, path: &str) -> Result<Inode> {
        self.view().lookup(path)
    }

    pub fn lookup_child(&self, parent: u64, name: &str) -> Result<Option<DirEntry>> {
        self.view().lookup_child(parent, name)
    }

    pub fn read_dir(&self, ino: u64) -> Result<Vec<DirEntry>> {
        self.view().read_dir(ino)
    }

    pub fn xattrs(&self, ino: u64) -> Result<Vec<Xattr>> {
        self.view().xattrs(ino)
    }

    pub fn getxattr(&self, ino: u64, name: &str) -> Result<Option<Vec<u8>>> {
        match self.view().xattr(ino, name)? {
            Some(x) => Ok(Some(self.xattr_value(&x)?)),
            None => Ok(None),
        }
    }

    pub fn xattr_value(&self, x: &Xattr) -> Result<Vec<u8>> {
        if let Some(d) = &x.data {
            return Ok(d.clone());
        }
        let (obj, size) = x.stream.ok_or_else(|| Error::Corrupt("xattr with no data".into()))?;
        let mut out = Vec::with_capacity(size as usize);
        self.read_stream(obj, size, &mut out)?;
        Ok(out)
    }

    /// The extents of a data stream, in order.
    pub fn extents(&self, private_id: u64) -> Result<Vec<Extent>> {
        let mut out = Vec::new();
        if let Some(fext) = &self.fext {
            let mut start = [0u8; 16];
            put64(&mut start, 0, private_id);
            fext.scan_from(self.c, &start, &mut |k, v| {
                if le64(k, 0) != private_id {
                    return Ok(false);
                }
                out.push(Extent {
                    logical: le64(k, 8),
                    len: le64(v, 0) & FE_LEN_MASK,
                    phys: le64(v, 8),
                    crypto: 0,
                });
                Ok(true)
            })?;
        } else {
            let mut start = [0u8; 16];
            put64(&mut start, 0, key_hdr(J_FILE_EXTENT, private_id));
            self.cat.scan_from(self.c, &start, &mut |k, v| {
                if !is_type_of(k, private_id, J_FILE_EXTENT) {
                    return Ok(false);
                }
                if v.len() < 24 {
                    return corrupt("short file extent record");
                }
                out.push(Extent {
                    logical: le64(k, 8),
                    len: le64(v, 0) & FE_LEN_MASK,
                    phys: le64(v, 8),
                    crypto: le64(v, 16),
                });
                Ok(true)
            })?;
        }
        Ok(out)
    }

    /// Streams `size` bytes of a data stream (holes read as zeros).
    pub fn read_stream<W: std::io::Write>(&self, private_id: u64, size: u64, w: &mut W) -> Result<()> {
        let bs = self.c.block_size as u64;
        let mut pos = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        for e in self.extents(private_id)? {
            if pos >= size {
                break;
            }
            if e.logical > pos {
                zero_fill(w, (e.logical - pos).min(size - pos))?;
                pos = e.logical;
            }
            if e.logical + e.len <= pos || pos >= size {
                continue;
            }
            if e.crypto != 0 && e.phys != 0 {
                return Err(Error::Unsupported("encrypted file extent".into()));
            }
            let mut off = pos - e.logical;
            let remain = (e.len - off).min(size - pos);
            let end = off + remain;
            while off < end {
                let n = ((end - off) as usize).min(buf.len());
                if e.phys == 0 {
                    buf[..n].fill(0);
                } else {
                    self.c.img.read_at(&mut buf[..n], e.phys * bs + off)?;
                }
                w.write_all(&buf[..n])?;
                off += n as u64;
            }
            pos += remain;
        }
        if pos < size {
            zero_fill(w, size - pos)?;
        }
        Ok(())
    }

    /// Full contents of a regular file, decompressed.
    pub fn read_file(&self, ino: &Inode) -> Result<Vec<u8>> {
        if ino.kind() != FileKind::File {
            return Err(Error::Invalid(format!("inode {:#x} is not a regular file", ino.ino)));
        }
        let mut out = Vec::new();
        self.write_file_to(ino, &mut out)?;
        Ok(out)
    }

    /// Writes a file's contents to `w`, decompressing as needed.
    pub fn write_file_to<W: std::io::Write>(&self, ino: &Inode, w: &mut W) -> Result<()> {
        if ino.is_compressed() {
            if let Some(attr) = self.getxattr(ino.ino, XATTR_DECMPFS)? {
                let hdr = decmpfs::parse_header(&attr)?;
                let rsrc = if hdr.algo & 1 == 0 && hdr.algo != 1 {
                    Some(
                        self.getxattr(ino.ino, XATTR_RSRC)?
                            .ok_or_else(|| Error::Corrupt("compressed file has no resource fork".into()))?,
                    )
                } else {
                    None
                };
                let data = decmpfs::decompress(&attr, rsrc.as_deref())?;
                w.write_all(&data)?;
                return Ok(());
            }
            // Compressed flag without a header: the plain stream is the data.
        }
        self.read_stream(ino.private_id, ino.size, w)
    }

    pub fn read_symlink(&self, ino: &Inode) -> Result<String> {
        let v = self
            .getxattr(ino.ino, XATTR_SYMLINK)?
            .ok_or_else(|| Error::Corrupt(format!("symlink {:#x} has no target", ino.ino)))?;
        Ok(String::from_utf8_lossy(strip_nul(&v)).into_owned())
    }

    /// Uncompressed length of a file as reported by the filesystem.
    pub fn file_size(&self, ino: &Inode) -> Result<u64> {
        if ino.is_compressed() {
            if let Some(attr) = self.getxattr(ino.ino, XATTR_DECMPFS)? {
                return Ok(decmpfs::parse_header(&attr)?.size);
            }
        }
        Ok(ino.size)
    }
}

fn zero_fill<W: std::io::Write>(w: &mut W, mut n: u64) -> Result<()> {
    let zeros = [0u8; 4096];
    while n > 0 {
        let k = (n as usize).min(zeros.len());
        w.write_all(&zeros[..k])?;
        n -= k as u64;
    }
    Ok(())
}

fn strip_nul(v: &[u8]) -> &[u8] {
    if v.last() == Some(&0) {
        &v[..v.len() - 1]
    } else {
        v
    }
}

// --- Key construction. Shared with the write path. ---

pub(crate) fn key_hdr(ty: u8, id: u64) -> u64 {
    ((ty as u64) << OBJ_TYPE_SHIFT) | (id & OBJ_ID_MASK)
}

pub(crate) fn inode_key(ino: u64) -> Vec<u8> {
    key_hdr(J_INODE, ino).to_le_bytes().to_vec()
}

pub(crate) fn xattr_key(ino: u64, name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(11 + name.len());
    k.extend_from_slice(&key_hdr(J_XATTR, ino).to_le_bytes());
    k.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes());
    k.extend_from_slice(name.as_bytes());
    k.push(0);
    k
}

pub(crate) fn drec_key(parent: u64, name: &str, hashed: bool, case_fold: bool) -> Vec<u8> {
    let mut k = Vec::with_capacity(14 + name.len());
    k.extend_from_slice(&key_hdr(J_DIR_REC, parent).to_le_bytes());
    if hashed {
        k.extend_from_slice(&names::drec_hash(name, case_fold).to_le_bytes());
    } else {
        k.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes());
    }
    k.extend_from_slice(name.as_bytes());
    k.push(0);
    k
}

/// A key that sorts at or before every entry with this key's hash.
fn key_for_scan(k: &[u8]) -> Vec<u8> {
    let mut s = k[..12].to_vec();
    s[8] &= 0;
    s[9] &= 0xfc;
    s.push(0);
    s
}

fn key_hash(k: &[u8]) -> u32 {
    le32(k, 8) & !DREC_LEN_MASK
}

fn key_hash_of(k: &[u8]) -> u32 {
    if k.len() < 12 {
        return u32::MAX;
    }
    le32(k, 8) & !DREC_LEN_MASK
}

/// Lower bound for all directory records of a directory.
pub(crate) fn drec_start(ino: u64) -> Vec<u8> {
    let mut k = vec![0u8; 8];
    put64(&mut k, 0, key_hdr(J_DIR_REC, ino));
    k
}

fn key_id_ty(k: &[u8]) -> (u64, u8) {
    if k.len() < 8 {
        return (0, 0);
    }
    let h = le64(k, 0);
    (h & OBJ_ID_MASK, (h >> OBJ_TYPE_SHIFT) as u8)
}

fn is_drec_of(k: &[u8], ino: u64) -> bool {
    key_id_ty(k) == (ino, J_DIR_REC)
}

fn is_type_of(k: &[u8], id: u64, ty: u8) -> bool {
    key_id_ty(k) == (id, ty)
}

pub(crate) fn decode_drec(k: &[u8], v: &[u8], hashed: bool) -> Result<DirEntry> {
    let name_off = if hashed { 12 } else { 10 };
    if k.len() <= name_off || v.len() < 18 {
        return corrupt("short directory record");
    }
    let name = String::from_utf8_lossy(strip_nul(&k[name_off..])).into_owned();
    let mut sibling_id = None;
    if v.len() > 18 {
        for (ty, data) in xfields(&v[18..]) {
            if ty == X_SIBLING_ID && data.len() >= 8 {
                sibling_id = Some(le64(data, 0));
            }
        }
    }
    Ok(DirEntry { name, ino: le64(v, 0), date_added: le64(v, 8), dtype: le16(v, 16), sibling_id })
}

pub(crate) fn decode_inode(ino: u64, v: &[u8]) -> Result<Inode> {
    if v.len() < INO_XFIELDS {
        return corrupt("short inode record");
    }
    let mut inode = Inode {
        ino,
        parent: le64(v, INO_PARENT),
        private_id: le64(v, INO_PRIVATE),
        create_time: le64(v, INO_CREATE),
        mod_time: le64(v, INO_MOD),
        change_time: le64(v, INO_CHANGE),
        access_time: le64(v, INO_ACCESS),
        internal_flags: le64(v, INO_FLAGS),
        nchildren_or_nlink: le32(v, INO_NCHILDREN),
        bsd_flags: le32(v, INO_BSD_FLAGS),
        uid: le32(v, INO_UID),
        gid: le32(v, INO_GID),
        mode: le16(v, INO_MODE),
        uncompressed_size: le64(v, INO_UNCOMP),
        name: None,
        size: 0,
        alloced: 0,
        sparse_bytes: 0,
        has_dstream: false,
        rdev: None,
    };
    for (ty, data) in xfields(&v[INO_XFIELDS..]) {
        match ty {
            X_NAME => inode.name = Some(String::from_utf8_lossy(strip_nul(data)).into_owned()),
            X_DSTREAM if data.len() >= 16 => {
                inode.size = le64(data, 0);
                inode.alloced = le64(data, 8);
                inode.has_dstream = true;
            }
            X_SPARSE if data.len() >= 8 => inode.sparse_bytes = le64(data, 0),
            X_RDEV if data.len() >= 4 => inode.rdev = Some(le32(data, 0)),
            _ => {}
        }
    }
    Ok(inode)
}

pub(crate) fn decode_xattr(k: &[u8], v: &[u8]) -> Result<Xattr> {
    if k.len() <= 10 || v.len() < 4 {
        return corrupt("short xattr record");
    }
    let name = String::from_utf8_lossy(strip_nul(&k[10..])).into_owned();
    let flags = le16(v, 0);
    let dlen = le16(v, 2) as usize;
    if flags & XATTR_EMBEDDED != 0 {
        let data = v
            .get(4..4 + dlen)
            .ok_or_else(|| Error::Corrupt("xattr value is out of range".into()))?
            .to_vec();
        Ok(Xattr { name, flags, data: Some(data), stream: None })
    } else if flags & XATTR_STREAM != 0 {
        if v.len() < 4 + 48 {
            return corrupt("short xattr stream record");
        }
        Ok(Xattr { name, flags, data: None, stream: Some((le64(v, 4), le64(v, 12))) })
    } else {
        corrupt("xattr is neither embedded nor a stream")
    }
}

/// Decodes an xfield collection into (type, value) pairs.
pub(crate) fn xfields(blob: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    if blob.len() < 4 {
        return out;
    }
    let n = le16(blob, 0) as usize;
    let meta_end = 4 + n * 4;
    if meta_end > blob.len() {
        return out;
    }
    let mut off = meta_end;
    for i in 0..n {
        let ty = blob[4 + i * 4];
        let size = le16(blob, 4 + i * 4 + 2) as usize;
        if off + size > blob.len() {
            break;
        }
        out.push((ty, &blob[off..off + size]));
        off += round_up(size, 8);
    }
    out
}

fn snap_name_key(name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(11 + name.len());
    k.extend_from_slice(&key_hdr(11, OBJ_ID_MASK).to_le_bytes());
    k.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes());
    k.extend_from_slice(name.as_bytes());
    k.push(0);
    k
}

fn lookup_snapshot(c: &Container, sb: &[u8], name: &str) -> Result<(u64, Vec<u8>)> {
    let root = le64(sb, FS_SNAPMETA_OID);
    if root == 0 {
        return Err(Error::NotFound("the volume has no snapshots".into()));
    }
    let tree = Tree { kind: KeyKind::SnapMeta, root, virt: None };
    let key = snap_name_key(name);
    let v = tree
        .find(c, &key)?
        .ok_or_else(|| Error::NotFound(format!("snapshot '{name}'")))?;
    let xid = le64(&v, 0);
    let meta_key = key_hdr(1, xid).to_le_bytes();
    let meta = tree
        .find(c, &meta_key)?
        .ok_or_else(|| Error::Corrupt(format!("snapshot '{name}' has no metadata")))?;
    if meta.len() < 50 {
        return corrupt("short snapshot metadata");
    }
    Ok((xid, meta))
}

fn list_snapshots(c: &Container, sb: &[u8]) -> Result<Vec<String>> {
    let root = le64(sb, FS_SNAPMETA_OID);
    let mut names = Vec::new();
    if root == 0 {
        return Ok(names);
    }
    let tree = Tree { kind: KeyKind::SnapMeta, root, virt: None };
    // Name records all carry the placeholder id, so they sort after every
    // metadata record (whose id is the snapshot's xid).
    let start = key_hdr(11, OBJ_ID_MASK).to_le_bytes();
    tree.scan_from(c, &start, &mut |k, _| {
        if (le64(k, 0) >> OBJ_TYPE_SHIFT) as u8 != 11 {
            return Ok(false);
        }
        if k.len() > 10 {
            names.push(String::from_utf8_lossy(strip_nul(&k[10..])).into_owned());
        }
        Ok(true)
    })?;
    Ok(names)
}

/// Iterates a directory tree depth-first, calling `f` with the path below the
/// root (no leading slash; "" for the root itself).
pub fn walk<F>(vol: &Volume<'_>, root: &Inode, f: &mut F) -> Result<()>
where
    F: FnMut(&str, &Inode) -> Result<()>,
{
    let mut stack: Vec<(String, u64)> = vec![(String::new(), root.ino)];
    f("", root)?;
    while let Some((prefix, ino)) = stack.pop() {
        let mut entries = vol.read_dir(ino)?;
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        for de in entries.into_iter().rev() {
            let p = if prefix.is_empty() { de.name.clone() } else { format!("{prefix}/{}", de.name) };
            let child = vol.inode(de.ino)?;
            f(&p, &child)?;
            if child.kind() == FileKind::Dir {
                stack.push((p, child.ino));
            }
        }
    }
    Ok(())
}

/// Catalog queries over any node source: the on-disk container, or the
/// staged state of an open write transaction.
pub(crate) struct CatView<'a, S: NodeSource + ?Sized> {
    pub src: &'a S,
    pub cat: Tree,
    pub case_insensitive: bool,
    pub norm_insensitive: bool,
}

impl<S: NodeSource + ?Sized> CatView<'_, S> {
    pub fn inode(&self, ino: u64) -> Result<Inode> {
        let key = inode_key(ino);
        let val = self
            .cat
            .find(self.src, &key)?
            .ok_or_else(|| Error::NotFound(format!("inode {ino:#x}")))?;
        decode_inode(ino, &val)
    }

    /// Empty components and `.` are ignored; `..` is a plain name.
    pub fn lookup(&self, path: &str) -> Result<Inode> {
        let mut cur = ROOT_DIR_INO;
        for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            let de = self.lookup_child(cur, comp)?.ok_or_else(|| {
                Error::NotFound(format!("{path}: no such file or directory ({comp})"))
            })?;
            cur = de.ino;
        }
        self.inode(cur)
    }

    pub fn lookup_child(&self, parent: u64, name: &str) -> Result<Option<DirEntry>> {
        let key = drec_key(parent, name, self.norm_insensitive, self.case_insensitive);
        if self.norm_insensitive {
            // Same 22-bit hash means the same bucket; compare normalized names.
            let want_hash = key_hash(&key);
            let mut found = None;
            self.cat.scan_from(self.src, &key_for_scan(&key), &mut |k, v| {
                if !is_drec_of(k, parent) || key_hash_of(k) != want_hash {
                    return Ok(false);
                }
                let de = decode_drec(k, v, true)?;
                if names::names_equal(&de.name, name, self.case_insensitive, true) {
                    found = Some(de);
                    return Ok(false);
                }
                Ok(true)
            })?;
            if found.is_some() {
                return Ok(found);
            }
            // Only a Unicode-revision difference in the normalization can
            // miss the bucket; a linear scan still finds the entry.
            let mut hit = None;
            for de in self.read_dir(parent)? {
                if names::names_equal(&de.name, name, self.case_insensitive, true) {
                    hit = Some(de);
                    break;
                }
            }
            Ok(hit)
        } else {
            match self.cat.find(self.src, &key)? {
                Some(v) => Ok(Some(decode_drec(&key, &v, false)?)),
                None => Ok(None),
            }
        }
    }

    pub fn read_dir(&self, ino: u64) -> Result<Vec<DirEntry>> {
        let start = drec_start(ino);
        let mut out = Vec::new();
        let hashed = self.norm_insensitive;
        self.cat.scan_from(self.src, &start, &mut |k, v| {
            if !is_drec_of(k, ino) {
                return Ok(false);
            }
            out.push(decode_drec(k, v, hashed)?);
            Ok(true)
        })?;
        Ok(out)
    }

    pub fn xattrs(&self, ino: u64) -> Result<Vec<Xattr>> {
        let mut start = vec![0u8; 8];
        put64(&mut start, 0, key_hdr(J_XATTR, ino));
        let mut out = Vec::new();
        self.cat.scan_from(self.src, &start, &mut |k, v| {
            if !is_type_of(k, ino, J_XATTR) {
                return Ok(false);
            }
            out.push(decode_xattr(k, v)?);
            Ok(true)
        })?;
        Ok(out)
    }

    pub fn xattr(&self, ino: u64, name: &str) -> Result<Option<Xattr>> {
        let key = xattr_key(ino, name);
        Ok(match self.cat.find(self.src, &key)? {
            Some(v) => Some(decode_xattr(&key, &v)?),
            None => None,
        })
    }
}
