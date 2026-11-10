//! The VM's way to the internet: QEMU's user-mode network (libslirp), a NAT made of the app's
//! own sockets. The guest gets 10.0.2.15 by DHCP from it, the gateway 10.0.2.2 and the resolver
//! 10.0.2.3, which libslirp forwards to the first IPv4 server of a resolv.conf. Android has no
//! such file, so the app keeps one ([`RESOLV_CONF_ENV`], android/patches/libslirp) and Kotlin
//! gives it the active network's servers (`VmNetwork`).
//!
//! libslirp looks at the file again at most once a second, when its inode, size or mtime
//! changed, so a rewrite here reaches the next lookup of a running VM.

use crate::paths::Paths;
use serde_json::{json, Value};
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

/// The variable the patched libslirp reads its resolv.conf's path from.
pub const RESOLV_CONF_ENV: &str = "VPHONE_RESOLV_CONF";

/// After the network's own servers: a mobile network may list only IPv6 resolvers, and the VM's
/// network is IPv4 only. libslirp uses the first server of the file alone, so these answer only
/// when the network gave no IPv4 one.
pub const FALLBACK_DNS: [&str; 2] = ["1.1.1.1", "8.8.8.8"];

/// The address QEMU's NAT gives its guest, and the network it is in.
const NAT_PREFIX: &str = "10.0.2.";

/// What the guest is asked to resolve and connect to.
const PROBE_HOST: &str = "apple.com";
const PROBE_PORT: u16 = 443;

/// The servers the file names for `android`'s (the active network's, as Android wrote them,
/// separated by spaces or commas): its IPv4 ones in order, then the fallbacks. IPv6 and loopback
/// addresses are left out, as the NAT could not reach them.
pub fn servers(android: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let given = android
        .split([' ', ',', '\n'])
        .filter_map(|s| s.trim().parse::<Ipv4Addr>().ok())
        .filter(|a| !a.is_loopback() && !a.is_unspecified())
        .map(|a| a.to_string());
    for server in given.chain(FALLBACK_DNS.iter().map(|s| s.to_string())) {
        if !out.contains(&server) {
            out.push(server);
        }
    }
    out
}

pub fn resolv_conf(servers: &[String]) -> String {
    let mut text = String::from("# Written by vphone for the VM's network (QEMU's libslirp).\n");
    for server in servers {
        text += &format!("nameserver {server}\n");
    }
    text
}

/// The servers a resolv.conf names.
pub fn read(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver "))
        .map(|s| s.trim().to_string())
        .collect()
}

/// Writes the file for `android`'s servers, as a new file moved into place: a new inode is a
/// change libslirp sees whatever the size and the second. Returns the servers written, or
/// `None` when the file already names them.
pub fn write(file: &Path, android: &str) -> std::io::Result<Option<Vec<String>>> {
    let servers = servers(android);
    if read(file) == servers {
        return Ok(None);
    }
    let part = file.with_extension("conf.part");
    std::fs::write(&part, resolv_conf(&servers))?;
    std::fs::rename(&part, file)?;
    Ok(Some(servers))
}

/// Kotlin's report of the active network's DNS servers: the line for the log when they changed.
pub fn set_dns_servers(paths: &Paths, android: &str) -> Option<String> {
    let seen = if android.trim().is_empty() { "none" } else { android.trim() };
    match write(&paths.resolv_conf(), android) {
        Ok(Some(servers)) => Some(format!(
            "network: DNS servers for the VM: {} (this phone's network gave: {seen}; only IPv4 ones are used, and only the first is asked)",
            servers.join(", ")
        )),
        Ok(None) => None,
        Err(e) => Some(format!("network: cannot write {}: {e}", paths.resolv_conf().display())),
    }
}

