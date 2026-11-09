//! Resumable firmware downloads straight from Apple's CDN.
//!
//! The file is fetched in fixed-size ranges into `<dest>.part`, each range appended and, every
//! few ranges, flushed to disk with its offset recorded in `<dest>.part.ok`. A resume starts from
//! that offset, whatever the process or the phone did in between. A network that disappears or
//! changes only delays the next range; the size and, where Apple's CDN publishes it
//! (`x-amz-meta-digest-sha256`), the SHA-256 of the finished file are checked before it is used.

use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SEGMENT: u64 = 16 << 20;
const CHECKPOINT_EVERY: u32 = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(240);
/// Kept free beyond what the download still needs.
const FREE_MARGIN: u64 = 1 << 30;
const MAX_HTTP_RETRIES: u32 = 8;

/// What the download asks of the creation that runs it.
pub trait Gate {
    fn cancelled(&self) -> bool;
    /// Why the transfer must wait right now; `None` when it may proceed.
    fn blocked(&self) -> Option<String>;
    fn free_bytes(&self) -> u64;
    /// Progress: bytes done and total, bytes per second, and what to show.
    fn report(&self, done: u64, total: u64, speed: u64, message: &str);
    /// `blocked`'s text while waiting, or "" once the wait is over.
    fn hold(&self, message: &str);
}

#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub size: u64,
    pub sha256: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Fetch {
    pub url: String,
    pub dest: PathBuf,
    /// The catalog's size for this image, where it has one.
    pub expected_size: Option<u64>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent("vphone-android/0.1")
        .build()
        .into()
}

fn lower_header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

/// The file's size, digest and date from the server.
pub fn probe(agent: &ureq::Agent, url: &str) -> Result<Remote, HttpError> {
    let resp = agent.head(url).call().map_err(HttpError::network)?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(HttpError::status(status));
    }
    let size = lower_header(&resp, "content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| HttpError::protocol("the server reported no Content-Length"))?;
    Ok(Remote {
        size,
        sha256: lower_header(&resp, "x-amz-meta-digest-sha256")
            .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())),
        last_modified: lower_header(&resp, "last-modified"),
    })
}

#[derive(Debug)]
pub struct HttpError {
    pub message: String,
    /// Worth retrying once the network is back; otherwise a definite failure (a 404, say).
    pub transient: bool,
}

impl HttpError {
    fn network(e: ureq::Error) -> HttpError {
        HttpError {
            message: format!("network: {e}"),
            transient: true,
        }
    }
    fn status(code: u16) -> HttpError {
        HttpError {
            message: format!("HTTP {code}"),
            transient: code == 408 || code == 429 || code >= 500,
        }
    }
    fn protocol(message: &str) -> HttpError {
        HttpError {
            message: message.to_string(),
            transient: false,
        }
    }
}

/// Bytes `first..=last` of `url`, appended to `out`; returns how many arrived.
fn get_range(
    agent: &ureq::Agent,
    url: &str,
    first: u64,
    last: u64,
    remote: &Remote,
    out: &mut File,
    sink: &mut dyn FnMut(u64) -> bool,
) -> Result<u64, HttpError> {
    let mut req = agent.get(url).header("Range", format!("bytes={first}-{last}"));
    if let Some(date) = &remote.last_modified {
        // Fails over to a whole-file 200 if Apple replaced the file under this URL.
        req = req.header("If-Range", date.as_str());
    }
    let mut resp = req.call().map_err(HttpError::network)?;
    match resp.status().as_u16() {
        206 => {}
        200 => {
            return Err(HttpError::protocol(
                "the server replaced the file or does not support resuming",
            ));
        }
        code => return Err(HttpError::status(code)),
    }
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 1 << 18];
    let mut got = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| HttpError {
            message: format!("network: {e}"),
            transient: true,
        })?;
        if n == 0 {
            return Ok(got);
        }
        out.write_all(&buf[..n]).map_err(|e| HttpError {
            message: storage_message(&e),
            transient: e.raw_os_error() == Some(libc::ENOSPC),
        })?;
        got += n as u64;
        if !sink(n as u64) {
            return Ok(got);
        }
    }
}

