//! Tree-level operations: manifests, extraction, host-tree copy-in, and the
//! ports of tools/ssh-ramdisk-inject.sh and the seal patcher's file swap.

use crate::error::{Error, Result};
use crate::raw::*;
use crate::volume::{walk, FileKind, Inode, Volume};
use crate::write::{Attrs, MutVolume};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{lchown, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// One manifest record.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub path: String,
    pub inode: Inode,
    pub target: Option<String>,
    pub xattrs: Vec<(String, Vec<u8>)>,
    pub size: u64,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(Error::Invalid(format!("odd hex string '{s}'")));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| Error::Invalid(format!("bad hex string '{s}'")))
        })
        .collect()
}

impl ManifestEntry {
    pub fn to_json(&self) -> Value {
        let i = &self.inode;
        let xattrs: Vec<Value> = self
            .xattrs
            .iter()
            .map(|(k, v)| json!({"name": k, "hex": hex(v)}))
            .collect();
        json!({
            "path": self.path,
            "kind": i.kind().as_str(),
            "mode": i.mode & 0o7777,
            "uid": i.uid,
            "gid": i.gid,
            "flags": i.bsd_flags,
            "size": self.size,
            "nlink": i.nlink(),
            "ino": i.ino,
            "mtime_ns": i.mod_time,
            "ctime_ns": i.change_time,
            "atime_ns": i.access_time,
            "btime_ns": i.create_time,
            "target": self.target,
            "rdev": i.rdev,
            "xattrs": xattrs,
        })
    }
}

fn is_hidden_xattr(x: &str, i: &Inode) -> bool {
    x == XATTR_SYMLINK || (i.is_compressed() && (x == XATTR_DECMPFS || x == XATTR_RSRC))
}

fn manifest_entry(v: &Volume<'_>, path: &str, i: &Inode) -> Result<ManifestEntry> {
    let target = if i.kind() == FileKind::Symlink { Some(v.read_symlink(i)?) } else { None };
    let mut xattrs = Vec::new();
    for x in v.xattrs(i.ino)? {
        if !is_hidden_xattr(&x.name, i) {
            let value = v.xattr_value(&x)?;
            xattrs.push((x.name, value));
        }
    }
    let size = if i.kind() == FileKind::File { v.file_size(i)? } else { 0 };
    Ok(ManifestEntry { path: path.to_string(), inode: i.clone(), target, xattrs, size })
}

/// Visits a tree and calls `f` with a manifest entry for everything in it.
pub fn walk_tree<F>(v: &Volume<'_>, src: &str, f: &mut F) -> Result<()>
where
    F: FnMut(&ManifestEntry) -> Result<()>,
{
    let root = v.lookup(src)?;
    let base = src.trim_matches('/');
    walk(v, &root, &mut |rel, i| {
        let path = match (base.is_empty(), rel.is_empty()) {
            (true, _) => format!("/{rel}"),
            (false, true) => format!("/{base}"),
            (false, false) => format!("/{base}/{rel}"),
        };
        f(&manifest_entry(v, &path, i)?)
    })
}

/// Sets uid/gid where the OS allows it. An app without CAP_CHOWN gets EPERM;
/// the manifest carries the real values for the caller to apply.
fn try_chown(path: &Path, uid: u32, gid: u32) {
    let _ = lchown(path, Some(uid), Some(gid));
}

fn set_time(path: &Path, mtime_ns: u64, atime_ns: u64) {
    let to = |ns: u64| std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ns);
    if let Ok(f) = std::fs::OpenOptions::new().read(true).open(path) {
        let _ = f.set_times(
            std::fs::FileTimes::new().set_accessed(to(atime_ns)).set_modified(to(mtime_ns)),
        );
    }
}

