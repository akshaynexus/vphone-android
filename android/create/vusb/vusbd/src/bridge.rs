//! The bus side: enumeration, stage following, and clients' URBs. This is `class USBIPBridge` in
//! tools/vusb-usbip.py with the USB/IP protocol replaced by the broker protocol, and the part the
//! Linux kernel did on attach (port reset, descriptor reads, SET_CONFIGURATION) done here.

use crate::link::{Link, LinkError, MSG_PAYLOAD_MAX};
use std::collections::{HashMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use vusb_common::{read_frame, status, write_frame, DeviceBlob, OpCode, C2S, PROTOCOL_VERSION, S2C};

const REDISCOVER_DELAY: Duration = Duration::from_millis(200);
const USB_CLASS_VENDOR: u8 = 0xff;
const USB_DT_DEVICE: u16 = 0x0100;
const USB_DT_CONFIG: u16 = 0x0200;
const LIBUSB_ERROR_IO: i32 = -1;
const LIBUSB_ERROR_NO_DEVICE: i32 = -4;
const LIBUSB_ERROR_NOT_FOUND: i32 = -5;
const LIBUSB_ERROR_BUSY: i32 = -6;
const LIBUSB_ERROR_TIMEOUT: i32 = -7;
const LIBUSB_ERROR_PIPE: i32 = -9;

/// `bmRequestType, bRequest, wValue, wIndex, wLength`: a standard GET_DESCRIPTOR.
fn get_descriptor(value: u16, length: u16) -> [u8; 8] {
    setup(0x80, 6, value, 0, length)
}

fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> [u8; 8] {
    let v = value.to_le_bytes();
    let i = index.to_le_bytes();
    let l = length.to_le_bytes();
    [request_type, request, v[0], v[1], i[0], i[1], l[0], l[1]]
}

struct Device {
    blob: DeviceBlob,
    pci_stage: bool,
    endpoint_sizes: HashMap<u8, u16>,
    interfaces: Vec<(u8, u8)>, // (config value, interface number)
    claims: Mutex<HashMap<u8, u64>>,
    active_config: Mutex<u8>,
}

struct Client {
    id: u64,
    write: Mutex<UnixStream>,
    gone: AtomicBool,
    urbs: Mutex<HashMap<u64, Arc<Urb>>>,
}

impl Client {
    fn send(&self, msg: &S2C) {
        if self.gone.load(Ordering::SeqCst) {
            return;
        }
        let frame = msg.encode();
        let mut w = self.write.lock().unwrap();
        if write_frame(&mut *w, &frame).is_err() {
            self.gone.store(true, Ordering::SeqCst);
        }
    }
}

struct Urb {
    client: Arc<Client>,
    id: u64,
    gen: u32,
    address: u8,
    length: u32,
    setup: [u8; 8],
    payload: Vec<u8>,
    cancel: AtomicBool,
}

impl Urb {
    fn complete(&self, status: i32, actual: u32, data: Vec<u8>) {
        self.client.urbs.lock().unwrap().remove(&self.id);
        self.client.send(&S2C::Complete {
            id: self.id,
            status,
            actual,
            data,
        });
    }
}

enum Job {
    Urb(Arc<Urb>),
    Op {
        client: Arc<Client>,
        opid: u32,
        gen: u32,
        op: OpCode,
        arg: u8,
    },
}

/// One FIFO per endpoint address: URBs on one endpoint complete in submission order, whole.
struct Worker {
    queue: Mutex<VecDeque<Job>>,
    cond: Condvar,
}

impl Worker {
    fn push(&self, job: Job) {
        self.queue.lock().unwrap().push_back(job);
        self.cond.notify_one();
    }

    fn pop(&self) -> Job {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(j) = q.pop_front() {
                return j;
            }
            q = self.cond.wait(q).unwrap();
        }
    }

    /// Drop a URB that has not started, if it has not.
    fn remove_queued(&self, urb: &Arc<Urb>) -> bool {
        let mut q = self.queue.lock().unwrap();
        let pos = q
            .iter()
            .position(|j| matches!(j, Job::Urb(u) if Arc::ptr_eq(u, urb)));
        match pos {
            Some(p) => {
                q.remove(p);
                true
            }
            None => false,
        }
    }
}

