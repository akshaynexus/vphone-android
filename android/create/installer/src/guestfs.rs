//! Guest file operations rooted at a mounted volume (/mnt1, /mnt7, /mnt6), over
//! SFTP: the Rust counterpart of what the installer did on its sshfs mounts.

use crate::bail;
use crate::error::{Error, Result};
use crate::ssh::GuestSsh;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::{fs::File, RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::AsyncWriteExt;

static TMP_COUNTER: AtomicU32 = AtomicU32::new(0);
const S_IFMT: u32 = 0o170000;

/// Reject anything that could step out of the volume.
fn check_relative(relative: &str) -> Result<()> {
    if relative.is_empty()
        || relative.starts_with('/')
        || relative.contains('\0')
        || relative.split('/').any(|c| c.is_empty() || c == "." || c == "..")
    {
        bail!("invalid guest path: {relative:?}");
    }
    Ok(())
}

fn sftp_err(context: &str, e: SftpError) -> Error {
    Error::Sftp(format!("{context}: {e}"))
}

fn missing(e: &SftpError) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == StatusCode::NoSuchFile)
}

/// File attributes of a guest path.
#[derive(Clone, Copy, Debug)]
pub struct Attrs {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
}

impl Attrs {
    fn from(a: &FileAttributes) -> Self {
        Self {
            mode: a.permissions.unwrap_or(0),
            uid: a.uid.unwrap_or(0),
            gid: a.gid.unwrap_or(0),
            size: a.size.unwrap_or(0),
        }
    }
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == 0o040000
    }
    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == 0o100000
    }
    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == 0o120000
    }
    pub fn perm(&self) -> u32 {
        self.mode & 0o7777
    }
}

/// `'`-quote a path for the guest's shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The contents of a file being installed.
enum Content<'c> {
    Bytes(&'c [u8]),
    Local(&'c Path),
}

/// One mounted guest volume.
pub struct GuestVolume<'a> {
    ssh: &'a GuestSsh,
    sftp: &'a SftpSession,
    raw: &'a RawSftpSession,
    root: String,
}

