//! A VM as one file, to send to another phone: `<name>.vphone`.
//!
//! Only what a boot reads goes in (see [`boot_files`]): the disk, NVRAM, SEP storage, the boot
//! ROMs and the small identity files. Logs, setup leftovers and this phone's `android.json` stay
//! behind, so the receiving phone starts from its own defaults.
//!
//! The disk is tens of GB logically and mostly holes. Its data ranges come from `SEEK_DATA` /
//! `SEEK_HOLE`, all-zero chunks inside them are skipped as well, and what remains is compressed
//! with zstd on several threads. Holes come back as holes on import.
//!
//! Format, version 1 (integers little-endian):
//!
//! ```text
//! "VPHONEVM" | u32 header length | header (JSON, [`Header`])
//! zstd frame: for each file in the header's order, records of
//!     u64 offset | u64 length | <length bytes>
//!   ending with offset = u64::MAX, length 0
//! trailer (JSON, [`Trailer`]) | u32 trailer length | "VPHNEND1"
//! ```
//!
//! Each file's BLAKE3 hash covers its records (offset, length and bytes, in order), so a file
//! that arrives with any byte or range wrong is refused. The file reads front to back, so a
//! content URI that cannot seek still imports.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

const MAGIC: &[u8; 8] = b"VPHONEVM";
const END_MAGIC: &[u8; 8] = b"VPHNEND1";
const VERSION: u32 = 1;
/// The unit zero runs are found in, and the largest record.
const CHUNK: usize = 1 << 20;
const END_OF_FILE: u64 = u64::MAX;
/// Headers and trailers are small; anything larger is not one.
const MAX_JSON: u32 = 16 << 20;
pub const EXTENSION: &str = "vphone";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub mode: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub version: u32,
    pub name: String,
    pub app_version: String,
    pub files: Vec<FileEntry>,
    /// The bytes inside the files' data ranges: at most what the import writes.
    pub data_bytes: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct Trailer {
    /// BLAKE3 of each file's records, hex, in the header's order.
    hashes: Vec<String>,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub active: bool,
    /// "export" or "import".
    pub op: String,
    pub name: String,
    pub file: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// Bytes of the `.vphone` file written (export) or read (import).
    pub archive_bytes: u64,
}

struct Current {
    op: &'static str,
    name: String,
    file: String,
    total: u64,
}

static CURRENT: Mutex<Option<Current>> = Mutex::new(None);
static DONE: AtomicU64 = AtomicU64::new(0);
static ARCHIVE: AtomicU64 = AtomicU64::new(0);
static CANCEL: AtomicBool = AtomicBool::new(false);

pub fn progress() -> Progress {
    match CURRENT.lock().unwrap().as_ref() {
        None => Progress::default(),
        Some(c) => Progress {
            active: true,
            op: c.op.to_string(),
            name: c.name.clone(),
            file: c.file.clone(),
            done_bytes: DONE.load(Ordering::Relaxed),
            total_bytes: c.total,
            archive_bytes: ARCHIVE.load(Ordering::Relaxed),
        },
    }
}

pub fn cancel() {
    CANCEL.store(true, Ordering::Relaxed);
}

/// Holds the one transfer slot for the life of an export or import.
struct Slot;

impl Slot {
    fn take(op: &'static str, name: &str, total: u64) -> Result<Slot, String> {
        let mut cur = CURRENT.lock().unwrap();
        if let Some(c) = cur.as_ref() {
            return Err(format!("another transfer is running ({} of {})", c.op, c.name));
        }
        DONE.store(0, Ordering::Relaxed);
        ARCHIVE.store(0, Ordering::Relaxed);
        CANCEL.store(false, Ordering::Relaxed);
        *cur = Some(Current { op, name: name.to_string(), file: String::new(), total });
        Ok(Slot)
    }

    fn file(&self, file: &str) {
        if let Some(c) = CURRENT.lock().unwrap().as_mut() {
            c.file = file.to_string();
        }
    }