/// Copies a tree out of the volume into `dest`, recreating hard links.
/// Returns the number of entries.
pub fn extract(
    v: &Volume<'_>,
    src: &str,
    dest: &Path,
    mut manifest: Option<&mut dyn Write>,
) -> Result<u64> {
    let root_inode = v.lookup(src)?;
    let root_is_dir = root_inode.kind() == FileKind::Dir;
    std::fs::create_dir_all(dest)?;
    let mut links: HashMap<u64, PathBuf> = HashMap::new();
    let mut count = 0u64;
    let mut dirs: Vec<(PathBuf, u16, u32, u32, u64, u64)> = Vec::new();
    walk_tree(v, src, &mut |e| {
        let rel = e.path.trim_start_matches('/');
        let base = src.trim_matches('/');
        let below = rel.strip_prefix(base).unwrap_or(rel).trim_start_matches('/');
        let host = if root_is_dir {
            dest.join(below)
        } else {
            dest.join(Path::new(rel).file_name().unwrap_or_default())
        };
        let i = &e.inode;
        match i.kind() {
            FileKind::Dir => {
                std::fs::create_dir_all(&host)?;
                dirs.push((host.clone(), i.mode & 0o7777, i.uid, i.gid, i.mod_time, i.access_time));
            }
            FileKind::Symlink => {
                let t = e.target.as_deref().unwrap_or("");
                if host.symlink_metadata().is_ok() {
                    std::fs::remove_file(&host)?;
                }
                std::os::unix::fs::symlink(t, &host)?;
                try_chown(&host, i.uid, i.gid);
            }
            FileKind::File => {
                if host.symlink_metadata().is_ok() {
                    std::fs::remove_file(&host)?;
                }
                if i.nlink() > 1 {
                    if let Some(first) = links.get(&i.ino) {
                        std::fs::hard_link(first, &host)?;
                        count += 1;
                        return write_manifest(&mut manifest, e);
                    }
                    links.insert(i.ino, host.clone());
                }
                let f = std::fs::File::create(&host)?;
                let mut w = std::io::BufWriter::new(f);
                v.write_file_to(i, &mut w)?;
                w.flush()?;
                drop(w);
                std::fs::set_permissions(&host, std::fs::Permissions::from_mode((i.mode & 0o7777) as u32))?;
                try_chown(&host, i.uid, i.gid);
                set_time(&host, i.mod_time, i.access_time);
            }
            // Device nodes, FIFOs and sockets cannot be created without
            // privilege; they stay in the manifest for the caller.
            _ => {}
        }
        count += 1;
        write_manifest(&mut manifest, e)
    })?;
    // Directory modes and times last, so children could be created inside them.
    for (p, mode, uid, gid, m, a) in dirs.into_iter().rev() {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode as u32))?;
        try_chown(&p, uid, gid);
        set_time(&p, m, a);
    }
    Ok(count)
}

fn write_manifest(m: &mut Option<&mut dyn Write>, e: &ManifestEntry) -> Result<()> {
    if let Some(w) = m.as_deref_mut() {
        writeln!(w, "{}", e.to_json())?;
    }
    Ok(())
}

/// Per-path metadata for [`copy_in`].
#[derive(Debug, Clone, Default)]
pub struct MetaOverride {
    pub entries: HashMap<String, Attrs>,
    pub xattrs: HashMap<String, Vec<(String, Vec<u8>)>>,
}

/// Parses one manifest line (extract's format) into the metadata map, keyed
/// by the path without its leading slash.
pub fn apply_meta_line(meta: &mut MetaOverride, line: &str) -> Result<()> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| Error::Invalid(format!("bad metadata line: {e}")))?;
    let path = v["path"]
        .as_str()
        .ok_or_else(|| Error::Invalid("metadata line without a path".into()))?
        .trim_matches('/')
        .to_string();
    let a = Attrs {
        mode: v["mode"].as_u64().map(|m| m as u16),
        uid: v["uid"].as_u64().map(|m| m as u32),
        gid: v["gid"].as_u64().map(|m| m as u32),
        mtime_ns: v["mtime_ns"].as_u64(),
        ctime_ns: v["ctime_ns"].as_u64(),
        atime_ns: v["atime_ns"].as_u64(),
        btime_ns: v["btime_ns"].as_u64(),
        // The compressed flag describes how the source stored the data; the
        // file written from this metadata is stored uncompressed.
        bsd_flags: v["flags"].as_u64().map(|m| m as u32 & !BSD_COMPRESSED),
    };
    meta.entries.insert(path.clone(), a);
    if let Some(list) = v["xattrs"].as_array() {
        let mut out = Vec::with_capacity(list.len());
        for x in list {
            if let (Some(n), Some(h)) = (x["name"].as_str(), x["hex"].as_str()) {
                out.push((n.to_string(), unhex(h)?));
            }
        }
        if !out.is_empty() {
            meta.xattrs.insert(path, out);
        }
    }
    Ok(())
}

