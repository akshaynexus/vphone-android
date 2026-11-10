//! How much iOS's kernel prints on the serial line, set from the VM's settings before each start.
//!
//! iBoot gives the kernel `serial=3 -v debug=0x2014e` followed by NVRAM's `boot-args`: the first
//! part is patched into the VM's LLB at creation (vphone-cli's `IBootPatcher.bootArgs`, a format
//! string ending in `%s`), so it cannot be changed without making the LLB again. What follows it
//! can: `nvram.bin` is a file of the VM, edited here while the VM is off. That NVRAM's
//! `boot-args` reach the kernel is how a restore's `rd=md0` does (tools/nvram-autoboot.py).
//!
//! With "Verbose kernel log" off, NVRAM gets [`QUIET`]:
//!
//! - `serial=0x13` is `serial=3` (console output and input on the UART) plus XNU's
//!   `SERIALMODE_NO_IOLOG` (0x10), which keeps the drivers' IOLog lines off the UART.
//! - `debug=0x20146` is `debug=0x2014e` without `DB_KPRT` (0x8), the kernel's kprintf on the
//!   UART. Every other bit stays, so a panic is handled as before, and its text still prints:
//!   the console is still the serial line, and `DB_PRT` (0x2, "enable always-on panic print to
//!   serial") is one of the bits kept.
//!
//! Every byte the guest writes to the UART leaves the translated code for QEMU's device, so the
//! less it prints the less the vCPUs stop. How much that is on a phone has not been measured:
//! the status lines carry the serial line's bytes a second for it ([`serial_words`]).
//!
//! Read in XNU's published source (xnu-12377.1.9, apple-oss-distributions `f6217f8`): the flag
//! values (`osfmk/console/serial_protos.h`, `osfmk/kern/debug.h`), and that the later of two
//! `serial=` or `debug=` is the one in force (`pexpert/gen/bootargs.c`: a numeric argument that
//! matched is copied and the scan goes on to the end of the line). Not run on this guest: iOS 27's
//! kernel may differ from that source, and the saving is unmeasured. `-v` is in iBoot's part and
//! stays either way. A kernel that ignored the two prints as before, which the serial figure shows.

use std::path::Path;
use vphone_create_steps::nvram::{self, NvramEdit};

/// What iBoot gives the kernel before NVRAM's `boot-args`.
pub const IBOOT: &str = "serial=3 -v debug=0x2014e";
/// What this app adds after it for a quiet kernel.
pub const QUIET: [&str; 2] = ["serial=0x13", "debug=0x20146"];

/// NVRAM's `boot-args` for the setting, given what is there: this app's two are taken out or
/// put at the end, and anything else stays where it is. `None` is no `boot-args` at all.
pub fn merged(current: Option<&str>, verbose: bool) -> Option<String> {
    let mut args: Vec<&str> = current.unwrap_or("").split_whitespace().filter(|a| !QUIET.contains(a)).collect();
    if !verbose {
        args.extend(QUIET);
    }
    (!args.is_empty()).then(|| args.join(" "))
}

/// Brings the VM's NVRAM to the setting; the line for the log. An NVRAM that cannot be edited
/// is left as it is and said: the VM boots with what it has.
pub fn apply(nvram_bin: &Path, verbose: bool) -> String {
    let done = || -> Result<bool, String> {
        let data = std::fs::read(nvram_bin).map_err(|e| e.to_string())?;
        let current = nvram::get(&data, "boot-args");
        let want = merged(current.as_deref(), verbose);
        if want == current {
            return Ok(false);
        }
        let edit = NvramEdit { auto_boot: nvram::get(&data, "auto-boot").as_deref() != Some("false"), boot_args: want, drop: Vec::new() };
        let mut data = data;
        if !nvram::apply(&mut data, &edit).map_err(|e| e.to_string())? {
            return Ok(false);
        }
        // Beside the file and renamed over it: a process that dies here leaves the old NVRAM whole.
        let new = nvram_bin.with_extension("bin.new");
        let written = std::fs::write(&new, &data).and_then(|()| std::fs::File::open(&new)?.sync_all()).and_then(|()| std::fs::rename(&new, nvram_bin));
        if let Err(why) = written {
            let _ = std::fs::remove_file(&new);
            return Err(why.to_string());
        }
        Ok(true)
    };
    let state = if verbose {
        format!("verbose: the kernel has iBoot's \"{IBOOT}\" alone")
    } else {
        format!(
            "quiet: NVRAM's boot-args \"{}\" follow iBoot's \"{IBOOT}\" (no IOLog and no kprintf on the serial line; a panic still prints; from XNU's source and not yet run on this guest, see the serial figure of the status lines)",
            QUIET.join(" ")
        )
    };
    match done() {
        Ok(true) => format!("kernel log: {state}; NVRAM changed for this start"),
        Ok(false) => format!("kernel log: {state}"),
        Err(why) => format!("kernel log: NVRAM's boot-args were not changed ({why}); the kernel prints as at the last start"),
    }
}

