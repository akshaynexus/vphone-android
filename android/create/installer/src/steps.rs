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
/// The whole of it, which the driver read straight from that cloudOS's image must have.
pub const KNOWN_GPU_SHA1: &str = "b5d95b3886dfcfdeb45de7d60124697bd4916109";
/// The decrypted system image while the driver is read from it; removed when the step ends.
/// The cloudOS build that hash belongs to.
pub const KNOWN_CLOUD_BUILD: &str = "23E5207q";
const GPU_IMAGE: &str = ".gpu-driver-system.img";

const GIGALOCKER_SH: &str = r#"x=; for n in 1 2 3 4 5 6 7; do t=$(/System/Library/Filesystems/apfs.fs/apfs.util -p /dev/disk1s$n 2>/dev/null); [ "$t" = xART ] && x=/dev/disk1s$n; done
[ -n "$x" ] || x=/dev/disk1s3
/sbin/mount_apfs "$x" /mnt7 2>/dev/null
if [ -f /mnt7/AA.gl ]; then
  out=$(/usr/libexec/seputil --gigalocker-init /mnt7 2>&1)
  case $out in *"doesn't exist"*) want=${out#*Gigalocker file (}; want=${want%%)*}; mv /mnt7/AA.gl "$want" && echo "gigalocker: AA.gl -> $want";; esac
fi
true"#;
const MOUNT_SH: &str = "/usr/bin/mount_filesystems; /sbin/mount";
const UNMOUNT_SH: &str = "sync; /sbin/umount /mnt2; /sbin/umount /mnt6; /sbin/umount /mnt1; sync";

/// tools/restore-wait.sh: how long the tool may go on after the device said the restore finished,
/// when neither it nor the device gives any other sign of being done.
const RESTORE_EXIT_GRACE: Duration = Duration::from_secs(120);
const RESTORE_POLL: Duration = Duration::from_secs(1);
/// How long usbmuxd has to name an attached device, which it does as soon as it is asked.
const USBMUX_ANSWER: Duration = Duration::from_secs(2);
/// A ramdisk boots slowly on a phone; this is what the wait came to at its old pace. Time the
/// person has the creation paused does not count.
const SSH_WAIT_LIMIT: Duration = Duration::from_secs(60 * 60);
/// How many connections a copy of gigabytes to the guest uses (measured through the ramdisk's
/// dropbear on a PC: 133 MB/s over one, 236 over two, 412 over four).
const COPY_STREAMS: usize = 3;
/// Between attempts that failed straight away; one still waiting blocks in `connect` instead.
const SSH_RETRY: Duration = Duration::from_secs(1);

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

fn zip_err(path: &Path) -> impl Fn(zip::result::ZipError) -> Error + '_ {
    move |e| Error::msg(format!("{}: {e}", path.display()))
}

/// The member of a cloudOS IPSW that is its system volume's image (the manifest's `OS`), and
/// the cloudOS build.
fn cloud_os_member(ipsw: &Path, archive: &mut zip::ZipArchive<File>) -> Result<(String, String)> {
    let mut bytes = Vec::new();
    archive
        .by_name("BuildManifest.plist")
        .map_err(zip_err(ipsw))?
        .read_to_end(&mut bytes)
        .map_err(|e| Error::path(ipsw, e))?;
    let manifest = Value::from_reader(std::io::Cursor::new(bytes))?;
    let mut members = std::collections::BTreeSet::new();
    let identities = manifest.as_dictionary().and_then(|d| d.get("BuildIdentities")).and_then(Value::as_array);
    for identity in identities.into_iter().flatten() {
        let path = identity
            .as_dictionary()
            .and_then(|d| d.get("Manifest"))
            .and_then(Value::as_dictionary)
            .and_then(|d| d.get("OS"))
            .and_then(Value::as_dictionary)
            .and_then(|d| d.get("Info"))
            .and_then(Value::as_dictionary)
            .and_then(|d| d.get("Path"))
            .and_then(Value::as_string);
        if let Some(path) = path {
            members.insert(path.to_owned());
        }
    }
    let build = manifest
        .as_dictionary()
        .and_then(|d| d.get("ProductBuildVersion"))
        .and_then(Value::as_string)
        .unwrap_or_default()
        .to_owned();
    let mut members = members.into_iter();
    match (members.next(), members.next()) {
        (Some(one), None) => Ok((one, build)),
        (None, _) => bail!("{}: its BuildManifest names no OS image", ipsw.display()),
        (Some(a), Some(b)) => bail!("{}: its BuildManifest names more than one OS image ({a}, {b})", ipsw.display()),
    }
}

