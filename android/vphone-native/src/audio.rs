//! Sound: what the VM's log says about it, and the one figure the guest is told.
//!
//! The machine has a virtio sound device (`launcher::sound_args`) that QEMU plays through
//! Android's AAudio (`qemu/audio/aaudio.c`). In iOS the kernel's `AppleVirtIOSound` matches it
//! and the CFW's HAL plugin, `VPhoneVirtIOSound.driver`, drives it from audiomxd. The plugin
//! writes what it does to a file of its own in the guest, [`DRIVER_LOG`] (vphone-cli
//! `VPhoneGuestComponents/VirtIOSound/VPVirtIOSoundPlugin.m`, `VPLogToFile`), which is what this
//! reads through the guest's agent: `logs.syslog` captures only what is logged while it runs,
//! so it cannot show a load that happened at boot.

use base64::Engine;
use serde_json::{json, Value};

/// The plugin's own log in the guest; it stops growing at 4 MiB.
pub const DRIVER_LOG: &str = "/var/mobile/vpquery.log";
const HAL_PLUGINS: &str = "/System/Library/Audio/Plug-Ins/HAL";
const PLUGIN: &str = "VPhoneVirtIOSound.driver";
/// The first line of each load of the plugin into audiomxd.
const LOAD: &str = "=== plugin load ===";

/// A line of the plugin's log without its `HH:MM:SS.mmm ` stamp.
fn text(line: &str) -> &str {
    line.split_once(' ').map_or(line, |(_, rest)| rest)
}

/// What the plugin wrote at its last load: when (UTC, as the plugin stamps it), how many sound
/// devices it found, each stream's format, and what it did about VirtualAudio's product ID.
/// `None` for a log with no load in it.
pub fn driver_report(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let at = lines.iter().rposition(|l| l.contains(LOAD))?;
    let stamp = lines[at].split('.').next().unwrap_or("");
    let since = &lines[at + 1..];
    let said = |needle: &str| since.iter().copied().map(text).filter(|l| l.contains(needle)).collect::<Vec<_>>();
    let mut parts: Vec<String> = Vec::new();
    match said("virtio sound device(s)").last() {
        Some(published) if published.starts_with("published 0 ") => {
            parts.push("it found no virtio sound device (is Sound on in this VM's settings?)".into())
        }
        Some(published) => parts.push((*published).to_string()),
        None => parts.push("it has not published a device yet".into()),
    }
    // `stream 0: output, 48000 Hz, 2 channels, 32-bit float, period 32768 bytes, lead 2 period(s)`
    parts.extend(said(" Hz, ").iter().filter(|l| l.starts_with("stream ") && l.contains("period")).map(|l| (*l).to_string()));
    if let Some(product) = said("ProductIDOverride").last() {
        parts.push((*product).to_string());
    }
    Some(format!("iOS's sound driver loaded at {stamp} UTC: {}", parts.join("; ")))
}

/// The plugin's counters of the last sound that ended:
/// `stream 0: 9.08 s, 108 writes, 0 starved, 0 bytes dropped, in ... frames (...), out ...`.
/// `writes` are periods handed to the device, `starved` the ones that found it with nothing
/// left to play.
pub fn last_run(log: &str) -> Option<String> {
    log.lines().rev().map(text).find(|l| l.starts_with("stream ") && l.contains(" writes, ") && l.contains(" starved")).map(str::to_string)
}

