//! The CFW install: a port of vphone-cli's `cfw install` for a VM booted into
//! the SSH ramdisk (`VPhoneCustomFirmwareInstallerLinux.swift` and the guest
//! half of `VPhoneCustomFirmwareInstaller.swift`), for iPhone and iPad guests.
//!
//! The volumes the Swift code reached over sshfs are reached here over SFTP and
//! the guest's tar. Patch verbs stay vphone-cli's: they run as
//! `libvphone_cli.so cfw <verb>` on staged local copies. The dyld cache, too large to
//! stage twice, is taken out of the cryptex image before upload, patched locally
//! and uploaded once.

use crate::bail;
use crate::error::{Error, Result};
use crate::guestfs::{Attrs, GuestVolume};
use crate::host::Host;
use crate::plist_py;
use crate::progress::{check_cancel, StepRecord};
use crate::resources::Release;
use crate::sign::{self, SignOptions};
use crate::ssh::GuestSsh;
use crate::tree::{self, ApfsTree, LocalTree, Meta};
use plist::{Date, Dictionary, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use vphone_apfs::container::VolumeSelector;

const OWNER: (u32, u32) = (0, 0);
const OS: &str = "System/Cryptexes/OS";
const APP: &str = "System/Cryptexes/App";
const DSC_IN_SYSTEM: &str = "System/Cryptexes/OS/System/Library/Caches/com.apple.dyld";
const DSC_IN_OS: &str = "System/Library/Caches/com.apple.dyld";
const DSC_UNDO_LOG: &str = ".vphone-dsc-undo.json";
const LAUNCHD: &str = "System/Library/xpc/launchd.plist";
const GPU: &str = "System/Library/Extensions/AppleParavirtGPUMetalIOGPUFamily.bundle";

const KERNEL_REGION: u64 = 0x1_8000_0000;
const KERNEL_REGION_WIDENED: u64 = 0x1_C000_0000;

mod id {
    pub const SETTINGS_ROOT_ROWS: &str = "system-preferences-exp-root_rows";
    pub const SETTINGS_SW_UPDATE: &str = "dyld-exp-settings_software_update_row";
    pub const PERSONA: &str = "system-installcoordination_proxy-cfw-persona_lookup";
    pub const PERSONA_MARKER: &str = "usr/lib/vphone-installcoordination-persona-lookup";
    pub const METAL_CACHE: &str = "system-systemhook-cfw-metal_cache_reuse";
    pub const METAL_CACHE_MARKER: &str = "usr/lib/vphone-metal-cache-reuse";
    pub const GYRO: &str = "system-backboardd-cfw-gyroscope";
    pub const ACCEL: &str = "system-backboardd-cfw-accelerometer";
    pub const HINGE: &str = "system-backboardd-cfw-hinge";
    pub const FOLD_GUARD: &str = "system-springboard-cfw-fold_guard";
    pub const FOLD_DISPLAY: &str = "system-springboard-cfw-fold_display";
    pub const ATTITUDE: &str = "system-apps-cfw-attitude";
    pub const FOLLOWUP: &str = "system-preferences-cfw-followup_groups";
    pub const SIGNIN: &str = "system-apps-cfw-signin_sheets";
    pub const X1_DYLD: &str = "system-dyld-boot-arm64e_x1_bootstrap";
    pub const X1_CACHE: &str = "dyld-boot-arm64e_x1_bootstrap";
    pub const X1_CACHE_REST: &str = "dyld-cfw-arm64e_x1_cache";
    pub const X1_LAUNCHD: &str = "system-launchd-boot-arm64e_x1_bootstrap";
    pub const X1_FSCK: &str = "system-fsck-boot-arm64e_x1_bootstrap";
    pub const X1_MOUNT: &str = "system-mount-boot-arm64e_x1_bootstrap";
    pub const X1_CIBARCODE: &str = "system-cibarcode-cfw-arm64e_x1";
    pub const GPU_INDEX: &str = "system-paravirt_gpu-cfw-heap_index_offset";
    pub const LOCATIOND: &str = "system-locationd-cfw-disable_auto_cohort";
    pub const BLUETOOTH: &str = "system-bluetoothd-cfw-btvc_le_connection";
    pub const JETSAM: &str = "system-jetsamproperties-cfw-disable_allowed";
    pub const VIRTIO_SOUND: &str = "system-virtiosound-cfw-hal_plugin";
    pub const VA_ROUTE: &str = "system-virtualaudio-cfw-speaker_route_throws";
    pub const VA_MUTE: &str = "system-virtualaudio-cfw-mute_set_throw";
    pub const VA_SP: &str = "system-virtualaudio-cfw-speaker_protection_gate";
    pub const VA_VOLUME: &str = "system-virtualaudio-cfw-volume_mode_precondition";
    pub const VA_DEVICE: &str = "system-virtualaudio-cfw-device_state_null_gate";
    pub const VA_GRAPH: &str = "system-virtualaudio-cfw-speaker_graph_chains";
    pub const VA_RAW: &str = "system-virtualaudio-cfw-speaker_raw_chains";
    pub const VA_MIC: &str = "system-virtualaudio-cfw-microphone_graph_chains";
    pub const DT_BOARD_AUDIO: &str = "preboot-cfw-devicetree_board_audio";
    pub const DT_IPHONE: &str = "preboot-cfw-devicetree_iphone_product";
    pub const DT_FOLD_SUB: &str = "preboot-cfw-devicetree_foldable_vm_subtype";
    pub const DT_FOLD_ID: &str = "preboot-cfw-devicetree_foldable_product_id";
    pub const DT_FOLD_DISP: &str = "preboot-cfw-devicetree_foldable_displays";
    pub const DT_HAPTICS: &str = "preboot-cfw-devicetree_haptics";
    pub const DT_DISPLAY_NODES: &str = "preboot-cfw-devicetree_display_nodes";
    pub const DT_MIC: &str = "preboot-cfw-devicetree_microphone_array";
    pub const DT_IDENTITY: &str = "preboot-exp-devicetree_identity";
    pub const HOMEPOD_AUDIO: &str = "preboot-homepod-exp-audio_product";
    pub const TV_TPRO: &str = "system-dyld-exp-tv_tpro";
    pub const BUILD: &str = "system-systemversion-cfw-build_version";
    pub const MIS_TRUST: &str = "dyld-exp-mis_trust_auth";
}

/// The libraries `cfw install` puts in /usr/lib (VPhoneGuestEnvironment.libraries).
const ENV_LIBRARIES: [&str; 17] = [
    "launchdhook-vphone.dylib",
    "SystemHook-vphone.dylib",
    "libvcamcaptured.dylib",
    "libcamfix.dylib",
    "libmisfix.dylib",
    "libhapticsfix.dylib",
    "libbatteryhealthfix.dylib",
    "libdevicehubfix.dylib",
    "libdevicename.dylib",
    "libprefsfix.dylib",
    "libsigninfix.dylib",
    "libvphonefolddisplay.dylib",
    "libvphonefoldguard.dylib",
    "libvphonehinge.dylib",
    "libvphonegyro.dylib",
    "libvphoneaccelerometer.dylib",
    "libvphoneattitude.dylib",
];
/// Installed or removed by their own selections (VPhoneGuestEnvironment.selectedLibraries).
const SELECTED_LIBRARIES: [&str; 8] = [
    "libprefsfix.dylib",
    "libsigninfix.dylib",
    "libvphonegyro.dylib",
    "libvphoneaccelerometer.dylib",
    "libvphoneattitude.dylib",
    "libvphonehinge.dylib",
    "libvphonefoldguard.dylib",
    "libvphonefolddisplay.dylib",
];

const VPHONED_ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.managedconfiguration.profiled-access</key><true/>
<key>com.apple.managedconfiguration.profiled.set</key><true/>
</dict></plist>"#;

/// Guest devices the installer handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    IPhone,
    IPad,
}

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub family: Family,
    pub product: &'static str,
    pub class: &'static str,
}

impl Device {
    pub fn presents_board(&self) -> bool {
        self.product != "iPhone11,6"
    }
    pub fn foldable(&self) -> bool {
        self.class.eq_ignore_ascii_case("V68AP")
    }
    pub fn board_tree_name(&self) -> String {
        format!("DeviceTree.{}.im4p", self.class)
    }
}

const IPHONES: [(&str, &str); 14] = [
    ("iPhone17,3", "d47ap"),
    ("iPhone17,1", "d93ap"),
    ("iPhone17,2", "d94ap"),
    ("iPhone17,4", "d48ap"),
    ("iPhone17,5", "v59ap"),
    ("iPhone18,1", "v53ap"),
    ("iPhone18,2", "v54ap"),
    ("iPhone18,3", "v57ap"),
    ("iPhone18,4", "d23ap"),
    ("iPhone18,5", "v159ap"),
    ("iPhone19,2", "v63ap"),
    ("iPhone19,4", "v68ap"),
    ("iPhone11,6", "d331pap"),
    ("iPhone11,4", "d331pap"),
];
const IPADS: [(&str, &str); 15] = [
    ("iPad16,1", "j410ap"),
    ("iPad16,2", "j410ap"),
    ("iPad15,7", "j481ap"),
    ("iPad15,3", "j607ap"),
    ("iPad15,4", "j607ap"),
    ("iPad15,5", "j637ap"),
    ("iPad15,6", "j637ap"),
    ("iPad16,3", "j717ap"),
    ("iPad16,4", "j717ap"),
    ("iPad16,5", "j720ap"),
    ("iPad16,6", "j720ap"),
    ("iPad17,1", "j817ap"),
    ("iPad17,2", "j817ap"),
    ("iPad17,3", "j820ap"),
    ("iPad17,4", "j820ap"),
];

/// The guest device a product type names, or `Unsupported` for a guest this
/// port does not cover (Apple TV, HomePod and Watch need their own userland swaps).
pub fn device_named(product: &str) -> Result<Device> {
    let canonical = if product == "iPhone11,4" { "iPhone11,6" } else { product };
    if let Some(&(p, class)) = IPHONES.iter().find(|(p, _)| *p == canonical) {
        return Ok(Device { family: Family::IPhone, product: p, class });
    }
    if let Some(&(p, class)) = IPADS.iter().find(|(p, _)| *p == product) {
        return Ok(Device { family: Family::IPad, product: p, class });
    }
    Err(Error::Unsupported(
        format!("guest {product}"),
        "the on-device CFW install covers iPhone and iPad guests; Apple TV, HomePod and Watch guests need OTA userland steps not ported".into(),
    ))
}

