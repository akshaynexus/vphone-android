//! IM4P handling: retyping for manifest digests (idevicerestore's behaviour) and
//! repacking the restore ramdisk around its modified APFS payload.

use crate::bail;
use crate::error::{Error, Result};
use sha2::{Digest, Sha384};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const IM4P_MARKER: &[u8] = b"\x16\x04IM4P";

/// The retype tag idevicerestore applies when sending a component (img4.c's
/// component tag table). `None` for components it does not retype.
pub fn retype_tag(key: &str) -> Option<&'static [u8; 4]> {
    match key {
        "RestoreKernelCache" => Some(b"rkrn"),
        "RestoreRamDisk" => Some(b"rdsk"),
        "RestoreTrustCache" => Some(b"rtsc"),
        "Ap,RestoreTrustedExecutionMonitor" => Some(b"rtrx"),
        _ => None,
    }
}

/// SHA-384 of an IM4P file after retyping its payload tag, as `tools/restore-tree-mode.sh` computes it.
pub fn retyped_digest(path: &Path, tag: &[u8; 4]) -> Result<[u8; 48]> {
    let mut file = File::open(path).map_err(|e| Error::path(path, e))?;
    let mut head = [0u8; 32];
    let head_len = read_up_to(&mut file, &mut head)?;
    let head = &head[..head_len];
    let Some(i) = head
        .windows(IM4P_MARKER.len())
        .position(|w| w == IM4P_MARKER)
    else {
        bail!("{}: not an IM4P", path.display());
    };
    if head.len() < i + 12 || &head[i + 6..i + 8] != b"\x16\x04" {
        bail!("{}: not an IM4P", path.display());
    }
    let mut hasher = Sha384::new();
    hasher.update(&head[..i + 8]);
    hasher.update(tag);
    hasher.update(&head[i + 12..]);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| Error::path(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Where the APFS payload sits inside the stock ramdisk IM4P: tools/ssh-ramdisk-repack.sh's
/// checks of the header layout, returning the offset and length.
pub fn rdsk_payload(header: &[u8], file_len: u64) -> Result<(usize, usize)> {
    if header.len() < 27 {
        bail!("ramdisk IM4P is truncated");
    }
    if header[0] != 0x30 || header[1] != 0x84 {
        bail!("ramdisk: not a DER SEQUENCE");
    }
    if &header[6..12] != b"\x16\x04IM4P" {
        bail!("ramdisk: not IM4P");
    }
    if &header[12..18] != b"\x16\x04rdsk" {
        bail!("ramdisk: not rdsk");
    }
    if &header[18..21] != b"\x16\x010" {
        bail!("ramdisk: bad description");
    }
    if header[21] != 0x04 || header[22] != 0x84 {
        bail!("ramdisk: not OCTET STRING");
    }
    let plen = u32::from_be_bytes(header[23..27].try_into().unwrap()) as usize;
    if 27 + plen as u64 != file_len {
        bail!("ramdisk: payload length does not match file");
    }
    Ok((27, plen))
}

/// Splits the stock ramdisk's APFS payload to `out` (the `tail -c +28` of
/// ssh-ramdisk-rebuild-signed.sh, with the header checks of ssh-ramdisk-repack.sh).
pub fn extract_rdsk_payload(stock: &Path, out: &Path) -> Result<()> {
    let mut input = File::open(stock).map_err(|e| Error::path(stock, e))?;
    let len = input.metadata().map_err(|e| Error::path(stock, e))?.len();
    let mut head = [0u8; 27];
    input.read_exact(&mut head).map_err(|e| Error::path(stock, e))?;
    let (offset, plen) = rdsk_payload(&head, len)?;
    let tmp = out.with_extension("part");
    {
        let mut o = File::create(&tmp).map_err(|e| Error::path(&tmp, e))?;
        input
            .seek(SeekFrom::Start(offset as u64))
            .map_err(|e| Error::path(stock, e))?;
        std::io::copy(&mut (&mut input).take(plen as u64), &mut o).map_err(|e| Error::path(&tmp, e))?;
    }
    fs::rename(&tmp, out).map_err(|e| Error::path(out, e))
}

/// Rebuild the ramdisk IM4P with `payload` (the same length as the stock one), written to `out`.
pub fn repack_rdsk(stock: &Path, payload: &Path, out: &Path) -> Result<()> {
    let mut input = File::open(stock).map_err(|e| Error::path(stock, e))?;
    let len = input.metadata().map_err(|e| Error::path(stock, e))?.len();
    let mut head = [0u8; 27];
    input.read_exact(&mut head).map_err(|e| Error::path(stock, e))?;
    let (offset, plen) = rdsk_payload(&head, len)?;
    let plen_u64 = plen as u64;
    let payload_len = fs::metadata(payload).map_err(|e| Error::path(payload, e))?.len();
    if payload_len != plen_u64 {
        bail!("payload size changed: {payload_len} != {plen_u64}");
    }
    let mut pfile = File::open(payload).map_err(|e| Error::path(payload, e))?;
    let mut magic = [0u8; 36];
    pfile.read_exact(&mut magic).map_err(|e| Error::path(payload, e))?;
    if &magic[32..36] != b"NXSB" {
        bail!("payload does not look like an APFS container");
    }
    pfile.seek(SeekFrom::Start(0)).map_err(|e| Error::path(payload, e))?;
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).map_err(|e| Error::path(dir, e))?;
    }
    let tmp = out.with_extension("part");
    {
        let mut o = File::create(&tmp).map_err(|e| Error::path(&tmp, e))?;
        o.write_all(&head[..offset]).map_err(|e| Error::path(&tmp, e))?;
        std::io::copy(&mut pfile, &mut o).map_err(|e| Error::path(&tmp, e))?;
        o.sync_all().map_err(|e| Error::path(&tmp, e))?;
    }
    fs::rename(&tmp, out).map_err(|e| Error::path(out, e))
}
