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
