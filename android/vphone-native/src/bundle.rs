//! A vphone VM bundle: what run-vphone.sh reads from the folder.

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_RES: (u32, u32) = (1290, 2796);
const DEFAULT_ROM: &str = "AVPBooter.vresearch1.bin";
const DEFAULT_SEPROM: &str = "AVPSEPBooter.vresearch1.bin";

#[derive(Clone, Debug, PartialEq)]
pub struct Bundle {
    pub name: String,
    pub dir: PathBuf,
    pub ecid: String,
    pub serial: Option<String>,
    pub rom: String,
    pub seprom: String,
    pub res: (u32, u32),
    pub has_sep_storage: bool,
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let joined: String = text.chars().filter(|c| *c != ' ' && *c != '\n').collect();
    (!joined.is_empty()).then_some(joined)
}

/// config.plist's `romImages`, or Apple's names when the machine predates it.
fn rom_names(dir: &Path) -> Result<(String, String), String> {
    let path = dir.join("config.plist");
    if !path.exists() {
        return Ok((DEFAULT_ROM.into(), DEFAULT_SEPROM.into()));
    }
    let plist =
        plist::Value::from_file(&path).map_err(|e| format!("config.plist cannot be read: {e}"))?;
    let images = plist
        .as_dictionary()
        .and_then(|d| d.get("romImages"))
        .and_then(|v| v.as_dictionary());
    let name = |key: &str, default: &str| {
        images
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    };
    Ok((
        name("avpBooter", DEFAULT_ROM),
        name("avpSEPBooter", DEFAULT_SEPROM),
    ))
}

pub fn parse_res(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.trim().split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

impl Bundle {
    pub fn open(dir: &Path) -> Result<Bundle, Vec<String>> {
        let mut problems = Vec::new();
        let name = dir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let ecid = read_trimmed(&dir.join("ecid")).unwrap_or_else(|| {
            problems.push("the \"ecid\" file is missing or empty".to_string());
            String::new()
        });
        let (rom, seprom) = rom_names(dir).unwrap_or_else(|e| {
            problems.push(e);
            (DEFAULT_ROM.into(), DEFAULT_SEPROM.into())
        });
        for required in ["Disk.img", "nvram.bin", rom.as_str(), seprom.as_str()] {
            if !dir.join(required).is_file() {
                problems.push(format!("{required} is missing"));
            }
        }
        let res = match read_trimmed(&dir.join("gfx-res")) {
            None => DEFAULT_RES,
            Some(text) => parse_res(&text).unwrap_or_else(|| {
                problems.push(format!(
                    "the \"gfx-res\" file holds {text:?}, not WIDTHxHEIGHT"
                ));
                DEFAULT_RES
            }),
        };
        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(Bundle {
            name,
            dir: dir.to_path_buf(),
            ecid,
            serial: read_trimmed(&dir.join("serial")),
            rom,
            seprom,
            res,
            has_sep_storage: dir.join("SEPStorage").is_file(),
        })
    }

    pub fn path(&self, file: &str) -> String {
        self.dir.join(file).to_string_lossy().into_owned()
    }
}

/// One line of the VM list.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    pub name: String,
    pub path: String,
    pub problems: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub disk_bytes: u64,
    pub config: crate::config::VmConfig,
}

/// Every folder in `vms`, the unusable ones with what is wrong with them. Unfinished imports
/// (`.import-*`) are not VMs.
pub fn list(vms: &Path) -> Vec<Listed> {
    list_in(&[vms.to_path_buf()])
}

/// Every VM in these libraries; a name in the first (internal storage) hides one in a later.
/// A folder still being created has `.create-spec.json` and no `.create-state` `booted` line:
/// the wizard shows those, not this list.
pub fn list_in(roots: &[PathBuf]) -> Vec<Listed> {
    use std::collections::BTreeMap;
    let mut merged = BTreeMap::new();
    for root in roots {
        for vm in scan(root) {
            merged.entry(vm.name.clone()).or_insert(vm);
        }
    }
    merged.into_values().collect()
}

fn in_progress(dir: &Path) -> bool {
    let temporary = dir
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("gpu-recovery-"));
    temporary || dir.join(".create-spec.json").exists()
        && !fs::read_to_string(dir.join(".create-state")).is_ok_and(|t| t.lines().any(|l| l == "booted"))
}

