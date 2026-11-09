//! The libusb-1.0 C ABI, for the subset libirecovery and usbmuxd use, whose only device is the
//! guest's virtual USB device. The device lives in the broker process (vusbd); this library is one
//! broker client per `libusb_context`, reached through the socket named by VPHONE_VUSB_SOCKET.
//!
//! An I/O thread per context reads the broker. As in the kernel, completed IN data is placed in the
//! transfer's buffer from that thread, and the callback runs from `libusb_handle_events*` on the
//! caller's thread.

#![allow(non_camel_case_types, non_snake_case, clippy::missing_safety_doc)]

mod descriptors;

use descriptors::{free_config, parse_config, RawConfig};
use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_char, c_int, c_short, c_uchar, c_uint, c_void};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use vusb_common::{status, transfer_type, DeviceBlob, OpCode, C2S, PROTOCOL_VERSION, S2C};

pub const LIBUSB_SUCCESS: c_int = 0;
pub const LIBUSB_ERROR_IO: c_int = -1;
pub const LIBUSB_ERROR_INVALID_PARAM: c_int = -2;
pub const LIBUSB_ERROR_ACCESS: c_int = -3;
pub const LIBUSB_ERROR_NO_DEVICE: c_int = -4;
pub const LIBUSB_ERROR_NOT_FOUND: c_int = -5;
pub const LIBUSB_ERROR_BUSY: c_int = -6;
pub const LIBUSB_ERROR_TIMEOUT: c_int = -7;
pub const LIBUSB_ERROR_OVERFLOW: c_int = -8;
pub const LIBUSB_ERROR_PIPE: c_int = -9;
pub const LIBUSB_ERROR_INTERRUPTED: c_int = -10;
pub const LIBUSB_ERROR_NO_MEM: c_int = -11;
pub const LIBUSB_ERROR_NOT_SUPPORTED: c_int = -12;
pub const LIBUSB_ERROR_OTHER: c_int = -99;

const LIBUSB_TRANSFER_TYPE_CONTROL: u8 = 0;
const LIBUSB_TRANSFER_TYPE_ISOCHRONOUS: u8 = 1;
const LIBUSB_TRANSFER_TYPE_BULK: u8 = 2;
const LIBUSB_TRANSFER_TYPE_INTERRUPT: u8 = 3;

const LIBUSB_TRANSFER_FREE_BUFFER: u8 = 2;
const LIBUSB_TRANSFER_FREE_TRANSFER: u8 = 4;

const LIBUSB_HOTPLUG_MATCH_ANY: c_int = -1;
const LIBUSB_HOTPLUG_ENUMERATE: c_int = 1;
const LIBUSB_HOTPLUG_EVENT_DEVICE_ARRIVED: c_int = 1;
const LIBUSB_HOTPLUG_EVENT_DEVICE_LEFT: c_int = 2;

#[repr(C)]
pub struct libusb_version {
    major: u16,
    minor: u16,
    micro: u16,
    nano: u16,
    rc: *const c_char,
    describe: *const c_char,
}

unsafe impl Sync for libusb_version {}

#[repr(C)]
pub struct libusb_pollfd {
    fd: c_int,
    events: c_short,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct libusb_iso_packet_descriptor {
    length: c_uint,
    actual_length: c_uint,
    status: c_int,
}

pub type libusb_transfer_cb_fn = Option<unsafe extern "C" fn(*mut libusb_transfer)>;

#[repr(C)]
pub struct libusb_transfer {
    pub dev_handle: *mut libusb_device_handle,
    pub flags: u8,
    pub endpoint: c_uchar,
    pub type_: c_uchar,
    pub timeout: c_uint,
    pub status: c_int,
    pub length: c_int,
    pub actual_length: c_int,
    pub callback: libusb_transfer_cb_fn,
    pub user_data: *mut c_void,
    pub buffer: *mut c_uchar,
    pub num_iso_packets: c_int,
    pub iso_packet_desc: [libusb_iso_packet_descriptor; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct libusb_device_descriptor {
    pub bLength: u8,
    pub bDescriptorType: u8,
    pub bcdUSB: u16,
    pub bDeviceClass: u8,
    pub bDeviceSubClass: u8,
    pub bDeviceProtocol: u8,
    pub bMaxPacketSize0: u8,
    pub idVendor: u16,
    pub idProduct: u16,
    pub bcdDevice: u16,
    pub iManufacturer: u8,
    pub iProduct: u8,
    pub iSerialNumber: u8,
    pub bNumConfigurations: u8,
}

pub struct libusb_device {
    shared: Weak<Shared>,
    blob: DeviceBlob,
    active: AtomicU8,
    gone: AtomicBool,
}

pub struct libusb_device_handle {
    device: Arc<libusb_device>,
    shared: Arc<Shared>,
    claimed: Mutex<u32>,
}

pub struct libusb_context {
    shared: Arc<Shared>,
}

type HotplugCb = unsafe extern "C" fn(
    *mut libusb_context,
    *mut libusb_device,
    c_int,
    *mut c_void,
) -> c_int;

struct Hotplug {
    handle: c_int,
    cb: HotplugCb,
    events: c_int,
    vendor: c_int,
    product: c_int,
    class: c_int,
    user_data: usize,
    min_seq: u64,
}

struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

enum Target {
    Async(SendPtr<libusb_transfer>),
    Sync(Arc<SyncSlot>),
}

struct SyncSlot {
    buffer: SendPtr<u8>,
    capacity: usize,
    result: Mutex<Option<(c_int, usize)>>,
    cond: Condvar,
}

struct Pending {
    target: Target,
    deadline: Option<Instant>,
    timed_out: bool,
}

enum Event {
    Transfer {
        transfer: SendPtr<libusb_transfer>,
        status: c_int,
        actual: c_int,
    },
    Hotplug {
        device: Arc<libusb_device>,
        arrived: bool,
        seq: u64,
    },
}

struct OpSlot {
    result: Mutex<Option<c_int>>,
    cond: Condvar,
}

#[derive(Default)]
struct State {
    device: Option<Arc<libusb_device>>,
    pending: HashMap<u64, Pending>,
    by_transfer: HashMap<usize, u64>,
    ops: HashMap<u32, Arc<OpSlot>>,
    events: VecDeque<Event>,
    hotplugs: Vec<Hotplug>,
    seq: u64,
}

struct Shared {
    write: Mutex<Option<UnixStream>>,
    state: Mutex<State>,
    event_fd: c_int,
    wake_fd: c_int,
    stop: AtomicBool,
    io_thread: Mutex<Option<JoinHandle<()>>>,
    next_id: AtomicU64,
    next_op: AtomicU32,
    next_hotplug: AtomicU32,
    event_lock: Mutex<bool>,
    event_cond: Condvar,
    debug: bool,
}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

fn lock(s: &Shared) -> MutexGuard<'_, State> {
    s.state.lock().unwrap()
}

fn eventfd_new() -> c_int {
    unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) }
}

fn signal(fd: c_int) {
    let one: u64 = 1;
    unsafe {
        libc::write(fd, &one as *const u64 as *const c_void, 8);
    }
}

fn drain_fd(fd: c_int) {
    let mut v: u64 = 0;
    unsafe {
        libc::read(fd, &mut v as *mut u64 as *mut c_void, 8);
    }
}