/// The guest's file `path`, through its agent (`files.read`, as the installer reads files).
fn read(rpc: &dyn Fn(&str, Value) -> Result<Value, String>, path: &str) -> Result<String, String> {
    let reply = rpc("files.read", json!({"path": path, "binary": true}))?;
    let content = reply["content"].as_str().ok_or("no content")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(content).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// After the unlock, once: whether iOS has the sound driver and what it did at its last load.
pub fn check(rpc: &dyn Fn(&str, Value) -> Result<Value, String>) -> String {
    match read(rpc, DRIVER_LOG) {
        Ok(log) => match driver_report(&log) {
            Some(report) => format!("audio: {report}"),
            None => format!("audio: iOS's sound driver has written no load to {DRIVER_LOG}"),
        },
        Err(why) => {
            // No log: the driver is not installed, or audiomxd has not loaded it.
            let installed = rpc("files.list", json!({"path": HAL_PLUGINS})).map(|list| list.to_string().contains(PLUGIN));
            match installed {
                Ok(true) => format!("audio: iOS has the sound driver ({PLUGIN}) and it has written no log yet ({why}); audiomxd loads it at its first start"),
                Ok(false) => format!("audio: this VM's iOS has no sound driver ({PLUGIN} is not in {HAL_PLUGINS}), so it has no sound; a VM created by this app, or by vphone-cli 2.14, has it"),
                Err(list) => format!("audio: the guest's agent gave neither the sound driver's log ({why}) nor iOS's audio plug-ins ({list})"),
            }
        }
    }
}

/// The plugin's counters of the last sound, if they are not the ones in `said`.
pub fn guest_run(rpc: &dyn Fn(&str, Value) -> Result<Value, String>, said: &mut String) -> Option<String> {
    let line = last_run(&read(rpc, DRIVER_LOG).ok()?)?;
    if line == *said {
        return None;
    }
    said.clone_from(&line);
    Some(line)
}

/// A change of the latency smaller than this is not worth telling the guest: its sound driver
/// stops and restarts its stream to take a new figure, which is a short break in what plays.
pub const LATENCY_STEP: f64 = 0.015;

/// The latency to tell the guest (seconds, `audio.host_latency`), given what it has stored and
/// what QEMU measured (microseconds from its write to the sound, -1 unknown): the measurement in
/// whole milliseconds, within what vphoned takes (0 to 1 s), when it differs by [`LATENCY_STEP`].
pub fn latency_to_send(stored: f64, measured_us: i64) -> Option<f64> {
    if measured_us < 0 {
        return None;
    }
    let seconds = ((measured_us as f64 / 1000.0).round() / 1000.0).min(1.0);
    ((seconds - stored).abs() >= LATENCY_STEP).then_some(seconds)
}

/// Tells the guest how long after QEMU's write the phone plays, when that has moved: the plugin
/// adds it to its speaker's output latency, and a player holds its picture back by as much
/// (vphone-cli `Research/Guest/virtio_sound.md` §6, "Picture against sound"). `known` is what
/// the guest has stored, read from it the first time.
pub fn sync_latency(rpc: &dyn Fn(&str, Value) -> Result<Value, String>, measured_us: i64, known: &mut Option<f64>) -> Option<String> {
    if measured_us < 0 {
        return None;
    }
    let stored = match *known {
        Some(stored) => stored,
        None => {
            let stored = rpc("audio.host_latency", json!({})).ok()?["seconds"].as_f64()?;
            *known = Some(stored);
            stored
        }
    };
    let seconds = latency_to_send(stored, measured_us)?;
    match rpc("audio.host_latency", json!({"seconds": seconds})) {
        Ok(_) => {
            *known = Some(seconds);
            Some(format!(
                "audio: told iOS this phone plays {:.0} ms after QEMU's write (it had {:.0} ms), so players hold their picture back by as much",
                seconds * 1000.0,
                stored * 1000.0
            ))
        }
        Err(why) => Some(format!("audio: iOS did not take the phone's latency ({why})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const LOG: &str = "\
09:00:01.100 === plugin load ===
09:00:01.200 published 0 virtio sound device(s)
11:58:02.000 === plugin load ===
11:58:02.010 ProductIDOverride on 'iPhone99,11': stored 8010, nothing to do; VirtualAudio not loaded yet
11:58:02.050 stream 0: direction 0, formats 0x7f, rates 0x3fff, channels 1-2
11:58:02.060 stream 0: output, 48000 Hz, 2 channels, 32-bit float, period 32768 bytes, lead 2 period(s)
11:58:02.070 device PuffinOutput: 48000 Hz nominal (48000 wire, 44100 advertised too), transport 'usb '
11:58:02.090 published 1 virtio sound device(s)
12:01:10.000 stream 0: 9.08 s, 108 writes, 0 starved, 0 bytes dropped, in 400384 frames (44079/s, hal 44100), out 435792 frames (47977/s); start 2.54, lead 1.46, in flight 2.00-3.00 periods; host 48000.2/s (+5 ppm), returns <= 86 ms apart
12:03:00.000 stream 0: running 60 s, 700 writes, 2 starved, in flight 2.00-3.00 periods this minute; host 48000.1/s (+2 ppm)
12:03:20.000 stream 0: 80.00 s, 940 writes, 3 starved, 0 bytes dropped, in 1 frames (1/s, hal 48000), out 1 frames (1/s); start 0.00, lead 2.00, in flight 1.00-3.00 periods; host 48000.0/s (+0 ppm), returns <= 120 ms apart
";

    #[test]
    fn the_report_is_of_the_last_load() {
        let report = driver_report(LOG).unwrap();
        assert!(report.starts_with("iOS's sound driver loaded at 11:58:02 UTC: published 1 virtio sound device(s); "), "{report}");
        assert!(report.contains("stream 0: output, 48000 Hz, 2 channels, 32-bit float, period 32768 bytes, lead 2 period(s)"), "{report}");
        assert!(report.ends_with("ProductIDOverride on 'iPhone99,11': stored 8010, nothing to do; VirtualAudio not loaded yet"), "{report}");
        // The stream's capabilities and its counters are not its format.
        assert!(!report.contains("direction 0") && !report.contains("writes"), "{report}");
        // A load that found no device says so, and a log with no load says nothing.
        let none = driver_report("09:00:01.100 === plugin load ===\n09:00:01.200 published 0 virtio sound device(s)\n").unwrap();
        assert!(none.contains("found no virtio sound device"), "{none}");
        assert!(driver_report("09:00:01.100 === plugin load ===\n").unwrap().contains("not published a device yet"));
        assert_eq!(driver_report("12:00:00.000 query x\n"), None);
    }

    #[test]
    fn the_last_sounds_counters_are_the_line_of_its_end() {
        let line = last_run(LOG).unwrap();
        assert!(line.starts_with("stream 0: 80.00 s, 940 writes, 3 starved, "), "{line}");
        assert_eq!(last_run("11:58:02.090 published 1 virtio sound device(s)\n"), None);
    }

    #[test]
    fn the_check_reads_the_log_or_says_why_there_is_none() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let with_log = |method: &str, params: Value| {
            assert_eq!((method, &params), ("files.read", &json!({"path": DRIVER_LOG, "binary": true})));
            Ok(json!({"content": b64(LOG)}))
        };
        assert!(check(&with_log).starts_with("audio: iOS's sound driver loaded at 11:58:02 UTC: published 1 "));
        let listing = |plugins: &'static str| {
            move |method: &str, _: Value| match method {
                "files.read" => Err("files.read: No such file".to_string()),
                _ => Ok(json!({"entries": [{"name": "BuiltinAudioPlugin.driver"}, {"name": plugins}]})),
            }
        };
        assert!(check(&listing("VPhoneVirtIOSound.driver")).contains("has written no log yet"));
        assert!(check(&listing("AirPlay.driver")).contains("has no sound driver"));
        let deaf = |method: &str, _: Value| Err(format!("{method}: timed out"));
        assert!(check(&deaf).contains("gave neither"));
        // The counters are said once.
        let mut said = String::new();
        assert!(guest_run(&with_log, &mut said).unwrap().contains("940 writes, 3 starved"));
        assert_eq!(guest_run(&with_log, &mut said), None);
    }

    #[test]
    fn the_latency_is_told_when_it_moves_by_enough() {
        assert_eq!(latency_to_send(0.0, -1), None);
        assert_eq!(latency_to_send(0.0, 46_400), Some(0.046));
        assert_eq!(latency_to_send(0.040, 46_400), None);
        assert_eq!(latency_to_send(0.046, 250_000), Some(0.25));
        assert_eq!(latency_to_send(0.25, 46_000), Some(0.046));
        assert_eq!(latency_to_send(0.0, 3_000_000), Some(1.0));
        assert_eq!(latency_to_send(0.0, 9_000), None);

        let calls = RefCell::new(Vec::new());
        let rpc = |method: &str, params: Value| {
            calls.borrow_mut().push(params.clone());
            assert_eq!(method, "audio.host_latency");
            Ok(if params.get("seconds").is_some() { json!({"seconds": params["seconds"], "changed": true}) } else { json!({"seconds": 0.0156}) })
        };
        let mut known = None;
        // Nothing measured: nothing asked of the guest.
        assert_eq!(sync_latency(&rpc, -1, &mut known), None);
        assert!(calls.borrow().is_empty());
        // Near what a Mac host stored: read once, left alone.
        assert_eq!(sync_latency(&rpc, 25_000, &mut known), None);
        assert_eq!((known, calls.borrow().len()), (Some(0.0156), 1));
        // Bluetooth: told, and not read again.
        let line = sync_latency(&rpc, 212_300, &mut known).unwrap();
        assert!(line.contains("plays 212 ms after QEMU's write (it had 16 ms)"), "{line}");
        assert_eq!(known, Some(0.212));
        assert_eq!(*calls.borrow(), [json!({}), json!({"seconds": 0.212})]);
        assert_eq!(sync_latency(&rpc, 215_000, &mut known), None);
        assert_eq!(calls.borrow().len(), 2);
    }
}
