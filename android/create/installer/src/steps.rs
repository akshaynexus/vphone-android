//! The create-VM steps the orchestrator calls, in `setup/create-vm.sh`'s order.
//! Each is safe to re-run after a failure: finished sub-steps are kept in
//! `<vm>/.create-steps`.

use crate::bail;
use crate::bundle::{self, Bundle, BundleParams, Screen};
use crate::cfw::{self, InstallContext, InstallReport};
use crate::display::{self, WidthPolicy};
use crate::error::{Error, Result};
use crate::guest;
use crate::guestfs::GuestVolume;
use crate::host::{BackgroundHelper, Host, VmKind};
use crate::manifest::{self, SshArtifacts};
use crate::nvram::{self, NvramEdit};
use crate::plist_py;
use crate::progress::{check_cancel, Progress, Silent, StepRecord};
use crate::ramdisk::{self, Ramdisk};
use crate::resources::{self, Release};
use crate::ssh::{Connect, GuestSsh, Target};
use crate::tree::ApfsTree;
use plist::Value;
use serde_json::json;
use sha1::{Digest, Sha1};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use vphone_apfs::container::VolumeSelector;

/// The helper with vphone-cli's commands.
pub const CLI: &str = "libvphone_cli.so";
pub const GPU_BUNDLE: &str = "AppleParavirtGPUMetalIOGPUFamily.bundle";
/// The first bytes of the driver's SHA-1 on the tested cloudOS 26.4 23E5207q.
pub const KNOWN_GPU_SHA1_PREFIX: &str = "b5d95b38";

const GIGALOCKER_SH: &str = r#"x=; for n in 1 2 3 4 5 6 7; do t=$(/System/Library/Filesystems/apfs.fs/apfs.util -p /dev/disk1s$n 2>/dev/null); [ "$t" = xART ] && x=/dev/disk1s$n; done
[ -n "$x" ] || x=/dev/disk1s3
/sbin/mount_apfs "$x" /mnt7 2>/dev/null
if [ -f /mnt7/AA.gl ]; then
  out=$(/usr/libexec/seputil --gigalocker-init /mnt7 2>&1)
  case $out in *"doesn'"'"'t exist"*) want=${out#*Gigalocker file (}; want=${want%%)*}; mv /mnt7/AA.gl "$want" && echo "gigalocker: AA.gl -> $want";; esac
fi
true"#;
const MOUNT_SH: &str = "/usr/bin/mount_filesystems; /sbin/mount";
const UNMOUNT_SH: &str = "sync; /sbin/umount /mnt2; /sbin/umount /mnt6; /sbin/umount /mnt1; sync";

fn cancellable_sleep(host: &dyn Progress, duration: Duration) -> Result<()> {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        check_cancel(host)?;
        std::thread::sleep(Duration::from_millis(500).min(end - Instant::now()));
    }
    check_cancel(host)
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

/// iOS 27 does not go below 2x (docs/ADVANCED.md, "Display scale").
pub fn effective_scale(ios: &str, scale: u32) -> u32 {
    if ios.starts_with("27.") && scale == 1 {
        2
    } else {
        scale
    }
}

/// Inputs of `create_bundle`.
pub struct CreateBundle<'a> {
    pub vm: &'a Path,
    pub device: &'a str,
    pub ios: &'a str,
    /// UI scale, 1, 2 or 3.
    pub scale: u32,
    pub width_policy: WidthPolicy,
    /// `setup/data/compatibility.json`.
    pub compat_json: &'a Path,
    /// Where run-time downloads are cached.
    pub assets: &'a Path,
    pub ecid: Option<u64>,
}

/// Step 1: an empty VM bundle, with the ROMs from vphone-cli's release.
pub fn create_bundle(params: &CreateBundle, progress: &dyn Progress) -> Result<Bundle> {
    let release = resources::fetch_release(params.assets, progress)?;
    let scale = effective_scale(params.ios, params.scale);
    bundle::create(
        &BundleParams {
            vm: params.vm,
            device: Some(params.device),
            screen: bundle::device_screen(params.compat_json, params.device),
            scale,
            width_policy: params.width_policy,
            ecid: params.ecid,
            memory_gib: 8,
            cpus: 8,
            open_avb: &release.open_avb(),
            open_sep: &release.open_sep(),
        },
        progress,
    )
}

