//! Signs real iOS binaries from vphone-cli's release with apple-codesign in
//! process and checks the result with the crate's parser and the Python cdhash tool.

mod common;
use common::*;
use apple_codesign::{CodeSignatureFlags, MachFile};
use std::fs;
use vphone_create_steps as vc;
use vc::sign::{self, SignOptions};

fn signature_info(data: &[u8]) -> (String, CodeSignatureFlags, Option<String>, bool) {
    let file = MachFile::parse(data).unwrap();
    let macho = file.nth_macho(0).unwrap();
    let sig = macho.code_signature().unwrap().expect("signed");
    let cd = sig.code_directory().unwrap().unwrap();
    let ents = sig.entitlements().unwrap().map(|b| b.as_str().to_owned());
    let der = sig.entitlements_der().unwrap().is_some();
    (cd.ident.to_string(), cd.flags, ents, der)
}

#[test]
fn adhoc_signing_of_vphoned_with_merged_entitlements() {
    let dir = tmp();
    let bin = dir.path().join("vphoned");
    fs::copy(guest_resources().join("vphoned"), &bin).unwrap();
    let original = fs::read(&bin).unwrap();
    let before_ent = sign::entitlements_of(&original).unwrap();
    assert!(!before_ent.is_empty(), "the release's vphoned has entitlements");
    let new = br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>com.apple.managedconfiguration.profiled-access</key><true/></dict></plist>"#;
    sign::sign_file(&bin, &SignOptions { identifier: Some("vphoned".into()), entitlements: Some(new.to_vec()), merge_existing: true }).unwrap();
    let signed = fs::read(&bin).unwrap();
    let (ident, flags, ents, der) = signature_info(&signed);
    assert_eq!(ident, "vphoned");
    assert!(flags.contains(CodeSignatureFlags::ADHOC), "flags {flags:?}");
    assert!(der, "DER entitlements on an executable");
    let ents = ents.unwrap();
    assert!(ents.contains("com.apple.managedconfiguration.profiled-access"));
    // Everything the original granted is still granted.
    let old = String::from_utf8(before_ent[0].clone()).unwrap();
    let old_value: plist::Value = plist::from_bytes(old.as_bytes()).unwrap();
    let new_value: plist::Value = plist::from_bytes(ents.as_bytes()).unwrap();
    for key in old_value.as_dictionary().unwrap().keys() {
        assert!(new_value.as_dictionary().unwrap().contains_key(key), "lost {key}");
    }
    let (id2, _, _, _) = signature_info(&signed);
    assert_eq!(sign::identifier_of(&signed).unwrap().as_deref(), Some(id2.as_str()));
    // The tool that writes trust caches agrees on the new cdhash.
    let py = python(
        &format!(
            "import importlib.util,sys\nspec=importlib.util.spec_from_file_location('t',r'{}')\nm=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)\nfor h in m.macho_cdhashes(sys.argv[1]): print(h.hex())\n",
            repo().join("tools/ssh-ramdisk-trustcache.py").display()
        ),
        &[bin.to_str().unwrap()],
    );
    let ours: Vec<String> = vc::trustcache::macho_cdhashes(&signed, "vphoned", &vc::progress::Silent).iter().map(hex::encode).collect();
    assert_eq!(py.lines().collect::<Vec<_>>(), ours);
}

#[test]
fn signing_a_dylib_without_entitlements_keeps_it_loadable() {
    let dir = tmp();
    let lib = dir.path().join("libmisfix.dylib");
    fs::copy(guest_resources().join("libmisfix.dylib"), &lib).unwrap();
    sign::sign_file(&lib, &SignOptions { identifier: Some("libmisfix".into()), ..Default::default() }).unwrap();
    let data = fs::read(&lib).unwrap();
    let (ident, flags, ents, _) = signature_info(&data);
    assert_eq!(ident, "libmisfix");
    assert!(flags.contains(CodeSignatureFlags::ADHOC));
    assert!(ents.is_none());
    // Re-signing an already signed file replaces the signature instead of stacking.
    let size = data.len();
    sign::sign_file(&lib, &SignOptions { identifier: Some("libmisfix".into()), ..Default::default() }).unwrap();
    assert_eq!(fs::read(&lib).unwrap().len(), size);
}

#[test]
fn merge_entitlements_updates_and_appends() {
    let base = br#"<plist version="1.0"><dict><key>a</key><true/><key>b</key><false/></dict></plist>"#;
    let new = br#"<plist version="1.0"><dict><key>b</key><true/><key>c</key><true/></dict></plist>"#;
    let merged = sign::merge_entitlements(base, new).unwrap();
    let v: plist::Value = plist::from_bytes(&merged).unwrap();
    let d = v.as_dictionary().unwrap();
    assert_eq!(d["a"].as_boolean(), Some(true));
    assert_eq!(d["b"].as_boolean(), Some(true));
    assert_eq!(d["c"].as_boolean(), Some(true));
    assert_eq!(d.keys().map(String::as_str).collect::<Vec<_>>(), ["a", "b", "c"]);
}

#[test]
fn sealing_a_real_guest_bundle_updates_code_resources() {
    let dir = tmp();
    let driver = dir.path().join("VPhoneVirtIOSound.driver");
    fs::create_dir_all(driver.join("_CodeSignature")).unwrap();
    let src = guest_resources().join("VPhoneVirtIOSound.driver");
    for f in ["Info.plist", "VPhoneVirtIOSound", "_CodeSignature/CodeResources"] {
        fs::copy(src.join(f), driver.join(f)).unwrap();
    }
    let before = fs::read(driver.join("VPhoneVirtIOSound")).unwrap();
    sign::seal_bundle(&driver).unwrap();
    let after = fs::read(driver.join("VPhoneVirtIOSound")).unwrap();
    assert_ne!(before, after, "the binary was re-signed");
    assert!(sign::identifier_of(&after).unwrap().is_some());
    assert!(driver.join("_CodeSignature/CodeResources").is_file());
    let (_, flags, _, _) = signature_info(&after);
    assert!(flags.contains(CodeSignatureFlags::ADHOC));
}
