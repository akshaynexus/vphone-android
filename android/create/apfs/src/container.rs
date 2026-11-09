//! The container: checkpoint selection, ephemeral objects, object maps and
//! the list of volumes.

use crate::btree::{KeyKind, Node, NodeSource, Tree};
use crate::error::{corrupt, Error, Result};
use crate::image::Image;
use crate::raw::*;
use crate::volume::Volume;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// One checkpoint-mapping entry: where an ephemeral object lives right now.
#[derive(Clone, Copy, Debug)]
pub struct CheckpointMapping {
    pub otype: u32,
    pub subtype: u32,
    pub size: u32,
    pub fs_oid: u64,
    pub oid: u64,
    pub paddr: u64,
}

/// Which volume to open.
#[derive(Clone, Debug, Default)]
pub struct VolumeSelector {
    pub index: Option<usize>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub snapshot: Option<String>,
}

/// One volume in the container's list.
#[derive(Clone, Debug)]
pub struct VolumeSummary {
    pub index: usize,
    pub oid: u64,
    pub paddr: u64,
    pub name: String,
    pub uuid: [u8; 16],
    pub role: u16,
    pub role_name: String,
    pub sealed: bool,
    pub encrypted: bool,
    pub incompat: u64,
    pub rocompat: u64,
    pub fs_flags: u64,
    pub snapshots: Vec<String>,
}

pub struct Container {
    pub img: Image,
    pub block_size: u32,
    /// Live container superblock (a full block).
    pub sb: Vec<u8>,
    pub xid: u64,
    pub block_count: u64,
    pub maps: Vec<CheckpointMapping>,
    pub omap_root: u64,
    cache: RefCell<HashMap<u64, Rc<Node>>>,
}

fn role_name(role: u16) -> String {
    const TOP: [(u16, &str); 6] = [
        (1, "system"),
        (2, "user"),
        (4, "recovery"),
        (8, "vm"),
        (0x10, "preboot"),
        (0x20, "installer"),
    ];
    let mut names: Vec<String> =
        TOP.iter().filter(|(b, _)| role & b != 0).map(|(_, n)| n.to_string()).collect();
    let en = role >> 6;
    let enum_name = match en {
        1 => Some("data"),
        2 => Some("baseband"),
        3 => Some("update"),
        4 => Some("xart"),
        5 => Some("hardware"),
        6 => Some("backup"),
        9 => Some("enterprise"),
        11 => Some("prelogin"),
        _ => None,
    };
    if let Some(n) = enum_name {
        names.push(n.to_string());
    } else if en != 0 {
        names.push(format!("role{:#x}", role));
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(",")
    }
}

