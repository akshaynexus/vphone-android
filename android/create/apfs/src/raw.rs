//! On-disk layout: offsets, flags and little-endian accessors.

pub const NX_MAGIC: &[u8; 4] = b"NXSB";
pub const FS_MAGIC: &[u8; 4] = b"APSB";

pub const OBJ_HDR: usize = 0x20;
pub const OBJ_TYPE_MASK: u32 = 0xffff;
pub const OBJ_STORAGE_MASK: u32 = 0xc000_0000;
pub const OBJ_VIRTUAL: u32 = 0;
pub const OBJ_EPHEMERAL: u32 = 0x8000_0000;
pub const OBJ_PHYSICAL: u32 = 0x4000_0000;

pub const T_NX: u32 = 1;
pub const T_BTREE: u32 = 2;
pub const T_BTREE_NODE: u32 = 3;
pub const T_SPACEMAN: u32 = 5;
pub const T_CAB: u32 = 6;
pub const T_CIB: u32 = 7;
pub const T_FQ: u32 = 9;
pub const T_OMAP: u32 = 0xb;
pub const T_CPM: u32 = 0xc;
pub const T_FS: u32 = 0xd;
pub const T_FSTREE: u32 = 0xe;
pub const T_EXTENTREF: u32 = 0xf;
pub const T_SNAPMETA: u32 = 0x10;
pub const T_OMAP_SNAP: u32 = 0x13;
pub const T_FEXT: u32 = 0x1f;

// B-tree node.
pub const BTN_FLAGS: usize = 0x20;
pub const BTN_LEVEL: usize = 0x22;
pub const BTN_NKEYS: usize = 0x24;
pub const BTN_TABLE: usize = 0x28;
pub const BTN_FREE: usize = 0x2c;
pub const BTN_KFREE: usize = 0x30;
pub const BTN_VFREE: usize = 0x34;
pub const BTN_DATA: usize = 0x38;
pub const BTNODE_ROOT: u16 = 1;
pub const BTNODE_LEAF: u16 = 2;
pub const BTNODE_FIXED: u16 = 4;
pub const BTNODE_HASHED: u16 = 8;
pub const BTNODE_NOHEADER: u16 = 0x10;
pub const BTOFF_INVALID: u16 = 0xffff;
pub const BT_INFO_SIZE: usize = 40;

pub const BTREE_UINT64_KEYS: u32 = 1;
pub const BTREE_SEQUENTIAL: u32 = 2;
pub const BTREE_GHOSTS: u32 = 4;
pub const BTREE_EPHEMERAL: u32 = 8;
pub const BTREE_PHYSICAL: u32 = 0x10;
pub const BTREE_NONALIGNED: u32 = 0x40;
pub const BTREE_HASHED: u32 = 0x80;
pub const BTREE_NOHEADER: u32 = 0x100;

// Container superblock.
pub const NX_BLOCK_SIZE: usize = 0x24;
pub const NX_BLOCK_COUNT: usize = 0x28;
pub const NX_FEATURES: usize = 0x30;
pub const NX_ROCOMPAT: usize = 0x38;
pub const NX_INCOMPAT: usize = 0x40;
pub const NX_UUID: usize = 0x48;
pub const NX_NEXT_OID: usize = 0x58;
pub const NX_NEXT_XID: usize = 0x60;
pub const NX_XP_DESC_BLOCKS: usize = 0x68;
pub const NX_XP_DATA_BLOCKS: usize = 0x6c;
pub const NX_XP_DESC_BASE: usize = 0x70;
pub const NX_XP_DATA_BASE: usize = 0x78;
pub const NX_XP_DESC_NEXT: usize = 0x80;
pub const NX_XP_DATA_NEXT: usize = 0x84;
pub const NX_XP_DESC_INDEX: usize = 0x88;
pub const NX_XP_DESC_LEN: usize = 0x8c;
pub const NX_XP_DATA_INDEX: usize = 0x90;
pub const NX_XP_DATA_LEN: usize = 0x94;
pub const NX_SPACEMAN_OID: usize = 0x98;
pub const NX_OMAP_OID: usize = 0xa0;
pub const NX_REAPER_OID: usize = 0xa8;
pub const NX_MAX_FS: usize = 0xb4;
pub const NX_FS_OID: usize = 0xb8;
pub const NX_MAX_FILE_SYSTEMS: usize = 100;

