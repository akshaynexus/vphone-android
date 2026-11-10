//! Integration against a real dropbear 2016.74 (the ramdisk's version, built
//! from source for this host) with password auth as root/alpine, and OpenSSH's
//! sftp-server run as a command exactly as on the ramdisk.

mod common;
use common::*;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use vphone_create_steps as vc;
use vc::guestfs::GuestVolume;
use vc::progress::Silent;
use vc::ssh::{Connect, GuestSsh, Target};
use vc::tree::{self, LocalTree, Meta};

fn dropbear_dir() -> PathBuf {
    assets().join("dropbear/src")
}

struct Server {
    child: Child,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Start dropbear on a private key of `key_type`; `SHIM_HOME` is the "root" home.
fn start_dropbear(key_type: &str, bits: Option<&str>) -> Server {
    let dir = tmp();
    let base = dropbear_dir();
    assert!(base.join("dropbear").exists(), "build dropbear 2016.74 first");
    let key = dir.path().join("host_key");
    let mut gen = Command::new(base.join("dropbearkey"));
    gen.args(["-t", key_type, "-f", key.to_str().unwrap()]);
    if let Some(b) = bits {
        gen.args(["-s", b]);
    }
    assert!(gen.stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success());
    let shim = dir.path().join("shim.so");
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/preload_shim.c");
    assert!(Command::new("gcc").args(["-shared", "-fPIC", "-o"]).arg(&shim).arg(&src).status().unwrap().success());
    let port = free_port();
    let child = Command::new(base.join("dropbear"))
        .args(["-F", "-p", &format!("127.0.0.1:{port}"), "-r"])
        .arg(&key)
        .env("LD_PRELOAD", &shim)
        .env("SHIM_HOME", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, port, _dir: dir }
}

fn connect(port: u16) -> GuestSsh {
    let mut options = Connect::ramdisk(Target::Tcp(format!("127.0.0.1:{port}")));
    options.sftp_program = "/usr/lib/ssh/sftp-server".into();
    GuestSsh::connect(options).expect("connect")
}

#[test]
fn password_auth_and_exec_against_real_dropbear_ecdsa() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let out = ssh.exec("id -u; echo err >&2; exit 3", Some(Duration::from_secs(30))).unwrap();
    assert_eq!(out.status, 3);
    assert_eq!(out.stderr_text().trim(), "err");
    assert_eq!(out.stdout_text().trim().parse::<u32>().unwrap(), unsafe { libc::getuid() });
    assert!(ssh.exec("exit 7", None).unwrap().check("x").is_err());
    ssh.disconnect();
}

#[test]
fn rsa_only_host_key_negotiates() {
    let server = start_dropbear("rsa", Some("2048"));
    let ssh = connect(server.port);
    assert_eq!(ssh.exec("true", None).unwrap().status, 0);
}

