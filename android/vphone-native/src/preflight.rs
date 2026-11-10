//! What is checked before a VM boots, with messages a person can act on.

use crate::bundle::Bundle;
use crate::config::VmConfig;
use crate::meminfo::MemInfo;
use crate::paths::Paths;
use serde::Serialize;
use std::path::Path;

pub const REQUIRED_LIBS: [&str; 5] = [
    "libqemu-vphone.so",
    "libvulkan_freedreno.so",
    "libadrenotools.so",
    "libhook_impl.so",
    "libmain_hook.so",
];

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Guest RAM has to fit; the code cache and the GPU's memory only ought to.
pub fn check_memory(cfg: &VmConfig, mem: MemInfo, report: &mut Report) {
    let usable = mem.usable_mib();
    let guest = u64::from(cfg.ram_mib);
    if usable < guest + 512 {
        report.errors.push(format!(
            "Not enough free memory: the VM's {guest} MiB of RAM needs about {} MiB, the phone has {usable} MiB free. Close other apps or lower the VM's RAM.",
            guest + 512
        ));
    } else if usable < guest + u64::from(cfg.tb_mib) + 1536 {
        report.warnings.push(format!(
            "Memory is tight: {usable} MiB free for {guest} MiB of guest RAM, a {} MiB code cache and the GPU. Android may stop the VM when it runs out.",
            cfg.tb_mib
        ));
    }
}

fn writable(path: &Path) -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_ok()
}

pub fn check(
    vm_dir: &Path,
    cfg: &VmConfig,
    paths: &Paths,
    mem: MemInfo,
) -> (Report, Option<Bundle>) {
    let mut report = Report::default();
    let bundle = match Bundle::open(vm_dir) {
        Ok(b) => Some(b),
        Err(problems) => {
            report.errors.extend(problems);
            None
        }
    };
    if let Some(b) = &bundle {
        for f in ["Disk.img", "nvram.bin"]
            .into_iter()
            .chain(b.has_sep_storage.then_some("SEPStorage"))
        {
            if !writable(&b.dir.join(f)) {
                report.errors.push(format!("{f} is not writable by the app. If it came through adb push, import the folder in the app instead."));
            }
        }
    }
    if let Err(e) = cfg.validate(crate::config::online_cpus(), mem.total_mib) {
        report.errors.push(e);
    }
    check_memory(cfg, mem, &mut report);
    for lib in REQUIRED_LIBS {
        if !paths.lib(lib).is_file() {
            report
                .errors
                .push(format!("{lib} is missing from the app's native libraries"));
        }
    }
    (report, bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(available_mib: u64) -> Report {
        let cfg = VmConfig {
            vcpus: 6,
            ram_mib: 4096,
            tb_mib: 1024,
            performance_hints: true,
            display: true,
        };
        let mut r = Report::default();
        check_memory(
            &cfg,
            MemInfo {
                total_mib: 11000,
                available_mib,
                swap_free_mib: 0,
            },
            &mut r,
        );
        r
    }

    #[test]
    fn memory_thresholds() {
        assert!(!run(4000).errors.is_empty());
        let tight = run(5000);
        assert!(tight.errors.is_empty() && !tight.warnings.is_empty());
        assert_eq!(run(8000), Report::default());
    }
}
