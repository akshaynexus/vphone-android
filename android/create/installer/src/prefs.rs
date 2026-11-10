//! iOS preferences that make the guest do less work on emulated CPUs, written through the
//! guest's agent: at the end of the first boot, and after each later unlock for whatever is
//! missing by then.
//!
//! Two groups, each a setting of the VM that can be turned off again:
//!
//! - **[`Group::Visuals`]**, "Lighter iOS visuals".
//!   - `CAEnableDeepFramebuffer`, `CAEnableFramebufferCompression` and
//!     `CAEnableIOSurfaceCompression` = false in the any-user global domain,
//!     `/private/var/preferences/.GlobalPreferences.plist`. That file is where vphone-cli 2.14.2
//!     puts the same three keys for an iOS 18 guest of the same paravirtual GPU, checked there
//!     across a reboot (`CryptexFilesystemPatcherGuestPayload.addCoreAnimationDefaults`: backboardd
//!     "consults neither com.apple.coreanimation nor the managed any-user file"). UniXNU seeds the
//!     keys in root's and mobile's `backboardd` and `.GlobalPreferences` domains
//!     (`scripts/make-data-seed.py`), AdrenoMetal sets them in each process that loads it. Not run
//!     on an iOS 27 guest of this app: whether backboardd reads them there, and what they save,
//!     is unmeasured.
//!   - `ReduceMotionEnabled` and `EnhancedBackgroundContrastEnabled` (Settings' Reduce Motion and
//!     Reduce Transparency) = true in `com.apple.Accessibility` for user mobile. The names are
//!     the ones libAccessibility has used since iOS 7; not checked on iOS 27.
//! - **[`Group::Watchdogs`]**, "Longer app launch limits".
//!   - `FBLaunchWatchdogScale` = [`LAUNCH_SCALE`] in `com.apple.springboard`: FrontBoard multiplies
//!     its launch and scene-create limits by it. Measured on the desktop test VM
//!     (docs/TROUBLESHOOTING.md: about 15 s unscaled, about 796 s with 40). iOSPlus10 seeds 3
//!     (`scripts/ios16/root-tweaks-extra.py`): a launch that never checks in holds the foreground
//!     for the whole limit, so the scale stays small.
//!   - `BKNoWatchdogs` = true in `com.apple.springboard`, as UniXNU seeds it
//!     (`scripts/seed-data-skip-setup.py`). Not checked on iOS 27.
//!   - UniXNU's `FBLaunchWatchdogExceptions` names only Setup Assistant, which this app skips.
//!
//! The mobile domains are written with `settings.set` (cfprefsd, as docs/TROUBLESHOOTING.md does
//! for the scale); the any-user file is read, changed and written back whole, as nothing of
//! vphoned's writes that domain. What is there is read from the plist files, so a key counts as
//! present only once cfprefsd has written it. SpringBoard and backboardd read these when they
//! start, so a key set now is in force from iOS's next start.
//!
//! `<vm>/prefs.json` lists the keys this app set, so turning a group off removes exactly those and
//! leaves a key the user set alone.

use base64::Engine;
use plist::{Dictionary, Value as Plist};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const FILE: &str = "prefs.json";
const GLOBAL_DIR: &str = "/private/var/preferences";
const GLOBAL_NAME: &str = ".GlobalPreferences.plist";
/// FrontBoard's launch limits times this: about a minute where iOS allows 15 s.
pub const LAUNCH_SCALE: f64 = 4.0;

/// A vphoned RPC: the method, its parameters; the result, or vphoned's message.
pub type Rpc<'a> = &'a dyn Fn(&str, Value) -> Result<Value, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Visuals,
    Watchdogs,
}

impl Group {
    pub const ALL: [Group; 2] = [Group::Visuals, Group::Watchdogs];