struct Bus {
    device: Option<Arc<Device>>,
    clients: Vec<Arc<Client>>,
}

pub struct Bridge {
    link: Arc<Link>,
    stage_timeout: Duration,
    out_window: usize,
    sent: std::sync::atomic::AtomicU64,
    sent_since: Mutex<Instant>,
    verbose: bool,
    bus: Mutex<Bus>,
    workers: Mutex<HashMap<u8, Arc<Worker>>>,
    next_address: AtomicU32,
    next_client: AtomicU64,
    next_gen: AtomicU32,
}

impl Bridge {
    pub fn new(link: Arc<Link>, stage_timeout: Duration, verbose: bool) -> Self {
        Bridge {
            link,
            stage_timeout,
            // VUSB_OUT_WINDOW=1 sends one packet per round trip, for comparison.
            out_window: std::env::var("VUSB_OUT_WINDOW")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|w| (1..=48).contains(w))
                .unwrap_or(crate::link::OUT_WINDOW),
            sent: std::sync::atomic::AtomicU64::new(0),
            sent_since: Mutex::new(Instant::now()),
            verbose,
            bus: Mutex::new(Bus {
                device: None,
                clients: Vec::new(),
            }),
            workers: Mutex::new(HashMap::new()),
            next_address: AtomicU32::new(1),
            next_client: AtomicU64::new(1),
            next_gen: AtomicU32::new(1),
        }
    }

    /// Read everything Linux's enumeration does, in its order, then present the device.
    fn enumerate(&self, reset_timeout: Duration) -> Result<Arc<Device>, String> {
        let link = &self.link;
        link.reset(reset_timeout).map_err(|e| e.to_string())?;
        // tools/vusb-usbip.py's discover(): the device, and the first configuration.
        let device = self.control_in(USB_DT_DEVICE, 18)?;
        if device.len() < 18 || device[1] != 1 {
            return Err("guest returned an invalid USB device descriptor".into());
        }
        let head = self.control_in(USB_DT_CONFIG, 9)?;
        if head.len() < 9 {
            return Err("short USB configuration descriptor".into());
        }
        let total = u16::from_le_bytes([head[2], head[3]]);
        if !(9..=4096).contains(&total) {
            return Err(format!("invalid USB configuration length {total}"));
        }
        self.control_in(USB_DT_CONFIG, total)?;

        // What the kernel does after usbip attach: port reset, then GET_DESCRIPTOR(DEVICE, 64), the
        // device descriptor, and every configuration. DFU's manifestation waits for this reset.
        link.reset(link.ctrl_timeout).map_err(|e| e.to_string())?;
        self.control_in(USB_DT_DEVICE, 64)?;
        let device = self.control_in(USB_DT_DEVICE, 18)?;
        let mut device_descriptor = [0u8; 18];
        if device.len() < 18 || device[1] != 1 {
            return Err("guest returned an invalid USB device descriptor".into());
        }
        device_descriptor.copy_from_slice(&device[..18]);
        let num_configs = device_descriptor[17];
        let mut configs = Vec::new();
        for i in 0..num_configs as u16 {
            let head = self.control_in(USB_DT_CONFIG | i, 9)?;
            if head.len() < 9 || head[1] != 2 {
                return Err(format!("invalid configuration descriptor {i}"));
            }
            let total = u16::from_le_bytes([head[2], head[3]]);
            if !(9..=4096).contains(&total) {
                return Err(format!("invalid USB configuration length {total}"));
            }
            let config = self.control_in(USB_DT_CONFIG | i, total)?;
            if config.len() < total as usize {
                return Err(format!(
                    "short USB configuration: got {}, expected {total}",
                    config.len()
                ));
            }
            configs.push(config[..total as usize].to_vec());
        }
        if configs.is_empty() {
            return Err("device has no configurations".into());
        }

        // The kernel's usb_choose_configuration: the first configuration whose first interface
        // is not vendor-specific, else the first configuration.
        let mut choice = config_value(&configs[0]);
        for c in &configs {
            if first_interface_class(c) != Some(USB_CLASS_VENDOR) {
                choice = config_value(c);
                break;
            }
        }
        let set_config = setup(0x00, 9, u16::from(choice), 0, 0);
        let active_config = match link.control(&set_config, &[], None) {
            Ok(_) => choice,
            Err(e @ LinkError::NoDevice) => return Err(e.to_string()),
            Err(_) => 0,
        };

        let vid = u16::from_le_bytes([device_descriptor[8], device_descriptor[9]]);
        let pid = u16::from_le_bytes([device_descriptor[10], device_descriptor[11]]);
        let mut endpoint_sizes = HashMap::new();
        let mut interfaces = Vec::new();
        for c in &configs {
            parse_config(c, &mut endpoint_sizes, &mut interfaces);
        }
        let gen = self.next_gen.fetch_add(1, Ordering::SeqCst);
        let address = self.next_address.fetch_add(1, Ordering::SeqCst) as u8 % 127 + 1;
        self.sent.store(0, Ordering::SeqCst);
        *self.sent_since.lock().unwrap() = Instant::now();
        eprintln!(
            "vusbd: USB device discovered: {vid:04x}:{pid:04x}, {} configuration(s), active {active_config}",
            configs.len()
        );
        // Both the RestoreOS (12ac) and the SSH ramdisk (12a8) use AppleUSBDeviceMux with 32 KiB
        // buffers: one whole mux packet per USB transfer. Only the DFU/recovery MMIO controller
        // needs splitting.
        let pci_stage = pid == 0x12ac || pid == 0x12a8;
        Ok(Arc::new(Device {
            blob: DeviceBlob {
                gen,
                address,
                active_config,
                device_descriptor,
                configs,
            },
            pci_stage,
            endpoint_sizes,
            interfaces,
            claims: Mutex::new(HashMap::new()),
            active_config: Mutex::new(active_config),
        }))
    }

    fn control_in(&self, value: u16, length: u16) -> Result<Vec<u8>, String> {
        self.link
            .control(&get_descriptor(value, length), &[], None)
            .map_err(|e| e.to_string())
    }

    /// Each guest boot stage presents a new device: on every unplug, drop the old one and
    /// enumerate the next.
    pub fn follow_stages(self: Arc<Self>) {
        loop {
            self.link.wait_detached();
            self.link.clear_detached();
            self.publish(None);
            loop {
                thread::sleep(REDISCOVER_DELAY);
                self.link.clear_detached();
                // QEMU holds the reset until the new stage's driver posts its first receive
                // buffer, so wait for that one rather than queueing more behind it.
                match self.enumerate(self.stage_timeout) {
                    Ok(device) => {
                        if self.link.is_detached() {
                            continue; // the guest moved on again mid-discovery
                        }
                        self.publish(Some(device));
                        break;
                    }
                    Err(e) => eprintln!("vusbd: waiting for the next stage's device ({e})"),
                }
            }
        }
    }

    fn publish(&self, device: Option<Arc<Device>>) {
        let mut bus = self.bus.lock().unwrap();
        if let Some(old) = bus.device.take() {
            for c in &bus.clients {
                c.send(&S2C::DeviceLeft { gen: old.blob.gen });
            }
        }
        if let Some(d) = &device {
            for c in &bus.clients {
                c.send(&S2C::DeviceAdded(d.blob.clone()));
            }
        }
        bus.device = device;
    }

    fn current(&self, gen: u32) -> Option<Arc<Device>> {
        let bus = self.bus.lock().unwrap();
        bus.device.as_ref().filter(|d| d.blob.gen == gen).cloned()
    }

    fn worker(self: &Arc<Self>, address: u8) -> Arc<Worker> {
        let mut workers = self.workers.lock().unwrap();
        if let Some(w) = workers.get(&address) {
            return w.clone();
        }
        let w = Arc::new(Worker {
            queue: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
        });
        workers.insert(address, w.clone());
        let worker = w.clone();
        let this = self.clone();
        thread::spawn(move || loop {
            match worker.pop() {
                Job::Urb(urb) => this.run_urb(&urb),
                Job::Op {
                    client,
                    opid,
                    gen,
                    op,
                    arg,
                } => {
                    let result = this.run_op(gen, op, arg);
                    client.send(&S2C::OpReply { opid, result });
                }
            }
        });
        w
    }

    fn run_urb(&self, urb: &Arc<Urb>) {
        if urb.cancel.load(Ordering::SeqCst) {
            urb.complete(status::CANCELLED, 0, Vec::new());
            return;
        }
        let Some(device) = self.current(urb.gen) else {
            urb.complete(status::NO_DEVICE, 0, Vec::new());
            return;
        };
        let (st, actual, data) = match self.transfer(&device, urb) {
            Ok((actual, data)) => (status::COMPLETED, actual as u32, data),
            Err(LinkError::Cancelled) => {
                let abort_address = if urb.address == 0 {
                    urb.setup[0] & 0x80
                } else {
                    urb.address
                };
                self.link.abort(abort_address);
                (status::CANCELLED, 0, Vec::new())
            }
            Err(LinkError::Stall) => (status::STALL, 0, Vec::new()),
            Err(LinkError::NoDevice) => (status::NO_DEVICE, 0, Vec::new()),
            Err(LinkError::Timeout) => (status::TIMED_OUT, 0, Vec::new()),
            Err(LinkError::Protocol(e)) => {
                eprintln!("vusbd: URB failed: {e}");
                (status::ERROR, 0, Vec::new())
            }
        };
        if self.verbose && urb.address != 0 && !data.is_empty() {
            log_mux(urb, &data);
        }
        urb.complete(st, actual, data);
    }

    /// A line in the log per 8 MiB sent to the DFU/recovery controller: the only sign of how far
    /// a transfer of hundreds of megabytes has got.
    fn note_sent(&self, bytes: u64) {
        const STEP: u64 = 8 << 20;
        let before = self.sent.fetch_add(bytes, Ordering::SeqCst);
        let total = before + bytes;
        if before / STEP != total / STEP {
            let elapsed = self.sent_since.lock().unwrap().elapsed().as_secs_f64();
            eprintln!("vusbd: sent {} MiB in {elapsed:.1} s", total >> 20);
        }
    }

    /// Returns (actual_length, IN data).
    fn transfer(&self, device: &Device, urb: &Urb) -> Result<(usize, Vec<u8>), LinkError> {
        let cancel = Some(&urb.cancel);
        if urb.address == 0 {
            let direction_in = urb.setup[0] & 0x80 != 0;
            let mut data = self.link.control(&urb.setup, &urb.payload, cancel)?;
            if direction_in {
                data.truncate(urb.length as usize);
                return Ok((data.len(), data));
            }
            return Ok((urb.payload.len(), Vec::new()));
        }
        if urb.address & 0x80 != 0 {
            let data = self
                .link
                .transfer_in(urb.address, urb.length as usize, cancel)?;
            return Ok((data.len(), data));
        }
        if urb.payload.is_empty() {
            // The ZLP delimiting a preceding URB of a multiple of wMaxPacketSize. It must reach
            // the device: without it the controller coalesces consecutive 32 KiB mux packets.
            self.link.transfer_out(urb.address, &[], cancel)?;
            return Ok((0, Vec::new()));
        }
        if device.pci_stage {
            // RestoreOS: AppleUSBDeviceMux requires one complete mux packet per USB transfer and
            // drops short ones, so the whole URB is one message.
            if self.verbose {
                log_mux(urb, &urb.payload);
            }
            self.link.transfer_out(urb.address, &urb.payload, cancel)?;
        } else {
            // DFU/recovery MMIO controller: its receive buffer is small.
            let packet_size = *device.endpoint_sizes.get(&urb.address).unwrap_or(&512) as usize;
            let packet_size = if packet_size == 0 { 512 } else { packet_size };
            self.link
                .transfer_out_stream(urb.address, &urb.payload, packet_size, self.out_window, cancel)?;
            self.note_sent(urb.payload.len() as u64);
        }
        Ok((urb.payload.len(), Vec::new()))
    }

    fn run_op(&self, gen: u32, op: OpCode, arg: u8) -> i32 {
        let Some(device) = self.current(gen) else {
            return LIBUSB_ERROR_NO_DEVICE;
        };
        match op {
            OpCode::SetConfig => self.set_config(&device, arg),
            OpCode::Reset => self.reset_device(&device),
            OpCode::Claim | OpCode::Release => unreachable!("handled on the client thread"),
        }
    }

    fn set_config(&self, device: &Device, value: u8) -> i32 {
        if !device.claims.lock().unwrap().is_empty() && value != *device.active_config.lock().unwrap()
        {
            return LIBUSB_ERROR_BUSY;
        }
        match self.link.control(&setup(0, 9, u16::from(value), 0, 0), &[], None) {
            Ok(_) => {
                *device.active_config.lock().unwrap() = value;
                let bus = self.bus.lock().unwrap();
                for c in &bus.clients {
                    c.send(&S2C::ConfigChanged {
                        gen: device.blob.gen,
                        value,
                    });
                }
                0
            }
            Err(LinkError::Stall) => LIBUSB_ERROR_PIPE,
            Err(LinkError::NoDevice) => LIBUSB_ERROR_NO_DEVICE,
            Err(_) => LIBUSB_ERROR_IO,
        }
    }

    /// libusb_reset_device: the bus reset, then the kernel's re-enumeration of the device.
    fn reset_device(&self, device: &Device) -> i32 {
        let ctrl = self.link.ctrl_timeout;
        match self.link.reset(ctrl) {
            Ok(()) => {}
            Err(LinkError::NoDevice) => return LIBUSB_ERROR_NOT_FOUND,
            Err(LinkError::Timeout) => return LIBUSB_ERROR_TIMEOUT,
            Err(_) => return LIBUSB_ERROR_IO,
        }
        // DFU's manifestation may start the next stage right after this reset; losing the device
        // here is the expected outcome, reported as "must re-enumerate".
        if self.control_in(USB_DT_DEVICE, 64).is_err() {
            return LIBUSB_ERROR_NOT_FOUND;
        }
        let active = *device.active_config.lock().unwrap();
        if active != 0 {
            let set = setup(0, 9, u16::from(active), 0, 0);
            if self.link.control(&set, &[], None).is_err() {
                return LIBUSB_ERROR_NOT_FOUND;
            }
        }
        0
    }

    fn claim(&self, client: &Client, gen: u32, interface: u8, release: bool) -> i32 {
        let Some(device) = self.current(gen) else {
            return LIBUSB_ERROR_NO_DEVICE;
        };
        let active = *device.active_config.lock().unwrap();
        let mut claims = device.claims.lock().unwrap();
        if release {
            return if claims.get(&interface) == Some(&client.id) {
                claims.remove(&interface);
                0
            } else {
                LIBUSB_ERROR_NOT_FOUND
            };
        }
        // Unconfigured devices have no interfaces; the claim would be refused by usbfs.
        if active != 0 && !device.interfaces.iter().any(|&i| i == (active, interface)) {
            return LIBUSB_ERROR_NOT_FOUND;
        }
        match claims.get(&interface) {
            Some(&owner) if owner != client.id => LIBUSB_ERROR_BUSY,
            _ => {
                claims.insert(interface, client.id);
                0
            }
        }
    }

    /// One libusb_context in another process.
    pub fn serve_client(self: Arc<Self>, stream: UnixStream) {
        let mut reader = match stream.try_clone() {
            Ok(r) => r,
            Err(_) => return,
        };
        let client = Arc::new(Client {
            id: self.next_client.fetch_add(1, Ordering::SeqCst),
            write: Mutex::new(stream),
            gone: AtomicBool::new(false),
            urbs: Mutex::new(HashMap::new()),
        });
        // The handshake, then the current device, before any bus change can overtake it.
        match read_frame(&mut reader).and_then(|f| C2S::decode(&f)) {
            Ok(C2S::Hello { version }) if version == PROTOCOL_VERSION => {}
            other => {
                eprintln!("vusbd: bad client handshake: {other:?}");
                return;
            }
        }
        {
            let mut bus = self.bus.lock().unwrap();
            if let Some(d) = &bus.device {
                // A snapshot, with the configuration as it stands now.
                let mut blob = d.blob.clone();
                blob.active_config = *d.active_config.lock().unwrap();
                client.send(&S2C::DeviceAdded(blob));
            }
            bus.clients.push(client.clone());
        }
        loop {
            let frame = match read_frame(&mut reader) {
                Ok(f) => f,
                Err(_) => break,
            };
            let msg = match C2S::decode(&frame) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("vusbd: bad client frame: {e}");
                    break;
                }
            };
            match msg {
                C2S::Hello { .. } => {}
                C2S::Submit {
                    id,
                    gen,
                    endpoint,
                    length,
                    setup,
                    payload,
                    ..
                } => {
                    let address = endpoint;
                    let urb = Arc::new(Urb {
                        client: client.clone(),
                        id,
                        gen,
                        address,
                        length,
                        setup,
                        payload,
                        cancel: AtomicBool::new(false),
                    });
                    if urb.payload.len() > MSG_PAYLOAD_MAX {
                        urb.complete(status::ERROR, 0, Vec::new());
                        continue;
                    }
                    client.urbs.lock().unwrap().insert(id, urb.clone());
                    self.worker(address).push(Job::Urb(urb));
                }
                C2S::Cancel { id } => {
                    let urb = client.urbs.lock().unwrap().get(&id).cloned();
                    if let Some(urb) = urb {
                        if self.worker(urb.address).remove_queued(&urb) {
                            urb.complete(status::CANCELLED, 0, Vec::new());
                        } else {
                            urb.cancel.store(true, Ordering::SeqCst);
                            self.link.notify();
                        }
                    }
                }
                C2S::Op {
                    opid,
                    gen,
                    op,
                    arg,
                } => match op {
                    OpCode::Claim | OpCode::Release => {
                        let result = self.claim(&client, gen, arg, op == OpCode::Release);
                        client.send(&S2C::OpReply { opid, result });
                    }
                    OpCode::SetConfig | OpCode::Reset => {
                        // Serialized with control transfers.
                        self.worker(0).push(Job::Op {
                            client: client.clone(),
                            opid,
                            gen,
                            op,
                            arg,
                        });
                    }
                },
            }
        }
        client.gone.store(true, Ordering::SeqCst);
        let urbs: Vec<_> = client.urbs.lock().unwrap().values().cloned().collect();
        for urb in urbs {
            urb.cancel.store(true, Ordering::SeqCst);
        }
        self.link.notify();
        let mut bus = self.bus.lock().unwrap();
        if let Some(d) = &bus.device {
            d.claims.lock().unwrap().retain(|_, owner| *owner != client.id);
        }
        bus.clients.retain(|c| !Arc::ptr_eq(c, &client));
    }
}

