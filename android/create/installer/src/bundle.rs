//! Create an empty VM bundle: tools/new-vm.sh and setup/lib/make-bundle.py.

use crate::bail;
use crate::display::{self, Native, WidthPolicy};
use crate::error::{Error, Result};
use crate::plist_py;
use crate::progress::Progress;
use plist::{Dictionary, Value};
use rand::RngCore;
use serde::Deserialize;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

const DISK_SIZE: u64 = 64 << 30;
const NVRAM_SIZE: u64 = 33_570_816; // 16 KiB aux-storage header + 32 MiB store/NVRAM
const SEP_SIZE: u64 = 512 * 1024;

/// A device's panel, from setup/data/compatibility.json (`devices[].screen`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub ppi: u32,
    pub scale: f64,
}

impl Default for Screen {
    fn default() -> Self {
        Screen {
            width: 1290,
            height: 2796,
            ppi: 460,
            scale: 3.0,
        }
    }
}

#[derive(Deserialize)]
struct Compat {
    devices: Vec<CompatDevice>,
}

#[derive(Deserialize)]
struct CompatDevice {
    id: String,
    screen: Option<(u32, u32, u32, f64)>,
}

/// `compatibility.json`'s screen for `device`, or the 1290x2796 default.
pub fn device_screen(compat_json: &Path, device: &str) -> Screen {
    let parsed = fs::read(compat_json)
        .ok()
        .and_then(|d| serde_json::from_slice::<Compat>(&d).ok());
    parsed
        .and_then(|c| c.devices.into_iter().find(|d| d.id == device))
        .and_then(|d| d.screen)
        .map(|(width, height, ppi, scale)| Screen { width, height, ppi, scale })
        .unwrap_or_default()
}

/// What a new VM is made of.
#[derive(Clone, Debug)]
pub struct BundleParams<'a> {
    pub vm: &'a Path,
    /// `guestProductType` in config.plist; `None` omits it (a temporary cloudOS VM).
    pub device: Option<&'a str>,
    pub screen: Screen,
    /// UI scale, 1, 2 or 3.
    pub scale: u32,
    pub width_policy: WidthPolicy,
    pub ecid: Option<u64>,
    pub memory_gib: u32,
    pub cpus: u32,
    pub open_avb: &'a Path,
    pub open_sep: &'a Path,
}

/// What was created.
#[derive(Clone, Debug)]
pub struct Bundle {
    pub vm: PathBuf,
    pub ecid: u64,
    pub render: (u32, u32),
}

fn random_ecid() -> u64 {
    // new-vm.sh: a 63-bit random value, kept nonzero and well inside 64 bits.
    (rand::thread_rng().next_u64() & ((1 << 63) - 1)) | (1 << 40)
}

fn random_mac() -> String {
    let mut mac = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut mac);
    mac[0] = (mac[0] & 0xFC) | 0x02; // unicast, locally administered
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn dict(entries: &[(&str, Value)]) -> Value {
    let mut d = Dictionary::new();
    for (k, v) in entries {
        d.insert((*k).to_owned(), v.clone());
    }
    Value::Dictionary(d)
}

/// `config.plist` as make-bundle.py writes it, with new-vm.sh's random MAC.
pub fn config_plist(params: &BundleParams, ecid: u64, mac: &str) -> Vec<u8> {
    let mut config = dict(&[
        ("schemaVersion", Value::from(2u32)),
        ("platformType", Value::from("vresearch101")),
        ("cpuCount", Value::from(params.cpus)),
        ("memorySize", Value::from(((params.memory_gib as u64) << 30) as u64)),
        (
            "screenConfig",
            dict(&[
                ("width", Value::from(params.screen.width)),
                ("height", Value::from(params.screen.height)),
                ("pixelsPerInch", Value::from(params.screen.ppi)),
                ("scale", Value::Real(params.screen.scale)),
            ]),
        ),
        (
            "romImages",
            dict(&[
                ("avpBooter", Value::from("OpenAVB.bin")),
                ("avpSEPBooter", Value::from("OpenSEP.bin")),
            ]),
        ),
        ("diskImage", Value::from("Disk.img")),
        ("nvramStorage", Value::from("nvram.bin")),
        ("sepStorage", Value::from("SEPStorage")),
        (
            "networkConfig",
            dict(&[("mode", Value::from("nat")), ("macAddress", Value::from(mac))]),
        ),
        ("machineIdentifier", Value::Data(plist_py::machine_identifier(ecid))),
    ]);
    if let (Some(device), Some(d)) = (params.device, config.as_dictionary_mut()) {
        d.insert("guestProductType".to_owned(), Value::from(device));
    }
    plist_py::to_xml(&config)
}

