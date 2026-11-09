//! A helper's own log, followed into its step's log: the restore writes to a file of its own, and
//! its lines are the only sign of how far it has got.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The stages of a restore, in order: a line containing the text starts that stage.
const STAGES: [(&str, &str); 12] = [
    ("Trying to fetch new SHSH", "Asking Apple to sign the restore"),
    ("Sending iBSS", "Loading the first boot stage"),
    ("Sending iBEC", "Loading the second boot stage"),
    ("Sending RestoreRamDisk", "Loading the restore system"),
    ("Waiting for device to enter restore mode", "Waiting for the restore system to boot"),
    ("successfully entered restore mode", "The restore system is running"),
    ("Creating partition map", "Preparing the virtual disk"),
    ("Validating the filesystem", "Checking the system image"),
    ("Sending filesystem now", "Writing the system to the disk"),
    ("Flashing firmware", "Installing firmware"),
    ("About to send Ap,SecurePageTableMonitor", "Installing the boot files"),
    ("Restore Finished", "Restore finished"),
];

pub const STAGE_COUNT: u64 = STAGES.len() as u64;

/// The stage a line starts: its number from 1, and what to show.
pub fn stage_of(line: &str) -> Option<(u64, &'static str)> {
    STAGES
        .iter()
        .position(|(needle, _)| line.contains(needle))
        .map(|i| (i as u64 + 1, STAGES[i].1))
}

/// The MiB the USB link reports having sent: `vusbd: sent 16 MiB in 3.2 s`.
pub fn sent_mib(line: &str) -> Option<u64> {
    line.strip_prefix("vusbd: sent ")?.split(' ').next()?.parse().ok()
}

pub struct Mirror {
    path: PathBuf,
    /// Kept open once found: the file's VM can be deleted before its last lines have been read.
    file: Option<File>,
    offset: u64,
    partial: Vec<u8>,
    stage: u64,
}

impl Mirror {
    /// Follows `path` from its current end, so an earlier attempt's lines are not repeated.
    pub fn new(path: &Path) -> Mirror {
        Mirror {
            path: path.to_path_buf(),
            file: File::open(path).ok(),
            offset: std::fs::metadata(path).map_or(0, |m| m.len()),
            partial: Vec::new(),
            stage: 0,
        }
    }

    /// The complete lines written since the last call.
    pub fn drain(&mut self) -> Vec<String> {
        if self.file.is_none() {
            self.file = File::open(&self.path).ok();
        }
        let Some(file) = self.file.as_mut() else {
            return Vec::new();
        };
        let mut new = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).is_err() || file.read_to_end(&mut new).is_err() {
            return Vec::new();
        }
        self.offset += new.len() as u64;
        self.partial.extend_from_slice(&new);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        String::from_utf8_lossy(&complete)
            .lines()
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect()
    }

    /// The new stage this line reaches, if it moves forward. Stages repeat in a log; a restore
    /// never goes back.
    pub fn advance(&mut self, line: &str) -> Option<(u64, &'static str)> {
        let (stage, text) = stage_of(line)?;
        (stage > self.stage).then(|| {
            self.stage = stage;
            (stage, text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Lines in the order a real cloudOS restore wrote them.
    const REAL: &str = "idevicerestore 1.1.0\nTrying to fetch new SHSH blob\nSending iBSS (380445 bytes)...\nSending iBEC (380475 bytes)...\nSending RestoreRamDisk (232789583 bytes)...\nWaiting for device to enter restore mode...\nDevice 0000FE01 has successfully entered restore mode\nCreating partition map (11)\nMounting filesystems (16)\nValidating the filesystem\nSending filesystem now...\nFlashing firmware (18)\nAbout to send Ap,SecurePageTableMonitor...\nStatus: Restore Finished\n";

    #[test]
    fn a_real_restore_moves_forward_through_every_stage() {
        let mut m = Mirror { path: PathBuf::new(), file: None, offset: 0, partial: Vec::new(), stage: 0 };
        let seen: Vec<u64> = REAL.lines().filter_map(|l| m.advance(l).map(|s| s.0)).collect();
        assert_eq!(seen, (1..=STAGE_COUNT).collect::<Vec<_>>());
        assert_eq!(m.advance("Sending iBSS (380445 bytes)..."), None, "it never goes back");
    }

    #[test]
    fn reads_the_usb_link_s_progress() {
        assert_eq!(sent_mib("vusbd: sent 16 MiB in 3.2 s"), Some(16));
        assert_eq!(sent_mib("vusbd: listening on /x"), None);
    }

    #[test]
    fn follows_new_complete_lines_only() {
        let dir = std::env::temp_dir().join(format!("vphone-mirror-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log");
        std::fs::write(&path, "from an earlier attempt\n").unwrap();
        let mut m = Mirror::new(&path);
        assert!(m.drain().is_empty(), "an earlier attempt is not repeated");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "first\nhalf a li").unwrap();
        f.flush().unwrap();
        assert_eq!(m.drain(), vec!["first"]);
        writeln!(f, "ne").unwrap();
        f.flush().unwrap();
        assert_eq!(m.drain(), vec!["half a line"]);
        writeln!(f, "the error").unwrap();
        f.flush().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(m.drain(), vec!["the error"], "a deleted file's last lines are still read");
        let _ = std::fs::remove_dir_all(dir);
    }
}