#[test]
fn wrong_password_is_refused() {
    let server = start_dropbear("ecdsa", None);
    let mut options = Connect::ramdisk(Target::Tcp(format!("127.0.0.1:{}", server.port)));
    options.password = Some("nope".into());
    let error = GuestSsh::connect(options).err().expect("refused");
    assert!(error.to_string().contains("authentication failed"), "{error}");
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn guest_volume_file_operations_over_sftp() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let (sftp, raw) = ssh.sftp().unwrap();
    let dir = tmp();
    let root = dir.path().join("mnt1");
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    let v = GuestVolume::new(&ssh, &sftp, &raw, root.to_str().unwrap());
    assert!(!v.exists("usr/lib/x").unwrap());
    assert!(v.is_dir("usr/bin").unwrap());
    v.mkdir_p("usr/lib/vphone", 0o755).unwrap();
    v.write_data("usr/lib/vphone/a", b"hello", 0o640, (unsafe { libc::getuid() }, unsafe { libc::getgid() })).unwrap();
    assert_eq!(fs::read(root.join("usr/lib/vphone/a")).unwrap(), b"hello");
    assert_eq!(mode(&root.join("usr/lib/vphone/a")), 0o640);
    assert_eq!(v.read_data("usr/lib/vphone/a").unwrap(), b"hello");
    // replacing keeps the new content and removes no sibling, leaves no temp
    let local = dir.path().join("local");
    fs::write(&local, vec![0x5Au8; 300_000]).unwrap();
    v.replace_file("usr/lib/vphone/a", &local, 0o755, (unsafe { libc::getuid() }, unsafe { libc::getgid() })).unwrap();
    assert_eq!(fs::read(root.join("usr/lib/vphone/a")).unwrap().len(), 300_000);
    assert_eq!(mode(&root.join("usr/lib/vphone/a")), 0o755);
    assert_eq!(v.entries("usr/lib/vphone").unwrap(), vec!["a"]);
    // symlinks: not followed, target stored verbatim, re-creation is idempotent
    v.create_symlink("/usr/lib/launchdhook-vphone.dylib", "vh").unwrap();
    assert_eq!(fs::read_link(root.join("vh")).unwrap(), Path::new("/usr/lib/launchdhook-vphone.dylib"));
    assert!(v.is_symlink("vh").unwrap());
    assert_eq!(v.read_link("vh").unwrap(), "/usr/lib/launchdhook-vphone.dylib");
    v.create_symlink("/usr/lib/launchdhook-vphone.dylib", "vh").unwrap();
    v.create_symlink("/other", "vh").unwrap();
    assert_eq!(fs::read_link(root.join("vh")).unwrap(), Path::new("/other"));
    // rename over an existing file; copy keeps mode; chmod
    v.write_data("b", b"old", 0o644, (unsafe { libc::getuid() }, unsafe { libc::getgid() })).unwrap();
    v.write_data("c", b"new", 0o644, (unsafe { libc::getuid() }, unsafe { libc::getgid() })).unwrap();
    v.rename("c", "b").unwrap();
    assert_eq!(fs::read(root.join("b")).unwrap(), b"new");
    assert!(!root.join("c").exists());
    v.copy_within("usr/lib/vphone/a", "d").unwrap();
    assert_eq!(mode(&root.join("d")), 0o755);
    v.set_mode("d", 0o600).unwrap();
    assert_eq!(mode(&root.join("d")), 0o600);
    v.download("usr/lib/vphone/a", &dir.path().join("dl")).unwrap();
    assert_eq!(fs::metadata(dir.path().join("dl")).unwrap().len(), 300_000);
    // recursive removal does not follow a link out of the tree
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep"), b"x").unwrap();
    fs::create_dir_all(root.join("tree/sub")).unwrap();
    fs::write(root.join("tree/sub/f"), b"1").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("tree/link")).unwrap();
    v.remove_item("tree").unwrap();
    assert!(!root.join("tree").exists());
    assert!(outside.join("keep").exists());
    v.remove_item("no/such/thing").unwrap();
    // paths cannot leave the volume
    assert!(v.path("../etc/passwd").is_err());
    assert!(v.path("/etc/passwd").is_err());
}

fn sample_tree(root: &Path) {
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/file"), b"contents").unwrap();
    fs::set_permissions(root.join("a/b/file"), fs::Permissions::from_mode(0o4755)).unwrap();
    fs::set_permissions(root.join("a/b"), fs::Permissions::from_mode(0o711)).unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    std::os::unix::fs::symlink("../dangling/target", root.join("link")).unwrap();
    let long: String = "d".repeat(120);
    fs::create_dir_all(root.join(&long)).unwrap();
    let long_name = format!("{long}/{}", "f".repeat(140));
    fs::write(root.join(&long_name), b"long name").unwrap();
    let target: String = format!("/{}", "t".repeat(180));
    std::os::unix::fs::symlink(&target, root.join("longlink")).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).flat_map(|x| x.to_le_bytes()).collect();
    fs::write(root.join("big"), &big).unwrap();
}

