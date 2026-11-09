//! The small amount of DER the IM4P and IMG4 containers need.

use crate::bail;
use crate::error::Result;

/// One element: its tag and the byte range of its content, plus the offset just past it.
#[derive(Clone, Copy, Debug)]
pub struct Tlv {
    pub tag: u8,
    pub start: usize,
    pub end: usize,
}

/// Parse the DER element at `off`.
pub fn tlv(data: &[u8], off: usize) -> Result<Tlv> {
    if off + 2 > data.len() {
        bail!("DER element at {off} is truncated");
    }
    let tag = data[off];
    let mut p = off + 1;
    let mut len = data[p] as usize;
    p += 1;
    if len & 0x80 != 0 {
        let count = len & 0x7F;
        if count == 0 || count > 8 || p + count > data.len() {
            bail!("DER length at {off} is invalid");
        }
        len = data[p..p + count]
            .iter()
            .fold(0usize, |n, &b| (n << 8) | b as usize);
        p += count;
    }
    if p + len > data.len() {
        bail!("DER element at {off} runs past the data");
    }
    Ok(Tlv {
        tag,
        start: p,
        end: p + len,
    })
}

/// A short-form-where-possible definite DER length, as the Python tools wrote it.
pub fn encode_len(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.extend_from_slice(&[0x81, len as u8]);
    } else if len < 0x10000 {
        out.push(0x82);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x83);
        out.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
    }
}

pub fn encode(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 5);
    out.push(tag);
    encode_len(value.len(), &mut out);
    out.extend_from_slice(value);
    out
}