/// The GPU driver found on disk or just extracted.
#[derive(Clone, Debug)]
pub struct GpuDriver {
    pub bundle: PathBuf,
    pub sha1: String,
    /// Whether the SHA-1 starts with the tested driver's.
    pub matches_known: bool,
}

/// SHA-1 of the bundle's driver binary, as tools/gpu-driver-from-cloudos.sh prints it.
pub fn describe_gpu_bundle(bundle: &Path) -> Result<GpuDriver> {
    let binary = bundle.join("AppleParavirtGPUMetalIOGPUFamily");
    let mut file = File::open(&binary).map_err(|e| Error::path(&binary, e))?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| Error::path(&binary, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sha1 = hex::encode(hasher.finalize());
    Ok(GpuDriver {
        bundle: bundle.to_path_buf(),
        matches_known: sha1.starts_with(KNOWN_GPU_SHA1_PREFIX),
        sha1,
    })
}

/// Read a driver bundle out of a restored `Disk.img` (the APFS library in place
/// of 7-Zip), reporting its SHA-1.
pub fn extract_gpu_bundle(disk_image: &Path, out_dir: &Path, progress: &dyn Progress) -> Result<GpuDriver> {
    fs::create_dir_all(out_dir).map_err(|e| Error::path(out_dir, e))?;
    let tmp = out_dir.join(".gpu-driver-extract");
    let _ = fs::remove_dir_all(&tmp);
    let tree = ApfsTree {
        image: disk_image.to_path_buf(),
        selector: VolumeSelector {
            role: Some("system".to_owned()),
            ..Default::default()
        },
        root: format!("/System/Library/Extensions/{GPU_BUNDLE}"),
    };
    tree.extract(&tmp, progress)?;
    if !tmp.join("AppleParavirtGPUMetalIOGPUFamily").is_file() {
        bail!("driver not found in the restored disk");
    }
    let out = out_dir.join(GPU_BUNDLE);
    let _ = fs::remove_dir_all(&out);
    fs::rename(&tmp, &out).map_err(|e| Error::path(&out, e))?;
    describe_gpu_bundle(&out)
}

/// Add iPhone99,11 to a manifest's SupportedProductTypes (the temporary VM's
/// product type, absent from cloudOS releases after the 26.4 beta).
pub fn admit_temporary_vm(manifest_path: &Path) -> Result<()> {
    let mut manifest = Value::from_file(manifest_path).map_err(|e| Error::msg(format!("{}: {e}", manifest_path.display())))?;
    let dict = manifest
        .as_dictionary_mut()
        .ok_or_else(|| Error::msg("BuildManifest is not a dictionary"))?;
    let product = Value::from("iPhone99,11");
    if !dict.contains_key("SupportedProductTypes") {
        dict.insert("SupportedProductTypes".to_owned(), Value::Array(Vec::new()));
    }
    let list = dict
        .get_mut("SupportedProductTypes")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::msg("SupportedProductTypes is not an array"))?;
    if !list.contains(&product) {
        list.push(product);
        fs::write(manifest_path, plist_py::to_xml(&manifest)).map_err(|e| Error::path(manifest_path, e))?;
    }
    Ok(())
}

fn unzip_all(zip_path: &Path, dest: &Path, progress: &dyn Progress) -> Result<()> {
    let file = File::open(zip_path).map_err(|e| Error::path(zip_path, e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| Error::msg(format!("{}: {e}", zip_path.display())))?;
    let total = archive.len() as u64;
    for i in 0..archive.len() {
        check_cancel(progress)?;
        let mut entry = archive.by_index(i).map_err(|e| Error::msg(format!("{}: {e}", zip_path.display())))?;
        let Some(rel) = entry.enclosed_name() else { continue };
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| Error::path(&out, e))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::path(parent, e))?;
        }
        let mut o = File::create(&out).map_err(|e| Error::path(&out, e))?;
        std::io::copy(&mut entry, &mut o).map_err(|e| Error::path(&out, e))?;
        progress.progress(i as u64 + 1, total, "unzipping the restore tree");
    }
    Ok(())
}