fn assert_trees_equal(a: &Path, b: &Path) {
    let mut names: Vec<_> = fs::read_dir(a).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    let mut other: Vec<_> = fs::read_dir(b).unwrap().map(|e| e.unwrap().file_name()).collect();
    other.sort();
    assert_eq!(names, other, "{a:?}");
    for name in names {
        let (x, y) = (a.join(&name), b.join(&name));
        let (mx, my) = (fs::symlink_metadata(&x).unwrap(), fs::symlink_metadata(&y).unwrap());
        assert_eq!(mx.file_type().is_symlink(), my.file_type().is_symlink(), "{x:?}");
        if mx.file_type().is_symlink() {
            assert_eq!(fs::read_link(&x).unwrap(), fs::read_link(&y).unwrap());
        } else if mx.is_dir() {
            assert_eq!(mx.mode() & 0o7777, my.mode() & 0o7777, "{x:?}");
            assert_trees_equal(&x, &y);
        } else {
            assert_eq!(mx.mode() & 0o7777, my.mode() & 0o7777, "{x:?}");
            assert_eq!(fs::read(&x).unwrap(), fs::read(&y).unwrap(), "{x:?}");
        }
    }
}

#[test]
fn tar_upload_preserves_tree_including_long_names_and_setuid() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(&source).unwrap();
    sample_tree(&source);
    let dest = dir.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    let stats = tree::upload_tree(&ssh, dest.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &Silent).unwrap();
    assert_eq!(stats.files, 4);
    assert_eq!(stats.symlinks, 2);
    assert_eq!(stats.dirs, 4);
    assert_trees_equal(&source, &dest);
    // exclusion
    let dest2 = dir.path().join("dest2");
    fs::create_dir_all(&dest2).unwrap();
    tree::upload_tree(&ssh, dest2.to_str().unwrap(), &LocalTree::new(&source), None, &|p| p == "big" || p == "a", &Silent).unwrap();
    assert!(!dest2.join("big").exists());
    assert!(!dest2.join("a").exists());
    assert!(dest2.join("empty").exists());
}

/// What a copy told the screen and the log.
#[derive(Default)]
struct Told {
    lines: std::sync::Mutex<Vec<String>>,
    progress: std::sync::Mutex<Vec<(u64, u64, String)>>,
}

impl vc::progress::Progress for Told {
    fn log(&self, line: &str) {
        self.lines.lock().unwrap().push(line.to_owned());
    }
    fn progress(&self, done: u64, total: u64, message: &str) {
        self.progress.lock().unwrap().push((done, total, message.to_owned()));
    }
}

/// The copy the phone showed as stuck: one file of the dyld cache's kind, through the real
/// dropbear. It must say how much there is before it starts, how far it is while it runs, and
/// how long it took.
#[test]
fn a_large_copy_reports_its_total_its_progress_and_its_speed() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(&source).unwrap();
    const SIZE: u64 = 600_000_000;
    fs::File::create(source.join("dyld_shared_cache_arm64e")).unwrap().set_len(SIZE).unwrap();
    fs::write(source.join("small"), b"x").unwrap();
    let dest = dir.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    let told = Told::default();
    let start = std::time::Instant::now();
    tree::upload_tree_as("the patched dyld cache", &ssh, dest.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &told).unwrap();
    let took = start.elapsed().as_secs_f64();
    println!("{} MB in {took:.2} s: {:.0} MB/s", SIZE / 1_000_000, SIZE as f64 / 1e6 / took);
    assert_eq!(fs::metadata(dest.join("dyld_shared_cache_arm64e")).unwrap().len(), SIZE);
    let lines = told.lines.lock().unwrap();
    assert_eq!(lines[0], "[*] copying the patched dyld cache to the VM: 2 files, 600 MB (tar over SSH, one stream)");
    let end = lines.last().unwrap();
    assert!(end.starts_with("[+] copied the patched dyld cache: 600 MB in ") && end.contains("(average "), "{end}");
    println!("{end}");
    let progress = told.progress.lock().unwrap();
    assert_eq!(progress[0], (0, SIZE + 1, "Copying the patched dyld cache to the VM: 0% (0 of 600 MB) \u{b7} measuring speed".to_owned()));
    // Reports within the one big file, each with the total, never going back.
    assert!(progress.len() >= 4, "{} reports", progress.len());
    assert!(progress.iter().all(|p| p.1 == SIZE + 1));
    assert!(progress.windows(2).all(|w| w[0].0 <= w[1].0));
    assert!(progress.iter().any(|p| p.0 > 0 && p.0 < SIZE), "a report from inside the file");
    assert_eq!(progress.last().unwrap().0, SIZE + 1);
    println!("{}", progress[progress.len() - 2].2);
}

