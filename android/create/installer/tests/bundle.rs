mod common;
use common::*;
use std::fs;
use vphone_create_steps as vc;
use vc::display::{Native, WidthPolicy};
use vc::progress::Silent;

fn params<'a>(vm: &'a std::path::Path, rom: &'a std::path::Path, device: &'a str, compat: &std::path::Path, scale: u32, ecid: Option<u64>) -> vc::bundle::BundleParams<'a> {
    vc::bundle::BundleParams {
        vm,
        device: Some(device),
        screen: vc::bundle::device_screen(compat, device),
        scale,
        width_policy: WidthPolicy::AlignTo16,
        ecid,
        memory_gib: 8,
        cpus: 8,
        open_avb: rom,
        open_sep: rom,
    }
}

#[test]
fn bundle_files_and_screen_from_compatibility_json() {
    let compat = repo().join("setup/data/compatibility.json");
    let dir = tmp();
    let rom = dir.path().join("OpenAVB.bin");
    fs::write(&rom, b"xxOpenBootROM: hello").unwrap();
    let vm = dir.path().join("vm");
    let b = vc::bundle::create(&params(&vm, &rom, "iPhone19,4", &compat, 1, Some(0x1234)), &Silent).unwrap();
    assert_eq!(b.ecid, 0x1234);
    // iPhone Duo: 1398/3 = 466 points, nearest multiple of 16 is 464.
    assert_eq!(fs::read_to_string(vm.join("gfx-res")).unwrap(), "464x678\n");
    assert_eq!(fs::read_to_string(vm.join("gfx-res.3x")).unwrap(), "1398x2034\n");
    assert_eq!(fs::read_to_string(vm.join("ecid")).unwrap(), "0x0000000000001234\n");
    assert_eq!(fs::read_to_string(vm.join("udid-prediction.txt")).unwrap(), "ECID=0000000000001234\n");
    assert_eq!(fs::metadata(vm.join("Disk.img")).unwrap().len(), 64 << 30);
    assert_eq!(fs::metadata(vm.join("nvram.bin")).unwrap().len(), 33570816);
    assert_eq!(fs::metadata(vm.join("SEPStorage")).unwrap().len(), 512 << 10);
    use std::os::unix::fs::MetadataExt;
    assert!(fs::metadata(vm.join("Disk.img")).unwrap().blocks() < 16, "Disk.img must be sparse");
    let config: plist::Value = plist::from_file(vm.join("config.plist")).unwrap();
    let d = config.as_dictionary().unwrap();
    assert_eq!(d["guestProductType"].as_string(), Some("iPhone19,4"));
    let sc = d["screenConfig"].as_dictionary().unwrap();
    assert_eq!(sc["width"].as_unsigned_integer(), Some(1398));
    let id: plist::Value = plist::from_bytes(d["machineIdentifier"].as_data().unwrap()).unwrap();
    assert_eq!(id.as_dictionary().unwrap()["ECID"].as_unsigned_integer(), Some(0x1234));
    let mac = d["networkConfig"].as_dictionary().unwrap()["macAddress"].as_string().unwrap();
    assert_eq!(mac.len(), 17);
    assert_eq!(u8::from_str_radix(&mac[..2], 16).unwrap() & 3, 2, "unicast, locally administered");

    // A re-run keeps the identity even if asked for another.
    let b2 = vc::bundle::create(&params(&vm, &rom, "iPhone19,4", &compat, 1, Some(0x9999)), &Silent).unwrap();
    assert_eq!(b2.ecid, 0x1234);
}

#[test]
fn bundle_refusals() {
    let compat = repo().join("setup/data/compatibility.json");
    let dir = tmp();
    let rom = dir.path().join("rom.bin");
    fs::write(&rom, b"xxOpenBootROM: hello").unwrap();
    let hidden = dir.path().join(".vm");
    assert!(vc::bundle::create(&params(&hidden, &rom, "iPhone17,3", &compat, 1, None), &Silent).is_err());
    let not_avb = dir.path().join("bad.bin");
    fs::write(&not_avb, b"nope").unwrap();
    let vm = dir.path().join("vm");
    assert!(vc::bundle::create(&params(&vm, &not_avb, "iPhone17,3", &compat, 1, None), &Silent).is_err());
}

