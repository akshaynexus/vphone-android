//! Port of tools/nvram-autoboot.py: edit the guest NVRAM banks in `nvram.bin`.

use crate::error::{Error, Result};
use std::fs;
use std::path::Path;

const SIGNATURE: u8 = 0x5A;
const HEADER: usize = 16;
const BANK_HEADER: usize = 32;
/// Observed bank sizes, largest first: the guest writes 512 KiB banks.
const BANK_SIZES: [usize; 5] = [0x80000, 0x100000, 0x40000, 0x10000, 0x2000];

/// zlib-compatible adler32 starting from 1.
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

fn chrp_checksum(header: &[u8]) -> u8 {
    let mut total = header[0] as u32;
    for i in 0..14 {
        total += header[2 + i] as u32;
        total = (total + ((total & 0xFF00) >> 8)) & 0xFF;
    }
    (total & 0xFF) as u8
}

fn cstr<'a>(bytes: &'a [u8]) -> &'a [u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(n) => &bytes[..n],
        None => bytes,
    }
}

/// Every bank whose header and adler32 verify, as (base, length).
fn find_banks(data: &[u8]) -> Vec<(usize, usize)> {
    let mut banks = Vec::new();
    let mut seen: Vec<usize> = Vec::new();
    let limit = data.len().saturating_sub(BANK_HEADER);
    let mut base = 0;
    while base < limit {
        let header = &data[base..base + BANK_HEADER];
        if header[0] == SIGNATURE
            && cstr(&header[4..16]) == b"nvram"
            && chrp_checksum(&header[..HEADER]) == header[1]
        {
            let adler = u32::from_le_bytes(data[base + 16..base + 20].try_into().unwrap());
            for length in BANK_SIZES {
                if base + length > data.len() {
                    continue;
                }
                if adler32(&data[base + 0x14..base + length]) == adler {
                    if seen.iter().all(|&o| base.abs_diff(o) >= 0x2000) {
                        seen.push(base);
                        banks.push((base, length));
                    }
                    break;
                }
            }
        }
        base += 16;
    }
    banks
}

/// The env string area of the bank's `common` partition, as (start, end).
fn common_region(data: &[u8], base: usize, length: usize) -> Option<(usize, usize)> {
    let mut offset = 0x20;
    while offset + HEADER <= length {
        let header = &data[base + offset..base + offset + HEADER];
        if matches!(header[0], 0x00 | 0x7F) {
            break;
        }
        let size = u16::from_be_bytes([header[2], header[3]]) as usize * 16;
        // `common` is written with a length larger than the bank it lives in
        // (observed: 0xFE7F0 in a 0x80000 bank); clamp it to the bank.
        if cstr(&header[4..16]) == b"common" {
            return Some((base + offset + HEADER, (base + offset + size).min(base + length)));
        }
        if size < HEADER || offset + size > length {
            break;
        }
        offset += size;
    }
    None
}

/// The argument set of `nvram-autoboot.py`.
#[derive(Clone, Debug)]
pub struct NvramEdit {
    pub auto_boot: bool,
    /// `--boot-args TEXT`: non-empty sets `boot-args`, empty/None drops it.
    pub boot_args: Option<String>,
    /// `--drop NAME`: remove these variables.
    pub drop: Vec<String>,
}

impl NvramEdit {
    pub fn auto_boot(value: bool) -> Self {
        Self {
            auto_boot: value,
            boot_args: None,
            drop: Vec::new(),
        }
    }
}

/// Apply `edit` to the NVRAM image bytes; returns true when something changed
/// (`nvram-autoboot.py` only writes the file then).
pub fn apply(data: &mut [u8], edit: &NvramEdit) -> Result<bool> {
    let banks = find_banks(data);
    if banks.is_empty() {
        return Err(Error::msg("no NVRAM bank with a valid adler32"));
    }
    let drop: Vec<&[u8]> = edit.drop.iter().map(|s| s.as_bytes()).collect();
    let mut auto_boot = b"auto-boot=".to_vec();
    auto_boot.extend_from_slice(if edit.auto_boot { b"true" } else { b"false" });
    let boot_args = edit.boot_args.as_deref().filter(|s| !s.is_empty());
    let mut changed = false;
    for (base, length) in banks {
        let Some((start, end)) = common_region(data, base, length) else {
            continue;
        };
        let block = &data[start..end];
        let mut entries: Vec<Vec<u8>> = Vec::new();
        let mut cursor = 0;
        while cursor < block.len() && block[cursor] != 0 {
            let Some(stop) = block[cursor..].iter().position(|&b| b == 0) else {
                break;
            };
            entries.push(block[cursor..cursor + stop].to_vec());
            cursor += stop + 1;
        }
        let mut kept: Vec<Vec<u8>> = Vec::new();
        for entry in entries.iter() {
            let name_end = entry.iter().position(|&b| b == b'=');
            let name = match name_end {
                Some(n) => &entry[..n],
                None => &entry[..],
            };
            // A truncated earlier write (observed: "-enable-reformat=1
            // -progress", the tail of a dropped boot-args) is not a variable.
            if name_end.is_none() || name.is_empty() || name.first() == Some(&b'-') || name.contains(&b' ') {
                continue;
            }
            if name == b"auto-boot" {
                kept.push(auto_boot.clone());
            } else if name == b"boot-args" || drop.iter().any(|d| *d == name) {
                continue;
            } else {
                kept.push(entry.clone());
            }
        }
        if let Some(args) = boot_args {
            // Insert right after auto-boot, where the list was just shortened.
            let index = kept
                .iter()
                .position(|e| e.starts_with(b"auto-boot="))
                .map(|i| i + 1)
                .unwrap_or(kept.len());
            let mut entry = b"boot-args=".to_vec();
            entry.extend_from_slice(args.as_bytes());
            kept.insert(index, entry);
        }
        let mut after = Vec::new();
        for e in &kept {
            after.extend_from_slice(e);
            after.push(0);
        }
        let clear = 0x1000usize.min(end - start);
        if after.len() > clear {
            return Err(Error::msg(format!(
                "bank 0x{base:x}: env list does not fit, refusing"
            )));
        }
        after.resize(clear, 0);
        if data[start..start + clear] == after[..] {
            continue;
        }
        data[start..start + clear].copy_from_slice(&after);
        let sum = adler32(&data[base + 0x14..base + length]);
        data[base + 16..base + 20].copy_from_slice(&sum.to_le_bytes());
        changed = true;
    }
    Ok(changed)
}

/// `nvram-autoboot.py nvram.bin --auto-boot <value>`: returns whether it changed the file.
pub fn set_file(path: &Path, edit: &NvramEdit) -> Result<bool> {
    let mut data = fs::read(path).map_err(|e| Error::path(path, e))?;
    let changed = apply(&mut data, edit)
        .map_err(|e| Error::msg(format!("{}: {e}", path.display())))?;
    if changed {
        fs::write(path, &data).map_err(|e| Error::path(path, e))?;
    }
    Ok(changed)
}
