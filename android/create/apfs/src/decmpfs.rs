//! Transparent compression (decmpfs). The `cmpfs` crate decodes every
//! algorithm except LZBITMAP, which it reports as unsupported and which this
//! file decodes itself (types 13 and 14, written from the format as documented
//! by the MIT-licensed libzbitmap).

use crate::error::{corrupt, Error, Result};
use crate::raw::*;

const CHUNK: usize = 0x10000;

/// The header at the start of a decmpfs xattr.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub algo: u32,
    pub size: u64,
}

pub fn parse_header(x: &[u8]) -> Result<Header> {
    if x.len() < 16 || &x[0..4] != b"fpmc" {
        return corrupt("invalid decmpfs header");
    }
    Ok(Header { algo: le32(x, 4), size: le64(x, 8) })
}

/// Decompresses a whole file. `attr` is the decmpfs xattr value and `rsrc`
/// gives the resource-fork bytes when the algorithm needs them.
pub fn decompress(attr: &[u8], rsrc: Option<&[u8]>) -> Result<Vec<u8>> {
    let hdr = parse_header(attr)?;
    match hdr.algo {
        13 => lzbitmap_block(&attr[16..], hdr.size as usize),
        14 => {
            let rsrc = rsrc.ok_or_else(|| Error::Corrupt("compressed file has no resource fork".into()))?;
            lzbitmap_rsrc(rsrc, hdr.size as usize)
        }
        _ => cmpfs::decompress(attr, rsrc).map_err(|e| Error::Corrupt(format!("decmpfs: {e}"))),
    }
}

fn lzbitmap_rsrc(rsrc: &[u8], total: usize) -> Result<Vec<u8>> {
    let chunks = total.div_ceil(CHUNK);
    if rsrc.len() < (chunks + 1) * 4 {
        return corrupt("resource fork offset table truncated");
    }
    let mut out = Vec::with_capacity(total);
    for b in 0..chunks {
        let (off, end) = (le32(rsrc, b * 4) as usize, le32(rsrc, (b + 1) * 4) as usize);
        if off > end || end > rsrc.len() {
            return corrupt("compressed block outside the resource fork");
        }
        let want = (total - b * CHUNK).min(CHUNK);
        out.extend_from_slice(&lzbitmap_block(&rsrc[off..end], want)?);
    }
    if out.len() != total {
        return corrupt("LZBITMAP produced the wrong size");
    }
    Ok(out)
}

fn lzbitmap_block(data: &[u8], want: usize) -> Result<Vec<u8>> {
    match data.first() {
        None => Ok(Vec::new()),
        Some(0x5a) => lzbitmap(data, want),
        Some(b) if b & 0x0f == 0x0f => Ok(data[1..].to_vec()),
        Some(_) => corrupt("lzbitmap block has an unknown marker"),
    }
}

const ZBM_BITMAP_COUNT: usize = 12;
const ZBM_BITMAP_BASE: u8 = 3;
const ZBM_BITMAP_BYTECNT: usize = 17;

fn u24(b: &[u8], o: usize) -> usize {
    b[o] as usize | (b[o + 1] as usize) << 8 | (b[o + 2] as usize) << 16
}

#[derive(Clone, Copy, Default)]
struct ZbmBmap {
    bitmap: u8,
    period_bytes: u8,
}

struct ZbmChunk<'a> {
    src: &'a [u8],
    meta1: usize,
    meta2: usize,
    meta3: usize,
    nibble: bool,
    data: usize,
    period: usize,
    written: usize,
    decmp_len: usize,
    bitmaps: [ZbmBmap; ZBM_BITMAP_COUNT],
}

pub fn lzbitmap(src: &[u8], want: usize) -> Result<Vec<u8>> {
    if src.len() < 4 || &src[..4] != b"ZBM\x09" {
        return corrupt("bad LZBITMAP magic");
    }
    let mut out = Vec::with_capacity(want);
    let mut pos = 4usize;
    loop {
        let h = src
            .get(pos..pos + 6)
            .ok_or_else(|| Error::Corrupt("truncated LZBITMAP chunk".to_string()))?;
        let (len, decmp_len) = (u24(h, 0), u24(h, 3));
        if len == 0 || pos + len > src.len() {
            return corrupt("LZBITMAP chunk out of range");
        }
        if decmp_len == 0 {
            return Ok(out);
        }
        if decmp_len > 0x8000 {
            return corrupt("LZBITMAP chunk too large");
        }
        let chunk = &src[pos..pos + len];
        if len == decmp_len + 6 {
            out.extend_from_slice(&chunk[6..6 + decmp_len]);
        } else {
            zbm_compressed(chunk, decmp_len, &mut out)?;
        }
        pos += len;
    }
}