/// `PatchPlan.plist`, as `fw patch` recorded it.
#[derive(Clone, Debug)]
pub struct Plan {
    pub preset: String,
    pub enabled: BTreeSet<String>,
    pub parameters: BTreeMap<String, String>,
}

impl Plan {
    /// `None` when the VM has no plan or it cannot be read (the installer then
    /// applies every legacy patch, as vphone-cli does).
    pub fn read(vm: &Path) -> Option<Plan> {
        let value = Value::from_file(vm.join("PatchPlan.plist")).ok()?;
        let d = value.as_dictionary()?;
        let enabled = d
            .get("EnabledPatches")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_string().map(str::to_owned))
            .collect();
        let parameters = d
            .get("Parameters")
            .and_then(Value::as_dictionary)
            .map(|p| {
                p.iter()
                    .filter_map(|(k, v)| v.as_string().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Some(Plan {
            preset: d.get("Preset")?.as_string()?.to_owned(),
            enabled,
            parameters,
        })
    }
}

fn receipt_part(vm: &Path, part: &str) -> Option<BTreeSet<String>> {
    let value = Value::from_file(vm.join("PatchReceipt.plist")).ok()?;
    let patches = value
        .as_dictionary()?
        .get("Parts")?
        .as_dictionary()?
        .get(part)?
        .as_dictionary()?
        .get("Patches")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_string().map(str::to_owned))
        .collect();
    Some(patches)
}

/// Replace the receipt's `Guest` part, keeping every other part. A failure is
/// logged, not fatal: the guest already carries the bytes.
pub fn record_guest_receipt(vm: &Path, live: &BTreeSet<String>, not_applicable: &BTreeSet<String>) -> Result<()> {
    let path = vm.join("PatchReceipt.plist");
    let mut root = Value::from_file(&path)
        .ok()
        .and_then(Value::into_dictionary)
        .unwrap_or_default();
    let mut parts = root
        .remove("Parts")
        .and_then(Value::into_dictionary)
        .unwrap_or_default();
    let strings = |set: &BTreeSet<String>| Value::Array(set.iter().map(|s| Value::from(s.as_str())).collect());
    let mut entry = Dictionary::new();
    entry.insert("Patches".into(), strings(live));
    entry.insert("Writer".into(), Value::from("cfw install"));
    entry.insert("Date".into(), Value::Date(Date::from(SystemTime::now())));
    entry.insert("NotApplicable".into(), strings(not_applicable));
    parts.insert("Guest".into(), Value::Dictionary(entry));
    root.insert("Parts".into(), Value::Dictionary(parts));
    fs::write(&path, plist_py::to_xml(&Value::Dictionary(root))).map_err(|e| Error::path(&path, e))
}

/// The `Cryptex1,SystemOS` and `Cryptex1,AppOS` image paths a BuildManifest names.
pub fn cryptex_paths(manifest: &Value) -> Result<(String, String)> {
    let identities = manifest
        .as_dictionary()
        .and_then(|d| d.get("BuildIdentities"))
        .and_then(Value::as_array)
        .ok_or_else(|| Error::msg("BuildManifest has no BuildIdentities"))?;
    let path_of = |identity: &Value, key: &str| -> Option<String> {
        identity
            .as_dictionary()?
            .get("Manifest")?
            .as_dictionary()?
            .get(key)?
            .as_dictionary()?
            .get("Info")?
            .as_dictionary()?
            .get("Path")?
            .as_string()
            .map(str::to_owned)
            .filter(|p| !p.is_empty())
    };
    for identity in identities {
        if let (Some(system), Some(app)) = (path_of(identity, "Cryptex1,SystemOS"), path_of(identity, "Cryptex1,AppOS")) {
            for path in [&system, &app] {
                if path.starts_with('/') || path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
                    bail!("cryptex image path is not plain: {path}");
                }
            }
            return Ok((system, app));
        }
    }
    bail!("Cryptex1,SystemOS/AppOS paths not found in any BuildIdentity");
}

/// Merge a daemon plist into `launchd.plist`'s LaunchDaemons (CustomFirmwareDaemons.inject).
pub fn inject_daemon(launchd: &[u8], name: &str, daemon: &[u8]) -> Result<Vec<u8>> {
    let mut target = match Value::from_reader(std::io::Cursor::new(launchd))? {
        Value::Dictionary(d) => d,
        _ => bail!("launchd.plist is not a dictionary"),
    };
    let contents = Value::from_reader(std::io::Cursor::new(daemon))?;
    if contents.as_dictionary().is_none() {
        bail!("{name}: the daemon plist is not a dictionary");
    }
    let mut daemons = target
        .remove("LaunchDaemons")
        .and_then(Value::into_dictionary)
        .unwrap_or_default();
    daemons.insert(format!("/System/Library/LaunchDaemons/{name}.plist"), contents);
    target.insert("LaunchDaemons".into(), Value::Dictionary(daemons));
    Ok(plist_py::to_xml(&Value::Dictionary(target)))
}

/// What the install leaves behind for the caller.
#[derive(Debug, Default)]
pub struct InstallReport {
    pub live: BTreeSet<String>,
    pub not_applicable: BTreeSet<String>,
    pub failures: Vec<String>,
    /// Extended attributes in the cryptexes that could not be carried, by name.
    pub xattrs_not_carried: BTreeMap<String, u64>,
}

/// The inputs of `install`.
pub struct InstallContext<'a> {
    pub host: &'a dyn Host,
    pub vm: &'a Path,
    pub release: &'a Release,
    /// The helper with vphone-cli's commands (`libvphone_cli.so`).
    pub cli: &'a str,
    /// A private working directory; kept between runs so a re-run can resume.
    pub work: &'a Path,
    pub log: &'a Path,
    pub ssh: &'a GuestSsh,
    /// More connections to the guest's SSH, for the copies of gigabytes; `None` keeps to `ssh`.
    pub more: Option<tree::MoreLinks<'a>>,
    pub system: GuestVolume<'a>,
    pub xart: GuestVolume<'a>,
    pub preboot: GuestVolume<'a>,
}

struct MachoSpec<'s> {
    identifier: &'s str,
    path: &'s str,
    verbs: &'s [&'s str],
    code_identifier: Option<&'s str>,
    preserve_entitlements: bool,
    inject: Option<&'s str>,
    bundle: Option<&'s str>,
}

impl<'s> MachoSpec<'s> {
    fn new(identifier: &'s str, path: &'s str) -> Self {
        Self {
            identifier,
            path,
            verbs: &[],
            code_identifier: None,
            preserve_entitlements: false,
            inject: None,
            bundle: None,
        }
    }
}

struct Installer<'a, 'c> {
    cx: &'c InstallContext<'a>,
    records: &'c mut StepRecord,
    plan: Option<Plan>,
    device: Device,
    version: String,
    prior: BTreeSet<String>,
    live: BTreeSet<String>,
    failures: Vec<String>,
    xattrs: BTreeMap<String, u64>,
    /// Where the OS cryptex's decrypted image is (for local dyld-cache extraction).
    os_image: Option<PathBuf>,
}

/// Run the install. The VM must be in the SSH ramdisk with its volumes mounted.
pub fn install(cx: &InstallContext, records: &mut StepRecord) -> Result<InstallReport> {
    fs::create_dir_all(cx.work).map_err(|e| Error::path(cx.work, e))?;
    let host = cx.host;
    let plan = Plan::read(cx.vm);
    match &plan {
        Some(p) => host.log(&format!("[*] Patch preset: {}  ({} patches on)", p.preset, p.enabled.len())),
        None => host.log("[*] No patch plan recorded; applying every guest patch"),
    }
    let config = Value::from_file(cx.vm.join("config.plist")).map_err(|e| Error::msg(format!("config.plist: {e}")))?;
    let product = config
        .as_dictionary()
        .and_then(|d| d.get("guestProductType"))
        .and_then(Value::as_string)
        .unwrap_or("iPhone17,3")
        .to_owned();
    let device = device_named(&product)?;
    let was_installed = cx.system.exists("System/Library/xpc/launchd.plist.bak").unwrap_or(false);
    let prior = receipt_part(cx.vm, "Guest").unwrap_or_else(|| {
        if was_installed {
            plan.as_ref()
                .map(|p| p.enabled.iter().filter(|e| e.starts_with("system-") || e.starts_with("dyld-") || e.starts_with("preboot-")).cloned().collect())
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        }
    });
    host.log(&format!("[*] wasInstalled={was_installed}, mode=full"));
    let mut i = Installer {
        cx,
        records,
        plan,
        device,
        version: String::new(),
        prior,
        live: BTreeSet::new(),
        failures: Vec::new(),
        xattrs: BTreeMap::new(),
        os_image: None,
    };
    i.run()?;
    let not_applicable = i.not_applicable();
    let report = InstallReport {
        live: i.live,
        not_applicable,
        failures: i.failures,
        xattrs_not_carried: i.xattrs,
    };
    if !report.failures.is_empty() {
        host.log(&format!(
            "[!] {} guest patch(es) failed and were left in their prior state: {}",
            report.failures.len(),
            report.failures.join(", ")
        ));
    }
    Ok(report)
}

