//! The framed chardev to QEMU's vusb device: a port of `class VUSB` in tools/vusb-usbip.py.
//!
//! Host to device: `u32 len (LE), u8 ep, u8 type, payload`. Device to host: `u32 len (LE), u8 code,
//! u8 ep, data`. See qemu/hw/vmapple/vusb.c. Requests and their responses are matched per endpoint,
//! in order.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Framing overhead of a host-to-device message.
const FRAME_HEADER: usize = 6;
/// Two whole messages, so a read always has room for what a partial one still needs.
const READ_BUFFER: usize = 2 * (VUSB_MSG_MAX + 4);

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
/// A NAK is retried after this long, doubling up to `NAK_DELAY_MAX` until the transfer gets data.
const NAK_DELAY_MIN: Duration = Duration::from_millis(1);
const NAK_DELAY_MAX: Duration = Duration::from_millis(16);
/// How long the guest may go without acknowledging a packet: far past any slow phone.
const OUT_ACK_TIMEOUT: Duration = Duration::from_secs(300);
/// Packets in flight. The controller queues at most 64 host messages (`VUSB_HOST_FIFO`) and pauses
/// the chardev beyond that.
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
    /// Endpoints whose stream writer only needs waking once this many acknowledgements are in.
    out_ack_want: HashMap<u8, usize>,
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
    out_ack_timeout: Duration,
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
            out_ack_timeout: OUT_ACK_TIMEOUT,
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
        // QEMU pauses its end when the controller's queue is full; a guest that has stopped reading
        // must fail a send the way it fails an acknowledgement.
        stream.set_write_timeout(Some(self.out_ack_timeout))?;
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

    fn read_loop(&self, r: &mut impl Read) -> io::Result<()> {
        let mut buf = vec![0u8; READ_BUFFER];
        let mut have = 0;
        loop {
            let n = match r.read(&mut buf[have..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            have += n;
            let used = self.dispatch_frames(&buf[..have])?;
            buf.copy_within(used..have, 0);
            have -= used;
        }
    }

    /// Every complete frame in `data`, under one lock and with one wakeup. Returns the bytes used.
    fn dispatch_frames(&self, data: &[u8]) -> io::Result<usize> {
        let mut rest = data;
        let mut wake = false;
        let mut result = Ok(());
        let mut s = self.lock();
        while rest.len() >= 4 {
            let length = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
            if length == 0 {
                rest = &rest[4..]; // an empty send (OpenAVB answers a malformed event so)
                continue;
            }
            if !(2..=VUSB_MSG_MAX).contains(&length) {
                result = Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid vusb response length {length}"),
                ));
                break;
            }
            if rest.len() < 4 + length {
                break;
            }
            wake |= self.dispatch(&mut s, &rest[4..4 + length]);
            rest = &rest[4 + length..];
        }
        drop(s);
        if wake {
            self.cond.notify_all();
        }
        result.map(|()| data.len() - rest.len())
    }

    /// Queue one message; false when the thread waiting for it can sleep on.
    fn dispatch(&self, s: &mut State, msg: &[u8]) -> bool {
        let (code, ep) = (msg[0], msg[1]);
        let data = msg[2..].to_vec();
        match code {
            CODE_DRIVER_RESET => {
                eprintln!("vusbd: guest re-initialised its USB controller: device unplugged");
                self.unplug(s);
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
            _ => {
                let want = s.out_ack_want.get(&ep).copied();
                let queue = s.out_acks.entry(ep).or_default();
                queue.push_back((code, data));
                return code != CODE_DATA || want.is_none_or(|w| queue.len() >= w);
            }
        }
        true
    }

    /// Fail everything waiting on the old device, and start fresh queues.
    fn unplug(&self, s: &mut State) {
        s.epoch += 1;
        s.detached = true;
        s.ctrl.clear();
        s.aborts.clear();
        s.out_acks.clear();
        s.out_ack_want.clear();
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
        let mut frame = Vec::with_capacity(FRAME_HEADER + payload.len());
        push_frame(&mut frame, ep, typ, payload);
        self.write_frames(&frame)
    }

    /// One write of whole frames. Failing part-way leaves the stream out of step, so the connection
    /// is dropped and the device with it.
    fn write_frames(&self, frames: &[u8]) -> Result<()> {
        let mut w = self.write.lock().unwrap();
        let Some(stream) = w.as_mut() else {
            return Err(LinkError::NoDevice);
        };
        stream.write_all(frames).map_err(|_| {
            let _ = stream.shutdown(Shutdown::Both);
            LinkError::NoDevice
        })
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
        let mut delay = NAK_DELAY_MIN;
        loop {
            let (code, data) =
                self.wait_for(epoch, deadline, cancel, |s| s.queue(queue).pop_front())?;
            match code {
                CODE_DATA => return Ok(data),
                CODE_STALL => return Err(LinkError::Stall),
                CODE_NAK => {
                    thread::sleep(delay);
                    delay = (delay * 2).min(NAK_DELAY_MAX);
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
                CODE_NAK => thread::sleep(NAK_DELAY_MIN),
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
    /// A NAK says the guest did not take the message and it holds no copy, so a retry resends it.
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
    /// `window` packets are in flight, written as one batch and refilled once half have been
    /// acknowledged, so a transfer of hundreds of thousands of packets costs neither a round trip
    /// nor a process wakeup per packet. A guest that stops acknowledging (a panic in iBoot, say)
    /// fails the transfer instead of hanging it.
    pub fn transfer_out_stream(
        &self,
        address: u8,
        payload: &[u8],
        packet: usize,
        window: usize,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        let epoch = self.epoch();
        let mut chunks = payload.chunks(packet).peekable();
        let mut batch =
            Vec::with_capacity(window.min(payload.len().div_ceil(packet)) * (FRAME_HEADER + packet));
        let mut in_flight = 0;
        loop {
            batch.clear();
            while in_flight < window {
                let Some(chunk) = chunks.next() else { break };
                push_frame(&mut batch, address, MSG_DATA, chunk);
                in_flight += 1;
            }
            if !batch.is_empty() {
                self.write_frames(&batch)?;
            }
            let target = if chunks.peek().is_some() { window / 2 } else { 0 };
            if in_flight > target {
                self.take_out_acks(address, in_flight - target, epoch, cancel)?;
                in_flight = target;
            }
            if in_flight == 0 && chunks.peek().is_none() {
                return Ok(());
            }
        }
    }

    /// Wait for and consume `want` acknowledgements. The timeout is since the last one to arrive.
    fn take_out_acks(
        &self,
        address: u8,
        want: usize,
        epoch: u64,
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        self.lock().out_ack_want.insert(address, want);
        let result = self.wait_out_acks(address, want, epoch, cancel);
        self.lock().out_ack_want.remove(&address);
        for (code, _) in result? {
            match code {
                CODE_DATA => {}
                CODE_STALL => return Err(LinkError::Stall),
                CODE_NAK => {
                    return Err(LinkError::Protocol("NAK from the DFU/recovery controller".into()))
                }
                other => {
                    return Err(LinkError::Protocol(format!(
                        "unexpected vusb response code {other}"
                    )))
                }
            }
        }
        Ok(())
    }

    fn wait_out_acks(
        &self,
        address: u8,
        want: usize,
        epoch: u64,
        cancel: Option<&AtomicBool>,
    ) -> Result<Vec<Response>> {
        let mut seen = 0;
        let mut deadline = Instant::now() + self.out_ack_timeout;
        loop {
            let got = self.wait_for(epoch, Some(deadline), cancel, |s| {
                let queue = s.out_acks.entry(address).or_default();
                if queue.len() >= want || queue.iter().any(|(code, _)| *code != CODE_DATA) {
                    let n = want.min(queue.len());
                    Some(queue.drain(..n).collect::<Vec<_>>())
                } else {
                    None
                }
            });
            match got {
                Err(LinkError::Timeout) => {
                    let arrived = self.lock().out_acks.entry(address).or_default().len();
                    if arrived == seen {
                        return Err(LinkError::Timeout);
                    }
                    seen = arrived;
                    deadline = Instant::now() + self.out_ack_timeout;
                }
                other => return other,
            }
        }
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

fn push_frame(out: &mut Vec<u8>, ep: u8, typ: u8, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(ep);
    out.push(typ);
    out.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const EP: u8 = 0x01;

    /// A link connected to the test as if it were QEMU.
    fn connected(ack_timeout: Duration) -> (Arc<Link>, UnixStream) {
        let (ours, peer) = UnixStream::pair().unwrap();
        let mut link = Link::new(Duration::from_secs(5));
        link.out_ack_timeout = ack_timeout;
        let link = Arc::new(link);
        let serving = link.clone();
        thread::spawn(move || {
            let _ = serving.serve(ours);
        });
        // `serve` publishes the stream on its own thread.
        while link.write.lock().unwrap().is_none() {
            thread::yield_now();
        }
        (link, peer)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    /// One read, which must hold whole frames; returns (ep, type, payload) of each.
    fn read_frames(peer: &mut UnixStream) -> Vec<(u8, u8, Vec<u8>)> {
        let mut buf = vec![0u8; 1 << 20];
        let n = peer.read(&mut buf).unwrap();
        let mut rest = &buf[..n];
        let mut frames = Vec::new();
        while !rest.is_empty() {
            let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
            assert!(rest.len() >= FRAME_HEADER + len, "frame split across writes");
            frames.push((rest[4], rest[5], rest[FRAME_HEADER..FRAME_HEADER + len].to_vec()));
            rest = &rest[FRAME_HEADER + len..];
        }
        frames
    }

    fn read_n_frames(peer: &mut UnixStream, n: usize) -> Vec<(u8, u8, Vec<u8>)> {
        let mut frames = Vec::new();
        while frames.len() < n {
            frames.extend(read_frames(peer));
        }
        frames
    }

    fn reply(peer: &mut UnixStream, code: u8, ep: u8, data: &[u8]) {
        let mut msg = ((2 + data.len()) as u32).to_le_bytes().to_vec();
        msg.extend_from_slice(&[code, ep]);
        msg.extend_from_slice(data);
        peer.write_all(&msg).unwrap();
    }

    fn acks(peer: &mut UnixStream, n: usize) {
        let mut all = Vec::new();
        for _ in 0..n {
            all.extend_from_slice(&2u32.to_le_bytes());
            all.extend_from_slice(&[CODE_DATA, EP]);
        }
        peer.write_all(&all).unwrap();
    }

    fn stream(link: &Arc<Link>, payload: Vec<u8>, window: usize) -> thread::JoinHandle<Result<()>> {
        let link = link.clone();
        thread::spawn(move || link.transfer_out_stream(EP, &payload, 512, window, None))
    }

    #[test]
    fn a_window_is_one_write_and_refills_at_half() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let payload = pattern(512 * 20);
        let sender = stream(&link, payload.clone(), 8);

        let first = read_frames(&mut peer);
        assert_eq!(first.len(), 8, "the whole window in one write");

        // Half a window, less one, must not provoke more data.
        acks(&mut peer, 3);
        peer.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut probe = [0u8; 1];
        let idle = peer.read(&mut probe).unwrap_err();
        assert_eq!(idle.kind(), io::ErrorKind::WouldBlock);
        peer.set_read_timeout(None).unwrap();

        acks(&mut peer, 1);
        let second = read_frames(&mut peer);
        assert_eq!(second.len(), 4, "refill of the half just acknowledged");

        acks(&mut peer, 4);
        let third = read_frames(&mut peer);
        assert_eq!(third.len(), 4);
        acks(&mut peer, 4);
        let last = read_frames(&mut peer);
        assert_eq!(last.len(), 4);

        // Not complete until everything has been acknowledged.
        acks(&mut peer, 3);
        thread::sleep(Duration::from_millis(50));
        assert!(!sender.is_finished());
        acks(&mut peer, 5);
        sender.join().unwrap().unwrap();

        let mut got = Vec::new();
        for (ep, typ, data) in first.into_iter().chain(second).chain(third).chain(last) {
            assert_eq!((ep, typ), (EP, MSG_DATA));
            got.extend_from_slice(&data);
        }
        assert_eq!(got, payload, "packets in order and intact");
    }

    #[test]
    fn a_short_tail_and_a_window_of_one() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let payload = pattern(512 * 2 + 100);
        let sender = stream(&link, payload.clone(), 1);
        let mut got = Vec::new();
        for _ in 0..3 {
            let frames = read_frames(&mut peer);
            assert_eq!(frames.len(), 1, "a window of one is one packet per round trip");
            got.extend_from_slice(&frames[0].2);
            acks(&mut peer, 1);
        }
        sender.join().unwrap().unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn acknowledgements_fragmented_and_coalesced_are_all_counted() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let sender = stream(&link, pattern(512 * 6), 6);
        read_n_frames(&mut peer, 6);
        let mut all = Vec::new();
        for _ in 0..6 {
            all.extend_from_slice(&2u32.to_le_bytes());
            all.extend_from_slice(&[CODE_DATA, EP]);
        }
        for byte in all {
            peer.write_all(&[byte]).unwrap();
        }
        sender.join().unwrap().unwrap();
    }

    #[test]
    fn no_acknowledgement_fails_at_the_timeout() {
        let (link, mut peer) = connected(Duration::from_millis(100));
        let sender = stream(&link, pattern(512 * 4), 4);
        read_n_frames(&mut peer, 4);
        let started = Instant::now();
        assert_eq!(sender.join().unwrap(), Err(LinkError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_timeout_counts_from_the_last_acknowledgement() {
        let (link, mut peer) = connected(Duration::from_millis(300));
        let sender = stream(&link, pattern(512 * 8), 8);
        read_n_frames(&mut peer, 8);
        // Far longer than the timeout in total, but always making progress.
        for _ in 0..8 {
            thread::sleep(Duration::from_millis(150));
            acks(&mut peer, 1);
        }
        sender.join().unwrap().unwrap();
    }

    #[test]
    fn a_stall_fails_the_stream() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let sender = stream(&link, pattern(512 * 4), 4);
        read_n_frames(&mut peer, 4);
        acks(&mut peer, 1);
        reply(&mut peer, CODE_STALL, EP, &[]);
        assert_eq!(sender.join().unwrap(), Err(LinkError::Stall));
    }

    #[test]
    fn a_driver_reset_fails_the_stream() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let sender = stream(&link, pattern(512 * 4), 4);
        read_n_frames(&mut peer, 4);
        reply(&mut peer, CODE_DRIVER_RESET, 0, &[]);
        assert_eq!(sender.join().unwrap(), Err(LinkError::NoDevice));
    }

    #[test]
    fn the_chardev_closing_fails_the_stream() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let sender = stream(&link, pattern(512 * 4), 4);
        read_n_frames(&mut peer, 4);
        drop(peer);
        assert_eq!(sender.join().unwrap(), Err(LinkError::NoDevice));
    }

    #[test]
    fn cancelling_stops_the_stream() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let cancel = Arc::new(AtomicBool::new(false));
        let (l, c, payload) = (link.clone(), cancel.clone(), pattern(512 * 4));
        let sender =
            thread::spawn(move || l.transfer_out_stream(EP, &payload, 512, 4, Some(&*c)));
        read_n_frames(&mut peer, 4);
        cancel.store(true, Ordering::SeqCst);
        link.notify();
        assert_eq!(sender.join().unwrap(), Err(LinkError::Cancelled));
    }

    #[test]
    fn nak_backs_off_and_resets_on_data() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let l = link.clone();
        let receiver = thread::spawn(move || {
            let first = l.transfer_in(0x81, 64, None).unwrap();
            let second = l.transfer_in(0x81, 64, None).unwrap();
            (first, second)
        });
        let mut last = Instant::now();
        let mut gaps = Vec::new();
        read_frames(&mut peer);
        for _ in 0..6 {
            reply(&mut peer, CODE_NAK, 0x81, &[]);
            read_frames(&mut peer);
            let now = Instant::now();
            gaps.push(now - last);
            last = now;
        }
        reply(&mut peer, CODE_DATA, 0x81, b"one");
        read_frames(&mut peer); // the second transfer's request, straight away
        reply(&mut peer, CODE_NAK, 0x81, &[]);
        let before = Instant::now();
        read_frames(&mut peer);
        let first_retry_after_data = before.elapsed();
        reply(&mut peer, CODE_DATA, 0x81, b"two");
        let (first, second) = receiver.join().unwrap();
        assert_eq!((first.as_slice(), second.as_slice()), (&b"one"[..], &b"two"[..]));
        // 1, 2, 4, 8, 16, 16 ms; sleeps only ever overshoot, so check the floor of each.
        let floor = [1u64, 2, 4, 8, 16, 16];
        for (gap, ms) in gaps.iter().zip(floor) {
            assert!(*gap >= Duration::from_millis(ms), "{gaps:?}");
        }
        assert!(first_retry_after_data < Duration::from_millis(16), "{first_retry_after_data:?}");
    }

    #[test]
    fn a_nak_on_out_resends_the_whole_payload() {
        let (link, mut peer) = connected(Duration::from_secs(10));
        let payload = pattern(32 * 1024);
        let (l, p) = (link.clone(), payload.clone());
        let sender = thread::spawn(move || l.transfer_out(EP, &p, None));
        let sent = read_n_frames(&mut peer, 1);
        assert_eq!(sent[0].2, payload);
        reply(&mut peer, CODE_NAK, EP, &[]);
        let again = read_n_frames(&mut peer, 1);
        assert_eq!(again[0].2, payload);
        reply(&mut peer, CODE_DATA, EP, &[]);
        sender.join().unwrap().unwrap();
    }
}
