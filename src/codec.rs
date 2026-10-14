//! What MPEG-TS input needs from the codecs: H.264 in Annex B made length-prefixed, its SPS read
//! for the picture size, AAC freed of its ADTS headers, and the sample entries an MP4 needs.

use crate::bytes::Wr;

/// Rewrites an Annex B access unit with a 4-byte length before each NAL unit, the way ffmpeg
/// does it (`ff_avc_parse_nal_units`), so the result can be checked against ffmpeg byte for byte.
pub fn annexb_to_avcc(b: &[u8], out: &mut Vec<u8>) {
    let end = b.len();
    let mut nal_start = find_startcode(b, 0);
    loop {
        while nal_start < end {
            nal_start += 1;
            if b[nal_start - 1] != 0 {
                break;
            }
        }
        if nal_start >= end {
            break;
        }
        let nal_end = find_startcode(b, nal_start);
        out.extend_from_slice(&((nal_end - nal_start) as u32).to_be_bytes());
        out.extend_from_slice(&b[nal_start..nal_end]);
        nal_start = nal_end;
    }
}

/// Position of the next `00 00 01` at or after `from`, moved back one byte when a fourth zero
/// leads it; `b.len()` when there is none.
fn find_startcode(b: &[u8], from: usize) -> usize {
    let mut i = from;
    while i + 2 < b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            return if i > from && b[i - 1] == 0 { i - 1 } else { i };
        }
        i += 1;
    }
    b.len()
}

/// The NAL units of a length-prefixed access unit.
pub fn nals(avcc: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut p = 0;
    std::iter::from_fn(move || {
        if p + 4 > avcc.len() {
            return None;
        }
        let n = u32::from_be_bytes(avcc[p..p + 4].try_into().unwrap()) as usize;
        let nal = avcc.get(p + 4..p + 4 + n)?;
        p += 4 + n;
        Some(nal)
    })
}

/// Reads bits of an RBSP, the emulation prevention bytes already taken out.
struct Bits<'a> {
    b: &'a [u8],
    p: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.b.get(self.p / 8)?;
        let v = (byte >> (7 - self.p % 8)) & 1;
        self.p += 1;
        Some(v as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = v << 1 | self.bit()?;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// What an H.264 sequence parameter set says about the picture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sps {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub sar: (u32, u32),
    pub chroma_format: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
}

const SAR: [(u32, u32); 17] = [
    (0, 1), (1, 1), (12, 11), (10, 11), (16, 11), (40, 33), (24, 11), (20, 11), (32, 11),
    (80, 33), (18, 11), (15, 11), (64, 33), (160, 99), (4, 3), (3, 2), (2, 1),
];

/// Parses an SPS NAL unit, header byte included.
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    let rbsp = unescape(nal);
    let mut r = Bits { b: rbsp.get(1..)?, p: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    let id = r.ue()?;
    let (mut chroma_format, mut bit_depth_luma, mut bit_depth_chroma) = (1, 8, 8);
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format = r.ue()?;
        if chroma_format == 3 {
            r.bit()?;
        }
        bit_depth_luma = r.ue()? + 8;
        bit_depth_chroma = r.ue()? + 8;
        r.bit()?;
        if r.bit()? == 1 {
            for i in 0..if chroma_format != 3 { 8 } else { 12 } {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?;
    let w_mbs = r.ue()? + 1;
    let h_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?; // direct_8x8_inference
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        (cl, cr, ct, cb) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
    }
    let mut sar = (1, 1);
    if r.bit() == Some(1) && r.bit() == Some(1) {
        let idc = r.bits(8)? as usize;
        sar = if idc == 255 { (r.bits(16)?, r.bits(16)?) } else { *SAR.get(idc).unwrap_or(&(1, 1)) };
        if sar.0 == 0 || sar.1 == 0 {
            sar = (1, 1);
        }
    }
    let (ux, uy) = match chroma_format {
        0 => (1, 2 - frame_mbs_only),
        1 => (2, 2 * (2 - frame_mbs_only)),
        2 => (2, 2 - frame_mbs_only),
        _ => (1, 2 - frame_mbs_only),
    };
    let width = (w_mbs * 16).checked_sub(ux * (cl + cr))?;
    let height = ((2 - frame_mbs_only) * h_units * 16).checked_sub(uy * (ct + cb))?;
    Some(Sps { id, width, height, sar, chroma_format, bit_depth_luma, bit_depth_chroma })
}

/// An `stsd` holding one `avc1` sample entry built from the parameter sets.
pub fn avc1_stsd(spss: &[Vec<u8>], ppss: &[Vec<u8>], sps: &Sps) -> Vec<u8> {
    let first = &spss[0];
    let mut w = Wr::default();
    let stsd = w.open_full(b"stsd", 0, 0);
    w.u32(1);
    let entry = w.open(b"avc1");
    w.zeros(6);
    w.u16(1); // data reference index
    w.zeros(16);
    w.u16(sps.width as u16);
    w.u16(sps.height as u16);
    w.u32(0x48_0000); // 72 dpi
    w.u32(0x48_0000);
    w.u32(0);
    w.u16(1); // frame count
    w.zeros(32); // compressor name
    w.u16(0x18); // depth
    w.i16(-1);
    let avcc = w.open(b"avcC");
    w.bytes(&[1, first[1], first[2], first[3], 0xff, 0xe0 | spss.len() as u8]);
    for s in spss {
        w.u16(s.len() as u16);
        w.bytes(s);
    }
    w.bytes(&[ppss.len() as u8]);
    for p in ppss {
        w.u16(p.len() as u16);
        w.bytes(p);
    }
    if matches!(first[1], 100 | 110 | 122 | 144) {
        w.bytes(&[
            0xfc | sps.chroma_format as u8,
            0xf8 | (sps.bit_depth_luma - 8) as u8,
            0xf8 | (sps.bit_depth_chroma - 8) as u8,
            0,
        ]);
    }
    w.close(avcc);
    if sps.sar != (1, 1) {
        let pasp = w.open(b"pasp");
        w.u32(sps.sar.0);
        w.u32(sps.sar.1);
        w.close(pasp);
    }
    w.close(entry);
    w.close(stsd);
    w.v
}

/// The fixed header of an ADTS frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adts {
    pub object_type: u8,
    pub freq_index: u8,
    pub channels: u8,
    pub header_len: usize,
    pub frame_len: usize,
    /// Raw data blocks in the frame, each 1024 samples.
    pub blocks: u8,
}