    fn title(self) -> &'static str {
        match self {
            Group::Visuals => "lighter visuals",
            Group::Watchdogs => "longer launch limits",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Store {
    /// A domain of user mobile, through cfprefsd.
    Mobile(&'static str),
    /// The any-user global domain's file.
    Global,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Want {
    Bool(bool),
    Number(f64),
}

impl Want {
    fn matches(self, have: &Plist) -> bool {
        match (self, have) {
            (Want::Bool(b), Plist::Boolean(h)) => b == *h,
            (Want::Bool(b), Plist::Integer(i)) => i.as_signed() == Some(i64::from(b)),
            (Want::Number(n), Plist::Integer(i)) => i.as_signed().is_some_and(|i| i as f64 == n),
            (Want::Number(n), Plist::Real(r)) => *r == n,
            _ => false,
        }
    }

    fn plist(self) -> Plist {
        match self {
            Want::Bool(b) => Plist::Boolean(b),
            Want::Number(n) => Plist::Real(n),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pref {
    pub group: Group,
    store: Store,
    pub key: &'static str,
    value: Want,
}

pub const PREFS: [Pref; 7] = [
    Pref { group: Group::Visuals, store: Store::Global, key: "CAEnableDeepFramebuffer", value: Want::Bool(false) },
    Pref { group: Group::Visuals, store: Store::Global, key: "CAEnableFramebufferCompression", value: Want::Bool(false) },
    Pref { group: Group::Visuals, store: Store::Global, key: "CAEnableIOSurfaceCompression", value: Want::Bool(false) },
    Pref { group: Group::Visuals, store: Store::Mobile("com.apple.Accessibility"), key: "ReduceMotionEnabled", value: Want::Bool(true) },
    Pref {
        group: Group::Visuals,
        store: Store::Mobile("com.apple.Accessibility"),
        key: "EnhancedBackgroundContrastEnabled",
        value: Want::Bool(true),
    },
    Pref { group: Group::Watchdogs, store: Store::Mobile("com.apple.springboard"), key: "FBLaunchWatchdogScale", value: Want::Number(LAUNCH_SCALE) },
    Pref { group: Group::Watchdogs, store: Store::Mobile("com.apple.springboard"), key: "BKNoWatchdogs", value: Want::Bool(true) },
];

impl Pref {
    /// The plist the key is in.
    pub fn path(&self) -> String {
        match self.store {
            Store::Mobile(domain) => format!("/var/mobile/Library/Preferences/{domain}.plist"),
            Store::Global => format!("{GLOBAL_DIR}/{GLOBAL_NAME}"),
        }
    }

    /// The key as the record names it.
    fn id(&self) -> String {
        format!("{}:{}", self.path(), self.key)
    }

    /// `settings.set`'s parameters for a key of a mobile domain.
    fn set_params(&self, domain: &str) -> Value {
        match self.value {
            Want::Bool(b) => json!({"domain": domain, "key": self.key, "value": b, "type": "bool"}),
            Want::Number(n) => json!({"domain": domain, "key": self.key, "value": n, "type": "float"}),
        }
    }
}

/// The keys this app set in the guest, in `<vm>/prefs.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub set: BTreeSet<String>,
}

impl Record {
    pub fn load(vm: &Path) -> Record {
        std::fs::read(vm.join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, vm: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(vm.join(FILE), json).map_err(|e| e.to_string())
    }
}

/// What one pass did to a group's keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Done {
    pub set_now: Vec<&'static str>,
    pub already: Vec<&'static str>,
    pub removed: Vec<&'static str>,
    pub failed: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// By [`Group::ALL`]'s order: whether the group is wanted, and what was done.
    pub groups: [(bool, Done); 2],
}

impl Outcome {
    /// Whether every key of the group was already in the guest when it was looked at: the group
    /// is in force in this boot.
    pub fn in_force(&self, group: Group) -> bool {
        let (wanted, done) = &self.groups[group as usize];
        *wanted && done.set_now.is_empty() && done.failed.is_empty() && !done.already.is_empty()
    }

    fn changed(&self) -> bool {
        self.groups.iter().any(|(_, d)| !d.set_now.is_empty() || !d.removed.is_empty())
    }

    /// `guest prefs: lighter visuals on: 3 set now (...), 2 already there; ...`
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        for group in Group::ALL {
            let (wanted, done) = &self.groups[group as usize];
            let mut said = Vec::new();
            if !done.set_now.is_empty() {
                said.push(format!("{} set now ({})", done.set_now.len(), done.set_now.join(", ")));
            }
            if !done.already.is_empty() {
                said.push(format!("{} already there", done.already.len()));
            }
            if !done.removed.is_empty() {
                said.push(format!("{} removed ({})", done.removed.len(), done.removed.join(", ")));
            }
            if !done.failed.is_empty() {
                said.push(format!("{} not done ({})", done.failed.len(), done.failed.join("; ")));
            }
            if said.is_empty() {
                said.push("nothing of this app's in the guest".to_owned());
            }
            parts.push(format!("{} {}: {}", group.title(), if *wanted { "on" } else { "off" }, said.join(", ")));
        }
        let mut line = format!("guest prefs: {}", parts.join("; "));
        if self.changed() {
            line += "; what changed takes effect when iOS next starts";
        }
        line
    }
}

fn read(rpc: Rpc, path: &str) -> Result<Dictionary, String> {
    let reply = rpc("files.read", json!({"path": path, "binary": true}))?;
    let content = reply.get("content").and_then(Value::as_str).ok_or("files.read gave no content")?;
    let bytes = B64.decode(content).map_err(|e| e.to_string())?;
    Plist::from_reader(std::io::Cursor::new(bytes))
        .ok()
        .and_then(Plist::into_dictionary)
        .ok_or_else(|| format!("{path} is not a property list of keys"))
}

/// The any-user global domain: its keys, empty where the guest has no such file. An error where
/// the file is there and cannot be read, so that it is never written over.
fn read_global(rpc: Rpc) -> Result<Dictionary, String> {
    let path = format!("{GLOBAL_DIR}/{GLOBAL_NAME}");
    let why = match read(rpc, &path) {
        Ok(keys) => return Ok(keys),
        Err(why) => why,
    };
    let list = rpc("files.list", json!({"path": GLOBAL_DIR}))?;
    let entries = list.get("entries").and_then(Value::as_array).ok_or("files.list gave no entries")?;
    if entries.iter().any(|e| e["name"] == GLOBAL_NAME) {
        return Err(why);
    }
    Ok(Dictionary::new())
}

/// Brings the guest's keys to `want` (by [`Group::ALL`]'s order) and `record` with them. Nothing
/// here fails a boot: what could not be done is in the outcome, and is tried at the next start.
pub fn apply(rpc: Rpc, record: &mut Record, want: [bool; 2]) -> Outcome {
    let mut outcome = Outcome::default();
    for group in Group::ALL {
        outcome.groups[group as usize].0 = want[group as usize];
    }
    let done = |o: &mut Outcome, p: &Pref, f: &dyn Fn(&mut Done)| f(&mut o.groups[p.group as usize].1);

    // The mobile domains, a key at a time through cfprefsd.
    let mut files: Vec<(String, Option<Dictionary>)> = Vec::new();
    for pref in PREFS.iter() {
        let Store::Mobile(domain) = pref.store else { continue };
        let wanted = want[pref.group as usize];
        let id = pref.id();
        if !wanted {
            if record.set.contains(&id) {
                match rpc("settings.delete", json!({"domain": domain, "key": pref.key})) {
                    Ok(_) => {
                        record.set.remove(&id);
                        done(&mut outcome, pref, &|d| d.removed.push(pref.key));
                    }
                    Err(why) => done(&mut outcome, pref, &|d| d.failed.push(format!("{}: {why}", pref.key))),
                }
            }
            continue;
        }
        let path = pref.path();
        if !files.iter().any(|(p, _)| *p == path) {
            // A file that is not there or not readable has none of the keys: they are set.
            files.push((path.clone(), read(rpc, &path).ok()));
        }
        let have = files.iter().find(|(p, _)| *p == path).and_then(|(_, keys)| keys.as_ref());
        if have.and_then(|keys| keys.get(pref.key)).is_some_and(|v| pref.value.matches(v)) {
            done(&mut outcome, pref, &|d| d.already.push(pref.key));
            continue;
        }
        match rpc("settings.set", pref.set_params(domain)) {
            Ok(_) => {
                record.set.insert(id);
                done(&mut outcome, pref, &|d| d.set_now.push(pref.key));
            }
            Err(why) => done(&mut outcome, pref, &|d| d.failed.push(format!("{}: {why}", pref.key))),
        }
    }

    // The any-user file: read once, written once if a key of it changes.
    let global: Vec<&Pref> = PREFS.iter().filter(|p| p.store == Store::Global).collect();
    let touches = global.iter().any(|p| want[p.group as usize] || record.set.contains(&p.id()));
    if !touches {
        return outcome;
    }
    let mut keys = match read_global(rpc) {
        Ok(keys) => keys,
        Err(why) => {
            for pref in global.iter().filter(|p| want[p.group as usize] || record.set.contains(&p.id())) {
                done(&mut outcome, pref, &|d| d.failed.push(format!("{}: {why}", pref.key)));
            }
            return outcome;
        }
    };
    let (mut set, mut removed) = (Vec::new(), Vec::new());
    for pref in &global {
        if want[pref.group as usize] {
            if keys.get(pref.key).is_some_and(|v| pref.value.matches(v)) {
                done(&mut outcome, pref, &|d| d.already.push(pref.key));
            } else {
                keys.insert(pref.key.to_owned(), pref.value.plist());
                set.push(*pref);
            }
        } else if record.set.contains(&pref.id()) {
            keys.remove(pref.key);
            removed.push(*pref);
        }
    }
    if set.is_empty() && removed.is_empty() {
        return outcome;
    }
    let mut bytes = Vec::new();
    let written = plist::to_writer_binary(&mut bytes, &Plist::Dictionary(keys)).map_err(|e| e.to_string()).and_then(|()| {
        let path = format!("{GLOBAL_DIR}/{GLOBAL_NAME}");
        rpc("files.write", json!({"path": path, "content": B64.encode(&bytes), "encoding": "base64"}))
    });
    match written {
        Ok(_) => {
            for pref in set {
                record.set.insert(pref.id());
                done(&mut outcome, pref, &|d| d.set_now.push(pref.key));
            }
            for pref in removed {
                record.set.remove(&pref.id());
                done(&mut outcome, pref, &|d| d.removed.push(pref.key));
            }
        }
        Err(why) => {
            for pref in set.into_iter().chain(removed) {
                done(&mut outcome, pref, &|d| d.failed.push(format!("{}: {why}", pref.key)));
            }
        }
    }
    outcome
}

/// [`apply`] with the VM's record read before and saved after; the line for the log.
pub fn apply_to_vm(rpc: Rpc, vm: &Path, want: [bool; 2]) -> (Outcome, String) {
    let mut record = Record::load(vm);
    let before = record.clone();
    let outcome = apply(rpc, &mut record, want);
    let mut line = outcome.line();
    if record != before {
        if let Err(why) = record.save(vm) {
            line += &format!("; not recorded ({why})");
        }
    }
    (outcome, line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A guest's files and cfprefsd: plists by path, and the calls made.
    #[derive(Default)]
    struct Guest {
        files: RefCell<BTreeMap<String, Dictionary>>,
        calls: RefCell<Vec<String>>,
        /// A file that is there and is no plist.
        junk: Option<&'static str>,
        refuse: Option<&'static str>,
    }

    impl Guest {
        fn rpc(&self, method: &str, p: Value) -> Result<Value, String> {
            self.calls.borrow_mut().push(format!("{method} {}", p["key"].as_str().or(p["path"].as_str()).unwrap_or("")));
            if self.refuse == Some(method) {
                return Err(format!("{method}: refused"));
            }
            let mobile = |domain: &Value| format!("/var/mobile/Library/Preferences/{}.plist", domain.as_str().unwrap());
            let mut files = self.files.borrow_mut();
            match method {
                "files.read" => {
                    let path = p["path"].as_str().unwrap();
                    assert_eq!(p["binary"], true);
                    if self.junk == Some(path) {
                        return Ok(json!({"content": B64.encode(b"not a plist")}));
                    }
                    let keys = files.get(path).ok_or("files.read: No such file or directory")?;
                    let mut bytes = Vec::new();
                    plist::to_writer_binary(&mut bytes, &Plist::Dictionary(keys.clone())).unwrap();
                    Ok(json!({"content": B64.encode(bytes)}))
                }
                "files.list" => {
                    let dir = p["path"].as_str().unwrap();
                    let mut names: Vec<Value> = files
                        .keys()
                        .filter_map(|path| path.strip_prefix(dir)?.strip_prefix('/'))
                        .map(|name| json!({"name": name, "type": "file"}))
                        .collect();
                    if let Some(name) = self.junk.and_then(|j| j.strip_prefix(dir)?.strip_prefix('/')) {
                        names.push(json!({"name": name, "type": "file"}));
                    }
                    Ok(json!({"entries": names}))
                }
                "files.write" => {
                    assert_eq!(p["encoding"], "base64");
                    let bytes = B64.decode(p["content"].as_str().unwrap()).unwrap();
                    assert!(bytes.starts_with(b"bplist00"));
                    let keys = Plist::from_reader(std::io::Cursor::new(bytes)).unwrap().into_dictionary().unwrap();
                    files.insert(p["path"].as_str().unwrap().to_owned(), keys);
                    Ok(json!({}))
                }
                "settings.set" => {
                    let value = match (p["type"].as_str().unwrap(), &p["value"]) {
                        ("bool", Value::Bool(b)) => Plist::Boolean(*b),
                        ("float", Value::Number(n)) => Plist::Real(n.as_f64().unwrap()),
                        other => panic!("settings.set with {other:?}"),
                    };
                    files.entry(mobile(&p["domain"])).or_default().insert(p["key"].as_str().unwrap().to_owned(), value);
                    Ok(json!({}))
                }
                "settings.delete" => {
                    files.entry(mobile(&p["domain"])).or_default().remove(p["key"].as_str().unwrap());
                    Ok(json!({}))
                }
                other => panic!("unexpected {other}"),
            }
        }

        fn has(&self, path: &str, key: &str) -> Option<Plist> {
            self.files.borrow().get(path)?.get(key).cloned()
        }
    }

    const GLOBAL: &str = "/private/var/preferences/.GlobalPreferences.plist";
    const AX: &str = "/var/mobile/Library/Preferences/com.apple.Accessibility.plist";
    const SB: &str = "/var/mobile/Library/Preferences/com.apple.springboard.plist";

    #[test]
    fn a_new_guest_gets_every_key_and_the_next_start_finds_them() {
        let guest = Guest::default();
        // A key of somebody else's in each file stays.
        guest.files.borrow_mut().entry(GLOBAL.into()).or_default().insert("AppleLanguages".into(), Plist::String("en".into()));
        guest.files.borrow_mut().entry(SB.into()).or_default().insert("SBIconVisibility".into(), Plist::Boolean(true));
        let mut record = Record::default();
        let first = apply(&|m, p| guest.rpc(m, p), &mut record, [true, true]);
        assert_eq!(first.groups[0].1.set_now.len(), 5);
        assert_eq!(first.groups[1].1.set_now, ["FBLaunchWatchdogScale", "BKNoWatchdogs"]);
        assert!(!first.in_force(Group::Visuals) && !first.in_force(Group::Watchdogs));
        assert_eq!(record.set.len(), 7);
        for key in ["CAEnableDeepFramebuffer", "CAEnableFramebufferCompression", "CAEnableIOSurfaceCompression"] {
            assert_eq!(guest.has(GLOBAL, key), Some(Plist::Boolean(false)));
        }
        assert_eq!(guest.has(GLOBAL, "AppleLanguages"), Some(Plist::String("en".into())));
        assert_eq!(guest.has(AX, "ReduceMotionEnabled"), Some(Plist::Boolean(true)));
        assert_eq!(guest.has(AX, "EnhancedBackgroundContrastEnabled"), Some(Plist::Boolean(true)));
        assert_eq!(guest.has(SB, "FBLaunchWatchdogScale"), Some(Plist::Real(4.0)));
        assert_eq!(guest.has(SB, "BKNoWatchdogs"), Some(Plist::Boolean(true)));
        assert_eq!(
            first.line(),
            "guest prefs: lighter visuals on: 5 set now (ReduceMotionEnabled, EnhancedBackgroundContrastEnabled, CAEnableDeepFramebuffer, \
             CAEnableFramebufferCompression, CAEnableIOSurfaceCompression); longer launch limits on: 2 set now (FBLaunchWatchdogScale, \
             BKNoWatchdogs); what changed takes effect when iOS next starts"
        );

        // The next start reads three files and writes nothing.
        guest.calls.borrow_mut().clear();
        let second = apply(&|m, p| guest.rpc(m, p), &mut record, [true, true]);
        assert_eq!(*guest.calls.borrow(), [format!("files.read {AX}"), format!("files.read {SB}"), format!("files.read {GLOBAL}")]);
        assert!(second.in_force(Group::Visuals) && second.in_force(Group::Watchdogs));
        assert_eq!(second.line(), "guest prefs: lighter visuals on: 5 already there; longer launch limits on: 2 already there");

        // A key that went missing is set again, alone.
        guest.files.borrow_mut().get_mut(AX).unwrap().remove("ReduceMotionEnabled");
        let third = apply(&|m, p| guest.rpc(m, p), &mut record, [true, true]);
        assert_eq!((third.groups[0].1.set_now.as_slice(), third.groups[0].1.already.len()), (&["ReduceMotionEnabled"][..], 4));
        assert!(!third.in_force(Group::Visuals) && third.in_force(Group::Watchdogs));
    }

    #[test]
    fn turning_a_group_off_removes_what_this_app_set_and_nothing_else() {
        let guest = Guest::default();
        // The user already had Reduce Motion on, and a scale of their own.
        guest.files.borrow_mut().entry(AX.into()).or_default().insert("ReduceMotionEnabled".into(), Plist::Integer(1.into()));
        guest.files.borrow_mut().entry(SB.into()).or_default().insert("FBLaunchWatchdogScale".into(), Plist::Integer(4.into()));
        let mut record = Record::default();
        let on = apply(&|m, p| guest.rpc(m, p), &mut record, [true, true]);
        assert_eq!(on.groups[0].1.already, ["ReduceMotionEnabled"]);
        assert_eq!(on.groups[1].1.already, ["FBLaunchWatchdogScale"]);
        assert_eq!(record.set.len(), 5);

        let off = apply(&|m, p| guest.rpc(m, p), &mut record, [false, true]);
        assert_eq!(off.groups[0].1.removed.len(), 4);
        assert!(record.set.iter().all(|id| id.starts_with(SB)), "{record:?}");
        assert_eq!(guest.has(AX, "ReduceMotionEnabled"), Some(Plist::Integer(1.into())));
        assert_eq!(guest.has(AX, "EnhancedBackgroundContrastEnabled"), None);
        assert_eq!(guest.has(GLOBAL, "CAEnableDeepFramebuffer"), None);
        assert_eq!(guest.has(SB, "BKNoWatchdogs"), Some(Plist::Boolean(true)));
        assert!(off.line().starts_with("guest prefs: lighter visuals off: 4 removed ("), "{}", off.line());

        // Off with nothing recorded asks the guest nothing.
        guest.calls.borrow_mut().clear();
        let mut none = Record::default();
        let idle = apply(&|m, p| guest.rpc(m, p), &mut none, [false, false]);
        assert!(guest.calls.borrow().is_empty());
        assert_eq!(
            idle.line(),
            "guest prefs: lighter visuals off: nothing of this app's in the guest; longer launch limits off: nothing of this app's in the guest"
        );
    }

    #[test]
    fn a_global_file_that_cannot_be_read_is_not_written_over() {
        let guest = Guest { junk: Some(GLOBAL), ..Guest::default() };
        let mut record = Record::default();
        let outcome = apply(&|m, p| guest.rpc(m, p), &mut record, [true, false]);
        assert_eq!(outcome.groups[0].1.failed.len(), 3);
        assert_eq!(outcome.groups[0].1.set_now, ["ReduceMotionEnabled", "EnhancedBackgroundContrastEnabled"]);
        assert!(!guest.calls.borrow().iter().any(|c| c.starts_with("files.write")));
        assert_eq!(record.set.len(), 2);

        // A write the guest refuses records nothing, so the next start tries again.
        let guest = Guest { refuse: Some("files.write"), ..Guest::default() };
        let mut record = Record::default();
        let outcome = apply(&|m, p| guest.rpc(m, p), &mut record, [true, false]);
        assert_eq!(outcome.groups[0].1.failed.len(), 3);
        assert!(record.set.iter().all(|id| id.starts_with(AX)));
        assert!(outcome.line().contains("3 not done (CAEnableDeepFramebuffer: files.write: refused"), "{}", outcome.line());
    }

    #[test]
    fn the_record_is_kept_with_the_vm() {
        let dir = tempfile::tempdir().unwrap();
        let guest = Guest::default();
        let (outcome, line) = apply_to_vm(&|m, p| guest.rpc(m, p), dir.path(), [true, true]);
        assert!(line.starts_with("guest prefs: lighter visuals on: 5 set now"));
        assert!(!outcome.in_force(Group::Visuals));
        assert_eq!(Record::load(dir.path()).set.len(), 7);
        let (_, line) = apply_to_vm(&|m, p| guest.rpc(m, p), dir.path(), [false, false]);
        assert!(line.contains("lighter visuals off: 5 removed"), "{line}");
        assert!(Record::load(dir.path()).set.is_empty());
        assert_eq!(PREFS.iter().filter(|p| p.group == Group::Visuals).count(), 5);
    }

    #[test]
    fn the_calls_are_the_ones_vphoned_documents() {
        let scale = PREFS.iter().find(|p| p.key == "FBLaunchWatchdogScale").unwrap();
        // docs/TROUBLESHOOTING.md's call, with this app's scale.
        assert_eq!(
            scale.set_params("com.apple.springboard"),
            json!({"domain": "com.apple.springboard", "key": "FBLaunchWatchdogScale", "value": 4.0, "type": "float"})
        );
        let motion = PREFS.iter().find(|p| p.key == "ReduceMotionEnabled").unwrap();
        assert_eq!(
            motion.set_params("com.apple.Accessibility"),
            json!({"domain": "com.apple.Accessibility", "key": "ReduceMotionEnabled", "value": true, "type": "bool"})
        );
        assert_eq!(motion.path(), AX);
        assert_eq!(PREFS[0].path(), GLOBAL);
    }
}