fn storage_message(e: &io::Error) -> String {
    if e.raw_os_error() == Some(libc::ENOSPC) {
        "the phone's storage is full".into()
    } else {
        format!("cannot write the download: {e}")
    }
}

fn hint_part(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", dest.display()))
}
fn hint_ok(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part.ok", dest.display()))
}
fn hint_meta(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta", dest.display()))
}

/// The offset a part file can be trusted to, given its recorded checkpoint.
fn resume_offset(dest: &Path, part_len: u64) -> u64 {
    fs::read_to_string(hint_ok(dest))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .map_or(0, |ok| ok.min(part_len))
}

fn checkpoint(dest: &Path, out: &File, offset: u64) -> io::Result<()> {
    out.sync_data()?;
    let path = hint_ok(dest);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, offset.to_string())?;
    fs::rename(&tmp, &path)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    url: String,
    size: u64,
    sha256: Option<String>,
    verified: bool,
}

fn read_meta(dest: &Path) -> Option<Meta> {
    serde_json::from_slice(&fs::read(hint_meta(dest)).ok()?).ok()
}

fn write_meta(dest: &Path, meta: &Meta) {
    if let Ok(json) = serde_json::to_vec(meta) {
        let _ = fs::write(hint_meta(dest), json);
    }
}

/// Waits out a `blocked()` reason, returning `false` when cancelled meanwhile.
fn wait_unblocked(gate: &dyn Gate) -> bool {
    let mut waited = false;
    while let Some(reason) = gate.blocked() {
        if gate.cancelled() {
            return false;
        }
        waited = true;
        gate.hold(&reason);
        std::thread::sleep(Duration::from_secs(2));
    }
    if waited {
        gate.hold("");
    }
    !gate.cancelled()
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempt.min(6)).min(60))
}

/// Runs `op` until it succeeds, waiting out the network and backing off; definite failures end
/// it. Used for the probe.
fn retry<T>(
    gate: &dyn Gate,
    mut op: impl FnMut() -> Result<T, HttpError>,
) -> Result<T, String> {
    let mut attempt = 0;
    loop {
        if !wait_unblocked(gate) {
            return Err("cancelled".into());
        }
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if !e.transient || attempt >= 60 => return Err(e.message),
            Err(e) => {
                attempt += 1;
                gate.hold(&format!("Retrying: {}", e.message));
                sleep_cancellable(gate, backoff(attempt))?;
            }
        }
    }
}