fn send_frame(s: &Shared, msg: &C2S) -> Result<(), ()> {
    use std::io::Write;
    let frame = msg.encode();
    let mut w = s.write.lock().unwrap();
    match w.as_mut() {
        Some(stream) => stream.write_all(&frame).map_err(|_| ()),
        None => Err(()),
    }
}

/// Fail everything in flight: the broker went away.
fn fail_all(s: &Shared) {
    let mut st = lock(s);
    st.by_transfer.clear();
    let pending: Vec<_> = st.pending.drain().collect();
    let mut signal_event = false;
    for (_, p) in pending {
        match p.target {
            Target::Async(t) => {
                st.events.push_back(Event::Transfer {
                    transfer: t,
                    status: status::NO_DEVICE,
                    actual: 0,
                });
                signal_event = true;
            }
            Target::Sync(slot) => {
                *slot.result.lock().unwrap() = Some((status::NO_DEVICE, 0));
                slot.cond.notify_all();
            }
        }
    }
    for (_, op) in st.ops.drain() {
        *op.result.lock().unwrap() = Some(LIBUSB_ERROR_NO_DEVICE);
        op.cond.notify_all();
    }
    if let Some(d) = st.device.take() {
        d.gone.store(true, Ordering::SeqCst);
        st.seq += 1;
        let seq = st.seq;
        st.events.push_back(Event::Hotplug {
            device: d,
            arrived: false,
            seq,
        });
        signal_event = true;
    }
    drop(st);
    if signal_event {
        signal(s.event_fd);
    }
}

