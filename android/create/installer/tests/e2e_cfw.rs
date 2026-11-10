//! A whole `cfw::install` against real dropbear and sftp-server with a guest
//! tree on this host. The patch verbs and QEMU are test doubles (worker 1's
//! Swift tool and a phone are not available here); everything else is real:
//! SSH, SFTP, tar, the APFS library on a real ramdisk used as the cryptexes,
//! rcodesign signing of real iOS binaries, plists, receipt and resume.

mod common;
use common::*;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use vphone_create_steps as vc;
use vc::cfw::{self, InstallContext};
use vc::guestfs::GuestVolume;
use vc::host::{BackgroundHelper, Host, VmKind};
use vc::plist_py;
use vc::progress::{Progress, StepRecord};
use vc::resources::Release;
use vc::ssh::{Connect, GuestSsh, Target};

struct Double {
    calls: Mutex<Vec<Vec<String>>>,
    cancelled: Mutex<bool>,
}

impl Progress for Double {
    fn log(&self, line: &str) {
        println!("{line}");
    }
    fn progress(&self, _d: u64, _t: u64, _m: &str) {}
    fn cancelled(&self) -> bool {
        *self.cancelled.lock().unwrap()
    }
}

impl Host for Double {
    fn start_vm(&self, _: VmKind, _: &Path, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn stop_vm(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn vm_alive(&self) -> bool {
        true
    }
    fn serial_log(&self) -> PathBuf {
        PathBuf::new()
    }
    fn vphoned_rpc(&self, _: &str, _: Value, _: Duration) -> Result<Value, String> {
        Err("no guest".into())
    }
    fn run_helper(&self, lib: &str, args: &[String], _: &[(String, String)], _: &Path, _: Option<Duration>) -> Result<i32, String> {
        assert_eq!(lib, "libvphone_cli.so");
        self.calls.lock().unwrap().push(args.to_vec());
        if args[0] == "cfw" {
            let verb = args[1].as_str();
            if let Some(i) = args.iter().position(|a| a == "--undo-log") {
                let path = &args[i + 1];
                let mut text = fs::read_to_string(path).unwrap_or_default();
                text.push_str(&format!("{}\n", args[i + 3]));
                fs::write(path, text).unwrap();
            }
            match verb {
                "patch-virtual-bluetooth" | "patch-virtualaudio-graph-configurations" | "patch-virtualaudio-speaker-raw" | "patch-virtualaudio-microphone-chains" | "patch-dt-haptics" | "patch-dt-display-nodes" | "patch-dt-microphone-array" => {
                    let mut data = fs::read(&args[2]).unwrap();
                    data.extend_from_slice(format!("<!-- {verb} -->").as_bytes());
                    fs::write(&args[2], data).unwrap();
                }
                _ => {}
            }
        }
        Ok(0)
    }
    fn spawn_helper(&self, _: &str, _: &[String], _: &[(String, String)], _: &Path) -> Result<Box<dyn BackgroundHelper>, String> {
        Err("not used".into())
    }
    fn usbmux_socket(&self) -> PathBuf {
        PathBuf::new()
    }
}

#[allow(dead_code)]
struct Server(Child, u16, tempfile::TempDir);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn dropbear() -> Server {
    let dir = tmp();
    let base = assets().join("dropbear/src");
    let key = dir.path().join("k");
    assert!(Command::new(base.join("dropbearkey")).args(["-t", "ecdsa", "-f", key.to_str().unwrap()]).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success());
    let shim = dir.path().join("s.so");
    assert!(Command::new("gcc").args(["-shared", "-fPIC", "-o"]).arg(&shim).arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/preload_shim.c")).status().unwrap().success());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let child = Command::new(base.join("dropbear"))
        .args(["-F", "-p", &format!("127.0.0.1:{port}"), "-r"])
        .arg(&key)
        .env("LD_PRELOAD", &shim)
        .env("SHIM_HOME", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Server(child, port, dir)
}

fn plist(path: &Path, value: plist::Value) {
    fs::write(path, plist_py::to_xml(&value)).unwrap();
}

fn dict(entries: Vec<(&str, plist::Value)>) -> plist::Value {
    let mut d = plist::Dictionary::new();
    for (k, v) in entries {
        d.insert(k.to_owned(), v);
    }
    plist::Value::Dictionary(d)
}

fn install_binary(system: &Path, rel: &str, source: &Path) {
    let p = system.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::copy(source, &p).unwrap();
}

fn guest_tree(root: &Path, guest_res: &Path, version: &str) -> (PathBuf, PathBuf, PathBuf) {
    let system = root.join("mnt1");
    let xart = root.join("mnt7");
    let preboot = root.join("mnt6");
    let lib = guest_res.join("libmisfix.dylib");
    for rel in [
        "usr/libexec/seputil", "usr/libexec/launchd_cache_loader", "usr/libexec/mobileactivationd",
        "usr/libexec/watchdogd", "sbin/launchd",
        "Applications/Preferences.app/Preferences",
    ] {
        install_binary(&system, rel, &lib);
    }
    install_binary(&system, "usr/libexec/debugserver", &guest_res.join("vphoned"));
    fs::create_dir_all(system.join("System/Library/xpc")).unwrap();
    plist(&system.join("System/Library/xpc/launchd.plist"), dict(vec![("LaunchDaemons", dict(vec![]))]));
    fs::create_dir_all(system.join("System/Library/LaunchDaemons")).unwrap();
    fs::create_dir_all(system.join("System/Library/CoreServices")).unwrap();
    plist(&system.join("System/Library/CoreServices/SystemVersion.plist"), dict(vec![("ProductVersion", version.into())]));
    fs::create_dir_all(system.join("System/Library/FeatureFlags/Domain")).unwrap();
    plist(&system.join("System/Library/FeatureFlags/Domain/BluetoothFeatures.plist"), dict(vec![("a", 1i64.into())]));
    fs::create_dir_all(system.join("System/Library/Caches")).unwrap();
    fs::create_dir_all(system.join("System/DriverKit/System/Library")).unwrap();
    fs::create_dir_all(system.join("usr/bin")).unwrap();
    fs::create_dir_all(system.join("usr/lib")).unwrap();
    fs::create_dir_all(system.join("System/Library/Extensions")).unwrap();
    fs::create_dir_all(system.join("System/Library/Audio/Plug-Ins/HAL")).unwrap();
    fs::create_dir_all(system.join("Library/Audio/Tunings/A1/VAD")).unwrap();
    plist(&system.join("Library/Audio/Tunings/A1/VAD/graph_configurations.plist"), dict(vec![("g", 1i64.into())]));
    // The restored rootfs has dangling Cryptex links.
    fs::create_dir_all(system.join("System/Cryptexes")).unwrap();
    std::os::unix::fs::symlink("/private/preboot/Cryptexes/OS", system.join("System/Cryptexes/OS")).unwrap();
    std::os::unix::fs::symlink("/private/preboot/Cryptexes/App", system.join("System/Cryptexes/App")).unwrap();
    fs::create_dir_all(&xart).unwrap();
    fs::write(xart.join("11111111-2222.gl"), b"gigalocker").unwrap();
    let firmware = preboot.join("ABCDEF/usr/standalone/firmware");
    fs::create_dir_all(&firmware).unwrap();
    fs::write(firmware.join("devicetree.img4"), b"device tree").unwrap();
    (system, xart, preboot)
}

fn cryptex_image(dir: &Path) -> PathBuf {
    // A copy of the real ramdisk, with a dyld cache directory added through the
    // APFS library, stands in for the OS cryptex.
    let image = dir.join("os.apfs");
    fs::copy(repo().join("android/build/apfs-work/real/ramdisk.apfs"), &image).unwrap();
    let mut v = vphone_apfs::MutVolume::open(&image, &Default::default()).unwrap();
    let host_dsc = dir.join("dsc");
    fs::create_dir_all(host_dsc.join("Caches/com.apple.dyld")).unwrap();
    let mut main = vec![0u8; 0x100];
    main[..7].copy_from_slice(b"dyld_v1");
    main[0x10..0x14].copy_from_slice(&0x200u32.to_le_bytes());
    main[0xE8..0xF0].copy_from_slice(&0x1_0000_0000u64.to_le_bytes());
    fs::write(host_dsc.join("Caches/com.apple.dyld/dyld_shared_cache_arm64e"), &main).unwrap();
    vphone_apfs::copy_in(&mut v, &host_dsc.join("Caches"), "/System/Library", Some((0, 0)), None).unwrap();
    v.commit().unwrap();
    image
}

fn vm_dir(root: &Path, image: &Path) -> PathBuf {
    let vm = root.join("vm");
    let tree = vm.join("X_Restore");
    fs::create_dir_all(tree.join(".pcc-gpu/AppleParavirtGPUMetalIOGPUFamily.bundle/_CodeSignature")).unwrap();
    let gpu = tree.join(".pcc-gpu/AppleParavirtGPUMetalIOGPUFamily.bundle");
    for f in ["AppleParavirtGPUMetalIOGPUFamily", "Info.plist", "_CodeSignature/CodeResources", "libAppleParavirtCompilerPluginIOGPUFamily.dylib"] {
        fs::write(gpu.join(f), b"gpu").unwrap();
    }
    fs::copy(guest_resources().join("libmisfix.dylib"), gpu.join("AppleParavirtGPUMetalIOGPUFamily")).unwrap();
    plist(&gpu.join("Info.plist"), dict(vec![
        ("CFBundleExecutable", "AppleParavirtGPUMetalIOGPUFamily".into()),
        ("CFBundleIdentifier", "com.apple.driver.AppleParavirtGPUMetalIOGPUFamily".into()),
        ("CFBundlePackageType", "BNDL".into()),
    ]));
    let comp = |p: &str| dict(vec![("Info", dict(vec![("Path", p.into())]))]);
    fs::copy(image, tree.join("os.dmg")).unwrap();
    fs::copy(image, tree.join("app.dmg")).unwrap();
    let manifest = dict(vec![(
        "BuildIdentities",
        plist::Value::Array(vec![dict(vec![(
            "Manifest",
            dict(vec![("Cryptex1,SystemOS", comp("os.dmg")), ("Cryptex1,AppOS", comp("app.dmg"))]),
        )])]),
    )]);
    plist(&tree.join("iPhone-BuildManifest.plist"), manifest);
    plist(&vm.join("config.plist"), dict(vec![("guestProductType", "iPhone17,3".into())]));
    vm
}

fn run_install(version: &str, plan: Option<plist::Value>) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, PathBuf, Vec<Vec<String>>, cfw::InstallReport) {
    let server = dropbear();
    let dir = tmp();
    let guest_res = guest_resources();
    let (system, xart, preboot) = guest_tree(dir.path(), &guest_res, version);
    let image = cryptex_image(dir.path());
    let vm = vm_dir(dir.path(), &image);
    if let Some(plan) = plan {
        plist(&vm.join("PatchPlan.plist"), plan);
    }
    let release_root = guest_res.parent().unwrap();
    let release = Release { root: release_root.to_path_buf() };
    let mut options = Connect::ramdisk(Target::Tcp(format!("127.0.0.1:{}", server.1)));
    // dropbear starts sessions with a clean environment; the chown shim goes on the command.
    options.sftp_program = format!("LD_PRELOAD={} /usr/lib/ssh/sftp-server", server.2.path().join("s.so").display());
    let ssh = GuestSsh::connect(options).unwrap();
    let (sftp, raw) = ssh.sftp().unwrap();
    let host = Double { calls: Mutex::new(Vec::new()), cancelled: Mutex::new(false) };
    let work = vm.join(".cfw-work");
    let log = vm.join("log.txt");
    let cx = InstallContext {
        more: None,
        disk: None,
        host: &host,
        vm: &vm,
        release: &release,
        cli: "libvphone_cli.so",
        work: &work,
        log: &log,
        ssh: &ssh,
        system: GuestVolume::new(&ssh, &sftp, &raw, system.to_str().unwrap()),
        xart: GuestVolume::new(&ssh, &sftp, &raw, xart.to_str().unwrap()),
        preboot: GuestVolume::new(&ssh, &sftp, &raw, preboot.to_str().unwrap()),
    };
    let mut records = StepRecord::open(&vm).unwrap();
    let report = cfw::install(&cx, &mut records).unwrap();
    cfw::record_guest_receipt(&vm, &report.live, &report.not_applicable).unwrap();
    // A second run over the finished guest must not fail or re-copy the cryptexes.
    let report2 = cfw::install(&cx, &mut records).unwrap();
    assert_eq!(report2.live, report.live, "re-run");
    let calls = host.calls.lock().unwrap().clone();
    drop(cx);
    drop(ssh);
    drop(server);
    (dir, system, xart, preboot, vm, calls, report)
}

#[test]
fn full_install_on_ios_26() {
    let (_dir, system, xart, preboot, vm, calls, report) = run_install("26.6.2", None);
    assert!(report.failures.is_empty(), "failures: {:?}", report.failures);
    // Cryptexes replaced the dangling links with real directories from the image.
    assert!(fs::symlink_metadata(system.join("System/Cryptexes/OS")).unwrap().file_type().is_dir());
    assert!(system.join("System/Cryptexes/OS/usr/lib").exists() || system.join("System/Cryptexes/OS/usr").exists());
    assert!(fs::read(system.join("System/Cryptexes/App/usr/local/bin/restored_external")).is_ok()
        || system.join("System/Cryptexes/App/usr").exists());
    // The dyld cache was held back from the tar, patched locally and uploaded once.
    let dsc = system.join("System/Cryptexes/OS/System/Library/Caches/com.apple.dyld");
    assert!(dsc.join("dyld_shared_cache_arm64e").is_file());
    assert!(dsc.join(".vphone-dsc-undo.json").is_file(), "the undo log travels with the cache");
    assert_eq!(fs::read_link(system.join("System/Library/Caches/com.apple.dyld")).unwrap(), Path::new("../../../System/Cryptexes/OS/System/Library/Caches/com.apple.dyld"));
    // vphoned, launchd hook, libraries.
    assert!(system.join("usr/bin/vphoned").is_file());
    assert_eq!(fs::metadata(system.join("usr/bin/vphoned")).unwrap().mode() & 0o777, 0o755);
    let launchd: plist::Value = plist::from_file(system.join("System/Library/xpc/launchd.plist")).unwrap();
    assert!(launchd.as_dictionary().unwrap()["LaunchDaemons"].as_dictionary().unwrap().contains_key("/System/Library/LaunchDaemons/vphoned.plist"));
    assert!(system.join("System/Library/xpc/launchd.plist.bak").is_file());
    assert_eq!(fs::read_link(system.join("vh")).unwrap(), Path::new("/usr/lib/launchdhook-vphone.dylib"));
    assert!(system.join("usr/lib/launchdhook-vphone.dylib").is_file());
    assert!(!system.join("usr/lib/libvphonefolddisplay.dylib").exists(), "fold-only library on a non-foldable");
    // Mach-O patch: backup kept, the verb ran, the result is ad-hoc signed with the original identifier.
    assert!(system.join("usr/libexec/seputil.bak").is_file());
    assert!(calls.iter().any(|c| c[0] == "cfw" && c[1] == "patch-seputil"));
    let seputil = fs::read(system.join("usr/libexec/seputil")).unwrap();
    assert_eq!(vc::sign::identifier_of(&seputil).unwrap().as_deref(), Some("com.apple.seputil"));
    assert!(calls.iter().any(|c| c[1] == "inject-dylib" && c[3] == "/vh"), "launchd hook");
    // patchCopy: a verb that changed the file leaves a .bak and the new bytes.
    let bt = system.join("System/Library/FeatureFlags/Domain/BluetoothFeatures.plist");
    assert!(fs::read_to_string(&bt).unwrap().contains("patch-virtual-bluetooth"));
    assert!(system.join("System/Library/FeatureFlags/Domain/BluetoothFeatures.plist.bak").is_file());
    // Gigalocker, GPU bundle, sound driver.
    assert_eq!(fs::read(xart.join("AA.gl")).unwrap(), b"gigalocker");
    assert!(!xart.join("11111111-2222.gl").exists());
    assert_eq!(fs::metadata(system.join("System/Library/Extensions/AppleParavirtGPUMetalIOGPUFamily.bundle/Info.plist")).unwrap().permissions().mode() & 0o777, 0o644);
    assert!(system.join("System/Library/Audio/Plug-Ins/HAL/VPhoneVirtIOSound.driver/VPhoneVirtIOSound").is_file());
    // Preboot device tree repairs, with the post-identity backup.
    let dt = preboot.join("ABCDEF/usr/standalone/firmware");
    assert!(fs::read_to_string(dt.join("devicetree.img4")).unwrap().contains("patch-dt-haptics"));
    assert_eq!(fs::read(dt.join("devicetree.img4.bak")).unwrap(), b"device tree");
    // Host side: receipt and the daemon copy.
    assert!(vm.join(".vphoned.signed").is_file());
    let receipt: plist::Value = plist::from_file(vm.join("PatchReceipt.plist")).unwrap();
    let guest = &receipt.as_dictionary().unwrap()["Parts"].as_dictionary().unwrap()["Guest"];
    assert!(guest.as_dictionary().unwrap()["Patches"].as_array().unwrap().len() > 10);
    assert!(report.live.contains("system-vphoned-boot-install"));
    assert!(report.live.contains("system-gigalocker-boot-rename"));
    // Without a plan the launchd X1 patch follows the legacy default (as vphone-cli's
    // `on()` does); the three version-gated binaries stay iOS 27 only.
    for name in ["/dyld", "/fsck", "/mount"] {
        assert!(!calls.iter().any(|c| c[1] == "patch-arm64e-x1" && c[2].ends_with(name)), "{name}");
    }
}

#[test]
fn a_plan_decides_what_is_applied() {
    let enabled: Vec<plist::Value> = [
        "system-vphoned-boot-install",
        "system-launchdaemons-boot-environment",
        "system-gigalocker-boot-rename",
        "system-launchd-boot-jetsam_panic_guard_bypass",
        "dyld-cfw-camera",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();
    let plan = dict(vec![
        ("Preset", "test".into()),
        ("EnabledPatches", plist::Value::Array(enabled)),
    ]);
    let (_dir, system, _xart, _preboot, _vm, calls, report) = run_install("26.6.2", Some(plan));
    assert!(report.failures.is_empty(), "failures: {:?}", report.failures);
    assert!(!system.join("usr/libexec/seputil.bak").exists(), "seputil is off in the plan");
    assert!(!calls.iter().any(|c| c[1] == "patch-seputil"));
    assert!(calls.iter().any(|c| c[1] == "patch-launchd-jetsam"));
    assert!(!calls.iter().any(|c| c[1] == "patch-arm64e-x1"), "X1 is off in this plan");
    assert!(calls.iter().any(|c| c[1] == "patch-camera-dsc"));
    assert!(!calls.iter().any(|c| c[1] == "patch-hv-vmm-dsc"));
    assert_eq!(report.live.iter().filter(|i| i.starts_with("dyld-")).count(), 1);
}
