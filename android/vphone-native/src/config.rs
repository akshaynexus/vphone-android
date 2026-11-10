//! One VM's settings on this phone (`<vm>/android.json`), and the defaults for the phone's size.

use crate::meminfo::MemInfo;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The largest code cache the desktop launcher uses; more would only take a phone's RAM.
pub const MAX_TB_MIB: u32 = 2048;
pub const MIN_TB_MIB: u32 = 64;
pub const MIN_RAM_MIB: u32 = 1024;
const FILE: &str = "android.json";

/// How Turnip lays out the images of the screen's swapchain, which Android's gralloc allocates
/// and the compositor reads. The driver reads it from `TU_ANDROID_SWAPCHAIN_LAYOUT`
/// (android/patches/mesa/0007).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwapchainLayout {
    /// What the buffer's handle says, and linear where it says nothing.
    #[default]
    Auto,
    Linear,
    /// Compressed (UBWC); the driver then asks gralloc for compressed buffers too.
    Ubwc,
}

impl SwapchainLayout {
    pub fn env_value(self) -> &'static str {
        match self {
            SwapchainLayout::Auto => "auto",
            SwapchainLayout::Linear => "linear",
            SwapchainLayout::Ubwc => "ubwc",
        }
    }
}

/// How hard the phone is asked to run the vCPUs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Performance {
    /// The clocks follow the load: a vCPU must be busier before they rise, and Android's
    /// sustained-performance mode stays on, which holds them below what the phone cannot keep up.
    Balanced,
    /// The highest clocks whenever the guest is busy: more heat and battery. The default, as an
    /// emulated CPU is the limit of everything iOS does here.
    #[default]
    Max,
}

/// The refresh rates a VM's screen can be given.
pub const REFRESH_RATES: [u32; 2] = [60, 120];

/// The settings' version this build writes. A file of an older one has the defaults that
/// changed since applied over it ([`VmConfig::load`]).
pub const VERSION: u32 = 2;