/// Removes the temporary VM and stops it, however the step ends.
struct TempVm<'a> {
    host: &'a dyn Host,
    vm: PathBuf,
}

impl Drop for TempVm<'_> {
    fn drop(&mut self) {
        let _ = self.host.stop_vm(&self.vm);
        let _ = fs::remove_dir_all(&self.vm);
    }
}

/// Inputs of `gpu_driver`.
pub struct GpuParams<'a> {
    pub host: &'a dyn Host,
    /// The VM library the temporary VM is made in.
    pub library: &'a Path,
    pub cloud_ipsw: &'a Path,
    /// Where `AppleParavirtGPUMetalIOGPUFamily.bundle` goes (shared by VMs).
    pub out_dir: &'a Path,
    pub release: &'a Release,
    pub log: &'a Path,
}

/// tools/gpu-driver-from-cloudos.sh: restore the stock cloudOS into a temporary
/// VM and read the driver from its System volume. About 20-30 minutes.
pub fn gpu_driver(params: &GpuParams) -> Result<GpuDriver> {
    let host = params.host;
    let existing = params.out_dir.join(GPU_BUNDLE);
    if existing.join("AppleParavirtGPUMetalIOGPUFamily").is_file() {
        return describe_gpu_bundle(&existing);
    }
    let name = format!("gpu-recovery-{}", std::process::id());
    let vm = params.library.join(&name);
    if vm.exists() {
        fs::remove_dir_all(&vm).map_err(|e| Error::path(&vm, e))?;
    }
    let temp = TempVm { host, vm: vm.clone() };
    bundle::create(
        &BundleParams {
            vm: &vm,
            device: None,
            screen: Screen::default(),
            scale: 3,
            width_policy: WidthPolicy::ExactPoints,
            ecid: None,
            memory_gib: 8,
            cpus: 8,
            open_avb: &params.release.open_avb(),
            open_sep: &params.release.open_sep(),
        },
        &Silent,
    )?;
    let tree = vm.join("iPhonePCC_Restore");
    host.log(&format!("=== unzipping {} as its restore tree", params.cloud_ipsw.display()));
    unzip_all(params.cloud_ipsw, &tree, host)?;
    admit_temporary_vm(&tree.join("BuildManifest.plist"))?;
    let run = vm.join(".runs-dfu");
    fs::create_dir_all(&run).map_err(|e| Error::path(&run, e))?;
    host.log("=== DFU");
    host.start_vm(VmKind::Dfu, &vm, &run).map_err(Error::msg)?;
    host.log("=== waiting for the VM's DFU mode");
    let mut in_dfu = false;
    // A phone's guest answers slowly: the tool's default of 2 seconds is too short here.
    let probe = strings(&["recovery-probe", "--timeout", "60"]);
    for _ in 0..20 {
        check_cancel(host)?;
        if !host.vm_alive() {
            bail!("the temporary VM stopped before reaching DFU\n{}", host.diagnostics());
        }
        if host.run_helper(CLI, &probe, &[], params.log, None).map_err(Error::msg)? == 0 {
            in_dfu = true;
            break;
        }
        cancellable_sleep(host, Duration::from_secs(3))?;
    }
    if !in_dfu {
        bail!("the temporary VM did not reach DFU within 20 minutes\n{}", host.diagnostics());
    }
    let log = run.join("restore.log");
    host.log("=== restoring the stock cloudOS");
    let library = params.library.to_string_lossy().into_owned();
    let args = vec!["restore".to_owned(), "-l".to_owned(), library, name, "-v".to_owned()];
    let env = [("VPHONE_RESTORE_TIMEOUT_SCALE".to_owned(), "10".to_owned())];
    let mut helper = host.spawn_helper(CLI, &args, &env, &log).map_err(Error::msg)?;
    wait_restore(host, helper.as_mut(), &log)?;
    helper.stop();
    let finished = log_contains_finished(&log)?;
    if !finished {
        bail!("restore did not finish; log: {}", log.display());
    }
    host.stop_vm(&vm).map_err(Error::msg)?;
    cancellable_sleep(host, Duration::from_secs(3))?;
    host.log("=== reading the driver from the System volume");
    let driver = extract_gpu_bundle(&vm.join("Disk.img"), params.out_dir, host)?;
    host.log(&format!(
        "=== {} (sha1 {}{})",
        driver.bundle.display(),
        driver.sha1,
        if driver.matches_known { ", the tested driver" } else { ", NOT the tested driver" }
    ));
    drop(temp);
    Ok(driver)
}

