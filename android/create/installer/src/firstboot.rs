//! What the first boot is doing, said as it goes: its phases, where the guest's CPU time goes
//! every half minute, and vphoned's `trimmed` service profile.
//!
//! - **Phases.** [`PHASES`], in order; each begins where the one before it ends
//!   ([`Progress::phase`]). The host times them and estimates the rest from its own earlier runs.
//! - **Load lines.** One line every [`SAMPLE_EVERY`], marked [`LOAD_MARK`] so that no log filter
//!   takes a process's name for an error: where the vCPUs were (the host's QMP samples, from the
//!   first second), and once vphoned answers, how far setup is (`setup.status`) and which guest
//!   processes used the most CPU since the line before (`processes.list` twice, as
//!   tools/guest-top.py reads it).
//! - **The service profile** (tools/service-profile.py; vphone-cli's
//!   `Research/Guest/service_trimming.md`): a list of launchd jobs vphoned disables with the
//!   override `launchctl disable` writes on the data volume. launchd honours it when it next
//!   loads the job, so a job already loaded keeps running until the guest restarts. It is asked
//!   for as soon as vphoned answers, so that whatever launchd has not loaded by then stays
//!   unloaded in this boot too, and the restart that ends setup comes up trimmed; and once more
//!   after Setup Assistant is skipped, for the two sign-in follow-up jobs upstream applies only
//!   once the first boot's work is done.

use crate::host::Host;
use crate::progress::Progress;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The first boot's phases: the key its times are kept under, and what the screen calls it.
pub const PHASES: [(&str, &str); 6] = [
    ("kernel", "Kernel boot"),
    ("launchd", "launchd and the guest agent start"),
    ("migration", "iOS migrates its data"),
    ("setup", "Skip Setup Assistant"),
    ("unlock", "Unlock"),
    ("finish", "Settings, restart and boot logo"),
];

/// What every load line starts with.
pub const LOAD_MARK: &str = "[load] ";
pub const SAMPLE_EVERY: Duration = Duration::from_secs(30);
const TOP: usize = 5;
const RPC: Duration = Duration::from_secs(60);

/// The daemon upstream's tool always keeps: without it an idle iOS 27 guest spins launchd.
const KEEP: &str = "com.apple.generativeexperiencesd";
/// vphoned's `signin_followup` group, which belongs after the first boot's work.
const AFTER_SETUP: [&str; 2] = ["com.apple.appleidsetupd", "com.apple.followupd"];

pub fn phase(progress: &dyn Progress, key: &str) {
    if let Some((key, title)) = PHASES.iter().find(|(k, _)| *k == key) {
        progress.phase(key, title);
    }
}

/// CPU seconds by process, from `processes.list`.
pub type CpuTimes = HashMap<(i64, String), f64>;

pub fn cpu_times(reply: &Value) -> Option<CpuTimes> {
    let list = reply.get("processes")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|p| {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("?").to_owned();
                Some(((p.get("pid")?.as_i64()?, name), p.get("cpu_seconds").and_then(Value::as_f64).unwrap_or(0.0)))
            })
            .collect(),
    )
}

/// `2.31 of 6 guest CPUs: installd 0.92, DataMigrator 0.71, ...; 412 processes`: the CPUs' worth
/// of time the guest's processes used between two samples `secs` apart, and who used the most. A
/// process that started in between counts from zero; one that ended is not seen.
pub fn top_line(before: &CpuTimes, after: &CpuTimes, secs: f64) -> String {
    let secs = secs.max(0.001);
    let mut rows: Vec<(f64, &str)> = after
        .iter()
        .map(|(key, cpu)| (cpu - before.get(key).copied().unwrap_or(0.0), key.1.as_str()))
        .filter(|(used, _)| *used > 0.0)
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
    let total = rows.iter().fold(0.0, |sum, r| sum + r.0);
    let top: Vec<String> = rows.iter().take(TOP).map(|(used, name)| format!("{name} {:.2}", used / secs)).collect();
    format!(
        "{:.2} guest CPUs busy: {}; {} processes",
        total / secs,
        if top.is_empty() { "none used any".to_owned() } else { top.join(", ") },
        after.len()
    )
}