/// `serial 0 B/s`, `serial 850 B/s`, `serial 12.3 kB/s`: what the guest's UART and QEMU's own
/// standard output wrote over `secs`.
pub fn serial_words(bytes: u64, secs: f64) -> String {
    let rate = bytes as f64 / secs.max(0.001);
    if rate >= 1000.0 {
        format!("serial {:.1} kB/s", rate / 1000.0)
    } else {
        format!("serial {rate:.0} B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quiet_arguments_are_added_after_what_is_there_and_taken_out_again() {
        assert_eq!(merged(None, false).as_deref(), Some("serial=0x13 debug=0x20146"));
        assert_eq!(merged(None, true), None);
        assert_eq!(merged(Some("serial=0x13 debug=0x20146"), false).as_deref(), Some("serial=0x13 debug=0x20146"));
        assert_eq!(merged(Some("serial=0x13 debug=0x20146"), true), None);
        // Somebody's own arguments stay, before this app's.
        assert_eq!(merged(Some("wdt=-1  serial=0x13 cpus=4"), false).as_deref(), Some("wdt=-1 cpus=4 serial=0x13 debug=0x20146"));
        assert_eq!(merged(Some("wdt=-1 debug=0x20146 cpus=4"), true).as_deref(), Some("wdt=-1 cpus=4"));
        // iBoot's flags without kprintf, and its serial mode with IOLog kept off the UART.
        assert_eq!(0x2014e & !0x8, 0x20146);
        assert_eq!(3 | 0x10, 0x13);
        assert!(IBOOT.contains("serial=3") && IBOOT.contains("debug=0x2014e"));
    }

    #[test]
    fn the_vms_nvram_follows_the_setting() {
        let dir = std::env::temp_dir().join(format!("vphone-bootargs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("nvram.bin");
        std::fs::write(&file, nvram::image_for_tests(&["auto-boot=true", "platform-uuid=x"])).unwrap();
        let read = |name: &str| nvram::get(&std::fs::read(&file).unwrap(), name);

        let line = apply(&file, false);
        assert!(line.starts_with("kernel log: quiet: NVRAM's boot-args \"serial=0x13 debug=0x20146\" follow iBoot's"), "{line}");
        assert!(line.ends_with("NVRAM changed for this start"), "{line}");
        assert_eq!(read("boot-args").as_deref(), Some("serial=0x13 debug=0x20146"));
        assert_eq!((read("auto-boot").as_deref(), read("platform-uuid").as_deref()), (Some("true"), Some("x")));
        // The next start finds it and writes nothing.
        let before = std::fs::read(&file).unwrap();
        assert!(!apply(&file, false).contains("NVRAM changed"));
        assert_eq!(std::fs::read(&file).unwrap(), before);

        let line = apply(&file, true);
        assert!(line.starts_with("kernel log: verbose") && line.ends_with("NVRAM changed for this start"), "{line}");
        assert_eq!(read("boot-args"), None);
        assert_eq!(read("auto-boot").as_deref(), Some("true"));
        assert_eq!(apply(&file, true), "kernel log: verbose: the kernel has iBoot's \"serial=3 -v debug=0x2014e\" alone");

        // A VM set to stop in DFU stays so, and a file that is no NVRAM is left alone.
        std::fs::write(&file, nvram::image_for_tests(&["auto-boot=false"])).unwrap();
        apply(&file, false);
        assert_eq!(read("auto-boot").as_deref(), Some("false"));
        std::fs::write(&file, vec![0u8; 0x9000]).unwrap();
        let line = apply(&file, false);
        assert!(line.starts_with("kernel log: NVRAM's boot-args were not changed (") && line.contains("no NVRAM bank"), "{line}");
        assert_eq!(std::fs::read(&file).unwrap(), vec![0u8; 0x9000]);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_serial_rate_is_said_in_bytes_or_kilobytes() {
        assert_eq!(serial_words(0, 30.0), "serial 0 B/s");
        assert_eq!(serial_words(25_500, 30.0), "serial 850 B/s");
        assert_eq!(serial_words(369_000, 30.0), "serial 12.3 kB/s");
    }
}
