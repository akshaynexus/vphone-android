//! Parse a raw configuration descriptor into libusb's public `libusb_config_descriptor` tree.

use std::os::raw::{c_int, c_uchar};
use std::ptr;

#[repr(C)]
pub struct RawEndpoint {
    pub bLength: u8,
    pub bDescriptorType: u8,
    pub bEndpointAddress: u8,
    pub bmAttributes: u8,
    pub wMaxPacketSize: u16,
    pub bInterval: u8,
    pub bRefresh: u8,
    pub bSynchAddress: u8,
    pub extra: *const c_uchar,
    pub extra_length: c_int,
}

#[repr(C)]
pub struct RawInterfaceDescriptor {
    pub bLength: u8,
    pub bDescriptorType: u8,
    pub bInterfaceNumber: u8,
    pub bAlternateSetting: u8,
    pub bNumEndpoints: u8,
    pub bInterfaceClass: u8,
    pub bInterfaceSubClass: u8,
    pub bInterfaceProtocol: u8,
    pub iInterface: u8,
    pub endpoint: *const RawEndpoint,
    pub extra: *const c_uchar,
    pub extra_length: c_int,
}

#[repr(C)]
pub struct RawInterface {
    pub altsetting: *const RawInterfaceDescriptor,
    pub num_altsetting: c_int,
}

#[repr(C)]
pub struct RawConfig {
    pub bLength: u8,
    pub bDescriptorType: u8,
    pub wTotalLength: u16,
    pub bNumInterfaces: u8,
    pub bConfigurationValue: u8,
    pub iConfiguration: u8,
    pub bmAttributes: u8,
    pub MaxPower: u8,
    pub interface: *const RawInterface,
    pub extra: *const c_uchar,
    pub extra_length: c_int,
}

/// The owner of everything a `RawConfig` points at; the config is its first field.
#[repr(C)]
struct Tree {
    config: RawConfig,
    interfaces: Vec<RawInterface>,
    altsettings: Vec<Vec<RawInterfaceDescriptor>>,
    #[allow(dead_code)] // the storage behind `config`'s endpoint pointers
    endpoints: Vec<Vec<RawEndpoint>>,
    extras: Vec<Vec<u8>>,
}

struct AltBuilder {
    descriptor: RawInterfaceDescriptor,
    endpoints: Vec<RawEndpoint>,
    ep_extras: Vec<Vec<u8>>,
    extra: Vec<u8>,
}

fn alt_descriptor(d: &[u8]) -> RawInterfaceDescriptor {
    RawInterfaceDescriptor {
        bLength: d[0],
        bDescriptorType: d[1],
        bInterfaceNumber: d[2],
        bAlternateSetting: d[3],
        bNumEndpoints: d[4],
        bInterfaceClass: d[5],
        bInterfaceSubClass: d[6],
        bInterfaceProtocol: d[7],
        iInterface: d[8],
        endpoint: ptr::null(),
        extra: ptr::null(),
        extra_length: 0,
    }
}

fn endpoint(d: &[u8]) -> RawEndpoint {
    RawEndpoint {
        bLength: d[0],
        bDescriptorType: d[1],
        bEndpointAddress: d[2],
        bmAttributes: d[3],
        wMaxPacketSize: u16::from_le_bytes([d[4], d[5]]),
        bInterval: d[6],
        bRefresh: d.get(7).copied().unwrap_or(0),
        bSynchAddress: d.get(8).copied().unwrap_or(0),
        extra: ptr::null(),
        extra_length: 0,
    }
}

