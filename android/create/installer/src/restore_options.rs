//! The restore ramdisk's own options, which decide whether the guest's restore takes its fast
//! path when it seals the system volume.
//!
//! `restored_external` hashes every system file with `mtree`, on one CPU, unless the option
//! `SealedVolumeUseFastMtree` is set; then it writes the same list from the manifest it was sent
//! ("Fast mtree path enabled."). It takes that option only from a file in its own ramdisk: what a
//! host sends is cut down to an allowlist that does not include it. The file is
//! `/usr/local/share/restore/options.<board>.plist` when there is one, else `options.plist`
//! beside it; allowed host options are laid over it.
//!
//! `fw patch` writes the file when it patches the ramdisk, but only since the build that learned
//! to, and a creation that resumes skips the steps it has done. So the option is made sure of
//! here, before every restore, on whatever ramdisk the tree has.

use crate::error::{Error, Result};
use crate::im4p;
use crate::manifest;
use crate::plist_py;
use crate::progress::Progress;
use plist::{Dictionary, Value};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use vphone_apfs::{Attrs, FileKind, MutVolume, VolumeSelector};

pub const OPTIONS_DIR: &str = "/usr/local/share/restore";
pub const FAST_MTREE: &str = "SealedVolumeUseFastMtree";
/// What only a restore ramdisk has; the SSH ramdisk built from it has it too, and is told apart
/// by the tree's `.full-restore` backups.
const RESTORE_TOOL: &str = "/usr/local/bin/restored_external";

/// Whether the next restore takes the fast path, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FastMtree {
    /// The ramdisk already had the option.
    AlreadyOn,
    /// It was added now, in these files of the ramdisk.
    TurnedOn(Vec<String>),
    /// It cannot be set on this tree; the restore runs `mtree` in full.
    Off(String),
}

impl FastMtree {
    /// One line for the step's log.
    pub fn describe(&self) -> String {
        match self {
            FastMtree::AlreadyOn => "Fast mtree: ON (the restore ramdisk already has the option)".to_owned(),
            FastMtree::TurnedOn(files) => format!(
                "Fast mtree: ON (added to the restore ramdisk now, in {}: it was patched by a build that did not set it)",
                files.join(" and ")
            ),
            FastMtree::Off(why) => format!("Fast mtree: OFF ({why}); the restore hashes the system volume in full, which is slower"),
        }
    }
}

fn apfs(e: vphone_apfs::Error) -> Error {
    Error::Apfs(e.to_string())
}

fn is_options_file(name: &str) -> bool {
    name == "options.plist" || (name.starts_with("options.") && name.ends_with(".plist"))
}

/// The options files of a ramdisk image, each with whether it turns the fast path on. An image
/// with none has an empty list.
fn read_options(image: &Path) -> Result<Vec<(String, Option<Dictionary>)>> {
    let container = vphone_apfs::open_image(image).map_err(apfs)?;
    let volume = container.open_volume(&VolumeSelector::default()).map_err(apfs)?;
    let Ok(dir) = volume.lookup(OPTIONS_DIR) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in volume.read_dir(dir.ino).map_err(apfs)? {
        if !is_options_file(&entry.name) {
            continue;
        }
        let inode = volume.lookup(&format!("{OPTIONS_DIR}/{}", entry.name)).map_err(apfs)?;
        if inode.kind() != FileKind::File {
            continue;
        }
        let bytes = volume.read_file(&inode).map_err(apfs)?;
        // A file the restore could not parse either is replaced rather than kept.
        let dict = Value::from_reader(std::io::Cursor::new(bytes)).ok().and_then(Value::into_dictionary);
        files.push((entry.name, dict));
    }
    Ok(files)
}

fn has_fast_mtree(dict: &Option<Dictionary>) -> bool {
    dict.as_ref().and_then(|d| d.get(FAST_MTREE)).and_then(Value::as_boolean) == Some(true)
}

/// Whether `restored_external` would find the option: in the board's file when there is one,
/// which it reads instead of `options.plist`. Every options file must carry it, as which board
/// name the guest asks for is its own business.
fn already_on(files: &[(String, Option<Dictionary>)]) -> bool {
    files.iter().any(|(name, _)| name == "options.plist") && files.iter().all(|(_, dict)| has_fast_mtree(dict))
}

