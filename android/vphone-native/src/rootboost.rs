//! "Root boost": on a rooted phone (KernelSU first, Magisk and APatch too), `root-boost.sh` runs as
//! root beside the VM and lifts what an app cannot: the vendor's clock caps, the GPU's and memory
//! buses' clocks, core parking, the priority and placement of QEMU's threads, the low-memory
//! killer. What it does and why is in the script. It ends, giving everything back, when the stop
//! file appears or the app's process is gone.
//!
//! KernelSU has no prompt: the app must be allowed root in the KernelSU manager (Superuser), and a
//! `su` the manager refuses fails at once. Its `su` is reached at `/system/bin/su` by the apps it
//! allows.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SCRIPT: &str = include_str!("root-boost.sh");

/// Where each root manager's `su` is found by the apps it allows.
const SU: [&str; 4] = ["/system/bin/su", "/system/xbin/su", "/debug_ramdisk/su", "su"];

fn stop_file(dir: &Path) -> PathBuf {
    dir.join("root-boost.stop")
}

/// Starts the boost for this process. Each line the script prints goes to `report`, and so does
/// the reason when there is no root to run it with.
pub fn start(dir: &Path, report: fn(&str)) {
    let script = dir.join("root-boost.sh");
    let _ = std::fs::remove_file(stop_file(dir));
    if let Err(e) = std::fs::write(&script, SCRIPT) {
        report(&format!("root boost: OFF (cannot write {}: {e})", script.display()));
        return;
    }
    let command = format!("sh '{}' {} '{}'", script.display(), std::process::id(), dir.display());
    let mut tried = Vec::new();
    for su in SU {
        match Command::new(su).arg("-c").arg(&command).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let su = su.to_string();
                let _ = std::thread::Builder::new().name("root-boost".into()).spawn(move || {
                    if let Some(err) = stderr {
                        let _ = std::thread::Builder::new().name("root-boost-err".into()).spawn(move || {
                            for line in BufReader::new(err).lines().map_while(Result::ok) {
                                report(&format!("root boost (su): {line}"));
                            }
                        });
                    }
                    let mut said = false;
                    if let Some(out) = stdout {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            said = true;
                            report(&line);
                        }
                    }
                    match child.wait() {
                        Ok(status) if !said => report(&format!(
                            "root boost: OFF ({su} ended with {status} before the script ran: allow this app root in \
                             the KernelSU manager's Superuser list, or in Magisk or APatch, then start the VM again)"
                        )),
                        Ok(status) if !status.success() => report(&format!("root boost ended: {status}")),
                        Ok(_) => {}
                        Err(e) => report(&format!("root boost: cannot wait for {su}: {e}")),
                    }
                });
                return;
            }
            Err(e) => tried.push(format!("{su}: {e}")),
        }
    }
    report(&format!("root boost: OFF (this phone has no su this app can run; {})", tried.join("; ")));
}

/// The VM ended: the script gives everything back within its next tick.
pub fn stop(dir: &Path) {
    let _ = std::fs::write(stop_file(dir), b"");
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_script_is_plain_sh() {
        let s = super::SCRIPT;
        assert!(s.starts_with("#!/system/bin/sh\n"));
        assert!(!s.contains('\r'));
        assert!(s.contains("give clocks") && s.contains("root-boost.stop"));
    }
}