pub fn parse_config(data: &[u8]) -> Option<*mut RawConfig> {
    if data.len() < 9 || data[1] != 2 {
        return None;
    }
    let mut groups: Vec<(u8, Vec<AltBuilder>)> = Vec::new();
    let mut config_extra = Vec::new();
    let mut extras: Vec<Vec<u8>> = Vec::new();
    let mut offset = usize::from(data[0]).max(9);

    while offset + 2 <= data.len() {
        let size = usize::from(data[offset]);
        if size < 2 || offset + size > data.len() {
            break;
        }
        let d = &data[offset..offset + size];
        match d[1] {
            4 if size >= 9 => {
                let number = d[2];
                let builder = AltBuilder {
                    descriptor: alt_descriptor(d),
                    endpoints: Vec::new(),
                    ep_extras: Vec::new(),
                    extra: Vec::new(),
                };
                match groups.iter_mut().find(|(n, _)| *n == number) {
                    Some((_, alts)) => alts.push(builder),
                    None => groups.push((number, vec![builder])),
                }
            }
            5 if size >= 7 => {
                if let Some((_, alts)) = groups.last_mut() {
                    if let Some(a) = alts.last_mut() {
                        a.endpoints.push(endpoint(d));
                        a.ep_extras.push(Vec::new());
                    }
                }
            }
            _ => match groups.last_mut().and_then(|(_, alts)| alts.last_mut()) {
                Some(a) => match a.ep_extras.last_mut() {
                    Some(e) => e.extend_from_slice(d),
                    None => a.extra.extend_from_slice(d),
                },
                None => config_extra.extend_from_slice(d),
            },
        }
        offset += size;
    }

    extras.push(config_extra);
    let config_extra_index = extras.len() - 1;

    let mut tree = Box::new(Tree {
        config: RawConfig {
            bLength: data[0],
            bDescriptorType: data[1],
            wTotalLength: u16::from_le_bytes([data[2], data[3]]),
            bNumInterfaces: 0,
            bConfigurationValue: data[5],
            iConfiguration: data[6],
            bmAttributes: data[7],
            MaxPower: data[8],
            interface: ptr::null(),
            extra: ptr::null(),
            extra_length: 0,
        },
        interfaces: Vec::new(),
        altsettings: Vec::new(),
        endpoints: Vec::new(),
        extras: Vec::new(),
    });

    let mut alt_extra_indices = Vec::new();
    for (_, alts) in groups {
        let mut altsettings = Vec::with_capacity(alts.len());
        for mut a in alts {
            extras.push(std::mem::take(&mut a.extra));
            let index = extras.len() - 1;
            alt_extra_indices.push(index);
            let mut endpoints = a.endpoints;
            for (ep, ex) in endpoints.iter_mut().zip(a.ep_extras) {
                if !ex.is_empty() {
                    extras.push(ex);
                    let v = extras.last().unwrap();
                    ep.extra = v.as_ptr();
                    ep.extra_length = v.len() as c_int;
                }
            }
            a.descriptor.bNumEndpoints = endpoints.len() as u8;
            a.descriptor.endpoint = endpoints.as_ptr();
            tree.endpoints.push(endpoints);
            altsettings.push(a.descriptor);
        }
        tree.altsettings.push(altsettings);
    }

    // Now that `extras` no longer reallocates, the pointers into it are stable.
    tree.extras = extras;
    tree.config.extra = tree.extras[config_extra_index].as_ptr();
    tree.config.extra_length = tree.extras[config_extra_index].len() as c_int;

    let mut n = 0usize;
    for alts in tree.altsettings.iter_mut() {
        for a in alts.iter_mut() {
            let v = &tree.extras[alt_extra_indices[n]];
            a.extra = v.as_ptr();
            a.extra_length = v.len() as c_int;
            n += 1;
        }
    }

    let interfaces: Vec<RawInterface> = tree
        .altsettings
        .iter()
        .map(|alts| RawInterface {
            altsetting: alts.as_ptr(),
            num_altsetting: alts.len() as c_int,
        })
        .collect();
    tree.config.bNumInterfaces = interfaces.len() as u8;
    tree.interfaces = interfaces;
    tree.config.interface = tree.interfaces.as_ptr();

    let raw = Box::into_raw(tree);
    Some(raw as *mut RawConfig)
}

/// # Safety
/// `config` must come from [`parse_config`], or be null.
pub unsafe fn free_config(config: *mut RawConfig) {
    if !config.is_null() {
        drop(Box::from_raw(config as *mut Tree));
    }
}

/// wMaxPacketSize of `endpoint` in a raw configuration.
pub fn max_packet_size(config: &[u8], endpoint: u8) -> Option<i32> {
    let mut offset = 0;
    while offset + 2 <= config.len() {
        let size = usize::from(config[offset]);
        if size < 2 || offset + size > config.len() {
            return None;
        }
        if config[offset + 1] == 5 && size >= 7 && config[offset + 2] == endpoint {
            return Some(i32::from(u16::from_le_bytes([
                config[offset + 4],
                config[offset + 5],
            ])));
        }
        offset += size;
    }
    None
}