fn io_thread(shared: Arc<Shared>, path: String) {
    while !shared.stop.load(Ordering::SeqCst) {
        let stream = match UnixStream::connect(&path) {
            Ok(st) => st,
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let hello = C2S::Hello {
            version: PROTOCOL_VERSION,
        };
        let w = stream.try_clone();
        match w {
            Ok(w) => *shared.write.lock().unwrap() = Some(w),
            Err(_) => continue,
        }
        if send_frame(&shared, &hello).is_err() {
            *shared.write.lock().unwrap() = None;
            continue;
        }
        serve(&shared, stream);
        *shared.write.lock().unwrap() = None;
        fail_all(&shared);
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn serve(shared: &Arc<Shared>, stream: UnixStream) {
    let fd = stream.as_raw_fd();
    let mut buf: Vec<u8> = Vec::with_capacity(128 * 1024);
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let timeout = next_deadline_ms(shared);
        let mut fds = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: shared.wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if fds[1].revents != 0 {
            drain_fd(shared.wake_fd);
        }
        fire_deadlines(shared);
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let got = unsafe {
                libc::recv(
                    fd,
                    chunk.as_mut_ptr() as *mut c_void,
                    chunk.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if got == 0 {
                return;
            }
            if got < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.raw_os_error() == Some(libc::EINTR)
                {
                    continue;
                }
                return;
            }
            buf.extend_from_slice(&chunk[..got as usize]);
            let mut offset = 0;
            while buf.len() - offset >= 4 {
                let len = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
                if len == 0 || len > vusb_common::MAX_FRAME {
                    return;
                }
                if buf.len() - offset < 4 + len {
                    break;
                }
                match S2C::decode(&buf[offset + 4..offset + 4 + len]) {
                    Ok(msg) => on_message(shared, msg),
                    Err(e) => {
                        eprintln!("libusb(vusb): bad broker frame: {e}");
                        return;
                    }
                }
                offset += 4 + len;
            }
            buf.drain(..offset);
        }
    }
}

fn next_deadline_ms(s: &Shared) -> c_int {
    let st = lock(s);
    let mut earliest: Option<Instant> = None;
    for p in st.pending.values() {
        if !p.timed_out {
            if let Some(d) = p.deadline {
                earliest = Some(earliest.map_or(d, |e: Instant| e.min(d)));
            }
        }
    }
    match earliest {
        None => -1,
        Some(d) => {
            let now = Instant::now();
            if d <= now {
                0
            } else {
                (d - now).as_millis().min(i32::MAX as u128) as c_int + 1
            }
        }
    }
}

fn fire_deadlines(s: &Shared) {
    let mut expired = Vec::new();
    {
        let mut st = lock(s);
        let now = Instant::now();
        for (id, p) in st.pending.iter_mut() {
            if !p.timed_out && p.deadline.is_some_and(|d| d <= now) {
                p.timed_out = true;
                expired.push(*id);
            }
        }
    }
    for id in expired {
        let _ = send_frame(s, &C2S::Cancel { id });
    }
}

fn on_message(s: &Shared, msg: S2C) {
    match msg {
        S2C::DeviceAdded(blob) => {
            let device = Arc::new(libusb_device {
                shared: arc_weak(s),
                active: AtomicU8::new(blob.active_config),
                gone: AtomicBool::new(false),
                blob,
            });
            if s.debug {
                eprintln!(
                    "libusb(vusb): device arrived {:04x}:{:04x}",
                    device_vendor(&device),
                    device_product(&device)
                );
            }
            let mut st = lock(s);
            st.device = Some(device.clone());
            st.seq += 1;
            let seq = st.seq;
            st.events.push_back(Event::Hotplug {
                device,
                arrived: true,
                seq,
            });
            drop(st);
            signal(s.event_fd);
        }
        S2C::DeviceLeft { gen } => {
            let mut st = lock(s);
            if st.device.as_ref().is_some_and(|d| d.blob.gen == gen) {
                let d = st.device.take().unwrap();
                d.gone.store(true, Ordering::SeqCst);
                st.seq += 1;
                let seq = st.seq;
                st.events.push_back(Event::Hotplug {
                    device: d,
                    arrived: false,
                    seq,
                });
                drop(st);
                signal(s.event_fd);
            }
        }
        S2C::ConfigChanged { gen, value } => {
            let st = lock(s);
            if let Some(d) = st.device.as_ref().filter(|d| d.blob.gen == gen) {
                d.active.store(value, Ordering::SeqCst);
            }
        }
        S2C::Complete {
            id,
            status: st_code,
            actual,
            data,
        } => {
            let mut st = lock(s);
            let Some(pending) = st.pending.remove(&id) else {
                return;
            };
            let status_code = if st_code == status::CANCELLED && pending.timed_out {
                status::TIMED_OUT
            } else {
                st_code
            };
            match pending.target {
                Target::Async(t) => unsafe {
                    st.by_transfer.remove(&(t.0 as usize));
                    let tr = &*t.0;
                    let (base, cap) = if tr.type_ == LIBUSB_TRANSFER_TYPE_CONTROL {
                        (tr.buffer.add(8), (tr.length - 8).max(0) as usize)
                    } else {
                        (tr.buffer, tr.length.max(0) as usize)
                    };
                    let n = data.len().min(cap);
                    ptr::copy_nonoverlapping(data.as_ptr(), base, n);
                    let delivered = if data.is_empty() {
                        actual as c_int
                    } else {
                        n as c_int
                    };
                    st.events.push_back(Event::Transfer {
                        transfer: t,
                        status: status_code,
                        actual: delivered,
                    });
                    drop(st);
                    signal(s.event_fd);
                },
                Target::Sync(slot) => {
                    let n = data.len().min(slot.capacity);
                    unsafe { ptr::copy_nonoverlapping(data.as_ptr(), slot.buffer.0, n) };
                    let delivered = if data.is_empty() { actual as usize } else { n };
                    *slot.result.lock().unwrap() = Some((status_code, delivered));
                    slot.cond.notify_all();
                }
            }
        }
        S2C::OpReply { opid, result } => {
            let slot = lock(s).ops.remove(&opid);
            if let Some(slot) = slot {
                *slot.result.lock().unwrap() = Some(result);
                slot.cond.notify_all();
            }
        }
    }
}

static WEAK_SELF: Mutex<Vec<(usize, Weak<Shared>)>> = Mutex::new(Vec::new());

fn arc_weak(s: &Shared) -> Weak<Shared> {
    let list = WEAK_SELF.lock().unwrap();
    list.iter()
        .find(|(p, _)| *p == s as *const Shared as usize)
        .map(|(_, w)| w.clone())
        .unwrap_or_default()
}

fn device_vendor(d: &libusb_device) -> u16 {
    u16::from_le_bytes([d.blob.device_descriptor[8], d.blob.device_descriptor[9]])
}

fn device_product(d: &libusb_device) -> u16 {
    u16::from_le_bytes([d.blob.device_descriptor[10], d.blob.device_descriptor[11]])
}

/* ------------------------------------------------------------------------- contexts ------ */

struct DefaultCtx {
    ptr: usize,
    refs: usize,
}

static DEFAULT: Mutex<Option<DefaultCtx>> = Mutex::new(None);

fn new_context() -> Result<*mut libusb_context, c_int> {
    let path = std::env::var("VPHONE_VUSB_SOCKET").map_err(|_| {
        eprintln!("libusb(vusb): VPHONE_VUSB_SOCKET is not set; no device can be reached");
        LIBUSB_ERROR_OTHER
    })?;
    let event_fd = eventfd_new();
    let wake_fd = eventfd_new();
    if event_fd < 0 || wake_fd < 0 {
        return Err(LIBUSB_ERROR_NO_MEM);
    }
    let shared = Arc::new(Shared {
        write: Mutex::new(None),
        state: Mutex::new(State::default()),
        event_fd,
        wake_fd,
        stop: AtomicBool::new(false),
        io_thread: Mutex::new(None),
        next_id: AtomicU64::new(1),
        next_op: AtomicU32::new(1),
        next_hotplug: AtomicU32::new(1),
        event_lock: Mutex::new(false),
        event_cond: Condvar::new(),
        debug: std::env::var_os("VUSB_DEBUG").is_some(),
    });
    WEAK_SELF
        .lock()
        .unwrap()
        .push((Arc::as_ptr(&shared) as usize, Arc::downgrade(&shared)));
    let thread_shared = shared.clone();
    let handle = thread::Builder::new()
        .name("libusb-vusb-io".into())
        .spawn(move || io_thread(thread_shared, path))
        .map_err(|_| LIBUSB_ERROR_NO_MEM)?;
    *shared.io_thread.lock().unwrap() = Some(handle);
    Ok(Box::into_raw(Box::new(libusb_context { shared })))
}

fn ctx_shared(ctx: *mut libusb_context) -> Option<Arc<Shared>> {
    if !ctx.is_null() {
        return Some(unsafe { (*ctx).shared.clone() });
    }
    let d = DEFAULT.lock().unwrap();
    d.as_ref()
        .map(|c| unsafe { (*(c.ptr as *mut libusb_context)).shared.clone() })
}

fn default_ptr() -> *mut libusb_context {
    DEFAULT
        .lock()
        .unwrap()
        .as_ref()
        .map_or(ptr::null_mut(), |c| c.ptr as *mut libusb_context)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_init(ctx: *mut *mut libusb_context) -> c_int {
    if ctx.is_null() {
        let mut d = DEFAULT.lock().unwrap();
        if let Some(c) = d.as_mut() {
            c.refs += 1;
            return LIBUSB_SUCCESS;
        }
        return match new_context() {
            Ok(p) => {
                *d = Some(DefaultCtx {
                    ptr: p as usize,
                    refs: 1,
                });
                LIBUSB_SUCCESS
            }
            Err(e) => e,
        };
    }
    match new_context() {
        Ok(p) => {
            *ctx = p;
            LIBUSB_SUCCESS
        }
        Err(e) => e,
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_init_context(
    ctx: *mut *mut libusb_context,
    _options: *const c_void,
    _num_options: c_int,
) -> c_int {
    libusb_init(ctx)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_exit(ctx: *mut libusb_context) {
    let ptr_to_free = if ctx.is_null() {
        let mut d = DEFAULT.lock().unwrap();
        match d.as_mut() {
            Some(c) if c.refs > 1 => {
                c.refs -= 1;
                return;
            }
            Some(_) => d.take().map(|c| c.ptr as *mut libusb_context),
            None => None,
        }
    } else {
        Some(ctx)
    };
    let Some(p) = ptr_to_free else {
        return;
    };
    let boxed = Box::from_raw(p);
    let s = boxed.shared.clone();
    s.stop.store(true, Ordering::SeqCst);
    signal(s.wake_fd);
    if let Some(stream) = s.write.lock().unwrap().as_ref() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    if let Some(h) = s.io_thread.lock().unwrap().take() {
        let _ = h.join();
    }
    *s.write.lock().unwrap() = None;
    WEAK_SELF
        .lock()
        .unwrap()
        .retain(|(k, _)| *k != Arc::as_ptr(&s) as usize);
    libc::close(s.event_fd);
    libc::close(s.wake_fd);
}

#[no_mangle]
pub unsafe extern "C" fn libusb_set_debug(_ctx: *mut libusb_context, _level: c_int) {}

/// Variadic in C; on the SysV and AAPCS64 ABIs the one extra argument passes as here.
#[no_mangle]
pub unsafe extern "C" fn libusb_set_option(
    _ctx: *mut libusb_context,
    option: c_int,
    _value: usize,
) -> c_int {
    const LOG_LEVEL: c_int = 0;
    const USE_USBDK: c_int = 1;
    const NO_DEVICE_DISCOVERY: c_int = 2;
    const LOG_CB: c_int = 3;
    match option {
        LOG_LEVEL | NO_DEVICE_DISCOVERY | LOG_CB => LIBUSB_SUCCESS,
        USE_USBDK => LIBUSB_ERROR_NOT_SUPPORTED,
        _ => LIBUSB_ERROR_INVALID_PARAM,
    }
}

static VERSION: libusb_version = libusb_version {
    major: 1,
    minor: 0,
    micro: 27,
    nano: 0,
    rc: b"-vphone\0".as_ptr() as *const c_char,
    describe: b"vphone virtual USB\0".as_ptr() as *const c_char,
};

#[no_mangle]
pub extern "C" fn libusb_get_version() -> *const libusb_version {
    &VERSION
}

#[no_mangle]
pub extern "C" fn libusb_has_capability(capability: u32) -> c_int {
    const CAP_HAS_CAPABILITY: u32 = 0;
    const CAP_HAS_HOTPLUG: u32 = 1;
    (capability == CAP_HAS_CAPABILITY || capability == CAP_HAS_HOTPLUG) as c_int
}

#[no_mangle]
pub extern "C" fn libusb_error_name(code: c_int) -> *const c_char {
    let s: &'static [u8] = match code {
        LIBUSB_SUCCESS => b"LIBUSB_SUCCESS\0",
        LIBUSB_ERROR_IO => b"LIBUSB_ERROR_IO\0",
        LIBUSB_ERROR_INVALID_PARAM => b"LIBUSB_ERROR_INVALID_PARAM\0",
        LIBUSB_ERROR_ACCESS => b"LIBUSB_ERROR_ACCESS\0",
        LIBUSB_ERROR_NO_DEVICE => b"LIBUSB_ERROR_NO_DEVICE\0",
        LIBUSB_ERROR_NOT_FOUND => b"LIBUSB_ERROR_NOT_FOUND\0",
        LIBUSB_ERROR_BUSY => b"LIBUSB_ERROR_BUSY\0",
        LIBUSB_ERROR_TIMEOUT => b"LIBUSB_ERROR_TIMEOUT\0",
        LIBUSB_ERROR_OVERFLOW => b"LIBUSB_ERROR_OVERFLOW\0",
        LIBUSB_ERROR_PIPE => b"LIBUSB_ERROR_PIPE\0",
        LIBUSB_ERROR_INTERRUPTED => b"LIBUSB_ERROR_INTERRUPTED\0",
        LIBUSB_ERROR_NO_MEM => b"LIBUSB_ERROR_NO_MEM\0",
        LIBUSB_ERROR_NOT_SUPPORTED => b"LIBUSB_ERROR_NOT_SUPPORTED\0",
        _ => b"LIBUSB_ERROR_OTHER\0",
    };
    s.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn libusb_strerror(code: c_int) -> *const c_char {
    libusb_error_name(code)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_setlocale(_locale: *const c_char) -> c_int {
    LIBUSB_SUCCESS
}

/* -------------------------------------------------------------------------- devices ------ */

fn current_device(s: &Shared) -> Option<Arc<libusb_device>> {
    lock(s).device.clone()
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_device_list(
    ctx: *mut libusb_context,
    list: *mut *mut *mut libusb_device,
) -> libc::ssize_t {
    let Some(shared) = ctx_shared(ctx) else {
        return LIBUSB_ERROR_INVALID_PARAM as libc::ssize_t;
    };
    let device = current_device(&shared);
    let n = usize::from(device.is_some());
    let array =
        libc::calloc(n + 1, std::mem::size_of::<*mut libusb_device>()) as *mut *mut libusb_device;
    if array.is_null() {
        return LIBUSB_ERROR_NO_MEM as libc::ssize_t;
    }
    if let Some(d) = device {
        *array = Arc::into_raw(d) as *mut libusb_device;
    }
    *list = array;
    n as libc::ssize_t
}

#[no_mangle]
pub unsafe extern "C" fn libusb_free_device_list(
    list: *mut *mut libusb_device,
    unref_devices: c_int,
) {
    if list.is_null() {
        return;
    }
    if unref_devices != 0 {
        let mut p = list;
        while !(*p).is_null() {
            libusb_unref_device(*p);
            p = p.add(1);
        }
    }
    libc::free(list as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn libusb_ref_device(dev: *mut libusb_device) -> *mut libusb_device {
    Arc::increment_strong_count(dev as *const libusb_device);
    dev
}

#[no_mangle]
pub unsafe extern "C" fn libusb_unref_device(dev: *mut libusb_device) {
    if !dev.is_null() {
        Arc::decrement_strong_count(dev as *const libusb_device);
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_device_descriptor(
    dev: *mut libusb_device,
    desc: *mut libusb_device_descriptor,
) -> c_int {
    let d = (*dev).blob.device_descriptor;
    *desc = libusb_device_descriptor {
        bLength: d[0],
        bDescriptorType: d[1],
        bcdUSB: u16::from_le_bytes([d[2], d[3]]),
        bDeviceClass: d[4],
        bDeviceSubClass: d[5],
        bDeviceProtocol: d[6],
        bMaxPacketSize0: d[7],
        idVendor: u16::from_le_bytes([d[8], d[9]]),
        idProduct: u16::from_le_bytes([d[10], d[11]]),
        bcdDevice: u16::from_le_bytes([d[12], d[13]]),
        iManufacturer: d[14],
        iProduct: d[15],
        iSerialNumber: d[16],
        bNumConfigurations: d[17],
    };
    LIBUSB_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_bus_number(_dev: *mut libusb_device) -> u8 {
    1
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_port_number(_dev: *mut libusb_device) -> u8 {
    1
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_port_numbers(
    _dev: *mut libusb_device,
    port_numbers: *mut u8,
    port_numbers_len: c_int,
) -> c_int {
    if port_numbers_len < 1 {
        return LIBUSB_ERROR_OVERFLOW;
    }
    *port_numbers = 1;
    1
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_device_address(dev: *mut libusb_device) -> u8 {
    (*dev).blob.address
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_device_speed(_dev: *mut libusb_device) -> c_int {
    3 // LIBUSB_SPEED_HIGH
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_parent(_dev: *mut libusb_device) -> *mut libusb_device {
    ptr::null_mut()
}

fn config_by_value(dev: &libusb_device, value: u8) -> Option<&Vec<u8>> {
    dev.blob
        .configs
        .iter()
        .find(|c| c.len() > 5 && c[5] == value)
}

/// The active configuration's raw descriptor, else (unconfigured) the first one.
fn active_or_first(dev: &libusb_device) -> Option<&Vec<u8>> {
    let active = dev.active.load(Ordering::SeqCst);
    if active != 0 {
        if let Some(c) = config_by_value(dev, active) {
            return Some(c);
        }
    }
    dev.blob.configs.first()
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_max_packet_size(
    dev: *mut libusb_device,
    endpoint: c_uchar,
) -> c_int {
    let Some(config) = active_or_first(&*dev) else {
        return LIBUSB_ERROR_NOT_FOUND;
    };
    descriptors::max_packet_size(config, endpoint).unwrap_or(LIBUSB_ERROR_NOT_FOUND)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_max_iso_packet_size(
    dev: *mut libusb_device,
    endpoint: c_uchar,
) -> c_int {
    libusb_get_max_packet_size(dev, endpoint)
}

fn deliver_config(raw: Option<&Vec<u8>>, out: *mut *mut RawConfig) -> c_int {
    match raw.and_then(|bytes| parse_config(bytes)) {
        Some(p) => {
            unsafe { *out = p };
            LIBUSB_SUCCESS
        }
        None => LIBUSB_ERROR_NOT_FOUND,
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_active_config_descriptor(
    dev: *mut libusb_device,
    config: *mut *mut RawConfig,
) -> c_int {
    let active = (*dev).active.load(Ordering::SeqCst);
    if active == 0 {
        return LIBUSB_ERROR_NOT_FOUND;
    }
    deliver_config(config_by_value(&*dev, active), config)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_config_descriptor(
    dev: *mut libusb_device,
    config_index: u8,
    config: *mut *mut RawConfig,
) -> c_int {
    deliver_config((&(*dev).blob.configs).get(config_index as usize), config)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_config_descriptor_by_value(
    dev: *mut libusb_device,
    value: u8,
    config: *mut *mut RawConfig,
) -> c_int {
    deliver_config(config_by_value(&*dev, value), config)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_free_config_descriptor(config: *mut RawConfig) {
    free_config(config)
}

/* -------------------------------------------------------------------------- handles ------ */

#[no_mangle]
pub unsafe extern "C" fn libusb_open(
    dev: *mut libusb_device,
    handle: *mut *mut libusb_device_handle,
) -> c_int {
    let device = (*dev).shared.upgrade();
    let Some(shared) = device else {
        return LIBUSB_ERROR_NO_DEVICE;
    };
    if (*dev).gone.load(Ordering::SeqCst) {
        return LIBUSB_ERROR_NO_DEVICE;
    }
    Arc::increment_strong_count(dev as *const libusb_device);
    let device = Arc::from_raw(dev as *const libusb_device);
    *handle = Box::into_raw(Box::new(libusb_device_handle {
        device,
        shared,
        claimed: Mutex::new(0),
    }));
    LIBUSB_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn libusb_open_device_with_vid_pid(
    ctx: *mut libusb_context,
    vendor: u16,
    product: u16,
) -> *mut libusb_device_handle {
    let Some(shared) = ctx_shared(ctx) else {
        return ptr::null_mut();
    };
    let Some(device) = current_device(&shared) else {
        return ptr::null_mut();
    };
    if device_vendor(&device) != vendor || device_product(&device) != product {
        return ptr::null_mut();
    }
    let raw = Arc::as_ptr(&device) as *mut libusb_device;
    let mut handle = ptr::null_mut();
    if libusb_open(raw, &mut handle) != LIBUSB_SUCCESS {
        return ptr::null_mut();
    }
    handle
}

#[no_mangle]
pub unsafe extern "C" fn libusb_close(handle: *mut libusb_device_handle) {
    if handle.is_null() {
        return;
    }
    let h = Box::from_raw(handle);
    let claimed = *h.claimed.lock().unwrap();
    let gen = h.device.blob.gen;
    for iface in 0..32u8 {
        if claimed & (1 << iface) != 0 {
            let _ = do_op(&h.shared, gen, OpCode::Release, iface);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_device(
    handle: *mut libusb_device_handle,
) -> *mut libusb_device {
    Arc::as_ptr(&(*handle).device) as *mut libusb_device
}

#[no_mangle]
pub unsafe extern "C" fn libusb_wrap_sys_device(
    _ctx: *mut libusb_context,
    _sys_dev: isize,
    _handle: *mut *mut libusb_device_handle,
) -> c_int {
    LIBUSB_ERROR_NOT_SUPPORTED
}

fn do_op(s: &Shared, gen: u32, op: OpCode, arg: u8) -> c_int {
    let opid = s.next_op.fetch_add(1, Ordering::SeqCst);
    let slot = Arc::new(OpSlot {
        result: Mutex::new(None),
        cond: Condvar::new(),
    });
    lock(s).ops.insert(opid, slot.clone());
    if send_frame(
        s,
        &C2S::Op {
            opid,
            gen,
            op,
            arg,
        },
    )
    .is_err()
    {
        lock(s).ops.remove(&opid);
        return LIBUSB_ERROR_NO_DEVICE;
    }
    let mut r = slot.result.lock().unwrap();
    while r.is_none() {
        r = slot.cond.wait(r).unwrap();
    }
    r.unwrap()
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_configuration(
    handle: *mut libusb_device_handle,
    config: *mut c_int,
) -> c_int {
    *config = c_int::from((&(*handle).device).active.load(Ordering::SeqCst));
    LIBUSB_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn libusb_set_configuration(
    handle: *mut libusb_device_handle,
    configuration: c_int,
) -> c_int {
    let h = &*handle;
    let value = if configuration < 0 { 0 } else { configuration as u8 };
    let r = do_op(&h.shared, h.device.blob.gen, OpCode::SetConfig, value);
    if r == LIBUSB_SUCCESS {
        h.device.active.store(value, Ordering::SeqCst);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn libusb_claim_interface(
    handle: *mut libusb_device_handle,
    interface: c_int,
) -> c_int {
    let h = &*handle;
    if !(0..32).contains(&interface) {
        return LIBUSB_ERROR_INVALID_PARAM;
    }
    let r = do_op(
        &h.shared,
        h.device.blob.gen,
        OpCode::Claim,
        interface as u8,
    );
    if r == LIBUSB_SUCCESS {
        *h.claimed.lock().unwrap() |= 1 << interface;
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn libusb_release_interface(
    handle: *mut libusb_device_handle,
    interface: c_int,
) -> c_int {
    let h = &*handle;
    if !(0..32).contains(&interface) {
        return LIBUSB_ERROR_INVALID_PARAM;
    }
    let r = do_op(
        &h.shared,
        h.device.blob.gen,
        OpCode::Release,
        interface as u8,
    );
    if r == LIBUSB_SUCCESS {
        *h.claimed.lock().unwrap() &= !(1 << interface);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn libusb_set_interface_alt_setting(
    handle: *mut libusb_device_handle,
    interface: c_int,
    alternate: c_int,
) -> c_int {
    let r = libusb_control_transfer(
        handle,
        0x01,
        0x0b,
        alternate as u16,
        interface as u16,
        ptr::null_mut(),
        0,
        5000,
    );
    r.min(0)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_clear_halt(
    handle: *mut libusb_device_handle,
    endpoint: c_uchar,
) -> c_int {
    let r = libusb_control_transfer(
        handle,
        0x02,
        0x01,
        0,
        u16::from(endpoint),
        ptr::null_mut(),
        0,
        5000,
    );
    r.min(0)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_reset_device(handle: *mut libusb_device_handle) -> c_int {
    let h = &*handle;
    do_op(&h.shared, h.device.blob.gen, OpCode::Reset, 0)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_kernel_driver_active(
    _handle: *mut libusb_device_handle,
    _interface: c_int,
) -> c_int {
    0 // no kernel driver on a broker-attached device
}

#[no_mangle]
pub unsafe extern "C" fn libusb_detach_kernel_driver(
    _handle: *mut libusb_device_handle,
    _interface: c_int,
) -> c_int {
    LIBUSB_ERROR_NOT_FOUND
}

#[no_mangle]
pub unsafe extern "C" fn libusb_attach_kernel_driver(
    _handle: *mut libusb_device_handle,
    _interface: c_int,
) -> c_int {
    LIBUSB_ERROR_NOT_FOUND
}

#[no_mangle]
pub unsafe extern "C" fn libusb_set_auto_detach_kernel_driver(
    _handle: *mut libusb_device_handle,
    _enable: c_int,
) -> c_int {
    LIBUSB_SUCCESS
}

/* ------------------------------------------------------------------------ transfers ------ */

#[no_mangle]
pub unsafe extern "C" fn libusb_alloc_transfer(iso_packets: c_int) -> *mut libusb_transfer {
    let size = std::mem::size_of::<libusb_transfer>()
        + iso_packets.max(0) as usize * std::mem::size_of::<libusb_iso_packet_descriptor>();
    libc::calloc(1, size) as *mut libusb_transfer
}

#[no_mangle]
pub unsafe extern "C" fn libusb_free_transfer(transfer: *mut libusb_transfer) {
    if transfer.is_null() {
        return;
    }
    if (*transfer).flags & LIBUSB_TRANSFER_FREE_BUFFER != 0 && !(*transfer).buffer.is_null() {
        libc::free((*transfer).buffer as *mut c_void);
    }
    libc::free(transfer as *mut c_void);
}

#[derive(Clone, Copy)]
struct Request {
    endpoint: u8,
    kind: u8,
    length: u32,
    setup: [u8; 8],
}

fn deadline_from(timeout_ms: c_uint) -> Option<Instant> {
    if timeout_ms == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
    }
}

fn wake_io(s: &Shared) {
    signal(s.wake_fd);
}

#[no_mangle]
pub unsafe extern "C" fn libusb_submit_transfer(transfer: *mut libusb_transfer) -> c_int {
    let tr = &mut *transfer;
    if tr.dev_handle.is_null() {
        return LIBUSB_ERROR_INVALID_PARAM;
    }
    let h = &*tr.dev_handle;
    if h.device.gone.load(Ordering::SeqCst) {
        return LIBUSB_ERROR_NO_DEVICE;
    }
    let (request, payload) = match prepare(tr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    tr.status = 0;
    tr.actual_length = 0;
    let shared = h.shared.clone();
    let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
    let deadline = deadline_from(tr.timeout);
    let gen = h.device.blob.gen;
    {
        let mut st = lock(&shared);
        st.pending.insert(
            id,
            Pending {
                target: Target::Async(SendPtr(transfer)),
                deadline,
                timed_out: false,
            },
        );
        st.by_transfer.insert(transfer as usize, id);
    }
    if deadline.is_some() {
        wake_io(&shared);
    }
    let msg = C2S::Submit {
        id,
        gen,
        endpoint: request.endpoint,
        kind: request.kind,
        length: request.length,
        setup: request.setup,
        payload,
    };
    if send_frame(&shared, &msg).is_err() {
        let mut st = lock(&shared);
        st.pending.remove(&id);
        st.by_transfer.remove(&(transfer as usize));
        return LIBUSB_ERROR_NO_DEVICE;
    }
    LIBUSB_SUCCESS
}

unsafe fn prepare(tr: &libusb_transfer) -> Result<(Request, Vec<u8>), c_int> {
    match tr.type_ {
        LIBUSB_TRANSFER_TYPE_CONTROL => {
            if tr.length < 8 || tr.buffer.is_null() {
                return Err(LIBUSB_ERROR_INVALID_PARAM);
            }
            let setup: [u8; 8] = std::slice::from_raw_parts(tr.buffer, 8).try_into().unwrap();
            let w_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;
            if tr.length < 8 + w_length as c_int {
                return Err(LIBUSB_ERROR_INVALID_PARAM);
            }
            let payload = if setup[0] & 0x80 == 0 {
                std::slice::from_raw_parts(tr.buffer.add(8), w_length).to_vec()
            } else {
                Vec::new()
            };
            Ok((
                Request {
                    endpoint: 0,
                    kind: transfer_type::CONTROL,
                    length: w_length as u32,
                    setup,
                },
                payload,
            ))
        }
        LIBUSB_TRANSFER_TYPE_BULK | LIBUSB_TRANSFER_TYPE_INTERRUPT => {
            if tr.length < 0 || (tr.length > 0 && tr.buffer.is_null()) {
                return Err(LIBUSB_ERROR_INVALID_PARAM);
            }
            if tr.length as usize > vusb_common::MAX_FRAME - 64 {
                return Err(LIBUSB_ERROR_INVALID_PARAM);
            }
            let payload = if tr.endpoint & 0x80 == 0 && tr.length > 0 {
                std::slice::from_raw_parts(tr.buffer, tr.length as usize).to_vec()
            } else {
                Vec::new()
            };
            let kind = if tr.type_ == LIBUSB_TRANSFER_TYPE_BULK {
                transfer_type::BULK
            } else {
                transfer_type::INTERRUPT
            };
            Ok((
                Request {
                    endpoint: tr.endpoint,
                    kind,
                    length: tr.length as u32,
                    setup: [0; 8],
                },
                payload,
            ))
        }
        LIBUSB_TRANSFER_TYPE_ISOCHRONOUS => Err(LIBUSB_ERROR_NOT_SUPPORTED),
        _ => Err(LIBUSB_ERROR_INVALID_PARAM),
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_cancel_transfer(transfer: *mut libusb_transfer) -> c_int {
    let h = &*(*transfer).dev_handle;
    let id = lock(&h.shared).by_transfer.get(&(transfer as usize)).copied();
    match id {
        Some(id) => {
            let _ = send_frame(&h.shared, &C2S::Cancel { id });
            LIBUSB_SUCCESS
        }
        None => LIBUSB_ERROR_NOT_FOUND,
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_transfer_get_stream_id(_transfer: *mut libusb_transfer) -> u32 {
    0
}

/// A synchronous transfer through the I/O thread. No event pumping is needed, so it can be called
/// from any thread.
unsafe fn do_sync(
    handle: *mut libusb_device_handle,
    request: Request,
    payload: Vec<u8>,
    buffer: *mut u8,
    capacity: usize,
    timeout: c_uint,
) -> (c_int, usize) {
    let h = &*handle;
    if h.device.gone.load(Ordering::SeqCst) {
        return (status::NO_DEVICE, 0);
    }
    let shared = h.shared.clone();
    let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
    let deadline = deadline_from(timeout);
    let gen = h.device.blob.gen;
    let slot = Arc::new(SyncSlot {
        buffer: SendPtr(buffer),
        capacity,
        result: Mutex::new(None),
        cond: Condvar::new(),
    });
    {
        let mut st = lock(&shared);
        st.pending.insert(
            id,
            Pending {
                target: Target::Sync(slot.clone()),
                deadline,
                timed_out: false,
            },
        );
    }
    if deadline.is_some() {
        wake_io(&shared);
    }
    let msg = C2S::Submit {
        id,
        gen,
        endpoint: request.endpoint,
        kind: request.kind,
        length: request.length,
        setup: request.setup,
        payload,
    };
    if send_frame(&shared, &msg).is_err() {
        lock(&shared).pending.remove(&id);
        return (status::NO_DEVICE, 0);
    }
    let mut r = slot.result.lock().unwrap();
    while r.is_none() {
        r = slot.cond.wait(r).unwrap();
    }
    r.unwrap()
}

fn map_status(code: c_int) -> c_int {
    match code {
        status::COMPLETED => LIBUSB_SUCCESS,
        status::TIMED_OUT => LIBUSB_ERROR_TIMEOUT,
        status::STALL => LIBUSB_ERROR_PIPE,
        status::NO_DEVICE => LIBUSB_ERROR_NO_DEVICE,
        status::OVERFLOW => LIBUSB_ERROR_OVERFLOW,
        status::CANCELLED => LIBUSB_ERROR_INTERRUPTED,
        _ => LIBUSB_ERROR_IO,
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_control_transfer(
    handle: *mut libusb_device_handle,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    data: *mut c_uchar,
    length: u16,
    timeout: c_uint,
) -> c_int {
    let v = value.to_le_bytes();
    let i = index.to_le_bytes();
    let l = length.to_le_bytes();
    let setup = [
        request_type,
        request,
        v[0],
        v[1],
        i[0],
        i[1],
        l[0],
        l[1],
    ];
    let direction_in = request_type & 0x80 != 0;
    let payload = if !direction_in && length > 0 {
        std::slice::from_raw_parts(data, length as usize).to_vec()
    } else {
        Vec::new()
    };
    let req = Request {
        endpoint: 0,
        kind: transfer_type::CONTROL,
        length: length as u32,
        setup,
    };
    let (st, actual) = do_sync(handle, req, payload, data, length as usize, timeout);
    if st == status::COMPLETED {
        actual as c_int
    } else {
        map_status(st)
    }
}

unsafe fn sync_data_transfer(
    handle: *mut libusb_device_handle,
    endpoint: c_uchar,
    data: *mut c_uchar,
    length: c_int,
    transferred: *mut c_int,
    timeout: c_uint,
    kind: u8,
) -> c_int {
    if length < 0 {
        return LIBUSB_ERROR_INVALID_PARAM;
    }
    let direction_in = endpoint & 0x80 != 0;
    let payload = if !direction_in && length > 0 {
        std::slice::from_raw_parts(data, length as usize).to_vec()
    } else {
        Vec::new()
    };
    let req = Request {
        endpoint,
        kind,
        length: length as u32,
        setup: [0; 8],
    };
    let (st, actual) = do_sync(handle, req, payload, data, length as usize, timeout);
    if !transferred.is_null() {
        *transferred = actual as c_int;
    }
    map_status(st)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_bulk_transfer(
    handle: *mut libusb_device_handle,
    endpoint: c_uchar,
    data: *mut c_uchar,
    length: c_int,
    transferred: *mut c_int,
    timeout: c_uint,
) -> c_int {
    sync_data_transfer(
        handle,
        endpoint,
        data,
        length,
        transferred,
        timeout,
        transfer_type::BULK,
    )
}

#[no_mangle]
pub unsafe extern "C" fn libusb_interrupt_transfer(
    handle: *mut libusb_device_handle,
    endpoint: c_uchar,
    data: *mut c_uchar,
    length: c_int,
    transferred: *mut c_int,
    timeout: c_uint,
) -> c_int {
    sync_data_transfer(
        handle,
        endpoint,
        data,
        length,
        transferred,
        timeout,
        transfer_type::INTERRUPT,
    )
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_string_descriptor_ascii(
    handle: *mut libusb_device_handle,
    desc_index: u8,
    data: *mut c_uchar,
    length: c_int,
) -> c_int {
    if desc_index == 0 {
        return LIBUSB_ERROR_INVALID_PARAM;
    }
    let mut tbuf = [0u8; 255];
    let r = libusb_control_transfer(
        handle,
        0x80,
        0x06,
        0x0300,
        0,
        tbuf.as_mut_ptr(),
        tbuf.len() as u16,
        1000,
    );
    if r < 0 {
        return r;
    }
    if r < 4 {
        return LIBUSB_ERROR_IO;
    }
    let langid = u16::from_le_bytes([tbuf[2], tbuf[3]]);
    let r = libusb_control_transfer(
        handle,
        0x80,
        0x06,
        0x0300 | u16::from(desc_index),
        langid,
        tbuf.as_mut_ptr(),
        tbuf.len() as u16,
        1000,
    );
    if r < 0 {
        return r;
    }
    if r < 2 || tbuf[1] != 3 || i32::from(tbuf[0]) > r {
        return LIBUSB_ERROR_IO;
    }
    let mut di = 0;
    let mut si = 2;
    while si + 1 < usize::from(tbuf[0]) && di + 1 < length {
        *data.offset(di as isize) = if tbuf[si] & 0x80 != 0 || tbuf[si + 1] != 0 {
            b'?'
        } else {
            tbuf[si]
        };
        di += 1;
        si += 2;
    }
    *data.offset(di as isize) = 0;
    di
}

/* --------------------------------------------------------------------------- events ------ */

fn dispatch(s: &Arc<Shared>, ctx: *mut libusb_context, completed: *const c_int) {
    drain_fd(s.event_fd);
    loop {
        let event = lock(s).events.pop_front();
        match event {
            None => break,
            Some(Event::Transfer {
                transfer,
                status,
                actual,
            }) => unsafe {
                let t = transfer.0;
                let flags = (*t).flags;
                (*t).status = status;
                (*t).actual_length = actual;
                if let Some(cb) = (*t).callback {
                    cb(t);
                }
                if flags & LIBUSB_TRANSFER_FREE_TRANSFER != 0 {
                    libusb_free_transfer(t);
                }
            },
            Some(Event::Hotplug {
                device,
                arrived,
                seq,
            }) => unsafe {
                let event_mask = if arrived {
                    LIBUSB_HOTPLUG_EVENT_DEVICE_ARRIVED
                } else {
                    LIBUSB_HOTPLUG_EVENT_DEVICE_LEFT
                };
                let cbs: Vec<(HotplugCb, usize, c_int)> = {
                    let st = lock(s);
                    st.hotplugs
                        .iter()
                        .filter(|h| h.events & event_mask != 0 && seq >= h.min_seq && hotplug_matches(h, &device))
                        .map(|h| (h.cb, h.user_data, h.handle))
                        .collect()
                };
                let raw = Arc::as_ptr(&device) as *mut libusb_device;
                for (cb, user, handle) in cbs {
                    if cb(ctx, raw, event_mask, user as *mut c_void) != 0 {
                        lock(s).hotplugs.retain(|h| h.handle != handle);
                    }
                }
            },
        }
        if !completed.is_null() && unsafe { *completed } != 0 {
            break;
        }
    }
    if !lock(s).events.is_empty() {
        signal(s.event_fd);
    }
}

fn hotplug_matches(h: &Hotplug, d: &libusb_device) -> bool {
    let blob = &d.blob.device_descriptor;
    (h.vendor == LIBUSB_HOTPLUG_MATCH_ANY || h.vendor == c_int::from(device_vendor(d)))
        && (h.product == LIBUSB_HOTPLUG_MATCH_ANY || h.product == c_int::from(device_product(d)))
        && (h.class == LIBUSB_HOTPLUG_MATCH_ANY || h.class == c_int::from(blob[4]))
}

fn wait_events(s: &Shared, timeout_ms: c_int) {
    let mut fd = libc::pollfd {
        fd: s.event_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut fd, 1, timeout_ms) };
}

fn timeval_ms(tv: *const libc::timeval) -> c_int {
    if tv.is_null() {
        return 60_000;
    }
    let tv = unsafe { *tv };
    (tv.tv_sec.max(0) as i64 * 1000 + (tv.tv_usec.max(0) as i64 + 999) / 1000)
        .min(i64::from(i32::MAX)) as c_int
}

fn handle_events(
    ctx: *mut libusb_context,
    tv: *const libc::timeval,
    completed: *const c_int,
) -> c_int {
    let Some(shared) = ctx_shared(ctx) else {
        return LIBUSB_ERROR_INVALID_PARAM;
    };
    if !completed.is_null() && unsafe { *completed } != 0 {
        return LIBUSB_SUCCESS;
    }
    wait_events(&shared, timeval_ms(tv));
    let hctx = if ctx.is_null() { default_ptr() } else { ctx };
    dispatch(&shared, hctx, completed);
    LIBUSB_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn libusb_handle_events_timeout_completed(
    ctx: *mut libusb_context,
    tv: *mut libc::timeval,
    completed: *mut c_int,
) -> c_int {
    handle_events(ctx, tv, completed)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_handle_events_timeout(
    ctx: *mut libusb_context,
    tv: *mut libc::timeval,
) -> c_int {
    handle_events(ctx, tv, ptr::null())
}

#[no_mangle]
pub unsafe extern "C" fn libusb_handle_events(ctx: *mut libusb_context) -> c_int {
    handle_events(ctx, ptr::null(), ptr::null())
}

#[no_mangle]
pub unsafe extern "C" fn libusb_handle_events_completed(
    ctx: *mut libusb_context,
    completed: *mut c_int,
) -> c_int {
    handle_events(ctx, ptr::null(), completed)
}

#[no_mangle]
pub unsafe extern "C" fn libusb_handle_events_locked(
    ctx: *mut libusb_context,
    tv: *mut libc::timeval,
) -> c_int {
    handle_events(ctx, tv, ptr::null())
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_next_timeout(
    _ctx: *mut libusb_context,
    _tv: *mut libc::timeval,
) -> c_int {
    0 // the I/O thread applies timeouts and wakes the event fd
}

#[no_mangle]
pub unsafe extern "C" fn libusb_pollfds_handle_timeouts(_ctx: *mut libusb_context) -> c_int {
    1
}

#[no_mangle]
pub unsafe extern "C" fn libusb_get_pollfds(
    ctx: *mut libusb_context,
) -> *mut *const libusb_pollfd {
    let Some(shared) = ctx_shared(ctx) else {
        return ptr::null_mut();
    };
    // One allocation, as libusb's: callers (usbmuxd) release it with free().
    let ptrs = 2 * std::mem::size_of::<*const libusb_pollfd>();
    let block = libc::calloc(1, ptrs + std::mem::size_of::<libusb_pollfd>()) as *mut u8;
    if block.is_null() {
        return ptr::null_mut();
    }
    let fds = block.add(ptrs) as *mut libusb_pollfd;
    (*fds).fd = shared.event_fd;
    (*fds).events = libc::POLLIN;
    let list = block as *mut *const libusb_pollfd;
    *list = fds;
    list
}

#[no_mangle]
pub unsafe extern "C" fn libusb_free_pollfds(pollfds: *mut *const libusb_pollfd) {
    libc::free(pollfds as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn libusb_set_pollfd_notifiers(
    _ctx: *mut libusb_context,
    _added: *mut c_void,
    _removed: *mut c_void,
    _user_data: *mut c_void,
) {
    // One event fd for the context's lifetime: there is nothing to notify about.
}

/* The event-handling lock: real locks, for callers that manage the handler role themselves. */

#[no_mangle]
pub unsafe extern "C" fn libusb_try_lock_events(ctx: *mut libusb_context) -> c_int {
    let Some(s) = ctx_shared(ctx) else {
        return 1;
    };
    let mut held = s.event_lock.lock().unwrap();
    if *held {
        return 1;
    }
    *held = true;
    0
}

#[no_mangle]
pub unsafe extern "C" fn libusb_lock_events(ctx: *mut libusb_context) {
    let Some(s) = ctx_shared(ctx) else {
        return;
    };
    let mut held = s.event_lock.lock().unwrap();
    while *held {
        held = s.event_cond.wait(held).unwrap();
    }
    *held = true;
}

#[no_mangle]
pub unsafe extern "C" fn libusb_unlock_events(ctx: *mut libusb_context) {
    let Some(s) = ctx_shared(ctx) else {
        return;
    };
    *s.event_lock.lock().unwrap() = false;
    s.event_cond.notify_all();
}

#[no_mangle]
pub unsafe extern "C" fn libusb_event_handling_ok(_ctx: *mut libusb_context) -> c_int {
    1
}

#[no_mangle]
pub unsafe extern "C" fn libusb_event_handler_active(ctx: *mut libusb_context) -> c_int {
    ctx_shared(ctx).is_some_and(|s| *s.event_lock.lock().unwrap()) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn libusb_interrupt_event_handler(ctx: *mut libusb_context) {
    if let Some(s) = ctx_shared(ctx) {
        signal(s.event_fd);
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_lock_event_waiters(_ctx: *mut libusb_context) {}

#[no_mangle]
pub unsafe extern "C" fn libusb_unlock_event_waiters(_ctx: *mut libusb_context) {}

#[no_mangle]
pub unsafe extern "C" fn libusb_wait_for_event(
    ctx: *mut libusb_context,
    tv: *mut libc::timeval,
) -> c_int {
    let Some(s) = ctx_shared(ctx) else {
        return LIBUSB_ERROR_INVALID_PARAM;
    };
    wait_events(&s, timeval_ms(tv));
    let ready = !lock(&s).events.is_empty();
    if ready {
        0
    } else {
        1
    }
}

/* -------------------------------------------------------------------------- hotplug ------ */

#[no_mangle]
pub unsafe extern "C" fn libusb_hotplug_register_callback(
    ctx: *mut libusb_context,
    events: c_int,
    flags: c_int,
    vendor_id: c_int,
    product_id: c_int,
    dev_class: c_int,
    cb: HotplugCb,
    user_data: *mut c_void,
    callback_handle: *mut c_int,
) -> c_int {
    let Some(s) = ctx_shared(ctx) else {
        return LIBUSB_ERROR_INVALID_PARAM;
    };
    let handle = s.next_hotplug.fetch_add(1, Ordering::SeqCst) as c_int;
    let enumerate;
    {
        let mut st = lock(&s);
        // An arrival already queued before this registration is delivered by the enumeration
        // below; the callback sees only events enqueued after it.
        let min_seq = st.seq + 1;
        enumerate = if flags & LIBUSB_HOTPLUG_ENUMERATE != 0 {
            st.device.clone()
        } else {
            None
        };
        st.hotplugs.push(Hotplug {
            handle,
            cb,
            events,
            vendor: vendor_id,
            product: product_id,
            class: dev_class,
            user_data: user_data as usize,
            min_seq,
        });
    }
    if let Some(d) = enumerate {
        if events & LIBUSB_HOTPLUG_EVENT_DEVICE_ARRIVED != 0
            && hotplug_matches(
                &Hotplug {
                    handle,
                    cb,
                    events,
                    vendor: vendor_id,
                    product: product_id,
                    class: dev_class,
                    user_data: 0,
                    min_seq: 0,
                },
                &d,
            )
        {
            let hctx = if ctx.is_null() { default_ptr() } else { ctx };
            let raw = Arc::as_ptr(&d) as *mut libusb_device;
            if cb(
                hctx,
                raw,
                LIBUSB_HOTPLUG_EVENT_DEVICE_ARRIVED,
                user_data,
            ) != 0
            {
                lock(&s).hotplugs.retain(|h| h.handle != handle);
            }
        }
    }
    if !callback_handle.is_null() {
        *callback_handle = handle;
    }
    LIBUSB_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn libusb_hotplug_deregister_callback(
    ctx: *mut libusb_context,
    callback_handle: c_int,
) {
    if let Some(s) = ctx_shared(ctx) {
        lock(&s).hotplugs.retain(|h| h.handle != callback_handle);
    }
}

#[no_mangle]
pub unsafe extern "C" fn libusb_hotplug_get_user_data(
    ctx: *mut libusb_context,
    callback_handle: c_int,
) -> *mut c_void {
    ctx_shared(ctx)
        .and_then(|s| {
            lock(&s)
                .hotplugs
                .iter()
                .find(|h| h.handle == callback_handle)
                .map(|h| h.user_data as *mut c_void)
        })
        .unwrap_or(ptr::null_mut())
}
