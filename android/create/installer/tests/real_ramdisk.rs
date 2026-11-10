//! Tests on a real cloudOS restore ramdisk (from the firmware on this host, not
//! in the repository): extraction through the APFS library, the full ramdisk
//! rebuild, and a real APFS tree streamed through real dropbear to GNU tar.

mod common;
use common::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use vphone_create_steps as vc;
use vc::progress::Silent;
use vc::tree::ApfsTree;
use vphone_apfs::container::VolumeSelector;
use vphone_apfs::volume::{self, FileKind};

fn real() -> PathBuf {
    repo().join("android/build/apfs-work/real")
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    kind: String,
    mode: u32,
    size: u64,
}

fn listing_from_apfs(image: &Path) -> BTreeMap<String, Entry> {
    let c = vphone_apfs::open_image(image).unwrap();
    let v = c.open_volume(&VolumeSelector::default()).unwrap();
    let root = v.lookup("/").unwrap();
    let mut out = BTreeMap::new();
    volume::walk(&v, &root, &mut |rel: &str, ino| {
        if !rel.is_empty() {
            let (kind, size) = match ino.kind() {
                FileKind::Dir => ("dir", 0),
                FileKind::Symlink => ("link", v.read_symlink(ino)?.len() as u64),
                FileKind::File => ("file", v.file_size(ino)?),
                other => panic!("{rel}: {other:?}"),
            };
            out.insert(rel.to_owned(), Entry { kind: kind.into(), mode: ino.mode as u32 & 0o777, size });
        }
        Ok(())
    })
    .unwrap();
    out
}

fn listing_from_disk(root: &Path, relative: &str, out: &mut BTreeMap<String, Entry>) {
    for entry in fs::read_dir(if relative.is_empty() { root.to_path_buf() } else { root.join(relative) }).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if relative.is_empty() { name } else { format!("{relative}/{name}") };
        let m = fs::symlink_metadata(entry.path()).unwrap();
        let (kind, size) = if m.file_type().is_dir() {
            ("dir", 0)
        } else if m.file_type().is_symlink() {
            ("link", fs::read_link(entry.path()).unwrap().as_os_str().len() as u64)
        } else {
            ("file", m.len())
        };
        out.insert(rel.clone(), Entry { kind: kind.into(), mode: m.mode() & 0o777, size });
        if m.file_type().is_dir() {
            listing_from_disk(root, &rel, out);
        }
    }
}

#[test]
fn extract_through_the_library_matches_the_listing() {
    let image = real().join("ramdisk.apfs");
    let dir = tmp();
    let dest = dir.path().join("x");
    let tree = ApfsTree { image: image.clone(), selector: VolumeSelector::default(), root: "/".into() };
    let metas = tree.extract(&dest, &Silent).unwrap();
    let apfs = listing_from_apfs(&image);
    let mut disk = BTreeMap::new();
    listing_from_disk(&dest, "", &mut disk);
    assert_eq!(apfs.len(), disk.len());
    assert!(apfs.len() > 500, "ramdisk has {} entries", apfs.len());
    for (path, a) in &apfs {
        let d = &disk[path];
        assert_eq!((&a.kind, a.size), (&d.kind, d.size), "{path}");
    }
    assert!(metas.values().any(|m| m.uid == 0));
}

#[test]
fn ramdisk_rebuild_with_the_real_ramdisk_and_tools() {
    let dir = tmp();
    let vm = dir.path().join("vm");
    let tree = vm.join("Z_Restore");
    fs::create_dir_all(tree.join("Firmware")).unwrap();
    fs::copy(real().join("094-39914-031.dmg"), tree.join("094-39914-031.dmg")).unwrap();
    fs::write(tree.join("Firmware/094-39914-031.dmg.trustcache"), b"stub rtsc is not read by rebuild").unwrap();
    let manifest = r#"<?xml version="1.0"?><plist version="1.0"><dict><key>BuildIdentities</key><array><dict><key>Manifest</key><dict>
      <key>RestoreRamDisk</key><dict><key>Info</key><dict><key>Path</key><string>094-39914-031.dmg</string></dict></dict>
      <key>RestoreTrustCache</key><dict><key>Info</key><dict><key>Path</key><string>Firmware/094-39914-031.dmg.trustcache</string></dict></dict>
    </dict></dict></array></dict></plist>"#;
    fs::write(tree.join("BuildManifest.plist"), manifest).unwrap();
    let rd = vc::ramdisk::Ramdisk::new(vm.join(".sshramdisk"));
    fs::create_dir_all(&rd.dir).unwrap();
    vc::ramdisk::unpack_staging(&assets().join("sshtars/ssh.tar.gz"), &rd.staging()).unwrap();
    vc::ramdisk::rebuild(&vm, &rd, &Silent).unwrap();
    let output = rd.output();
    assert_eq!(fs::metadata(&output).unwrap().len(), fs::metadata(real().join("094-39914-031.dmg")).unwrap().len());
    let payload = dir.path().join("payload.apfs");
    vc::im4p::extract_rdsk_payload(&output, &payload).unwrap();
    let c = vphone_apfs::open_image(&payload).unwrap();
    let v = c.open_volume(&VolumeSelector::default()).unwrap();
    let dropbear = v.lookup("/usr/local/bin/dropbear").unwrap();
    assert_eq!(v.read_file(&dropbear).unwrap(), fs::read(rd.staging().join("usr/local/bin/dropbear")).unwrap());
    assert_eq!(dropbear.mode & 0o111, 0o111, "executable");
    assert_eq!((dropbear.uid, dropbear.gid), (0, 0), "root-owned");
    assert!(v.lookup("/usr/libexec/sftp-server").is_ok());
    assert!(v.lookup("/usr/bin/img4tool").is_err(), "the drop list went");
}

