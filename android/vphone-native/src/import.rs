//! Copying a VM bundle in from file descriptors Kotlin opened through SAF. Runs of zeros stay
//! holes: a bundle's disk is a mostly sparse 20-25 GB.

use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

const BLOCK: usize = 4 << 20;

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub active: bool,
    pub name: String,
    pub file: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub error: String,
}

struct Current {
    name: String,
    file: String,
    error: String,
    temp: PathBuf,
    dest: PathBuf,
    total: u64,
}

static CURRENT: Mutex<Option<Current>> = Mutex::new(None);
static DONE: AtomicU64 = AtomicU64::new(0);
static CANCEL: AtomicBool = AtomicBool::new(false);

/// A folder name usable as a VM's: path separators and control characters become `_`.
pub fn vm_name(folder: &str) -> Result<String, String> {
    let name: String = folder
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " ._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim().trim_start_matches('.').to_string();
    if name.is_empty() {
        return Err(format!("{folder:?} cannot name a VM"));
    }
    Ok(name)
}

pub fn begin(vms: &Path, folder: &str, total: u64) -> Result<String, String> {
    let name = vm_name(folder)?;
    let dest = vms.join(&name);
    if dest.exists() {
        return Err(format!("a VM named \"{name}\" already exists"));
    }
    let mut current = CURRENT.lock().unwrap();
    if current.is_some() {
        return Err("another import is running".into());
    }
    let temp = vms.join(format!(".import-{name}"));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).map_err(|e| format!("cannot create {}: {e}", temp.display()))?;
    DONE.store(0, Ordering::Relaxed);
    CANCEL.store(false, Ordering::Relaxed);
    *current = Some(Current {
        name: name.clone(),
        file: String::new(),
        error: String::new(),
        temp,
        dest,
        total,
    });
    Ok(name)
}

fn safe_relative(rel: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(rel);
    if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(format!("unsafe file name in the folder: {rel:?}"));
    }
    Ok(path)
}

/// Copies `src` to `dest`; bytes are added to DONE as they pass. Returns the length.
pub fn copy_sparse(
    mut src: impl Read,
    dest: &Path,
    done: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<u64, String> {
    let describe = |e: std::io::Error| match e.raw_os_error() {
        Some(libc::ENOSPC) => "the phone's storage is full".to_string(),
        _ => format!("{}: {e}", dest.display()),
    };
    let mut out: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(describe)?;
    let mut buf = vec![0u8; BLOCK];
    let mut length = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let mut filled = 0;
        while filled < BLOCK {
            match src.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("reading the source: {e}")),
            }
        }
        if filled == 0 {
            break;
        }
        if buf[..filled].iter().all(|b| *b == 0) {
            out.seek(SeekFrom::Current(filled as i64))
                .map_err(describe)?;
        } else {
            out.write_all(&buf[..filled]).map_err(describe)?;
        }
        length += filled as u64;
        done.fetch_add(filled as u64, Ordering::Relaxed);
    }
    out.set_len(length).map_err(describe)?;
    out.sync_all().map_err(describe)?;
    Ok(length)
}

/// Copies one file of the running import. Takes ownership of `src`.
pub fn file(src: File, rel: &str) -> Result<(), String> {
    let (temp, rel_path) = {
        let mut guard = CURRENT.lock().unwrap();
        let cur = guard.as_mut().ok_or("no import is running")?;
        cur.file = rel.to_string();
        (cur.temp.clone(), safe_relative(rel)?)
    };
    let dest = temp.join(rel_path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let result = copy_sparse(src, &dest, &DONE, &CANCEL).map(drop);
    if let Err(e) = &result {
        if let Some(cur) = CURRENT.lock().unwrap().as_mut() {
            cur.error = e.clone();
        }
    }
    result
}

/// All files arrived: the VM appears under its name, or the bundle's own problem is the error.
pub fn finish() -> Result<(), String> {
    let mut guard = CURRENT.lock().unwrap();
    let cur = guard.as_mut().ok_or("no import is running")?;
    if let Err(problems) = crate::bundle::Bundle::open(&cur.temp) {
        let message = format!("not a VM folder: {}", problems.join("; "));
        cur.error = message.clone();
        let _ = fs::remove_dir_all(&cur.temp);
        return Err(message);
    }
    fs::rename(&cur.temp, &cur.dest).map_err(|e| {
        let m = format!("cannot move the VM into place: {e}");
        cur.error = m.clone();
        m
    })?;
    *guard = None;
    Ok(())
}

/// Ends an import that failed or was cancelled, removing what it copied.
pub fn abort() {
    let mut guard = CURRENT.lock().unwrap();
    if let Some(cur) = guard.take() {
        let _ = fs::remove_dir_all(&cur.temp);
    }
    CANCEL.store(false, Ordering::Relaxed);
}

pub fn cancel() {
    CANCEL.store(true, Ordering::Relaxed);
}

pub fn progress() -> Progress {
    match CURRENT.lock().unwrap().as_ref() {
        None => Progress::default(),
        Some(c) => Progress {
            active: true,
            name: c.name.clone(),
            file: c.file.clone(),
            done_bytes: DONE.load(Ordering::Relaxed),
            total_bytes: c.total,
            error: c.error.clone(),
        },
    }
}

/// Drops `.import-*` folders left by a process that died mid-import.
pub fn remove_stale(vms: &Path) {
    if CURRENT.lock().unwrap().is_some() {
        return;
    }
    if let Ok(entries) = fs::read_dir(vms) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(".import-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::MetadataExt;

    fn dir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../build/test-fixtures/import")
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn zero_blocks_stay_holes_and_data_survives() {
        let d = dir("sparse");
        let mut data = vec![0u8; BLOCK * 8 + 100];
        data[BLOCK * 3 + 7] = 0xaa;
        data[BLOCK * 8 + 99] = 0x55;
        let done = AtomicU64::new(0);
        let dest = d.join("Disk.img");
        let n = copy_sparse(Cursor::new(&data), &dest, &done, &AtomicBool::new(false)).unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(done.load(Ordering::Relaxed), n);
        assert_eq!(fs::read(&dest).unwrap(), data);
        assert!(fs::metadata(&dest).unwrap().blocks() * 512 < (BLOCK * 3) as u64);
    }

    #[test]
    fn cancel_stops_the_copy() {
        let d = dir("cancel");
        let err = copy_sparse(
            Cursor::new(vec![1u8; 10]),
            &d.join("f"),
            &AtomicU64::new(0),
            &AtomicBool::new(true),
        );
        assert_eq!(err, Err("cancelled".to_string()));
    }

    #[test]
    fn names_and_paths_are_made_safe() {
        assert_eq!(vm_name("my phone/2").unwrap(), "my phone_2");
        assert_eq!(vm_name("..hidden").unwrap(), "hidden");
        assert!(vm_name("...").is_err());
        assert!(safe_relative("../evil").is_err());
        assert!(safe_relative("/abs").is_err());
        assert!(safe_relative("sub/ok.bin").is_ok());
    }
}