impl<'a, 'c> Installer<'a, 'c> {
    fn host(&self) -> &'a dyn Host {
        self.cx.host
    }

    fn log(&self, line: impl AsRef<str>) {
        self.host().log(line.as_ref());
    }

    fn on(&self, identifier: &str) -> bool {
        match &self.plan {
            Some(plan) => {
                if !plan.enabled.contains(identifier) {
                    self.log(format!("  [·] {identifier}: off in preset {}", plan.preset));
                    return false;
                }
                true
            }
            None => ![id::SETTINGS_ROOT_ROWS, id::SETTINGS_SW_UPDATE, id::GYRO, id::ACCEL, id::ATTITUDE, id::HINGE, id::FOLD_GUARD].contains(&identifier),
        }
    }

    /// Whether a device-tree patch is on (a VM with no plan gets every one).
    fn dt_on(&self, identifier: &str) -> bool {
        self.plan.as_ref().is_none_or(|p| p.enabled.contains(identifier))
    }

    fn enabled(&self, identifier: &str) -> bool {
        self.plan.as_ref().map_or(!matches!(identifier, id::SETTINGS_ROOT_ROWS | id::SETTINGS_SW_UPDATE), |p| p.enabled.contains(identifier))
    }

    fn backup_safe(&self, ids: &[&str]) -> bool {
        ids.iter().all(|i| !self.prior.contains(*i))
    }

    /// A patch step: a failure is reported, the patch keeps its prior-live state
    /// and the install carries on. Cancellation still stops it.
    fn isolate(&mut self, ids: &[&str], body: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        match body(self) {
            Ok(()) => Ok(()),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(error) => {
                self.failures.extend(ids.iter().map(|s| s.to_string()));
                self.log(format!(
                    "  [!] {}: patch step failed, left in its prior state: {error}",
                    ids.join(", ")
                ));
                for i in ids {
                    if self.prior.contains(*i) {
                        self.live.insert(i.to_string());
                    }
                }
                Ok(())
            }
        }
    }

    fn cli(&self, args: &[String]) -> Result<()> {
        check_cancel(self.host())?;
        let code = self
            .host()
            .run_helper(self.cx.cli, args, &[], self.cx.log, None)
            .map_err(Error::msg)?;
        if code != 0 {
            bail!("{} {} failed ({code})", self.cx.cli, args.join(" "));
        }
        Ok(())
    }

    /// `vphone-cli cfw <verb> <args...>`.
    fn patch(&self, verb: &str, args: &[String]) -> Result<()> {
        let mut all = vec!["cfw".to_owned(), verb.to_owned()];
        all.extend_from_slice(args);
        self.cli(&all)
    }

    fn staged(&self, name: &str) -> PathBuf {
        self.cx.work.join(name)
    }

    fn require_x1(&self) -> Result<()> {
        let required = [id::X1_DYLD, id::X1_CACHE, id::X1_LAUNCHD, id::X1_FSCK, id::X1_MOUNT];
        let failed: BTreeSet<&str> = required
            .iter()
            .filter(|r| self.failures.iter().any(|f| f == *r))
            .filter(|r| self.plan.as_ref().map_or(self.version.starts_with("27."), |p| p.enabled.contains(**r)))
            .copied()
            .collect();
        if !failed.is_empty() {
            bail!("Required X1 boot patches failed; installation stopped before setup boot: {failed:?}. See the patch error above.");
        }
        Ok(())
    }

    fn stage_path(&self, path: &str) -> Result<(PathBuf, String)> {
        let name = path.rsplit('/').next().unwrap().to_owned();
        let staged = self.staged(&name);
        Ok((staged, name))
    }

    /// Stage a guest Mach-O, patch it, re-sign it and put it back.
    fn patch_macho(&self, spec: &MachoSpec) -> Result<()> {
        let system = &self.cx.system;
        let backup = format!("{}.bak", spec.path);
        if !system.exists(&backup)? {
            system.copy_within(spec.path, &backup)?;
        }
        let (staged, _) = self.stage_path(spec.path)?;
        let _ = fs::remove_file(&staged);
        system.download(&backup, &staged)?;
        let data = fs::read(&staged).map_err(|e| Error::path(&staged, e))?;
        let signing_identifier = match spec.code_identifier {
            Some(i) => Some(i.to_owned()),
            None => sign::identifier_of(&data)?,
        };
        let entitlements = if spec.preserve_entitlements {
            sign::entitlements_of(&data)?.into_iter().find(|e| !e.is_empty())
        } else {
            None
        };
        drop(data);
        let staged_arg = staged.to_string_lossy().into_owned();
        for verb in spec.verbs {
            self.patch(verb, std::slice::from_ref(&staged_arg))?;
        }
        if let Some(lib) = spec.inject {
            self.patch("inject-dylib", &[staged_arg.clone(), lib.to_owned()])?;
        }
        // The ad-hoc shape: TXM 187.100.3 routes a CodeDirectory without the
        // CS_ADHOC flag into a branch that demands a CMS wrapper.
        sign::sign_file(
            &staged,
            &SignOptions {
                identifier: signing_identifier,
                entitlements,
                merge_existing: true,
            },
        )?;
        system.replace_file(spec.path, &staged, 0o755, OWNER)?;
        if let Some(bundle) = spec.bundle {
            self.seal_guest_bundle(bundle)?;
        }
        Ok(())
    }

    /// Restore a guest Mach-O from its `.bak`, re-sealing its bundle; false when
    /// there is no backup.
    fn revert_macho(&self, path: &str, bundle: Option<&str>) -> Result<bool> {
        let system = &self.cx.system;
        let backup = format!("{path}.bak");
        if !system.exists(&backup)? {
            return Ok(false);
        }
        let attrs = system.status(path)?;
        let (staged, _) = self.stage_path(path)?;
        let _ = fs::remove_file(&staged);
        system.download(&backup, &staged)?;
        let (mode, owner) = attrs.map_or((0o755, OWNER), |a| (a.perm(), (a.uid, a.gid)));
        system.replace_file(path, &staged, mode, owner)?;
        system.remove_item(&backup)?;
        if let Some(bundle) = bundle {
            self.seal_guest_bundle(bundle)?;
        }
        Ok(true)
    }

    /// Bring a guest directory to a local copy, recording each file's guest attrs.
    fn pull_dir(&self, relative: &str, local: &Path, attrs: &mut BTreeMap<String, Attrs>) -> Result<()> {
        fs::create_dir_all(local).map_err(|e| Error::path(local, e))?;
        for (name, a) in self.cx.system.list(relative)? {
            let child = format!("{relative}/{name}");
            let out = local.join(&name);
            attrs.insert(child.clone(), a);
            if a.is_dir() {
                self.pull_dir(&child, &out, attrs)?;
            } else if a.is_symlink() {
                let target = self.cx.system.read_link(&child)?;
                let _ = fs::remove_file(&out);
                std::os::unix::fs::symlink(target, &out).map_err(|e| Error::path(&out, e))?;
            } else {
                self.cx.system.download(&child, &out)?;
            }
        }
        Ok(())
    }

    /// `rcodesign sign --shallow` on a guest bundle: pull it, seal it locally
    /// with apple-codesign, and push back only the files that changed.
    fn seal_guest_bundle(&self, bundle: &str) -> Result<()> {
        let local = self.staged(&format!("seal-{}", bundle.replace('/', "_")));
        let _ = fs::remove_dir_all(&local);
        let mut guest_attrs = BTreeMap::new();
        self.pull_dir(bundle, &local, &mut guest_attrs)?;
        let before = snapshot_files(&local, "")?;
        sign::seal_bundle(&local)?;
        let after = snapshot_files(&local, "")?;
        for (relative, data) in &after {
            if before.get(relative) == Some(data) {
                continue;
            }
            let guest_path = format!("{bundle}/{relative}");
            let (mode, owner) = guest_attrs
                .get(&guest_path)
                .map_or((0o644, OWNER), |a| (a.perm(), (a.uid, a.gid)));
            if let Some(parent) = guest_path.rsplit_once('/').map(|(p, _)| p) {
                self.cx.system.mkdir_p(parent, 0o755)?;
            }
            self.cx.system.write_data(&guest_path, data, mode, owner)?;
        }
        let _ = fs::remove_dir_all(&local);
        Ok(())
    }

    fn macho(&mut self, spec: MachoSpec) -> Result<()> {
        let identifier = spec.identifier.to_owned();
        self.isolate(&[identifier.as_str()], |s| {
            if s.on(&identifier) {
                s.patch_macho(&spec)?;
                s.live.insert(identifier.clone());
            } else if s.revert_macho(spec.path, spec.bundle)? {
                s.log(format!("  [+] {identifier}: restored {} from backup", spec.path));
            } else if s.prior.contains(&identifier) {
                s.log(format!("  [!] {identifier}: off now but no backup to revert; leaving the patched binary (not revertible)"));
                s.live.insert(identifier.clone());
            }
            Ok(())
        })
    }

    /// Run a verb on a private copy of a guest file (patchCopy).
    fn patch_copy(&self, volume: &GuestVolume, relative: &str, verb: &str, arguments: &[String], backup_safe: bool) -> Result<()> {
        let Some(original) = volume.status(relative)?.filter(|a| a.is_file()) else {
            bail!("{relative} on the VM is missing or is not a regular file. Restore the VM, then install CFW again.");
        };
        let folder = self.staged(&format!("stage-{}", relative.replace('/', "_")));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).map_err(|e| Error::path(&folder, e))?;
        let leaf = relative.rsplit('/').next().unwrap();
        let staged = folder.join(leaf);
        volume.download(relative, &staged)?;
        let before = fs::read(&staged).map_err(|e| Error::path(&staged, e))?;
        let mut args = vec![staged.to_string_lossy().into_owned()];
        args.extend_from_slice(arguments);
        self.patch(verb, &args)?;
        let after = fs::read(&staged).map_err(|e| Error::path(&staged, e))?;
        if after != before {
            let backup = format!("{relative}.bak");
            if backup_safe && !volume.exists(&backup)? {
                volume.write_data(&backup, &before, original.perm(), (original.uid, original.gid))?;
            }
            volume.write_data(relative, &after, original.perm(), (original.uid, original.gid))?;
        }
        let _ = fs::remove_dir_all(&folder);
        Ok(())
    }

    fn revert_copy(&self, volume: &GuestVolume, relative: &str) -> Result<bool> {
        let backup = format!("{relative}.bak");
        if !volume.exists(&backup)? {
            return Ok(false);
        }
        volume.rename(&backup, relative)?;
        Ok(true)
    }

    fn product_version(&self) -> Result<String> {
        let data = self.cx.system.read_data("System/Library/CoreServices/SystemVersion.plist")?;
        Value::from_reader(std::io::Cursor::new(data))
            .ok()
            .and_then(|v| v.as_dictionary()?.get("ProductVersion")?.as_string().map(str::to_owned))
            .ok_or_else(|| Error::msg("Unable to read the iOS version from the VM system volume. Restore the VM, then install CFW again."))
    }

    fn restore_tree(&self) -> Result<PathBuf> {
        crate::manifest::restore_tree(self.cx.vm)
    }

    /// `populated()`: the cryptex is present on System or already on Preboot.
    fn populated(&self, path: &str, name: &str) -> Result<bool> {
        if self.cx.system.exists(path)? && !self.cx.system.entries(path).unwrap_or_default().is_empty() {
            return Ok(true);
        }
        Ok(self
            .cx
            .preboot
            .entries(&format!("Cryptexes/{name}"))
            .is_ok_and(|e| !e.is_empty()))
    }

    /// The decrypted SystemOS image (decrypting with `fw aea-decrypt` when it is
    /// an AEA archive), and the AppOS image.
    fn cryptex_images(&self) -> Result<(PathBuf, PathBuf)> {
        let tree = self.restore_tree()?;
        let manifest_path = tree.join("iPhone-BuildManifest.plist");
        let manifest = Value::from_file(&manifest_path).map_err(|e| Error::msg(format!("{}: {e}", manifest_path.display())))?;
        let (system_rel, app_rel) = cryptex_paths(&manifest)?;
        let system_image = tree.join(&system_rel);
        let app_image = tree.join(&app_rel);
        for f in [&system_image, &app_image] {
            if !f.is_file() {
                bail!("The Cryptex image {} is missing. Run fw prepare, then install CFW again.", f.display());
            }
        }
        let mut magic = [0u8; 4];
        fs::File::open(&system_image)
            .and_then(|mut f| f.read_exact(&mut magic))
            .map_err(|e| Error::path(&system_image, e))?;
        if &magic != b"AEA1" {
            return Ok((system_image, app_image));
        }
        let plain = self.staged("SystemOS.dmg");
        let done = self.staged("SystemOS.dmg.done");
        if !done.exists() {
            let _ = fs::remove_file(&plain);
            self.log("[*] decrypting the OS cryptex (fw aea-decrypt)");
            self.cli(&[
                "fw".into(),
                "aea-decrypt".into(),
                "-i".into(),
                system_image.to_string_lossy().into_owned(),
                "-o".into(),
                plain.to_string_lossy().into_owned(),
            ])?;
            fs::write(&done, b"").map_err(|e| Error::path(&done, e))?;
        }
        Ok((plain, app_image))
    }

    fn os_tree(&self, image: &Path, root: &str) -> ApfsTree {
        ApfsTree {
            image: image.to_path_buf(),
            selector: VolumeSelector::default(),
            root: root.to_owned(),
        }
    }

    /// installCryptexes: copy the OS and App cryptex volumes into System.
    fn install_cryptexes(&mut self) -> Result<()> {
        let os_ok = self.populated(OS, "OS")?;
        let app_ok = self.populated(APP, "App")?;
        if !(os_ok && app_ok) {
            let (os_image, app_image) = self.cryptex_images()?;
            for (key, image, dest, skip_dsc) in [
                ("cfw.cryptex-os", os_image.clone(), OS, true),
                ("cfw.cryptex-app", app_image, APP, false),
            ] {
                if self.records.is_done(key) {
                    self.log(format!("  • {key}  (already done)"));
                    continue;
                }
                // The restored rootfs has dangling Cryptex symlinks; they are
                // removed as links.
                self.cx.system.remove_item(dest)?;
                self.cx.system.mkdir_p(dest, 0o755)?;
                let tree = self.os_tree(&image, "/");
                let exclude = move |rel: &str| skip_dsc && rel == DSC_IN_OS;
                let guest_dest = self.cx.system.path(dest)?;
                let what = format!("iOS's system files ({dest})");
                let stats = tree::upload_tree_over(&what, self.cx.ssh, self.cx.more.as_ref(), &guest_dest, &tree, None, &exclude, self.host())?;
                self.log(format!(
                    "[+] {dest}: {} files, {} dirs, {} links, {} MiB",
                    stats.files,
                    stats.dirs,
                    stats.symlinks,
                    stats.bytes >> 20
                ));
                for (name, count) in stats.xattrs_not_carried {
                    *self.xattrs.entry(name).or_default() += count;
                }
                self.records.mark(key)?;
            }
            self.os_image = Some(os_image);
        }
        self.cx.system.create_symlink(
            "../../../System/Cryptexes/OS/System/Library/Caches/com.apple.dyld",
            "System/Library/Caches/com.apple.dyld",
        )?;
        self.cx.system.create_symlink(
            "../../../../System/Cryptexes/OS/System/DriverKit/System/Library/dyld",
            "System/DriverKit/System/Library/dyld",
        )
    }

    fn local_dsc(&self) -> PathBuf {
        self.staged("dsc")
    }

    /// A pristine local copy of the dyld cache directory, and the metadata to
    /// upload it with.
    fn extract_local_dsc(&mut self) -> Result<BTreeMap<String, Meta>> {
        let dsc = self.local_dsc();
        let _ = fs::remove_dir_all(&dsc);
        let image = match &self.os_image {
            Some(image) => image.clone(),
            None => self.cryptex_images()?.0,
        };
        self.log("[*] taking the dyld shared cache out of the OS cryptex");
        self.os_tree(&image, &format!("/{DSC_IN_OS}")).extract(&dsc, self.host())
    }

    fn shared_region_check(&self, dsc: &Path) -> Result<()> {
        let widened = receipt_part(self.cx.vm, "kernelcache")
            .map_or(self.version.starts_with("27."), |k| k.contains("kernel-boot-shared_region_size"));
        let region = if widened { KERNEL_REGION_WIDENED } else { KERNEL_REGION };
        let main = dsc.join("dyld_shared_cache_arm64e");
        let mut header = [0u8; 0x100];
        let read = fs::File::open(&main).and_then(|mut f| f.read_exact(&mut header));
        if read.is_err() || &header[..7] != b"dyld_v1" {
            self.log("  [·] shared region check skipped: the cache is not a plain arm64e file");
            return Ok(());
        }
        let mapping_offset = u32::from_le_bytes(header[0x10..0x14].try_into().unwrap());
        if mapping_offset < 0xF8 {
            self.log("  [·] shared region check skipped: this cache version has no maxSlide field");
            return Ok(());
        }
        let size = u64::from_le_bytes(header[0xE8..0xF0].try_into().unwrap());
        if size > region {
            bail!("the dyld shared cache spans 0x{size:x}, more than this VM kernel's shared region (0x{region:x}): it would panic at first boot");
        }
        Ok(())
    }

    fn apply_dyld_patches(&mut self, dsc: &Path) -> Result<()> {
        let dsc_arg = dsc.to_string_lossy().into_owned();
        let undo = dsc.join(DSC_UNDO_LOG).to_string_lossy().into_owned();
        let main = if !dsc.join("dyld_shared_cache_arm64e").exists() && dsc.join("dyld_shared_cache_arm64e_x1").exists() {
            dsc.join("dyld_shared_cache_arm64e_x1")
        } else {
            dsc.join("dyld_shared_cache_arm64e")
        };
        let version = self.version.clone();
        let mut patches: Vec<(&str, &str, Vec<String>)> = Vec::new();
        if version.starts_with("27.") {
            patches.push((id::X1_CACHE, "patch-arm64e-x1-bootstrap-cache", vec![dsc_arg.clone()]));
            patches.push((id::X1_CACHE_REST, "patch-arm64e-x1-cache", vec![dsc_arg.clone()]));
            patches.push(("dyld-boot-iomfb_force_kern", "patch-iomfb-force-kern", vec![dsc_arg.clone()]));
            patches.push(("dyld-boot-maxslide", "patch-dsc-maxslide", vec![dsc_arg.clone()]));
            patches.push(("dyld-boot-lsd_embedded_reg", "patch-lsd-embedded-reg", vec![dsc_arg.clone()]));
            patches.push(("dyld-boot-xpc_lwcr", "patch-xpc-lwcr", vec![dsc_arg.clone()]));
            patches.push(("dyld-boot-lockdown_mode", "patch-lockdown-mode", vec![dsc_arg.clone()]));
        } else if version.starts_with("26.0") || version.starts_with("18.") {
            patches.push(("dyld-boot-iomfb_swapend", "patch-iomfb-swapend", vec![dsc_arg.clone(), "--target-size".into(), "0x560".into()]));
        }
        patches.push((id::SETTINGS_SW_UPDATE, "patch-settings-software-update", vec![dsc_arg.clone()]));
        patches.push((id::MIS_TRUST, "patch-mis-trust-auth", vec![dsc_arg.clone()]));
        patches.push(("dyld-exp-hv_vmm", "patch-hv-vmm-dsc", vec![dsc_arg.clone()]));
        patches.push(("dyld-cfw-camera", "patch-camera-dsc", vec![dsc_arg.clone(), main.to_string_lossy().into_owned()]));
        let mut revertible = Vec::new();
        for (identifier, verb, mut args) in patches {
            let want = self.enabled(identifier);
            let had = self.prior.contains(identifier);
            args.extend(["--undo-log".to_owned(), undo.clone(), "--undo-id".to_owned(), identifier.to_owned()]);
            match (want, had) {
                (true, true) if identifier == "dyld-boot-maxslide" || identifier == id::X1_CACHE => {
                    if let Err(e) = self.patch(verb, &args) {
                        self.dyld_failed(identifier, &e)?;
                    } else {
                        self.live.insert(identifier.to_owned());
                    }
                }
                (true, true) => {
                    self.live.insert(identifier.to_owned());
                }
                (true, false) => match self.patch(verb, &args) {
                    Ok(()) => {
                        self.live.insert(identifier.to_owned());
                    }
                    Err(e) => self.dyld_failed(identifier, &e)?,
                },
                (false, true) => revertible.push(identifier.to_owned()),
                (false, false) => {}
            }
        }
        for identifier in revertible {
            let args = vec![dsc_arg.clone(), "--undo-log".into(), undo.clone(), "--patch".into(), identifier.clone()];
            if let Err(e) = self.patch("patch-dsc-revert", &args) {
                self.failures.push(identifier.clone());
                self.log(format!("  [!] dyld revert failed, left in prior state: {e}"));
                self.live.insert(identifier);
            }
        }
        Ok(())
    }

    fn dyld_failed(&mut self, identifier: &str, error: &Error) -> Result<()> {
        if matches!(error, Error::Cancelled) {
            return Err(Error::Cancelled);
        }
        self.failures.push(identifier.to_owned());
        self.log(format!("  [!] {identifier}: dyld patch failed, left in its prior state: {error}"));
        if identifier == id::X1_CACHE {
            bail!("Required X1 boot patch {identifier} failed; installation stopped before setup boot: {error}");
        }
        Ok(())
    }

    fn install_vphoned(&mut self) -> Result<()> {
        let release = self.cx.release;
        let staged = self.staged("vphoned");
        fs::copy(release.guest("vphoned"), &staged).map_err(|e| Error::path(&staged, e))?;
        sign::sign_file(
            &staged,
            &SignOptions {
                identifier: Some("vphoned".into()),
                entitlements: Some(VPHONED_ENTITLEMENTS.as_bytes().to_vec()),
                merge_existing: true,
            },
        )?;
        let system = &self.cx.system;
        system.mkdir_p("usr/lib/vphone", 0o755)?;
        system.replace_file(
            "usr/lib/vphone/libswiftCompatibilitySpan.dylib",
            &release.guest("libswiftCompatibilitySpan.dylib"),
            0o755,
            OWNER,
        )?;
        system.replace_file("usr/bin/vphoned", &staged, 0o755, OWNER)?;
        let daemon = release.guest("vphoned.plist");
        system.replace_file("System/Library/LaunchDaemons/vphoned.plist", &daemon, 0o644, OWNER)?;
        let backup = format!("{LAUNCHD}.bak");
        if !system.exists(&backup)? {
            system.copy_within(LAUNCHD, &backup)?;
        }
        let launchd = system.read_data(&backup)?;
        let daemon_data = fs::read(&daemon).map_err(|e| Error::path(&daemon, e))?;
        let injected = inject_daemon(&launchd, "vphoned", &daemon_data)?;
        system.write_data(LAUNCHD, &injected, 0o644, OWNER)
    }

    fn install_alias(&self, alias: &str, target: &str) -> Result<()> {
        let system = &self.cx.system;
        if system.exists(alias)? {
            if system.read_link(alias).ok().as_deref() != Some(target) {
                bail!("Another file already uses /{alias} on the VM system volume. Remove it, then install CFW again.");
            }
            return Ok(());
        }
        system.create_symlink(target, alias)
    }

    fn install_environment(&mut self) -> Result<()> {
        for name in ENV_LIBRARIES.iter().filter(|n| !SELECTED_LIBRARIES.contains(n)) {
            self.cx
                .system
                .replace_file(&format!("usr/lib/{name}"), &self.cx.release.guest(name), 0o755, OWNER)?;
        }
        // launchd has little free header space: /vh fits the same 32-byte command.
        self.install_alias("vh", "/usr/lib/launchdhook-vphone.dylib")?;
        let plist = "usr/lib/libmisfix.plist";
        if self.cx.system.exists(plist)? {
            self.log("  [·] usr/lib/libmisfix.plist: already present, left as it is");
        } else {
            self.cx
                .system
                .replace_file(plist, &self.cx.release.guest("libmisfix.plist"), 0o644, OWNER)?;
        }
        Ok(())
    }

    fn apply_simple_libraries(&mut self) -> Result<()> {
        let list = [
            (id::FOLLOWUP, "libprefsfix.dylib"),
            (id::SIGNIN, "libsigninfix.dylib"),
            (id::GYRO, "libvphonegyro.dylib"),
            (id::ACCEL, "libvphoneaccelerometer.dylib"),
            (id::ATTITUDE, "libvphoneattitude.dylib"),
            (id::HINGE, "libvphonehinge.dylib"),
            (id::FOLD_GUARD, "libvphonefoldguard.dylib"),
            (id::FOLD_DISPLAY, "libvphonefolddisplay.dylib"),
        ];
        for (identifier, library) in list {
            self.isolate(&[identifier], |s| {
                let path = format!("usr/lib/{library}");
                let fold_only = [id::HINGE, id::FOLD_GUARD, id::FOLD_DISPLAY].contains(&identifier);
                if s.on(identifier) && (!fold_only || s.device.foldable()) {
                    s.cx.system.replace_file(&path, &s.cx.release.guest(library), 0o755, OWNER)?;
                    s.live.insert(identifier.to_owned());
                } else {
                    s.cx.system.remove_item(&path)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn install_gpu_bundle(&self) -> Result<()> {
        let tree = self.restore_tree()?;
        let staged = tree.join(".pcc-gpu/AppleParavirtGPUMetalIOGPUFamily.bundle");
        if !staged.is_dir() {
            bail!("PCC GPU driver is missing: {}. Re-run fw prepare with the PCC IPSW.", staged.display());
        }
        let system = &self.cx.system;
        system.remove_item(GPU)?;
        system.mkdir_p(GPU, 0o755)?;
        let dest = system.path(GPU)?;
        let source = LocalTree::new(&staged);
        tree::upload_tree_as("the GPU driver", self.cx.ssh, &dest, &source, Some(OWNER), &|_| false, self.host())?;
        for file in [GPU.to_owned(), format!("{GPU}/AppleParavirtGPUMetalIOGPUFamily"), format!("{GPU}/_CodeSignature")] {
            system.set_mode(&file, 0o755)?;
        }
        let plugin = format!("{GPU}/libAppleParavirtCompilerPluginIOGPUFamily.dylib");
        if !system.exists(&plugin)? {
            bail!("PCC GPU compiler plugin is missing: {plugin}. Re-run fw prepare with a complete vphone-cli.app.");
        }
        system.set_mode(&plugin, 0o755)?;
        for file in [format!("{GPU}/Info.plist"), format!("{GPU}/_CodeSignature/CodeResources")] {
            system.set_mode(&file, 0o644)?;
        }
        Ok(())
    }

    fn install_virtio_sound(&self) -> Result<()> {
        let target = "System/Library/Audio/Plug-Ins/HAL/VPhoneVirtIOSound.driver";
        let source = LocalTree::new(self.cx.release.guest("VPhoneVirtIOSound.driver"));
        let system = &self.cx.system;
        system.remove_item(target)?;
        system.mkdir_p(target, 0o755)?;
        let dest = system.path(target)?;
        tree::upload_tree_as("the sound driver", self.cx.ssh, &dest, &source, Some(OWNER), &|_| false, self.host())?;
        for dir in [target.to_owned(), format!("{target}/_CodeSignature")] {
            system.set_mode(&dir, 0o755)?;
        }
        system.set_mode(&format!("{target}/VPhoneVirtIOSound"), 0o755)?;
        for file in [format!("{target}/Info.plist"), format!("{target}/_CodeSignature/CodeResources")] {
            system.set_mode(&file, 0o644)?;
        }
        self.log(format!("  [+] /{target}"));
        Ok(())
    }

    fn patch_watchdog(&self) -> Result<()> {
        let system = &self.cx.system;
        let target = "usr/libexec/watchdogd";
        let backup = format!("{target}.bak");
        if !system.exists(&backup)? {
            system.copy_within(target, &backup)?;
        }
        let staged = self.staged("watchdogd");
        let _ = fs::remove_file(&staged);
        system.download(&backup, &staged)?;
        // The patcher re-attests watchdogd's original pages; re-signing it would
        // change Apple's identifier and break launchd's boot-task identity check.
        self.patch("patch-watchdogd", &[staged.to_string_lossy().into_owned()])?;
        system.replace_file(target, &staged, 0o755, OWNER)
    }

    fn revert_virtio_sound(&self) -> Result<bool> {
        let target = "System/Library/Audio/Plug-Ins/HAL/VPhoneVirtIOSound.driver";
        if !self.cx.system.exists(target)? {
            return Ok(false);
        }
        self.cx.system.remove_item(target)?;
        Ok(true)
    }

    fn patch_debugserver(&self, backup_safe: bool) -> Result<bool> {
        let system = &self.cx.system;
        let target = "usr/libexec/debugserver";
        if !system.is_regular(target)? {
            self.log("[!] debugserver absent; entitlement patch skipped");
            return Ok(false);
        }
        let staged = self.staged("debugserver");
        let _ = fs::remove_file(&staged);
        system.download(target, &staged)?;
        let data = fs::read(&staged).map_err(|e| Error::path(&staged, e))?;
        let Some(source) = sign::entitlements_of(&data)?.into_iter().find(|e| !e.is_empty()) else {
            self.log("[!] debugserver has no readable entitlements; patch skipped");
            return Ok(false);
        };
        let Some(mut plist) = Value::from_reader_xml(&source[..]).ok().and_then(Value::into_dictionary) else {
            self.log("[!] debugserver has no readable entitlements; patch skipped");
            return Ok(false);
        };
        plist.remove("seatbelt-profiles");
        plist.insert("task_for_pid-allow".into(), Value::Boolean(true));
        let mut xml = Vec::new();
        plist::to_writer_xml(&mut xml, &Value::Dictionary(plist))?;
        let backup = format!("{target}.bak");
        if backup_safe && !system.exists(&backup)? {
            system.copy_within(target, &backup)?;
        }
        sign::sign_file(
            &staged,
            &SignOptions {
                entitlements: Some(xml),
                ..Default::default()
            },
        )?;
        system.replace_file(target, &staged, 0o755, OWNER)?;
        Ok(true)
    }

    fn patch_campo(&self, backup_safe: bool) -> Result<bool> {
        let system = &self.cx.system;
        let target = "Applications/Campo.app/Campo";
        if !system.is_regular(target)? {
            self.log("[!] Campo absent; entitlement patch skipped");
            return Ok(false);
        }
        let staged = self.staged("Campo");
        let _ = fs::remove_file(&staged);
        system.download(target, &staged)?;
        let data = fs::read(&staged).map_err(|e| Error::path(&staged, e))?;
        let Some(source) = sign::entitlements_of(&data)?.into_iter().find(|e| !e.is_empty()) else {
            self.log("[!] Campo has no readable entitlements; patch skipped");
            return Ok(false);
        };
        let ent = self.staged("Campo.entitlements");
        fs::write(&ent, &source).map_err(|e| Error::path(&ent, e))?;
        self.patch("patch-campo-entitlements", &[ent.to_string_lossy().into_owned()])?;
        let backup = format!("{target}.bak");
        if backup_safe && !system.exists(&backup)? {
            system.copy_within(target, &backup)?;
        }
        sign::sign_file(
            &staged,
            &SignOptions {
                entitlements: Some(fs::read(&ent).map_err(|e| Error::path(&ent, e))?),
                merge_existing: true,
                ..Default::default()
            },
        )?;
        system.replace_file(target, &staged, 0o755, OWNER)?;
        Ok(true)
    }

    fn restore_misfix_targets(&self) -> Result<()> {
        let system = &self.cx.system;
        for path in ["usr/libexec/installd", "usr/libexec/misagent", "System/Library/CoreServices/SpringBoard.app/SpringBoard"] {
            let backup = format!("{path}.bak");
            if system.is_regular(&backup)? {
                system.rename(&backup, path)?;
                self.log(format!("  [+] {path}: restored the original, libmisfix is inserted at spawn"));
            }
        }
        if system.is_symlink("mf")? && system.read_link("mf")? == "/usr/lib/libmisfix.dylib" {
            system.remove_item("mf")?;
        }
        Ok(())
    }

    fn rename_gigalocker(&self) -> Result<()> {
        let xart = &self.cx.xart;
        for source in xart.entries("")? {
            if !source.ends_with(".gl") || source == "AA.gl" {
                continue;
            }
            xart.remove_item("AA.gl")?;
            xart.rename(&source, "AA.gl")?;
        }
        Ok(())
    }

    fn patch_virtual_audio_graph(&mut self, verb: &str, backup_safe: bool) -> Result<bool> {
        let system = &self.cx.system;
        let tunings = "Library/Audio/Tunings";
        if !system.is_dir(tunings)? {
            self.log(format!("  [·] {tunings}: not on this VM, left out"));
            return Ok(false);
        }
        let mut patched = 0;
        for acoustic in system.entries(tunings)? {
            let plist = format!("{tunings}/{acoustic}/VAD/graph_configurations.plist");
            if !system.is_regular(&plist)? {
                continue;
            }
            self.patch_copy(system, &plist, verb, &[], backup_safe)?;
            patched += 1;
            if verb != "patch-virtualaudio-microphone-chains" {
                continue;
            }
            let strips = format!("{tunings}/{acoustic}/VAD");
            for name in system.entries(&strips)? {
                if name.contains("_mic") && name.ends_with("_measurement.austrip") {
                    let path = format!("{strips}/{name}");
                    if system.is_regular(&path)? {
                        self.patch_copy(system, &path, "patch-virtualaudio-microphone-gain", &[], backup_safe)?;
                    }
                }
            }
        }
        if patched == 0 {
            self.log(format!("  [·] {tunings}: no graph_configurations.plist under any tuning set, left out"));
            return Ok(false);
        }
        Ok(true)
    }

    fn restore_virtual_audio_tunings(&self) -> Result<bool> {
        let system = &self.cx.system;
        let tunings = "Library/Audio/Tunings";
        if !system.is_dir(tunings)? {
            return Ok(false);
        }
        let mut restored = false;
        for acoustic in system.entries(tunings)? {
            let vad = format!("{tunings}/{acoustic}/VAD");
            if !system.is_dir(&vad)? {
                continue;
            }
            for name in system.entries(&vad)? {
                if let Some(original) = name.strip_suffix(".bak") {
                    let backup = format!("{vad}/{name}");
                    if system.is_regular(&backup)? {
                        system.rename(&backup, &format!("{vad}/{original}"))?;
                        restored = true;
                    }
                }
            }
        }
        Ok(restored)
    }

    fn run(&mut self) -> Result<()> {
        if !self.records.is_done("cfw.cryptexes") {
            self.install_cryptexes()?;
            self.records.mark("cfw.cryptexes")?;
        }
        self.version = self.product_version()?;
        let version = self.version.clone();
        let v27 = version.starts_with("27.");

        for (identifier, path) in [
            (id::X1_DYLD, "usr/lib/dyld"),
            (id::X1_FSCK, "sbin/fsck"),
            (id::X1_MOUNT, "sbin/mount"),
        ] {
            if v27 || self.prior.contains(identifier) {
                self.macho(MachoSpec {
                    verbs: &["patch-arm64e-x1"],
                    preserve_entitlements: true,
                    ..MachoSpec::new(identifier, path)
                })?;
                self.require_x1()?;
            }
        }
        if version.starts_with("27.1.") || version == "27.1" || self.prior.contains(id::X1_CIBARCODE) {
            self.macho(MachoSpec {
                verbs: &["patch-arm64e-x1"],
                preserve_entitlements: true,
                bundle: Some("System/Library/CoreImage/CIBarcode.cifilter"),
                ..MachoSpec::new(id::X1_CIBARCODE, "System/Library/CoreImage/CIBarcode.cifilter/CIBarcode")
            })?;
        }

        // The dyld cache: pristine local copy, region preflight, patches. A run
        // that stopped partway starts again from the pristine bytes; the verbs
        // do not all tolerate an already-patched cache.
        let dsc = self.local_dsc();
        let metas_path = self.staged("dsc-meta.json");
        let metas = if self.records.is_done("cfw.dsc-patched") {
            read_metas(&metas_path)?
        } else {
            let metas = self.extract_local_dsc()?;
            write_metas(&metas_path, &metas)?;
            metas
        };
        self.shared_region_check(&dsc)?;

        // 1. The redeploy first, so no later patch failure can block it.
        self.isolate(&["system-vphoned-boot-install"], |s| {
            if s.on("system-vphoned-boot-install") {
                s.install_vphoned()?;
                s.live.insert("system-vphoned-boot-install".to_owned());
            }
            Ok(())
        })?;
        self.isolate(&["system-launchdaemons-boot-environment"], |s| {
            if s.on("system-launchdaemons-boot-environment") {
                s.install_environment()?;
                s.live.insert("system-launchdaemons-boot-environment".to_owned());
            }
            Ok(())
        })?;
        self.apply_simple_libraries()?;

        // 2. The dyld shared cache.
        let dsc_live_path = self.staged("dsc-live.json");
        if !self.records.is_done("cfw.dsc-patched") {
            self.apply_dyld_patches(&dsc)?;
            self.require_x1()?;
            let applied: Vec<&String> = self.live.iter().filter(|i| i.starts_with("dyld-")).collect();
            fs::write(&dsc_live_path, serde_json::to_vec(&applied).unwrap()).map_err(|e| Error::path(&dsc_live_path, e))?;
            self.records.mark("cfw.dsc-patched")?;
        } else {
            // A resumed run: the patched cache is what the earlier run left.
            let data = fs::read(&dsc_live_path).map_err(|e| Error::path(&dsc_live_path, e))?;
            let applied: Vec<String> = serde_json::from_slice(&data).map_err(|e| Error::msg(format!("{}: {e}", dsc_live_path.display())))?;
            self.live.extend(applied);
        }
        if !self.records.is_done("cfw.dsc-uploaded") {
            let source = LocalTree {
                root: dsc.clone(),
                meta: metas,
            };
            let system = &self.cx.system;
            system.remove_item(DSC_IN_SYSTEM)?;
            system.mkdir_p(DSC_IN_SYSTEM, 0o755)?;
            let dest = system.path(DSC_IN_SYSTEM)?;
            tree::upload_tree_over("the patched dyld cache", self.cx.ssh, self.cx.more.as_ref(), &dest, &source, None, &|_| false, self.host())?;
            self.records.mark("cfw.dsc-uploaded")?;
        }

        let bluetooth = "System/Library/FeatureFlags/Domain/BluetoothFeatures.plist";
        self.isolate(&[id::BLUETOOTH], |s| {
            if !version.starts_with('1') && s.on(id::BLUETOOTH) {
                let safe = s.backup_safe(&[id::BLUETOOTH]);
                s.patch_copy(&s.cx.system, bluetooth, "patch-virtual-bluetooth", &[], safe)?;
                s.live.insert(id::BLUETOOTH.to_owned());
            } else if s.revert_copy(&s.cx.system, bluetooth)? {
                s.log(format!("  [+] {}: restored the Bluetooth feature flags", id::BLUETOOTH));
            } else if s.prior.contains(id::BLUETOOTH) {
                s.live.insert(id::BLUETOOTH.to_owned());
            }
            Ok(())
        })?;
        let locationd = "System/Library/FeatureFlags/Domain/CoreLocation.plist";
        self.isolate(&[id::LOCATIOND], |s| {
            if v27 && s.on(id::LOCATIOND) {
                let safe = s.backup_safe(&[id::LOCATIOND]);
                s.patch_copy(&s.cx.system, locationd, "patch-locationd-cohorting", &[], safe)?;
                s.live.insert(id::LOCATIOND.to_owned());
            } else if s.revert_copy(&s.cx.system, locationd)? {
                s.log(format!("  [+] {}: restored CoreLocation feature flags", id::LOCATIOND));
            } else if s.prior.contains(id::LOCATIOND) {
                s.live.insert(id::LOCATIOND.to_owned());
            }
            Ok(())
        })?;
        self.isolate(&[id::JETSAM], |s| {
            let directory = "System/Library/LaunchDaemons";
            let names = s.cx.system.entries(directory)?;
            let is_jetsam = |n: &str| n.starts_with("com.apple.jetsamproperties.") && n.ends_with(".plist");
            if v27 && s.on(id::JETSAM) {
                let safe = s.backup_safe(&[id::JETSAM]);
                for name in names.iter().filter(|n| is_jetsam(n)) {
                    let path = format!("{directory}/{name}");
                    if s.cx.system.is_regular(&path)? {
                        s.patch_copy(&s.cx.system, &path, "patch-jetsam-disable-allowed", &[], safe)?;
                    }
                }
                s.live.insert(id::JETSAM.to_owned());
            } else {
                let mut reverted = false;
                for name in &names {
                    if let Some(original) = name.strip_suffix(".bak").filter(|o| is_jetsam(o)) {
                        reverted |= s.revert_copy(&s.cx.system, &format!("{directory}/{original}"))?;
                    }
                }
                if reverted {
                    s.log(format!("  [+] {}: restored the jetsam properties", id::JETSAM));
                } else if s.prior.contains(id::JETSAM) {
                    s.live.insert(id::JETSAM.to_owned());
                }
            }
            Ok(())
        })?;
        let build_version = self
            .plan
            .as_ref()
            .and_then(|p| p.parameters.get("BuildVersion"))
            .filter(|b| !b.is_empty())
            .cloned();
        self.isolate(&[id::BUILD], |s| {
            let paths = ["System/Library/CoreServices/SystemVersion.plist", "System/Cryptexes/OS/System/Library/CoreServices/SystemVersion.plist"];
            if let Some(build) = build_version.as_ref().filter(|_| s.on(id::BUILD)) {
                let mut applied = false;
                let safe = s.backup_safe(&[id::BUILD]);
                for path in paths {
                    if s.cx.system.is_regular(path)? {
                        s.patch_copy(&s.cx.system, path, "patch-build-version", &[build.clone()], safe)?;
                        applied = true;
                    }
                }
                if applied {
                    s.live.insert(id::BUILD.to_owned());
                }
            } else if !s.on(id::BUILD) {
                let mut reverted = false;
                for path in paths {
                    reverted |= s.revert_copy(&s.cx.system, path)?;
                }
                if !reverted && s.prior.contains(id::BUILD) {
                    s.live.insert(id::BUILD.to_owned());
                }
            }
            Ok(())
        })?;

        self.macho(MachoSpec {
            verbs: &["patch-seputil"],
            code_identifier: Some("com.apple.seputil"),
            ..MachoSpec::new("system-seputil-boot-gigalocker_uuid", "usr/libexec/seputil")
        })?;
        self.macho(MachoSpec {
            verbs: &["patch-settings-root-rows"],
            code_identifier: Some("com.apple.Preferences"),
            preserve_entitlements: true,
            ..MachoSpec::new(id::SETTINGS_ROOT_ROWS, "Applications/Preferences.app/Preferences")
        })?;
        if v27 {
            self.macho(MachoSpec {
                verbs: &["patch-diskimagesiod"],
                preserve_entitlements: true,
                ..MachoSpec::new("system-diskimagesiod-cfw-is_mount_complete", "usr/libexec/diskimagesiod")
            })?;
        }
        if self.on("system-gigalocker-boot-rename") {
            self.isolate(&["system-gigalocker-boot-rename"], |s| {
                s.rename_gigalocker()?;
                s.live.insert("system-gigalocker-boot-rename".to_owned());
                Ok(())
            })?;
        }
        let mut gpu_reinstalled = false;
        if self.on("system-extensions-boot-gpu_bundle") {
            let mut reinstalled = false;
            self.isolate(&["system-extensions-boot-gpu_bundle"], |s| {
                s.install_gpu_bundle()?;
                reinstalled = true;
                s.live.insert("system-extensions-boot-gpu_bundle".to_owned());
                Ok(())
            })?;
            gpu_reinstalled = reinstalled;
        }
        let gpu_driver = format!("{GPU}/AppleParavirtGPUMetalIOGPUFamily");
        self.isolate(&[id::GPU_INDEX], |s| {
            if s.on(id::GPU_INDEX) {
                s.patch_macho(&MachoSpec {
                    verbs: &["patch-paravirt-gpu-index-offset"],
                    code_identifier: Some("com.apple.driver.AppleParavirtGPUMetalIOGPUFamily"),
                    ..MachoSpec::new(id::GPU_INDEX, &gpu_driver)
                })?;
                s.seal_guest_bundle(GPU)?;
                s.live.insert(id::GPU_INDEX.to_owned());
            } else if s.revert_macho(&gpu_driver, Some(GPU))? {
                s.log(format!("  [+] {}: restored {gpu_driver} from backup", id::GPU_INDEX));
            } else if s.prior.contains(id::GPU_INDEX) && !gpu_reinstalled {
                s.live.insert(id::GPU_INDEX.to_owned());
            }
            Ok(())
        })?;
        self.macho(MachoSpec {
            verbs: &["patch-launchd-cache-loader"],
            code_identifier: Some("com.apple.launchd_cache_loader"),
            ..MachoSpec::new("system-launchd_cache_loader-boot-unsecure_cache_gate", "usr/libexec/launchd_cache_loader")
        })?;
        self.macho(MachoSpec {
            verbs: &["patch-mobileactivationd"],
            ..MachoSpec::new("system-mobileactivationd-boot-should_hactivate", "usr/libexec/mobileactivationd")
        })?;
        self.isolate(&["system-watchdogd-exp-hv_vmm_cache"], |s| {
            if s.on("system-watchdogd-exp-hv_vmm_cache") {
                s.patch_watchdog()?;
                s.live.insert("system-watchdogd-exp-hv_vmm_cache".to_owned());
            } else if s.revert_macho("usr/libexec/watchdogd", None)? {
                s.log("  [+] system-watchdogd-exp-hv_vmm_cache: restored watchdogd from backup");
            } else if s.prior.contains("system-watchdogd-exp-hv_vmm_cache") {
                s.live.insert("system-watchdogd-exp-hv_vmm_cache".to_owned());
            }
            Ok(())
        })?;
        if self.on(id::VIRTIO_SOUND) {
            self.isolate(&[id::VIRTIO_SOUND], |s| {
                s.install_virtio_sound()?;
                s.live.insert(id::VIRTIO_SOUND.to_owned());
                Ok(())
            })?;
        } else {
            self.isolate(&[id::VIRTIO_SOUND], |s| {
                if s.revert_virtio_sound()? {
                    s.log(format!("  [+] {}: removed the virtio sound driver", id::VIRTIO_SOUND));
                }
                Ok(())
            })?;
        }

        // The VirtualAudio HAL plugin: the sub-patches ride one staged copy.
        let audio: [(&str, &str); 5] = [
            (id::VA_ROUTE, "patch-virtualaudio"),
            (id::VA_MUTE, "patch-virtualaudio-mute"),
            (id::VA_SP, "patch-virtualaudio-sp-gate"),
            (id::VA_VOLUME, "patch-virtualaudio-volume-gate"),
            (id::VA_DEVICE, "patch-virtualaudio-device-state-null-gate"),
        ];
        let bundle = "Library/Audio/Plug-Ins/HAL/VirtualAudio.plugin";
        let binary = format!("{bundle}/VirtualAudio");
        let audio_ids: Vec<&str> = audio.iter().map(|a| a.0).collect();
        let audio_on: Vec<(&str, &str)> = audio.iter().copied().filter(|a| self.on(a.0)).collect();
        self.isolate(&audio_ids, |s| {
            if !audio_on.is_empty() {
                if !s.cx.system.is_regular(&binary)? {
                    s.log("  [·] VirtualAudio: not on this VM, left out");
                    return Ok(());
                }
                let verbs: Vec<&str> = audio_on.iter().map(|a| a.1).collect();
                s.patch_macho(&MachoSpec {
                    verbs: &verbs,
                    preserve_entitlements: true,
                    ..MachoSpec::new("system-virtualaudio", &binary)
                })?;
                s.seal_guest_bundle(bundle)?;
                for (i, _) in &audio_on {
                    s.live.insert((*i).to_owned());
                }
            } else if s.revert_macho(&binary, Some(bundle))? {
                s.log("  [+] VirtualAudio restored from backup (all speaker/mute patches off)");
            } else {
                for (i, _) in audio {
                    if s.prior.contains(i) {
                        s.live.insert(i.to_owned());
                    }
                }
            }
            Ok(())
        })?;

        let graph: [(&str, &str); 3] = [
            (id::VA_GRAPH, "patch-virtualaudio-graph-configurations"),
            (id::VA_RAW, "patch-virtualaudio-speaker-raw"),
            (id::VA_MIC, "patch-virtualaudio-microphone-chains"),
        ];
        let graph_on: Vec<(&str, &str)> = graph.iter().copied().filter(|g| self.on(g.0)).collect();
        let graph_safe = self.backup_safe(&[graph[0].0, graph[1].0, graph[2].0]);
        let graph_ids: Vec<&str> = graph.iter().map(|g| g.0).collect();
        self.isolate(&graph_ids, |s| {
            let had_backup = s.restore_virtual_audio_tunings()?;
            if !graph_on.is_empty() {
                for (i, verb) in &graph_on {
                    if s.patch_virtual_audio_graph(verb, graph_safe)? {
                        s.live.insert((*i).to_owned());
                    }
                }
            } else if !had_backup {
                for (i, _) in graph {
                    if s.prior.contains(i) {
                        s.live.insert(i.to_owned());
                    }
                }
            }
            Ok(())
        })?;

        let jetsam = "system-launchd-boot-jetsam_panic_guard_bypass";
        self.isolate(&[jetsam, id::X1_LAUNCHD], |s| {
            let jetsam_on = s.on(jetsam);
            let x1_on = s.on(id::X1_LAUNCHD);
            if jetsam_on || x1_on {
                let mut verbs: Vec<&str> = Vec::new();
                if jetsam_on {
                    verbs.push("patch-launchd-jetsam");
                }
                if x1_on {
                    verbs.push("patch-arm64e-x1");
                }
                s.patch_macho(&MachoSpec {
                    verbs: &verbs,
                    preserve_entitlements: true,
                    inject: Some("/vh"),
                    ..MachoSpec::new("launchd", "sbin/launchd")
                })?;
                if jetsam_on {
                    s.live.insert(jetsam.to_owned());
                }
                if x1_on {
                    s.live.insert(id::X1_LAUNCHD.to_owned());
                }
            } else if s.revert_macho("sbin/launchd", None)? {
                s.log("  [+] launchd patches: restored sbin/launchd from backup");
            } else {
                for i in [jetsam, id::X1_LAUNCHD] {
                    if s.prior.contains(i) {
                        s.live.insert(i.to_owned());
                    }
                }
            }
            Ok(())
        })?;
        self.require_x1()?;
        self.isolate(&[], |s| s.restore_misfix_targets())?;

        self.isolate(&["system-debugserver-cfw-install"], |s| {
            if s.on("system-debugserver-cfw-install") {
                let safe = s.backup_safe(&["system-debugserver-cfw-install"]);
                if s.patch_debugserver(safe)? {
                    s.live.insert("system-debugserver-cfw-install".to_owned());
                }
            } else if s.revert_macho("usr/libexec/debugserver", None)? {
                s.log("  [+] system-debugserver-cfw-install: restored debugserver from backup");
            } else if s.prior.contains("system-debugserver-cfw-install") {
                s.live.insert("system-debugserver-cfw-install".to_owned());
            }
            Ok(())
        })?;
        if v27 {
            self.isolate(&["system-campo-cfw-entitlements"], |s| {
                if s.on("system-campo-cfw-entitlements") {
                    let safe = s.backup_safe(&["system-campo-cfw-entitlements"]);
                    if s.patch_campo(safe)? {
                        s.live.insert("system-campo-cfw-entitlements".to_owned());
                    }
                } else if s.revert_macho("Applications/Campo.app/Campo", None)? {
                    s.log("  [+] system-campo-cfw-entitlements: restored Campo from backup");
                } else if s.prior.contains("system-campo-cfw-entitlements") {
                    s.live.insert("system-campo-cfw-entitlements".to_owned());
                }
                Ok(())
            })?;
        }

        self.isolate(&[id::PERSONA], |s| {
            let proxy = "System/Library/PrivateFrameworks/InstallCoordination.framework/Support/installcoordination_proxy";
            if s.prior.contains(id::PERSONA) && s.revert_macho(proxy, None)? {
                s.log(format!("  [+] {}: restored proxy before enabling launchd compatibility", id::PERSONA));
            }
            if v27 && s.on(id::PERSONA) {
                s.cx.system.write_data(id::PERSONA_MARKER, b"1\n", 0o644, OWNER)?;
                s.live.insert(id::PERSONA.to_owned());
            } else {
                s.cx.system.remove_item(id::PERSONA_MARKER)?;
            }
            Ok(())
        })?;
        self.isolate(&[id::METAL_CACHE], |s| {
            if s.on(id::METAL_CACHE) {
                s.cx.system.write_data(id::METAL_CACHE_MARKER, b"1\n", 0o644, OWNER)?;
                s.live.insert(id::METAL_CACHE.to_owned());
            } else {
                s.cx.system.remove_item(id::METAL_CACHE_MARKER)?;
            }
            Ok(())
        })?;

        self.patch_preboot(build_version.as_deref())?;
        self.install_signed_daemon_copy()?;
        self.log("[+] guest install complete");
        Ok(())
    }

    /// The host copy of the installed vphoned, for launch-time auto-update.
    fn install_signed_daemon_copy(&self) -> Result<()> {
        let staged = self.staged("vphoned");
        if !staged.exists() {
            return Ok(());
        }
        let out = self.cx.vm.join(".vphoned.signed");
        let tmp = self.cx.vm.join(".vphoned.signed.tmp");
        fs::copy(&staged, &tmp).map_err(|e| Error::path(&tmp, e))?;
        fs::rename(&tmp, &out).map_err(|e| Error::path(&out, e))
    }

    fn board_device_tree(&self) -> Result<Option<PathBuf>> {
        let originals = self.cx.vm.join("FirmwareOriginals");
        if !originals.is_dir() {
            return Ok(None);
        }
        let mut found = Vec::new();
        for tree in fs::read_dir(&originals).map_err(|e| Error::path(&originals, e))? {
            let tree = tree.map_err(|e| Error::path(&originals, e))?.path();
            let flash = tree.join("Firmware/all_flash");
            if !flash.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&flash).map_err(|e| Error::path(&flash, e))? {
                let entry = entry.map_err(|e| Error::path(&flash, e))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("DeviceTree.") && name.ends_with(".im4p") && !name.starts_with("DeviceTree.vphone600") {
                    found.push((name, entry.path()));
                }
            }
        }
        if found.len() > 1 {
            let own = self.device.board_tree_name();
            if found.iter().filter(|f| f.0 == own).count() == 1 {
                found.retain(|f| f.0 == own);
            }
        }
        match found.len() {
            0 => Ok(None),
            1 => {
                let (name, path) = found.pop().unwrap();
                let staged = self.staged(&name);
                fs::copy(&path, &staged).map_err(|e| Error::path(&staged, e))?;
                self.log(format!("[*] Board device tree: {}", path.display()));
                Ok(Some(staged))
            }
            n => bail!("Found {n} board device trees in FirmwareOriginals. Keep one."),
        }
    }

    fn patch_preboot(&mut self, build_version: Option<&str>) -> Result<()> {
        let board = self.board_device_tree()?;
        if board.is_none() {
            self.log("[!] no board device tree in FirmwareOriginals; the board audio / product repairs are skipped");
        }
        let device = self.device;
        let identity = id::DT_IDENTITY;
        let mut rewrite_identity = false;
        if device.presents_board() {
            self.log(format!("  [·] {identity}: skipped, the device tree already presents {}", device.product));
        } else {
            rewrite_identity = self.dt_on(identity);
            if !rewrite_identity {
                self.log(format!("  [·] {identity}: off in the current selection"));
            }
        }
        let path_arg = |p: &PathBuf| p.to_string_lossy().into_owned();
        let mut group: Vec<(&str, &str, Vec<String>)> = Vec::new();
        if let Some(board) = &board {
            if device.presents_board() {
                group.push((id::DT_BOARD_AUDIO, "patch-dt-board-audio", vec![path_arg(board)]));
            }
            if device.family == Family::IPhone {
                group.push((id::DT_IPHONE, "patch-dt-iphone-product", vec![path_arg(board)]));
            }
        }
        if device.foldable() {
            if board.is_none() && (self.dt_on(id::DT_FOLD_ID) || self.dt_on(id::DT_FOLD_DISP)) {
                bail!("Foldable product identity and panel nodes need the board's own device tree.");
            }
            group.push((id::DT_FOLD_SUB, "patch-dt-foldable-vm-subtype", vec![]));
            group.push((id::DT_FOLD_ID, "patch-dt-foldable-product-id", board.iter().map(path_arg).collect()));
            group.push((id::DT_FOLD_DISP, "patch-dt-foldable-displays", board.iter().map(path_arg).collect()));
        }
        group.push((id::DT_HAPTICS, "patch-dt-haptics", vec![]));
        if !device.foldable() {
            group.push((id::DT_DISPLAY_NODES, "patch-dt-display-nodes", vec![]));
        }
        group.push((id::DT_MIC, "patch-dt-microphone-array", vec![]));

        let preboot = &self.cx.preboot;
        let mut candidates = Vec::new();
        for (name, attrs) in preboot.list("")? {
            let path = format!("{name}/usr/standalone/firmware/devicetree.img4");
            if attrs.is_dir() && preboot.is_regular(&path)? {
                candidates.push(path);
            }
        }
        if candidates.len() != 1 {
            bail!("Expected one device tree in the Preboot volume but found {}. Restore the VM, then install CFW again.", candidates.len());
        }
        let device_tree = candidates.pop().unwrap();
        let meta = preboot.status(&device_tree)?.unwrap();
        let current = preboot.read_data(&device_tree)?;
        let backup = format!("{device_tree}.bak");
        let had_backup = preboot.exists(&backup)?;
        let backup_safe = had_backup || group.iter().all(|g| !self.prior.contains(g.0));
        let folder = self.staged("dt");
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).map_err(|e| Error::path(&folder, e))?;
        let staged = folder.join("devicetree.img4");
        let base = if had_backup { preboot.read_data(&backup)? } else { current.clone() };
        fs::write(&staged, &base).map_err(|e| Error::path(&staged, e))?;
        let staged_arg = staged.to_string_lossy().into_owned();
        if rewrite_identity {
            self.patch("patch-post-restore-dt", std::slice::from_ref(&staged_arg))?;
            self.live.insert(identity.to_owned());
        }
        let post_identity = fs::read(&staged).map_err(|e| Error::path(&staged, e))?;
        for (i, verb, args) in &group {
            if self.dt_on(i) {
                let mut all = vec![staged_arg.clone()];
                all.extend_from_slice(args);
                self.patch(verb, &all)?;
                self.live.insert((*i).to_owned());
            }
        }
        if group.iter().all(|g| !self.dt_on(g.0)) && !backup_safe {
            for (i, _, _) in &group {
                if self.prior.contains(*i) {
                    self.live.insert((*i).to_owned());
                }
            }
        }
        let final_bytes = fs::read(&staged).map_err(|e| Error::path(&staged, e))?;
        let preboot = &self.cx.preboot;
        if final_bytes != current {
            preboot.write_data(&device_tree, &final_bytes, meta.perm(), (meta.uid, meta.gid))?;
        }
        if backup_safe && (!preboot.exists(&backup)? || preboot.read_data(&backup)? != post_identity) {
            preboot.write_data(&backup, &post_identity, meta.perm(), (meta.uid, meta.gid))?;
        }
        let _ = fs::remove_dir_all(&folder);

        // The build-version spoof on the Preboot SystemVersion.
        let version = "Cryptexes/OS/System/Library/CoreServices/SystemVersion.plist";
        if self.cx.preboot.is_regular(version)? {
            let on = self.dt_on(id::BUILD);
            if let Some(build) = build_version.filter(|b| !b.is_empty() && on) {
                let safe = self.backup_safe(&[id::BUILD]);
                self.patch_copy(&self.cx.preboot, version, "patch-build-version", &[build.to_owned()], safe)?;
            } else if !on {
                self.revert_copy(&self.cx.preboot, version)?;
            }
        }
        Ok(())
    }

    fn not_applicable(&self) -> BTreeSet<String> {
        let device = self.device;
        let mut set: BTreeSet<String> = BTreeSet::new();
        if !device.presents_board() {
            set.insert(id::DT_BOARD_AUDIO.into());
        }
        if device.family != Family::IPhone {
            set.insert(id::DT_IPHONE.into());
        }
        set.insert(id::HOMEPOD_AUDIO.into());
        if device.foldable() {
            set.insert(id::DT_DISPLAY_NODES.into());
        } else {
            set.extend([id::DT_FOLD_SUB, id::DT_FOLD_ID, id::DT_FOLD_DISP, id::HINGE, id::FOLD_GUARD].map(String::from));
        }
        set.insert(id::TV_TPRO.into());
        if self.plan.as_ref().and_then(|p| p.parameters.get("BuildVersion")).is_none_or(|b| b.is_empty()) {
            set.insert(id::BUILD.into());
        }
        &set - &self.live
    }
}

fn snapshot_files(root: &Path, relative: &str) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let dir = if relative.is_empty() { root.to_path_buf() } else { root.join(relative) };
    for entry in fs::read_dir(&dir).map_err(|e| Error::path(&dir, e))? {
        let entry = entry.map_err(|e| Error::path(&dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if relative.is_empty() { name } else { format!("{relative}/{name}") };
        let kind = entry.file_type().map_err(|e| Error::path(&dir, e))?;
        if kind.is_dir() {
            out.extend(snapshot_files(root, &rel)?);
        } else if kind.is_file() {
            let data = fs::read(entry.path()).map_err(|e| Error::path(&entry.path(), e))?;
            out.insert(rel, data);
        }
    }
    Ok(out)
}

fn read_metas(path: &Path) -> Result<BTreeMap<String, Meta>> {
    let data = fs::read(path).map_err(|e| Error::path(path, e))?;
    let raw: BTreeMap<String, (u32, u32, u32, u64)> =
        serde_json::from_slice(&data).map_err(|e| Error::msg(format!("{}: {e}", path.display())))?;
    Ok(raw
        .into_iter()
        .map(|(k, (mode, uid, gid, mtime))| (k, Meta { mode, uid, gid, mtime }))
        .collect())
}

fn write_metas(path: &Path, metas: &BTreeMap<String, Meta>) -> Result<()> {
    let raw: BTreeMap<&String, (u32, u32, u32, u64)> =
        metas.iter().map(|(k, m)| (k, (m.mode, m.uid, m.gid, m.mtime))).collect();
    fs::write(path, serde_json::to_vec(&raw).unwrap()).map_err(|e| Error::path(path, e))
}

/// Install the `cfw` markers' reset for a fresh install.
pub fn forget_progress(records: &mut StepRecord) -> Result<()> {
    records.forget_prefix("cfw.")
}
