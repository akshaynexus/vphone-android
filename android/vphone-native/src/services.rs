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
//! **Where the labels are from, and what is known of them.** The build machine has no iOS system
//! image, so none was read from an iPhone's iOS 27 `launchd.plist` here. Instead each start that
//! applies a level asks the guest's launchd (`services.list`, `services.disabled`) and sorts
//! every label ([`sort`]): disabled now, already off, or not in this iOS. A label launchd does
//! not have is not asked for, and the log names it.
//!
//! - `com.apple.momentsd` and `com.apple.modelcatalogd`: labels docs/ADVANCED.md disabled on the
//!   desktop test VM.
//! - `com.apple.coreduetd`, `com.apple.modelcatalogd`, `com.apple.mediastream.mstreamd`:
//!   UniXNU-sheng's `package/BootWatchdogPolicy.c` (`d28d3b0`), whose labels are "the ones in the
//!   target's own LaunchDaemons" of iPadOS 27.0.1 (24A446). There coreduetd kept one thread above
//!   half a core for the whole boot. Their effect in this app's guest is not measured.
//! - [`FEWEST_EXTRA`]'s `com.apple.fairplayd.H2` is the label iOSPlus10 drops on iOS 16
//!   (`scripts/ios16/root-tweaks-extra.py`, `146e889`) and the name of the binary vphone-cli lists
//!   in this firmware (`usr/sbin/fairplayd.H2`); UniXNU-sheng's tablet has `.A2`, a respawn loop
//!   there. `com.apple.mobile.obliteration` is UniXNU-sheng's; iOS 27 marks it
//!   `DisableAllowed = false` in the jetsam properties, which the CFW removes. `com.apple.spindump`,
//!   `com.apple.tailspind`, `com.apple.aggregated` and `com.apple.activityawardsd` are iOSPlus10's
//!   iOS 16 labels. `com.apple.apsd` is the usual label of the push daemon, named in
//!   macOS-DroidVM's notes. `com.apple.sharingd` and `com.apple.chronod` are from Apple's naming.
//!   None of these is verified on iOS 27, and each costs a feature.

use crate::vphoned::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

/// The daemon upstream's tool always keeps: without it an idle iOS 27 guest spins launchd.
const KEEP: &str = "com.apple.generativeexperiencesd";
/// Disabled with the profile: Journal's suggestions (0.67 of a guest CPU in the 0.6.0 log),
/// CoreDuet's knowledge store, the on-device model catalogue and My Photo Stream's daemon.
pub const TRIMMED_EXTRA: [&str; 4] =
    ["com.apple.momentsd", "com.apple.coreduetd", "com.apple.modelcatalogd", "com.apple.mediastream.mstreamd"];
/// Disabled besides, for [`Level::Fewest`]. Each costs something, and none has been run:
///
/// - sharingd: AirDrop, Handoff and the share sheet's nearby people.
/// - chronod: every Home Screen and Lock Screen widget (WidgetKit's daemon, which runs the
///   widget extensions and so their App Intents in BackgroundShortcutRunner).
/// - fairplayd.H2: FairPlay, which apps from the App Store need to start.
/// - mobile.obliteration: Erase All Content and Settings.
/// - spindump, tailspind, aggregated: the reports iOS writes when something hangs.
/// - activityawardsd: Fitness awards.
/// - apsd: push notifications, for every app. Its clients may retry instead of resting;
///   `guest top` shows it.
pub const FEWEST_EXTRA: [&str; 9] = [
    "com.apple.sharingd",
    "com.apple.chronod",
    "com.apple.fairplayd.H2",
    "com.apple.mobile.obliteration",
    "com.apple.spindump",
    "com.apple.tailspind",
    "com.apple.aggregated",
    "com.apple.activityawardsd",
    "com.apple.apsd",
];
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
    /// The level's labels as they were asked for: a build with another list applies again.
    pub extra: Vec<String>,
    /// The labels this app disabled, to turn back on when the level no longer has them. A file
    /// from before 0.6.6 has none, and every label of `extra` was disabled then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owned: Option<Vec<String>>,
}

