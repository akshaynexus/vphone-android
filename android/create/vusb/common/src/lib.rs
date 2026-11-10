//! Wire protocol between the broker (vusbd) and the libusb-1.0 replacement.
//!
//! One Unix stream socket per libusb context. Frames: `u32 len (LE)` then `len`
//! bytes, the first being the message type. All integers little-endian.

use std::io::{self, Read, Write};

pub mod active;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// libusb_transfer_status values, carried as they are.
pub mod status {
    pub const COMPLETED: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const TIMED_OUT: i32 = 2;
    pub const CANCELLED: i32 = 3;
    pub const STALL: i32 = 4;
    pub const NO_DEVICE: i32 = 5;
    pub const OVERFLOW: i32 = 6;
}

pub mod transfer_type {
    pub const CONTROL: u8 = 0;
    pub const BULK: u8 = 2;
    pub const INTERRUPT: u8 = 3;
}

/// Operations answered with [`S2C::OpReply`]. `result` is a libusb_error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    Claim = 1,
    Release = 2,
    SetConfig = 3,
    Reset = 4,
}

#[derive(Debug)]
pub enum C2S {
    Hello {
        version: u32,
    },
    Op {
        opid: u32,
        gen: u32,
        op: OpCode,
        arg: u8,
    },
    Submit {
        id: u64,
        gen: u32,
        /// bEndpointAddress, with the direction bit (0 for control).
        endpoint: u8,
        kind: u8,
        /// Data-stage length: bytes to read for IN, ignored for OUT (`payload` length).
        length: u32,
        setup: [u8; 8],
        payload: Vec<u8>,
    },
    Cancel {
        id: u64,
    },
}

#[derive(Clone, Debug)]
pub struct DeviceBlob {
    pub gen: u32,
    pub address: u8,
    pub active_config: u8,
    pub device_descriptor: [u8; 18],
    pub configs: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub enum S2C {
    DeviceAdded(DeviceBlob),
    DeviceLeft {
        gen: u32,
    },
    ConfigChanged {
        gen: u32,
        value: u8,
    },
    Complete {
        id: u64,
        status: i32,
        actual: u32,
        data: Vec<u8>,
    },
    OpReply {
        opid: u32,
        result: i32,
    },
}

struct Writer(Vec<u8>);

impl Writer {
    fn new(kind: u8) -> Self {
        let mut b = Vec::with_capacity(64);
        b.extend_from_slice(&[0, 0, 0, 0, kind]);
        Writer(b)
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[0..4].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

struct Reader<'a>(&'a [u8]);

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "short vusb frame")
}

impl Reader<'_> {
    fn u8(&mut self) -> io::Result<u8> {
        let (v, r) = self.0.split_first().ok_or_else(short)?;
        self.0 = r;
        Ok(*v)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(short());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn rest(self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl C2S {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            C2S::Hello { version } => {
                let mut w = Writer::new(1);
                w.u32(*version);
                w.finish()
            }
            C2S::Op {
                opid,
                gen,
                op,
                arg,
            } => {
                let mut w = Writer::new(2);
                w.u32(*opid).u32(*gen).u8(*op as u8).u8(*arg);
                w.finish()
            }
            C2S::Submit {
                id,
                gen,
                endpoint,
                kind,
                length,
                setup,
                payload,
            } => {
                let mut w = Writer::new(3);
                w.u64(*id)
                    .u32(*gen)
                    .u8(*endpoint)
                    .u8(*kind)
                    .u32(*length)
                    .bytes(setup)
                    .bytes(payload);
                w.finish()
            }
            C2S::Cancel { id } => {
                let mut w = Writer::new(4);
                w.u64(*id);
                w.finish()
            }
        }
    }

    pub fn decode(frame: &[u8]) -> io::Result<C2S> {
        let mut r = Reader(frame);
        Ok(match r.u8()? {
            1 => C2S::Hello { version: r.u32()? },
            2 => {
                let opid = r.u32()?;
                let gen = r.u32()?;
                let op = match r.u8()? {
                    1 => OpCode::Claim,
                    2 => OpCode::Release,
                    3 => OpCode::SetConfig,
                    4 => OpCode::Reset,
                    n => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unknown op {n}"),
                        ))
                    }
                };
                C2S::Op {
                    opid,
                    gen,
                    op,
                    arg: r.u8()?,
                }
            }
            3 => {
                let id = r.u64()?;
                let gen = r.u32()?;
                let endpoint = r.u8()?;
                let kind = r.u8()?;
                let length = r.u32()?;
                let setup: [u8; 8] = r.bytes(8)?.try_into().unwrap();
                C2S::Submit {
                    id,
                    gen,
                    endpoint,
                    kind,
                    length,
                    setup,
                    payload: r.rest(),
                }
            }
            4 => C2S::Cancel { id: r.u64()? },
            n => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown client message {n}"),
                ))
            }
        })
    }
}

impl S2C {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            S2C::DeviceAdded(d) => {
                let mut w = Writer::new(11);
                w.u32(d.gen)
                    .u8(d.address)
                    .u8(d.active_config)
                    .bytes(&d.device_descriptor)
                    .u32(d.configs.len() as u32);
                for c in &d.configs {
                    w.u32(c.len() as u32).bytes(c);
                }
                w.finish()
            }
            S2C::DeviceLeft { gen } => {
                let mut w = Writer::new(12);
                w.u32(*gen);
                w.finish()
            }
            S2C::ConfigChanged { gen, value } => {
                let mut w = Writer::new(15);
                w.u32(*gen).u8(*value);
                w.finish()
            }
            S2C::Complete {
                id,
                status,
                actual,
                data,
            } => {
                let mut w = Writer::new(13);
                w.u64(*id).i32(*status).u32(*actual).bytes(data);
                w.finish()
            }
            S2C::OpReply { opid, result } => {
                let mut w = Writer::new(14);
                w.u32(*opid).i32(*result);
                w.finish()
            }
        }
    }

    pub fn decode(frame: &[u8]) -> io::Result<S2C> {
        let mut r = Reader(frame);
        Ok(match r.u8()? {
            11 => {
                let gen = r.u32()?;
                let address = r.u8()?;
                let active_config = r.u8()?;
                let device_descriptor: [u8; 18] = r.bytes(18)?.try_into().unwrap();
                let n = r.u32()? as usize;
                if n > 255 {
                    return Err(short());
                }
                let mut configs = Vec::with_capacity(n);
                for _ in 0..n {
                    let len = r.u32()? as usize;
                    configs.push(r.bytes(len)?.to_vec());
                }
                S2C::DeviceAdded(DeviceBlob {
                    gen,
                    address,
                    active_config,
                    device_descriptor,
                    configs,
                })
            }
            12 => S2C::DeviceLeft { gen: r.u32()? },
            13 => {
                let id = r.u64()?;
                let status = r.i32()?;
                let actual = r.u32()?;
                S2C::Complete {
                    id,
                    status,
                    actual,
                    data: r.rest(),
                }
            }
            14 => S2C::OpReply {
                opid: r.u32()?,
                result: r.i32()?,
            },
            15 => S2C::ConfigChanged {
                gen: r.u32()?,
                value: r.u8()?,
            },
            n => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown broker message {n}"),
                ))
            }
        })
    }
}

/// Read one frame body. `EOF` before the first byte is `ErrorKind::UnexpectedEof`.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad vusb frame length {len}"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

pub fn write_frame<W: Write>(w: &mut W, frame: &[u8]) -> io::Result<()> {
    w.write_all(frame)
}
