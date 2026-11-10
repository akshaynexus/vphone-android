//! Measures the copies of the custom firmware install against a real SSH ramdisk VM on this host:
//! the pieces android/create/vusb/bench-copy.sh drives.
//!
//!   copy_bench prep <vm directory> <vphone-cli>          build the SSH ramdisk, tree to ssh mode
//!   copy_bench mount <usbmuxd socket> <vm directory>     mount the VM's volumes as the install does
//!   copy_bench sh <usbmuxd socket> <command>             run a command in the ramdisk
//!   copy_bench copy <usbmuxd socket> <local tree> <guest directory> <ssh streams> [<transfer image>]
//!                                                        the install's copy; with an image, by the
//!                                                        transfer disk when it applies
//!   copy_bench check <Disk.img> <volume name> <path in it> <local tree>
//!                                                        the copy as it is on the stopped VM's disk,
//!                                                        compared with the local tree
//!   copy_bench full <vm directory>                       the tree back to full mode

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use vphone_apfs::container::VolumeSelector;
use vphone_apfs::volume::{self, FileKind};
use vphone_create_steps::error::{Error, Result};
use vphone_create_steps::manifest::{self, SshArtifacts};
use vphone_create_steps::nvram::{self, NvramEdit};
use vphone_create_steps::progress::Progress;
use vphone_create_steps::ramdisk::{self, Ramdisk};
use vphone_create_steps::ssh::{Connect, GuestSsh, Target};
use vphone_create_steps::tree::{self, LocalTree, MoreLinks, TransferDisk};

struct Print;

impl Progress for Print {
    fn log(&self, line: &str) {
        println!("{line}");
    }
    fn progress(&self, _: u64, _: u64, message: &str) {
        if std::env::var_os("BENCH_PROGRESS").is_some() {
            println!("    {message}");
        }
    }
}

fn connect(socket: &Path) -> Result<GuestSsh> {
    let stream = vusb_portfwd::connect(socket, 22, Duration::from_secs(600)).map_err(|e| Error::Ssh(format!("usbmux: {e}")))?;
    GuestSsh::connect(Connect::ramdisk(Target::Connected(stream)))
}

fn prep(vm: &Path, cli: &str) -> Result<()> {
    let rd = Ramdisk::new(vm.join(".sshramdisk"));
    let cache = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/create-steps/sshtars");
    ramdisk::stage(&rd, &cache, &Print)?;
    if !rd.output().exists() {
        ramdisk::rebuild(vm, &rd, &Print)?;
    }
    let artifacts = SshArtifacts { ramdisk: &rd.output(), stock_trustcache: &rd.stock_trustcache(), staging: &rd.staging() };
    let library = vm.parent().unwrap().to_string_lossy().into_owned();
    let name = vm.file_name().unwrap().to_string_lossy().into_owned();
    manifest::set_ssh_mode(
        vm,
        &artifacts,
        &mut || {
            let status = Command::new(cli)
                .args(["fw", "patch", "-l", &library, &name, "--restore-txm-precheck"])
                .status()
                .map_err(|e| Error::msg(format!("{cli}: {e}")))?;
            if status.success() {
                Ok(())
            } else {
                Err(Error::msg(format!("fw patch: {status}")))
            }
        },
        &Print,
    )?;
    nvram::set_file(&vm.join("nvram.bin"), &NvramEdit::auto_boot(false))?;
    Ok(())
}

fn sh(socket: &Path, command: &str) -> Result<()> {
    let ssh = connect(socket)?;
    let began = Instant::now();
    let out = ssh.exec(command, Some(Duration::from_secs(3600)))?;
    print!("{}", out.stdout_text());
    eprint!("{}", out.stderr_text());
    eprintln!("[exit {} in {:.2} s; {}]", out.status, began.elapsed().as_secs_f64(), ssh.negotiated());
    Ok(())
}

fn copy(socket: &Path, source: &Path, dest: &str, streams: usize, image: Option<&Path>) -> Result<()> {
    let ssh = connect(socket)?;
    let open = || connect(socket);
    let more = MoreLinks { open: &open, streams };
    let mut disk = match image {
        Some(image) => TransferDisk::find(&ssh, image, &Print)?,
        None => None,
    };
    if let (Some(disk), Some(readers)) = (disk.as_mut(), std::env::var("BENCH_DISK_READERS").ok().and_then(|v| v.parse().ok())) {
        disk.streams = readers;
    }
    ssh.exec(&format!("rm -rf '{dest}'; mkdir -p '{dest}'"), Some(Duration::from_secs(600)))?.check("clearing the destination")?;
    let tree = LocalTree::new(source);
    let began = Instant::now();
    let stats = tree::upload_tree_over("the test tree", &ssh, Some(&more), disk.as_ref(), dest, &tree, None, &|_| false, &Print)?;
    let copied = began.elapsed().as_secs_f64();
    ssh.exec("sync", Some(Duration::from_secs(600)))?.check("sync")?;
    let synced = began.elapsed().as_secs_f64();
    println!(
        "RESULT files={} bytes={} copy={:.1}s ({:.1} MB/s) with-sync={:.1}s ({:.1} MB/s)",
        stats.files,
        stats.bytes,
        copied,
        stats.bytes as f64 / copied / 1e6,
        synced,
        stats.bytes as f64 / synced / 1e6
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    kind: &'static str,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    /// A file's size and a hash of its contents; a link's target.
    content: String,
}

fn hash(data: impl std::io::Read) -> std::io::Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let mut data = data;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut data, &mut hasher)?;
    Ok((size, hex::encode(hasher.finalize())))
}