impl Applied {
    pub fn load(vm: &Path) -> Applied {
        std::fs::read(vm.join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, vm: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(vm.join(FILE), json).map_err(|e| e.to_string())
    }

    pub fn owned(&self) -> &[String] {
        self.owned.as_deref().unwrap_or(&self.extra)
    }

    /// `services as they are`, `services trimmed and 4 more`: what this start runs with, as the
    /// record of the starts before it has it.
    pub fn words(&self) -> String {
        match self.level {
            Level::Unchanged => "services as they are".to_string(),
            level => format!("services {} and {} more", format!("{level:?}").to_lowercase(), self.owned().len()),
        }
    }
}

/// What the guest's launchd has: the labels it knows, and the ones with a `disable` override.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Known {
    pub labels: BTreeSet<String>,
    pub disabled: BTreeSet<String>,
}

impl Known {
    /// From `services.list` (`{services: [{label, ...}]}`) and `services.disabled`
    /// (`{disabled: {label: bool}}`); `None` where the list names no service, which is a reply
    /// this build cannot read.
    pub fn read(list: &Value, disabled: &Value) -> Option<Known> {
        let labels: BTreeSet<String> =
            list.get("services")?.as_array()?.iter().filter_map(|row| row.get("label")?.as_str().map(str::to_owned)).collect();
        if labels.is_empty() {
            return None;
        }
        let disabled = disabled
            .get("disabled")
            .and_then(Value::as_object)
            .map(|overrides| overrides.iter().filter(|(_, off)| **off == true).map(|(label, _)| label.clone()).collect())
            .unwrap_or_default();
        Some(Known { labels, disabled })
    }
}

/// A level's labels by what is to be done with each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sorted {
    /// Enabled now: disabled by this call, and this app's from then on.
    pub disable: Vec<&'static str>,
    /// This app's from an earlier start, and still disabled.
    pub kept: Vec<&'static str>,
    /// Disabled by somebody else: left alone, now and when the level goes down.
    pub already: Vec<&'static str>,
    /// Not a job of this iOS's launchd: not asked for.
    pub missing: Vec<&'static str>,
}