/// Copies `host` into the volume at `guest`.
///
/// - `meta`: exact per-path attributes (relative paths), from a manifest.
/// - `owner`: uid/gid for entries `meta` does not cover. Host ownership is
///   never used, since on Android it is the app's own uid.
///
/// Hard links in the host tree become hard links in the volume, symlinks
/// stay symlinks, and an existing destination directory is kept (its contents
/// merge). Device nodes and sockets are refused rather than dropped.
pub fn copy_in(
    v: &mut MutVolume,
    host: &Path,
    guest: &str,
    owner: Option<(u32, u32)>,
    meta: Option<&MetaOverride>,
) -> Result<u64> {
    let md = std::fs::symlink_metadata(host)?;
    let name = host
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Invalid(format!("{}: bad name", host.display())))?;
    let top = format!("{}/{}", guest.trim_end_matches('/'), name);
    let mut copied = 0;
    let mut links: HashMap<(u64, u64), String> = HashMap::new();
    copy_one(v, host, &top, "", &md, owner, meta, &mut links, &mut copied)?;
    Ok(copied)
}

fn host_attrs(md: &std::fs::Metadata, owner: Option<(u32, u32)>, meta: Option<&Attrs>) -> Attrs {
    if let Some(m) = meta {
        return *m;
    }
    let (uid, gid) = owner.map_or((None, None), |(u, g)| (Some(u), Some(g)));
    Attrs {
        mode: Some(md.mode() as u16 & 0o7777),
        uid,
        gid,
        mtime_ns: Some((md.mtime() as u64).saturating_mul(1_000_000_000) + md.mtime_nsec() as u64),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_one(
    v: &mut MutVolume,
    host: &Path,
    guest: &str,
    rel: &str,
    md: &std::fs::Metadata,
    owner: Option<(u32, u32)>,
    meta: Option<&MetaOverride>,
    links: &mut HashMap<(u64, u64), String>,
    copied: &mut u64,
) -> Result<()> {
    let m = meta.and_then(|x| x.entries.get(rel));
    let a = host_attrs(md, owner, m);
    let ft = md.file_type();
    if ft.is_dir() {
        if !v.exists(guest)? {
            v.mkdir(guest, &a, false)?;
        }
        let mut names: Vec<_> = std::fs::read_dir(host)?.collect::<std::io::Result<Vec<_>>>()?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let n = e.file_name();
            let n = n
                .to_str()
                .ok_or_else(|| Error::Invalid(format!("{}: non-UTF-8 name", e.path().display())))?;
            let child_rel = if rel.is_empty() { n.to_string() } else { format!("{rel}/{n}") };
            let child_guest = format!("{guest}/{n}");
            let cmd = e.metadata()?;
            copy_one(v, &e.path(), &child_guest, &child_rel, &cmd, owner, meta, links, copied)?;
        }
    } else if ft.is_symlink() {
        let t = std::fs::read_link(host)?;
        let t = t
            .to_str()
            .ok_or_else(|| Error::Invalid(format!("{}: non-UTF-8 target", host.display())))?;
        v.symlink(t, guest, &a)?;
    } else if ft.is_file() {
        let key = (md.dev(), md.ino());
        if md.nlink() > 1 {
            if let Some(first) = links.get(&key) {
                v.link(first, guest)?;
                *copied += 1;
                return v.commit_if_large();
            }
            links.insert(key, guest.to_string());
        }
        let data = std::fs::read(host)?;
        v.create_file(guest, &data, &a)?;
    } else {
        return Err(Error::Unsupported(format!(
            "{}: device nodes, FIFOs and sockets cannot be copied in",
            host.display()
        )));
    }
    if let Some(xs) = meta.and_then(|x| x.xattrs.get(rel)) {
        for (n, val) in xs {
            v.setxattr(guest, n, val)?;
        }
    }
    *copied += 1;
    v.commit_if_large()
}

/// The paths tools/ssh-ramdisk-inject.sh never keeps.
const DROP: [&str; 7] = [
    "/usr/bin/img4tool",
    "/usr/bin/img4",
    "/usr/sbin/dietappleh13camerad",
    "/usr/sbin/dietappleh16camerad",
    "/usr/local/bin/wget",
    "/usr/local/bin/procexp",
    "/usr/standalone/firmware/S6TUUP1",
];

fn drop_listed(v: &mut MutVolume) -> Result<()> {
    for p in DROP {
        if v.exists(p)? {
            v.remove_all(p)?;
        }
    }
    Ok(())
}

/// Port of tools/ssh-ramdisk-inject.sh: removes the drop list (to make room),
/// unlinks every existing non-directory the staging tree will replace,
/// extracts the staging tree (existing directories keep their metadata, new
/// entries get the staging modes and the extraction time), makes the whole
/// volume root-owned without following symlinks, and drops the list again.
pub fn inject_ramdisk(v: &mut MutVolume, stage: &Path) -> Result<u64> {
    if !stage.is_dir() {
        return Err(Error::NotFound(format!("staging tree {}", stage.display())));
    }
    drop_listed(v)?;
    let mut names = Vec::new();
    collect_host_tree(stage, Path::new(""), &mut names)?;
    // Unlink first: this keeps other links of a hard-linked base file alive
    // (a plain overwrite would truncate the shared inode).
    for (rel, md) in &names {
        let g = format!("/{}", rel.display());
        if md.file_type().is_dir() || !v.exists(&g)? {
            continue;
        }
        let existing = v.lookup(&g)?;
        if existing.mode & S_IFMT != S_IFDIR {
            v.remove(&g)?;
        }
    }
    let (mut copied, mut links) = (0u64, HashMap::<(u64, u64), String>::new());
    for (rel, md) in &names {
        let host = stage.join(rel);
        let g = format!("/{}", rel.display());
        let perm = md.mode() as u16 & 0o7777;
        let a = Attrs { mode: Some(perm), ..Default::default() };
        let ft = md.file_type();
        if ft.is_dir() {
            if !v.exists(&g)? {
                v.mkdir(&g, &a, false)?;
            }
        } else if ft.is_symlink() {
            let t = std::fs::read_link(&host)?;
            v.symlink(&t.to_string_lossy(), &g, &a)?;
        } else if ft.is_file() {
            let key = (md.dev(), md.ino());
            if md.nlink() > 1 {
                if let Some(first) = links.get(&key) {
                    v.link(first, &g)?;
                    copied += 1;
                    v.commit_if_large()?;
                    continue;
                }
                links.insert(key, g.clone());
            }
            let data = std::fs::read(&host)?;
            v.create_file(&g, &data, &a)?;
        } else {
            return Err(Error::Unsupported(format!(
                "{}: device nodes, FIFOs and sockets cannot be injected",
                host.display()
            )));
        }
        copied += 1;
        v.commit_if_large()?;
    }
    chown_all_root(v)?;
    drop_listed(v)?;
    v.commit()?;
    Ok(copied)
}

fn collect_host_tree(
    root: &Path,
    rel: &Path,
    out: &mut Vec<(PathBuf, std::fs::Metadata)>,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(root.join(rel))?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let child = rel.join(e.file_name());
        let md = e.metadata()?;
        let is_dir = md.is_dir();
        out.push((child.clone(), md));
        if is_dir {
            collect_host_tree(root, &child, out)?;
        }
    }
    Ok(())
}