fn unversioned() -> u32 {
    0
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VmConfig {
    /// [`VERSION`] of the build that saved the file; 0 for a file from before it had one.
    #[serde(default = "unversioned")]
    pub version: u32,
    pub performance: Performance,
    /// The refresh rate of the display mode iOS is offered, and the rate Android is asked to
    /// show the window at: one of [`REFRESH_RATES`].
    pub refresh_hz: u32,
    pub vcpus: u32,
    pub ram_mib: u32,
    pub tb_mib: u32,
    /// Report the vCPU threads' load to the system's Performance Hint service.
    pub performance_hints: bool,
    /// Draw the guest's screen in the app's window. Off, the GPU device still runs and draws
    /// nowhere (`REIMS_VGPU_WINDOW=0`), as a setup boot with its display off does.
    pub display: bool,
    /// Let the GPU driver keep its own images compressed (UBWC). Off, Turnip runs with
    /// `TU_DEBUG=noubwc`. The scrambled screen of 0.5.6 and 0.5.7 was the swapchain's layout
    /// (`swapchain_layout`), with this on and off; it stays off by default until a phone has
    /// shown a right picture with it on.
    pub gpu_compression: bool,
    /// Diagnostics for a wrong picture: the driver prints each image's layout
    /// (`TU_DEBUG=layout`), and the surface behind the frame on screen is saved every few
    /// seconds (`REIMS_VGPU_FRAME_DUMP`), for the log screen's Share.
    /// Off by default since 0.5.9: in 0.5.7's log the driver wrote 7,767 layout lines in four
    /// and a half minutes, each through the app's capture of stderr.
    pub gpu_diagnostics: bool,
    pub swapchain_layout: SwapchainLayout,
    /// Draw the screen on the CPU: the GPU device copies the guest's surface into the window's
    /// buffers (`REIMS_VGPU_SOFTWARE_DISPLAY=1`, `ANativeWindow_lock`) and makes no swapchain.
    /// Slower, and right whatever the GPU driver does with a swapchain.
    pub software_display: bool,
    /// Which of iOS's background services run (`crate::services`), applied by the guest's agent
    /// when it has changed, from iOS's next start.
    pub services: crate::services::Level,
    /// Give the VM the internet through QEMU's user-mode network (`crate::net`): a NAT made of
    /// the app's own sockets, IPv4 only. Off, iOS still has its network card, with nothing on
    /// the other end, as every VM had before 0.6.4.
    pub network: bool,
    /// Give the VM a sound device (virtio-snd, which iOS's `AppleVirtIOSound` and the CFW's HAL
    /// plugin drive) that plays on this phone through AAudio. Off, the machine has no sound
    /// device, as every VM had before 0.6.5, and iOS has no audio route.
    pub sound: bool,
    /// "Lighter iOS visuals": no compressed or deep framebuffer for CoreAnimation, Reduce Motion
    /// and Reduce Transparency (`vphone_create_steps::prefs`, its `Visuals` group), written into
    /// the guest after the unlock when missing. Off, the keys this app set are removed.
    pub lighter_visuals: bool,
    /// "Longer app launch limits": FrontBoard's launch limits times four and backboardd's
    /// watchdogs off (the same module's `Watchdogs` group), so that a launch that is slow on
    /// emulated CPUs is not killed and started again.
    pub longer_launch_limits: bool,
    /// "Verbose kernel log": off, NVRAM's `boot-args` keep IOLog and kprintf off the serial line
    /// (`crate::bootargs`); on, the kernel prints everything iBoot's arguments ask for.
    pub verbose_kernel_log: bool,
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
            version: VERSION,
            performance: Performance::Max,
            refresh_hz: 120,
            vcpus: 6.min(cpus.max(1)),
            ram_mib,
            tb_mib,
            performance_hints: true,
            display: true,
            gpu_compression: false,
            gpu_diagnostics: false,
            swapchain_layout: SwapchainLayout::Auto,
            software_display: false,
            services: crate::services::Level::Unchanged,
            network: true,
            sound: true,
            lighter_visuals: true,
            longer_launch_limits: true,
            verbose_kernel_log: false,
        }
    }

    /// Turnip's `TU_DEBUG` for these settings; `None` where they ask for nothing.
    pub fn tu_debug(&self) -> Option<String> {
        let flags: Vec<&str> = [
            (!self.gpu_compression).then_some("noubwc"),
            self.gpu_diagnostics.then_some("layout"),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!flags.is_empty()).then(|| flags.join(","))
    }

    pub fn load(vm: &Path) -> VmConfig {
        std::fs::read(vm.join(FILE))
            .ok()
            .and_then(|b| serde_json::from_slice::<VmConfig>(&b).ok())
            .map(VmConfig::migrated)
            .unwrap_or_default()
    }

    /// A file of an older version with the defaults that changed since: the GPU diagnostics,
    /// which every file saved by 0.5.7 and 0.5.8 has on because they were the default then, go
    /// off. The settings 0.5.9 added are absent from such a file and take their defaults as it is
    /// read.
    pub fn migrated(mut self) -> VmConfig {
        if self.version < 2 {
            self.gpu_diagnostics = false;
        }
        self.version = VERSION;
        self
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
        if !REFRESH_RATES.contains(&self.refresh_hz) {
            return Err("the refresh rate must be 60 or 120 Hz".into());
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
        let c = VmConfig { vcpus: 9, ram_mib: 4096, tb_mib: 1024, ..VmConfig::for_phone(11 * 1024, 8) };
        assert!(c.validate(8, 11000).is_err());
        assert!(VmConfig { vcpus: 6, ..c }.validate(8, 11000).is_ok());
        assert!(VmConfig { vcpus: 6, refresh_hz: 90, ..c }.validate(8, 11000).is_err());
        assert_eq!(VmConfig { vcpus: 6, ..c }.ram_arg(), "4G");
        assert_eq!(VmConfig { ram_mib: 5000, ..c }.ram_arg(), "5000M");
    }

    /// Settings saved before the screen had a switch keep their screen.
    #[test]
    fn a_file_without_the_screen_setting_shows_the_screen() {
        let old: VmConfig = serde_json::from_str(r#"{"vcpus":4,"ramMib":4096,"tbMib":512,"performanceHints":false}"#).unwrap();
        assert!(old.display);
        assert_eq!((old.vcpus, old.performance_hints), (4, false));
        let off: VmConfig = serde_json::from_str(r#"{"display":false}"#).unwrap();
        assert!(!off.display);
        assert!(serde_json::to_string(&off).unwrap().contains(r#""display":false"#));
    }

    /// Settings saved by 0.5.8 and before lose the diagnostics those versions had on by default,
    /// and get the speed settings 0.5.9 added; a file 0.5.9 saved keeps what it says.
    #[test]
    fn an_older_file_gets_the_defaults_that_changed() {
        let dir = std::env::temp_dir().join(format!("vphone-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), r#"{"vcpus":4,"gpuDiagnostics":true,"performanceHints":true}"#).unwrap();
        let old = VmConfig::load(&dir);
        assert_eq!((old.vcpus, old.gpu_diagnostics, old.version), (4, false, VERSION));
        assert_eq!((old.performance, old.refresh_hz), (Performance::Max, 120));
        assert_eq!(old.tu_debug().as_deref(), Some("noubwc"));
        std::fs::write(dir.join(FILE), r#"{"version":2,"gpuDiagnostics":true,"performance":"balanced","refreshHz":60}"#).unwrap();
        let new = VmConfig::load(&dir);
        assert!(new.gpu_diagnostics);
        assert_eq!((new.performance, new.refresh_hz), (Performance::Balanced, 60));
        let json = serde_json::to_string(&new).unwrap();
        assert!(json.contains(r#""performance":"balanced""#) && json.contains(r#""refreshHz":60"#) && json.contains(r#""version":2"#), "{json}");
        std::fs::remove_dir_all(&dir).unwrap();
        // With no file: this build's defaults.
        let fresh = VmConfig::for_phone(15 * 1024, 8);
        assert!(!fresh.gpu_diagnostics && !fresh.gpu_compression);
        assert_eq!((fresh.performance, fresh.refresh_hz, fresh.version), (Performance::Max, 120, VERSION));
    }

    /// Settings saved before the services had a setting leave the VM's services alone.
    #[test]
    fn the_services_setting_defaults_to_unchanged() {
        use crate::services::Level;
        let old: VmConfig = serde_json::from_str(r#"{"vcpus":4,"version":2}"#).unwrap();
        assert_eq!(old.services, Level::Unchanged);
        let set: VmConfig = serde_json::from_str(r#"{"services":"fewest"}"#).unwrap();
        assert_eq!(set.services, Level::Fewest);
        assert!(serde_json::to_string(&VmConfig { services: Level::Trimmed, ..old }).unwrap().contains(r#""services":"trimmed""#));
    }

    /// Settings saved before the network had a setting get the network.
    #[test]
    fn the_network_is_on_unless_a_file_says_otherwise() {
        assert!(VmConfig::for_phone(11 * 1024, 8).network);
        let old: VmConfig = serde_json::from_str(r#"{"vcpus":4,"version":2}"#).unwrap();
        assert!(old.network);
        let off: VmConfig = serde_json::from_str(r#"{"network":false}"#).unwrap();
        assert!(!off.network);
        assert!(serde_json::to_string(&off).unwrap().contains(r#""network":false"#));
    }

    /// Settings saved before sound had a setting get sound.
    #[test]
    fn sound_is_on_unless_a_file_says_otherwise() {
        assert!(VmConfig::for_phone(11 * 1024, 8).sound);
        let old: VmConfig = serde_json::from_str(r#"{"vcpus":4,"version":2,"network":false}"#).unwrap();
        assert!(old.sound);
        let off: VmConfig = serde_json::from_str(r#"{"sound":false}"#).unwrap();
        assert!(!off.sound && off.network);
        assert!(serde_json::to_string(&off).unwrap().contains(r#""sound":false"#));
    }

    /// Settings saved before 0.6.6 get the lighter preferences and the quiet kernel log.
    #[test]
    fn the_guest_tuning_defaults_and_can_be_turned_off() {
        let fresh = VmConfig::for_phone(11 * 1024, 8);
        assert!(fresh.lighter_visuals && fresh.longer_launch_limits && !fresh.verbose_kernel_log);
        let old: VmConfig = serde_json::from_str(r#"{"vcpus":4,"version":2,"sound":false}"#).unwrap();
        assert!(old.lighter_visuals && old.longer_launch_limits && !old.verbose_kernel_log);
        let set: VmConfig = serde_json::from_str(r#"{"lighterVisuals":false,"longerLaunchLimits":false,"verboseKernelLog":true}"#).unwrap();
        assert!(!set.lighter_visuals && !set.longer_launch_limits && set.verbose_kernel_log);
        let json = serde_json::to_string(&set).unwrap();
        assert!(json.contains(r#""lighterVisuals":false"#) && json.contains(r#""longerLaunchLimits":false"#) && json.contains(r#""verboseKernelLog":true"#), "{json}");
    }

    /// The GPU switches name the driver's flags.
    #[test]
    fn gpu_switches_default_and_name_the_drivers_flags() {
        let old: VmConfig = serde_json::from_str(r#"{"vcpus":4,"display":true,"gpuDiagnostics":true}"#).unwrap();
        assert!(!old.gpu_compression && old.gpu_diagnostics);
        assert_eq!(old.tu_debug().as_deref(), Some("noubwc,layout"));
        let set: VmConfig =
            serde_json::from_str(r#"{"gpuCompression":true,"gpuDiagnostics":false}"#).unwrap();
        assert_eq!(set.tu_debug(), None);
        let quiet = VmConfig { gpu_diagnostics: false, ..old };
        assert_eq!(quiet.tu_debug().as_deref(), Some("noubwc"));
        let compressed = VmConfig { gpu_compression: true, ..old };
        assert_eq!(compressed.tu_debug().as_deref(), Some("layout"));
        assert!(serde_json::to_string(&old).unwrap().contains(r#""gpuCompression":false"#));
    }

    /// Settings saved before the swapchain's layout had a setting leave it to the driver.
    #[test]
    fn swapchain_layout_defaults_to_auto_and_names_the_drivers_values() {
        let old: VmConfig = serde_json::from_str(r#"{"vcpus":4,"gpuCompression":false}"#).unwrap();
        assert_eq!(old.swapchain_layout, SwapchainLayout::Auto);
        assert!(!old.software_display);
        let cpu: VmConfig = serde_json::from_str(r#"{"softwareDisplay":true}"#).unwrap();
        assert!(cpu.software_display);
        assert!(serde_json::to_string(&old).unwrap().contains(r#""swapchainLayout":"auto""#));
        for (json, layout, env) in [
            ("auto", SwapchainLayout::Auto, "auto"),
            ("linear", SwapchainLayout::Linear, "linear"),
            ("ubwc", SwapchainLayout::Ubwc, "ubwc"),
        ] {
            let set: VmConfig =
                serde_json::from_str(&format!(r#"{{"swapchainLayout":"{json}"}}"#)).unwrap();
            assert_eq!(set.swapchain_layout, layout);
            assert_eq!(layout.env_value(), env);
        }
        // An unknown value is a file this build cannot read, and falls to the defaults.
        assert!(serde_json::from_str::<VmConfig>(r#"{"swapchainLayout":"tiled"}"#).is_err());
    }
}
