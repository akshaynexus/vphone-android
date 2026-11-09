//! Resumable HTTPS downloads with a pinned SHA-256.

use crate::bail;
use crate::error::{Error, Result};
use crate::progress::{check_cancel, Progress};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(120))
        .user_agent("vphone-create-steps")
        .build()
}

/// SHA-256 of a file, lower-case hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|e| Error::path(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| Error::path(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Download `url` to `dest` unless it already holds `sha256`. The download goes
/// to `<dest>.part` and resumes there after an interruption; only a verified
/// file is renamed into place.
pub fn fetch_verified(url: &str, dest: &Path, sha256: &str, progress: &dyn Progress) -> Result<()> {
    if dest.exists() {
        if sha256_file(dest)? == sha256 {
            return Ok(());
        }
        fs::remove_file(dest).map_err(|e| Error::path(dest, e))?;
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| Error::path(dir, e))?;
    }
    let part = dest.with_extension("part");
    let mut attempts = 0;
    loop {
        match try_fetch(url, &part, progress) {
            Ok(()) => break,
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) if attempts < 8 => {
                attempts += 1;
                progress.log(&format!("download stopped ({e}); retrying"));
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            Err(e) => return Err(e),
        }
    }
    let got = sha256_file(&part)?;
    if got != sha256 {
        fs::remove_file(&part).map_err(|e| Error::path(&part, e))?;
        bail!("{}: unexpected SHA-256 {got} (want {sha256})", dest.display());
    }
    fs::rename(&part, dest).map_err(|e| Error::path(dest, e))
}

fn try_fetch(url: &str, part: &Path, progress: &dyn Progress) -> Result<()> {
    let have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut request = agent().get(url);
    if have > 0 {
        request = request.set("Range", &format!("bytes={have}-"));
    }
    let response = match request.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(416, _)) => return Ok(()), // the part is already whole
        Err(e) => return Err(Error::Http(e.to_string())),
    };
    let status = response.status();
    let resumed = have > 0 && status == 206;
    let total = response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|n| if resumed { n + have } else { n })
        .unwrap_or(0);
    let mut file = if resumed {
        OpenOptions::new().append(true).open(part)
    } else {
        File::create(part)
    }
    .map_err(|e| Error::path(part, e))?;
    let mut done = if resumed { have } else { 0 };
    let mut reader = response.into_reader();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        check_cancel(progress)?;
        let n = reader.read(&mut buf).map_err(|e| Error::Http(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| Error::path(part, e))?;
        done += n as u64;
        progress.progress(done, total, "downloading");
    }
    if total != 0 && done != total {
        bail!("{}: {done} bytes downloaded, {total} expected", part.display());
    }
    Ok(())
}

/// A `GET` of a small document (the AEA key URL).
pub fn get_text(url: &str) -> Result<String> {
    let response = agent().get(url).call().map_err(|e| Error::Http(e.to_string()))?;
    response.into_string().map_err(|e| Error::Http(e.to_string()))
}
