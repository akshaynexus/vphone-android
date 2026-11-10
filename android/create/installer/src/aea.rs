//! Apple Encrypted Archive (AEA1) decryption for the firmware's `.dmg.aea` images.
//!
//! Profile 1 only (symmetric key, unsigned), the profile of every IPSW image. The layout and
//! key schedule are the ones vphone-cli's `fw aea-decrypt` follows (blacktop/ipsw pkg/aea):
//!
//! ```text
//! "AEA1" | u32 profile (low 24 bits) | u32 auth data size | auth data
//! 32-byte main salt
//! root header: HMAC(32) | encrypted root header (48) | first cluster's HMAC(32)
//! clusters: encrypted segment headers | next cluster's HMAC | segment HMACs | segment data...
//! ```
//!
//! Keys are HKDF-SHA256: the main key from the archive key (`AEA_AMK` + profile word, salted with
//! the main salt), then the root header key (`AEA_RHEK`), each cluster's key (`AEA_CK` + index),
//! its header key (`AEA_CHEK`) and each segment's key (`AEA_SK` + index). A header key is 80
//! bytes: HMAC key, AES-256 key, CTR IV. Every HMAC is HMAC-SHA256(salt | data | u64 salt length).
//!
//! The archive is read once, in order, so it can come straight out of the IPSW; the segments
//! (authenticate, decrypt, decompress, checksum, write) are independent and run on a thread pool.

use crate::bail;
use crate::error::{Error, Result};
use crate::progress::Progress;
use aes::cipher::{KeyIvInit, StreamCipher};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rayon::iter::{ParallelBridge, ParallelIterator};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// The field of the auth data that holds the archive's own key, as hex.
pub const KEY_FIELD: &str = "encryption_key";

/// The start of an archive: what comes before the main salt.
pub struct Prologue {
    profile_word: u32,
    auth_data: Vec<u8>,
}

fn read_exact(reader: &mut dyn Read, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .map_err(|e| if e.kind() == std::io::ErrorKind::UnexpectedEof { Error::msg("the archive is truncated") } else { Error::io("reading the archive", e) })?;
    Ok(buf)
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().expect("four bytes"))
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().expect("eight bytes"))
}

impl Prologue {
    pub fn read(reader: &mut dyn Read) -> Result<Prologue> {
        let header = read_exact(reader, 12)?;
        if &header[..4] != b"AEA1" {
            bail!("not an AEA1 archive");
        }
        let profile_word = u32_at(&header, 4);
        if profile_word & 0xff_ffff != 1 {
            bail!("AEA profile {} is not supported (only 1, symmetric key)", profile_word & 0xff_ffff);
        }
        let auth_size = u32_at(&header, 8) as usize;
        if auth_size > 16 << 20 {
            bail!("the archive's auth data is {auth_size} bytes, which no firmware image has");
        }
        Ok(Prologue { profile_word, auth_data: read_exact(reader, auth_size)? })
    }

    /// The auth data's fields: each is a u32 size (of itself), then `key\0value`.
    pub fn fields(&self) -> Vec<(String, &[u8])> {
        let mut out = Vec::new();
        let mut rest = &self.auth_data[..];
        while rest.len() >= 4 {
            let size = u32_at(rest, 0) as usize;
            if size < 4 || size > rest.len() {
                break;
            }
            let field = &rest[4..size];
            if let Some(nul) = field.iter().position(|b| *b == 0) {
                out.push((String::from_utf8_lossy(&field[..nul]).into_owned(), &field[nul + 1..]));
            }
            rest = &rest[size..];
        }
        out
    }

    /// The archive's key, when its header carries one (`encryption_key`, 64 hex digits).
    /// The root header's HMAC decides whether it is the right one, in `decrypt`.
    pub fn embedded_key(&self) -> Option<[u8; 32]> {
        let (_, value) = self.fields().into_iter().find(|(name, _)| name == KEY_FIELD)?;
        let text = std::str::from_utf8(value).ok()?.trim_end_matches('\0').trim();
        let bytes = hex::decode(text).ok()?;
        bytes.try_into().ok()
    }
}

/// HMAC key, AES-256 key and CTR IV.
struct HeaderKey([u8; 80]);

