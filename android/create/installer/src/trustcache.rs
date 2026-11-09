//! Port of tools/ssh-ramdisk-trustcache.py: merge the stock restore trust cache
//! with the cdhashes of every Mach-O in the SSH staging tree.

use apple_codesign::{CodeSigningSlot, MachFile};

use crate::bail;
use crate::der;
use crate::error::{Error, Result};
use crate::progress::Progress;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn le32(d: &[u8]) -> u32 {
    u32::from_le_bytes(d[..4].try_into().unwrap())
}

/// The cdhash of every signed slice of a Mach-O (nothing for any other file).
/// SHA-1 CodeDirectories hash to 20 bytes; SHA-256 ones are cut to 20, as in the
/// stock entries.
pub fn macho_cdhashes(data: &[u8], name: &str, progress: &dyn Progress) -> Vec<[u8; 20]> {
    let Ok(file) = MachFile::parse(data) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for macho in file.iter_macho() {
        let Ok(Some(signature)) = macho.code_signature() else {
            continue;
        };
        let Some(blob) = signature.find_slot(CodeSigningSlot::CodeDirectory) else {
            continue;
        };
        let directory = blob.data;
        match directory.get(37) {
            Some(1) => out.push(Sha1::digest(directory).into()),
            Some(2) => {
                let full: [u8; 32] = Sha256::digest(directory).into();
                let mut cut = [0u8; 20];
                cut.copy_from_slice(&full[..20]);
                out.push(cut);
            }
            Some(other) => progress.log(&format!(
                "warn: {name}: CodeDirectory hashType {other} not supported, skipped"
            )),
            None => {}
        }
    }
    out
}

/// What was built.
#[derive(Clone, Copy, Debug)]
pub struct Built {
    pub stock: usize,
    pub added: usize,
    pub total: usize,
}

/// Build `<out>` from the stock rtsc IM4P and the staging tree.
pub fn build(stock: &[u8], tree: &Path, out: &Path, progress: &dyn Progress) -> Result<Built> {
    let outer = der::tlv(stock, 0)?;
    if outer.tag != 0x30 {
        bail!("not a DER SEQUENCE");
    }
    let body = &stock[outer.start..outer.end];
    let mut strings: Vec<&[u8]> = Vec::new();
    let mut octets: Vec<&[u8]> = Vec::new();
    let mut off = 0;
    while off < body.len() {
        let t = der::tlv(body, off)?;
        match t.tag {
            0x16 => strings.push(&body[t.start..t.end]),
            0x04 => octets.push(&body[t.start..t.end]),
            _ => {}
        }
        off = t.end;
    }
    if strings.len() < 3 || octets.is_empty() {
        bail!("unexpected stock rtsc DER shape");
    }
    if strings[0] != b"IM4P" || strings[1] != b"rtsc" {
        bail!("unexpected stock rtsc type");
    }
    let desc = strings[2];
    let payload = octets[0];
    if payload.len() < 24 {
        bail!("unsupported stock rtsc payload");
    }
    let version = le32(payload);
    let uuid = &payload[4..20];
    let count = le32(&payload[20..]) as usize;
    if version != 1 || 24 + count * 22 != payload.len() {
        bail!("unsupported stock rtsc payload");
    }
    let mut entries: BTreeMap<[u8; 20], [u8; 2]> = BTreeMap::new();
    for i in 0..count {
        let e = &payload[24 + i * 22..24 + (i + 1) * 22];
        entries.insert(e[..20].try_into().unwrap(), e[20..22].try_into().unwrap());
    }
    let stock_count = entries.len();
    let mut added = BTreeMap::new();
    walk_tree(tree, progress, &mut |path, data| {
        for cdhash in macho_cdhashes(data, &path.display().to_string(), progress) {
            if !entries.contains_key(&cdhash) {
                added.insert(cdhash, [0x02u8, 0x00]);
            }
        }
    })?;
    let added_count = added.len();
    entries.extend(added);
    let mut new_payload = Vec::with_capacity(24 + entries.len() * 22);
    new_payload.extend_from_slice(&1u32.to_le_bytes());
    new_payload.extend_from_slice(uuid);
    new_payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (cdhash, tail) in &entries {
        new_payload.extend_from_slice(cdhash);
        new_payload.extend_from_slice(tail);
    }
    let mut inner = der::encode(0x16, b"IM4P");
    inner.extend_from_slice(&der::encode(0x16, b"rtsc"));
    inner.extend_from_slice(&der::encode(0x16, desc));
    inner.extend_from_slice(&der::encode(0x04, &new_payload));
    fs::write(out, der::encode(0x30, &inner)).map_err(|e| Error::path(out, e))?;
    Ok(Built {
        stock: stock_count,
        added: added_count,
        total: entries.len(),
    })
}

fn walk_tree(
    root: &Path,
    progress: &dyn Progress,
    f: &mut dyn FnMut(&Path, &[u8]),
) -> Result<()> {
    let rd = fs::read_dir(root).map_err(|e| Error::path(root, e))?;
    let mut names: Vec<_> = rd
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::path(root, e))?;
    names.sort_by_key(|e| e.file_name());
    for entry in names {
        crate::progress::check_cancel(progress)?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| Error::path(&path, e))?;
        if meta.file_type().is_dir() {
            walk_tree(&path, progress, f)?;
        } else if meta.file_type().is_file() {
            let data = fs::read(&path).map_err(|e| Error::path(&path, e))?;
            f(&path, &data);
        }
    }
    Ok(())
}