/// A copy shaped like the patched dyld cache (a few large files) over one connection and over
/// several, through the real dropbear: the same tree arrives either way, directory times
/// included, and the rates are printed for comparison.
#[test]
fn a_large_copy_over_several_connections_arrives_the_same_and_faster() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(source.join("sub/deeper")).unwrap();
    let mut total = 0u64;
    for (i, mb) in [300u64, 250, 200, 150, 100, 100, 50, 50].iter().enumerate() {
        let name = if i % 3 == 2 { format!("sub/deeper/cache.{i:02}") } else { format!("cache.{i:02}") };
        // Not all zeros: each megabyte starts with its own number.
        let f = fs::File::create(source.join(&name)).unwrap();
        f.set_len(mb * 1_000_000).unwrap();
        use std::os::unix::fs::FileExt;
        for m in 0..*mb {
            f.write_all_at(format!("{name} {m}").as_bytes(), m * 1_000_000).unwrap();
        }
        total += mb * 1_000_000;
    }
    std::os::unix::fs::symlink("cache.00", source.join("link")).unwrap();
    fs::set_permissions(source.join("sub"), fs::Permissions::from_mode(0o750)).unwrap();
    // A directory time from the past, which adding files to the copy must not replace.
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::open(source.join("sub")).unwrap().set_modified(old).unwrap();
    let port = server.port;
    let open = move || Ok(connect(port));
    let mut rates = Vec::new();
    for streams in [1usize, 2, 4] {
        let dest = dir.path().join(format!("dest{streams}"));
        fs::create_dir_all(&dest).unwrap();
        let told = Told::default();
        let more = tree::MoreLinks { open: &open, streams };
        let start = std::time::Instant::now();
        let stats = tree::upload_tree_over("the cache", &ssh, Some(&more), None, dest.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &told).unwrap();
        let rate = total as f64 / 1e6 / start.elapsed().as_secs_f64();
        println!("{streams} stream(s): {rate:.0} MB/s  ({})", told.lines.lock().unwrap().last().unwrap());
        rates.push(rate);
        assert_eq!((stats.files, stats.bytes, stats.symlinks, stats.dirs), (8, total, 1, 3));
        assert_trees_equal(&source, &dest);
        assert_eq!(fs::metadata(dest.join("sub")).unwrap().modified().unwrap(), old, "{streams} streams: the directory's time");
        assert_eq!(fs::metadata(dest.join("sub")).unwrap().permissions().mode() & 0o777, 0o750);
        let first = told.lines.lock().unwrap()[0].clone();
        assert!(first.contains(if streams == 1 { "one stream" } else { "streams, one connection each" }), "{first}");
        let progress = told.progress.lock().unwrap();
        assert!(progress.windows(2).all(|w| w[0].0 <= w[1].0), "progress never goes back");
        assert_eq!(progress.last().unwrap().0, total);
        fs::remove_dir_all(&dest).unwrap();
    }
    assert!(rates[2] > rates[0] * 1.3, "four connections were not faster than one: {rates:?}");
}