impl HeaderKey {
    fn derive(key: &[u8], info: &[u8]) -> HeaderKey {
        let mut out = [0u8; 80];
        derive(key, None, info, &mut out);
        HeaderKey(out)
    }

    fn verify(&self, data: &[u8], salt: &[&[u8]], expected: &[u8], what: impl FnOnce() -> String) -> Result<()> {
        let mut mac = HmacSha256::new_from_slice(&self.0[..32]).expect("any key length");
        let mut salt_len = 0u64;
        for part in salt {
            mac.update(part);
            salt_len += part.len() as u64;
        }
        mac.update(data);
        mac.update(&salt_len.to_le_bytes());
        mac.verify_slice(expected)
            .map_err(|_| Error::msg(format!("{} failed authentication (wrong key or damaged archive)", what())))
    }

    fn decrypt(&self, data: &mut [u8]) {
        let mut cipher = Aes256Ctr::new_from_slices(&self.0[32..64], &self.0[64..80]).expect("key and IV sizes");
        cipher.apply_keystream(data);
    }
}

fn derive(key: &[u8], salt: Option<&[u8]>, info: &[u8], out: &mut [u8]) {
    Hkdf::<Sha256>::new(salt, key).expand(info, out).expect("an HKDF output this short");
}

fn info(label: &str, value: Option<u32>) -> Vec<u8> {
    let mut out = label.as_bytes().to_vec();
    if let Some(v) = value {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// MurmurHash64A (Austin Appleby), AEA's "murmur" segment checksum.
fn murmur64(data: &[u8]) -> u64 {
    const M: u64 = 0xC6A4_A793_5BD1_E995;
    const R: u32 = 47;
    let mut h = 0xE223_6FDC_26A5_F6D2u64 ^ (data.len() as u64).wrapping_mul(M);
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let mut k = u64::from_le_bytes(chunk.try_into().expect("eight bytes"));
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h ^= k;
        h = h.wrapping_mul(M);
    }
    let tail = chunks.remainder();
    if !tail.is_empty() {
        let mut t = 0u64;
        for byte in tail.iter().rev() {
            t = (t << 8) | u64::from(*byte);
        }
        h ^= t;
        h = h.wrapping_mul(M);
    }
    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^ (h >> R)
}

#[derive(Clone, Copy, PartialEq)]
enum Compression {
    None,
    Lzfse,
    Zlib,
}

#[derive(Clone, Copy)]
enum Checksum {
    None,
    Murmur,
    Sha256,
}

impl Checksum {
    fn size(self) -> usize {
        match self {
            Checksum::None => 0,
            Checksum::Murmur => 8,
            Checksum::Sha256 => 32,
        }
    }
}

/// One segment as it lies in the archive, with what its cluster's header said of it.
struct Segment {
    cluster: u32,
    number: u32,
    cluster_key: [u8; 32],
    mac: [u8; 32],
    checksum: Vec<u8>,
    plain_size: usize,
    stored: Vec<u8>,
}

/// Reads the archive in order: verifies each cluster's headers and hands out its segments.
struct Segments<'r> {
    reader: &'r mut (dyn Read + Send),
    main_key: [u8; 32],
    cluster_mac: Vec<u8>,
    per_cluster: usize,
    checksum: Checksum,
    cluster: u32,
    /// The current cluster's decrypted headers, its segment MACs and the next segment in it.
    headers: Vec<u8>,
    macs: Vec<u8>,
    cluster_key: [u8; 32],
    next: usize,
    left: u64,
    failed: bool,
    read_bytes: &'r AtomicU64,
}

