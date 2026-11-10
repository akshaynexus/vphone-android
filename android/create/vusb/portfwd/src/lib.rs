//! usbmux client: connect to a TCP port on the attached device through usbmuxd.
//!
//! usbmuxd's binary protocol (version 0): every message starts with `length, version, message, tag`
//! as little-endian u32s. CONNECT turns the socket into a raw stream to the device port once its
//! RESULT is 0. The port is in network byte order, as in iproxy.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const MESSAGE_RESULT: u32 = 1;
const MESSAGE_CONNECT: u32 = 2;
const MESSAGE_LISTEN: u32 = 3;
const MESSAGE_DEVICE_ADD: u32 = 4;
const MESSAGE_DEVICE_REMOVE: u32 = 5;
const HEADER: usize = 16;

/// A device as usbmuxd reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: u32,
    pub product_id: u16,
    pub serial: String,
    pub location: u32,
}

fn send_header(s: &mut UnixStream, message: u32, tag: u32, payload: &[u8]) -> io::Result<()> {
    let mut m = Vec::with_capacity(HEADER + payload.len());
    m.extend_from_slice(&((HEADER + payload.len()) as u32).to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&message.to_le_bytes());
    m.extend_from_slice(&tag.to_le_bytes());
    m.extend_from_slice(payload);
    s.write_all(&m)
}

fn read_message(s: &mut UnixStream) -> io::Result<(u32, u32, Vec<u8>)> {
    let mut h = [0u8; HEADER];
    s.read_exact(&mut h)?;
    let length = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
    let message = u32::from_le_bytes(h[8..12].try_into().unwrap());
    let tag = u32::from_le_bytes(h[12..16].try_into().unwrap());
    if !(HEADER..=64 * 1024).contains(&length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad usbmuxd message length {length}"),
        ));
    }
    let mut body = vec![0u8; length - HEADER];
    s.read_exact(&mut body)?;
    Ok((message, tag, body))
}

fn expect_ok(s: &mut UnixStream, what: &str) -> io::Result<()> {
    loop {
        let (message, _, body) = read_message(s)?;
        if message != MESSAGE_RESULT {
            continue; // a device event ahead of the result
        }
        let result = u32::from_le_bytes(body.get(..4).unwrap_or(&[0; 4]).try_into().unwrap());
        if result != 0 {
            return Err(io::Error::other(format!("{what}: usbmuxd result {result}")));
        }
        return Ok(());
    }
}

fn parse_device(body: &[u8]) -> Option<Device> {
    if body.len() < 4 + 2 + 256 + 2 + 4 {
        return None;
    }
    let serial = &body[6..262];
    let end = serial.iter().position(|&b| b == 0).unwrap_or(256);
    Some(Device {
        id: u32::from_le_bytes(body[0..4].try_into().unwrap()),
        product_id: u16::from_le_bytes(body[4..6].try_into().unwrap()),
        serial: String::from_utf8_lossy(&serial[..end]).into_owned(),
        location: u32::from_le_bytes(body[264..268].try_into().unwrap()),
    })
}

/// Wait for a usbmux device to be present and return it.
pub fn wait_for_device(usbmuxd_socket: &Path, timeout: Duration) -> io::Result<Device> {
    let deadline = Instant::now() + timeout;
    loop {
        match try_wait_for_device(usbmuxd_socket, deadline) {
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                thread::sleep(Duration::from_millis(250));
            }
            other => return other,
        }
    }
}

fn try_wait_for_device(usbmuxd_socket: &Path, deadline: Instant) -> io::Result<Device> {
    let mut s = UnixStream::connect(usbmuxd_socket)?;
    let timeout = deadline.saturating_duration_since(Instant::now());
    s.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
    send_header(&mut s, MESSAGE_LISTEN, 1, &[])?;
    expect_ok(&mut s, "LISTEN")?;
    loop {
        let (message, _, body) = read_message(&mut s)?;
        if message == MESSAGE_DEVICE_ADD {
            if let Some(d) = parse_device(&body) {
                return Ok(d);
            }
        } else if message != MESSAGE_DEVICE_REMOVE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected usbmuxd message {message}"),
            ));
        }
    }
}

/// Open a connection to `device_port` on device `device_id`. On success the returned socket is a raw
/// byte stream to that port.
pub fn connect_device(
    usbmuxd_socket: &Path,
    device_id: u32,
    device_port: u16,
) -> io::Result<UnixStream> {
    let mut s = UnixStream::connect(usbmuxd_socket)?;
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&device_id.to_le_bytes());
    payload.extend_from_slice(&device_port.to_be_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    send_header(&mut s, MESSAGE_CONNECT, 2, &payload)?;
    expect_ok(&mut s, &format!("CONNECT to device port {device_port}"))?;
    Ok(s)
}