/// The transfer disk's copy, with this host as the guest: the "device" its tar reads is the disk's
/// file itself. The same tree arrives as over SSH (contents, modes, links, long names, directory
/// times), with one reader and with several; the log names the way; the file's space is given
/// back; and a small copy stays on SSH.
#[test]
fn a_copy_by_the_transfer_disk_arrives_the_same() {
    use std::os::unix::fs::FileExt;
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(&source).unwrap();
    sample_tree(&source);
    let mut total: u64 = 8 + 9 + 12_000_000;
    for (i, mb) in [20u64, 12, 9].iter().enumerate() {
        let f = fs::File::create(source.join(format!("cache.{i}"))).unwrap();
        // Not a whole number of blocks, and not all zeros.
        f.set_len(mb * 1_000_000 + 7).unwrap();
        for m in 0..*mb {
            f.write_all_at(format!("cache {i} at {m}").as_bytes(), m * 1_000_000).unwrap();
        }
        total += mb * 1_000_000 + 7;
    }
    fs::create_dir_all(source.join("many/kept")).unwrap();
    for i in 0..2100u32 {
        fs::write(source.join(format!("many/{}{i}", if i % 2 == 0 { "" } else { "kept/" })), i.to_le_bytes().repeat(i as usize % 50)).unwrap();
        total += 4 * (i as u64 % 50);
    }
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::open(source.join("many")).unwrap().set_modified(old).unwrap();
    fs::File::open(source.join("many/4")).unwrap().set_modified(old).unwrap();

    let image = dir.path().join(tree::TRANSFER_IMAGE);
    tree::TransferDisk::create(&image).unwrap();
    for readers in [1usize, 3] {
        let disk = tree::TransferDisk::open(&image, image.to_str().unwrap()).unwrap().with_readers(readers);
        let dest = dir.path().join(format!("dest{readers}"));
        fs::create_dir_all(&dest).unwrap();
        let told = Told::default();
        let stats = tree::upload_tree_over("the cache", &ssh, None, Some(&disk), dest.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &told).unwrap();
        assert_eq!((stats.files, stats.bytes, stats.symlinks, stats.dirs), (4 + 3 + 2100, total, 2, 4 + 2), "{readers} readers");
        assert_trees_equal(&source, &dest);
        for sub in ["many", "many/4"] {
            assert_eq!(fs::metadata(dest.join(sub)).unwrap().modified().unwrap(), old, "{readers} readers: {sub}'s time");
        }
        assert_eq!(fs::metadata(dest.join("a/b/file")).unwrap().permissions().mode() & 0o7777, 0o4755);
        let lines = told.lines.lock().unwrap();
        let way = if readers == 1 { "one reader" } else { "3 readers" };
        assert!(lines[0].contains("2107 files") && lines[0].contains(&format!("(transfer disk {}, {way})", image.display())), "{lines:?}");
        assert!(lines[1].starts_with("[+] copied the cache: ") && lines[1].contains("packed here in") && lines[1].contains("unpacked by the VM in"), "{lines:?}");
        let progress = told.progress.lock().unwrap();
        assert!(progress.windows(2).all(|w| w[0].0 <= w[1].0), "progress never goes back");
        assert!(progress.iter().all(|p| p.1 == total), "one total for the bar throughout");
        assert_eq!(progress.last().unwrap().0, total);
        assert!(progress[0].2.starts_with("Packing the cache for the VM's transfer disk: 0%"), "{:?}", progress[0]);
        // The archive's space went back; the label is still there for the next copy.
        assert!(fs::metadata(&image).unwrap().blocks() * 512 < 1 << 20, "the disk's file still holds the archive");
        assert_eq!(fs::metadata(&image).unwrap().len(), tree::TRANSFER_CAPACITY);
    }
    // A failing guest tar fails the copy, with what it said.
    let disk = tree::TransferDisk::open(&image, image.to_str().unwrap()).unwrap();
    let err = tree::upload_tree_over("the cache", &ssh, None, Some(&disk), "/nonexistent/destination", &LocalTree::new(&source), None, &|_| false, &Silent).unwrap_err();
    assert!(err.to_string().contains("guest tar failed") && err.to_string().contains("/nonexistent/destination"), "{err}");
    // A small copy is not worth an archive: over SSH, and the log says so.
    let small = dir.path().join("small");
    fs::create_dir_all(&small).unwrap();
    sample_tree(&small);
    let dest = dir.path().join("dest-small");
    fs::create_dir_all(&dest).unwrap();
    let told = Told::default();
    tree::upload_tree_over("a small tree", &ssh, None, Some(&disk), dest.to_str().unwrap(), &LocalTree::new(&small), None, &|_| false, &told).unwrap();
    let lines = told.lines.lock().unwrap();
    assert!(lines[0].contains("not by the transfer disk (12 MB is a small copy)") && lines[1].contains("tar over SSH, one stream"), "{lines:?}");
    assert_trees_equal(&small, &dest);
}

