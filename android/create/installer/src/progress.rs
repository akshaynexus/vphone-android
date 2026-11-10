//! Progress reporting, cancellation and the resumable per-VM step record.

use crate::bail;
use crate::error::{Error, Result};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What the steps tell the caller. The orchestrator implements this.
pub trait Progress: Send + Sync {
    /// A line for the step log.
    fn log(&self, line: &str);
    /// Progress within the current step; `total == 0` when unknown.
    fn progress(&self, done: u64, total: u64, message: &str);
    /// A named phase of the step began, and the one before it ended.
    fn phase(&self, _key: &str, _title: &str) {}
    /// Polled between units of work; true makes the step return `Error::Cancelled`.
    fn cancelled(&self) -> bool {
        false
    }
}

/// A `Progress` that discards everything. For callers with nothing to report.
pub struct Silent;

impl Progress for Silent {
    fn log(&self, _line: &str) {}
    fn progress(&self, _done: u64, _total: u64, _message: &str) {}
}

/// Check the cancel flag; `Err(Cancelled)` when set.
pub fn check_cancel(progress: &dyn Progress) -> Result<()> {
    if progress.cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// The steps done for one VM: a text file of keys, one per line, kept in the VM
/// directory so a re-run resumes where the last one stopped. A key is written
/// only after its step returned Ok.
pub struct StepRecord {
    path: PathBuf,
    done: BTreeSet<String>,
}

impl StepRecord {
    pub const FILE_NAME: &'static str = ".create-steps";

    pub fn open(vm: &Path) -> Result<Self> {
        let path = vm.join(Self::FILE_NAME);
        let done = match fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(Error::path(&path, e)),
        };
        Ok(Self { path, done })
    }

    pub fn is_done(&self, key: &str) -> bool {
        self.done.contains(key)
    }

    pub fn mark(&mut self, key: &str) -> Result<()> {
        if key.contains('\n') {
            bail!("step key contains a newline: {key:?}");
        }
        if self.done.insert(key.to_owned()) {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|e| Error::path(&self.path, e))?;
            writeln!(f, "{key}").map_err(|e| Error::path(&self.path, e))?;
            f.sync_all().map_err(|e| Error::path(&self.path, e))?;
        }
        Ok(())
    }

    /// Forget keys starting with `prefix`, so their steps run again.
    pub fn forget_prefix(&mut self, prefix: &str) -> Result<()> {
        self.done.retain(|k| !k.starts_with(prefix));
        let tmp = self.path.with_extension("tmp");
        let mut text = String::new();
        for k in &self.done {
            text.push_str(k);
            text.push('\n');
        }
        fs::write(&tmp, text).map_err(|e| Error::path(&tmp, e))?;
        fs::rename(&tmp, &self.path).map_err(|e| Error::path(&self.path, e))?;
        Ok(())
    }

    /// Run `f` unless `key` is recorded, recording `key` when it returns Ok.
    pub fn step<T: Default>(
        &mut self,
        progress: &dyn Progress,
        key: &str,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if self.is_done(key) {
            progress.log(&format!("  • {key}  (already done)"));
            return Ok(T::default());
        }
        progress.log(&format!("▶ {key}"));
        let value = f()?;
        self.mark(key)?;
        Ok(value)
    }
}