/// Sets the option in every options file of `image`, creating `options.plist`. Returns the files
/// written.
fn write_options(image: &Path, files: Vec<(String, Option<Dictionary>)>) -> Result<Vec<String>> {
    let mut files = files;
    if !files.iter().any(|(name, _)| name == "options.plist") {
        files.push(("options.plist".to_owned(), None));
    }
    let mut volume = MutVolume::open(image, &VolumeSelector::default()).map_err(apfs)?;
    let root = Attrs { mode: Some(0o755), uid: Some(0), gid: Some(0), ..Default::default() };
    volume.mkdir(OPTIONS_DIR, &root, true).map_err(apfs)?;
    let mut written = Vec::new();
    for (name, dict) in files {
        if has_fast_mtree(&dict) {
            continue;
        }
        let mut dict = dict.unwrap_or_default();
        dict.insert(FAST_MTREE.to_owned(), Value::Boolean(true));
        let path = format!("{OPTIONS_DIR}/{name}");
        let attrs = Attrs { mode: Some(0o644), uid: Some(0), gid: Some(0), ..Default::default() };
        volume.create_file(&path, &plist_py::to_xml(&Value::Dictionary(dict)), &attrs).map_err(apfs)?;
        written.push(path);
    }
    volume.commit().map_err(apfs)?;
    Ok(written)
}

