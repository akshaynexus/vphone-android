//! The framed chardev to QEMU's vusb device: a port of `class VUSB` in tools/vusb-usbip.py.
//!
//! Host to device: `u32 len (LE), u8 ep, u8 type, payload`. Device to host: `u32 len (LE), u8 code,
//! u8 ep, data`. See qemu/hw/vmapple/vusb.c. Requests and their responses are matched per endpoint,
//! in order.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

pub const MSG_DATA: u8 = 1;
pub const MSG_RESET: u8 = 2;
pub const MSG_ABORT: u8 = 3;

const CODE_DATA: u8 = 1;
const CODE_STALL: u8 = 2;
/// The PCI restore controller reports a USB NAK while an IN endpoint has no data yet.
const CODE_NAK: u8 = 3;
const CODE_RESET: u8 = 4;
const CODE_ABORT: u8 = 5;
/// The guest re-initialised the controller (a new boot stage): the device it presented is gone.
const CODE_DRIVER_RESET: u8 = 6;

pub const VUSB_MSG_MAX: usize = 0x10000;
pub const MSG_PAYLOAD_MAX: usize = VUSB_MSG_MAX - 6;
pub const CTRL_TIMEOUT: Duration = Duration::from_secs(5);
const ABORT_TIMEOUT: Duration = Duration::from_secs(5);
const NAK_DELAY: Duration = Duration::from_millis(1);
/// How long the guest may take to acknowledge one packet: far past any slow phone.
const OUT_ACK_TIMEOUT: Duration = Duration::from_secs(300);
/// The controller queues at most 64 host messages (`VUSB_HOST_FIFO`).
pub const OUT_WINDOW: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The chardev dropped or the guest's driver reset: the device is gone.
    NoDevice,
    Stall,
    Timeout,
    Cancelled,
    Protocol(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::NoDevice => write!(f, "device unplugged (guest driver reset)"),
            LinkError::Stall => write!(f, "guest returned STALL"),
            LinkError::Timeout => write!(f, "timed out"),
            LinkError::Cancelled => write!(f, "cancelled"),
            LinkError::Protocol(s) => write!(f, "{s}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, LinkError>;
type Response = (u8, Vec<u8>);

#[derive(Default)]
struct State {
    /// Bumped by every unplug: waiters started under an older value fail with NoDevice.
    epoch: u64,
    connected: bool,
    /// An unplug happened since `clear_detached`: `follow_stages` must enumerate the next device.
    detached: bool,
    reset_pending: bool,
    reset_responses: VecDeque<u8>,
    ctrl: VecDeque<Response>,
    aborts: HashMap<u8, VecDeque<Response>>,
    out_acks: HashMap<u8, VecDeque<Response>>,
    in_data: HashMap<u8, VecDeque<Response>>,
    /// Data the guest returned beyond the current URB's length, for the next one. The guest does
    /// not know the host's URB size; dropping the surplus would stall usbmuxd's stream.
    in_remainder: HashMap<u8, VecDeque<u8>>,
}

impl State {
    fn queue(&mut self, q: Queue) -> &mut VecDeque<Response> {
        match q {
            Queue::Ctrl => &mut self.ctrl,
            Queue::In(a) => self.in_data.entry(a).or_default(),
            Queue::Out(a) => self.out_acks.entry(a).or_default(),
        }
    }
}

#[derive(Clone, Copy)]
enum Queue {
    Ctrl,
    In(u8),
    Out(u8),
}

pub struct Link {
    write: Mutex<Option<UnixStream>>,
    state: Mutex<State>,
    cond: Condvar,
    ctrl_lock: Mutex<()>,
    pub ctrl_timeout: Duration,
}

impl Link {
    pub fn new(ctrl_timeout: Duration) -> Self {
        Link {
            write: Mutex::new(None),
            state: Mutex::new(State {
                detached: true,
                ..Default::default()
            }),
            cond: Condvar::new(),
            ctrl_lock: Mutex::new(()),
            ctrl_timeout,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Wake waiters so they re-check their cancellation flag.
    pub fn notify(&self) {
        let _guard = self.lock();
        self.cond.notify_all();
    }

    /// Use `stream` as the chardev and read it until EOF; afterwards the device is gone.
    pub fn serve(&self, stream: UnixStream) -> io::Result<()> {
        let mut reader = stream.try_clone()?;
        *self.write.lock().unwrap() = Some(stream);
        self.lock().connected = true;
        let result = self.read_loop(&mut reader);
        *self.write.lock().unwrap() = None;
        let mut s = self.lock();
        s.connected = false;
        self.unplug(&mut s);
        drop(s);
        self.cond.notify_all();
        result
    }

    fn read_loop(&self, r: &mut UnixStream) -> io::Result<()> {
        let mut hdr = [0u8; 4];
        loop {
            r.read_exact(&mut hdr)?;
            let length = u32::from_le_bytes(hdr) as usize;
            if length == 0 {
                continue; // an empty send (OpenAVB answers a malformed event so)
            }
            if !(2..=VUSB_MSG_MAX).contains(&length) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid vusb response length {length}"),
                ));
            }
            let mut msg = vec![0u8; length];
            r.read_exact(&mut msg)?;
            self.dispatch(msg);
        }
    }

    fn dispatch(&self, msg: Vec<u8>) {
        let (code, ep) = (msg[0], msg[1]);
        let data = msg[2..].to_vec();
        let mut s = self.lock();
        match code {
            CODE_DRIVER_RESET => {
                eprintln!("vusbd: guest re-initialised its USB controller: device unplugged");
                self.unplug(&mut s);
            }
            CODE_RESET => {
                if s.reset_pending {
                    s.reset_responses.push_back(code);
                }
            }
            CODE_NAK if ep == 0 && s.reset_pending => s.reset_responses.push_back(code),
            CODE_ABORT => s.aborts.entry(ep).or_default().push_back((code, data)),
            _ if ep == 0 => s.ctrl.push_back((code, data)),
            _ if ep & 0x80 != 0 => s.in_data.entry(ep).or_default().push_back((code, data)),
            _ => s.out_acks.entry(ep).or_default().push_back((code, data)),
        }
        drop(s);
        self.cond.notify_all();
    }

    /// Fail everything waiting on the old device, and start fresh queues.
    fn unplug(&self, s: &mut State) {
        s.epoch += 1;
        s.detached = true;
        s.ctrl.clear();
        s.aborts.clear();
        s.out_acks.clear();
        s.in_data.clear();
        s.in_remainder.clear();
    }

    pub fn clear_detached(&self) {
        self.lock().detached = false;
    }

    pub fn is_detached(&self) -> bool {
        self.lock().detached
    }

    pub fn wait_detached(&self) {
        let mut s = self.lock();
        while !s.detached {
            s = self.cond.wait(s).unwrap();
        }
    }

    pub fn send(&self, ep: u8, typ: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > MSG_PAYLOAD_MAX {
            return Err(LinkError::Protocol(format!(
                "vusb payload too large: {}",
                payload.len()
            )));
        }
        let mut frame = Vec::with_capacity(6 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.push(ep);
        frame.push(typ);
        frame.extend_from_slice(payload);
        let mut w = self.write.lock().unwrap();
        match w.as_mut() {
            Some(stream) => stream.write_all(&frame).map_err(|_| LinkError::NoDevice),
            None => Err(LinkError::NoDevice),
        }
    }

    fn wait_for<T>(
        &self,
        epoch: u64,
        deadline: Option<Instant>,
        cancel: Option<&AtomicBool>,
        mut pick: impl FnMut(&mut State) -> Option<T>,
    ) -> Result<T> {
        let mut s = self.lock();
        loop {
            if s.epoch != epoch {
                return Err(LinkError::NoDevice);
            }
            if let Some(v) = pick(&mut s) {
                return Ok(v);
            }
            if let Some(c) = cancel {
                if c.load(Ordering::SeqCst) {
                    return Err(LinkError::Cancelled);
                }
            }
            s = match deadline {
                None => self.cond.wait(s).unwrap(),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(LinkError::Timeout);
                    }
                    self.cond.wait_timeout(s, d - now).unwrap().0
                }
            };
        }
    }

    fn next(
        &self,
        queue: Queue,
        epoch: u64,
        deadline: Option<Instant>,
        cancel: Option<&AtomicBool>,
        retry: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<u8>> {
        loop {
            let (code, data) =
                self.wait_for(epoch, deadline, cancel, |s| s.queue(queue).pop_front())?;
            match code {
                CODE_DATA => return Ok(data),
                CODE_STALL => return Err(LinkError::Stall),
                CODE_NAK => {
                    thread::sleep(NAK_DELAY);
                    retry()?;
                }
                other => {
                    return Err(LinkError::Protocol(format!(
                        "unexpected vusb response code {other}"
                    )))
                }
            }
        }
    }

    /// USB bus reset; completes when the current boot stage's driver answers.
    pub fn reset(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let epoch = {
            let mut s = self.lock();
            s.reset_responses.clear();
            s.reset_pending = true;
            s.epoch
        };
        let result = (|| loop {
            self.send(0, MSG_RESET, &[])?;
            match self.wait_for(epoch, Some(deadline), None, |s| s.reset_responses.pop_front())? {
                CODE_RESET => return Ok(()),
                CODE_NAK => thread::sleep(NAK_DELAY),
                other => {
                    return Err(LinkError::Protocol(format!("unexpected reset response {other}")))
                }
            }
        })();
        self.lock().reset_pending = false;
        result
    }

    /// One control transfer: returns the data the guest sent (IN) or nothing (OUT).
    pub fn control(
        &self,
        setup: &[u8; 8],
        out_data: &[u8],
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<u8>> {
        let _ctrl = self.ctrl_lock.lock().unwrap();
        let mut payload = Vec::with_capacity(8 + out_data.len());
        payload.extend_from_slice(setup);
        payload.extend_from_slice(out_data);
        let epoch = self.epoch();
        let retry = || self.send(0, MSG_DATA, &payload);
        retry()?;
        let deadline = Some(Instant::now() + self.ctrl_timeout);
        self.next(Queue::Ctrl, epoch, deadline, cancel, &retry)
    }

    /// One bulk/interrupt IN transfer. Returns at most `max_length` bytes.
    pub fn transfer_in(
        &self,
        address: u8,
        max_length: usize,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<u8>> {
        {
            let mut s = self.lock();
            if let Some(rest) = s.in_remainder.get_mut(&address) {
                if !rest.is_empty() {
                    // Finish the transfer the guest already handed us.
                    let n = max_length.min(rest.len());
                    return Ok(rest.drain(..n).collect());
                }
            }
        }
        let epoch = self.epoch();
        let retry = || self.send(address, MSG_DATA, &[]);
        retry()?;
        let mut data = self.next(Queue::In(address), epoch, None, cancel, &retry)?;
        if data.len() > max_length {
            let rest: VecDeque<u8> = data[max_length..].iter().copied().collect();
            self.lock().in_remainder.insert(address, rest);
            data.truncate(max_length);
        }
        Ok(data)
    }

    /// One bulk/interrupt OUT transfer: the payload goes as one message, with its acknowledgement.
    pub fn transfer_out(
        &self,
        address: u8,
        payload: &[u8],
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        let epoch = self.epoch();
        let retry = || self.send(address, MSG_DATA, payload);
        retry()?;
        self.next(Queue::Out(address), epoch, None, cancel, &retry)
            .map(|_| ())
    }

    /// One bulk OUT transfer to the DFU/recovery controller, in packets of `packet` bytes. Up to
    /// `window` packets are in flight, which the controller queues, so a transfer of hundreds of
    /// thousands of packets does not wait for a host-side round trip per packet. A guest that stops
    /// acknowledging (a panic in iBoot, say) fails the transfer instead of hanging it.
    pub fn transfer_out_stream(
        &self,
        address: u8,
        payload: &[u8],
        packet: usize,
        window: usize,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        let epoch = self.epoch();
        let no_nak = || Err(LinkError::Protocol("NAK from the DFU/recovery controller".into()));
        let ack = || {
            let deadline = Some(Instant::now() + OUT_ACK_TIMEOUT);
            self.next(Queue::Out(address), epoch, deadline, cancel, &no_nak)
        };
        let mut in_flight = 0;
        for chunk in payload.chunks(packet) {
            if in_flight == window {
                ack()?;
                in_flight -= 1;
            }
            self.send(address, MSG_DATA, chunk)?;
            in_flight += 1;
        }
        while in_flight > 0 {
            ack()?;
            in_flight -= 1;
        }
        Ok(())
    }

    /// Endpoint abort after a cancelled transfer. A control abort is answered on ep 0 for either
    /// direction, as the device reports EP0 IN as 0.
    pub fn abort(&self, address: u8) {
        let epoch = self.epoch();
        if self.send(address, MSG_ABORT, &[]).is_err() {
            return;
        }
        let deadline = Some(Instant::now() + ABORT_TIMEOUT);
        let _ = self.wait_for(epoch, deadline, None, |s| {
            let key = if address & 0x7f == 0 { 0 } else { address };
            s.aborts.entry(key).or_default().pop_front()
        });
    }
}
