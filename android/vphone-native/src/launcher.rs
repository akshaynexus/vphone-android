//! run-vphone.sh for GFX=1 WINDOW=1 on Android: QEMU's command line and environment.

use crate::bundle::Bundle;
use crate::config::VmConfig;
use crate::paths::Paths;

/// One VM runs at a time, so the CID never collides with another.
pub const GUEST_CID: u32 = 3;

#[derive(Debug, PartialEq)]
pub struct Launch {
    pub args: Vec<String>,
    pub env: Vec<(&'static str, String)>,
}

/// An option value's commas, as QEMU's option parser reads them.
fn opt(path: &str) -> String {
    path.replace(',', ",,")
}

pub fn build(b: &Bundle, cfg: &VmConfig, paths: &Paths) -> Launch {
    let res = format!("{}x{}", b.res.0, b.res.1);
    let mem = cfg.ram_arg();
    let file = |name: &str| opt(&b.path(name));

    let mut machine = format!("vresearch101,ecid={},sep-rom={}", b.ecid, file(&b.seprom));
    if let Some(serial) = &b.serial {
        machine += &format!(",serial={serial}");
    }
    machine += &gfx_machine(&res, cfg.refresh_hz);

    let mut args: Vec<String> = vec!["qemu-system-aarch64".into(), "-M".into(), machine];
    let mut push = |parts: &[&str]| args.extend(parts.iter().map(|s| s.to_string()));

    push(&["-name", "vphone,debug-threads=on"]);
    push(&["-accel", &format!("tcg,tb-size={}", cfg.tb_mib)]);
    push(&[
        "-smp",
        &format!("{},maxcpus={}", cfg.vcpus, cfg.vcpus + 1),
        "-m",
        &mem,
    ]);
    push(&["-bios", &b.path(&b.rom)]);
    let nvram = file("nvram.bin");
    let disk = file("Disk.img");
    push(&[
        "-drive",
        &format!("file={nvram},if=pflash,format=raw,offset=16384,file.locking=off"),
    ]);
    push(&[
        "-drive",
        &format!("file={disk},if=pflash,format=raw,file.locking=off"),
    ]);
    push(&[
        "-drive",
        &format!("file={disk},if=none,id=root-pci,format=raw,file.locking=off"),
    ]);
    push(&[
        "-drive",
        &format!("file={nvram},if=none,id=aux-pci,format=raw,offset=16384,file.locking=off"),
    ]);
    push(&["-serial", "mon:stdio"]);
    if b.has_sep_storage {
        let sep = file("SEPStorage");
        push(&[
            "-drive",
            &format!("file={sep},if=none,id=sep-storage,format=raw,file.locking=off"),
        ]);
    }
    let gfx = gfx_args(&res, &mem);
    push(&gfx.iter().map(String::as_str).collect::<Vec<_>>());
    // The virtual network card's boot ROM is not in the app, and iOS does not use it.
    push(&["-global", "virtio-net-pci.romfile="]);
    push(&["-display", "none"]);
    let vsock = opt(&paths.vsock_socket().to_string_lossy());
    push(&[
        "-device",
        &format!("virtio-vsock-uds-pci,guest-cid={GUEST_CID},uds-path={vsock}"),
    ]);
    let qmp = opt(&paths.qmp_socket().to_string_lossy());
    push(&["-qmp", &format!("unix:{qmp},server=on,wait=off")]);
    // Names the vCPU threads "CPU n/TCG", which is how the performance hints find them.
    push(&["-name", "vphone,debug-threads=on"]);

    let dir = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
    let mut env = vec![
        ("VPHONE_TARGET_PAGE_BITS", "14".to_string()),
        ("VPHONE_TLB_RANGE_PAGES", "1".to_string()),
        ("VPHONE_XPAGE_LINKS", "1".to_string()),
        ("VPHONE_RET_PREFETCH", "2".to_string()),
        ("QEMU_TLB_UNSYNCED", "2".to_string()),
        ("VPHONE_XHCI_NONREMOVABLE", "0x1fe".to_string()),
        // The guest's clock reads (iOS's mach_absolute_time is `MRS CNTVCT_EL0` in user code)
        // without QEMU's global lock or the end of their block: on a phone the vCPU threads
        // waited for that lock 25-43 thousand times a second (the 0.6.0 log).
        ("VPHONE_FAST_CNT", "1".to_string()),
    ];
    env.extend(gfx_env(paths).into_iter().map(|(key, value)| {
        // Without the screen the GPU device is the same and draws nowhere.
        if key == "REIMS_VGPU_WINDOW" && !cfg.display {
            (key, "0".to_string())
        } else {
            (key, value)
        }
    }));
    if let Some(flags) = cfg.tu_debug() {
        env.push(("TU_DEBUG", flags));
    }
    env.push(("TU_ANDROID_SWAPCHAIN_LAYOUT", cfg.swapchain_layout.env_value().to_string()));
    if cfg.software_display && cfg.display {
        env.push(("REIMS_VGPU_SOFTWARE_DISPLAY", "1".to_string()));
    }
    // The frames the guest presents are counted, for the `gpu` line of the log; nothing is
    // written by the GPU device itself (`collect`).
    env.push(("REIMS_VGPU_STATS", "collect".to_string()));
    if cfg.gpu_diagnostics && cfg.display {
        env.push(("REIMS_VGPU_FRAME_DUMP", dir(frame_dump_dir(paths))));
    }
    env.extend([
        ("TMPDIR", dir(paths.cache.join("tmp"))),
        ("HOME", dir(paths.files.join("home"))),
    ]);
    Launch { args, env }
}

/// run-vphone.sh's GFX=1 part of `-M`, for a screen of `res` (`1290x2796`) at `refresh_hz`: the one
/// display mode iOS is offered.
pub fn gfx_machine(res: &str, refresh_hz: u32) -> String {
    format!(",gfx-device=apple-gfx-mmio,memory-backend=ram0,gfx-modes={res}@{refresh_hz}")
}

/// run-vphone.sh's GFX=1 arguments: the paravirtual GPU needs guest RAM as a shared mapping.
pub fn gfx_args(res: &str, mem: &str) -> Vec<String> {
    [
        "-object".to_string(),
        format!("memory-backend-memfd,id=ram0,size={mem},share=on"),
        "-global".to_string(),
        format!("apple-gfx-mmio.resolution={res}"),
        "-global".to_string(),
        "apple-gfx-mmio.process-isolation=on".to_string(),
    ]
    .to_vec()
}

/// GPU environment of run-vphone.sh's GFX=1 WINDOW=1.
pub fn gfx_env(paths: &Paths) -> Vec<(&'static str, String)> {
    let dir = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
    vec![
        ("REIMS_VGPU_WINDOW", "1".to_string()),
        ("REIMS_VGPU_WINDOW_TITLE", "vphone-linux".to_string()),
        ("REIMS_VGPU_CENSUS_DIR", dir(paths.files.join("runs/census"))),
        ("XDG_CACHE_HOME", dir(paths.cache.clone())),
    ]
}

/// Where the GPU device saves the surface behind the frame on screen, with the diagnostics on.
pub fn frame_dump_dir(paths: &Paths) -> std::path::PathBuf {
    paths.files.join("runs/frames")
}

/// The directories the environment above names, and the vsock socket's.
pub fn runtime_dirs(paths: &Paths) -> Vec<std::path::PathBuf> {
    vec![
        paths.files.join("runs/census"),
        frame_dump_dir(paths),
        paths.files.join("home"),
        paths.files.join("vsock"),
        paths.cache.join("tmp"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::tests::fixture;
    use crate::config::SwapchainLayout;
    use std::path::PathBuf;

    fn paths() -> Paths {
        Paths {
            files: "/data/files".into(),
            cache: "/data/cache".into(),
            external: "/sdcard/ext".into(),
            native_lib: "/lib".into(),
        }
    }

    fn cfg() -> VmConfig {
        VmConfig {
            vcpus: 6,
            ram_mib: 4096,
            tb_mib: 1024,
            performance_hints: false,
            gpu_diagnostics: true,
            refresh_hz: 60,
            ..VmConfig::for_phone(11 * 1024, 8)
        }
    }

    #[test]
    fn the_refresh_rate_is_the_guests_display_mode() {
        let dir = fixture("refresh");
        let bundle = Bundle::open(&dir).unwrap();
        let fast = build(&bundle, &VmConfig { refresh_hz: 120, ..cfg() }, &paths());
        assert!(fast.args[2].ends_with("@120"), "{}", fast.args[2]);
        assert!(build(&bundle, &cfg(), &paths()).args[2].ends_with("@60"));
        assert!(fast.env.contains(&("REIMS_VGPU_STATS", "collect".to_string())));
        // The defaults: 120 Hz, no driver chatter, no frame dumps.
        let default = build(&bundle, &VmConfig::for_phone(15 * 1024, 8), &paths());
        assert!(default.args[2].ends_with("@120"));
        assert!(default.env.contains(&("TU_DEBUG", "noubwc".to_string())));
        assert!(!default.env.iter().any(|(k, _)| *k == "REIMS_VGPU_FRAME_DUMP"));
    }

    #[test]
    fn matches_the_shell_launcher() {
        let dir: PathBuf = fixture("launch");
        std::fs::write(dir.join("serial"), "ABC123\n").unwrap();
        std::fs::write(dir.join("SEPStorage"), b"x").unwrap();
        std::fs::write(dir.join("gfx-res"), "860x1864\n").unwrap();
        let l = build(&Bundle::open(&dir).unwrap(), &cfg(), &paths());
        let v = dir.to_string_lossy();
        let line = l.args.join(" ");
        assert!(line.contains(&format!("-M vresearch101,ecid=deadbeef,sep-rom={v}/AVPSEPBooter.vresearch1.bin,serial=ABC123,gfx-device=apple-gfx-mmio,memory-backend=ram0,gfx-modes=860x1864@60")));
        assert!(line.contains("-accel tcg,tb-size=1024 -smp 6,maxcpus=7 -m 4G"));
        assert!(line.contains(&format!(
            "-drive file={v}/nvram.bin,if=pflash,format=raw,offset=16384,file.locking=off"
        )));
        assert!(line.contains(&format!(
            "-drive file={v}/SEPStorage,if=none,id=sep-storage,format=raw,file.locking=off"
        )));
        assert!(line.contains("-object memory-backend-memfd,id=ram0,size=4G,share=on"));
        assert!(line.contains("-global apple-gfx-mmio.resolution=860x1864"));
        assert!(line.contains("-display none"));
        assert!(line.contains(
            "-device virtio-vsock-uds-pci,guest-cid=3,uds-path=/data/files/vsock/3.sock"
        ));
        assert!(line.contains("-qmp unix:/data/files/qmp.sock,server=on,wait=off"));
        assert!(l.env.contains(&("QEMU_TLB_UNSYNCED", "2".to_string())));
        assert!(l.env.contains(&("VPHONE_FAST_CNT", "1".to_string())));
        assert!(l.env.contains(&("REIMS_VGPU_WINDOW", "1".to_string())));
    }

    #[test]
    fn a_vm_with_its_screen_off_draws_nowhere() {
        let dir = fixture("no-screen");
        let bundle = Bundle::open(&dir).unwrap();
        let on = build(&bundle, &cfg(), &paths());
        let off = build(&bundle, &VmConfig { display: false, ..cfg() }, &paths());
        assert!(off.env.contains(&("REIMS_VGPU_WINDOW", "0".to_string())));
        assert!(!off.env.contains(&("REIMS_VGPU_WINDOW", "1".to_string())));
        // Nothing else differs: the GPU device and its memory are the same.
        assert_eq!(on.args, off.args);
        // But the frame dumps, which only a window's presenter writes.
        assert_eq!(on.env.len(), off.env.len() + 1);
        assert!(!off.env.iter().any(|(k, _)| *k == "REIMS_VGPU_FRAME_DUMP"));
    }

    #[test]
    fn the_gpu_switches_reach_the_driver_and_the_gpu_device() {
        let dir = fixture("gpu-switches");
        let bundle = Bundle::open(&dir).unwrap();
        let value = |l: &Launch, key: &str| {
            l.env.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
        };
        let default = build(&bundle, &cfg(), &paths());
        assert_eq!(value(&default, "TU_DEBUG").as_deref(), Some("noubwc,layout"));
        assert_eq!(value(&default, "TU_ANDROID_SWAPCHAIN_LAYOUT").as_deref(), Some("auto"));
        let linear = VmConfig { swapchain_layout: SwapchainLayout::Linear, ..cfg() };
        let linear = build(&bundle, &linear, &paths());
        assert_eq!(value(&linear, "TU_ANDROID_SWAPCHAIN_LAYOUT").as_deref(), Some("linear"));
        let ubwc = VmConfig { swapchain_layout: SwapchainLayout::Ubwc, ..cfg() };
        let ubwc = build(&bundle, &ubwc, &paths());
        assert_eq!(value(&ubwc, "TU_ANDROID_SWAPCHAIN_LAYOUT").as_deref(), Some("ubwc"));
        assert_eq!(value(&default, "REIMS_VGPU_SOFTWARE_DISPLAY"), None);
        let cpu = VmConfig { software_display: true, ..cfg() };
        assert_eq!(
            value(&build(&bundle, &cpu, &paths()), "REIMS_VGPU_SOFTWARE_DISPLAY").as_deref(),
            Some("1")
        );
        // With the screen off nothing draws it, on the CPU or otherwise.
        let dark = VmConfig { software_display: true, display: false, ..cfg() };
        assert_eq!(value(&build(&bundle, &dark, &paths()), "REIMS_VGPU_SOFTWARE_DISPLAY"), None);
        assert_eq!(
            value(&default, "REIMS_VGPU_FRAME_DUMP").as_deref(),
            Some("/data/files/runs/frames")
        );
        assert!(runtime_dirs(&paths()).contains(&PathBuf::from("/data/files/runs/frames")));
        let plain = VmConfig { gpu_compression: true, gpu_diagnostics: false, ..cfg() };
        let plain = build(&bundle, &plain, &paths());
        assert_eq!(value(&plain, "TU_DEBUG"), None);
        assert_eq!(value(&plain, "REIMS_VGPU_FRAME_DUMP"), None);
        assert_eq!(default.args, plain.args);
    }

    #[test]
    fn doubles_commas_in_paths() {
        let dir = fixture("a,b");
        let l = build(&Bundle::open(&dir).unwrap(), &cfg(), &paths());
        assert!(l.args.iter().any(|a| a.contains("a,,b/Disk.img")));
        assert!(!l.args.iter().any(|a| a.contains("a,b/Disk.img")));
    }
}
