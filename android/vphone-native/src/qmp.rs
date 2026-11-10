//! QEMU's monitor protocol, for powering the VM off when the guest cannot do it itself.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub fn quit(socket: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?; // the greeting
    writer.write_all(b"{\"execute\":\"qmp_capabilities\"}\n")?;
    line.clear();
    reader.read_line(&mut line)?;
    writer.write_all(b"{\"execute\":\"quit\"}\n")?;
    line.clear();
    // Its "return", or the end of the connection when QEMU is quicker.
    let _ = reader.read_line(&mut line);
    Ok(())
}

/// Sends one QMP command without arguments; returns once QEMU has answered.
pub fn command(socket: &Path, command: &str) -> io::Result<()> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?; // the greeting
    writer.write_all(b"{\"execute\":\"qmp_capabilities\"}\n")?;
    line.clear();
    reader.read_line(&mut line)?;
    writer.write_all(format!("{{\"execute\":\"{command}\"}}\n").as_bytes())?;
    line.clear();
    reader.read_line(&mut line)?;
    Ok(())
}

/// One vCPU's place in the guest, from `info registers`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vcpu {
    pub index: u32,
    pub pc: u64,
    pub el: u8,
}

/// `info registers -a` over HMP, as tools/cpu-sample.py samples it, for the vCPUs below `vcpus`
/// (the SEP coprocessor's CPU comes after them). A new connection each time: QEMU's monitor
/// socket takes one client, and the pause and stop paths must not wait behind a long-lived one.
pub fn sample_vcpus(socket: &Path, vcpus: u32) -> io::Result<Vec<Vcpu>> {
    Ok(parse_registers(&hmp(socket, "info registers -a", None)?, vcpus))
}

/// One command of QEMU's human monitor over QMP, for vCPU `cpu` where given: its output.
fn hmp(socket: &Path, command: &str, cpu: Option<u32>) -> io::Result<String> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?; // the greeting
    writer.write_all(b"{\"execute\":\"qmp_capabilities\"}\n")?;
    line.clear();
    reader.read_line(&mut line)?;
    let mut arguments = serde_json::json!({"command-line": command});
    if let Some(cpu) = cpu {
        arguments["cpu-index"] = cpu.into();
    }
    let request = serde_json::json!({"execute": "human-monitor-command", "arguments": arguments});
    writer.write_all(format!("{request}\n").as_bytes())?;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "QMP closed"));
        }
        let reply: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(text) = reply.get("return").and_then(|v| v.as_str()) {
            return Ok(text.to_owned());
        }
        if reply.get("error").is_some() {
            return Err(io::Error::other(line.trim().to_string()));
        }
        // An event: the answer is still to come.
    }
}

/// `count` 32-bit words of the guest's memory at the virtual address `at`, as vCPU `cpu` maps it
/// now (the monitor's `x`): the instructions around a PC, for telling a spin loop from work.
/// Fewer words where the guest maps fewer; none where it maps none.
pub fn read_words(socket: &Path, cpu: u32, at: u64, count: u32) -> io::Result<Vec<u32>> {
    Ok(parse_words(&hmp(socket, &format!("x /{count}xw {at:#x}"), Some(cpu))?))
}

/// The words of the monitor's `x /Nxw`: lines of `<address>: 0x<word> 0x<word> ...`; a line that
/// is not one (`Cannot access memory`) ends them.
pub fn parse_words(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((_, words)) = line.split_once(':') else {
            break;
        };
        for word in words.split_whitespace() {
            match word.strip_prefix("0x").and_then(|hex| u32::from_str_radix(hex, 16).ok()) {
                Some(word) => out.push(word),
                None => return out,
            }
        }
    }
    out
}