impl<'a> GuestVolume<'a> {
    pub fn new(ssh: &'a GuestSsh, sftp: &'a SftpSession, raw: &'a RawSftpSession, root: &str) -> Self {
        Self {
            ssh,
            sftp,
            raw,
            root: root.trim_end_matches('/').to_owned(),
        }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn path(&self, relative: &str) -> Result<String> {
        check_relative(relative)?;
        Ok(format!("{}/{}", self.root, relative))
    }

    /// `lstat`; `None` when the path does not exist.
    pub fn status(&self, relative: &str) -> Result<Option<Attrs>> {
        let path = self.path(relative)?;
        match self.ssh.block_on(self.sftp.symlink_metadata(path.clone())) {
            Ok(a) => Ok(Some(Attrs::from(&a))),
            Err(e) if missing(&e) => Ok(None),
            Err(e) => Err(sftp_err(&path, e)),
        }
    }

    pub fn exists(&self, relative: &str) -> Result<bool> {
        Ok(self.status(relative)?.is_some())
    }

    pub fn is_dir(&self, relative: &str) -> Result<bool> {
        Ok(self.status(relative)?.is_some_and(|a| a.is_dir()))
    }

    pub fn is_regular(&self, relative: &str) -> Result<bool> {
        Ok(self.status(relative)?.is_some_and(|a| a.is_file()))
    }

    pub fn is_symlink(&self, relative: &str) -> Result<bool> {
        Ok(self.status(relative)?.is_some_and(|a| a.is_symlink()))
    }

    pub fn read_link(&self, relative: &str) -> Result<String> {
        let path = self.path(relative)?;
        self.ssh
            .block_on(self.sftp.read_link(path.clone()))
            .map_err(|e| sftp_err(&path, e))
    }

    /// The names and attributes in a directory (`""` is the volume root), sorted.
    pub fn list(&self, relative: &str) -> Result<Vec<(String, Attrs)>> {
        let path = if relative.is_empty() {
            self.root.clone()
        } else {
            self.path(relative)?
        };
        self.list_absolute(&path)
    }

    fn list_absolute(&self, path: &str) -> Result<Vec<(String, Attrs)>> {
        let dir = self
            .ssh
            .block_on(self.sftp.read_dir(path.to_owned()))
            .map_err(|e| sftp_err(path, e))?;
        let mut out: Vec<(String, Attrs)> = dir
            .map(|e| (e.file_name(), Attrs::from(&e.metadata())))
            .filter(|(n, _)| n != "." && n != "..")
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    pub fn entries(&self, relative: &str) -> Result<Vec<String>> {
        Ok(self.list(relative)?.into_iter().map(|e| e.0).collect())
    }

    pub fn read_data(&self, relative: &str) -> Result<Vec<u8>> {
        let path = self.path(relative)?;
        self.ssh
            .block_on(self.sftp.read(path.clone()))
            .map_err(|e| sftp_err(&path, e))
    }

    async fn write_content(file: &mut File, content: Content<'_>) -> Result<()> {
        match content {
            Content::Bytes(bytes) => file.write_all(bytes).await.map_err(|e| Error::Sftp(format!("write: {e}"))),
            Content::Local(path) => {
                let mut local = tokio::fs::File::open(path).await.map_err(|e| Error::path(path, e))?;
                tokio::io::copy(&mut local, file)
                    .await
                    .map(|_| ())
                    .map_err(|e| Error::Sftp(format!("{}: {e}", path.display())))
            }
        }
    }

    /// `posix-rename@openssh.com`: unlike SSH_FXP_RENAME, replaces an existing destination.
    async fn posix_rename(&self, from: &str, to: &str) -> Result<()> {
        let mut data = Vec::with_capacity(from.len() + to.len() + 8);
        for s in [from, to] {
            data.extend_from_slice(&(s.len() as u32).to_be_bytes());
            data.extend_from_slice(s.as_bytes());
        }
        self.raw
            .extended("posix-rename@openssh.com", data)
            .await
            .map(|_| ())
            .map_err(|e| sftp_err(&format!("rename {from} -> {to}"), e))
    }

    /// Write `<relative>`'s bytes to a temporary beside it and rename it into place.
    fn install(&self, relative: &str, mode: u32, owner: (u32, u32), content: Content<'_>) -> Result<()> {
        let path = self.path(relative)?;
        let (dir, leaf) = path.rsplit_once('/').unwrap();
        let temporary = format!(
            "{dir}/.{leaf}.tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let outcome: Result<()> = self.ssh.block_on(async {
            let mut file = self
                .sftp
                .open_with_flags(
                    temporary.clone(),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
                )
                .await
                .map_err(|e| sftp_err(&temporary, e))?;
            Self::write_content(&mut file, content).await?;
            file.flush().await.map_err(|e| Error::Sftp(format!("flush: {e}")))?;
            // Owner first: chown clears set-ID bits, so the mode has to come after.
            let attrs = FileAttributes {
                uid: Some(owner.0),
                gid: Some(owner.1),
                ..FileAttributes::empty()
            };
            file.set_metadata(attrs).await.map_err(|e| sftp_err(&temporary, e))?;
            let attrs = FileAttributes {
                permissions: Some(mode),
                ..FileAttributes::empty()
            };
            file.set_metadata(attrs).await.map_err(|e| sftp_err(&temporary, e))?;
            file.sync_all().await.map_err(|e| sftp_err(&temporary, e))?;
            file.shutdown().await.map_err(|e| Error::Sftp(format!("close: {e}")))?;
            self.posix_rename(&temporary, &path).await
        });
        if outcome.is_err() {
            let _ = self.ssh.block_on(self.sftp.remove_file(temporary));
        }
        outcome
    }

    /// Create or atomically replace a file with `data`.
    pub fn write_data(&self, relative: &str, data: &[u8], mode: u32, owner: (u32, u32)) -> Result<()> {
        self.install(relative, mode, owner, Content::Bytes(data))
    }

    /// Create or atomically replace a file with a local file's contents.
    pub fn replace_file(&self, relative: &str, local: &Path, mode: u32, owner: (u32, u32)) -> Result<()> {
        self.install(relative, mode, owner, Content::Local(local))
    }

    /// Copy a guest file to a local path.
    pub fn download(&self, relative: &str, local: &Path) -> Result<()> {
        let path = self.path(relative)?;
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::path(parent, e))?;
        }
        self.ssh.block_on(async {
            let mut remote = self.sftp.open(path.clone()).await.map_err(|e| sftp_err(&path, e))?;
            let mut out = tokio::fs::File::create(local).await.map_err(|e| Error::path(local, e))?;
            tokio::io::copy(&mut remote, &mut out)
                .await
                .map(|_| ())
                .map_err(|e| Error::Sftp(format!("{path}: {e}")))
        })
    }

    /// Copy a guest file onto another guest path, keeping its mode and owner.
    pub fn copy_within(&self, from: &str, to: &str) -> Result<()> {
        let attrs = self
            .status(from)?
            .ok_or_else(|| Error::msg(format!("guest file missing: {from}")))?;
        let data = self.read_data(from)?;
        self.write_data(to, &data, attrs.perm(), (attrs.uid, attrs.gid))
    }

    pub fn mkdir_p(&self, relative: &str, mode: u32) -> Result<()> {
        check_relative(relative)?;
        let mut built = self.root.clone();
        for component in relative.split('/') {
            built.push('/');
            built.push_str(component);
            let attrs = FileAttributes {
                permissions: Some(mode),
                ..FileAttributes::empty()
            };
            let made = self.ssh.block_on(self.raw.mkdir(built.clone(), attrs));
            if let Err(e) = made {
                let is_dir = self
                    .ssh
                    .block_on(self.sftp.symlink_metadata(built.clone()))
                    .map(|a| Attrs::from(&a).is_dir())
                    .unwrap_or(false);
                if !is_dir {
                    return Err(sftp_err(&built, e));
                }
            }
        }
        Ok(())
    }

    /// Remove a file, link or whole tree, never following links.
    pub fn remove_item(&self, relative: &str) -> Result<()> {
        let Some(attrs) = self.status(relative)? else {
            return Ok(());
        };
        let path = self.path(relative)?;
        self.remove_at(&path, attrs)
    }

    fn remove_at(&self, path: &str, attrs: Attrs) -> Result<()> {
        if attrs.is_dir() {
            for (name, child) in self.list_absolute(path)? {
                self.remove_at(&format!("{path}/{name}"), child)?;
            }
            self.ssh
                .block_on(self.sftp.remove_dir(path.to_owned()))
                .map_err(|e| sftp_err(path, e))
        } else {
            self.ssh
                .block_on(self.sftp.remove_file(path.to_owned()))
                .map_err(|e| sftp_err(path, e))
        }
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (self.path(from)?, self.path(to)?);
        self.ssh.block_on(self.posix_rename(&from, &to))
    }

    /// Create a symlink at `at` pointing at `target`, replacing a stale entry.
    pub fn create_symlink(&self, target: &str, at: &str) -> Result<()> {
        if self.exists(at)? {
            if self.is_symlink(at)? && self.read_link(at)? == target {
                return Ok(());
            }
            self.remove_item(at)?;
        }
        let link = self.path(at)?;
        // OpenSSH's sftp-server takes SSH_FXP_SYMLINK as (target, link path).
        self.ssh
            .block_on(self.sftp.symlink(target.to_owned(), link.clone()))
            .map_err(|e| sftp_err(&link, e))
    }

    pub fn set_mode(&self, relative: &str, mode: u32) -> Result<()> {
        self.setstat(
            relative,
            FileAttributes {
                permissions: Some(mode),
                ..FileAttributes::empty()
            },
        )
    }

    pub fn set_owner(&self, relative: &str, uid: u32, gid: u32) -> Result<()> {
        self.setstat(
            relative,
            FileAttributes {
                uid: Some(uid),
                gid: Some(gid),
                ..FileAttributes::empty()
            },
        )
    }

    fn setstat(&self, relative: &str, attrs: FileAttributes) -> Result<()> {
        let path = self.path(relative)?;
        self.ssh
            .block_on(self.sftp.set_metadata(path.clone(), attrs))
            .map_err(|e| sftp_err(&path, e))
    }
}