/// Before a boot: the file is there, with the fallbacks alone if Kotlin never wrote it. The line
/// for the boot's log.
pub fn boot_line(paths: &Paths) -> String {
    let file = paths.resolv_conf();
    if read(&file).is_empty() {
        if let Err(e) = write(&file, "") {
            return format!("network: on, but {} cannot be written ({e}): the VM will resolve no names", file.display());
        }
    }
    format!(
        "network: on (QEMU's NAT, IPv4 only; the guest should get {NAT_PREFIX}15); DNS servers {} in {}",
        read(&file).join(", "),
        file.display()
    )
}

/// What `device.network` says of the guest's `en0`.
#[derive(Debug, PartialEq)]
pub enum Link {
    /// An address of QEMU's NAT.
    Nat(String),
    /// `en0` is there with another IPv4 address: a self-assigned one (DHCP did not answer) or
    /// one set by hand.
    Other(String),
    /// `en0` is there (it has an IPv6 link-local address) and has no IPv4 address yet.
    NoAddress,
    /// No `en0` in the list: iOS's driver did not bind QEMU's network card. The interfaces seen.
    NoInterface(Vec<String>),
    /// vphoned did not answer, or not with a list of addresses.
    Unknown(String),
}

/// vphoned's `device.network`: `{"addresses": ["en0 10.0.2.15", "lo0 127.0.0.1", ...]}`.
pub fn link(reply: &Result<Value, String>) -> Link {
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return Link::Unknown(e.clone()),
    };
    let Some(list) = reply["addresses"].as_array() else {
        let mut text = reply.to_string();
        text.truncate(200);
        return Link::Unknown(format!("device.network gave no addresses: {text}"));
    };
    let pairs: Vec<(&str, &str)> = list
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|s| s.trim().split_once(' '))
        .collect();
    let en0: Vec<&str> = pairs.iter().filter(|(i, _)| *i == "en0").map(|(_, a)| a.trim()).collect();
    if en0.is_empty() {
        let mut seen: Vec<String> = Vec::new();
        for (interface, _) in &pairs {
            if !seen.iter().any(|s| s == interface) {
                seen.push(interface.to_string());
            }
        }
        return Link::NoInterface(seen);
    }
    let v4: Vec<&str> = en0.iter().copied().filter(|a| a.parse::<Ipv4Addr>().is_ok()).collect();
    match v4.iter().find(|a| a.starts_with(NAT_PREFIX)).or(v4.first()) {
        Some(a) if a.starts_with(NAT_PREFIX) => Link::Nat(a.to_string()),
        Some(a) => Link::Other(a.to_string()),
        None => Link::NoAddress,
    }
}

impl Link {
    /// Whether asking again later could find the address: DHCP may not have answered yet.
    pub fn may_settle(&self) -> bool {
        matches!(self, Link::NoAddress | Link::Unknown(_))
            || matches!(self, Link::Other(a) if a.starts_with("169.254."))
    }

    fn words(&self) -> String {
        match self {
            Link::Nat(a) => format!("en0 {a} (QEMU's NAT)"),
            Link::Other(a) if a.starts_with("169.254.") => format!(
                "NO ADDRESS: en0 is there with the self-assigned {a}, so iOS's driver bound QEMU's network card and DHCP from QEMU's NAT did not answer"
            ),
            Link::Other(a) => format!(
                "en0 has {a}, which is not QEMU's NAT ({NAT_PREFIX}x): an address set by hand in iOS, or kept from another host"
            ),
            Link::NoAddress => "NO ADDRESS: en0 is there without an IPv4 address, so iOS's driver bound QEMU's network card and DHCP from QEMU's NAT did not answer".to_string(),
            Link::NoInterface(seen) => format!(
                "NO en0: iOS's driver did not bind QEMU's network card (virtio-net-pci); iOS has only {}",
                if seen.is_empty() { "no interfaces".to_string() } else { seen.join(", ") }
            ),
            Link::Unknown(e) => format!("not read: {e}"),
        }
    }
}

