//! Which of iOS's background services a VM runs, set from the VM's settings and applied by the
//! guest's agent each time the setting has changed since it was last applied.
//!
//! The 0.6.0 log's guest had 5.2 to 5.9 of its 6 CPUs busy at rest, with BackgroundShortcutRunner
//! (0.6-0.8 of a CPU), momentsd (0.67), locationd (0.58), the Weather and World Clock widgets
//! (0.2-0.4 each) and sharingd (0.16-0.27) beside SpringBoard and backboardd: work that competes
//! with the screen for emulated CPUs.
//!
//! - [`Level::Trimmed`] is vphoned's `trimmed` profile, the about 137 daemons creation's
//!   "fewer background services" option disables (`installer/src/firstboot.rs`), plus
//!   [`TRIMMED_EXTRA`].
//! - [`Level::Fewest`] adds [`FEWEST_EXTRA`], which costs features and has not been run.
//!
//! vphoned writes launchd's `disable` override on the guest's data volume, so a change takes
//! effect when iOS next starts; the daemons already running keep running until then. What this
//! app disabled beyond the profile is kept in `<vm>/services.json`, so that a lower level turns
//! exactly those back on. A VM whose services were trimmed at creation and never changed here
//! has no such file and is left as it is under [`Level::Unchanged`].
//!
//! Not reachable this way, by vphoned's own notes (vphone-cli `Research/Guest/service_trimming.md`):
//! BackgroundShortcutRunner is an XPC service that siriactionsd starts for the widgets' and
//! controls' App Intents, not a launchd daemon, and it goes quiet with the widgets; locationd is
//! on the profile's never-disable list because vphoned's own location features and its locationd
//! watchdog use it.
//!
//! The labels could not be checked against an iOS 27 `launchd.plist` on the build machine (it
//! has no iOS system image). `com.apple.momentsd` is the label docs/ADVANCED.md used on the
//! desktop test VM; the two of [`FEWEST_EXTRA`] are from Apple's naming and unverified.
//! `launchctl disable` accepts a label launchd does not have, so a wrong one does nothing.

use crate::vphoned::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// The daemon upstream's tool always keeps: without it an idle iOS 27 guest spins launchd.
const KEEP: &str = "com.apple.generativeexperiencesd";
/// Disabled with the profile: Journal's suggestions, 0.67 of a guest CPU in the 0.6.0 log.
pub const TRIMMED_EXTRA: [&str; 1] = ["com.apple.momentsd"];
/// Disabled besides, for [`Level::Fewest`]: AirDrop, Handoff and the share sheet's nearby
/// people (sharingd), and every Home Screen and Lock Screen widget (chronod, WidgetKit's
/// daemon, which runs the widget extensions and so their App Intents in
/// BackgroundShortcutRunner). Unverified: iOS may retry a missing daemon; `guest top` shows it.
pub const FEWEST_EXTRA: [&str; 2] = ["com.apple.sharingd", "com.apple.chronod"];
const FILE: &str = "services.json";
const RPC: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Whatever the VM has: nothing is asked of the guest unless a level was applied before.
    #[default]
    Unchanged,
    Trimmed,
    Fewest,
}

impl Level {
    /// The labels disabled one by one on top of vphoned's profile.
    pub fn extra(self) -> Vec<&'static str> {
        match self {
            Level::Unchanged => Vec::new(),
            Level::Trimmed => TRIMMED_EXTRA.to_vec(),
            Level::Fewest => TRIMMED_EXTRA.iter().chain(&FEWEST_EXTRA).copied().collect(),
        }
    }

    fn profile(self) -> &'static str {
        match self {
            Level::Unchanged => "none",
            _ => "trimmed",
        }
    }
}

/// What this app last applied to the VM.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Applied {
    pub level: Level,
    /// The labels disabled beyond the profile, to turn back on when the level no longer has them.
    pub extra: Vec<String>,
}