impl Segments<'_> {
    fn open_cluster(&mut self) -> Result<()> {
        derive(&self.main_key, None, &info("AEA_CK", Some(self.cluster)), &mut self.cluster_key);
        let header_key = HeaderKey::derive(&self.cluster_key, b"AEA_CHEK");
        let header_size = (8 + self.checksum.size()) * self.per_cluster;
        let mut headers = read_exact(self.reader, header_size)?;
        let next_mac = read_exact(self.reader, 32)?;
        let macs = read_exact(self.reader, 32 * self.per_cluster)?;
        let cluster = self.cluster;
        header_key.verify(&headers, &[&next_mac, &macs], &self.cluster_mac, || format!("cluster {cluster}'s headers"))?;
        header_key.decrypt(&mut headers);
        self.read_bytes.fetch_add((header_size + 32 + macs.len()) as u64, Ordering::Relaxed);
        self.headers = headers;
        self.macs = macs;
        self.cluster_mac = next_mac;
        self.next = 0;
        Ok(())
    }

    fn read_segment(&mut self) -> Result<Option<Segment>> {
        loop {
            if self.left == 0 {
                return Ok(None);
            }
            if self.next == self.per_cluster || self.headers.is_empty() {
                if !self.headers.is_empty() {
                    self.cluster += 1;
                }
                self.open_cluster()?;
            }
            let number = self.next;
            self.next += 1;
            let entry = 8 + self.checksum.size();
            let at = number * entry;
            let plain_size = u32_at(&self.headers, at) as usize;
            let stored_size = u32_at(&self.headers, at + 4) as usize;
            if plain_size == 0 {
                continue;
            }
            if stored_size > plain_size.max(1 << 16) * 2 {
                bail!("cluster {} segment {number} claims {stored_size} stored bytes", self.cluster);
            }
            let stored = read_exact(self.reader, stored_size)?;
            self.read_bytes.fetch_add(stored_size as u64, Ordering::Relaxed);
            self.left = self.left.saturating_sub(plain_size as u64);
            return Ok(Some(Segment {
                cluster: self.cluster,
                number: number as u32,
                cluster_key: self.cluster_key,
                mac: self.macs[32 * number..32 * number + 32].try_into().expect("32 bytes"),
                checksum: self.headers[at + 8..at + entry].to_vec(),
                plain_size,
                stored,
            }));
        }
    }
}

impl Iterator for Segments<'_> {
    type Item = Result<Segment>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let item = self.read_segment();
        self.failed = item.is_err();
        item.transpose()
    }
}

/// What `decrypt` did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Decrypted {
    /// The size of the plain image.
    pub size: u64,
    /// Bytes of it that were written; all-zero segments are left as holes.
    pub written: u64,
    pub threads: usize,
}

/// How many threads a decrypt uses on this machine: every core up to eight. The one reader
/// feeding them is the limit beyond that (measured on 20 cores: 2.25 s on one thread, 0.38 s on
/// eight, 0.31 s on twelve, 0.37 s on twenty).
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).min(8)
}

