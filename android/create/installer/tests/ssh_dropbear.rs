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
