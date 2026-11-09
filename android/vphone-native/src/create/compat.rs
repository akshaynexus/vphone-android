//! Which devices and iOS versions can be made, and where their firmware comes from.
//!
//! `setup/data/compatibility.json` (the desktop pipeline's own list) and a snapshot of
//! `vphone-cli fw catalog --json` are compiled into the library, so the wizard works offline and
//! the URLs resolved match what `setup/lib/get-firmware.py` resolves on a PC.

use serde::{Deserialize, Deserializer, Serialize};

const COMPAT_JSON: &str = include_str!("../../../../setup/data/compatibility.json");
const CATALOG_JSON: &str = include_str!("data/firmware-catalog.json");

/// The list writes `null` for fields a pairing lacks.
fn nullable<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// The pairing a new VM starts from: verified end to end on the desktop flow, at 1x.
pub const DEFAULT_DEVICE: &str = "iPhone17,3";
pub const DEFAULT_IOS: &str = "26.6.2";

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Pairing {
    pub device: String,
    pub os_version: String,
    #[serde(deserialize_with = "nullable")]
    pub build: String,
    #[serde(deserialize_with = "nullable")]
    pub ipsw: String,
    #[serde(rename = "cloudOS", deserialize_with = "nullable")]
    pub cloud_os: String,
    #[serde(deserialize_with = "nullable")]
    pub preset: String,
    pub linux_tested: bool,
    pub mac_tested: bool,
    pub recommended: bool,
    pub requires_mac_builder: bool,
    pub unsupported: bool,
    #[serde(deserialize_with = "nullable")]
    pub note: String,
}