fn log_contains_finished(log: &Path) -> Result<bool> {
    let mut text = String::new();
    File::open(log)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(|e| Error::path(log, e))?;
    Ok(text.to_ascii_lowercase().contains("restore finished"))
}

/// tools/restore-wait.sh: the tool can wait forever after a finished restore, so
/// once the device said it finished the tool gets 120 s, then it counts as done.
fn wait_restore(host: &dyn Host, helper: &mut dyn BackgroundHelper, log: &Path) -> Result<()> {
    let mut offset = 0u64;
    let mut finished_at = None;
    loop {
        if let Some(code) = helper.finished() {
            if code != 0 && finished_at.is_none() {
                let tail = fs::read(log).unwrap_or_default();
                let tail = String::from_utf8_lossy(&tail[tail.len().saturating_sub(3000)..]).into_owned();
                bail!("restore exited with status {code}\nIts last output:\n{}\n\n{}", tail.trim(), host.diagnostics());
            }
            return Ok(());
        }
        if finished_at.is_none() {
            if let Ok(mut f) = File::open(log) {
                if f.seek(SeekFrom::Start(offset.saturating_sub(64))).is_ok() {
                    let mut chunk = Vec::new();
                    let _ = f.read_to_end(&mut chunk);
                    offset += chunk.len() as u64;
                    if String::from_utf8_lossy(&chunk).to_ascii_lowercase().contains("restore finished") {
                        finished_at = Some(Instant::now());
                        host.log("the device reports the restore finished; waiting up to 120 s for the tool");
                    }
                }
            }
        }
        if finished_at.is_some_and(|t: Instant| t.elapsed() >= Duration::from_secs(120)) {
            host.log("the tool is still waiting for the device to come back; stopping it (the restore itself finished)");
            return Ok(());
        }
        cancellable_sleep(host, Duration::from_secs(5))?;
    }
}

/// Inputs of `cfw`.
pub struct CfwParams<'a> {
    pub host: &'a dyn Host,
    pub library: &'a Path,
    pub name: &'a str,
    pub release: &'a Release,
    /// Where sshtars is cached.
    pub assets: &'a Path,
    pub scale: u32,
    pub ecid: u64,
    pub log: &'a Path,
}

fn ssh_wait(host: &dyn Host, log: &Path) -> Result<GuestSsh> {
    let socket = host.usbmux_socket();
    let mut last = String::new();
    for tries in 0..60 {
        check_cancel(host)?;
        let attempt = (|| -> Result<GuestSsh> {
            let stream = vusb_portfwd::connect(&socket, 22, Duration::from_secs(30))
                .map_err(|e| Error::Ssh(format!("usbmux: {e}")))?;
            let ssh = GuestSsh::connect(Connect::ramdisk(Target::Connected(stream)))?;
            ssh.exec("id", Some(Duration::from_secs(30)))?.check("id")?;
            Ok(ssh)
        })();
        match attempt {
            Ok(ssh) => return Ok(ssh),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => last = e.to_string(),
        }
        if (tries + 1) % 10 == 0 {
            host.log(&format!("SSH ramdisk not answering yet ({last}); see {}", log.display()));
        }
        cancellable_sleep(host, Duration::from_secs(30))?;
    }
    bail!("the SSH ramdisk did not answer in 30 min: {last}")
}