impl Applied {
    pub fn load(vm: &Path) -> Applied {
        std::fs::read(vm.join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, vm: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(vm.join(FILE), json).map_err(|e| e.to_string())
    }
}

/// The calls that bring a VM from `applied` to `want`, in order: the profile, the labels to turn
/// back on, the labels to disable. Empty when nothing has changed.
pub fn plan(applied: &Applied, want: Level) -> Vec<(&'static str, Value)> {
    let extra = want.extra();
    if applied.level == want && applied.extra.iter().map(String::as_str).eq(extra.iter().copied()) {
        return Vec::new();
    }
    let mut calls = vec![(
        "services.profile.apply",
        if want == Level::Unchanged {
            json!({"profile": want.profile(), "force": true})
        } else {
            json!({"profile": want.profile(), "allow": [KEEP], "force": true})
        },
    )];
    for label in applied.extra.iter().filter(|l| !extra.contains(&l.as_str())) {
        calls.push(("services.enable", json!({"label": label})));
    }
    for label in &extra {
        calls.push(("services.disable", json!({"label": label, "force": true})));
    }
    calls
}

/// Applies `want` to the running guest if it differs from what was applied; one or two lines
/// through `say`. Never fails the boot: what could not be asked is said and tried at the next.
pub fn apply(client: &Client, vm: &Path, want: Level, say: &dyn Fn(&str)) {
    let applied = Applied::load(vm);
    let calls = plan(&applied, want);
    if calls.is_empty() {
        if want != Level::Unchanged {
            say(&format!("services: {want:?}, as applied at an earlier start"));
        }
        return;
    }
    let mut failed = Vec::new();
    for (method, params) in &calls {
        match client.rpc(method, params.clone(), RPC) {
            Ok(reply) if *method == "services.profile.apply" && want != Level::Unchanged => {
                say(&format!("services: {}", vphone_create_steps::firstboot::profile_words(&reply)));
            }
            Ok(_) => {}
            Err(why) => failed.push(why),
        }
    }
    if let Some(first) = failed.first() {
        // The profile is the first call: without it nothing is recorded, and the next start
        // asks again.
        say(&format!("services: {} of {} changes were refused, the first: {first}; asked again at the next start", failed.len(), calls.len()));
        return;
    }
    let extra = want.extra();
    let record = Applied { level: want, extra: extra.iter().map(|l| (*l).to_string()).collect() };
    if let Err(why) = record.save(vm) {
        say(&format!("services: applied, but not recorded ({why})"));
    }
    say(&match want {
        Level::Unchanged => "services: every daemon this app had disabled is enabled again; it takes effect when iOS next starts".to_string(),
        _ => format!(
            "services: {want:?} applied, with {} more disabled ({}); it takes effect when iOS next starts: shut the VM down and start it again",
            extra.len(),
            extra.join(", ")
        ),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn methods(calls: &[(&'static str, Value)]) -> Vec<String> {
        calls
            .iter()
            .map(|(m, p)| format!("{m} {}", p.get("label").or(p.get("profile")).and_then(Value::as_str).unwrap_or("")))
            .collect()
    }

    #[test]
    fn a_vm_nobody_changed_is_left_alone() {
        assert!(plan(&Applied::default(), Level::Unchanged).is_empty());
        // And one already at its level is not asked again.
        let trimmed = Applied { level: Level::Trimmed, extra: vec!["com.apple.momentsd".into()] };
        assert!(plan(&trimmed, Level::Trimmed).is_empty());
    }

    #[test]
    fn trimming_applies_the_profile_then_the_extra_labels() {
        let calls = plan(&Applied::default(), Level::Trimmed);
        assert_eq!(methods(&calls), ["services.profile.apply trimmed", "services.disable com.apple.momentsd"]);
        assert_eq!(calls[0].1, json!({"profile": "trimmed", "allow": ["com.apple.generativeexperiencesd"], "force": true}));
        assert_eq!(calls[1].1["force"], true);
        let fewest = plan(&Applied::default(), Level::Fewest);
        assert_eq!(
            methods(&fewest),
            [
                "services.profile.apply trimmed",
                "services.disable com.apple.momentsd",
                "services.disable com.apple.sharingd",
                "services.disable com.apple.chronod"
            ]
        );
        // Nothing vphoned's profile refuses to disable is asked for by label.
        for label in Level::Fewest.extra() {
            assert!(!["com.apple.locationd", "com.apple.siriactionsd", "com.apple.SpringBoard", "com.apple.backboardd"].contains(&label));
        }
    }

    #[test]
    fn a_lower_level_turns_back_on_exactly_what_this_app_disabled() {
        let fewest = Applied { level: Level::Fewest, extra: Level::Fewest.extra().iter().map(|l| (*l).to_string()).collect() };
        assert_eq!(
            methods(&plan(&fewest, Level::Trimmed)),
            [
                "services.profile.apply trimmed",
                "services.enable com.apple.sharingd",
                "services.enable com.apple.chronod",
                "services.disable com.apple.momentsd"
            ]
        );
        let back = plan(&fewest, Level::Unchanged);
        assert_eq!(
            methods(&back),
            [
                "services.profile.apply none",
                "services.enable com.apple.momentsd",
                "services.enable com.apple.sharingd",
                "services.enable com.apple.chronod"
            ]
        );
        assert_eq!(back[0].1, json!({"profile": "none", "force": true}));
    }

    #[test]
    fn the_record_survives_a_restart_and_a_missing_one_is_unchanged() {
        let dir = std::env::temp_dir().join(format!("vphone-services-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Applied::load(&dir), Applied::default());
        let record = Applied { level: Level::Trimmed, extra: vec!["com.apple.momentsd".into()] };
        record.save(&dir).unwrap();
        assert_eq!(Applied::load(&dir), record);
        assert!(std::fs::read_to_string(dir.join(FILE)).unwrap().contains(r#""level": "trimmed""#));
        // A list of extras from an older build that differs is applied again.
        let older = Applied { level: Level::Trimmed, extra: Vec::new() };
        assert!(!plan(&older, Level::Trimmed).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