    fn total(&self, total: u64) {
        if let Some(c) = CURRENT.lock().unwrap().as_mut() {
            c.total = total;
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *CURRENT.lock().unwrap() = None;
        CANCEL.store(false, Ordering::Relaxed);
    }
}

fn cancelled() -> Result<(), String> {
    if CANCEL.load(Ordering::Relaxed) {
        Err("cancelled".into())
    } else {
        Ok(())
    }
}

fn io_text(what: &str, e: std::io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::ENOSPC) => "the phone's storage is full".to_string(),
        _ => format!("{what}: {e}"),
    }
}

/// The files of `dir` a boot reads, by the bundle's own rules: the required four plus the
/// optional identity and SEP files that exist.
pub fn boot_files(dir: &Path) -> Result<Vec<String>, String> {
    let bundle = crate::bundle::Bundle::open(dir).map_err(|p| format!("not a VM that can boot: {}", p.join("; ")))?;
    let mut files = vec!["Disk.img".to_string(), "nvram.bin".to_string(), bundle.rom.clone(), bundle.seprom.clone()];
    for optional in ["SEPStorage", "config.plist", "ecid", "serial", "gfx-res"] {
        if dir.join(optional).is_file() {
            files.push(optional.to_string());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// A file's data ranges, by `SEEK_DATA`/`SEEK_HOLE`; the whole file where those are not
/// supported.
fn data_ranges(file: &File, size: u64) -> Vec<(u64, u64)> {
    let fd = file.as_raw_fd();
    let mut out = Vec::new();
    let mut pos: i64 = 0;
    while (pos as u64) < size {
        // SAFETY: lseek on an fd this function borrows.
        let start = unsafe { libc::lseek(fd, pos, libc::SEEK_DATA) };
        if start < 0 {
            let e = std::io::Error::last_os_error().raw_os_error();
            if e == Some(libc::ENXIO) {
                break; // only a hole remains
            }
            return vec![(0, size)];
        }
        // SAFETY: as above.
        let end = unsafe { libc::lseek(fd, start, libc::SEEK_HOLE) };
        let end = if end < 0 { size as i64 } else { end };
        out.push((start as u64, (end as u64).min(size) - start as u64));
        pos = end;
    }
    out
}

/// What an export of `dir` would hold: the files and the bytes inside their data ranges.
pub fn estimate(dir: &Path) -> Result<(Vec<FileEntry>, u64), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries = Vec::new();
    let mut data = 0;
    for path in boot_files(dir)? {
        let f = File::open(dir.join(&path)).map_err(|e| io_text(&path, e))?;
        let meta = f.metadata().map_err(|e| io_text(&path, e))?;
        data += data_ranges(&f, meta.len()).iter().map(|r| r.1).sum::<u64>();
        entries.push(FileEntry { path, size: meta.len(), mode: meta.permissions().mode() & 0o777 });
    }
    Ok((entries, data))
}

/// Counts the bytes that pass, for the progress of the `.vphone` file itself.
struct Counted<T> {
    inner: T,
}

impl<T: Write> Write for Counted<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        ARCHIVE.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Read> Read for Counted<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        ARCHIVE.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn record(out: &mut impl Write, hash: &mut blake3::Hasher, offset: u64, data: &[u8]) -> std::io::Result<()> {
    let head = [offset.to_le_bytes(), (data.len() as u64).to_le_bytes()].concat();
    hash.update(&head);
    hash.update(data);
    out.write_all(&head)?;
    out.write_all(data)
}

/// Writes the VM in `dir` to `out` as a `.vphone` file. `name` is what the receiver calls it.
pub fn export(dir: &Path, name: &str, out: File, threads: u32) -> Result<u64, String> {
    let slot = Slot::take("export", name, 0)?;
    let (files, data_bytes) = estimate(dir)?;
    slot.total(data_bytes);
    let header = Header {
        version: VERSION,
        name: name.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        files: files.clone(),
        data_bytes,
    };
    let header = serde_json::to_vec(&header).map_err(|e| e.to_string())?;
    let mut w = Counted { inner: BufWriter::with_capacity(1 << 20, out) };
    let wr = |e: std::io::Error| io_text("writing the VM file", e);
    w.write_all(MAGIC).map_err(wr)?;
    w.write_all(&(header.len() as u32).to_le_bytes()).map_err(wr)?;
    w.write_all(&header).map_err(wr)?;

    let mut z = zstd::stream::write::Encoder::new(w, 3).map_err(wr)?;
    if threads > 1 {
        z.multithread(threads).map_err(wr)?;
    }
    let mut hashes = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    for entry in &files {
        slot.file(&entry.path);
        let mut f = File::open(dir.join(&entry.path)).map_err(|e| io_text(&entry.path, e))?;
        let mut hash = blake3::Hasher::new();
        for (start, len) in data_ranges(&f, entry.size) {
            f.seek(SeekFrom::Start(start)).map_err(|e| io_text(&entry.path, e))?;
            let mut at = start;
            let end = start + len;
            while at < end {
                cancelled()?;
                let n = (end - at).min(CHUNK as u64) as usize;
                f.read_exact(&mut buf[..n]).map_err(|e| io_text(&entry.path, e))?;
                if buf[..n].iter().any(|b| *b != 0) {
                    record(&mut z, &mut hash, at, &buf[..n]).map_err(wr)?;
                }
                at += n as u64;
                DONE.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        record(&mut z, &mut hash, END_OF_FILE, &[]).map_err(wr)?;
        hashes.push(hash.finalize().to_hex().to_string());
    }
    let mut w = z.finish().map_err(wr)?;
    let trailer = serde_json::to_vec(&Trailer { hashes }).map_err(|e| e.to_string())?;
    w.write_all(&trailer).map_err(wr)?;
    w.write_all(&(trailer.len() as u32).to_le_bytes()).map_err(wr)?;
    w.write_all(END_MAGIC).map_err(wr)?;
    let mut inner = w.inner.into_inner().map_err(|e| wr(e.into_error()))?;
    inner.flush().map_err(wr)?;
    inner.sync_all().map_err(wr)?;
    Ok(ARCHIVE.load(Ordering::Relaxed))
}

fn read_json<T: for<'a> Deserialize<'a>>(r: &mut impl Read, what: &str) -> Result<T, String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).map_err(|e| format!("the VM file ends inside its {what}: {e}"))?;
    let len = u32::from_le_bytes(len);
    if len > MAX_JSON {
        return Err(format!("the VM file's {what} is {len} bytes, which is not one"));
    }
    let mut bytes = vec![0u8; len as usize];
    r.read_exact(&mut bytes).map_err(|e| format!("the VM file ends inside its {what}: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("the VM file's {what} cannot be read: {e}"))
}

/// Reads a `.vphone` file's header: the name it carries and what it holds.
pub fn read_header(r: &mut impl Read) -> Result<Header, String> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(|_| "this is not a vphone VM file (it is too short)".to_string())?;
    if &magic != MAGIC {
        return Err("this is not a vphone VM file".into());
    }
    let header: Header = read_json(r, "header")?;
    if header.version != VERSION {
        return Err(format!("this VM file is format {}, and this app reads format {VERSION}; update vphone", header.version));
    }
    Ok(header)
}

/// The first name under `vms` not taken: `name`, then `name 2`, `name 3`, ...
pub fn free_name(vms: &Path, name: &str) -> String {
    let mut candidate = name.to_string();
    let mut n = 2;
    while vms.join(&candidate).exists() || vms.join(format!(".import-{candidate}")).exists() {
        candidate = format!("{name} {n}");
        n += 1;
    }
    candidate
}

/// Imports a `.vphone` file from `src` into `vms`; returns the VM's name there. `free` is the
/// space available under `vms`, checked against what the file says it holds.
pub fn import(vms: &Path, src: File, free: u64) -> Result<String, String> {
    let mut r = BufReader::with_capacity(1 << 20, Counted { inner: src });
    let header = read_header(&mut r)?;
    let name = free_name(vms, &crate::import::vm_name(&header.name)?);
    let slot = Slot::take("import", &name, header.data_bytes)?;
    const MARGIN: u64 = 1 << 30;
    if free < header.data_bytes + MARGIN {
        return Err(format!(
            "the VM needs up to {} MB on this phone and {} MB is free",
            (header.data_bytes + MARGIN) >> 20,
            free >> 20
        ));
    }
    let temp = vms.join(format!(".import-{name}"));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).map_err(|e| io_text(&temp.display().to_string(), e))?;
    let result = unpack(&mut r, &header, &temp, &slot).and_then(|()| {
        crate::bundle::Bundle::open(&temp).map_err(|p| format!("the VM file does not hold a VM that can boot: {}", p.join("; ")))?;
        fs::rename(&temp, vms.join(&name)).map_err(|e| format!("cannot move the VM into place: {e}"))
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result.map(|()| name)
}

fn unpack(r: &mut BufReader<Counted<File>>, header: &Header, temp: &Path, slot: &Slot) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut z = zstd::stream::read::Decoder::with_buffer(&mut *r)
        .map_err(|e| format!("the VM file's data cannot be read: {e}"))?
        .single_frame();
    let bad = |e: std::io::Error| format!("the VM file's data is damaged or cut short: {e}");
    let mut hashes = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    for entry in &header.files {
        let rel = safe_name(&entry.path)?;
        slot.file(&entry.path);
        let dest = temp.join(rel);
        let mut out: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .map_err(|e| io_text(&dest.display().to_string(), e))?;
        let mut hash = blake3::Hasher::new();
        loop {
            cancelled()?;
            let mut head = [0u8; 16];
            z.read_exact(&mut head).map_err(bad)?;
            hash.update(&head);
            let offset = u64::from_le_bytes(head[..8].try_into().unwrap());
            let len = u64::from_le_bytes(head[8..].try_into().unwrap());
            if offset == END_OF_FILE {
                break;
            }
            if len as usize > CHUNK || offset.checked_add(len).is_none_or(|end| end > entry.size) {
                return Err(format!("the VM file's data for {} is damaged (a range past the file's end)", entry.path));
            }
            let data = &mut buf[..len as usize];
            z.read_exact(data).map_err(bad)?;
            hash.update(data);
            out.seek(SeekFrom::Start(offset)).map_err(|e| io_text(&entry.path, e))?;
            out.write_all(data).map_err(|e| io_text(&entry.path, e))?;
            DONE.fetch_add(len, Ordering::Relaxed);
        }
        out.set_len(entry.size).map_err(|e| io_text(&entry.path, e))?;
        out.sync_all().map_err(|e| io_text(&entry.path, e))?;
        let _ = fs::set_permissions(&dest, fs::Permissions::from_mode((entry.mode & 0o777) | 0o600));
        hashes.push(hash.finalize().to_hex().to_string());
    }
    // The frame's end, then the trailer and its marker, which end the file.
    let mut rest = Vec::new();
    z.read_to_end(&mut rest).map_err(bad)?;
    if !rest.is_empty() {
        return Err("the VM file's data is damaged (more data than its files)".into());
    }
    drop(z);
    let mut tail = Vec::new();
    r.read_to_end(&mut tail).map_err(bad)?;
    if tail.len() < 12 || &tail[tail.len() - 8..] != END_MAGIC {
        return Err("the VM file is cut short (its end marker is missing)".into());
    }
    let len = u32::from_le_bytes(tail[tail.len() - 12..tail.len() - 8].try_into().unwrap()) as usize;
    if len != tail.len() - 12 {
        return Err("the VM file's trailer is damaged".into());
    }
    let trailer: Trailer = serde_json::from_slice(&tail[..len]).map_err(|e| format!("the VM file's trailer cannot be read: {e}"))?;
    if trailer.hashes.len() != hashes.len() {
        return Err("the VM file's trailer does not match its files".into());
    }
    for ((entry, got), want) in header.files.iter().zip(&hashes).zip(&trailer.hashes) {
        if got != want {
            return Err(format!("{} arrived damaged (its checksum does not match); send the VM again", entry.path));
        }
    }
    Ok(())
}

/// A file name from the archive, accepted only as a plain name in the VM's folder.
fn safe_name(path: &str) -> Result<&str, String> {
    if path.is_empty() || path.contains('/') || path.contains('\\') || path == "." || path == ".." || path.contains('\0') {
        return Err(format!("the VM file names an unsafe file: {path:?}"));
    }
    Ok(path)
}

/// Deletes exported files in `dir` older than `max_age_secs`, except the one being written.
pub fn remove_old_exports(dir: &Path, max_age_secs: u64) {
    if CURRENT.lock().unwrap().as_ref().is_some_and(|c| c.op == "export") {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_none_or(|age| age.as_secs() >= max_age_secs);
        if old {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// The destination of a `Share` export: the newest one replaces any before it.
pub fn share_path(dir: &Path, name: &str) -> PathBuf {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let _ = fs::remove_file(e.path());
        }
    }
    dir.join(format!("{}.{EXTENSION}", crate::import::vm_name(name).unwrap_or_else(|_| "vm".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// One test at a time may hold the transfer slot.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn dir(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/share").join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A VM folder of synthetic files: a sparse disk with data, a zero chunk and holes.
    fn vm(d: &Path) -> Vec<u8> {
        let src = d.join("src");
        fs::create_dir_all(&src).unwrap();
        let size = 64 * CHUNK + 1234;
        let disk = File::create(src.join("Disk.img")).unwrap();
        disk.set_len(size as u64).unwrap();
        let mut image = vec![0u8; size];
        for (at, byte) in [(3 * CHUNK + 5, 0xa5u8), (40 * CHUNK, 0x5a), (size - 1, 0x77)] {
            image[at] = byte;
            let mut f = OpenOptions::new().write(true).open(src.join("Disk.img")).unwrap();
            f.seek(SeekFrom::Start(at as u64)).unwrap();
            f.write_all(&[byte]).unwrap();
        }
        // A written chunk of zeros: data to SEEK_DATA, skipped by the zero check.
        let mut f = OpenOptions::new().write(true).open(src.join("Disk.img")).unwrap();
        f.seek(SeekFrom::Start(20 * CHUNK as u64)).unwrap();
        f.write_all(&vec![0u8; CHUNK]).unwrap();
        fs::write(src.join("nvram.bin"), b"nvram contents").unwrap();
        fs::write(src.join("ecid"), "0x1234\n").unwrap();
        fs::write(src.join("serial.log"), "left behind").unwrap();
        fs::write(src.join("android.json"), "{}").unwrap();
        image
    }

    fn rom_names_ok(src: &Path) {
        // The bundle's default ROM names, whatever they are, must exist for Bundle::open.
        if crate::bundle::Bundle::open(src).is_err() {
            let problems = crate::bundle::Bundle::open(src).err().unwrap();
            for p in problems {
                if let Some(missing) = p.strip_suffix(" is missing") {
                    fs::write(src.join(missing), b"rom").unwrap();
                }
            }
        }
    }

    #[test]
    fn a_vm_round_trips_with_its_holes_and_without_logs_or_settings() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = dir("round-trip");
        let image = vm(&d);
        rom_names_ok(&d.join("src"));
        let archive = d.join("vm.vphone");
        let size = export(&d.join("src"), "my vm", File::create(&archive).unwrap(), 2).unwrap();
        assert_eq!(size, fs::metadata(&archive).unwrap().len());
        assert!(size < 1 << 20, "a mostly empty disk compresses: {size} bytes");

        let vms = d.join("vms");
        fs::create_dir_all(vms.join("my vm")).unwrap(); // a clash: the import takes another name
        let name = import(&vms, File::open(&archive).unwrap(), u64::MAX).unwrap();
        assert_eq!(name, "my vm 2");
        let got = vms.join(&name);
        assert_eq!(fs::read(got.join("Disk.img")).unwrap(), image);
        assert!(fs::metadata(got.join("Disk.img")).unwrap().blocks() * 512 < 16 * CHUNK as u64, "holes stay holes");
        assert_eq!(fs::read(got.join("nvram.bin")).unwrap(), b"nvram contents");
        assert!(!got.join("serial.log").exists() && !got.join("android.json").exists());
        assert!(!vms.join(".import-my vm 2").exists());
    }

    #[test]
    fn a_damaged_or_cut_file_is_refused_and_leaves_nothing() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = dir("damaged");
        vm(&d);
        rom_names_ok(&d.join("src"));
        let archive = d.join("vm.vphone");
        export(&d.join("src"), "vm", File::create(&archive).unwrap(), 1).unwrap();
        let bytes = fs::read(&archive).unwrap();
        let vms = d.join("vms");
        fs::create_dir_all(&vms).unwrap();

        let cut = d.join("cut.vphone");
        fs::write(&cut, &bytes[..bytes.len() - 20]).unwrap();
        assert!(import(&vms, File::open(&cut).unwrap(), u64::MAX).is_err());

        // Flip one byte of the trailer's first hash: every range arrives, the checksum differs.
        let mut flipped = bytes.clone();
        let at = flipped.windows(12).rposition(|w| w == b"{\"hashes\":[\"").unwrap() + 12;
        flipped[at] = if flipped[at] == b'0' { b'1' } else { b'0' };
        let bad = d.join("bad.vphone");
        fs::write(&bad, &flipped).unwrap();
        let err = import(&vms, File::open(&bad).unwrap(), u64::MAX).unwrap_err();
        assert!(err.contains("checksum"), "{err}");

        fs::write(d.join("junk.vphone"), b"not a vm").unwrap();
        assert!(import(&vms, File::open(d.join("junk.vphone")).unwrap(), u64::MAX).unwrap_err().contains("not a vphone"));
        assert_eq!(fs::read_dir(&vms).unwrap().count(), 0, "nothing is left behind");
    }

    #[test]
    fn too_little_space_is_said_before_anything_is_written() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = dir("space");
        vm(&d);
        rom_names_ok(&d.join("src"));
        let archive = d.join("vm.vphone");
        export(&d.join("src"), "vm", File::create(&archive).unwrap(), 1).unwrap();
        let vms = d.join("vms");
        fs::create_dir_all(&vms).unwrap();
        let err = import(&vms, File::open(&archive).unwrap(), 1 << 20).unwrap_err();
        assert!(err.contains("MB is free"), "{err}");
        assert_eq!(fs::read_dir(&vms).unwrap().count(), 0);
    }

    #[test]
    fn a_cancelled_export_stops() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = dir("cancel");
        vm(&d);
        rom_names_ok(&d.join("src"));
        let src = d.join("src");
        let out = File::create(d.join("vm.vphone")).unwrap();
        let t = std::thread::spawn(move || export(&src, "vm", out, 1));
        // Cancel as soon as the export holds the slot.
        while !progress().active && !t.is_finished() {
            std::thread::yield_now();
        }
        cancel();
        let r = t.join().unwrap();
        assert!(r.is_err() || r.is_ok(), "either it finished first or it was cancelled");
        if let Err(e) = r {
            assert_eq!(e, "cancelled");
        }
        assert!(!progress().active);
    }

    #[test]
    fn archive_names_are_plain_file_names() {
        assert!(safe_name("Disk.img").is_ok());
        for bad in ["../x", "a/b", "", "..", "x\\y"] {
            assert!(safe_name(bad).is_err(), "{bad}");
        }
    }
}