/// vphoned's `network.resolve` with a port: `{status, elapsed_ms, addresses: [{address, connect,
/// connect_ms, connect_error}]}`, or `{status: "error", error}`.
fn resolve_words(reply: &Result<Value, String>) -> String {
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) if e.to_lowercase().contains("unknown method") => {
            return "DNS and TCP not tested: this VM's vphoned has no network.resolve (older guest tools)".to_string();
        }
        Err(e) => return format!("DNS not tested: {e}"),
    };
    let ms = reply["elapsed_ms"].as_i64().unwrap_or(-1);
    match reply["status"].as_str() {
        Some("resolved") => {}
        Some("timeout") => return format!("DNS FAILED: {PROBE_HOST} did not resolve in {ms} ms"),
        _ => {
            return format!(
                "DNS FAILED: {PROBE_HOST}: {} ({ms} ms)",
                reply["error"].as_str().unwrap_or("no answer")
            )
        }
    }
    let first = &reply["addresses"][0];
    let address = first["address"].as_str().unwrap_or("?");
    let tcp = match first["connect"].as_str() {
        Some("connected") => format!(
            "TCP {address}:{PROBE_PORT} connected in {} ms",
            first["connect_ms"].as_i64().unwrap_or(-1)
        ),
        Some(other) => format!(
            "TCP {address}:{PROBE_PORT} FAILED: {}",
            first["connect_error"].as_str().unwrap_or(other)
        ),
        None => format!("TCP {address}:{PROBE_PORT} not tried"),
    };
    format!("DNS {PROBE_HOST} -> {address} in {ms} ms; {tcp}")
}

/// A vphoned RPC, as [`crate::vphoned::Client::rpc`].
pub type Rpc<'a> = &'a dyn Fn(&str, Value, Duration) -> Result<Value, String>;

/// An `en0` that vphone-cli's Mac host pinned to its own network's address (`network.ipv4.set`,
/// marked `managed`) goes back to DHCP, which QEMU's NAT answers. An address the user set in iOS's
/// own Settings is not marked, and vphoned leaves it alone. The line to log, if anything was
/// changed or refused.
pub fn release_managed_address(rpc: Rpc) -> Option<String> {
    let t = Duration::from_secs(30);
    let now = rpc("network.ipv4.get", json!({}), t).ok()?;
    if now["method"] != "manual" || now["managed"] != true {
        return None;
    }
    let was = now["address"].as_str().unwrap_or("?").to_string();
    Some(match rpc("network.ipv4.set", json!({"method": "dhcp"}), t) {
        Ok(_) => format!("network: en0 was held at {was} by the Mac this VM came from; set back to DHCP"),
        Err(e) => format!("network: en0 is held at {was} by the Mac this VM came from, and DHCP could not be set: {e}"),
    })
}