fn sleep_cancellable(gate: &dyn Gate, d: Duration) -> Result<(), String> {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if gate.cancelled() {
            return Err("cancelled".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// The SHA-256 of `path`, reporting progress. `None` when cancelled.
fn hash_file(path: &Path, size: u64, gate: &dyn Gate) -> io::Result<Option<String>> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        if gate.cancelled() {
            return Ok(None);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        if last.elapsed() >= Duration::from_secs(1) {
            gate.report(done, size, 0, "Verifying the download");
            last = Instant::now();
        }
    }
    let digest = hasher.finalize();
    Ok(Some(
        digest.iter().fold(String::new(), |s, b| s + &format!("{b:02x}")),
    ))
}

/// True when `dest` exists and already holds the wanted file (trusting a verified record, and
/// verifying digest otherwise).
fn complete(dest: &Path, remote: &Remote, gate: &dyn Gate) -> Result<bool, String> {
    let Ok(md) = fs::metadata(dest) else {
        return Ok(false);
    };
    if md.len() != remote.size {
        // A short file from an older run resumes; a longer one starts over.
        if md.len() < remote.size {
            fs::rename(dest, hint_part(dest)).map_err(|e| e.to_string())?;
            fs::write(hint_ok(dest), md.len().to_string()).map_err(|e| e.to_string())?;
        } else {
            let _ = fs::remove_file(dest);
        }
        return Ok(false);
    }
    if let Some(meta) = read_meta(dest) {
        if meta.verified && meta.size == remote.size {
            return Ok(true);
        }
    }
    if let Some(want) = &remote.sha256 {
        match hash_file(dest, remote.size, gate).map_err(|e| e.to_string())? {
            None => return Err("cancelled".into()),
            Some(got) if &got == want => {}
            Some(_) => {
                let _ = fs::remove_file(dest);
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Downloads `job.url` to `job.dest`, resuming a partial download.
pub fn fetch(job: &Fetch, gate: &dyn Gate) -> Result<(), String> {
    let agent = agent();
    gate.report(0, 0, 0, "Contacting Apple's update server");
    let remote = retry(gate, || probe(&agent, &job.url))?;
    if let Some(want) = job.expected_size {
        if want != remote.size {
            return Err(format!(
                "Apple's server reports {} bytes for {}, the catalog says {want}",
                remote.size,
                job.dest.display()
            ));
        }
    }
    if complete(&job.dest, &remote, gate)? {
        write_meta(
            &job.dest,
            &Meta {
                url: job.url.clone(),
                size: remote.size,
                sha256: remote.sha256.clone(),
                verified: true,
            },
        );
        return Ok(());
    }

    let part = hint_part(&job.dest);
    let mut out = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&part)
        .map_err(|e| format!("cannot create {}: {e}", part.display()))?;
    let mut offset = resume_offset(&job.dest, out.metadata().map_or(0, |m| m.len()));
    out.set_len(offset).map_err(|e| storage_message(&e))?;
    out.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;

    let mut window = Instant::now();
    let mut window_bytes = 0u64;
    let mut speed = 0u64;
    let mut since_checkpoint = 0u32;
    let mut http_failures = 0u32;

    while offset < remote.size {
        if !wait_unblocked(gate) {
            return Err("cancelled".into());
        }
        let remaining = remote.size - offset;
        while gate.free_bytes() < remaining + FREE_MARGIN {
            if gate.cancelled() {
                return Err("cancelled".into());
            }
            gate.hold(&format!(
                "Waiting for storage: {} GB more free space needed",
                (remaining + FREE_MARGIN - gate.free_bytes()) / (1 << 30)
            ));
            std::thread::sleep(Duration::from_secs(5));
        }
        gate.hold("");

        let last = (offset + SEGMENT - 1).min(remote.size - 1);
        let mut received = 0u64;
        let result = get_range(&agent, &job.url, offset, last, &remote, &mut out, &mut |n| {
            received += n;
            window_bytes += n;
            let elapsed = window.elapsed();
            if elapsed >= Duration::from_secs(1) {
                speed = (window_bytes as f64 / elapsed.as_secs_f64()) as u64;
                window = Instant::now();
                window_bytes = 0;
                gate.report(offset + received, remote.size, speed, "Downloading");
            }
            !gate.cancelled() && gate.blocked().is_none()
        });
        offset += received;
        match result {
            Ok(_) => {
                http_failures = 0;
                since_checkpoint += 1;
                if since_checkpoint >= CHECKPOINT_EVERY {
                    since_checkpoint = 0;
                    checkpoint(&job.dest, &out, offset).map_err(|e| storage_message(&e))?;
                }
            }
            Err(e) if e.transient => {
                // Trust nothing past the last whole write; the next request re-covers it.
                checkpoint(&job.dest, &out, offset).map_err(|e| storage_message(&e))?;
                gate.hold(&format!("Retrying: {}", e.message));
                sleep_cancellable(gate, backoff(http_failures))?;
                gate.hold("");
                http_failures += 1;
                if http_failures > MAX_HTTP_RETRIES * 8 {
                    return Err(e.message);
                }
            }
            Err(e) => {
                // A file replaced at Apple restarts cleanly next time.
                let _ = fs::remove_file(&part);
                let _ = fs::remove_file(hint_ok(&job.dest));
                return Err(e.message);
            }
        }
    }
    checkpoint(&job.dest, &out, offset).map_err(|e| storage_message(&e))?;
    drop(out);

    if let Some(want) = &remote.sha256 {
        match hash_file(&part, remote.size, gate).map_err(|e| e.to_string())? {
            None => return Err("cancelled".into()),
            Some(got) if &got == want => {}
            Some(got) => {
                let _ = fs::remove_file(&part);
                let _ = fs::remove_file(hint_ok(&job.dest));
                return Err(format!(
                    "the downloaded file's SHA-256 is {got}, Apple's is {want}: it will be downloaded again on retry"
                ));
            }
        }
    }
    fs::rename(&part, &job.dest).map_err(|e| format!("cannot finish the download: {e}"))?;
    let _ = fs::remove_file(hint_ok(&job.dest));
    write_meta(
        &job.dest,
        &Meta {
            url: job.url.clone(),
            size: remote.size,
            sha256: remote.sha256,
            verified: true,
        },
    );
    gate.report(remote.size, remote.size, 0, "Download complete");
    Ok(())
}

/// Removes a cached image and its bookkeeping (the "reclaim space" choice).
pub fn remove_cached(dest: &Path) {
    for p in [
        dest.to_path_buf(),
        hint_part(dest),
        hint_ok(dest),
        hint_meta(dest),
    ] {
        let _ = fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    /// A small HTTP/1.1 server with ranges; `fail_after` closes a connection once.
    fn serve(body: Vec<u8>, sha: Option<String>, fail_after: Option<u64>) -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/iPhone.ipsw", listener.local_addr().unwrap());
        let served = Arc::new(AtomicU64::new(0));
        let counter = served.clone();
        std::thread::spawn(move || {
            let mut failed = false;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let body = body.clone();
                let sha = sha.clone();
                let counter = counter.clone();
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    if s.read(&mut b).unwrap_or(0) == 0 {
                        break;
                    }
                    buf.push(b[0]);
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                if head.starts_with("HEAD") {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nLast-Modified: Thu, 03 Sep 2026 14:55:06 GMT\r\n",
                        body.len()
                    );
                    if let Some(sha) = &sha {
                        r += &format!("x-amz-meta-digest-sha256: {sha}\r\n");
                    }
                    let _ = s.write_all((r + "Connection: close\r\n\r\n").as_bytes());
                    continue;
                }
                let range = head
                    .lines()
                    .find_map(|l| {
                        let l = l.to_ascii_lowercase();
                        l.strip_prefix("range: bytes=").map(|v| v.trim().to_string())
                    })
                    .expect("a range request");
                let (a, z) = range.split_once('-').unwrap();
                let a: usize = a.parse().unwrap();
                let z: usize = z.parse().unwrap();
                let slice = &body[a..=z];
                let r = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{z}/{}\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    body.len()
                );
                let _ = s.write_all(r.as_bytes());
                let drop_now = fail_after.is_some_and(|n| !failed && counter.load(Ordering::SeqCst) >= n);
                if drop_now {
                    failed = true;
                    let _ = s.write_all(&slice[..slice.len() / 2]);
                    counter.fetch_add((slice.len() / 2) as u64, Ordering::SeqCst);
                    continue;
                }
                let _ = s.write_all(slice);
                counter.fetch_add(slice.len() as u64, Ordering::SeqCst);
            }
        });
        (url, served)
    }

    struct TestGate {
        cancel: Arc<AtomicBool>,
        cancel_when: Option<(Arc<AtomicU64>, u64)>,
        free: u64,
    }

    impl TestGate {
        fn new() -> TestGate {
            TestGate {
                cancel: Arc::new(AtomicBool::new(false)),
                cancel_when: None,
                free: 1 << 40,
            }
        }
    }

    impl Gate for TestGate {
        fn cancelled(&self) -> bool {
            self.cancel.load(Ordering::SeqCst)
                || self
                    .cancel_when
                    .as_ref()
                    .is_some_and(|(served, n)| served.load(Ordering::SeqCst) >= *n)
        }
        fn blocked(&self) -> Option<String> {
            None
        }
        fn free_bytes(&self) -> u64 {
            self.free
        }
        fn report(&self, _done: u64, _total: u64, _speed: u64, _m: &str) {}
        fn hold(&self, _m: &str) {}
    }

    fn sha_of(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .fold(String::new(), |s, b| s + &format!("{b:02x}"))
    }

    fn dir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../build/test-fixtures/download")
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn body(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn downloads_and_verifies() {
        let data = body((SEGMENT * 2 + 12345) as usize);
        let (url, _) = serve(data.clone(), Some(sha_of(&data)), None);
        let d = dir("fresh");
        let dest = d.join("f.ipsw");
        let job = Fetch { url, dest: dest.clone(), expected_size: Some(data.len() as u64) };
        fetch(&job, &TestGate::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
        assert!(!hint_part(&dest).exists());
        // A second run sees the verified file and transfers nothing.
        fetch(&job, &TestGate::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
    }

    #[test]
    fn survives_a_dropped_connection() {
        let data = body((SEGMENT * 2) as usize);
        let (url, _) = serve(data.clone(), Some(sha_of(&data)), Some(SEGMENT / 2));
        let d = dir("drop");
        let dest = d.join("f.ipsw");
        fetch(&Fetch { url, dest: dest.clone(), expected_size: None }, &TestGate::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
    }

    #[test]
    fn resumes_after_cancellation_from_the_checkpoint() {
        let data = body((SEGMENT * 6) as usize);
        let (url, served) = serve(data.clone(), Some(sha_of(&data)), None);
        let d = dir("resume");
        let dest = d.join("f.ipsw");
        let job = Fetch { url, dest: dest.clone(), expected_size: None };
        let mut gate = TestGate::new();
        gate.cancel_when = Some((served.clone(), SEGMENT));
        let err = fetch(&job, &gate).unwrap_err();
        assert_eq!(err, "cancelled");
        assert!(hint_part(&dest).exists());
        let part_bytes = fs::metadata(hint_part(&dest)).unwrap().len();
        assert!(part_bytes < data.len() as u64);
        let before = served.load(Ordering::SeqCst);
        fetch(&job, &TestGate::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
        // Only what was missing was fetched again (at most one segment re-covered).
        let total = served.load(Ordering::SeqCst);
        assert!(total - before <= data.len() as u64 - part_bytes + SEGMENT, "{before} {total} {part_bytes}");
    }

    #[test]
    fn rejects_a_corrupt_download_then_retries_clean() {
        let data = body(1 << 20);
        let (url, _) = serve(data.clone(), Some("0".repeat(64)), None);
        let d = dir("corrupt");
        let dest = d.join("f.ipsw");
        let job = Fetch { url, dest: dest.clone(), expected_size: None };
        assert!(fetch(&job, &TestGate::new()).unwrap_err().contains("SHA-256"));
        assert!(!hint_part(&dest).exists() && !dest.exists());
    }

    #[test]
    fn waits_for_storage() {
        let data = body(1 << 16);
        let (url, _) = serve(data.clone(), None, None);
        let d = dir("storage");
        let dest = d.join("f.ipsw");
        let job = Fetch { url, dest: dest.clone(), expected_size: None };
        let gate = TestGate {
            cancel: Arc::new(AtomicBool::new(false)),
            cancel_when: None,
            free: 0,
        };
        let cancel = gate.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel.store(true, Ordering::SeqCst);
        });
        assert_eq!(fetch(&job, &gate).unwrap_err(), "cancelled");
        assert!(!dest.exists());
    }

    /// Against Apple's CDN: TLS chain, HEAD digest header, a real range. Needs the network.
    #[test]
    #[ignore]
    fn apple_cdn_range() {
        let url = "https://updates.cdn-apple.com/private-cloud-compute/c0ecdb4b310cf5239ab2b248dd3098eec297dc5aa3bbe6ada27273262b0b8b64";
        let agent = agent();
        let remote = probe(&agent, url).map_err(|e| e.message).unwrap();
        assert_eq!(remote.size, 1199454323);
        assert!(remote.last_modified.is_some(), "{remote:?}");
        let d = dir("apple");
        let mut out = File::create(d.join("x")).unwrap();
        let n = get_range(&agent, url, 0, (1 << 20) - 1, &remote, &mut out, &mut |_| true)
            .map_err(|e| e.message)
            .unwrap();
        assert_eq!(n, 1 << 20);
        assert_eq!(&fs::read(d.join("x")).unwrap()[..2], b"PK");
    }
}