/// A copy too small to be worth a second key exchange stays on the one connection, and so does a
/// large one when no further connection can be made.
#[test]
fn a_small_copy_or_no_second_connection_means_one_stream() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(&source).unwrap();
    sample_tree(&source);
    let opened = std::sync::atomic::AtomicUsize::new(0);
    let open = || {
        opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(vc::error::Error::Ssh("refused".into()))
    };
    let more = tree::MoreLinks { open: &open, streams: 4 };
    let dest = dir.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    let told = Told::default();
    tree::upload_tree_over("a small tree", &ssh, Some(&more), None, dest.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &told).unwrap();
    assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 0, "a small copy asks for no connection");
    assert_trees_equal(&source, &dest);
    // Large, and the guest takes no second connection: one stream, said in the log, and it works.
    fs::File::create(source.join("large")).unwrap().set_len(tree::PARALLEL_MIN_BYTES + 1).unwrap();
    let dest = dir.path().join("dest-large");
    fs::create_dir_all(&dest).unwrap();
    let told = Told::default();
    tree::upload_tree_over("a large tree", &ssh, Some(&more), None, dest.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &told).unwrap();
    assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1, "it stops asking at the first refusal");
    let lines = told.lines.lock().unwrap();
    assert!(lines[0].contains("no further connection to the VM's SSH") && lines[1].contains("one stream"), "{lines:?}");
    assert_trees_equal(&source, &dest);
}

#[test]
fn tar_upload_reports_a_failing_guest_tar() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("f"), vec![0u8; 8 * 1024 * 1024]).unwrap();
    let missing = dir.path().join("does/not/exist");
    let error = tree::upload_tree(&ssh, missing.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &Silent);
    assert!(error.is_err(), "extraction into a missing directory must fail");
}

#[test]
fn a_metadata_override_sets_mode() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("f"), b"x").unwrap();
    let mut local = LocalTree::new(&source);
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    local.meta.insert("f".into(), Meta { mode: 0o640, uid, gid, mtime: 1234567890 });
    let dest = dir.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    tree::upload_tree(&ssh, dest.to_str().unwrap(), &local, None, &|_| false, &Silent).unwrap();
    let m = fs::metadata(dest.join("f")).unwrap();
    assert_eq!(m.mode() & 0o7777, 0o640);
    assert_eq!(m.mtime(), 1234567890);
}

/// More than russh's rekey limit (1 GiB) through dropbear 2016.74, which has no
/// strict-kex: the session must survive a re-key mid-stream.
#[test]
fn a_stream_past_the_rekey_limit_survives() {
    let server = start_dropbear("ecdsa", None);
    let ssh = connect(server.port);
    let dir = tmp();
    let source = dir.path().join("src");
    fs::create_dir_all(&source).unwrap();
    let f = fs::File::create(source.join("big")).unwrap();
    f.set_len(1_300_000_000).unwrap();
    let dest = dir.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    let start = std::time::Instant::now();
    let stats = tree::upload_tree(&ssh, dest.to_str().unwrap(), &LocalTree::new(&source), None, &|_| false, &Silent).unwrap();
    println!("{} MiB in {:?}", stats.bytes >> 20, start.elapsed());
    assert_eq!(fs::metadata(dest.join("big")).unwrap().len(), 1_300_000_000);
    // The session still works after the re-key.
    assert_eq!(ssh.exec("true", None).unwrap().status, 0);
}