pub const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

impl Adts {
    pub fn parse(b: &[u8]) -> Option<Adts> {
        if b.len() < 7 || b[0] != 0xff || b[1] & 0xf6 != 0xf0 {
            return None;
        }
        let protection_absent = b[1] & 1;
        let freq_index = (b[2] >> 2) & 0xf;
        let frame_len = ((b[3] as usize & 3) << 11) | (b[4] as usize) << 3 | (b[5] as usize) >> 5;
        let header_len = if protection_absent == 1 { 7 } else { 9 };
        if freq_index > 12 || frame_len <= header_len {
            return None;
        }
        Some(Adts {
            object_type: (b[2] >> 6) + 1,
            freq_index,
            channels: ((b[2] & 1) << 2) | (b[3] >> 6),
            header_len,
            frame_len,
            blocks: (b[6] & 3) + 1,
        })
    }

    pub fn rate(&self) -> u32 {
        AAC_RATES[self.freq_index as usize]
    }

    /// Same stream parameters, so the frames can share one sample description.
    pub fn same_config(&self, o: &Adts) -> bool {
        (self.object_type, self.freq_index, self.channels) == (o.object_type, o.freq_index, o.channels)
    }
}

/// An `stsd` holding one `mp4a` sample entry for the AAC the ADTS headers describe.
pub fn mp4a_stsd(a: &Adts, es_id: u16) -> Vec<u8> {
    let asc = [
        (a.object_type << 3) | (a.freq_index >> 1),
        ((a.freq_index & 1) << 7) | (a.channels << 3),
    ];
    let mut w = Wr::default();
    let stsd = w.open_full(b"stsd", 0, 0);
    w.u32(1);
    let entry = w.open(b"mp4a");
    w.zeros(6);
    w.u16(1);
    w.zeros(8);
    w.u16(if a.channels == 0 { 2 } else { a.channels as u16 });
    w.u16(16);
    w.u32(0);
    w.u32(a.rate() << 16);
    let esds = w.open_full(b"esds", 0, 0);
    let dsi_len = 2;
    let dcd_len = 13 + 2 + dsi_len;
    let es_len = 3 + 2 + dcd_len + 3;
    w.bytes(&[0x03, es_len as u8]);
    w.u16(es_id);
    w.bytes(&[0, 0x04, dcd_len as u8, 0x40, 0x15]);
    w.bytes(&[0, 0, 0]); // buffer size
    w.u32(0); // max bitrate: not known
    w.u32(0); // average bitrate: not known
    w.bytes(&[0x05, dsi_len as u8]);
    w.bytes(&asc);
    w.bytes(&[0x06, 1, 2]);
    w.close(esds);
    w.close(entry);
    w.close(stsd);
    w.v
}

pub fn hdlr(handler: &[u8; 4], name: &str) -> Vec<u8> {
    let mut w = Wr::default();
    let at = w.open_full(b"hdlr", 0, 0);
    w.u32(0);
    w.bytes(handler);
    w.zeros(12);
    w.bytes(name.as_bytes());
    w.bytes(&[0]);
    w.close(at);
    w.v
}

pub fn vmhd() -> Vec<u8> {
    let mut w = Wr::default();
    let at = w.open_full(b"vmhd", 0, 1);
    w.zeros(8);
    w.close(at);
    w.v
}

pub fn smhd() -> Vec<u8> {
    let mut w = Wr::default();
    let at = w.open_full(b"smhd", 0, 0);
    w.zeros(4);
    w.close(at);
    w.v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startcodes_like_ffmpeg() {
        // A 4-byte start code, a 3-byte one, and trailing zeros kept in the last NAL as ffmpeg keeps them.
        let au = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x65, 0x88, 0x84, 0, 0, 0];
        let mut out = Vec::new();
        annexb_to_avcc(&au, &mut out);
        assert_eq!(out, [0, 0, 0, 2, 0x09, 0xf0, 0, 0, 0, 6, 0x65, 0x88, 0x84, 0, 0, 0]);
    }

    #[test]
    fn sps_1080p() {
        // The SPS of the Mux test stream: High profile, 1088 coded lines cropped to 1080.
        let sps = [0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x80, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00,
                   0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x01, 0xe0, 0x3c, 0x60, 0xc6, 0x68];
        let s = parse_sps(&sps).unwrap();
        assert_eq!((s.width, s.height, s.sar, s.chroma_format), (1920, 1080, (1, 1), 1));
    }
}
