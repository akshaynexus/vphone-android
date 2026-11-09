//! Userspace APFS: read any container/volume found in an image file, and
//! write to unsealed, unencrypted volumes with no mount and no root.
//!
//! Layering:
//! - [`image`]: finds the APFS container inside a file (raw, IM4P, GPT, UDIF)
//!   and exposes block I/O.
//! - [`raw`]: on-disk layout constants and little-endian helpers.
//! - [`btree`], [`container`], [`volume`]: the read path.
//! - [`decmpfs`]: transparent compression.
//! - [`write`]: writes, through the pinned `apfs-write` engine (allocation,
//!   B-trees, object maps, free queues, checkpoint commit).

pub mod btree;
pub mod container;
pub mod decmpfs;
pub mod error;
pub mod image;
pub mod names;
pub mod ops;
pub mod raw;
pub mod volume;
pub mod write;

pub use container::{Container, VolumeSelector};
pub use error::{Error, Result};
pub use image::Image;
pub use ops::{apply_meta_line, copy_in, extract, inject_ramdisk, walk_tree, ManifestEntry, MetaOverride};
pub use volume::{DirEntry, FileKind, Inode, Volume, Xattr};
pub use write::{Attrs, MutVolume};

/// Opens an image read-only.
pub fn open_image(path: &std::path::Path) -> Result<Container> {
    Container::open(Image::open(path, false)?)
}