/// `chown -Rh 0:0 /`: everything in the volume, symlinks included.
fn chown_all_root(v: &mut MutVolume) -> Result<()> {
    let mut stack = vec![(String::new(), ROOT_DIR_INO)];
    while let Some((dir, ino)) = stack.pop() {
        for de in v.read_dir(ino)? {
            let path = format!("{dir}/{}", de.name);
            let i = v.inode(de.ino)?;
            if i.uid != 0 || i.gid != 0 {
                v.set_attrs(&path, &Attrs { uid: Some(0), gid: Some(0), ..Default::default() })?;
                v.commit_if_large()?;
            }
            if i.mode & S_IFMT == S_IFDIR {
                stack.push((path, de.ino));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        assert_eq!(unhex(&hex(b"\x00\xffab")).unwrap(), b"\x00\xffab");
        assert!(unhex("0").is_err());
    }

    #[test]
    fn meta_line_parses_extract_output() {
        let mut m = MetaOverride::default();
        apply_meta_line(
            &mut m,
            r#"{"path":"/usr/bin/x","mode":493,"uid":0,"gid":80,"flags":2,"mtime_ns":5,"xattrs":[{"name":"a","hex":"00ff"}]}"#,
        )
        .unwrap();
        let a = m.entries["usr/bin/x"];
        assert_eq!((a.mode, a.uid, a.gid, a.mtime_ns, a.bsd_flags), (Some(493), Some(0), Some(80), Some(5), Some(2)));
        assert_eq!(m.xattrs["usr/bin/x"], vec![("a".to_string(), vec![0, 255])]);
    }
}