/// Points every identity's `RestoreRamDisk` digest at `digest`, where it names `ramdisk`.
fn set_ramdisk_digest(manifest: &mut Value, ramdisk: &str, digest: &[u8; 48]) -> Result<usize> {
    let identities = manifest
        .as_dictionary_mut()
        .and_then(|d| d.get_mut("BuildIdentities"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::msg("BuildManifest has no BuildIdentities"))?;
    let mut changed = 0;
    for identity in identities {
        let component = identity
            .as_dictionary_mut()
            .and_then(|d| d.get_mut("Manifest"))
            .and_then(Value::as_dictionary_mut)
            .and_then(|m| m.get_mut("RestoreRamDisk"))
            .and_then(Value::as_dictionary_mut);
        let Some(component) = component else { continue };
        let path = component.get("Info").and_then(Value::as_dictionary).and_then(|i| i.get("Path")).and_then(Value::as_string);
        if path != Some(ramdisk) || !component.contains_key("Digest") {
            continue;
        }
        component.insert("Digest".into(), Value::Data(digest.to_vec()));
        changed += 1;
    }
    Ok(changed)
}

/// Makes sure the restore ramdisk of `vm`'s tree turns the fast mtree path on. Safe to run before
/// every restore: a ramdisk that has the option is left alone. A tree it cannot be set on is
/// reported, not failed, as the restore works without it.
///
/// The ramdisk is rewritten beside itself and its manifest digest updated before it replaces the
/// old one, so a run that is cut short leaves a tree the next run sets right.
pub fn ensure_fast_mtree(vm: &Path, progress: &dyn Progress) -> Result<FastMtree> {
    let tree = manifest::restore_tree(vm)?;
    let manifest_path = tree.join("BuildManifest.plist");
    let mut manifest = Value::from_file(&manifest_path).map_err(|e| Error::msg(format!("{}: {e}", manifest_path.display())))?;
    let name = manifest::component_path(&manifest, "RestoreRamDisk")?;
    let ramdisk = tree.join(&name);
    if Path::new(&format!("{}.full-restore", ramdisk.display())).exists() {
        return Ok(FastMtree::Off("the restore tree holds the SSH ramdisk, not the restore's".to_owned()));
    }
    let mut header = [0u8; 27];
    let length = fs::metadata(&ramdisk).map_err(|e| Error::path(&ramdisk, e))?.len();
    File::open(&ramdisk)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| Error::path(&ramdisk, e))?;
    if let Err(e) = im4p::rdsk_payload(&header, length) {
        return Ok(FastMtree::Off(format!("the restore ramdisk is not a plain disk image ({e})")));
    }
    let work = ramdisk.with_extension("options-work");
    let _ = fs::remove_file(&work);
    let outcome = (|| -> Result<FastMtree> {
        im4p::extract_rdsk_payload(&ramdisk, &work)?;
        let files = match read_options(&work) {
            Ok(files) => files,
            Err(e) => return Ok(FastMtree::Off(format!("the restore ramdisk's volume could not be read ({e})"))),
        };
        {
            let container = vphone_apfs::open_image(&work).map_err(apfs)?;
            let volume = container.open_volume(&VolumeSelector::default()).map_err(apfs)?;
            if volume.lookup(RESTORE_TOOL).is_err() {
                return Ok(FastMtree::Off(format!("the tree's ramdisk has no {RESTORE_TOOL}")));
            }
        }
        if already_on(&files) {
            return Ok(FastMtree::AlreadyOn);
        }
        progress.log("the restore ramdisk does not turn the fast mtree path on yet: adding the option");
        let written = write_options(&work, files)?;
        if !already_on(&read_options(&work)?) {
            return Err(Error::msg("the option did not read back from the rewritten restore ramdisk"));
        }
        // The new image under its final name's `.part`, the manifest, then the rename.
        let staged = ramdisk.with_extension("options-new");
        im4p::repack_rdsk(&ramdisk, &work, &staged)?;
        let digest = im4p::retyped_digest(&staged, im4p::retype_tag("RestoreRamDisk").expect("it has a tag"))?;
        let changed = set_ramdisk_digest(&mut manifest, &name, &digest)?;
        let tmp = manifest_path.with_extension("plist.part");
        fs::write(&tmp, plist_py::to_xml(&manifest)).map_err(|e| Error::path(&tmp, e))?;
        fs::rename(&tmp, &manifest_path).map_err(|e| Error::path(&manifest_path, e))?;
        fs::rename(&staged, &ramdisk).map_err(|e| Error::path(&ramdisk, e))?;
        progress.log(&format!("restore ramdisk rewritten; its digest updated in {changed} build identities"));
        Ok(FastMtree::TurnedOn(written))
    })();
    let _ = fs::remove_file(&work);
    let _ = fs::remove_file(ramdisk.with_extension("options-new"));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(pairs: &[(&str, bool)]) -> Option<Dictionary> {
        let mut d = Dictionary::new();
        for (k, v) in pairs {
            d.insert((*k).to_owned(), Value::Boolean(*v));
        }
        Some(d)
    }

    #[test]
    fn the_option_must_be_in_every_options_file_and_in_the_plain_one() {
        assert!(!already_on(&[]), "no file at all");
        assert!(already_on(&[("options.plist".into(), dict(&[(FAST_MTREE, true)]))]));
        assert!(!already_on(&[("options.plist".into(), dict(&[(FAST_MTREE, false)]))]));
        assert!(!already_on(&[("options.plist".into(), None)]), "an unreadable file");
        // A board's file is read instead of the plain one, so it needs the option as well.
        let board = ("options.vresearch101.plist".to_owned(), dict(&[("Other", true)]));
        assert!(!already_on(&[("options.plist".into(), dict(&[(FAST_MTREE, true)])), board]));
        assert!(!already_on(&[("options.j1.plist".into(), dict(&[(FAST_MTREE, true)]))]), "no plain file");
        assert!(is_options_file("options.plist") && is_options_file("options.n104.plist"));
        assert!(!is_options_file("options.plist.bak") && !is_options_file("restore.plist"));
    }

    #[test]
    fn only_the_named_ramdisk_gets_the_new_digest() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>BuildIdentities</key><array>
<dict><key>Manifest</key><dict><key>RestoreRamDisk</key><dict><key>Digest</key><data>AAAA</data><key>Info</key><dict><key>Path</key><string>a.dmg</string></dict></dict></dict></dict>
<dict><key>Manifest</key><dict><key>RestoreRamDisk</key><dict><key>Digest</key><data>AAAA</data><key>Info</key><dict><key>Path</key><string>b.dmg</string></dict></dict></dict></dict>
<dict><key>Manifest</key><dict><key>OS</key><dict/></dict></dict>
</array></dict></plist>"#;
        let mut manifest = Value::from_reader(std::io::Cursor::new(&xml[..])).unwrap();
        assert_eq!(set_ramdisk_digest(&mut manifest, "a.dmg", &[7u8; 48]).unwrap(), 1);
        let ids = manifest.as_dictionary().unwrap()["BuildIdentities"].as_array().unwrap();
        let digest = |i: usize| ids[i].as_dictionary().unwrap()["Manifest"].as_dictionary().unwrap()["RestoreRamDisk"].as_dictionary().unwrap()["Digest"].as_data().unwrap().to_vec();
        assert_eq!(digest(0), vec![7u8; 48]);
        assert_eq!(digest(1), vec![0u8; 3]);
    }
}