/// `setup.status` in a few words.
pub fn setup_words(status: &Value) -> String {
    let flag = |key: &str| status.get(key).and_then(Value::as_bool);
    let migration = match flag("data_migration_done") {
        Some(true) => "migration done",
        Some(false) => "migration running",
        None => "migration state not reported",
    };
    let setup = match (flag("pending"), flag("running")) {
        (Some(false), _) => ", Setup Assistant done",
        (_, Some(true)) => ", Setup Assistant on screen",
        (Some(true), _) => ", Setup Assistant pending",
        _ => "",
    };
    format!("{migration}{setup}")
}

/// What `services.profile.apply` did, in one line.
pub fn profile_words(reply: &Value) -> String {
    let count = |key: &str| match reply.get(key) {
        Some(Value::Array(list)) => list.len() as u64,
        Some(other) => other.as_u64().unwrap_or(0),
        None => 0,
    };
    let mut line = format!(
        "service profile trimmed (list version {}): {} background daemons disabled now, {} already were, {} kept on",
        reply.get("list_version").and_then(Value::as_u64).unwrap_or(0),
        count("disabled"),
        count("kept"),
        count("allowed")
    );
    let failed = count("failed");
    if failed > 0 {
        line += &format!(", {failed} could not be changed");
    }
    line
}

/// Applies the `trimmed` profile, keeping `keep` besides [`KEEP`]. Never fails the boot: an agent
/// without the profile, or an iOS with no list, is said and setup goes on untrimmed.
pub fn apply_profile(host: &dyn Host, after_setup: bool) {
    let mut allow = vec![KEEP];
    if !after_setup {
        allow.extend(AFTER_SETUP);
    }
    match host.vphoned_rpc("services.profile.apply", json!({"profile": "trimmed", "allow": allow, "force": true}), Duration::from_secs(120)) {
        Ok(reply) => {
            host.log(&profile_words(&reply));
            // What the profile owns and launchd had loaded before it was asked: these ran in this
            // boot all the same, and go at the restart.
            if let Ok(state) = host.vphoned_rpc("services.profile", json!({}), RPC) {
                let running = state.get("running").and_then(Value::as_array).map_or(0, Vec::len);
                host.log(&format!(
                    "service profile: {running} of its daemons are running in this boot (loaded before it was applied); they stay off from the next boot"
                ));
            }
        }
        Err(why) => host.log(&format!("the service profile was not applied, so this VM keeps every background daemon: {why}")),
    }
}

/// The half-minute sampler, on a thread of its own for as long as the first boot's waits last.
pub struct Monitor<'a> {
    host: &'a dyn Host,
    stop: AtomicBool,
    /// Set once vphoned answered: from then on the phases are the main flow's.
    agent_up: Mutex<bool>,
}