fn sparse(path: &Path, size: u64) -> Result<()> {
    let file = File::create(path).map_err(|e| Error::path(path, e))?;
    file.set_len(size).map_err(|e| Error::path(path, e))
}

/// The `ecid` file's value, parsed as hex.
pub fn read_ecid(vm: &Path) -> Result<u64> {
    let path = vm.join("ecid");
    let text = fs::read_to_string(&path).map_err(|e| Error::path(&path, e))?;
    let text = text.trim().trim_start_matches("0x");
    u64::from_str_radix(text, 16).map_err(|_| Error::msg(format!("{}: not a hex ECID", path.display())))
}

/// Create the bundle. Re-running on a complete one is safe: an existing `ecid`
/// is kept, so the identity a restore was made for does not change.
pub fn create(params: &BundleParams, progress: &dyn Progress) -> Result<Bundle> {
    let vm = params.vm;
    match vm.file_name().and_then(|n| n.to_str()) {
        None | Some("") => bail!("{}: not a VM directory", vm.display()),
        Some(n) if n.starts_with('.') => {
            bail!("the VM's directory name must not start with '.' (vphone-cli refuses it)")
        }
        _ => {}
    }
    for f in [params.open_avb, params.open_sep] {
        if !f.is_file() {
            bail!("missing {}", f.display());
        }
    }
    let avb = fs::read(params.open_avb).map_err(|e| Error::path(params.open_avb, e))?;
    if !avb.windows(12).any(|w| w == b"OpenBootROM:") {
        bail!("{} is not OpenAVB", params.open_avb.display());
    }
    let native = Native {
        width: params.screen.width,
        height: params.screen.height,
        scale: params.screen.scale,
    };
    let render = native.render_size(params.scale, params.width_policy)?;
    fs::create_dir_all(vm).map_err(|e| Error::path(vm, e))?;
    let ecid = match read_ecid(vm) {
        Ok(existing) => existing,
        Err(_) => params.ecid.unwrap_or_else(random_ecid),
    };
    fs::copy(params.open_avb, vm.join("OpenAVB.bin")).map_err(|e| Error::path(&vm.join("OpenAVB.bin"), e))?;
    fs::copy(params.open_sep, vm.join("OpenSEP.bin")).map_err(|e| Error::path(&vm.join("OpenSEP.bin"), e))?;
    for (name, size) in [("Disk.img", DISK_SIZE), ("nvram.bin", NVRAM_SIZE), ("SEPStorage", SEP_SIZE)] {
        let path = vm.join(name);
        if !path.exists() {
            sparse(&path, size)?;
        }
    }
    let config_path = vm.join("config.plist");
    fs::write(&config_path, config_plist(params, ecid, &random_mac())).map_err(|e| Error::path(&config_path, e))?;
    fs::write(vm.join("ecid"), format!("0x{ecid:016x}\n")).map_err(|e| Error::path(&vm.join("ecid"), e))?;
    fs::write(vm.join("udid-prediction.txt"), format!("ECID={ecid:016X}\n"))
        .map_err(|e| Error::path(&vm.join("udid-prediction.txt"), e))?;
    display::write_res(&vm.join("gfx-res"), render)?;
    display::write_res(&vm.join("gfx-res.3x"), (params.screen.width, params.screen.height))?;
    progress.log(&format!(
        "bundle ready: {} (ECID 0x{ecid:x}, screen {}x{} native @{}x, scale {}x -> render {}x{})",
        vm.display(),
        params.screen.width,
        params.screen.height,
        params.screen.scale,
        params.scale,
        render.0,
        render.1
    ));
    Ok(Bundle { vm: vm.to_path_buf(), ecid, render })
}