/// Reads `CPU#n`, ` PC=<hex> ...` and `PSTATE=<hex> <flags> EL<n>...` from `info registers`'s text.
pub fn parse_registers(text: &str, vcpus: u32) -> Vec<Vcpu> {
    let mut out = Vec::new();
    for chunk in text.split("CPU#").skip(1) {
        let Some(index) = chunk
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if index >= vcpus {
            continue;
        }
        let pc = chunk.split("PC=").nth(1).and_then(|t| {
            let hex: String = t.chars().take_while(char::is_ascii_hexdigit).collect();
            u64::from_str_radix(&hex, 16).ok()
        });
        let el = chunk
            .split("PSTATE=")
            .nth(1)
            .and_then(|t| t.split_whitespace().find_map(|w| w.strip_prefix("EL")))
            .and_then(|w| w.chars().next())
            .and_then(|c| c.to_digit(10))
            .map(|d| d as u8);
        if let (Some(pc), Some(el)) = (pc, el) {
            out.push(Vcpu { index, pc, el });
        }
    }
    out.sort_by_key(|c| c.index);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Against a real QEMU: `VPHONE_TEST_QMP=<socket> cargo test --lib live_qmp -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_qmp_sample() {
        let socket = PathBuf::from(std::env::var("VPHONE_TEST_QMP").expect("VPHONE_TEST_QMP"));
        for _ in 0..5 {
            let vcpus = sample_vcpus(&socket, 64).unwrap();
            println!("{vcpus:?}");
            assert!(!vcpus.is_empty(), "the AP vCPUs were read");
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    #[test]
    fn reads_the_registers_cpu_sample_reads() {
        let text = "CPU#0\r\n PC=fffffe003f23c6ec SP=fffffe0044a43f90\r\nX00=0000000000000000\r\nPSTATE=60400005 -ZC- EL1h    FPU disabled\r\nCPU#1\r\n PC=0000000100003f10 SP=000000016fd0\r\nPSTATE=80000000 N--- EL0t\r\nCPU#6\r\n PC=00000000deadbeef SP=0\r\nPSTATE=00000005 ---- EL1h";
        assert_eq!(
            parse_registers(text, 6),
            vec![
                Vcpu { index: 0, pc: 0xfffffe003f23c6ec, el: 1 },
                Vcpu { index: 1, pc: 0x100003f10, el: 0 },
            ],
            "the SEP's CPU, past the AP vCPUs, is left out"
        );
    }

    #[test]
    fn samples_over_the_monitor_after_skipping_events() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/sock");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qmp-regs");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            s.write_all(b"{\"QMP\":{}}\n").unwrap();
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            s.write_all(b"{\"return\":{}}\n").unwrap();
            l.clear();
            r.read_line(&mut l).unwrap();
            assert!(l.contains("info registers -a"), "{l}");
            s.write_all(b"{\"event\":\"STOP\"}\n").unwrap();
            s.write_all(b"{\"return\":\"CPU#0\\r\\n PC=abc SP=0\\r\\nPSTATE=0 ---- EL1h\\r\\n\"}\n").unwrap();
        });
        assert_eq!(sample_vcpus(&path, 8).unwrap(), vec![Vcpu { index: 0, pc: 0xabc, el: 1 }]);
        server.join().unwrap();
    }
    use std::path::PathBuf;

    #[test]
    fn reads_the_words_of_the_monitors_dump() {
        let text = "00000002c0eaca00: 0xd503201f 0xf9400008 0xb4000048 0xd65f03c0\r\n00000002c0eaca10: 0xd503205f 0x17fffffe\r\n";
        assert_eq!(parse_words(text), vec![0xd503201f, 0xf9400008, 0xb4000048, 0xd65f03c0, 0xd503205f, 0x17fffffe]);
        assert_eq!(parse_words("Cannot access memory\r\n"), Vec::<u32>::new());
        assert_eq!(parse_words("00000002c0eaca00: 0xd503201f 0xf9400008\r\n00000002c0eaca08: Cannot access memory\r\n"), vec![0xd503201f, 0xf9400008]);
    }

    #[test]
    fn a_dump_is_asked_of_the_vcpu_whose_memory_it_is() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/sock");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qmp-words");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            s.write_all(b"{\"QMP\":{}}\n").unwrap();
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            s.write_all(b"{\"return\":{}}\n").unwrap();
            l.clear();
            r.read_line(&mut l).unwrap();
            let request: serde_json::Value = serde_json::from_str(&l).unwrap();
            assert_eq!(request["execute"], "human-monitor-command");
            assert_eq!(request["arguments"]["command-line"], "x /2xw 0x2c0eaca00");
            assert_eq!(request["arguments"]["cpu-index"], 3);
            s.write_all(b"{\"return\":\"00000002c0eaca00: 0xd503205f 0x17ffffff\\r\\n\"}\n").unwrap();
        });
        assert_eq!(read_words(&path, 3, 0x2c0eaca00, 2).unwrap(), vec![0xd503205f, 0x17ffffff]);
        server.join().unwrap();
    }

    #[test]
    fn negotiates_then_quits() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures/sock");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qmp");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            s.write_all(b"{\"QMP\":{}}\n").unwrap();
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            assert!(l.contains("qmp_capabilities"));
            s.write_all(b"{\"return\":{}}\n").unwrap();
            l.clear();
            r.read_line(&mut l).unwrap();
            assert!(l.contains("\"quit\""));
        });
        quit(&path).unwrap();
        server.join().unwrap();
    }
}