/// The one line of a boot that says whether the VM reaches the internet:
/// `network: en0 10.0.2.15 (QEMU's NAT); DNS apple.com -> 17.253.144.10 in 180 ms; TCP 17.253.144.10:443 connected in 95 ms`.
/// The names are resolved and the connection made by the guest (vphoned's `network.resolve`).
pub fn report(link: &Link, rpc: Rpc) -> String {
    let tested = match link {
        Link::NoInterface(_) => "DNS and TCP not tested".to_string(),
        _ => resolve_words(&rpc(
            "network.resolve",
            json!({"host": PROBE_HOST, "family": "ipv4", "port": PROBE_PORT, "first_only": true, "timeout_ms": 20000}),
            Duration::from_secs(60),
        )),
    };
    format!("network: {}; {tested}", link.words())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_names_the_networks_ipv4_servers_then_the_fallbacks() {
        assert_eq!(servers("192.168.1.1 fe80::1%wlan0 2001:4860:4860::8888"), ["192.168.1.1", "1.1.1.1", "8.8.8.8"]);
        // A mobile network with IPv6 resolvers only, and no network at all.
        assert_eq!(servers("2001:db8::53,2001:db8::54"), FALLBACK_DNS);
        assert_eq!(servers(""), FALLBACK_DNS);
        // Nothing twice, and nothing the NAT cannot reach.
        assert_eq!(servers("8.8.8.8 127.0.0.1 0.0.0.0 8.8.8.8 10.1.1.1"), ["8.8.8.8", "10.1.1.1", "1.1.1.1"]);
        let text = resolv_conf(&servers("192.168.1.1"));
        assert!(text.ends_with("nameserver 192.168.1.1\nnameserver 1.1.1.1\nnameserver 8.8.8.8\n"), "{text}");
        // What libslirp's parser takes: `nameserver`, blanks, the address.
        assert!(text.lines().skip(1).all(|l| l.starts_with("nameserver ") && l[11..].parse::<Ipv4Addr>().is_ok()));
    }

    fn paths(name: &str) -> Paths {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../build/test-fixtures").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Paths { files: dir.clone(), cache: dir.clone(), external: dir.clone(), native_lib: dir }
    }

    #[test]
    fn a_change_of_network_is_a_new_file_and_the_same_servers_are_not_rewritten() {
        use std::os::unix::fs::MetadataExt;
        let p = paths("net-write");
        let file = p.resolv_conf();
        // No file yet: a boot writes the fallbacks and says where.
        let line = boot_line(&p);
        assert!(line.starts_with("network: on") && line.contains("DNS servers 1.1.1.1, 8.8.8.8 in "), "{line}");
        let first = std::fs::metadata(&file).unwrap().ino();
        let line = set_dns_servers(&p, "192.168.1.1 fe80::1").unwrap();
        assert!(line.starts_with("network: DNS servers for the VM: 192.168.1.1, 1.1.1.1, 8.8.8.8 (this phone's network gave: 192.168.1.1 fe80::1"), "{line}");
        assert_eq!(read(&file), ["192.168.1.1", "1.1.1.1", "8.8.8.8"]);
        assert_ne!(std::fs::metadata(&file).unwrap().ino(), first);
        assert_eq!(set_dns_servers(&p, "192.168.1.1"), None);
        assert!(boot_line(&p).contains("DNS servers 192.168.1.1, 1.1.1.1, 8.8.8.8"));
        // The network is gone: the fallbacks.
        assert!(set_dns_servers(&p, "").unwrap().contains("gave: none"));
        assert_eq!(read(&file), FALLBACK_DNS);
        assert!(!file.with_extension("conf.part").exists());
    }

    #[test]
    fn the_guests_link_is_read_from_its_addresses() {
        let of = |list: &[&str]| link(&Ok(json!({"addresses": list})));
        assert_eq!(of(&["lo0 127.0.0.1", "en0 fe80::1c2b%en0", "en0 10.0.2.15"]), Link::Nat("10.0.2.15".into()));
        assert_eq!(of(&["lo0 127.0.0.1", "en0 169.254.12.7"]), Link::Other("169.254.12.7".into()));
        assert_eq!(of(&["en0 192.168.64.3"]), Link::Other("192.168.64.3".into()));
        assert_eq!(of(&["lo0 127.0.0.1", "en0 fe80::1c2b%en0"]), Link::NoAddress);
        assert_eq!(of(&["lo0 127.0.0.1", "lo0 ::1", "utun0 fe80::1"]), Link::NoInterface(vec!["lo0".into(), "utun0".into()]));
        assert!(matches!(link(&Err("device.network: timed out".into())), Link::Unknown(_)));
        assert!(matches!(link(&Ok(json!({"wifi": true}))), Link::Unknown(_)));
        assert!(of(&["en0 169.254.12.7"]).may_settle() && of(&["en0 fe80::1"]).may_settle());
        assert!(!of(&["en0 10.0.2.15"]).may_settle() && !of(&["lo0 127.0.0.1"]).may_settle());
        assert!(!of(&["en0 192.168.64.3"]).may_settle());
    }

    #[test]
    fn the_line_says_what_works_and_what_does_not() {
        let resolved = |connect: Value| {
            move |method: &str, params: Value, _: Duration| -> Result<Value, String> {
                assert_eq!(method, "network.resolve");
                assert_eq!((params["host"].as_str(), params["port"].as_i64()), (Some("apple.com"), Some(443)));
                let mut first = json!({"address": "17.253.144.10", "family": "ipv4"});
                first.as_object_mut().unwrap().extend(connect.as_object().unwrap().clone());
                Ok(json!({"host": "apple.com", "status": "resolved", "elapsed_ms": 180, "addresses": [first]}))
            }
        };
        let nat = Link::Nat("10.0.2.15".into());
        assert_eq!(
            report(&nat, &resolved(json!({"connect": "connected", "connect_ms": 95}))),
            "network: en0 10.0.2.15 (QEMU's NAT); DNS apple.com -> 17.253.144.10 in 180 ms; TCP 17.253.144.10:443 connected in 95 ms"
        );
        let line = report(&nat, &resolved(json!({"connect": "error", "connect_error": "Network is unreachable"})));
        assert!(line.ends_with("TCP 17.253.144.10:443 FAILED: Network is unreachable"), "{line}");
        let line = report(&nat, &|_, _, _| Ok(json!({"status": "error", "error": "nodename nor servname provided, or not known", "elapsed_ms": 30})));
        assert!(line.contains("DNS FAILED: apple.com: nodename nor servname provided"), "{line}");
        let line = report(&nat, &|_, _, _| Ok(json!({"status": "timeout", "elapsed_ms": 20000})));
        assert!(line.contains("DNS FAILED: apple.com did not resolve in 20000 ms"), "{line}");
        let line = report(&nat, &|_, _, _| Err("network.resolve: Unknown method: network.resolve".into()));
        assert!(line.contains("en0 10.0.2.15") && line.contains("has no network.resolve"), "{line}");
        // A card iOS did not bind is said first, and nothing is asked of a network that is not there.
        let line = report(&Link::NoInterface(vec!["lo0".into()]), &|_, _, _| panic!("asked"));
        assert_eq!(line, "network: NO en0: iOS's driver did not bind QEMU's network card (virtio-net-pci); iOS has only lo0; DNS and TCP not tested");
        let line = report(&Link::NoAddress, &|_, _, _| Ok(json!({"status": "timeout", "elapsed_ms": 20000})));
        assert!(line.starts_with("network: NO ADDRESS: en0 is there without an IPv4 address"), "{line}");
    }

    #[test]
    fn only_an_address_the_mac_host_pinned_goes_back_to_dhcp() {
        use std::cell::RefCell;
        let asked = RefCell::new(Vec::new());
        let guest = |get: Value| {
            let asked = &asked;
            move |method: &str, params: Value, _: Duration| -> Result<Value, String> {
                asked.borrow_mut().push(format!("{method} {params}"));
                Ok(if method == "network.ipv4.get" { get.clone() } else { json!({"changed": true}) })
            }
        };
        let line = release_managed_address(&guest(json!({"method": "manual", "managed": true, "address": "192.168.64.3"}))).unwrap();
        assert!(line.contains("192.168.64.3") && line.ends_with("set back to DHCP"), "{line}");
        assert_eq!(asked.borrow().last().unwrap(), r#"network.ipv4.set {"method":"dhcp"}"#);
        asked.borrow_mut().clear();
        // Set by the user in iOS's Settings, or DHCP already: nothing is changed.
        assert_eq!(release_managed_address(&guest(json!({"method": "manual", "managed": false, "address": "10.0.2.50"}))), None);
        assert_eq!(release_managed_address(&guest(json!({"method": "dhcp", "managed": false}))), None);
        assert_eq!(asked.borrow().len(), 2);
        assert_eq!(release_managed_address(&|_, _, _| Err("Unknown method".into())), None);
    }
}