/// Wait for a device, then open `device_port` on it. Keeps retrying CONNECT until `timeout`, since
/// a service on the device (ramdisk SSH, ...) may come up later than the USB device.
pub fn connect(usbmuxd_socket: &Path, device_port: u16, timeout: Duration) -> io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("device port {device_port} did not accept a connection in time"),
            ));
        }
        let attempt = wait_for_device(usbmuxd_socket, remaining)
            .and_then(|d| connect_device(usbmuxd_socket, d.id, device_port));
        match attempt {
            Ok(s) => return Ok(s),
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(500)),
            Err(e) => return Err(e),
        }
    }
}

/// The ECID a device's usbmux serial carries, in the forms the VM's stages use: sixteen hex
/// digits, with `0x`, or after the eight of a chip id (`0000FE01-0123456789ABCDEF`). The stock SSH
/// ramdisk's build-date serial has none.
pub fn serial_ecid(serial: &str) -> Option<u64> {
    let s = serial.trim().to_ascii_uppercase();
    let hex16 = |t: &str| (t.len() == 16 && t.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u64::from_str_radix(t, 16).ok()).flatten();
    if let Some(rest) = s.strip_prefix("0X") {
        return hex16(rest);
    }
    if s.len() == 16 {
        return hex16(&s);
    }
    let chip = |t: &str| t.len() == 8 && t.bytes().all(|b| b.is_ascii_hexdigit());
    match s.len() {
        24 if chip(&s[..8]) => hex16(&s[8..]),
        25 if chip(&s[..8]) && s.as_bytes()[8] == b'-' => hex16(&s[9..]),
        _ => None,
    }
}

/// The device of `ecid` among `devices`: the one whose serial carries it, or a sole device whose
/// serial carries none (the SSH ramdisk). Anything else is an error, never a guess: with an ECID
/// asked for, a device of another VM must not be taken for this one.
pub fn pick_device(devices: &[Device], ecid: u64) -> Result<&Device, String> {
    let matches: Vec<&Device> = devices.iter().filter(|d| serial_ecid(&d.serial) == Some(ecid)).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] if devices.len() == 1 && serial_ecid(&devices[0].serial).is_none() => Ok(&devices[0]),
        [] => Err(format!("ECID 0x{ecid:016X} matches none of the {} usbmux devices", devices.len())),
        _ => Err(format!("ECID 0x{ecid:016X} matches more than one usbmux device")),
    }
}

/// The devices usbmuxd has now: it lists them on LISTEN, and `settle` without another event ends
/// the list.
pub fn list_devices(usbmuxd_socket: &Path, settle: Duration) -> io::Result<Vec<Device>> {
    let mut s = UnixStream::connect(usbmuxd_socket)?;
    s.set_read_timeout(Some(settle.max(Duration::from_millis(1))))?;
    send_header(&mut s, MESSAGE_LISTEN, 1, &[])?;
    expect_ok(&mut s, "LISTEN")?;
    let mut devices: Vec<Device> = Vec::new();
    loop {
        match read_message(&mut s) {
            Ok((MESSAGE_DEVICE_ADD, _, body)) => devices.extend(parse_device(&body)),
            Ok((MESSAGE_DEVICE_REMOVE, _, body)) if body.len() >= 4 => {
                let id = u32::from_le_bytes(body[..4].try_into().unwrap());
                devices.retain(|d| d.id != id);
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(devices),
            Err(e) => return Err(e),
        }
    }
}

/// As [`connect`], to the device of `ecid` (see [`pick_device`]). No device yet is waited for; a
/// device that is there and is not this VM's is an error at once.
pub fn connect_ecid(usbmuxd_socket: &Path, device_port: u16, timeout: Duration, ecid: u64) -> io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        let attempt = list_devices(usbmuxd_socket, Duration::from_millis(300)).and_then(|devices| {
            if devices.is_empty() {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no usbmux device yet"));
            }
            let device = pick_device(&devices, ecid).map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why))?;
            connect_device(usbmuxd_socket, device.id, device_port)
        });
        match attempt {
            Ok(s) => return Ok(s),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(e),
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(500)),
            Err(e) => return Err(e),
        }
    }
}

fn copy_shutdown(mut from: impl Read, mut to: impl Write, on_done: impl FnOnce()) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    on_done();
}

