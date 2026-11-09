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
    machine += &gfx_machine(&res);

    let mut args: Vec<String> = vec!["qemu-system-aarch64".into(), "-M".into(), machine];
    let mut push = |parts: &[&str]| args.extend(parts.iter().map(|s| s.to_string()));

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
    ];
    env.extend(gfx_env(paths));
    env.extend([
        ("TMPDIR", dir(paths.cache.join("tmp"))),
        ("HOME", dir(paths.files.join("home"))),
    ]);
    Launch { args, env }
}

/// run-vphone.sh's GFX=1 part of `-M`, for a screen of `res` (`1290x2796`).
pub fn gfx_machine(res: &str) -> String {
    format!(",gfx-device=apple-gfx-mmio,memory-backend=ram0,gfx-modes={res}@60")
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

/// The directories the environment above names, and the vsock socket's.
pub fn runtime_dirs(paths: &Paths) -> Vec<std::path::PathBuf> {
    vec![
        paths.files.join("runs/census"),
        paths.files.join("home"),
        paths.files.join("vsock"),
        paths.cache.join("tmp"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::tests::fixture;
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
        }
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
        assert!(l.env.contains(&("REIMS_VGPU_WINDOW", "1".to_string())));
    }

    #[test]
    fn doubles_commas_in_paths() {
        let dir = fixture("a,b");
        let l = build(&Bundle::open(&dir).unwrap(), &cfg(), &paths());
        assert!(l.args.iter().any(|a| a.contains("a,,b/Disk.img")));
        assert!(!l.args.iter().any(|a| a.contains("a,b/Disk.img")));
    }
}