/// `do_cfw`: build and boot the SSH ramdisk, install the CFW, and leave the
/// bundle ready to boot. Resumes after a failure.
pub fn cfw(params: &CfwParams) -> Result<InstallReport> {
    let host = params.host;
    let vm = params.library.join(params.name);
    let mut records = StepRecord::open(&vm)?;
    let rd = Ramdisk::new(vm.join(".sshramdisk"));
    let library = params.library.to_string_lossy().into_owned();
    let cli_ok = |args: &[String], env: &[(String, String)], timeout: Option<Duration>| -> Result<()> {
        let code = host.run_helper(CLI, args, env, params.log, timeout).map_err(Error::msg)?;
        if code != 0 {
            bail!("{CLI} {} failed ({code})", args.join(" "));
        }
        Ok(())
    };
    records.step(host, "cfw.staged", || ramdisk::stage(&rd, params.assets, host))?;
    records.step(host, "cfw.ramdisk-built", || ramdisk::rebuild(&vm, &rd, host))?;
    records.step(host, "cfw.tree-ssh", || {
        let artifacts = SshArtifacts {
            ramdisk: &rd.output(),
            stock_trustcache: &rd.stock_trustcache(),
            staging: &rd.staging(),
        };
        let args: Vec<String> = vec!["fw".into(), "patch".into(), "-l".into(), library.clone(), params.name.into(), "--restore-txm-precheck".into()];
        manifest::set_ssh_mode(&vm, &artifacts, &mut || cli_ok(&args, &[], None), host)
    })?;
    let report = if records.is_done("cfw.installed") {
        InstallReport::default()
    } else {
        nvram::set_file(&vm.join("nvram.bin"), &NvramEdit::auto_boot(false))?;
        host.stop_vm(&vm).map_err(Error::msg)?;
        let run = vm.join(".runs-sshrd");
        fs::create_dir_all(&run).map_err(|e| Error::path(&run, e))?;
        host.start_vm(VmKind::Ramdisk, &vm, &run).map_err(Error::msg)?;
        let ecid = format!("0x{:016x}", params.ecid);
        cli_ok(&["recovery-probe".into(), "--ecid".into(), ecid.clone(), "--timeout".into(), "180".into()], &[], None)?;
        // --boot-ramdisk ends by waiting for restore mode, which the SSH ramdisk
        // never enters: it runs in the background until SSH answers.
        let boot_log = run.join("boot-ramdisk.log");
        let args: Vec<String> = vec![
            "restore".into(), "-l".into(), library.clone(), params.name.into(),
            "--ecid".into(), ecid, "--boot-ramdisk".into(), "-v".into(),
        ];
        let env = [
            ("VPHONE_RESTORE_TIMEOUT_SCALE".to_owned(), "10".to_owned()),
            ("VPHONE_RESTORE_BOOT_ARGS_EXTRA".to_owned(), "serial=3".to_owned()),
        ];
        let helper = host.spawn_helper(CLI, &args, &env, &boot_log).map_err(Error::msg)?;
        host.log(&format!("waiting for the SSH ramdisk (log: {})", boot_log.display()));
        let ssh = ssh_wait(host, &boot_log);
        helper.stop();
        let ssh = ssh?;
        ssh.exec(GIGALOCKER_SH, Some(Duration::from_secs(300)))?.check("gigalocker")?;
        ssh.exec(MOUNT_SH, Some(Duration::from_secs(1800)))?.check("mount_filesystems")?;
        let (sftp, raw) = ssh.sftp()?;
        let context = InstallContext {
            host,
            vm: &vm,
            release: params.release,
            cli: CLI,
            work: &vm.join(".cfw-work"),
            log: params.log,
            ssh: &ssh,
            system: GuestVolume::new(&ssh, &sftp, &raw, "/mnt1"),
            xart: GuestVolume::new(&ssh, &sftp, &raw, "/mnt7"),
            preboot: GuestVolume::new(&ssh, &sftp, &raw, "/mnt6"),
        };
        let report = cfw::install(&context, &mut records)?;
        cfw::record_guest_receipt(&vm, &report.live, &report.not_applicable)?;
        for (name, count) in &report.xattrs_not_carried {
            host.log(&format!("[!] xattr {name}: {count} files; not carried (neither tar 1.17 nor sftp-server can set them)"));
        }
        // xART at /mnt7 stays busy; sync is enough for it.
        let _ = ssh.exec(UNMOUNT_SH, Some(Duration::from_secs(600)));
        records.mark("cfw.installed")?;
        report
    };
    host.stop_vm(&vm).map_err(Error::msg)?;
    records.step(host, "cfw.flip", || {
        cli_ok(&["fw".into(), "snapshot-flip".into(), "-l".into(), library.clone(), params.name.into()], &[], None)
    })?;
    nvram::set_file(&vm.join("nvram.bin"), &NvramEdit::auto_boot(true))?;
    records.step(host, "cfw.tree-full", || manifest::set_full_mode(&vm, host))?;
    display::patch_file(&vm.join("nvram.bin"), if params.scale == 3 { 0 } else { params.scale })?;
    let _ = fs::remove_dir_all(vm.join(".cfw-work"));
    Ok(report)
}

