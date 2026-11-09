//! Finding the APFS container in a file, and block I/O on it.
//!
//! Handled wrappers: a raw container; an IM4P whose payload is the raw
//! container (restore ramdisks, payload offset 27 or wherever the DER says);
//! a GPT disk with an APFS partition; and UDIF disk images, read-only (the
//! `udif` crate decodes the APFS partition to a temporary file).

use crate::error::{corrupt, Error, Result};
use crate::raw::*;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const APFS_GPT_TYPE: [u8; 16] = [
    0xef, 0x57, 0x34, 0x7c, 0x00, 0x00, 0xaa, 0x11, 0xaa, 0x11, 0x00, 0x30, 0x65, 0x43, 0xec, 0xac,
];

/// An opened image: the one object every other layer reads and writes through.
pub struct Image {
    file: File,
    base: u64,
    writable: bool,
    /// How the container was found, for `info`.
    pub wrapper: &'static str,
    len: u64,
}

impl Image {
    pub fn open(path: &Path, writable: bool) -> Result<Image> {
        let mut file = OpenOptions::new().read(true).write(writable).open(path).map_err(|e| {
            Error::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
        })?;
        let file_len = file.metadata()?.len();
        let (located, wrapper) = locate(&mut file, file_len)?;
        match located {
            Located::Plain(base) => Ok(Image { file, base, writable, wrapper, len: file_len - base }),
            Located::Udif => {
                if writable {
                    return Err(Error::Unsupported(
                        "UDIF images are read-only here; convert to a raw image first".into(),
                    ));
                }
                let raw = extract_udif(path)?;
                let len = raw.metadata()?.len();
                if !is_nx_at(&raw, 0) {
                    return corrupt("the UDIF image has no APFS partition");
                }
                Ok(Image { file: raw, base: 0, writable: false, wrapper, len })
            }
        }
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    /// Size of the container in bytes (for IM4P-wrapped images, of the payload).
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reads exactly `buf.len()` bytes at container-relative `off`.
    pub fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> {
        self.file.read_exact_at(buf, self.base + off).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("reading {} bytes at offset {off:#x}: {e}", buf.len()),
            ))
        })
    }

    pub fn write_at(&self, buf: &[u8], off: u64) -> Result<()> {
        if !self.writable {
            return Err(Error::Invalid("image is not writable".into()));
        }
        self.file.write_all_at(buf, self.base + off)?;
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }
}

enum Located {
    Plain(u64),
    Udif,
}

fn read_exact_at(file: &File, buf: &mut [u8], off: u64) -> Result<()> {
    file.read_exact_at(buf, off).map_err(Error::Io)
}

fn is_nx_at(file: &File, off: u64) -> bool {
    let mut b = [0u8; 4];
    file.read_exact_at(&mut b, off + 32).is_ok() && &b == NX_MAGIC
}

fn locate(file: &mut File, file_len: u64) -> Result<(Located, &'static str)> {
    if file_len >= 40 && is_nx_at(file, 0) {
        return Ok((Located::Plain(0), "raw"));
    }
    if let Some(off) = im4p_payload_offset(file, file_len)? {
        if is_nx_at(file, off) {
            return Ok((Located::Plain(off), "im4p"));
        }
    }
    if let Some(off) = gpt_apfs_partition(file, file_len)? {
        return Ok((Located::Plain(off), "gpt"));
    }
    if file_len >= 512 {
        let mut trailer = [0u8; 512];
        read_exact_at(file, &mut trailer, file_len - 512)?;
        if &trailer[0..4] == b"koly" {
            return Ok((Located::Udif, "udif"));
        }
    }
    corrupt("no APFS container found (not raw APFS, IM4P, GPT or UDIF)")
}

/// DER walker for `30 <len> 16 04 'IM4P' 16 04 <tag> 16 <desc> 04 <len> payload`.
fn im4p_payload_offset(file: &File, file_len: u64) -> Result<Option<u64>> {
    if file_len < 32 {
        return Ok(None);
    }
    let mut head = [0u8; 512];
    let n = head.len().min(file_len as usize);
    read_exact_at(file, &mut head[..n], 0)?;
    let h = &head[..n];
    if h[0] != 0x30 {
        return Ok(None);
    }
    let mut pos = 1usize;
    let (_, adv) = match der_len(&h[pos..]) {
        Some(x) => x,
        None => return Ok(None),
    };
    pos += adv;
    let mut items = 0;
    while pos < h.len() && items < 4 {
        let tag = h[pos];
        let (len, adv) = match der_len(h.get(pos + 1..).unwrap_or(&[])) {
            Some(x) => x,
            None => return Ok(None),
        };
        let data = pos + 1 + adv;
        if tag == 0x16 && items == 0 {
            if data + len > h.len() || &h[data..data + len] != b"IM4P" {
                return Ok(None);
            }
        } else if tag == 0x04 {
            return Ok(Some(data as u64));
        }
        pos = data + len;
        items += 1;
    }
    Ok(None)
}

fn der_len(b: &[u8]) -> Option<(usize, usize)> {
    let first = *b.first()?;
    if first < 0x80 {
        return Some((first as usize, 1));
    }
    let n = (first & 0x7f) as usize;
    if n == 0 || n > 4 || b.len() < 1 + n {
        return None;
    }
    let mut v = 0usize;
    for &x in &b[1..1 + n] {
        v = (v << 8) | x as usize;
    }
    Some((v, 1 + n))
}

fn gpt_apfs_partition(file: &File, file_len: u64) -> Result<Option<u64>> {
    for ssz in [512u64, 4096] {
        if file_len < ssz * 2 + 92 {
            continue;
        }
        let mut hdr = [0u8; 92];
        read_exact_at(file, &mut hdr, ssz)?;
        if &hdr[0..8] != b"EFI PART" {
            continue;
        }
        let entries_lba = le64(&hdr, 72);
        let n = le32(&hdr, 80) as u64;
        let esize = le32(&hdr, 84) as u64;
        if !(128..=4096).contains(&esize) || n > 16384 {
            return corrupt("implausible GPT partition table");
        }
        let mut table = vec![0u8; (n * esize) as usize];
        read_exact_at(file, &mut table, entries_lba * ssz)?;
        for e in table.chunks(esize as usize) {
            if e[..16] == APFS_GPT_TYPE {
                let off = le64(e, 32) * ssz;
                if is_nx_at(file, off) {
                    return Ok(Some(off));
                }
            }
        }
    }
    Ok(None)
}

/// Decodes a UDIF image's APFS partition into an unnamed temporary file
/// (in `TMPDIR`; the Android app points that at its cache directory). The
/// space it uses is released as soon as the image is closed.
fn extract_udif(path: &Path) -> Result<File> {
    let mut dmg = udif::DmgArchive::open(path)
        .map_err(|e| Error::Corrupt(format!("UDIF: {e}")))?;
    let tmp = std::env::temp_dir().join(format!(
        ".vphone-apfs-{}-{:x}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?;
    std::fs::remove_file(&tmp)?;
    dmg.extract_main_partition_to(&mut file)
        .map_err(|e| Error::Corrupt(format!("UDIF: {e}")))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths() {
        assert_eq!(der_len(&[0x20]), Some((0x20, 1)));
        assert_eq!(der_len(&[0x84, 0, 0, 1, 2]), Some((0x102, 5)));
        assert_eq!(der_len(&[0x81]), None);
    }
}