// Object map.
pub const OM_FLAGS: usize = 0x20;
pub const OM_SNAP_COUNT: usize = 0x24;
pub const OM_TREE_OID: usize = 0x30;
pub const OM_SNAP_TREE_OID: usize = 0x38;
pub const OM_MOST_RECENT_SNAP: usize = 0x40;
pub const OMAP_VAL_DELETED: u32 = 1;
pub const OMAP_VAL_NOHEADER: u32 = 8;
pub const OMAP_MANUALLY_MANAGED: u32 = 1;

// Volume superblock.
pub const FS_INDEX: usize = 0x24;
pub const FS_FEATURES: usize = 0x28;
pub const FS_ROCOMPAT: usize = 0x30;
pub const FS_INCOMPAT: usize = 0x38;
pub const FS_UNMOUNT_TIME: usize = 0x40;
pub const FS_RESERVE: usize = 0x48;
pub const FS_QUOTA: usize = 0x50;
pub const FS_ALLOC_COUNT: usize = 0x58;
pub const FS_ROOT_TREE_TYPE: usize = 0x74;
pub const FS_OMAP_OID: usize = 0x80;
pub const FS_ROOT_TREE_OID: usize = 0x88;
pub const FS_EXTENTREF_OID: usize = 0x90;
pub const FS_SNAPMETA_OID: usize = 0x98;
pub const FS_NEXT_OBJ_ID: usize = 0xb0;
pub const FS_NUM_FILES: usize = 0xb8;
pub const FS_NUM_DIRS: usize = 0xc0;
pub const FS_NUM_SYMLINKS: usize = 0xc8;
pub const FS_NUM_OTHER: usize = 0xd0;
pub const FS_NUM_SNAPS: usize = 0xd8;
pub const FS_TOTAL_ALLOC: usize = 0xe0;
pub const FS_TOTAL_FREED: usize = 0xe8;
pub const FS_UUID: usize = 0xf0;
pub const FS_LAST_MOD: usize = 0x100;
pub const FS_FLAGS: usize = 0x108;
pub const FS_FORMATTED_BY: usize = 0x110;
pub const FS_MODIFIED_BY: usize = 0x140;
pub const FS_MODIFIED_ENTRY: usize = 48;
pub const FS_MAX_HIST: usize = 8;
pub const FS_VOLNAME: usize = 0x2c0;
pub const FS_ROLE: usize = 0x3c4;
pub const FS_INTEGRITY_META: usize = 0x400;
pub const FS_FEXT_OID: usize = 0x408;
pub const FS_FEXT_TYPE: usize = 0x410;

pub const FSF_UNENCRYPTED: u64 = 1;
pub const FSF_ONEKEY: u64 = 8;

pub const INCOMPAT_CASE_INSENSITIVE: u64 = 1;
pub const INCOMPAT_DATALESS_SNAPS: u64 = 2;
pub const INCOMPAT_ENC_ROLLED: u64 = 4;
pub const INCOMPAT_NORM_INSENSITIVE: u64 = 8;
pub const INCOMPAT_INCOMPLETE_RESTORE: u64 = 0x10;
pub const INCOMPAT_SEALED: u64 = 0x20;
pub const INCOMPAT_PFK: u64 = 0x40;
pub const INCOMPAT_PREALLOC_FLAG: u64 = 0x80;
pub const INCOMPAT_SECONDARY_FSROOT: u64 = 0x100;
pub const INCOMPAT_UNKNOWN_BIT9: u64 = 0x200;
/// iOS 27 sets read-only-compatible bits 2 and 3; both are understood for
/// reading and do not constrain the writes this crate makes.
pub const ROCOMPAT_KNOWN: u64 = 0x4 | 0x8;
pub const INCOMPAT_SUPPORTED: u64 = INCOMPAT_CASE_INSENSITIVE
    | INCOMPAT_DATALESS_SNAPS
    | INCOMPAT_ENC_ROLLED
    | INCOMPAT_NORM_INSENSITIVE
    | INCOMPAT_INCOMPLETE_RESTORE
    | INCOMPAT_SEALED
    | INCOMPAT_PFK
    | INCOMPAT_PREALLOC_FLAG
    | INCOMPAT_SECONDARY_FSROOT
    | INCOMPAT_UNKNOWN_BIT9;

