//! Run-time assets: vphone-cli's release (OpenAVB, OpenSEP and the guest
//! binaries), and SSHRD_Script's sshtars. Nothing here is committed or bundled.

use crate::error::{Error, Result};
use crate::net;
use crate::progress::Progress;
use std::fs::{self, File};
use std::io::{self};
use std::path::{Path, PathBuf};

pub const VPHONE_RELEASE_VERSION: &str = "2.14.2";
pub const VPHONE_RELEASE_URL: &str = "https://github.com/Lakr233/vphone-cli/releases/download/2.14.2/VPhone-2.14.2.zip";
pub const VPHONE_RELEASE_SHA256: &str = "80212fc06b9d2269d8ed76ac12ed09c5bc9f44b88a53dec905e5ed5fcd428923";

pub const SSHTARS_URL: &str = "https://github.com/verygenericname/sshtars/raw/9f9551a14a268148d920370635e9c1bce33bf49c/ssh.tar.gz";
pub const SSHTARS_SHA256: &str = "ddfa230acd2789c7e61ddb0d2ec3df9a6c741f2ddcab58fc1a826d278be1d74d";

const RESOURCE_PREFIX: &str = "VPhone.bundle/Contents/Resources/";

/// The unpacked vphone-cli release assets.
#[derive(Clone, Debug)]
pub struct Release {
    pub root: PathBuf,
}

impl Release {
    pub fn open_avb(&self) -> PathBuf {
        self.root.join("OpenAVB/OpenAVB.bin")
    }
    pub fn open_sep(&self) -> PathBuf {
        self.root.join("OpenSEP/OpenSEP.bin")
    }
    pub fn guest(&self, name: &str) -> PathBuf {
        self.root.join("guest-resources").join(name)
    }
}

/// Which release members are kept: the two ROMs, the guest payloads, and what `libvphone_cli.so`
/// reads through `VPHONE_RESOURCES`.
fn wanted(name: &str) -> Option<&str> {
    const KEPT: [&str; 6] = [
        "OpenAVB/",
        "OpenSEP/",
        "guest-resources/",
        "kernelcache_recipes/",
        "patches_presets/",
        "scripts/",
    ];
    let rel = name.strip_prefix(RESOURCE_PREFIX)?;
    KEPT.iter().any(|dir| rel.starts_with(dir)).then_some(rel)
}

/// Download (once, verified) and unpack vphone-cli's release into `<dir>/release`.
pub fn fetch_release(dir: &Path, progress: &dyn Progress) -> Result<Release> {
    let root = dir.join("release");
    let done = root.join(".unpacked");
    if done.exists() {
        return Ok(Release { root });
    }
    let zip_path = dir.join(format!("VPhone-{VPHONE_RELEASE_VERSION}.zip"));
    net::fetch_verified(VPHONE_RELEASE_URL, &zip_path, VPHONE_RELEASE_SHA256, progress)?;
    unpack_release(&zip_path, &root, progress)?;
    fs::write(&done, b"").map_err(|e| Error::path(&done, e))?;
    Ok(Release { root })
}

/// Unpack only the wanted members of the release zip. Symlinks in the zip are
/// not followed: every member is written as a plain file under `root`.
pub fn unpack_release(zip_path: &Path, root: &Path, progress: &dyn Progress) -> Result<()> {
    let file = File::open(zip_path).map_err(|e| Error::path(zip_path, e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| Error::msg(format!("{}: {e}", zip_path.display())))?;
    if root.exists() {
        fs::remove_dir_all(root).map_err(|e| Error::path(root, e))?;
    }
    for i in 0..archive.len() {
        crate::progress::check_cancel(progress)?;
        let mut entry = archive.by_index(i).map_err(|e| Error::msg(format!("{}: {e}", zip_path.display())))?;
        let name = entry.name().to_owned();
        let Some(rel) = wanted(&name) else { continue };
        if rel.split('/').any(|c| c == "..") {
            continue;
        }
        let out = root.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| Error::path(&out, e))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::path(parent, e))?;
        }
        let mut o = File::create(&out).map_err(|e| Error::path(&out, e))?;
        io::copy(&mut entry, &mut o).map_err(|e| Error::path(&out, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                fs::set_permissions(&out, fs::Permissions::from_mode(mode & 0o777))
                    .map_err(|e| Error::path(&out, e))?;
            }
        }
    }
    Ok(())
}
