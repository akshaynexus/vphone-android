//! One VM's settings on this phone (`<vm>/android.json`), and the defaults for the phone's size.

use crate::meminfo::MemInfo;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The largest code cache the desktop launcher uses; more would only take a phone's RAM.
pub const MAX_TB_MIB: u32 = 2048;
pub const MIN_TB_MIB: u32 = 64;
pub const MIN_RAM_MIB: u32 = 1024;
const FILE: &str = "android.json";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VmConfig {
    pub vcpus: u32,
    pub ram_mib: u32,
    pub tb_mib: u32,
    /// Report the vCPU threads' load to the system's Performance Hint service.
    pub performance_hints: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self::for_phone(MemInfo::read().total_mib, online_cpus())
    }
}

pub fn online_cpus() -> u32 {
    std::thread::available_parallelism().map_or(8, |n| n.get() as u32)
}

impl VmConfig {
    /// A "12 GB" phone reports about 11 GiB, so 14 GiB separates it from a 16 GB one.
    pub fn for_phone(total_mib: u64, cpus: u32) -> VmConfig {
        let (ram_mib, tb_mib) = match total_mib {
            t if t >= 22 * 1024 => (8192, MAX_TB_MIB),
            t if t >= 14 * 1024 => (6144, MAX_TB_MIB),
            _ => (4096, 1024),
        };
        VmConfig {
            vcpus: 6.min(cpus.max(1)),
            ram_mib,
            tb_mib,
            performance_hints: true,
        }
    }

    pub fn load(vm: &Path) -> VmConfig {
        std::fs::read(vm.join(FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, vm: &Path) -> Result<(), String> {
        self.validate(online_cpus(), MemInfo::read().total_mib)?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(vm.join(FILE), json).map_err(|e| format!("cannot save the settings: {e}"))
    }

    pub fn validate(&self, cpus: u32, total_mib: u64) -> Result<(), String> {
        if self.vcpus == 0 || self.vcpus > cpus {
            return Err(format!("vCPUs must be 1 to {cpus} on this phone"));
        }
        if self.ram_mib < MIN_RAM_MIB || u64::from(self.ram_mib) >= total_mib {
            return Err(format!(
                "RAM must be at least {MIN_RAM_MIB} MiB and below the phone's {total_mib} MiB"
            ));
        }
        if !(MIN_TB_MIB..=MAX_TB_MIB).contains(&self.tb_mib) {
            return Err(format!(
                "the code cache must be {MIN_TB_MIB} to {MAX_TB_MIB} MiB"
            ));
        }
        Ok(())
    }

    /// RAM in the form QEMU's `-m` and `size=` take.
    pub fn ram_arg(&self) -> String {
        if self.ram_mib.is_multiple_of(1024) {
            format!("{}G", self.ram_mib / 1024)
        } else {
            format!("{}M", self.ram_mib)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_phone() {
        let c = VmConfig::for_phone(11 * 1024, 8);
        assert_eq!((c.vcpus, c.ram_mib, c.tb_mib), (6, 4096, 1024));
        let c = VmConfig::for_phone(15 * 1024, 8);
        assert_eq!((c.ram_mib, c.tb_mib), (6144, 2048));
    }

    #[test]
    fn validation_and_ram_form() {
        let c = VmConfig {
            vcpus: 9,
            ram_mib: 4096,
            tb_mib: 1024,
            performance_hints: true,
        };
        assert!(c.validate(8, 11000).is_err());
        assert_eq!(VmConfig { vcpus: 6, ..c }.ram_arg(), "4G");
        assert_eq!(VmConfig { ram_mib: 5000, ..c }.ram_arg(), "5000M");
    }
}