/// Removes the decrypted image however the step ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// How the driver was read from the image, for the log and for measuring.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuTimings {
    pub decrypt: Duration,
    pub extract: Duration,
    pub threads: usize,
    pub image_bytes: u64,
    pub written_bytes: u64,
}

/// The driver read from the cloudOS IPSW itself: its system image is an encrypted archive that
/// carries its own key, so it is decrypted on `threads` threads into `out_dir` and the bundle is
/// read from the APFS volume inside. `None` when the image has no key in its header; then only a
/// restored cloudOS has the driver (`gpu_driver`).
///
/// The decrypted image exists only in `out_dir`, and only until this returns. The key is used
/// and dropped here; nothing logs it.
pub fn gpu_driver_from_image(
    cloud_ipsw: &Path,
    out_dir: &Path,
    threads: usize,
    progress: &dyn Progress,
) -> Result<Option<(GpuDriver, GpuTimings)>> {
    fs::create_dir_all(out_dir).map_err(|e| Error::path(out_dir, e))?;
    let scratch = Scratch(out_dir.join(GPU_IMAGE));
    let _ = fs::remove_file(&scratch.0);
    let file = File::open(cloud_ipsw).map_err(|e| Error::path(cloud_ipsw, e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err(cloud_ipsw))?;
    let (member, build) = cloud_os_member(cloud_ipsw, &mut archive)?;
    let mut entry = archive.by_name(&member).map_err(zip_err(cloud_ipsw))?;
    let archive_size = entry.size();
    let prologue = crate::aea::Prologue::read(&mut entry).map_err(|e| Error::msg(format!("{member}: {e}")))?;
    let Some(key) = prologue.embedded_key() else {
        return Ok(None);
    };
    progress.log(&format!(
        "=== Decrypting the cloudOS system image ({member}, {} MB) on {threads} threads",
        archive_size >> 20
    ));
    let image = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&scratch.0)
        .map_err(|e| Error::path(&scratch.0, e))?;
    let began = Instant::now();
    let done = crate::aea::decrypt(
        &prologue,
        &mut entry,
        archive_size,
        &key,
        &image,
        threads,
        progress,
        "Decrypting the cloudOS system image",
    )
    .map_err(|e| match e {
        Error::Cancelled => Error::Cancelled,
        e => Error::msg(format!("{member}: {e}")),
    })?;
    drop(image);
    let decrypt = began.elapsed();
    progress.log(&format!(
        "decrypted {} MB ({} MB of it data) in {:.1} s",
        done.size >> 20,
        done.written >> 20,
        decrypt.as_secs_f64()
    ));
    progress.log("=== Reading the GPU driver from the system image");
    progress.progress(0, 0, "Reading the GPU driver from the system image");
    let began = Instant::now();
    let driver = extract_gpu_bundle(&scratch.0, out_dir, progress)?;
    // The tested cloudOS has one driver; anything else from its image is a wrong read.
    if build == KNOWN_CLOUD_BUILD && driver.sha1 != KNOWN_GPU_SHA1 {
        let _ = fs::remove_dir_all(&driver.bundle);
        bail!(
            "the GPU driver read from cloudOS {build} has SHA-1 {}, not {KNOWN_GPU_SHA1}: the image or the read is damaged",
            driver.sha1
        );
    }
    let timings = GpuTimings {
        decrypt,
        extract: began.elapsed(),
        threads,
        image_bytes: done.size,
        written_bytes: done.written,
    };
    drop(scratch);
    Ok(Some((driver, timings)))
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
/// VM and read the driver from its System volume. Only for a cloudOS whose image
/// `gpu_driver_from_image` cannot open: it is a whole restore.
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
    let probe = strings(&["recovery-probe", "--timeout", "60"]);
    let mut in_dfu = false;
    for _ in 0..10 {
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
        bail!("the temporary VM did not reach DFU within 10 minutes\n{}", host.diagnostics());
    }
    let log = run.join("restore.log");
    host.log("=== restoring the stock cloudOS");
    let library = params.library.to_string_lossy().into_owned();
    let args = vec!["restore".to_owned(), "-l".to_owned(), library, name, "-v".to_owned()];
    let env = [("VPHONE_RESTORE_TIMEOUT_SCALE".to_owned(), "10".to_owned())];
    let mut helper = host.spawn_helper(CLI, &args, &env, &log).map_err(Error::msg)?;
    wait_restore(host, helper.as_mut(), &log, RESTORE_EXIT_GRACE)?;
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

/// Whether usbmuxd says no device is attached. An error reaching it says nothing either way, so
/// it counts as still there.
pub fn device_left_usb(usbmux_socket: &Path) -> bool {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    matches!(
        vusb_portfwd::wait_for_device(usbmux_socket, USBMUX_ANSWER),
        Err(e) if matches!(e.kind(), TimedOut | WouldBlock)
    )
}

fn tail_of(log: &Path) -> String {
    let tail = fs::read(log).unwrap_or_default();
    String::from_utf8_lossy(&tail[tail.len().saturating_sub(3000)..]).trim().to_owned()
}

/// tools/restore-wait.sh: the tool can wait forever after a finished restore, so once the device
/// has said it finished, the wait ends when the tool exits, the VM stops or the device leaves USB,
/// and after `grace` if none of those happens. A VM that stops before then has failed the restore.
fn wait_restore(host: &dyn Host, helper: &mut dyn BackgroundHelper, log: &Path, grace: Duration) -> Result<()> {
    let mut offset = 0u64;
    let mut finished_at: Option<Instant> = None;
    loop {
        if let Some(code) = helper.finished() {
            if code != 0 && finished_at.is_none() {
                bail!("restore exited with status {code}\nIts last output:\n{}\n\n{}", tail_of(log), host.diagnostics());
            }
            return Ok(());
        }
        if finished_at.is_none() {
            if let Ok(mut f) = File::open(log) {
                // A little of what was read before, for a line split across two reads.
                let start = offset.saturating_sub(64);
                if f.seek(SeekFrom::Start(start)).is_ok() {
                    let mut chunk = Vec::new();
                    let _ = f.read_to_end(&mut chunk);
                    offset = start + chunk.len() as u64;
                    if String::from_utf8_lossy(&chunk).to_ascii_lowercase().contains("restore finished") {
                        finished_at = Some(Instant::now());
                        host.log(&format!("the device reports the restore finished; waiting up to {} s for the tool", grace.as_secs()));
                    }
                }
            }
        }
        // After the log, so a VM that went away once the restore finished reads as just that.
        let vm_alive = host.vm_alive();
        match finished_at {
            None if !vm_alive => {
                bail!("the VM stopped during the restore\nIts last output:\n{}\n\n{}", tail_of(log), host.diagnostics());
            }
            None => {}
            Some(_) if !vm_alive => {
                host.log("the VM stopped after the restore finished; not waiting for the tool");
                return Ok(());
            }
            Some(_) if device_left_usb(&host.usbmux_socket()) => {
                host.log("the device has left USB after the restore finished; not waiting for the tool");
                return Ok(());
            }
            Some(t) if t.elapsed() >= grace => {
                host.log("the tool is still waiting for the device to come back; stopping it (the restore itself finished)");
                return Ok(());
            }
            Some(_) => {}
        }
        cancellable_sleep(host, RESTORE_POLL)?;
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

/// A stream to the guest's SSH port. With the VM's ECID, only its own device is taken: one whose
/// serial carries that ECID, or the sole device when its serial carries none (the SSH ramdisk).
fn connect_ssh(socket: &Path, timeout: Duration, ecid: Option<u64>) -> Result<std::os::unix::net::UnixStream> {
    match ecid {
        Some(ecid) => vusb_portfwd::connect_ecid(socket, 22, timeout, ecid),
        None => vusb_portfwd::connect(socket, 22, timeout),
    }
    .map_err(|e| Error::Ssh(format!("usbmux: {e}")))
}

/// Until the ramdisk's SSH answers: `connect` blocks until the device's port accepts, so nothing
/// here sleeps but the short gap after an attempt that failed straight away.
fn ssh_wait(host: &dyn Host, log: &Path, ecid: Option<u64>) -> Result<GuestSsh> {
    const NOTE_EVERY: Duration = Duration::from_secs(5 * 60);
    let socket = host.usbmux_socket();
    let mut last = String::new();
    let mut spent = Duration::ZERO;
    let mut noted = Duration::ZERO;
    while spent < SSH_WAIT_LIMIT {
        check_cancel(host)?;
        if !host.vm_alive() {
            bail!("the VM stopped while waiting for the SSH ramdisk\n{}", host.diagnostics());
        }
        let began = crate::active::ActiveInstant::now();
        let attempt = (|| -> Result<GuestSsh> {
            let stream = connect_ssh(&socket, Duration::from_secs(30), ecid)?;
            let ssh = GuestSsh::connect(Connect::ramdisk(Target::Connected(stream)))?;
            ssh.exec("id", Some(Duration::from_secs(30)))?.check("id")?;
            Ok(ssh)
        })();
        match attempt {
            Ok(ssh) => return Ok(ssh),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => last = e.to_string(),
        }
        // On the active clock, and before the sleep, which a pause holds: paused time is not spent.
        spent += began.elapsed() + SSH_RETRY;
        if spent >= noted + NOTE_EVERY {
            noted = spent;
            host.log(&format!("SSH ramdisk not answering yet ({last}); see {}", log.display()));
        }
        cancellable_sleep(host, SSH_RETRY)?;
    }
    bail!("the SSH ramdisk did not answer in {} min: {last}", SSH_WAIT_LIMIT.as_secs() / 60)
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
        cli_ok(&["recovery-probe".into(), "--ecid".into(), ecid.clone(), "--timeout".into(), "600".into()], &[], None)?;
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
        let ssh = ssh_wait(host, &boot_log, Some(params.ecid));
        helper.stop();
        let ssh = ssh?;
        ssh.exec(GIGALOCKER_SH, Some(Duration::from_secs(300)))?.check("gigalocker")?;
        ssh.exec(MOUNT_SH, Some(Duration::from_secs(1800)))?.check("mount_filesystems")?;
        let (sftp, raw) = ssh.sftp()?;
        // The copies of gigabytes go over a few connections: the guest's SSH server is one
        // process a connection, and the slowest part of the path. Three leave the guest's other
        // vCPUs to its tar and its filesystem.
        let mux = host.usbmux_socket();
        let ecid_value = params.ecid;
        let open = move || -> Result<GuestSsh> {
            let stream = connect_ssh(&mux, Duration::from_secs(60), Some(ecid_value))?;
            GuestSsh::connect(Connect::ramdisk(Target::Connected(stream)))
        };
        let context = InstallContext {
            more: Some(crate::tree::MoreLinks { open: &open, streams: COPY_STREAMS }),
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

#[cfg(test)]
mod tests {
    #[test]
    fn the_scripts_sent_to_the_guest_parse() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for script in [GIGALOCKER_SH, MOUNT_SH, UNMOUNT_SH] {
            let mut sh = Command::new("/bin/sh")
                .arg("-n")
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            sh.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
            let out = sh.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
    }

    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct FakeHost {
        alive: AtomicBool,
        lines: Mutex<Vec<String>>,
        usbmux: PathBuf,
    }

    impl FakeHost {
        fn new(alive: bool, usbmux: PathBuf) -> FakeHost {
            FakeHost { alive: AtomicBool::new(alive), lines: Mutex::default(), usbmux }
        }
        fn said(&self, text: &str) -> bool {
            self.lines.lock().unwrap().iter().any(|l| l.contains(text))
        }
    }

    impl Progress for FakeHost {
        fn log(&self, line: &str) {
            self.lines.lock().unwrap().push(line.to_owned());
        }
        fn progress(&self, _: u64, _: u64, _: &str) {}
    }

    impl Host for FakeHost {
        fn start_vm(&self, _: VmKind, _: &Path, _: &Path) -> std::result::Result<(), String> {
            Ok(())
        }
        fn stop_vm(&self, _: &Path) -> std::result::Result<(), String> {
            Ok(())
        }
        fn vm_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
        fn serial_log(&self) -> PathBuf {
            PathBuf::new()
        }
        fn diagnostics(&self) -> String {
            "HOW THE VM ENDED".to_owned()
        }
        fn vphoned_rpc(&self, _: &str, _: serde_json::Value, _: Duration) -> std::result::Result<serde_json::Value, String> {
            Err("no guest".to_owned())
        }
        fn run_helper(&self, _: &str, _: &[String], _: &[(String, String)], _: &Path, _: Option<Duration>) -> std::result::Result<i32, String> {
            Ok(0)
        }
        fn spawn_helper(&self, _: &str, _: &[String], _: &[(String, String)], _: &Path) -> std::result::Result<Box<dyn BackgroundHelper>, String> {
            Err("not used".to_owned())
        }
        fn usbmux_socket(&self) -> PathBuf {
            self.usbmux.clone()
        }
    }

    /// A tool that is still running, or has ended with this status.
    struct Tool(Option<i32>);

    impl BackgroundHelper for Tool {
        fn finished(&mut self) -> Option<i32> {
            self.0
        }
        fn stop(self: Box<Self>) {}
    }

    /// usbmuxd's side of LISTEN, naming `device` or nothing, for every client.
    fn fake_usbmuxd(path: &Path, device: bool) {
        fn message(kind: u32, body: &[u8]) -> Vec<u8> {
            let mut m = Vec::new();
            m.extend_from_slice(&(16 + body.len() as u32).to_le_bytes());
            m.extend_from_slice(&0u32.to_le_bytes());
            m.extend_from_slice(&kind.to_le_bytes());
            m.extend_from_slice(&1u32.to_le_bytes());
            m.extend_from_slice(body);
            m
        }
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for client in listener.incoming() {
                let mut client = client.unwrap();
                let mut header = [0u8; 16];
                client.read_exact(&mut header).unwrap();
                client.write_all(&message(1, &0u32.to_le_bytes())).unwrap();
                if device {
                    client.write_all(&message(4, &[0u8; 268])).unwrap();
                }
                held.push(client);
            }
        });
    }

    fn setup(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("restore.log");
        fs::write(&log, text).unwrap();
        (dir, log)
    }

    const GRACE: Duration = Duration::from_secs(60);
    const FINISHED: &str = "Status: Restore Finished\n";

    #[test]
    fn a_vm_that_dies_during_the_restore_fails_it_at_once() {
        let (dir, log) = setup("Sending filesystem now...\nthe tools last words\n");
        let host = FakeHost::new(false, dir.path().join("no-usbmux"));
        let start = Instant::now();
        let err = wait_restore(&host, &mut Tool(None), &log, GRACE).unwrap_err().to_string();
        assert!(start.elapsed() < Duration::from_secs(5), "it did not wait");
        assert!(err.contains("the VM stopped during the restore"), "{err}");
        assert!(err.contains("the tools last words"), "{err}");
        assert!(err.contains("HOW THE VM ENDED"), "{err}");
    }

    #[test]
    fn a_vm_stopping_after_restore_finished_is_not_a_failure() {
        let (dir, log) = setup(FINISHED);
        let host = FakeHost::new(false, dir.path().join("no-usbmux"));
        let start = Instant::now();
        wait_restore(&host, &mut Tool(None), &log, GRACE).unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "it did not wait");
        assert!(host.said("the VM stopped after the restore finished"));
    }

    #[test]
    fn a_tool_that_fails_before_the_restore_finished_reports_its_status() {
        let (dir, log) = setup("half way\n");
        let host = FakeHost::new(false, dir.path().join("no-usbmux"));
        let err = wait_restore(&host, &mut Tool(Some(137)), &log, GRACE).unwrap_err().to_string();
        assert!(err.contains("restore exited with status 137"), "{err}");
        assert!(err.contains("HOW THE VM ENDED"), "{err}");
    }

    #[test]
    fn the_device_leaving_usb_ends_the_wait() {
        let (dir, log) = setup(FINISHED);
        let socket = dir.path().join("usbmux");
        fake_usbmuxd(&socket, false);
        let host = FakeHost::new(true, socket);
        let start = Instant::now();
        wait_restore(&host, &mut Tool(None), &log, GRACE).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10), "the grace was not waited out");
        assert!(host.said("has left USB"));
    }

    #[test]
    fn a_tool_still_waiting_on_an_attached_device_is_stopped_after_the_grace() {
        let (dir, log) = setup(FINISHED);
        let socket = dir.path().join("usbmux");
        fake_usbmuxd(&socket, true);
        let host = FakeHost::new(true, socket);
        wait_restore(&host, &mut Tool(None), &log, Duration::from_millis(200)).unwrap();
        assert!(host.said("still waiting for the device to come back"));
    }

    #[test]
    fn a_finished_line_written_during_the_wait_is_seen() {
        let (dir, log) = setup("");
        let host = Arc::new(FakeHost::new(true, dir.path().join("no-usbmux")));
        let (writer, path) = (host.clone(), log.clone());
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1200));
            fs::write(&path, FINISHED).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            writer.alive.store(false, Ordering::SeqCst);
        });
        wait_restore(host.as_ref(), &mut Tool(None), &log, GRACE).unwrap();
        thread.join().unwrap();
        assert!(host.said("the device reports the restore finished"));
    }

    #[test]
    fn device_left_usb_needs_usbmuxd_to_say_so() {
        let dir = tempfile::tempdir().unwrap();
        let (none, some) = (dir.path().join("none"), dir.path().join("some"));
        fake_usbmuxd(&none, false);
        fake_usbmuxd(&some, true);
        assert!(device_left_usb(&none));
        assert!(!device_left_usb(&some));
        assert!(!device_left_usb(&dir.path().join("absent")), "no usbmuxd says nothing about the device");
    }

    #[test]
    fn the_ssh_wait_ends_when_the_vm_has_stopped() {
        let (dir, log) = setup("");
        let host = FakeHost::new(false, dir.path().join("no-usbmux"));
        let start = Instant::now();
        let err = ssh_wait(&host, &log, None).err().expect("no VM, no SSH").to_string();
        assert!(start.elapsed() < Duration::from_secs(5), "it did not wait");
        assert!(err.contains("the VM stopped while waiting for the SSH ramdisk"), "{err}");
        assert!(err.contains("HOW THE VM ENDED"), "{err}");
    }
}