/// Copy between `client` and `device` until either side closes.
fn pump_tcp(client: TcpStream, device: UnixStream) {
    let (Ok(c2), Ok(d2)) = (client.try_clone(), device.try_clone()) else {
        return;
    };
    let (Ok(c3), Ok(d3)) = (client.try_clone(), device.try_clone()) else {
        return;
    };
    thread::spawn(move || {
        copy_shutdown(c2, d2, move || {
            let _ = d3.shutdown(Shutdown::Both);
        });
    });
    copy_shutdown(device, client, move || {
        let _ = c3.shutdown(Shutdown::Both);
    });
}

fn pump_unix(client: UnixStream, device: UnixStream) {
    let (Ok(c2), Ok(d2)) = (client.try_clone(), device.try_clone()) else {
        return;
    };
    let (Ok(c3), Ok(d3)) = (client.try_clone(), device.try_clone()) else {
        return;
    };
    thread::spawn(move || {
        copy_shutdown(c2, d2, move || {
            let _ = d3.shutdown(Shutdown::Both);
        });
    });
    copy_shutdown(device, client, move || {
        let _ = c3.shutdown(Shutdown::Both);
    });
}

/// Where the forwarder listens.
#[derive(Clone, Debug)]
pub enum Listen {
    Tcp(String),
    Unix(PathBuf),
}

impl Listen {
    /// `unix:/path` or `host:port`.
    pub fn parse(s: &str) -> Listen {
        match s.strip_prefix("unix:") {
            Some(p) => Listen::Unix(PathBuf::from(p)),
            None => Listen::Tcp(s.to_owned()),
        }
    }
}

/// Serve `listen` forever: each accepted connection gets a new usbmux connection to `device_port`.
pub fn forward(usbmuxd_socket: &Path, listen: &Listen, device_port: u16) -> io::Result<()> {
    let connect_timeout = Duration::from_secs(120);
    match listen {
        Listen::Tcp(addr) => {
            let addr = addr
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| io::Error::other("no listen address"))?;
            let listener = TcpListener::bind(addr)?;
            for conn in listener.incoming() {
                let client = conn?;
                let _ = client.set_nodelay(true);
                let sock = usbmuxd_socket.to_owned();
                thread::spawn(move || match connect(&sock, device_port, connect_timeout) {
                    Ok(device) => pump_tcp(client, device),
                    Err(e) => eprintln!("vphone_portfwd: connect failed: {e}"),
                });
            }
        }
        Listen::Unix(path) => {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)?;
            for conn in listener.incoming() {
                let client = conn?;
                let sock = usbmuxd_socket.to_owned();
                thread::spawn(move || match connect(&sock, device_port, connect_timeout) {
                    Ok(device) => pump_unix(client, device),
                    Err(e) => eprintln!("vphone_portfwd: connect failed: {e}"),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod ecid_tests {
    use super::*;

    fn device(id: u32, serial: &str) -> Device {
        Device { id, product_id: 0x1281, serial: serial.into(), location: 0 }
    }

    #[test]
    fn a_serial_s_ecid_is_read_in_each_form_the_stages_use() {
        const E: u64 = 0x36CE_A734_EBD7_E663;
        for serial in ["36CEA734EBD7E663", "0x36cea734ebd7e663", "0000FE01-36CEA734EBD7E663", "0000FE0136CEA734EBD7E663"] {
            assert_eq!(serial_ecid(serial), Some(E), "{serial}");
        }
        for serial in ["", "Sep 22 2022", "36CEA734EBD7E66", "0000FE01_36CEA734EBD7E663", "ZZCEA734EBD7E663"] {
            assert_eq!(serial_ecid(serial), None, "{serial}");
        }
    }

    #[test]
    fn only_this_vm_s_device_is_picked() {
        const E: u64 = 0x36CE_A734_EBD7_E663;
        let mine = device(2, "0000FE01-36CEA734EBD7E663");
        let other = device(3, "0000FE01-0000000000000001");
        let ramdisk = device(4, "Sep 22 2022");
        assert_eq!(pick_device(&[other.clone(), mine.clone()], E).unwrap().id, 2);
        assert_eq!(pick_device(&[ramdisk.clone()], E).unwrap().id, 4, "a sole device without an ECID in its serial");
        assert!(pick_device(&[other.clone()], E).is_err(), "another VM's device is not taken");
        assert!(pick_device(&[ramdisk.clone(), other], E).is_err(), "not sole: no guess");
        assert!(pick_device(&[mine.clone(), mine], E).is_err());
        assert!(pick_device(&[], E).is_err());
    }
}
