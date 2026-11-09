//! The SSH ramdisk: tools/ssh-ramdisk-stage.sh and ssh-ramdisk-rebuild-signed.sh,
//! with the APFS library in place of a loop mount.

use crate::bail;
use crate::error::{Error, Result};
use crate::im4p;
use crate::manifest;
use crate::net;
use crate::progress::Progress;
use crate::resources::{SSHTARS_SHA256, SSHTARS_URL};
use flate2::read::GzDecoder;
use plist::Value;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// Everything the SSH ramdisk build makes, under one directory per VM.
pub struct Ramdisk {
    pub dir: PathBuf,
}

impl Ramdisk {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    pub fn staging(&self) -> PathBuf {
        self.dir.join("ssh-root")
    }
    fn stock(&self) -> PathBuf {
        self.dir.join("stock-ramdisk.dmg")
    }
    pub fn stock_trustcache(&self) -> PathBuf {
        self.dir.join("stock-ramdisk.dmg.trustcache")
    }
    fn base(&self) -> PathBuf {
        self.dir.join("base.apfs")
    }
    pub fn output(&self) -> PathBuf {
        self.dir.join("sshrd.dmg")
    }
}

/// `tools/ssh-ramdisk-stage.sh`: fetch sshtars (pinned, checked) and unpack it.
pub fn stage(rd: &Ramdisk, cache: &Path, progress: &dyn Progress) -> Result<()> {
    let out = rd.staging();
    if out.join("usr/local/bin/dropbear").exists() {
        return Ok(());
    }
    let tarball = cache.join("ssh.tar.gz");
    net::fetch_verified(SSHTARS_URL, &tarball, SSHTARS_SHA256, progress)?;
    unpack_staging(&tarball, &out)?;
    progress.log(&format!("=== {} unpacked", out.display()));
    Ok(())
}

/// Unpack the tools, dropping what the staging script drops.
pub fn unpack_staging(tarball: &Path, out: &Path) -> Result<()> {
    if out.exists() {
        fs::remove_dir_all(out).map_err(|e| Error::path(out, e))?;
    }
    fs::create_dir_all(out).map_err(|e| Error::path(out, e))?;
    let file = File::open(tarball).map_err(|e| Error::path(tarball, e))?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    archive.set_preserve_ownerships(false);
    archive.set_preserve_permissions(true);
    archive.unpack(out).map_err(|e| Error::path(out, e))?;
    let _ = fs::remove_file(out.join("usr/share/misc/magic.mgc"));
    let trollstore = out.join("usr/trollstore");
    if trollstore.exists() {
        fs::remove_dir_all(&trollstore).map_err(|e| Error::path(&trollstore, e))?;
    }
    Ok(())
}

fn component_paths(tree: &Path) -> Result<(String, String)> {
    let path = tree.join("BuildManifest.plist");
    let manifest = Value::from_file(&path).map_err(|e| Error::msg(format!("{}: {e}", path.display())))?;
    Ok((
        manifest::component_path(&manifest, "RestoreRamDisk")?,
        manifest::component_path(&manifest, "RestoreTrustCache")?,
    ))
}

/// `tools/ssh-ramdisk-rebuild-signed.sh`: the stock restore ramdisk with the SSH
/// tools added, repacked in its IM4P wrapper. Run with the tree in full mode.
pub fn rebuild(vm: &Path, rd: &Ramdisk, progress: &dyn Progress) -> Result<()> {
    let tree = manifest::restore_tree(vm)?;
    let (ramdisk, trustcache) = component_paths(&tree)?;
    let stock = rd.stock();
    fs::create_dir_all(&rd.dir).map_err(|e| Error::path(&rd.dir, e))?;
    if !stock.exists() {
        if PathBuf::from(format!("{}.full-restore", tree.join(&ramdisk).display())).exists() {
            bail!("tree is in ssh mode; switch it to full first");
        }
        fs::copy(tree.join(&ramdisk), &stock).map_err(|e| Error::path(&stock, e))?;
        fs::copy(tree.join(&trustcache), rd.stock_trustcache()).map_err(|e| Error::path(&rd.stock_trustcache(), e))?;
    }
    if !rd.staging().join("usr/local/bin/dropbear").exists() {
        bail!("missing the staging tree: run stage first");
    }
    let base = rd.base();
    if !base.exists() {
        im4p::extract_rdsk_payload(&stock, &base)?;
    }
    let work = rd.dir.join("work.apfs");
    fs::copy(&base, &work).map_err(|e| Error::path(&work, e))?;
    progress.log("=== inject the staging tree");
    let apfs = |e: vphone_apfs::Error| Error::Apfs(e.to_string());
    {
        let mut volume = vphone_apfs::MutVolume::open(&work, &Default::default()).map_err(apfs)?;
        vphone_apfs::inject_ramdisk(&mut volume, &rd.staging()).map_err(apfs)?;
        volume.commit().map_err(apfs)?;
    }
    progress.log("=== repack into the stock rdsk IM4P wrapper");
    im4p::repack_rdsk(&stock, &work, &rd.output())?;
    fs::remove_file(&work).map_err(|e| Error::path(&work, e))?;
    progress.log(&format!("=== built {}", rd.output().display()));
    Ok(())
}