/// Inputs of `first_boot`.
pub struct FirstBoot<'a> {
    pub host: &'a dyn Host,
    pub vm: &'a Path,
    pub scale: u32,
    pub gfx: bool,
}

fn wait_vphoned(host: &dyn Host) -> Result<()> {
    loop {
        check_cancel(host)?;
        if !host.vm_alive() {
            bail!("QEMU exited; see {}", host.serial_log().display());
        }
        if host.vphoned_rpc("setup.status", json!({}), Duration::from_secs(150)).is_ok() {
            return Ok(());
        }
        cancellable_sleep(host, Duration::from_secs(10))?;
    }
}

/// `do_first_boot`: first boot, data migration, Setup Assistant, unlock,
/// Auto-Lock Never, and the display scale's guest half with its restart and 1x logo.
pub fn first_boot(params: &FirstBoot) -> Result<()> {
    let host = params.host;
    let run = params.vm.join(".runs-first");
    fs::create_dir_all(&run).map_err(|e| Error::path(&run, e))?;
    host.log("the first boot migrates iOS's data behind an Apple logo and progress bar (20+ min)");
    host.start_vm(VmKind::FirstBoot { gfx: params.gfx }, params.vm, &run).map_err(Error::msg)?;
    wait_vphoned(host)?;
    while !guest::data_migration_done(host) {
        if !host.vm_alive() {
            bail!("QEMU exited during the first boot; see {}", host.serial_log().display());
        }
        cancellable_sleep(host, Duration::from_secs(30))?;
    }
    guest::skip_setup(host)?;
    if !guest::unlock(host) {
        host.log("the screen did not unlock");
    }
    guest::auto_lock_never(host)?;
    if params.scale == 3 {
        return Ok(());
    }
    // iOS 27 also takes the scale from the guest's device tree.
    if !guest::guest_dt_scale(host, params.scale)? {
        host.log("no display-scale in the guest tree: the LLB patch alone sets this guest's scale");
        guest::clear_scale_cache(host);
    }
    host.log("restarting the guest so the changes apply");
    let _ = host.vphoned_rpc("system.shutdown", json!({"force": true}), Duration::from_secs(60));
    while host.vm_alive() {
        cancellable_sleep(host, Duration::from_secs(2))?;
    }
    host.start_vm(VmKind::FirstBoot { gfx: params.gfx }, params.vm, &run).map_err(Error::msg)?;
    wait_vphoned(host)?;
    if !guest::unlock(host) {
        host.log("the screen did not unlock");
    }
    if params.scale == 1 {
        let wrote = guest::write_1x_logos(host)?;
        host.log(&format!("wrote {wrote} 1x logo(s); the logo shows from the next boot"));
    }
    Ok(())
}