impl Container {
    pub fn open(img: Image) -> Result<Container> {
        let mut first = vec![0u8; 4096];
        img.read_at(&mut first, 0)?;
        if &first[32..36] != NX_MAGIC {
            return corrupt("no NXSB magic in block zero");
        }
        let block_size = le32(&first, NX_BLOCK_SIZE);
        if !(4096..=65536).contains(&block_size) || !block_size.is_power_of_two() {
            return corrupt(format!("invalid block size {block_size}"));
        }
        first.resize(block_size as usize, 0);
        img.read_at(&mut first, 0)?;

        // The newest usable checkpoint is the newest container superblock in
        // the descriptor ring whose mapped ephemeral objects all check out.
        let desc_blocks = le32(&first, NX_XP_DESC_BLOCKS);
        let desc_base = le64(&first, NX_XP_DESC_BASE);
        if desc_blocks >> 31 != 0 || desc_base >> 63 != 0 {
            return Err(Error::Unsupported("checkpoint descriptor B-tree".into()));
        }
        let mut candidates: Vec<(u64, u64)> = Vec::new();
        let mut b = vec![0u8; block_size as usize];
        for i in 0..desc_blocks as u64 {
            let bno = desc_base + i;
            if img.read_at(&mut b, bno * block_size as u64).is_err() {
                continue;
            }
            if verify_checksum(&b)
                && (le32(&b, 0x18) & OBJ_TYPE_MASK) == T_NX
                && &b[32..36] == NX_MAGIC
            {
                candidates.push((le64(&b, 0x10), bno));
            }
        }
        if candidates.is_empty() {
            return corrupt("no valid container superblock in the checkpoint area");
        }
        candidates.sort_unstable_by(|a, b| b.cmp(a));

        let mut last_err = None;
        for (xid, bno) in candidates {
            match Self::try_checkpoint(&img, block_size, xid, bno) {
                Ok((sb, maps)) => {
                    let block_count = le64(&sb, NX_BLOCK_COUNT);
                    let omap_phys = le64(&sb, NX_OMAP_OID);
                    let cache = RefCell::new(HashMap::new());
                    let mut c = Container {
                        img,
                        block_size,
                        sb,
                        xid,
                        block_count,
                        maps,
                        omap_root: 0,
                        cache,
                    };
                    let mut om = vec![0u8; block_size as usize];
                    c.read_block(omap_phys, &mut om)?;
                    if !verify_checksum(&om) {
                        return corrupt("bad checksum on container object map");
                    }
                    c.omap_root = le64(&om, OM_TREE_OID);
                    return Ok(c);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Corrupt("no usable checkpoint".into())))
    }

    fn try_checkpoint(
        img: &Image,
        block_size: u32,
        xid: u64,
        sb_bno: u64,
    ) -> Result<(Vec<u8>, Vec<CheckpointMapping>)> {
        let mut sb = vec![0u8; block_size as usize];
        img.read_at(&mut sb, sb_bno * block_size as u64)?;
        let desc_base = le64(&sb, NX_XP_DESC_BASE);
        let desc_blocks = le32(&sb, NX_XP_DESC_BLOCKS) as u64;
        let index = le32(&sb, NX_XP_DESC_INDEX) as u64;
        let len = le32(&sb, NX_XP_DESC_LEN) as u64;
        if len == 0 || len > desc_blocks {
            return corrupt("checkpoint descriptor range is invalid");
        }
        let mut maps = Vec::new();
        let mut b = vec![0u8; block_size as usize];
        for i in 0..len - 1 {
            let bno = desc_base + (index + i) % desc_blocks;
            img.read_at(&mut b, bno * block_size as u64)?;
            if !verify_checksum(&b)
                || (le32(&b, 0x18) & OBJ_TYPE_MASK) != T_CPM
                || le64(&b, 0x10) != xid
            {
                return corrupt("checkpoint map block is invalid");
            }
            let count = le32(&b, 0x24) as usize;
            if 0x28 + count * 40 > block_size as usize {
                return corrupt("checkpoint map has too many entries");
            }
            for j in 0..count {
                let e = 0x28 + j * 40;
                maps.push(CheckpointMapping {
                    otype: le32(&b, e),
                    subtype: le32(&b, e + 4),
                    size: le32(&b, e + 8),
                    fs_oid: le64(&b, e + 16),
                    oid: le64(&b, e + 24),
                    paddr: le64(&b, e + 32),
                });
            }
        }
        for m in &maps {
            let obj = read_ephemeral_raw(img, &sb, block_size, m)?;
            if !verify_checksum(&obj) || le64(&obj, 0x10) != xid {
                return corrupt("an ephemeral object of this checkpoint is invalid");
            }
        }
        Ok((sb, maps))
    }

    /// Reads one block by number.
    pub fn read_block(&self, bno: u64, buf: &mut [u8]) -> Result<()> {
        if bno >= self.block_count_hint() {
            return corrupt(format!("block {bno:#x} is outside the container"));
        }
        self.img.read_at(buf, bno * self.block_size as u64)
    }

    fn block_count_hint(&self) -> u64 {
        if self.block_count != 0 {
            self.block_count
        } else {
            u64::MAX / self.block_size as u64
        }
    }

    pub fn ephemeral(&self, oid: u64) -> Result<Vec<u8>> {
        let m = self
            .maps
            .iter()
            .find(|m| m.oid == oid)
            .ok_or_else(|| Error::Corrupt(format!("no ephemeral object {oid:#x}")))?;
        read_ephemeral_raw(&self.img, &self.sb, self.block_size, m)
    }

    /// (paddr, flags) of a virtual object in the given omap tree at a transaction.
    pub fn omap_lookup(&self, omap_root: u64, oid: u64, xid: u64) -> Result<Option<(u64, u32)>> {
        let tree = Tree { kind: KeyKind::Omap, root: omap_root, virt: None };
        let mut key = [0u8; 16];
        put64(&mut key, 0, oid);
        put64(&mut key, 8, xid);
        match tree.find_le(self, &key)? {
            Some((k, v)) if le64(&k, 0) == oid => {
                if v.len() < 16 {
                    return corrupt("short omap value");
                }
                let flags = le32(&v, 0);
                Ok(if flags & OMAP_VAL_DELETED != 0 { None } else { Some((le64(&v, 8), flags)) })
            }
            _ => Ok(None),
        }
    }

    /// Drops cached nodes (a commit changed the world).
    pub fn clear_cache(&self) {
        self.cache.borrow_mut().clear();
    }

    /// All volumes, in container order.
    pub fn volumes(&self) -> Result<Vec<VolumeSummary>> {
        let max = (le32(&self.sb, NX_MAX_FS) as usize).min(NX_MAX_FILE_SYSTEMS);
        let mut out = Vec::new();
        let mut b = vec![0u8; self.block_size as usize];
        for i in 0..max {
            let oid = le64(&self.sb, NX_FS_OID + i * 8);
            if oid == 0 {
                continue;
            }
            let (paddr, _) = match self.omap_lookup(self.omap_root, oid, self.xid)? {
                Some(x) => x,
                None => continue,
            };
            self.read_block(paddr, &mut b)?;
            if &b[32..36] != FS_MAGIC || !verify_checksum(&b) {
                return corrupt(format!("invalid superblock for volume {i}"));
            }
            let uuid: [u8; 16] = b[FS_UUID..FS_UUID + 16].try_into().unwrap();
            let name = String::from_utf8_lossy(cstr_bytes(&b[FS_VOLNAME..FS_VOLNAME + 256])).into_owned();
            let role = le16(&b, FS_ROLE);
            let incompat = le64(&b, FS_INCOMPAT);
            let fs_flags = le64(&b, FS_FLAGS);
            let sealed = incompat & INCOMPAT_SEALED != 0;
            let encrypted = fs_flags & FSF_UNENCRYPTED == 0;
            out.push(VolumeSummary {
                index: i,
                oid,
                paddr,
                name,
                uuid,
                role,
                role_name: role_name(role),
                sealed,
                encrypted,
                incompat,
                rocompat: le64(&b, FS_ROCOMPAT),
                fs_flags,
                snapshots: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Opens a volume, read-only. See [`VolumeSelector`].
    pub fn open_volume(&self, sel: &VolumeSelector) -> Result<Volume<'_>> {
        let mut list = self.volumes()?;
        if list.is_empty() {
            return Err(Error::NotFound("the container has no volumes".into()));
        }
        let pick = pick_volume(&list, sel)?;
        let s = list.swap_remove(pick);
        Volume::open(self, &s, sel.snapshot.as_deref())
    }
}

/// Matches a volume by index, role (exact word) or name.
pub fn pick_volume(list: &[VolumeSummary], sel: &VolumeSelector) -> Result<usize> {
    let hits: Vec<usize> = list
        .iter()
        .enumerate()
        .filter(|(_, v)| {
            sel.index.is_none_or(|i| v.index == i)
                && sel.role.as_ref().is_none_or(|r| v.role_name.split(',').any(|n| n == r))
                && sel.name.as_ref().is_none_or(|n| &v.name == n)
        })
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [] => Err(Error::NotFound(format!("no volume matches {sel:?}"))),
        [only] => Ok(*only),
        many => {
            if sel.index.is_none() && sel.role.is_none() && sel.name.is_none() {
                Ok(many[0])
            } else {
                Err(Error::Invalid(format!("{} volumes match {sel:?}", many.len())))
            }
        }
    }
}

fn cstr_bytes(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(n) => &b[..n],
        None => b,
    }
}

/// Reads an ephemeral object, following the wrap of the checkpoint data ring.
pub fn read_ephemeral_raw(
    img: &Image,
    sb: &[u8],
    block_size: u32,
    m: &CheckpointMapping,
) -> Result<Vec<u8>> {
    let size = m.size as usize;
    let bs = block_size as usize;
    if size == 0 || size % bs != 0 {
        return corrupt("ephemeral object has an invalid size");
    }
    let base = le64(sb, NX_XP_DATA_BASE);
    let blocks = le32(sb, NX_XP_DATA_BLOCKS) as u64;
    let mut idx = m.paddr.checked_sub(base).filter(|i| *i < blocks).ok_or_else(|| {
        Error::Corrupt(format!("ephemeral object at {:#x} is outside the data area", m.paddr))
    })?;
    let mut out = vec![0u8; size];
    for chunk in out.chunks_mut(bs) {
        img.read_at(chunk, (base + idx) * block_size as u64)?;
        idx += 1;
        if idx == blocks {
            idx = 0;
        }
    }
    Ok(out)
}

impl NodeSource for Container {
    fn node(&self, tree: &Tree, oid: u64) -> Result<Rc<Node>> {
        let paddr = match tree.virt {
            None => oid,
            Some((omap_root, xid)) => self
                .omap_lookup(omap_root, oid, xid)?
                .ok_or_else(|| Error::Corrupt(format!("omap has no record for oid {oid:#x}")))?
                .0,
        };
        if let Some(n) = self.cache.borrow().get(&paddr) {
            return Ok(n.clone());
        }
        let mut b = vec![0u8; self.block_size as usize];
        self.read_block(paddr, &mut b)?;
        // Sealed nodes have no header. Every other node has a checksum.
        let flags = le16(&b, BTN_FLAGS);
        let noheader = flags & BTNODE_NOHEADER != 0;
        if !noheader && !verify_checksum(&b) {
            return corrupt(format!("bad checksum on B-tree node at {paddr:#x}"));
        }
        let node = Rc::new(Node::parse(b, tree.kind)?);
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= 8192 {
            cache.clear();
        }
        cache.insert(paddr, node.clone());
        Ok(node)
    }
}
