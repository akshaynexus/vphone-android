//! vusbd: owns QEMU's vusb chardev and presents the guest's USB device to the processes linking the
//! libusb-1.0 replacement (usbmuxd, libirecovery). Ported from tools/vusb-usbip.py: QEMU's chardev
//! takes one client, and this is the single client.

mod bridge;
mod link;

use bridge::Bridge;
use link::Link;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn usage() -> ! {
    eprintln!("usage: vusbd --chardev <qemu-vusb.sock> --listen <vusbd.sock> [-v]");
    std::process::exit(2);
}

fn duration_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(Duration::from_secs_f64)
        .unwrap_or(default)
}

fn remove_stale_socket(path: &Path) {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.file_type().is_socket() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn guest_connection(link: Arc<Link>, chardev: PathBuf) {
    loop {
        match UnixStream::connect(&chardev) {
            Ok(stream) => {
                eprintln!("vusbd: connected to QEMU chardev {}", chardev.display());
                if let Err(e) = link.serve(stream) {
                    eprintln!("vusbd: chardev closed: {e}");
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut chardev = None;
    let mut listen = None;
    let mut verbose = std::env::var_os("VUSB_DEBUG").is_some();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--chardev" => chardev = args.next().map(PathBuf::from),
            "--listen" => listen = args.next().map(PathBuf::from),
            "-v" => verbose = true,
            _ => usage(),
        }
    }
    let (Some(chardev), Some(listen)) = (chardev, listen) else {
        usage()
    };
    // Must survive a write to a client that has gone away.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    let ctrl_timeout = duration_env("VUSB_CTRL_TIMEOUT", link::CTRL_TIMEOUT);
    let stage_timeout = duration_env("VUSB_STAGE_TIMEOUT", Duration::from_secs(300));
    let link = Arc::new(Link::new(ctrl_timeout));
    let bridge = Arc::new(Bridge::new(link.clone(), stage_timeout, verbose));

    let gc = link.clone();
    thread::spawn(move || guest_connection(gc, chardev));
    let b = bridge.clone();
    thread::spawn(move || b.follow_stages());

    remove_stale_socket(&listen);
    let listener = match UnixListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vusbd: cannot listen on {}: {e}", listen.display());
            std::process::exit(1);
        }
    };
    eprintln!("vusbd: listening on {}", listen.display());
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let b = bridge.clone();
                thread::spawn(move || b.serve_client(stream));
            }
            Err(e) => {
                eprintln!("vusbd: accept: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
