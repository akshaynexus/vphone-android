//! Port of tools/restore-tree-mode.sh: switch a restore tree between a full
//! restore and the SSH ramdisk, with the retyped-image manifest digests.

use crate::bail;
use crate::error::{Error, Result};
use crate::im4p;
use crate::plist_py;
use crate::progress::Progress;
use crate::trustcache;
use plist::{Dictionary, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The single `*_Restore` directory of a VM bundle.
pub fn restore_tree(vm: &Path) -> Result<PathBuf> {
    let mut trees = Vec::new();
    let rd = fs::read_dir(vm).map_err(|e| Error::path(vm, e))?;
    for entry in rd {
        let entry = entry.map_err(|e| Error::path(vm, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with("_Restore") && entry.path().is_dir() {
            trees.push(entry.path());
        }
    }
    if trees.len() != 1 {
        bail!("expected one *_Restore tree in {}, found {}", vm.display(), trees.len());
    }
    Ok(trees.pop().unwrap())
}

fn load(path: &Path) -> Result<Value> {
    Value::from_file(path).map_err(|e| Error::msg(format!("{}: {e}", path.display())))
}

fn identities(manifest: &Value) -> Result<&Vec<Value>> {
    manifest
        .as_dictionary()
        .and_then(|d| d.get("BuildIdentities"))
        .and_then(Value::as_array)
        .ok_or_else(|| Error::msg("BuildManifest has no BuildIdentities"))
}

fn info_path(component: &Dictionary) -> Option<&str> {
    component.get("Info")?.as_dictionary()?.get("Path")?.as_string().filter(|s| !s.is_empty())
}

/// `component_path`: the manifest component's path, from the first identity that has it.
pub fn component_path(manifest: &Value, key: &str) -> Result<String> {
    for identity in identities(manifest)? {
        let component = identity
            .as_dictionary()
            .and_then(|d| d.get("Manifest"))
            .and_then(Value::as_dictionary)
            .and_then(|m| m.get(key))
            .and_then(Value::as_dictionary);
        if let Some(path) = component.and_then(info_path) {
            return Ok(path.to_owned());
        }
    }
    bail!("BuildManifest has no {key}");
}

/// The artifacts `ssh` mode installs into the tree.
pub struct SshArtifacts<'a> {
    /// The rebuilt SSH ramdisk image.
    pub ramdisk: &'a Path,
    /// The stock restore trust cache (kept beside the stock ramdisk).
    pub stock_trustcache: &'a Path,
    /// The staging tree whose tools the trust cache admits.
    pub staging: &'a Path,
}

fn save(path: &Path) -> Result<()> {
    let backup = PathBuf::from(format!("{}.full-restore", path.display()));
    if !backup.exists() {
        fs::copy(path, &backup).map_err(|e| Error::path(&backup, e))?;
    }
    Ok(())
}

fn backup_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.full-restore", path.display()))
}

/// The manifest edit of `ssh` mode on an already-parsed manifest: point
/// RestoreKernelCache at the research kernel and set the digests of the four
/// retyped components. Returns the rewritten plist bytes.
pub fn apply_ssh_digests(manifest: &mut Value, tree: &Path, kernel: &str) -> Result<Vec<u8>> {
    let array = manifest
        .as_dictionary_mut()
        .and_then(|d| d.get_mut("BuildIdentities"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::msg("BuildManifest has no BuildIdentities"))?;
    for identity in array {
        let man = identity
            .as_dictionary_mut()
            .and_then(|d| d.get_mut("Manifest"))
            .and_then(Value::as_dictionary_mut)
            .ok_or_else(|| Error::msg("BuildIdentity has no Manifest"))?;
        if let Some(c) = man.get_mut("RestoreKernelCache").and_then(Value::as_dictionary_mut) {
            let info = c
                .get_mut("Info")
                .and_then(Value::as_dictionary_mut)
                .ok_or_else(|| Error::msg("RestoreKernelCache has no Info"))?;
            info.insert("Path".into(), Value::String(kernel.to_owned()));
        }
        for key in [
            "RestoreKernelCache",
            "RestoreRamDisk",
            "RestoreTrustCache",
            "Ap,RestoreTrustedExecutionMonitor",
        ] {
            let Some(c) = man.get_mut(key).and_then(Value::as_dictionary_mut) else {
                continue;
            };
            let Some(path) = info_path(c).map(str::to_owned) else {
                continue;
            };
            if !c.contains_key("Digest") {
                continue;
            }
            let tag = im4p::retype_tag(key).expect("every key above has a tag");
            let digest = im4p::retyped_digest(&tree.join(&path), tag)?;
            c.insert("Digest".into(), Value::Data(digest.to_vec()));
        }
    }
    Ok(plist_py::to_xml(manifest))
}

/// `restore-tree-mode.sh <lib> <vm> ssh`.
///
/// `patch_txm` runs `fw patch --restore-txm-precheck` (a helper of the host): it
/// returns Err when the tool fails, and `restore-tree-mode.sh`'s rule applies: a
/// failure after TXM changed is tolerated, an unchanged TXM is not.
pub fn set_ssh_mode(
    vm: &Path,
    artifacts: &SshArtifacts,
    patch_txm: &mut dyn FnMut() -> Result<()>,
    progress: &dyn Progress,
) -> Result<()> {
    for f in [
        artifacts.ramdisk.to_path_buf(),
        artifacts.stock_trustcache.to_path_buf(),
        artifacts.staging.join("usr/local/bin/dropbear"),
    ] {
        if !f.exists() {
            bail!("missing {}: build the SSH ramdisk first", f.display());
        }
    }
    let tree = restore_tree(vm)?;
    let m = tree.join("BuildManifest.plist");
    let mut manifest = load(&m)?;
    let rd = component_path(&manifest, "RestoreRamDisk")?;
    let tc = component_path(&manifest, "RestoreTrustCache")?;
    let txm = component_path(&manifest, "Ap,RestoreTrustedExecutionMonitor")?;
    let saved = [m.clone(), tree.join(&rd), tree.join(&tc), tree.join(&txm)];
    for f in &saved {
        save(f)?;
    }
    progress.log(&format!("=== SSH ramdisk -> {rd}"));
    fs::copy(artifacts.ramdisk, tree.join(&rd)).map_err(|e| Error::path(&tree.join(&rd), e))?;
    progress.log(&format!("=== trust cache -> {tc}"));
    let stock = fs::read(artifacts.stock_trustcache).map_err(|e| Error::path(artifacts.stock_trustcache, e))?;
    let built = trustcache::build(&stock, artifacts.staging, &tree.join(&tc), progress)?;
    progress.log(&format!(
        "entries: stock {}, added {}, total {}",
        built.stock, built.added, built.total
    ));
    progress.log("=== TXM selector-24 pre-check -> PASS");
    // Always patch the full-restore TXM, so running this twice is safe.
    let txm_path = tree.join(&txm);
    let txm_backup = backup_of(&txm_path);
    fs::copy(&txm_backup, &txm_path).map_err(|e| Error::path(&txm_path, e))?;
    if let Err(error) = patch_txm() {
        let before = fs::read(&txm_backup).map_err(|e| Error::path(&txm_backup, e))?;
        let after = fs::read(&txm_path).map_err(|e| Error::path(&txm_path, e))?;
        if before == after {
            bail!("TXM was not patched: {error}");
        }
        progress.log(&format!(
            "(fw patch's manifest refresh failed after patching TXM; continuing: {error})"
        ));
    }
    let kernel = component_path(&manifest, "KernelCache")?;
    progress.log(&format!("=== restore kernel -> {kernel} (research)"));
    let bytes = apply_ssh_digests(&mut manifest, &tree, &kernel)?;
    fs::write(&m, bytes).map_err(|e| Error::path(&m, e))?;
    progress.log(&format!("=== {} is in ssh mode", tree.display()));
    Ok(())
}

/// `restore-tree-mode.sh <lib> <vm> full`.
pub fn set_full_mode(vm: &Path, progress: &dyn Progress) -> Result<()> {
    let tree = restore_tree(vm)?;
    let m = tree.join("BuildManifest.plist");
    let manifest = load(&m)?;
    let rd = component_path(&manifest, "RestoreRamDisk")?;
    let tc = component_path(&manifest, "RestoreTrustCache")?;
    let txm = component_path(&manifest, "Ap,RestoreTrustedExecutionMonitor")?;
    let saved = [m, tree.join(rd), tree.join(tc), tree.join(txm)];
    for f in &saved {
        if !backup_of(f).exists() {
            bail!("no {}.full-restore: tree is already in full mode?", f.display());
        }
    }
    for f in &saved {
        fs::rename(backup_of(f), f).map_err(|e| Error::path(f, e))?;
    }
    progress.log(&format!("=== {} is in full mode", tree.display()));
    Ok(())
}