#[test]
fn screen_defaults_and_scale_widths() {
    let compat = repo().join("setup/data/compatibility.json");
    let s = vc::bundle::device_screen(&compat, "iPhone18,1");
    assert_eq!((s.width, s.height), (1290, 2796));
    let native = Native { width: 1290, height: 2796, scale: 3.0 };
    assert_eq!(native.render_size(1, WidthPolicy::AlignTo16).unwrap(), (432, 932));
    assert_eq!(native.render_size(2, WidthPolicy::AlignTo16).unwrap(), (864, 1864));
    assert_eq!(native.render_size(3, WidthPolicy::AlignTo16).unwrap(), (1290, 2796));
    assert_eq!(native.render_size(1, WidthPolicy::ExactPoints).unwrap(), (430, 932));
    assert_eq!(native.render_size(2, WidthPolicy::ExactPoints).unwrap(), (860, 1864));
    assert!(native.render_size(4, WidthPolicy::AlignTo16).is_err());
    assert_eq!(432 * 4 % 64, 0);
    assert_eq!(vc::steps::effective_scale("27.1", 1), 2);
    assert_eq!(vc::steps::effective_scale("26.6.2", 1), 1);
}

#[test]
fn release_unpack_keeps_only_guest_payloads_and_roms() {
    let zip_path = assets().join("release/VPhone-2.14.2.zip");
    assert_eq!(vc::net::sha256_file(&zip_path).unwrap(), vc::resources::VPHONE_RELEASE_SHA256);
    let dir = tmp();
    let root = dir.path().join("r");
    vc::resources::unpack_release(&zip_path, &root, &Silent).unwrap();
    let r = vc::resources::Release { root: root.clone() };
    assert!(fs::read(r.open_avb()).unwrap().windows(12).any(|w| w == b"OpenBootROM:"));
    assert!(r.open_sep().is_file());
    assert!(r.guest("vphoned").is_file());
    assert!(r.guest("VPhoneVirtIOSound.driver/_CodeSignature/CodeResources").is_file());
    assert!(!root.join("en.lproj").exists());
    assert!(root.join("kernelcache_recipes").is_dir());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(r.guest("vphoned")).unwrap().permissions().mode() & 0o111, 0o111);
}

#[test]
fn staging_unpack_drops_what_the_script_drops() {
    let dir = tmp();
    let out = dir.path().join("root");
    let tarball = assets().join("sshtars/ssh.tar.gz");
    assert_eq!(vc::net::sha256_file(&tarball).unwrap(), vc::resources::SSHTARS_SHA256);
    vc::ramdisk::unpack_staging(&tarball, &out).unwrap();
    assert!(out.join("usr/local/bin/dropbear").is_file());
    assert!(out.join("usr/libexec/sftp-server").is_file());
    assert!(!out.join("usr/trollstore").exists());
    assert!(!out.join("usr/share/misc/magic.mgc").exists());
    assert!(out.join("usr/lib/libresolv.9.dylib").exists() || fs::symlink_metadata(out.join("usr/lib/libresolv.9.dylib")).is_ok());
}

#[test]
fn step_record_resumes() {
    let dir = tmp();
    let mut r = vc::progress::StepRecord::open(dir.path()).unwrap();
    let mut runs = 0;
    r.step(&Silent, "a", || { runs += 1; Ok::<(), vc::error::Error>(()) }).unwrap();
    let failed: vc::error::Result<()> = r.step(&Silent, "b", || Err(vc::error::Error::msg("x")));
    assert!(failed.is_err());
    let mut r = vc::progress::StepRecord::open(dir.path()).unwrap();
    r.step(&Silent, "a", || { runs += 1; Ok::<(), vc::error::Error>(()) }).unwrap();
    assert_eq!(runs, 1);
    assert!(!r.is_done("b"));
    r.step(&Silent, "cfw.x", || Ok::<(), vc::error::Error>(())).unwrap();
    r.forget_prefix("cfw.").unwrap();
    assert!(!vc::progress::StepRecord::open(dir.path()).unwrap().is_done("cfw.x"));
}