fn config_value(config: &[u8]) -> u8 {
    config.get(5).copied().unwrap_or(1)
}

fn first_interface_class(config: &[u8]) -> Option<u8> {
    let mut offset = 0;
    while offset + 2 <= config.len() {
        let size = config[offset] as usize;
        if size < 2 || offset + size > config.len() {
            return None;
        }
        if config[offset + 1] == 4 && size >= 9 && config[offset + 3] == 0 {
            return Some(config[offset + 5]);
        }
        offset += size;
    }
    None
}

fn parse_config(
    config: &[u8],
    endpoint_sizes: &mut HashMap<u8, u16>,
    interfaces: &mut Vec<(u8, u8)>,
) {
    let value = config_value(config);
    let mut offset = 0;
    while offset + 2 <= config.len() {
        let size = config[offset] as usize;
        let kind = config[offset + 1];
        if size < 2 || offset + size > config.len() {
            return;
        }
        if kind == 4 && size >= 9 {
            let entry = (value, config[offset + 2]);
            if !interfaces.contains(&entry) {
                interfaces.push(entry);
            }
        } else if kind == 5 && size >= 7 {
            let address = config[offset + 2];
            let max_packet = u16::from_le_bytes([config[offset + 4], config[offset + 5]]) & 0x7ff;
            endpoint_sizes.insert(address, max_packet);
        }
        offset += size;
    }
}

/// The 16-byte usbmux header, so sequence gaps show in a debug log.
fn log_mux(urb: &Urb, data: &[u8]) {
    if data.len() >= 16 && (data[8..12] == [0xfe, 0xed, 0xfa, 0xce] || data[8..12] == [0xfa, 0xce, 0xfa, 0xce])
    {
        let dir = if urb.address & 0x80 != 0 { "in" } else { "out" };
        eprintln!(
            "vusbd: URB {:#x} mux {dir} proto {} len {} tx {} rx {} (transfer {})",
            urb.id,
            u32::from_be_bytes(data[0..4].try_into().unwrap()),
            u32::from_be_bytes(data[4..8].try_into().unwrap()),
            u16::from_be_bytes(data[12..14].try_into().unwrap()),
            u16::from_be_bytes(data[14..16].try_into().unwrap()),
            data.len()
        );
    }
}

