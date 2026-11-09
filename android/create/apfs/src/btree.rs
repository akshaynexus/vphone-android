//! B-tree nodes and read-only traversal.

use crate::error::{corrupt, Error, Result};
use crate::raw::*;
use std::cmp::Ordering;

/// Which record type a tree holds; decides key comparison and fixed sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Omap,
    FreeQueue,
    /// Catalog: `hashed` when directory keys carry a name hash.
    Catalog { hashed: bool },
    ExtentRef,
    SnapMeta,
    OmapSnap,
    Fext,
}

/// Key and value sizes for fixed-size trees, given whether the node is a leaf.
pub fn fixed_sizes(kind: KeyKind, leaf: bool) -> Option<(usize, usize)> {
    match kind {
        KeyKind::Omap => Some((16, if leaf { 16 } else { 8 })),
        KeyKind::FreeQueue => Some((16, 8)),
        KeyKind::OmapSnap => Some((8, if leaf { 16 } else { 8 })),
        KeyKind::Fext => Some((16, if leaf { 16 } else { 8 })),
        _ => None,
    }
}

/// Parsed fields of a catalog key needed to order it.
struct CatKey<'a> {
    id: u64,
    ty: u8,
    number: u64,
    name: Option<&'a [u8]>,
}

fn cat_key(k: &[u8], hashed: bool) -> CatKey<'_> {
    if k.len() < 8 {
        return CatKey { id: 0, ty: 0, number: 0, name: None };
    }
    let hdr = le64(k, 0);
    let id = hdr & OBJ_ID_MASK;
    let ty = (hdr >> OBJ_TYPE_SHIFT) as u8;
    let mut number = 0u64;
    let mut name = None;
    match ty {
        J_DIR_REC => {
            if hashed && k.len() >= 12 {
                number = (le32(k, 8) & !DREC_LEN_MASK) as u64;
                name = Some(cstr(&k[12..]));
            } else if !hashed && k.len() >= 10 {
                name = Some(cstr(&k[10..]));
            }
        }
        J_XATTR if k.len() >= 10 => name = Some(cstr(&k[10..])),
        J_FILE_EXTENT | J_SIBLING_LINK | J_FILE_INFO if k.len() >= 16 => {
            number = le64(k, 8);
        }
        _ => {}
    }
    CatKey { id, ty, number, name }
}

fn cstr(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(n) => &b[..n],
        None => b,
    }
}

fn u64pair(k: &[u8]) -> (u64, u64) {
    (
        if k.len() >= 8 { le64(k, 0) } else { 0 },
        if k.len() >= 16 { le64(k, 8) } else { 0 },
    )
}

/// Orders two raw keys in a tree of the given kind.
pub fn compare(kind: KeyKind, a: &[u8], b: &[u8]) -> Ordering {
    match kind {
        KeyKind::Omap | KeyKind::FreeQueue | KeyKind::Fext => u64pair(a).cmp(&u64pair(b)),
        KeyKind::OmapSnap => u64pair(a).0.cmp(&u64pair(b).0),
        KeyKind::ExtentRef => (le64(a, 0) & OBJ_ID_MASK).cmp(&(le64(b, 0) & OBJ_ID_MASK)),
        KeyKind::SnapMeta => {
            let (x, y) = (cat_key(a, false), cat_key(b, false));
            (x.id, x.ty).cmp(&(y.id, y.ty)).then_with(|| x.name.cmp(&y.name))
        }
        KeyKind::Catalog { hashed } => {
            let (x, y) = (cat_key(a, hashed), cat_key(b, hashed));
            (x.id, x.ty, x.number).cmp(&(y.id, y.ty, y.number)).then_with(|| x.name.cmp(&y.name))
        }
    }
}

/// A node as read from disk.
#[derive(Clone)]
pub struct Node {
    raw: Vec<u8>,
    pub flags: u16,
    pub level: u16,
    pub nkeys: u32,
    table_off: usize,
    key_base: usize,
    val_end: usize,
    /// Fixed key/value size of the tree, if any (a free-queue value may also be absent).
    fixed: Option<(usize, usize)>,
}

impl Node {
    pub fn parse(raw: Vec<u8>, kind: KeyKind) -> Result<Node> {
        if raw.len() < BTN_DATA {
            return corrupt("B-tree node is too short");
        }
        let flags = le16(&raw, BTN_FLAGS);
        let level = le16(&raw, BTN_LEVEL);
        let nkeys = le32(&raw, BTN_NKEYS);
        let table_off = BTN_DATA + le16(&raw, BTN_TABLE) as usize;
        let table_len = le16(&raw, BTN_TABLE + 2) as usize;
        let key_base = table_off + table_len;
        if key_base > raw.len() {
            return corrupt("B-tree node table of contents is out of range");
        }
        let leaf = flags & BTNODE_LEAF != 0;
        if (flags & BTNODE_ROOT != 0) && raw.len() < BTN_DATA + BT_INFO_SIZE {
            return corrupt("B-tree root node is too short");
        }
        let val_end = raw.len() - if flags & BTNODE_ROOT != 0 { BT_INFO_SIZE } else { 0 };
        let fixed = if flags & BTNODE_FIXED != 0 {
            match fixed_sizes(kind, leaf) {
                Some(x) => Some(x),
                None => return corrupt("B-tree node is unexpectedly flagged fixed-size"),
            }
        } else {
            None
        };
        let entry = if fixed.is_some() { 4 } else { 8 };
        if table_off + nkeys as usize * entry > raw.len() {
            return corrupt("B-tree node has too many records");
        }
        Ok(Node { raw, flags, level, nkeys, table_off, key_base, val_end, fixed })
    }