struct HashWriter(sha2::Sha256, u64);

impl std::io::Write for HashWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        self.0.update(data);
        self.1 += data.len() as u64;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn local_entries(root: &Path, relative: &str, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    let dir = if relative.is_empty() { root.to_path_buf() } else { root.join(relative) };
    for child in std::fs::read_dir(&dir).map_err(|e| Error::path(&dir, e))? {
        let child = child.map_err(|e| Error::path(&dir, e))?;
        let name = child.file_name().to_string_lossy().into_owned();
        let rel = if relative.is_empty() { name } else { format!("{relative}/{name}") };
        let m = std::fs::symlink_metadata(child.path()).map_err(|e| Error::path(&child.path(), e))?;
        let (kind, content) = if m.file_type().is_dir() {
            ("dir", String::new())
        } else if m.file_type().is_symlink() {
            ("link", std::fs::read_link(child.path()).map_err(|e| Error::path(&child.path(), e))?.to_string_lossy().into_owned())
        } else {
            let (size, digest) = hash(std::fs::File::open(child.path()).map_err(|e| Error::path(&child.path(), e))?)?;
            ("file", format!("{size} {digest}"))
        };
        out.insert(rel.clone(), Entry { kind, mode: m.mode() & 0o7777, uid: m.uid(), gid: m.gid(), mtime: m.mtime().max(0) as u64, content });
        if m.file_type().is_dir() {
            local_entries(root, &rel, out)?;
        }
    }
    Ok(())
}

fn check(image: &Path, volume_name: &str, path: &str, local: &Path) -> Result<()> {
    let apfs = |e: vphone_apfs::Error| Error::Apfs(e.to_string());
    let container = vphone_apfs::open_image(image).map_err(apfs)?;
    let selector = VolumeSelector { name: Some(volume_name.to_owned()), ..Default::default() };
    let vol = container.open_volume(&selector).map_err(apfs)?;
    let root = vol.lookup(path).map_err(apfs)?;
    let mut guest = BTreeMap::new();
    volume::walk(&vol, &root, &mut |rel: &str, ino| {
        if rel.is_empty() {
            return Ok(());
        }
        let (kind, content) = match ino.kind() {
            FileKind::Dir => ("dir", String::new()),
            FileKind::Symlink => ("link", vol.read_symlink(ino)?),
            FileKind::File => {
                use sha2::Digest;
                let mut w = HashWriter(sha2::Sha256::new(), 0);
                vol.write_file_to(ino, &mut w)?;
                ("file", format!("{} {}", w.1, hex::encode(w.0.finalize())))
            }
            _ => ("other", String::new()),
        };
        let extra: Vec<String> = vol.xattrs(ino.ino)?.into_iter().map(|x| x.name).filter(|n| n != "com.apple.fs.symlink").collect();
        if !extra.is_empty() || ino.bsd_flags != 0 {
            println!("NOTE {rel}: xattrs {extra:?}, flags {:#x}", ino.bsd_flags);
        }
        guest.insert(
            rel.to_owned(),
            Entry { kind, mode: ino.mode as u32 & 0o7777, uid: ino.uid, gid: ino.gid, mtime: ino.mod_time / 1_000_000_000, content },
        );
        Ok(())
    })
    .map_err(apfs)?;
    let mut here = BTreeMap::new();
    local_entries(local, "", &mut here)?;
    let mut wrong = 0;
    for (rel, want) in &here {
        match guest.get(rel) {
            Some(got) if got == want => {}
            // A link's mode and times are the guest's own; its target is what is carried.
            Some(got) if want.kind == "link" && got.kind == "link" && got.content == want.content && (got.uid, got.gid) == (want.uid, want.gid) => {}
            other => {
                wrong += 1;
                if wrong <= 20 {
                    println!("DIFFERS {rel}:\n  local {want:?}\n  guest {other:?}");
                }
            }
        }
    }
    let extra = guest.keys().filter(|k| !here.contains_key(*k)).count();
    println!("CHECK {} entries here, {} in the guest; {wrong} differ, {extra} only in the guest", here.len(), guest.len());
    if wrong != 0 || extra != 0 {
        return Err(Error::msg("the copy on the VM's disk is not the local tree"));
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = match strs.as_slice() {
        ["prep", vm, cli] => prep(Path::new(vm), cli),
        ["full", vm] => manifest::set_full_mode(Path::new(vm), &Print),
        ["sh", socket, command] => sh(Path::new(socket), command),
        ["mount", socket, vm] => connect(Path::new(socket)).and_then(|ssh| vphone_create_steps::steps::mount_guest_volumes(&ssh, Path::new(vm), &Print)),
        ["copy", socket, source, dest, streams] => copy(Path::new(socket), Path::new(source), dest, streams.parse().unwrap_or(1), None),
        ["copy", socket, source, dest, streams, image] => {
            copy(Path::new(socket), Path::new(source), dest, streams.parse().unwrap_or(1), Some(Path::new(image)))
        }
        ["check", image, volume, path, local] => check(Path::new(image), volume, path, Path::new(local)),
        _ => {
            eprintln!("usage: copy_bench prep|mount|sh|copy|check|full ... (see the file's head)");
            std::process::exit(2);
        }
    };
    if let Err(e) = done {
        eprintln!("failed: {e}");
        std::process::exit(1);
    }
}