fn scan(vms: &Path) -> Vec<Listed> {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = fs::read_dir(vms) else {
        return Vec::new();
    };
    let mut out: Vec<Listed> = entries
        .flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
        .filter(|e| !in_progress(&e.path()))
        .map(|e| {
            let dir = e.path();
            let (problems, res) = match Bundle::open(&dir) {
                Ok(b) => (Vec::new(), b.res),
                Err(p) => (p, DEFAULT_RES),
            };
            Listed {
                name: e.file_name().to_string_lossy().into_owned(),
                path: dir.to_string_lossy().into_owned(),
                problems,
                width: res.0,
                height: res.1,
                disk_bytes: fs::metadata(dir.join("Disk.img")).map_or(0, |m| m.blocks() * 512),
                config: crate::config::VmConfig::load(&dir),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn fixture(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../build/test-fixtures")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for f in ["Disk.img", "nvram.bin", DEFAULT_ROM, DEFAULT_SEPROM] {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::write(dir.join("ecid"), "dead beef\n").unwrap();
        dir
    }

    #[test]
    fn reads_a_minimal_bundle() {
        let b = Bundle::open(&fixture("minimal")).unwrap();
        assert_eq!(b.ecid, "deadbeef");
        assert_eq!(b.res, DEFAULT_RES);
        assert_eq!((b.serial, b.has_sep_storage), (None, false));
    }

    #[test]
    fn takes_rom_names_from_the_plist() {
        let dir = fixture("plist");
        let mut images = plist::Dictionary::new();
        images.insert("avpBooter".into(), "OpenAVB.bin".to_string().into());
        images.insert("avpSEPBooter".into(), "OpenSEP.bin".to_string().into());
        let mut root = plist::Dictionary::new();
        root.insert("romImages".into(), plist::Value::Dictionary(images));
        plist::Value::Dictionary(root)
            .to_file_xml(dir.join("config.plist"))
            .unwrap();
        assert!(Bundle::open(&dir)
            .unwrap_err()
            .iter()
            .any(|p| p.contains("OpenAVB.bin")));
        fs::write(dir.join("OpenAVB.bin"), b"x").unwrap();
        fs::write(dir.join("OpenSEP.bin"), b"x").unwrap();
        fs::write(dir.join("gfx-res"), "448x932\n").unwrap();
        let b = Bundle::open(&dir).unwrap();
        assert_eq!(
            (b.rom.as_str(), b.seprom.as_str(), b.res),
            ("OpenAVB.bin", "OpenSEP.bin", (448, 932))
        );
    }

    #[test]
    fn reports_what_is_missing() {
        let dir = fixture("broken");
        fs::remove_file(dir.join("nvram.bin")).unwrap();
        fs::remove_file(dir.join("ecid")).unwrap();
        fs::write(dir.join("gfx-res"), "wide").unwrap();
        assert_eq!(Bundle::open(&dir).unwrap_err().len(), 3);
    }

    #[test]
    fn merges_libraries_and_hides_creations_in_progress() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/list2");
        let _ = fs::remove_dir_all(&root);
        let (a, b) = (root.join("internal"), root.join("external"));
        fs::create_dir_all(a.join("made")).unwrap();
        fs::create_dir_all(a.join("half")).unwrap();
        fs::create_dir_all(b.join("made")).unwrap();
        fs::create_dir_all(b.join("imported")).unwrap();
        fs::write(a.join("half/.create-spec.json"), "{}").unwrap();
        fs::write(a.join("made/.create-spec.json"), "{}").unwrap();
        fs::write(a.join("made/.create-state"), "sanity\nbooted\n").unwrap();
        let l = list_in(&[a.clone(), b]);
        let names: Vec<_> = l.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["imported", "made"]);
        assert!(l[1].path.contains("internal"));
    }

    #[test]
    fn lists_vms_and_skips_unfinished_imports() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/list");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".import-x")).unwrap();
        fs::create_dir_all(root.join("incomplete")).unwrap();
        let l = list(&root);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].name, "incomplete");
        assert!(!l[0].problems.is_empty());
    }
}