    pub fn is_leaf(&self) -> bool {
        self.flags & BTNODE_LEAF != 0
    }

    pub fn len(&self) -> usize {
        self.nkeys as usize
    }

    pub fn is_empty(&self) -> bool {
        self.nkeys == 0
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn key(&self, i: usize) -> Result<&[u8]> {
        let (off, len) = if self.fixed.is_some() {
            (le16(&self.raw, self.table_off + i * 4) as usize, self.fixed.unwrap().0)
        } else {
            let e = self.table_off + i * 8;
            (le16(&self.raw, e) as usize, le16(&self.raw, e + 2) as usize)
        };
        self.slice(self.key_base + off, len)
    }

    pub fn val(&self, i: usize) -> Result<&[u8]> {
        let (off, len) = if let Some((_, v)) = self.fixed {
            let off = le16(&self.raw, self.table_off + i * 4 + 2);
            if off == BTOFF_INVALID {
                return Ok(&[]);
            }
            (off as usize, v)
        } else {
            let e = self.table_off + i * 8;
            (le16(&self.raw, e + 4) as usize, le16(&self.raw, e + 6) as usize)
        };
        if off > self.val_end {
            return corrupt("B-tree value offset is out of range");
        }
        self.slice(self.val_end - off, len)
    }

    fn slice(&self, off: usize, len: usize) -> Result<&[u8]> {
        self.raw
            .get(off..off.checked_add(len).ok_or_else(|| Error::Corrupt("record overflow".into()))?)
            .ok_or_else(|| Error::Corrupt("B-tree record is out of range".into()))
    }

    /// Index of the last record whose key is `<= key`, or None.
    pub fn floor(&self, kind: KeyKind, key: &[u8]) -> Result<Option<usize>> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if compare(kind, self.key(mid)?, key) == Ordering::Greater {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok(if lo == 0 { None } else { Some(lo - 1) })
    }
}

/// What a traversal needs from the container: fetching a node.
pub trait NodeSource {
    fn node(&self, tree: &Tree, oid: u64) -> Result<std::rc::Rc<Node>>;
}

/// A tree: how to read its nodes and order its keys.
#[derive(Clone, Copy, Debug)]
pub struct Tree {
    pub kind: KeyKind,
    pub root: u64,
    /// Physical trees use `root` and child values as block numbers. Virtual
    /// trees resolve them through the omap at (omap root, xid).
    pub virt: Option<(u64, u64)>,
}

impl Tree {
    /// A sealed catalog stores child pointers as offsets from the root's oid.
    pub fn child_oid(&self, node: &Node, i: usize) -> Result<u64> {
        let v = node.val(i)?;
        if v.len() < 8 {
            return corrupt("B-tree child value is too short");
        }
        let id = le64(v, 0);
        if node.flags & BTNODE_HASHED != 0 && !node.is_leaf() {
            Ok(id.wrapping_add(self.root))
        } else {
            Ok(id)
        }
    }

    /// Exact lookup.
    pub fn find<S: NodeSource + ?Sized>(&self, src: &S, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.find_le(src, key)?.filter(|(k, _)| compare(self.kind, k, key) == Ordering::Equal).map(|(_, v)| v))
    }

    /// The last record with key `<= key`.
    pub fn find_le<S: NodeSource + ?Sized>(
        &self,
        src: &S,
        key: &[u8],
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut node = src.node(self, self.root)?;
        let mut depth = 0;
        loop {
            let i = match node.floor(self.kind, key)? {
                Some(i) => i,
                None => return Ok(None),
            };
            if node.is_leaf() {
                return Ok(Some((node.key(i)?.to_vec(), node.val(i)?.to_vec())));
            }
            let child = self.child_oid(&node, i)?;
            node = src.node(self, child)?;
            depth += 1;
            if depth > 32 {
                return corrupt("B-tree is too deep");
            }
        }
    }

    /// Visits records in key order starting from the first record `>= start`,
    /// until the callback returns false.
    pub fn scan_from<S, F>(&self, src: &S, start: &[u8], f: &mut F) -> Result<()>
    where
        S: NodeSource + ?Sized,
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        let root = src.node(self, self.root)?;
        self.scan_node(src, &root, Some(start), f, 0)?;
        Ok(())
    }

    /// Visits every record.
    pub fn scan_all<S, F>(&self, src: &S, f: &mut F) -> Result<()>
    where
        S: NodeSource + ?Sized,
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        let root = src.node(self, self.root)?;
        self.scan_node(src, &root, None, f, 0)?;
        Ok(())
    }

    fn scan_node<S, F>(
        &self,
        src: &S,
        node: &Node,
        start: Option<&[u8]>,
        f: &mut F,
        depth: u32,
    ) -> Result<bool>
    where
        S: NodeSource + ?Sized,
        F: FnMut(&[u8], &[u8]) -> Result<bool>,
    {
        if depth > 32 {
            return corrupt("B-tree is too deep");
        }
        let first = match start {
            Some(k) => node.floor(self.kind, k)?.unwrap_or(0),
            None => 0,
        };
        for i in first..node.len() {
            if node.is_leaf() {
                if let Some(k) = start {
                    if compare(self.kind, node.key(i)?, k) == Ordering::Less {
                        continue;
                    }
                }
                if !f(node.key(i)?, node.val(i)?)? {
                    return Ok(false);
                }
            } else {
                let child = src.node(self, self.child_oid(node, i)?)?;
                let s = if i == first { start } else { None };
                if !self.scan_node(src, &child, s, f, depth + 1)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}