impl Default for Pairing {
    fn default() -> Self {
        Pairing {
            device: String::new(),
            os_version: String::new(),
            build: String::new(),
            ipsw: String::new(),
            cloud_os: String::new(),
            preset: String::new(),
            linux_tested: false,
            mac_tested: false,
            recommended: false,
            requires_mac_builder: false,
            unsupported: false,
            note: String::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub note: String,
    pub unsupported: bool,
    pub screen: Vec<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Compat {
    devices: Vec<Device>,
    pairings: Vec<Pairing>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
struct CatalogItem {
    name: String,
    url: String,
    size: Option<u64>,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
struct CatalogPairing {
    ios: CatalogItem,
    #[serde(rename = "recommendedCloudOS")]
    recommended_cloud_os: CatalogItem,
    #[serde(rename = "matchingCloudOS")]
    matching_cloud_os: CatalogItem,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CatalogDevice {
    product_type: String,
    pairings: Vec<CatalogPairing>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Catalog {
    devices: Vec<CatalogDevice>,
}

/// The compiled-in list, for the steps that read it from a file.
pub fn json_text() -> &'static str {
    COMPAT_JSON
}

fn compat() -> Compat {
    serde_json::from_str(COMPAT_JSON).expect("compatibility.json is compiled in")
}

fn catalog() -> Catalog {
    serde_json::from_str(CATALOG_JSON).expect("firmware-catalog.json is compiled in")
}

/// What the wizard offers. Pairings the desktop flow cannot make (a macOS kmutil is needed, or
/// the list marks it unsupported) are left out, as `create-vm.sh` refuses them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardChoices {
    pub default_device: String,
    pub default_ios: String,
    pub devices: Vec<WizardDevice>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardDevice {
    pub id: String,
    pub name: String,
    pub note: String,
    pub pairings: Vec<WizardPairing>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardPairing {
    pub ios: String,
    pub build: String,
    pub cloud_os: String,
    /// "tested": verified end to end on Linux. "undertest": macOS-tested, not yet on Linux.
    pub status: &'static str,
    pub recommended: bool,
    pub note: String,
    /// iOS 27 does not go below 2x.
    pub min_scale: u32,
}

pub fn min_scale(ios: &str) -> u32 {
    if ios.starts_with("27.") {
        2
    } else {
        1
    }
}

fn selectable(p: &Pairing) -> bool {
    !p.unsupported && !p.requires_mac_builder
}

fn status_of(p: &Pairing) -> &'static str {
    if p.linux_tested {
        "tested"
    } else {
        "undertest"
    }
}

pub fn wizard_choices() -> WizardChoices {
    let c = compat();
    let mut devices = Vec::new();
    for d in c.devices.iter().filter(|d| !d.unsupported) {
        let pairings: Vec<WizardPairing> = c
            .pairings
            .iter()
            .filter(|p| p.device == d.id && selectable(p))
            .map(|p| WizardPairing {
                ios: p.os_version.clone(),
                build: p.build.clone(),
                cloud_os: p.cloud_os.clone(),
                status: status_of(p),
                recommended: p.recommended,
                note: p.note.clone(),
                min_scale: min_scale(&p.os_version),
            })
            .collect();
        if !pairings.is_empty() {
            devices.push(WizardDevice {
                id: d.id.clone(),
                name: d.name.clone(),
                note: d.note.clone(),
                pairings,
            });
        }
    }
    WizardChoices {
        default_device: DEFAULT_DEVICE.into(),
        default_ios: DEFAULT_IOS.into(),
        devices,
    }
}

pub fn find_pairing(device: &str, ios: &str) -> Result<Pairing, String> {
    let c = compat();
    let p = c
        .pairings
        .into_iter()
        .find(|p| p.device == device && p.os_version == ios)
        .ok_or_else(|| format!("no tested pairing for {device} + iOS {ios}"))?;
    if p.requires_mac_builder {
        return Err(format!(
            "{device} {ios} needs a macOS kmutil to link its kernelcache; it cannot be made on a phone"
        ));
    }
    if p.unsupported {
        return Err(format!("{device} {ios} is marked unsupported"));
    }
    Ok(p)
}

/// One firmware image: the name to cache it as, its URL, and its size where the catalog knows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub file: String,
    pub url: String,
    pub size: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Firmware {
    pub ios: Image,
    pub cloud: Image,
}

/// Restores the catalog does not list, as in get-firmware.py.
const KNOWN_IOS: &[(&str, &str, &str, &str, u64)] = &[(
    "iPhone19,4",
    "27.1",
    "iOS 27.1",
    "https://updates.cdn-apple.com/2026FallFCS/3f41a447-d8a6-48ae-83ae-056a64f57fd4/iPhone19,4_27.1_24A94425_Restore.ipsw",
    12_724_731_538,
)];

/// The `.ipsw` name a catalog entry is cached under, as get-firmware.py names it.
fn cache_name(name: &str, url: &str) -> String {
    let base = url.rsplit('/').next().unwrap_or_default();
    if ["iPhone", "iPad", "Watch", "AppleTV", "Audio"]
        .iter()
        .any(|p| base.starts_with(p))
    {
        return base.to_string();
    }
    // cloudOS URLs are opaque CDN hashes: a build in parentheses, or "cloudOS 26.4".
    if let (Some(l), Some(r)) = (name.find('('), name.find(')')) {
        if r > l + 1 {
            return format!("cloudOS-{}.ipsw", &name[l + 1..r]);
        }
    }
    let plain = name.trim().strip_prefix("cloudOS ").unwrap_or(name.trim());
    format!("cloudOS-{}.ipsw", plain.replace(' ', "-"))
}

fn release_version(name: &str) -> Option<&str> {
    for p in ["iOS", "iPadOS", "watchOS", "tvOS", "HomePod Software"] {
        if let Some(rest) = name.strip_prefix(p) {
            let v = rest.trim_start();
            if !v.is_empty() && !v.contains(' ') {
                return Some(v);
            }
        }
    }
    None
}

fn wants_matching(label: &str, matching: &CatalogItem) -> bool {
    let build = label.rsplit('-').next().unwrap_or("");
    !build.is_empty() && !matching.url.is_empty() && matching.name.contains(build)
}

/// Port of get-firmware.py for one pairing. A device's restore is looked up by the exact IPSW the
/// compatibility list names when it can be, else by its marketing version.
pub fn resolve(p: &Pairing) -> Result<Firmware, String> {
    let cat = catalog();
    let entries: &[CatalogPairing] = cat
        .devices
        .iter()
        .find(|d| d.product_type == p.device)
        .map(|d| d.pairings.as_slice())
        .unwrap_or_default();

    let mut ios: Option<(String, String, Option<u64>)> = None;
    let mut cloud: Option<&CatalogItem> = None;
    let by_file = entries
        .iter()
        .find(|e| !p.ipsw.is_empty() && e.ios.url.rsplit('/').next() == Some(p.ipsw.as_str()));
    let by_version = entries
        .iter()
        .find(|e| release_version(&e.ios.name) == Some(p.os_version.as_str()));
    if let Some(e) = by_file.or(by_version) {
        ios = Some((e.ios.name.clone(), e.ios.url.clone(), e.ios.size));
        cloud = Some(if wants_matching(&p.cloud_os, &e.matching_cloud_os) {
            &e.matching_cloud_os
        } else {
            &e.recommended_cloud_os
        });
    }

    if ios.is_none() {
        if let Some(k) = KNOWN_IOS
            .iter()
            .find(|k| k.0 == p.device && k.1 == p.os_version)
        {
            ios = Some((k.2.to_string(), k.3.to_string(), Some(k.4)));
            let major = p.os_version.split('.').next().unwrap_or("");
            for e in entries.iter().filter(|e| {
                e.ios
                    .name
                    .strip_prefix("iOS ")
                    .and_then(|v| v.split(['.', ' ']).next())
                    == Some(major)
            }) {
                cloud = Some(if wants_matching(&p.cloud_os, &e.matching_cloud_os) {
                    &e.matching_cloud_os
                } else {
                    &e.recommended_cloud_os
                });
            }
        }
    }

    let (name, url, size) = ios.ok_or_else(|| {
        format!(
            "the firmware catalog has no restore for {} {}",
            p.device, p.os_version
        )
    })?;
    let cloud = cloud
        .filter(|c| !c.url.is_empty())
        .ok_or_else(|| format!("no cloudOS image is known for {} {}", p.device, p.os_version))?;
    Ok(Firmware {
        ios: Image {
            file: cache_name(&name, &url),
            url,
            size,
        },
        cloud: Image {
            file: cache_name(&cloud.name, &cloud.url),
            url: cloud.url.clone(),
            size: cloud.size,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_pairing_resolves_to_apples_urls() {
        let p = find_pairing(DEFAULT_DEVICE, DEFAULT_IOS).unwrap();
        assert!(p.linux_tested && p.recommended);
        let f = resolve(&p).unwrap();
        assert_eq!(f.ios.file, "iPhone17,3_26.6.2_23G90_Restore.ipsw");
        assert!(f.ios.url.starts_with("https://updates.cdn-apple.com/"));
        assert_eq!(f.cloud.file, "cloudOS-26.4.ipsw");
        assert!(f.cloud.url.contains("c0ecdb4b310cf5239ab2b248dd3098eec297dc5aa"));
    }

    #[test]
    fn every_selectable_pairing_has_urls() {
        for d in wizard_choices().devices {
            for p in d.pairings {
                let pairing = find_pairing(&d.id, &p.ios).unwrap();
                let f = resolve(&pairing).unwrap_or_else(|e| panic!("{}: {e}", d.id));
                assert!(f.ios.url.starts_with("https://"), "{f:?}");
                assert!(f.cloud.url.starts_with("https://"), "{f:?}");
            }
        }
    }

    #[test]
    fn an_exact_ipsw_name_beats_a_version_match() {
        // Compatibility lists the 27.0 RC build (24A435); the catalog also has 27.0 final (24A437).
        let p = find_pairing("iPhone17,3", "27.0").unwrap();
        assert_eq!(resolve(&p).unwrap().ios.file, "iPhone17,3_27.0_24A435_Restore.ipsw");
    }

    #[test]
    fn unmakeable_pairings_are_not_offered() {
        let w = wizard_choices();
        let d = w.devices.iter().find(|d| d.id == "iPhone17,3").unwrap();
        assert!(d.pairings.iter().all(|p| p.ios != "27.1"));
        assert!(w.devices.iter().all(|d| d.id != "iPhone11,6"));
        assert!(find_pairing("iPhone11,6", "18.7.2").is_err());
        assert!(find_pairing("iPhone17,3", "27.1").is_err());
    }

    #[test]
    fn scale_floor() {
        assert_eq!(min_scale("27.0.1"), 2);
        assert_eq!(min_scale("26.6.2"), 1);
    }
}