/// Decrypts the archive `reader` is at the start of (its `prologue` already read) into `output`,
/// which is truncated first. `archive_size` is only for the progress reported.
pub fn decrypt(
    prologue: &Prologue,
    reader: &mut (dyn Read + Send),
    archive_size: u64,
    key: &[u8; 32],
    output: &File,
    threads: usize,
    progress: &dyn Progress,
    message: &str,
) -> Result<Decrypted> {
    let main_salt = read_exact(reader, 32)?;
    let mut main_key = [0u8; 32];
    derive(key, Some(&main_salt), &info("AEA_AMK", Some(prologue.profile_word)), &mut main_key);

    let root_mac = read_exact(reader, 32)?;
    let mut root = read_exact(reader, 48)?;
    let cluster_mac = read_exact(reader, 32)?;
    let root_key = HeaderKey::derive(&main_key, b"AEA_RHEK");
    root_key.verify(&root, &[&cluster_mac, &prologue.auth_data], &root_mac, || "the root header".to_owned())?;
    root_key.decrypt(&mut root);
    let size = u64_at(&root, 0);
    let segment_size = u32_at(&root, 16) as usize;
    let per_cluster = u32_at(&root, 20) as usize;
    if segment_size == 0 || segment_size > 64 << 20 || per_cluster == 0 || per_cluster > 1 << 16 {
        bail!("the archive's root header is not one of a firmware image ({segment_size}-byte segments, {per_cluster} a cluster)");
    }
    let compression = match root[24] {
        b'-' => Compression::None,
        b'e' => Compression::Lzfse,
        b'z' => Compression::Zlib,
        other => bail!("segment compression '{}' is not supported", other as char),
    };
    let checksum = match root[25] {
        0 => Checksum::None,
        1 => Checksum::Murmur,
        2 => Checksum::Sha256,
        other => bail!("segment checksum type {other} is not supported"),
    };

    output.set_len(0).map_err(|e| Error::io("the decrypted image", e))?;
    output.set_len(size).map_err(|e| Error::io("the decrypted image", e))?;

    let read_bytes = AtomicU64::new(12 + prologue.auth_data.len() as u64 + 32 + 112);
    let written = AtomicU64::new(0);
    let produced = AtomicU64::new(0);
    let segments = Segments {
        reader,
        main_key,
        cluster_mac,
        per_cluster,
        checksum,
        cluster: 0,
        headers: Vec::new(),
        macs: Vec::new(),
        cluster_key: [0; 32],
        next: 0,
        left: size,
        failed: false,
        read_bytes: &read_bytes,
    };
    let threads = threads.max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("aea-{i}"))
        .build()
        .map_err(|e| Error::msg(format!("the decrypt threads did not start: {e}")))?;
    pool.install(|| {
        segments.par_bridge().try_for_each(|segment| -> Result<()> {
            let mut segment = segment?;
            if progress.cancelled() {
                return Err(Error::Cancelled);
            }
            let (cluster, number) = (segment.cluster, segment.number);
            let key = HeaderKey::derive(&segment.cluster_key, &info("AEA_SK", Some(number)));
            key.verify(&segment.stored, &[], &segment.mac, || format!("cluster {cluster} segment {number}"))?;
            key.decrypt(&mut segment.stored);
            let plain = if segment.stored.len() == segment.plain_size || compression == Compression::None {
                segment.stored
            } else {
                let mut plain = Vec::with_capacity(segment.plain_size);
                match compression {
                    Compression::Lzfse => {
                        lzfse_rust::decode_bytes(&segment.stored, &mut plain)
                            .map_err(|e| Error::msg(format!("cluster {cluster} segment {number} did not decompress: {e}")))?;
                    }
                    Compression::Zlib => {
                        flate2::read::ZlibDecoder::new(&segment.stored[..])
                            .read_to_end(&mut plain)
                            .map_err(|e| Error::msg(format!("cluster {cluster} segment {number} did not decompress: {e}")))?;
                    }
                    Compression::None => unreachable!("handled above"),
                }
                plain
            };
            if plain.len() != segment.plain_size {
                bail!("cluster {cluster} segment {number} is {} bytes, its header says {}", plain.len(), segment.plain_size);
            }
            let good = match checksum {
                Checksum::None => true,
                Checksum::Murmur => murmur64(&plain).to_le_bytes()[..] == segment.checksum[..],
                Checksum::Sha256 => Sha256::digest(&plain)[..] == segment.checksum[..],
            };
            if !good {
                bail!("cluster {cluster} segment {number} checksum mismatch");
            }
            let offset = (u64::from(cluster) * per_cluster as u64 + u64::from(number)) * segment_size as u64;
            if offset + plain.len() as u64 > size {
                bail!("cluster {cluster} segment {number} lies past the image's end");
            }
            // A segment of zeros stays a hole: the image is mostly free space.
            if plain.iter().any(|b| *b != 0) {
                output.write_all_at(&plain, offset).map_err(|e| Error::io("the decrypted image", e))?;
                written.fetch_add(plain.len() as u64, Ordering::Relaxed);
            }
            produced.fetch_add(plain.len() as u64, Ordering::Relaxed);
            progress.progress(read_bytes.load(Ordering::Relaxed).min(archive_size), archive_size, message);
            Ok(())
        })
    })?;
    let produced = produced.into_inner();
    if produced != size {
        bail!("decrypted {produced} bytes, the archive says {size}");
    }
    Ok(Decrypted { size, written: written.into_inner(), threads })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Silent;

    fn hmac(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).unwrap();
        for p in parts {
            mac.update(p);
        }
        mac.finalize().into_bytes().to_vec()
    }

    /// Builds an archive the way the format lays it out, so `decrypt` is tested against the
    /// writing side of the same key schedule. The real check is the IPSW's own image, whose
    /// HMACs only verify if the schedule is Apple's (`cargo run --example gpu_driver`).
    fn archive(plain: &[u8], key: &[u8; 32], segment_size: usize, per_cluster: usize, compress: bool, checksum: u8) -> Vec<u8> {
        let auth = {
            let field = |k: &str, v: &[u8]| {
                let mut f = ((4 + k.len() + 1 + v.len()) as u32).to_le_bytes().to_vec();
                f.extend_from_slice(k.as_bytes());
                f.push(0);
                f.extend_from_slice(v);
                f
            };
            let mut a = field("com.apple.wkms.url", b"https://example.invalid/");
            a.extend(field(KEY_FIELD, hex::encode(key).as_bytes()));
            a
        };
        let profile = 1u32;
        let salt = [7u8; 32];
        let mut main_key = [0u8; 32];
        derive(key, Some(&salt), &info("AEA_AMK", Some(profile)), &mut main_key);
        let entry = 8 + match checksum { 1 => 8, 2 => 32, _ => 0 };
        let chunks: Vec<&[u8]> = plain.chunks(segment_size).collect();
        // Clusters are built back to front: each one's headers are sealed with the next one's MAC.
        let clusters: Vec<&[&[u8]]> = chunks.chunks(per_cluster).collect();
        let mut next_mac = vec![0u8; 32];
        let mut bodies = Vec::new();
        for (c, cluster) in clusters.iter().enumerate().rev() {
            let mut ck = [0u8; 32];
            derive(&main_key, None, &info("AEA_CK", Some(c as u32)), &mut ck);
            let hk = HeaderKey::derive(&ck, b"AEA_CHEK");
            let mut headers = vec![0u8; entry * per_cluster];
            let mut macs = vec![0u8; 32 * per_cluster];
            let mut data = Vec::new();
            for (s, seg) in cluster.iter().enumerate() {
                let mut stored = seg.to_vec();
                if compress {
                    let mut packed = Vec::new();
                    lzfse_rust::encode_bytes(seg, &mut packed).unwrap();
                    if packed.len() < seg.len() {
                        stored = packed;
                    }
                }
                let sk = HeaderKey::derive(&ck, &info("AEA_SK", Some(s as u32)));
                sk.decrypt(&mut stored);
                macs[32 * s..32 * s + 32].copy_from_slice(&hmac(&sk.0[..32], &[&stored, &0u64.to_le_bytes()]));
                headers[entry * s..entry * s + 4].copy_from_slice(&(seg.len() as u32).to_le_bytes());
                headers[entry * s + 4..entry * s + 8].copy_from_slice(&(stored.len() as u32).to_le_bytes());
                match checksum {
                    1 => headers[entry * s + 8..entry * s + 16].copy_from_slice(&murmur64(seg).to_le_bytes()),
                    2 => headers[entry * s + 8..entry * s + 40].copy_from_slice(&Sha256::digest(seg)),
                    _ => {}
                }
                data.extend(stored);
            }
            hk.decrypt(&mut headers);
            let mac = hmac(&hk.0[..32], &[&next_mac, &macs, &headers, &((32 + macs.len()) as u64).to_le_bytes()]);
            let mut body = headers;
            body.extend(&next_mac);
            body.extend(macs);
            body.extend(data);
            bodies.push(body);
            next_mac = mac;
        }
        let mut root = vec![0u8; 48];
        root[..8].copy_from_slice(&(plain.len() as u64).to_le_bytes());
        root[16..20].copy_from_slice(&(segment_size as u32).to_le_bytes());
        root[20..24].copy_from_slice(&(per_cluster as u32).to_le_bytes());
        root[24] = if compress { b'e' } else { b'-' };
        root[25] = checksum;
        let rk = HeaderKey::derive(&main_key, b"AEA_RHEK");
        rk.decrypt(&mut root);
        let root_mac = hmac(&rk.0[..32], &[&next_mac, &auth, &root, &((32 + auth.len()) as u64).to_le_bytes()]);
        let mut out = b"AEA1".to_vec();
        out.extend(profile.to_le_bytes());
        out.extend((auth.len() as u32).to_le_bytes());
        out.extend(auth);
        out.extend(salt);
        out.extend(root_mac);
        out.extend(root);
        out.extend(next_mac);
        for body in bodies.into_iter().rev() {
            out.extend(body);
        }
        out
    }

    fn sample(len: usize) -> Vec<u8> {
        // Text-like runs, noise and a long run of zeros, so all three segment shapes occur.
        let mut out = Vec::with_capacity(len);
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            match (x >> 60) & 3 {
                0 => out.extend(std::iter::repeat_n(0u8, 5000)),
                1 => out.extend((0..3000).map(|i| b"the quick brown fox "[i % 20])),
                _ => out.extend((0..2000).map(|i| (x >> (i % 56)) as u8)),
            }
        }
        out.truncate(len);
        out
    }

    fn roundtrip(plain: &[u8], compress: bool, checksum: u8, threads: usize) -> Result<Vec<u8>> {
        let key = [0x42u8; 32];
        let bytes = archive(plain, &key, 4096, 3, compress, checksum);
        let mut reader = &bytes[..];
        let prologue = Prologue::read(&mut reader)?;
        assert_eq!(prologue.embedded_key(), Some(key));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let out = File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        let done = decrypt(&prologue, &mut reader, bytes.len() as u64, &key, &out, threads, &Silent, "t")?;
        assert_eq!(done.size, plain.len() as u64);
        assert!(done.written <= done.size);
        Ok(std::fs::read(&path).unwrap())
    }

    #[test]
    fn every_segment_shape_decrypts_on_one_thread_and_on_many() {
        let plain = sample(4096 * 11 + 123);
        for (compress, checksum) in [(true, 2), (true, 1), (false, 0), (false, 2)] {
            for threads in [1, 4] {
                assert_eq!(roundtrip(&plain, compress, checksum, threads).unwrap(), plain, "compress {compress} checksum {checksum} threads {threads}");
            }
        }
    }

    #[test]
    fn a_damaged_segment_and_a_wrong_key_are_refused() {
        let plain = sample(4096 * 7);
        let key = [0x42u8; 32];
        let good = archive(&plain, &key, 4096, 3, true, 2);
        let run = |bytes: &[u8], key: &[u8; 32]| -> Result<Decrypted> {
            let mut reader = bytes;
            let prologue = Prologue::read(&mut reader)?;
            let out = tempfile::tempfile().unwrap();
            decrypt(&prologue, &mut reader, bytes.len() as u64, key, &out, 2, &Silent, "t")
        };
        run(&good, &key).unwrap();
        let wrong = run(&good, &[0x43u8; 32]).unwrap_err().to_string();
        assert!(wrong.contains("the root header failed authentication"), "{wrong}");
        let mut bad = good.clone();
        let last = bad.len() - 10;
        bad[last] ^= 1;
        let damaged = run(&bad, &key).unwrap_err().to_string();
        assert!(damaged.contains("failed authentication"), "{damaged}");
        let short = run(&good[..good.len() - 100], &key).unwrap_err().to_string();
        assert!(short.contains("truncated"), "{short}");
    }

    #[test]
    fn an_archive_without_the_key_field_has_no_embedded_key() {
        let p = Prologue { profile_word: 1, auth_data: Vec::new() };
        assert!(p.embedded_key().is_none());
        let mut auth = 29u32.to_le_bytes().to_vec();
        auth.extend(b"com.apple.wkms.url\0https:");
        let p = Prologue { profile_word: 1, auth_data: auth };
        assert_eq!(p.fields().len(), 1);
        assert!(p.embedded_key().is_none());
    }

    #[test]
    fn murmur_of_nothing_is_the_seed_finalized_and_tails_count() {
        assert_eq!(murmur64(b""), {
            let mut h = 0xE223_6FDC_26A5_F6D2u64;
            h ^= h >> 47;
            h = h.wrapping_mul(0xC6A4_A793_5BD1_E995);
            h ^ (h >> 47)
        });
        assert_ne!(murmur64(b"abcdefgh"), murmur64(b"abcdefgi"));
        assert_ne!(murmur64(b"abcdefghi"), murmur64(b"abcdefghj"));
    }
}
