//! `libvphone_portfwd.so <usbmuxd-socket> <listen> <device-port>`: iproxy without libusbmuxd.
//! `<listen>` is `host:port` or `unix:/path`.

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!("usage: vphone_portfwd <usbmuxd-socket> <host:port|unix:/path> <device-port>");
        std::process::exit(2);
    }
    let Ok(port) = args[2].parse::<u16>() else {
        eprintln!("vphone_portfwd: bad device port {}", args[2]);
        std::process::exit(2);
    };
    let socket = PathBuf::from(&args[0]);
    let listen = vusb_portfwd::Listen::parse(&args[1]);
    if let Err(e) = vusb_portfwd::forward(&socket, &listen, port) {
        eprintln!("vphone_portfwd: {e}");
        std::process::exit(1);
    }
}
