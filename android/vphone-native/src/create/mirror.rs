//! A helper's own log, followed into its step's log: the restore writes to a file of its own, and
//! its lines are the only sign of how far it has got.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

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

/// A long stage of the restore that only the guest's own log shows, and what to say during it.
pub fn guest_stage(line: &str) -> Option<&'static str> {
    line.contains("executing /usr/sbin/mtree")
        .then_some("Verifying every iOS system file (one CPU, no progress reported; this can take a long time)")
}

/// How far the guest's image restore has got: `ASR RESTORE PROGRESS: 40%`, anywhere in a line.
pub fn asr_percent(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once("ASR RESTORE PROGRESS:")?;
    let rest = rest.trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    if !rest[end..].starts_with('%') {
        return None;
    }
    rest[..end].parse().ok().filter(|percent| *percent <= 100)
}

/// The window the speed and the time left are measured over: long enough to span several percent.
const RATE_WINDOW_SECS: f64 = 120.0;
/// Less than this says nothing about the rate.
const RATE_MIN_SECS: f64 = 20.0;

/// What the image restore's percentages say about its speed.
pub struct RestoreRate {
    start: Instant,
    /// Seconds since `start`, percent, bytes the disk image holds.
    samples: VecDeque<(f64, u64, Option<u64>)>,
}

impl RestoreRate {
    pub fn new() -> RestoreRate {
        RestoreRate { start: Instant::now(), samples: VecDeque::new() }
    }

    /// Time spent paused is not restore time.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// The line to show for a percentage, if it is a new one.
    pub fn update(&mut self, percent: u64, disk: Option<u64>) -> Option<String> {
        self.update_at(self.start.elapsed().as_secs_f64(), percent, disk)
    }

    fn update_at(&mut self, at: f64, percent: u64, disk: Option<u64>) -> Option<String> {
        match self.samples.back() {
            Some(last) if last.1 == percent => return None,
            Some(last) if last.1 > percent => self.samples.clear(),
            _ => {}
        }
        self.samples.push_back((at, percent, disk));
        while self.samples.len() > 2 && self.samples[0].0 < at - RATE_WINDOW_SECS {
            self.samples.pop_front();
        }
        let &(first_at, first_percent, first_disk) = self.samples.front()?;
        let span = at - first_at;
        let gained = percent - first_percent;
        let mut text = format!("Writing iOS to the disk: {percent}%");
        if span < RATE_MIN_SECS || gained == 0 {
            text += " \u{b7} measuring speed";
            return Some(text);
        }
        if let (Some(first), Some(now)) = (first_disk, disk) {
            if now > first {
                let per_sec = (now - first) as f64 / span / 1_000_000.0;
                text += &if per_sec < 10.0 {
                    format!(" \u{b7} {per_sec:.1} MB/s")
                } else {
                    format!(" \u{b7} {per_sec:.0} MB/s")
                };
            }
        }
        if percent < 100 {
            let left = (100 - percent) as f64 * span / gained as f64;
            text += &format!(" \u{b7} about {} left", duration(left));
        }
        Some(text)
    }
}

fn duration(secs: f64) -> String {
    let minutes = (secs / 60.0).ceil() as u64;
    match minutes {
        0..=1 => "1 min".to_string(),
        2..=59 => format!("{minutes} min"),
        _ => format!("{} h {} min", minutes / 60, minutes % 60),
    }
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

    /// Follows `path` from its beginning: a VM's serial log, fresh with each boot.
    pub fn from_start(path: &Path) -> Mirror {
        Mirror { offset: 0, ..Mirror::new(path) }
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
    fn names_the_long_verify_stage() {
        assert!(guest_stage("executing /usr/sbin/mtree -p /mnt1 -m /mnt5/x -f /mnt5/y").is_some());
        assert_eq!(guest_stage("Fast mtree path not enabled."), None);
    }

    #[test]
    fn reads_the_image_restore_s_percentage() {
        assert_eq!(asr_percent("ASR RESTORE PROGRESS: 40%"), Some(40));
        assert_eq!(asr_percent("[guest] ASR RESTORE PROGRESS: 100%"), Some(100));
        assert_eq!(asr_percent("ASR RESTORE PROGRESS: 101%"), None);
        assert_eq!(asr_percent("ASR RESTORE PROGRESS: soon"), None);
        assert_eq!(asr_percent("Restoring image (13)"), None);
    }

    #[test]
    fn the_rate_is_measured_and_a_repeat_or_a_restart_is_handled() {
        const GB: u64 = 1_000_000_000;
        let mut r = RestoreRate::new();
        assert!(r.update_at(0.0, 10, Some(GB)).unwrap().ends_with("10% \u{b7} measuring speed"));
        assert_eq!(r.update_at(5.0, 10, Some(GB)), None, "the same percentage again");
        assert!(r.update_at(10.0, 11, Some(GB)).unwrap().contains("measuring"), "too soon to say");
        // 10 points in 60 s, and 600 MB written: 10 MB/s, and 80 points left at 6 s each.
        assert_eq!(
            r.update_at(60.0, 20, Some(GB + 600_000_000)).unwrap(),
            "Writing iOS to the disk: 20% \u{b7} 10 MB/s \u{b7} about 8 min left"
        );
        assert_eq!(
            r.update_at(600.0, 100, Some(2 * GB)).unwrap(),
            "Writing iOS to the disk: 100% \u{b7} 0.7 MB/s"
        );
        let text = r.update_at(700.0, 5, None).unwrap();
        assert!(text.ends_with("5% \u{b7} measuring speed"), "a new image starts over: {text}");
    }

    #[test]
    fn only_a_recent_window_sets_the_rate() {
        let mut r = RestoreRate::new();
        r.update_at(0.0, 1, None);
        r.update_at(1000.0, 2, None);
        // 1 point a minute lately, despite the slow start.
        let text = r.update_at(1060.0, 3, None).unwrap();
        assert!(text.ends_with("about 1 h 37 min left"), "{text}");
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