// Catalog key types.
pub const J_INODE: u8 = 3;
pub const J_XATTR: u8 = 4;
pub const J_SIBLING_LINK: u8 = 5;
pub const J_DSTREAM_ID: u8 = 6;
pub const J_FILE_EXTENT: u8 = 8;
pub const J_DIR_REC: u8 = 9;
pub const J_DIR_STATS: u8 = 10;
pub const J_SIBLING_MAP: u8 = 12;
pub const J_FILE_INFO: u8 = 13;
pub const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
pub const OBJ_TYPE_SHIFT: u32 = 60;

pub const DREC_LEN_MASK: u32 = 0x3ff;
pub const DREC_HASH_SHIFT: u32 = 10;

// Inode value.
pub const INO_PARENT: usize = 0;
pub const INO_PRIVATE: usize = 8;
pub const INO_CREATE: usize = 0x10;
pub const INO_MOD: usize = 0x18;
pub const INO_CHANGE: usize = 0x20;
pub const INO_ACCESS: usize = 0x28;
pub const INO_FLAGS: usize = 0x30;
pub const INO_NCHILDREN: usize = 0x38;
pub const INO_BSD_FLAGS: usize = 0x44;
pub const INO_UID: usize = 0x48;
pub const INO_GID: usize = 0x4c;
pub const INO_MODE: usize = 0x50;
pub const INO_UNCOMP: usize = 0x54;
pub const INO_XFIELDS: usize = 0x5c;
pub const INODE_IS_SPARSE: u64 = 0x200;
pub const INODE_HAS_RSRC: u64 = 0x4000;
pub const INODE_NO_RSRC: u64 = 0x8000;
pub const INODE_HAS_FINDER: u64 = 0x100;
pub const INODE_MAINTAIN_DIR_STATS: u64 = 2;
pub const INODE_DIR_STATS_ORIGIN: u64 = 4;
pub const INODE_HAS_UNCOMP: u64 = 0x40000;
pub const BSD_COMPRESSED: u32 = 0x20;

pub const X_NAME: u8 = 4;
pub const X_DSTREAM: u8 = 8;
pub const X_DIR_STATS_KEY: u8 = 10;
pub const X_SPARSE: u8 = 13;
pub const X_RDEV: u8 = 14;
pub const X_SIBLING_ID: u8 = 1;
pub const XF_DO_NOT_COPY: u8 = 2;
pub const XF_SYSTEM_FIELD: u8 = 0x20;

pub const XATTR_STREAM: u16 = 1;
pub const XATTR_EMBEDDED: u16 = 2;
pub const XATTR_FS_OWNED: u16 = 4;
pub const XATTR_MAX_EMBEDDED: usize = 3804;
pub const XATTR_SYMLINK: &str = "com.apple.fs.symlink";
pub const XATTR_DECMPFS: &str = "com.apple.decmpfs";
pub const XATTR_RSRC: &str = "com.apple.ResourceFork";
pub const XATTR_FINDER: &str = "com.apple.FinderInfo";

pub const FE_LEN_MASK: u64 = 0x00ff_ffff_ffff_ffff;
pub const FE_CRYPTO_FLAG: u64 = 1 << 56;
pub const PEXT_LEN_MASK: u64 = 0x0fff_ffff_ffff_ffff;
pub const PEXT_KIND_SHIFT: u32 = 60;
pub const KIND_NEW: u64 = 1;
pub const OWNING_UNKNOWN: u64 = !1u64;

pub const ROOT_DIR_PARENT: u64 = 1;
pub const ROOT_DIR_INO: u64 = 2;
pub const PRIV_DIR_INO: u64 = 3;
pub const MIN_USER_INO: u64 = 16;

pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFIFO: u16 = 0o010000;
pub const S_IFSOCK: u16 = 0o140000;

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Fletcher-64 over everything after the checksum field.
pub fn fletcher(block: &[u8]) -> u64 {
    const M: u64 = 0xffff_ffff;
    let (mut s1, mut s2) = (0u64, 0u64);
    for w in block[8..].chunks_exact(4) {
        s1 = (s1 + u32::from_le_bytes(w.try_into().unwrap()) as u64) % M;
        s2 = (s2 + s1) % M;
    }
    let c1 = M - (s1 + s2) % M;
    let c2 = M - (s1 + c1) % M;
    (c2 << 32) | c1
}

pub fn set_checksum(block: &mut [u8]) {
    let c = fletcher(block);
    put64(block, 0, c);
}

pub fn verify_checksum(block: &[u8]) -> bool {
    block.len() >= 8 && le64(block, 0) == fletcher(block)
}

pub fn round_up(v: usize, to: usize) -> usize {
    (v + to - 1) / to * to
}