impl<'a> Monitor<'a> {
    pub fn new(host: &'a dyn Host) -> Monitor<'a> {
        Monitor { host, stop: AtomicBool::new(false), agent_up: Mutex::new(false) }
    }

    /// vphoned answered: the `launchd` phase, if it was not seen starting, is over either way.
    pub fn agent_answered(&self) {
        *self.agent_up.lock().unwrap() = true;
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn run(&self) {
        let started = Instant::now();
        let mut userspace = false;
        let mut last_line = Instant::now();
        let mut last_top: Option<(Instant, CpuTimes)> = None;
        while !self.stop.load(Ordering::SeqCst) && !self.host.cancelled() {
            std::thread::sleep(Duration::from_secs(1));
            if !userspace && self.host.guest_userspace_seen() {
                userspace = true;
                let up = self.agent_up.lock().unwrap();
                if !*up {
                    phase(self.host, "launchd");
                }
            }
            if last_line.elapsed() < SAMPLE_EVERY || !self.host.vm_alive() {
                continue;
            }
            last_line = Instant::now();
            let mut parts = vec![format!("t+{}s", started.elapsed().as_secs())];
            let agent = *self.agent_up.lock().unwrap();
            if agent {
                match self.host.vphoned_rpc("setup.status", json!({}), RPC) {
                    Ok(status) => parts.push(setup_words(&status)),
                    Err(_) => parts.push("the guest agent did not answer".to_owned()),
                }
                let now = Instant::now();
                match self.host.vphoned_rpc("processes.list", json!({}), RPC).ok().as_ref().and_then(cpu_times) {
                    Some(times) => {
                        if let Some((then, before)) = &last_top {
                            parts.push(top_line(before, &times, now.duration_since(*then).as_secs_f64()));
                        }
                        last_top = Some((now, times));
                    }
                    None => parts.push("no process list this time".to_owned()),
                }
            } else {
                parts.push("the guest agent has not answered yet".to_owned());
            }
            let vcpus = self.host.vcpu_note();
            if !vcpus.is_empty() {
                parts.push(vcpus);
            }
            self.host.log(&format!("{LOAD_MARK}{}", parts.join(" | ")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(rows: &[(i64, &str, f64)]) -> CpuTimes {
        rows.iter().map(|(pid, name, cpu)| ((*pid, (*name).to_owned()), *cpu)).collect()
    }

    #[test]
    fn the_process_list_is_read_as_guest_top_reads_it() {
        let reply = json!({"processes": [
            {"pid": 1, "name": "launchd", "cpu_seconds": 12.5, "footprint_bytes": 1},
            {"pid": 40, "name": "installd"},
            {"name": "no pid"},
        ]});
        assert_eq!(cpu_times(&reply).unwrap(), times(&[(1, "launchd", 12.5), (40, "installd", 0.0)]));
        assert!(cpu_times(&json!({"error": "x"})).is_none());
    }

    #[test]
    fn the_top_line_names_who_used_the_cpus_between_two_samples() {
        let before = times(&[(1, "launchd", 10.0), (40, "installd", 100.0), (41, "DataMigrator", 50.0), (9, "idle", 3.0)]);
        // A pid reused by another process counts from zero, as does one that started in between.
        let after = times(&[(1, "launchd", 13.0), (40, "installd", 130.0), (41, "mediaanalysisd", 6.0), (77, "searchd", 1.5), (9, "idle", 3.0)]);
        assert_eq!(
            top_line(&before, &after, 30.0),
            "1.35 guest CPUs busy: installd 1.00, mediaanalysisd 0.20, launchd 0.10, searchd 0.05; 5 processes"
        );
        assert_eq!(top_line(&after, &after, 30.0), "0.00 guest CPUs busy: none used any; 5 processes");
        let many = times(&[(1, "a", 6.0), (2, "b", 5.0), (3, "c", 4.0), (4, "d", 3.0), (5, "e", 2.0), (6, "f", 1.0)]);
        let line = top_line(&CpuTimes::new(), &many, 1.0);
        assert!(line.starts_with("21.00 guest CPUs busy: a 6.00, b 5.00, c 4.00, d 3.00, e 2.00; 6"), "{line}");
    }

    #[test]
    fn setup_status_and_the_profile_reply_are_put_in_words() {
        assert_eq!(setup_words(&json!({"data_migration_done": false, "pending": true, "running": false})), "migration running, Setup Assistant pending");
        assert_eq!(setup_words(&json!({"data_migration_done": true, "pending": true, "running": true})), "migration done, Setup Assistant on screen");
        assert_eq!(setup_words(&json!({"data_migration_done": true, "pending": false})), "migration done, Setup Assistant done");
        assert_eq!(setup_words(&json!({})), "migration state not reported");
        let reply = json!({"list_version": 3, "disabled": ["a", "b"], "kept": ["c"], "allowed": ["k"], "failed": [{"label": "x"}]});
        assert_eq!(
            profile_words(&reply),
            "service profile trimmed (list version 3): 2 background daemons disabled now, 1 already were, 1 kept on, 1 could not be changed"
        );
        assert!(!profile_words(&json!({"disabled": 136, "kept": 0, "allowed": 1})).contains("could not"));
    }

    #[test]
    fn every_phase_has_its_own_key() {
        let mut keys: Vec<&str> = PHASES.iter().map(|p| p.0).collect();
        keys.dedup();
        assert_eq!(keys.len(), PHASES.len());
    }
}