/// Without `known` (an agent whose list could not be read) every label is disabled unseen, as
/// before 0.6.6: `launchctl disable` accepts a label launchd does not have and it does nothing.
pub fn sort(extra: &[&'static str], applied: &Applied, known: Option<&Known>) -> Sorted {
    let mut sorted = Sorted::default();
    let Some(known) = known else {
        sorted.disable = extra.to_vec();
        return sorted;
    };
    for label in extra {
        let off = known.disabled.contains(*label);
        if off && applied.owned().iter().any(|o| o == label) {
            sorted.kept.push(label);
        } else if off {
            sorted.already.push(label);
        } else if known.labels.contains(*label) {
            sorted.disable.push(label);
        } else {
            sorted.missing.push(label);
        }
    }
    sorted
}

/// Whether the VM is not at `want` as this build has it.
pub fn differs(applied: &Applied, want: Level) -> bool {
    applied.level != want || !applied.extra.iter().map(String::as_str).eq(want.extra().iter().copied())
}

/// The calls that bring a VM from `applied` to `want`, in order: the profile, the labels to turn
/// back on, the labels to disable. Empty when nothing has changed.
pub fn plan(applied: &Applied, want: Level, sorted: &Sorted) -> Vec<(&'static str, Value)> {
    if !differs(applied, want) {
        return Vec::new();
    }
    let extra = want.extra();
    let mut calls = vec![(
        "services.profile.apply",
        if want == Level::Unchanged {
            json!({"profile": want.profile(), "force": true})
        } else {
            json!({"profile": want.profile(), "allow": [KEEP], "force": true})
        },
    )];
    for label in applied.owned().iter().filter(|l| !extra.contains(&l.as_str())) {
        calls.push(("services.enable", json!({"label": label})));
    }
    for label in &sorted.disable {
        calls.push(("services.disable", json!({"label": label, "force": true})));
    }
    calls
}

/// `3 more disabled now (a, b, c); 1 already off (d); 2 not in this iOS (e, f)`
pub fn sorted_words(sorted: &Sorted, checked: bool) -> String {
    let short = |labels: &[&str]| labels.iter().map(|l| l.trim_start_matches("com.apple.")).collect::<Vec<_>>().join(", ");
    let mut parts = Vec::new();
    if !sorted.disable.is_empty() {
        parts.push(format!(
            "{} more disabled now{} ({})",
            sorted.disable.len(),
            if checked { "" } else { ", unseen: launchd's list could not be read" },
            short(&sorted.disable)
        ));
    }
    if !sorted.kept.is_empty() {
        parts.push(format!("{} this app disabled before ({})", sorted.kept.len(), short(&sorted.kept)));
    }
    if !sorted.already.is_empty() {
        parts.push(format!("{} already off and left as they are ({})", sorted.already.len(), short(&sorted.already)));
    }
    if !sorted.missing.is_empty() {
        parts.push(format!("{} not in this iOS's launchd ({})", sorted.missing.len(), short(&sorted.missing)));
    }
    if parts.is_empty() {
        parts.push("no label beyond the profile".to_string());
    }
    parts.join("; ")
}

/// Applies `want` to the running guest if it differs from what was applied; one or two lines
/// through `say`. Never fails the boot: what could not be asked is said and tried at the next.
pub fn apply(client: &Client, vm: &Path, want: Level, say: &dyn Fn(&str)) {
    let applied = Applied::load(vm);
    if !differs(&applied, want) {
        if want != Level::Unchanged {
            say(&format!("services: {want:?}, as applied at an earlier start ({} labels beyond the profile are this app's)", applied.owned().len()));
        }
        return;
    }
    let extra = want.extra();
    let known = if extra.is_empty() {
        None
    } else {
        match (client.rpc("services.list", json!({}), RPC), client.rpc("services.disabled", json!({}), RPC)) {
            (Ok(list), Ok(disabled)) => Known::read(&list, &disabled),
            _ => None,
        }
    };
    let sorted = sort(&extra, &applied, known.as_ref());
    let calls = plan(&applied, want, &sorted);
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
    let record = Applied {
        level: want,
        extra: extra.iter().map(|l| (*l).to_string()).collect(),
        owned: Some(sorted.kept.iter().chain(&sorted.disable).map(|l| (*l).to_string()).collect()),
    };
    if let Err(why) = record.save(vm) {
        say(&format!("services: applied, but not recorded ({why})"));
    }
    say(&match want {
        Level::Unchanged => "services: every daemon this app had disabled is enabled again; it takes effect when iOS next starts".to_string(),
        _ => format!(
            "services: {want:?} applied: {}; it takes effect when iOS next starts: shut the VM down and start it again",
            sorted_words(&sorted, known.is_some())
        ),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn methods(calls: &[(&'static str, Value)]) -> Vec<String> {
        calls
            .iter()
            .map(|(m, p)| format!("{m} {}", p.get("label").or(p.get("profile")).and_then(Value::as_str).unwrap_or("").trim_start_matches("com.apple.")))
            .collect()
    }

    fn owned(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|l| (*l).to_string()).collect()
    }

    /// The plan for a guest whose launchd could not be asked: every label, unseen.
    fn blind(applied: &Applied, want: Level) -> Vec<(&'static str, Value)> {
        plan(applied, want, &sort(&want.extra(), applied, None))
    }

    fn at(level: Level) -> Applied {
        Applied { level, extra: owned(&level.extra()), owned: Some(owned(&level.extra())) }
    }

    #[test]
    fn a_vm_nobody_changed_is_left_alone() {
        assert!(blind(&Applied::default(), Level::Unchanged).is_empty());
        // And one already at its level is not asked again.
        assert!(blind(&at(Level::Trimmed), Level::Trimmed).is_empty());
        assert!(differs(&at(Level::Trimmed), Level::Fewest) && !differs(&at(Level::Fewest), Level::Fewest));
    }

    #[test]
    fn trimming_applies_the_profile_then_the_extra_labels() {
        let calls = blind(&Applied::default(), Level::Trimmed);
        assert_eq!(
            methods(&calls),
            [
                "services.profile.apply trimmed",
                "services.disable momentsd",
                "services.disable coreduetd",
                "services.disable modelcatalogd",
                "services.disable mediastream.mstreamd"
            ]
        );
        assert_eq!(calls[0].1, json!({"profile": "trimmed", "allow": ["com.apple.generativeexperiencesd"], "force": true}));
        assert_eq!(calls[1].1, json!({"label": "com.apple.momentsd", "force": true}));
        let fewest = blind(&Applied::default(), Level::Fewest);
        assert_eq!(fewest.len(), 1 + TRIMMED_EXTRA.len() + FEWEST_EXTRA.len());
        assert_eq!(methods(&fewest)[5..7], ["services.disable sharingd", "services.disable chronod"]);
        // Nothing vphoned's profile refuses to disable is asked for by label, and no label twice.
        let all = Level::Fewest.extra();
        for label in &all {
            assert!(label.starts_with("com.apple."));
            assert!(!["com.apple.locationd", "com.apple.siriactionsd", "com.apple.SpringBoard", "com.apple.backboardd", "com.apple.cloudd", "com.apple.sleepd"]
                .contains(label));
        }
        assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), all.len());
        // What is unverified, or costs a feature, is only in Fewest.
        for label in ["com.apple.apsd", "com.apple.fairplayd.H2", "com.apple.spindump", "com.apple.mobile.obliteration"] {
            assert!(FEWEST_EXTRA.contains(&label) && !Level::Trimmed.extra().contains(&label));
        }
    }

    #[test]
    fn the_labels_are_sorted_by_what_launchd_says_of_them() {
        let list = json!({"services": [
            {"label": "com.apple.momentsd", "pid": 71, "running": true},
            {"label": "com.apple.coreduetd"},
            {"label": "com.apple.SpringBoard"},
            {"label": "com.apple.mediastream.mstreamd", "disabled": true},
        ]});
        let disabled = json!({"disabled": {"com.apple.mediastream.mstreamd": true, "com.apple.coreduetd": false}});
        let known = Known::read(&list, &disabled).unwrap();
        let sorted = sort(&Level::Trimmed.extra(), &Applied::default(), Some(&known));
        assert_eq!(sorted.disable, ["com.apple.momentsd", "com.apple.coreduetd"]);
        assert_eq!(sorted.already, ["com.apple.mediastream.mstreamd"]);
        assert_eq!(sorted.missing, ["com.apple.modelcatalogd"]);
        // Only what launchd has and has on is asked for.
        assert_eq!(
            methods(&plan(&Applied::default(), Level::Trimmed, &sorted)),
            ["services.profile.apply trimmed", "services.disable momentsd", "services.disable coreduetd"]
        );
        assert_eq!(
            sorted_words(&sorted, true),
            "2 more disabled now (momentsd, coreduetd); 1 already off and left as they are (mediastream.mstreamd); \
             1 not in this iOS's launchd (modelcatalogd)"
        );

        // 0.6.5's record: momentsd is this app's, and a disabled job launchd did not load is
        // known by its override alone.
        let older = Applied { level: Level::Trimmed, extra: owned(&["com.apple.momentsd"]), owned: None };
        assert!(differs(&older, Level::Trimmed));
        let known = Known::read(
            &json!({"services": [{"label": "com.apple.coreduetd"}, {"label": "com.apple.modelcatalogd"}]}),
            &json!({"disabled": {"com.apple.momentsd": true}}),
        )
        .unwrap();
        let sorted = sort(&Level::Trimmed.extra(), &older, Some(&known));
        assert_eq!(sorted.kept, ["com.apple.momentsd"]);
        assert_eq!(sorted.disable, ["com.apple.coreduetd", "com.apple.modelcatalogd"]);
        assert_eq!(sorted.missing, ["com.apple.mediastream.mstreamd"]);

        // A reply with no services is not taken for a launchd with none.
        assert_eq!(Known::read(&json!({"services": []}), &json!({})), None);
        assert_eq!(Known::read(&json!({"rows": []}), &json!({})), None);
        let unseen = sort(&Level::Trimmed.extra(), &Applied::default(), None);
        assert_eq!(unseen.disable.len(), 4);
        assert!(sorted_words(&unseen, false).starts_with("4 more disabled now, unseen: launchd's list could not be read (momentsd, "));
    }

    #[test]
    fn a_lower_level_turns_back_on_exactly_what_this_app_disabled() {
        // Of Fewest's labels this app disabled three; the others were off already or missing.
        let fewest = Applied {
            level: Level::Fewest,
            extra: owned(&Level::Fewest.extra()),
            owned: Some(owned(&["com.apple.momentsd", "com.apple.sharingd", "com.apple.chronod"])),
        };
        let known = Known {
            labels: ["com.apple.momentsd", "com.apple.coreduetd"].iter().map(|l| (*l).to_string()).collect(),
            disabled: ["com.apple.momentsd", "com.apple.sharingd", "com.apple.chronod"].iter().map(|l| (*l).to_string()).collect(),
        };
        let sorted = sort(&Level::Trimmed.extra(), &fewest, Some(&known));
        assert_eq!(
            methods(&plan(&fewest, Level::Trimmed, &sorted)),
            ["services.profile.apply trimmed", "services.enable sharingd", "services.enable chronod", "services.disable coreduetd"]
        );
        let back = blind(&fewest, Level::Unchanged);
        assert_eq!(
            methods(&back),
            ["services.profile.apply none", "services.enable momentsd", "services.enable sharingd", "services.enable chronod"]
        );
        assert_eq!(back[0].1, json!({"profile": "none", "force": true}));
        // A record from before 0.6.6 owns every label it names.
        let older = Applied { level: Level::Fewest, extra: owned(&["com.apple.momentsd", "com.apple.sharingd", "com.apple.chronod"]), owned: None };
        assert_eq!(methods(&blind(&older, Level::Unchanged)), methods(&back));
    }

    #[test]
    fn the_record_survives_a_restart_and_a_missing_one_is_unchanged() {
        let dir = std::env::temp_dir().join(format!("vphone-services-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Applied::load(&dir), Applied::default());
        assert_eq!(Applied::default().words(), "services as they are");
        let record = Applied { level: Level::Trimmed, extra: owned(&Level::Trimmed.extra()), owned: Some(owned(&["com.apple.momentsd"])) };
        record.save(&dir).unwrap();
        assert_eq!(Applied::load(&dir), record);
        assert_eq!(record.words(), "services trimmed and 1 more");
        assert!(std::fs::read_to_string(dir.join(FILE)).unwrap().contains(r#""level": "trimmed""#));
        // 0.6.5's file, which has no list of what is this app's, is read, and is applied again
        // because this build's list is longer.
        std::fs::write(dir.join(FILE), r#"{"level": "trimmed", "extra": ["com.apple.momentsd"]}"#).unwrap();
        let older = Applied::load(&dir);
        assert_eq!((older.level, older.owned()), (Level::Trimmed, &owned(&["com.apple.momentsd"])[..]));
        assert!(!blind(&older, Level::Trimmed).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