/// A restore tree with the real stock ramdisk and the cloudOS manifest, as `fw prepare` leaves it
/// for a build that does not set the restore's options.
fn tree_with_stock_ramdisk(vm: &Path) -> (PathBuf, PathBuf) {
    let tree = vm.join("iPhone_Restore");
    fs::create_dir_all(&tree).unwrap();
    let ramdisk = tree.join("094-39914-031.dmg");
    fs::copy(real().join("094-39914-031.dmg"), &ramdisk).unwrap();
    let ipsw = repo().join("android/build/vusb/ipsw/cloudOS-26.4-23E5207q.ipsw");
    let mut archive = zip::ZipArchive::new(fs::File::open(ipsw).unwrap()).unwrap();
    let mut out = fs::File::create(tree.join("BuildManifest.plist")).unwrap();
    std::io::copy(&mut archive.by_name("BuildManifest.plist").unwrap(), &mut out).unwrap();
    (tree, ramdisk)
}

#[test]
fn the_fast_mtree_option_is_added_once_to_a_real_ramdisk_and_the_manifest_follows() {
    use sha2::{Digest, Sha384};
    use vc::restore_options::{ensure_fast_mtree, FastMtree, FAST_MTREE, OPTIONS_DIR};
    let dir = tmp();
    let (tree, ramdisk) = tree_with_stock_ramdisk(dir.path());
    let before = fs::metadata(&ramdisk).unwrap().len();

    let first = ensure_fast_mtree(dir.path(), &Silent).unwrap();
    assert_eq!(first, FastMtree::TurnedOn(vec![format!("{OPTIONS_DIR}/options.plist")]));
    assert_eq!(fs::metadata(&ramdisk).unwrap().len(), before, "the image keeps its size");

    // What restored_external reads: the file, as a plist with the option true.
    let payload = dir.path().join("payload.apfs");
    vc::im4p::extract_rdsk_payload(&ramdisk, &payload).unwrap();
    let container = vphone_apfs::open_image(&payload).unwrap();
    let volume = container.open_volume(&VolumeSelector::default()).unwrap();
    let inode = volume.lookup(&format!("{OPTIONS_DIR}/options.plist")).unwrap();
    assert_eq!((inode.mode as u32 & 0o777, inode.uid, inode.gid), (0o644, 0, 0));
    let options = plist::Value::from_reader(std::io::Cursor::new(volume.read_file(&inode).unwrap())).unwrap();
    assert_eq!(options.as_dictionary().unwrap()[FAST_MTREE].as_boolean(), Some(true));
    // The rest of the ramdisk is as it was.
    let stock = vphone_apfs::open_image(&real().join("ramdisk.apfs")).unwrap();
    let stock = stock.open_volume(&VolumeSelector::default()).unwrap();
    for tool in ["/usr/local/bin/restored_external", "/System/Library/Filesystems/apfs.fs/apfs_sealvolume"] {
        let (a, b) = (volume.lookup(tool).unwrap(), stock.lookup(tool).unwrap());
        assert_eq!(volume.read_file(&a).unwrap(), stock.read_file(&b).unwrap(), "{tool}");
    }

    // Every identity's digest is the rewritten file's, which is what the restore personalizes.
    let digest = Sha384::digest(fs::read(&ramdisk).unwrap()).to_vec();
    let manifest = plist::Value::from_file(tree.join("BuildManifest.plist")).unwrap();
    let identities = manifest.as_dictionary().unwrap()["BuildIdentities"].as_array().unwrap();
    let mut checked = 0;
    for identity in identities {
        // Not every identity of a cloudOS has a restore ramdisk.
        let Some(rd) = identity.as_dictionary().unwrap()["Manifest"].as_dictionary().unwrap().get("RestoreRamDisk") else { continue };
        assert_eq!(rd.as_dictionary().unwrap()["Digest"].as_data().unwrap(), &digest[..]);
        checked += 1;
    }
    assert!(checked > 0);

    // Again: nothing to do, and nothing touched.
    let bytes = fs::read(&ramdisk).unwrap();
    assert_eq!(ensure_fast_mtree(dir.path(), &Silent).unwrap(), FastMtree::AlreadyOn);
    assert_eq!(fs::read(&ramdisk).unwrap(), bytes);
    assert!(!ramdisk.with_extension("options-work").exists() && !ramdisk.with_extension("options-new").exists());

    // A tree holding the SSH ramdisk is left alone and says so.
    fs::write(format!("{}.full-restore", ramdisk.display()), b"").unwrap();
    assert!(matches!(ensure_fast_mtree(dir.path(), &Silent).unwrap(), FastMtree::Off(_)));
    assert_eq!(fs::read(&ramdisk).unwrap(), bytes);
}
