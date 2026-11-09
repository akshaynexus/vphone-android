//! Running the app's helper executables (`lib*.so` in nativeLibraryDir) for a step: their output
//! in the step's log, cancellable, under a time limit, in their own process group so a stop also
//! stops what they started.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Exec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub log: PathBuf,
    pub timeout: Option<Duration>,
    /// restore-wait.sh: once the output contains any of these, the program has this long to exit
    /// by itself; then it is stopped and the run counts as a success.
    pub done_when: Option<(Vec<String>, Duration)>,
}

impl Exec {
    fn command(&self) -> Result<Command, String> {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|e| format!("cannot open {}: {e}", self.log.display()))?;
        let _ = writeln!(log, "\n+ {} {}", self.program.display(), self.args.join(" "));
        let out = log.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(log);
        // SAFETY: setsid is async-signal-safe and allowed between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(cmd)
    }

    fn start(&self) -> Result<Child, String> {
        self.command()?
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.program.display()))
    }

    /// Runs to the end; the exit code, or an error for a cancel or a time-out.
    pub fn run(&self, cancelled: &dyn Fn() -> bool) -> Result<i32, String> {
        let mut scan_from = fs::metadata(&self.log).map_or(0, |m| m.len());
        let mut child = self.start()?;
        let started = Instant::now();
        let mut finished_at: Option<Instant> = None;
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)));
            }
            if cancelled() {
                stop_group(&mut child);
                return Err("cancelled".into());
            }
            if self.timeout.is_some_and(|t| started.elapsed() > t) {
                stop_group(&mut child);
                return Err(format!(
                    "{} did not finish within {} minutes",
                    self.program.file_name().unwrap_or_default().to_string_lossy(),
                    self.timeout.unwrap_or_default().as_secs() / 60
                ));
            }
            if let Some((needles, grace)) = &self.done_when {
                if finished_at.is_none() && scan(&self.log, &mut scan_from, needles) {
                    finished_at = Some(Instant::now());
                }
                if finished_at.is_some_and(|t| t.elapsed() >= *grace) {
                    stop_group(&mut child);
                    return Ok(0);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Starts it in the background; dropping the handle stops it.
    pub fn spawn(&self) -> Result<Background, String> {
        Ok(Background { child: self.start()? })
    }
}

pub struct Background {
    child: Child,
}

impl Background {
    pub fn exited(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(s)) => Some(s.code().unwrap_or(1)),
            _ => None,
        }
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        stop_group(&mut self.child);
    }
}

fn stop_group(child: &mut Child) {
    let pid = child.id() as i32;
    // SAFETY: signals are sent only to the group this child created with setsid.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    // SAFETY: as above.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let _ = child.wait();
}

/// Whether the log, from `from`, now contains any needle; keeps `from` moving, keeping a tail so
/// a needle split across two reads is still found.
fn scan(log: &Path, from: &mut u64, needles: &[String]) -> bool {
    let Ok(mut f) = File::open(log) else {
        return false;
    };
    if f.seek(SeekFrom::Start(*from)).is_err() {
        return false;
    }
    let mut new = Vec::new();
    if f.read_to_end(&mut new).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&new).to_ascii_lowercase();
    *from += new.len().saturating_sub(128) as u64;
    needles.iter().any(|n| text.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(args: &[&str], log: PathBuf) -> Exec {
        Exec {
            program: "/bin/sh".into(),
            args: std::iter::once("-c".to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .collect(),
            env: vec![("FOO".into(), "bar".into())],
            cwd: PathBuf::from("/tmp"),
            log,
            timeout: None,
            done_when: None,
        }
    }

    fn log(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/exec");
        fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn logs_output_and_returns_the_code() {
        let p = log("ok");
        assert_eq!(exec(&["echo hello $FOO; exit 3"], p.clone()).run(&|| false), Ok(3));
        let text = fs::read_to_string(p).unwrap();
        assert!(text.contains("hello bar"), "{text}");
    }

    #[test]
    fn cancel_stops_the_whole_group() {
        let p = log("cancel");
        let start = Instant::now();
        let e = exec(&["sleep 30 & sleep 30"], p);
        let err = e.run(&move || start.elapsed() > Duration::from_millis(300)).unwrap_err();
        assert_eq!(err, "cancelled");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn times_out() {
        let p = log("timeout");
        let mut e = exec(&["sleep 30"], p);
        e.timeout = Some(Duration::from_millis(200));
        assert!(e.run(&|| false).unwrap_err().contains("did not finish"));
    }

    #[test]
    fn a_finished_restore_ends_after_its_grace() {
        let p = log("done-when");
        let mut e = exec(&["echo 'Status: Restore Finished'; sleep 30"], p);
        e.done_when = Some((vec!["restore finished".into()], Duration::from_millis(500)));
        let start = Instant::now();
        assert_eq!(e.run(&|| false), Ok(0));
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
