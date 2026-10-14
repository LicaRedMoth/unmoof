//! Big-endian reading and writing, and iteration over ISO BMFF boxes.

use crate::{Error, Result};

/// A cursor over a byte slice that reads big-endian fields.
pub struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }

    pub fn left(&self) -> usize {
        self.b.len() - self.p
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.left() < n {
            return Err(Error("a box ends before its fields do".into()));
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    /// Reads the version and flags of a full box.
    pub fn full(&mut self) -> Result<(u8, u32)> {
        let vf = self.u32()?;
        Ok(((vf >> 24) as u8, vf & 0xff_ffff))
    }
}

/// One box: its type, its payload and the whole box with its header.
pub struct BoxRef<'a> {
    pub typ: [u8; 4],
    pub payload: &'a [u8],
    pub raw: &'a [u8],
    /// Offset of the box within the slice it was found in.
    pub start: usize,
}

/// The boxes laid one after another in a slice, each wholly inside it.
pub struct Boxes<'a> {
    b: &'a [u8],
    p: usize,
}

pub fn boxes(b: &[u8]) -> Boxes<'_> {
    Boxes { b, p: 0 }
}

/// Reads a box header at the start of `b`: type, header length and total size.
/// A size of 0 (to the end of the file) comes back as `None`.
pub fn header(b: &[u8]) -> Option<([u8; 4], usize, Option<u64>)> {
    if b.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes(b[0..4].try_into().unwrap()) as u64;
    let typ: [u8; 4] = b[4..8].try_into().unwrap();
    match size {
        0 => Some((typ, 8, None)),
        1 => {
            if b.len() < 16 {
                return None;
            }
            Some((typ, 16, Some(u64::from_be_bytes(b[8..16].try_into().unwrap()))))
        }
        _ => Some((typ, 8, Some(size))),
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<BoxRef<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = &self.b[self.p..];
        if rest.is_empty() {
            return None;
        }
        let Some((typ, hdr, size)) = header(rest) else {
            self.p = self.b.len();
            return Some(Err(Error("a box header is cut short".into())));
        };
        let size = size.unwrap_or(rest.len() as u64);
        if size < hdr as u64 || size > rest.len() as u64 {
            self.p = self.b.len();
            return Some(Err(Error(format!(
                "box '{}' claims {size} bytes but {} are left",
                fourcc(&typ),
                rest.len()
            ))));
        }
        let size = size as usize;
        let start = self.p;
        self.p += size;
        Some(Ok(BoxRef { typ, payload: &rest[hdr..size], raw: &rest[..size], start }))
    }
}

pub fn fourcc(t: &[u8; 4]) -> String {
    t.iter().map(|&c| if c.is_ascii_graphic() { c as char } else { '?' }).collect()
}

/// A growing buffer that writes big-endian fields and boxes.
#[derive(Default)]
pub struct Wr {
    pub v: Vec<u8>,
}

impl Wr {
    pub fn u16(&mut self, x: u16) {
        self.v.extend_from_slice(&x.to_be_bytes());
    }

    pub fn i16(&mut self, x: i16) {
        self.u16(x as u16);
    }

    pub fn u32(&mut self, x: u32) {
        self.v.extend_from_slice(&x.to_be_bytes());
    }

    pub fn i32(&mut self, x: i32) {
        self.u32(x as u32);
    }

    pub fn u64(&mut self, x: u64) {
        self.v.extend_from_slice(&x.to_be_bytes());
    }

    pub fn i64(&mut self, x: i64) {
        self.u64(x as u64);
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.v.extend_from_slice(b);
    }

    pub fn zeros(&mut self, n: usize) {
        self.v.resize(self.v.len() + n, 0);
    }

    /// Starts a box; `close` fills its size in.
    pub fn open(&mut self, typ: &[u8; 4]) -> usize {
        let at = self.v.len();
        self.u32(0);
        self.bytes(typ);
        at
    }

    pub fn open_full(&mut self, typ: &[u8; 4], version: u8, flags: u32) -> usize {
        let at = self.open(typ);
        self.u32((version as u32) << 24 | flags);
        at
    }

    pub fn close(&mut self, at: usize) {
        let size = (self.v.len() - at) as u32;
        self.v[at..at + 4].copy_from_slice(&size.to_be_bytes());
    }
}
