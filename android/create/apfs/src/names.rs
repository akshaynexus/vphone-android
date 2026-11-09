//! Filename hashing and comparison.
//!
//! APFS directory keys on normalization-insensitive volumes are built from
//! the CRC-32C of the NFD-normalized (and, on case-insensitive volumes,
//! case-folded) name, as UTF-32 little-endian code points.

use unicode_normalization::UnicodeNormalization;

const CRC32C_POLY: u32 = 0x82f6_3b78;

fn crc32c_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ CRC32C_POLY } else { c >> 1 };
            }
            *e = c;
        }
        t
    })
}

fn crc32c_update(mut crc: u32, data: &[u8]) -> u32 {
    let t = crc32c_table();
    for &b in data {
        crc = t[((crc as u8) ^ b) as usize] ^ (crc >> 8);
    }
    crc
}

/// The code points APFS compares and hashes for a name.
pub fn normalized_codepoints(name: &str, case_fold: bool) -> Vec<u32> {
    if case_fold {
        caseless::default_case_fold_str(name).nfd().map(|c| c as u32).collect()
    } else {
        name.nfd().map(|c| c as u32).collect()
    }
}

/// `name_len_and_hash` for a hashed directory key: the hash in the high 22
/// bits, and the length of the name (with its NUL) in the low 10 bits.
pub fn drec_hash(name: &str, case_fold: bool) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for cp in normalized_codepoints(name, case_fold) {
        crc = crc32c_update(crc, &cp.to_le_bytes());
    }
    let hash = crc & 0x003f_ffff;
    (hash << 10) | ((name.len() as u32 + 1) & 0x3ff)
}

pub fn names_equal(a: &str, b: &str, case_fold: bool, norm_insensitive: bool) -> bool {
    if !norm_insensitive {
        return a == b;
    }
    normalized_codepoints(a, case_fold) == normalized_codepoints(b, case_fold)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_check_value() {
        // The standard CRC-32C check value for "123456789".
        assert_eq!(crc32c_update(0xffff_ffff, b"123456789") ^ 0xffff_ffff, 0xe306_9283);
    }

    #[test]
    fn normalization_collapses_composed_forms() {
        assert!(names_equal("cafe\u{301}", "caf\u{e9}", false, true));
        assert!(!names_equal("cafe\u{301}", "caf\u{e9}", false, false));
        assert!(names_equal("Readme", "README", true, true));
    }
}