fn zbm_compressed(c: &[u8], decmp_len: usize, out: &mut Vec<u8>) -> Result<()> {
    if c.len() < 15 + ZBM_BITMAP_BYTECNT {
        return corrupt("short LZBITMAP chunk");
    }
    let (m1, m2, m3) = (u24(c, 6), u24(c, 9), u24(c, 12));
    if m1 >= c.len() || m2 >= c.len() || m3 >= c.len() {
        return corrupt("bad LZBITMAP metadata offsets");
    }
    let mut st = ZbmChunk {
        src: c,
        meta1: m1,
        meta2: m2,
        meta3: m3,
        nibble: false,
        data: 15,
        period: 8,
        written: 0,
        decmp_len,
        bitmaps: [ZbmBmap::default(); ZBM_BITMAP_COUNT],
    };
    // The 12 bitmaps are packed 10 bits each at the end of the chunk.
    let mut p = (c.len() - ZBM_BITMAP_BYTECNT) * 8;
    let bit = |p: &mut usize| -> u8 {
        let v = (c[*p / 8] >> (*p % 8)) & 1;
        *p += 1;
        v
    };
    for b in st.bitmaps.iter_mut() {
        for i in 0..8 {
            b.bitmap |= bit(&mut p) << i;
        }
        for i in 0..2 {
            b.period_bytes |= bit(&mut p) << i;
        }
        if b.period_bytes > 2 {
            return corrupt("bad LZBITMAP bitmap");
        }
    }
    while st.written < decmp_len {
        let n = zbm_nibble(&mut st)?;
        let mut repeat = 1u32;
        if st.decmp_len - st.written > 8 {
            let next = zbm_nibble(&mut st)?;
            if next == 0xf {
                let mut total = 4u32;
                let mut cur = next;
                while cur == 0xf {
                    cur = zbm_nibble(&mut st)?;
                    total += cur as u32;
                    if total > 0xffff {
                        return corrupt("LZBITMAP repeat count overflow");
                    }
                }
                repeat = total;
            } else {
                // Not a repetition: give the nibble back.
                st.nibble = !st.nibble;
                if st.nibble {
                    st.meta3 -= 1;
                }
            }
        }
        for _ in 0..repeat {
            zbm_apply(&mut st, n, out)?;
        }
    }
    Ok(())
}

fn zbm_nibble(st: &mut ZbmChunk) -> Result<u8> {
    let b = *st
        .src
        .get(st.meta3)
        .ok_or_else(|| Error::Corrupt("LZBITMAP nibble area overrun".to_string()))?;
    Ok(if !st.nibble {
        st.nibble = true;
        b & 0xf
    } else {
        st.nibble = false;
        st.meta3 += 1;
        b >> 4
    })
}

fn zbm_apply(st: &mut ZbmChunk, n: u8, out: &mut Vec<u8>) -> Result<()> {
    let overrun = || Error::Corrupt("LZBITMAP metadata overrun".to_string());
    let bmap = if n > 2 {
        if n == 0xf {
            return corrupt("LZBITMAP: invalid bitmap number");
        }
        st.bitmaps[(n - ZBM_BITMAP_BASE) as usize]
    } else {
        let b = *st.src.get(st.meta2).ok_or_else(overrun)?;
        st.meta2 += 1;
        ZbmBmap { bitmap: b, period_bytes: n }
    };
    if bmap.period_bytes != 0 {
        st.period = 0;
        for i in 0..bmap.period_bytes as usize {
            st.period |= (*st.src.get(st.meta1).ok_or_else(overrun)? as usize) << (8 * i);
            st.meta1 += 1;
        }
    }
    if st.period == 0 {
        return corrupt("LZBITMAP period is zero");
    }
    for i in 0..8 {
        if st.written == st.decmp_len {
            break;
        }
        if bmap.bitmap & (1 << i) != 0 {
            let b = *st
                .src
                .get(st.data)
                .ok_or_else(|| Error::Corrupt("LZBITMAP data overrun".to_string()))?;
            out.push(b);
            st.data += 1;
        } else {
            if st.period > out.len() {
                return corrupt("LZBITMAP reference before start");
            }
            let b = out[out.len() - st.period];
            out.push(b);
        }
        st.written += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_zlib_goes_through_cmpfs() {
        let payload = b"hello hello hello hello hello";
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(payload, 6);
        let mut attr = b"fpmc".to_vec();
        attr.extend_from_slice(&3u32.to_le_bytes());
        attr.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        attr.extend_from_slice(&compressed);
        assert_eq!(decompress(&attr, None).unwrap(), payload);
    }

    #[test]
    fn lzbitmap_uncompressed_chunk() {
        let mut src = b"ZBM\x09".to_vec();
        let body = b"abcd";
        src.extend_from_slice(&((body.len() + 6) as u32).to_le_bytes()[..3]);
        src.extend_from_slice(&(body.len() as u32).to_le_bytes()[..3]);
        src.extend_from_slice(body);
        src.extend_from_slice(&[6, 0, 0, 0, 0, 0]);
        assert_eq!(lzbitmap(&src, 4).unwrap(), body);
    }

    #[test]
    fn lzbitmap_rejects_bad_magic() {
        assert!(lzbitmap(b"nope", 4).is_err());
    }
}
