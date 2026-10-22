//! What TS input needs from the codecs: H.264 and HEVC made length-prefixed and their SPS read,
//! AAC and MPEG audio frame headers, and the sample entries an MP4 needs for them.

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

/// The type of a NAL unit from its first header byte, numbered as H.264 or HEVC number them.
pub fn nal_type(header: u8, hevc: bool) -> u8 {
    if hevc { header >> 1 & 0x3f } else { header & 0x1f }
}

/// Whether a NAL unit begins a picture playback can start from: an IDR picture in H.264, any
/// IRAP picture (BLA, IDR, CRA) in HEVC, as ffmpeg marks them.
pub fn is_random_access(nal: &[u8], hevc: bool) -> bool {
    let t = nal_type(nal[0], hevc);
    if hevc { (16..=23).contains(&t) } else { t == 5 }
}

/// The NAL units of a length-prefixed access unit, empty ones left out.
pub fn nals(avcc: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut p = 0;
    std::iter::from_fn(move || {
        loop {
            if p + 4 > avcc.len() {
                return None;
            }
            let n = u32::from_be_bytes(avcc[p..p + 4].try_into().unwrap()) as usize;
            let nal = avcc.get(p + 4..(p + 4).checked_add(n)?)?;
            p += 4 + n;
            if !nal.is_empty() {
                return Some(nal);
            }
        }
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
    (0, 1),
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
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
        bit_depth_luma = r.ue()?.checked_add(8).filter(|&d| d <= 16)?;
        bit_depth_chroma = r.ue()?.checked_add(8).filter(|&d| d <= 16)?;
        r.bit()?;
        if r.bit()? == 1 {
            for i in 0..if chroma_format != 3 { 8 } else { 12 } {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (i64::from(last) + i64::from(r.se()?) + 256).rem_euclid(256) as i32;
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
    // Past 4095 macroblocks a side (65 520 pixels) the SPS is damaged, and sizes would overflow.
    let w_mbs = r.ue()?.checked_add(1).filter(|&n| n < 4096)?;
    let h_units = r.ue()?.checked_add(1).filter(|&n| n < 4096)?;
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
    let (ux, uy): (u32, u32) = match chroma_format {
        0 => (1, 2 - frame_mbs_only),
        1 => (2, 2 * (2 - frame_mbs_only)),
        2 => (2, 2 - frame_mbs_only),
        _ => (1, 2 - frame_mbs_only),
    };
    let width = (w_mbs * 16).checked_sub(ux.checked_mul(cl.checked_add(cr)?)?)?;
    let height = ((2 - frame_mbs_only) * h_units * 16).checked_sub(uy.checked_mul(ct.checked_add(cb)?)?)?;
    Some(Sps { id, width, height, sar, chroma_format, bit_depth_luma, bit_depth_chroma })
}

/// An `stsd` holding one `avc1` sample entry built from the parameter sets.
pub fn avc1_stsd(spss: &[Vec<u8>], ppss: &[Vec<u8>], sps: &Sps) -> Vec<u8> {
    // avcC counts SPS in 5 bits, PPS in 8, and their lengths in 16.
    let fits = |n: &&Vec<u8>| n.len() <= usize::from(u16::MAX);
    let spss: Vec<&Vec<u8>> = spss.iter().filter(fits).take(31).collect();
    let ppss: Vec<&Vec<u8>> = ppss.iter().filter(fits).take(255).collect();
    let first = spss[0];
    let mut w = Wr::default();
    let stsd = w.open_full(b"stsd", 0, 0);
    w.u32(1);
    let entry = w.open(b"avc1");
    visual_entry(&mut w, sps.width, sps.height);
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
    pasp(&mut w, sps.sar);
    w.close(entry);
    w.close(stsd);
    w.v
}

/// The fields of a visual sample entry before its codec's boxes.
fn visual_entry(w: &mut Wr, width: u32, height: u32) {
    w.zeros(6);
    w.u16(1); // data reference index
    w.zeros(16);
    w.u16(width as u16);
    w.u16(height as u16);
    w.u32(0x48_0000); // 72 dpi
    w.u32(0x48_0000);
    w.u32(0);
    w.u16(1); // frame count
    w.zeros(32); // compressor name
    w.u16(0x18); // depth
    w.i16(-1);
}

fn pasp(w: &mut Wr, sar: (u32, u32)) {
    if sar != (1, 1) {
        let at = w.open(b"pasp");
        w.u32(sar.0);
        w.u32(sar.1);
        w.close(at);
    }
}

/// What an HEVC sequence parameter set says about the picture and the stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HevcSps {
    pub id: u32,
    /// The general profile, tier and level of `profile_tier_level`, as hvcC repeats them.
    pub ptl: [u8; 12],
    pub sub_layers: u8,
    pub temporal_id_nesting: bool,
    pub chroma_format: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub width: u32,
    pub height: u32,
    pub sar: (u32, u32),
}

/// Parses an HEVC SPS NAL unit, its 2-byte header included (H.265 7.3.2.2).
pub fn parse_hevc_sps(nal: &[u8]) -> Option<HevcSps> {
    let rbsp = unescape(nal);
    let b = rbsp.get(2..)?;
    let mut r = Bits { b, p: 0 };
    r.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as usize;
    let temporal_id_nesting = r.bit()? == 1;

    // The general part of profile_tier_level is byte-aligned: the 12 bytes after the first.
    let ptl: [u8; 12] = b.get(1..13)?.try_into().ok()?;
    r.p += 96;

    // Sub-layers may each repeat a profile (88 bits) and a level (8 bits).
    let mut present = [(false, false); 7];
    for p in present.iter_mut().take(max_sub_layers_minus1) {
        *p = (r.bit()? == 1, r.bit()? == 1);
    }
    if max_sub_layers_minus1 > 0 {
        r.p += 2 * (8 - max_sub_layers_minus1);
    }
    for &(profile, level) in present.iter().take(max_sub_layers_minus1) {
        if profile {
            r.p += 88;
        }
        if level {
            r.p += 8;
        }
    }

    let id = r.ue()?;
    let chroma_format = r.ue().filter(|&c| c <= 3)?;
    if chroma_format == 3 {
        r.bit()?; // separate_colour_plane_flag
    }

    // Levels stop at 16 888 pixels a side; past 65 535 the SPS is damaged.
    let coded_w = r.ue().filter(|w| (1..65536).contains(w))?;
    let coded_h = r.ue().filter(|h| (1..65536).contains(h))?;
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        (cl, cr, ct, cb) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
    }
    let (ux, uy): (u32, u32) = match chroma_format {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = coded_w.checked_sub(ux.checked_mul(cl.checked_add(cr)?)?)?;
    let height = coded_h.checked_sub(uy.checked_mul(ct.checked_add(cb)?)?)?;

    let bit_depth_luma = r.ue()?.checked_add(8).filter(|&d| d <= 16)?;
    let bit_depth_chroma = r.ue()?.checked_add(8).filter(|&d| d <= 16)?;
    let sar = hevc_sar(&mut r, max_sub_layers_minus1).unwrap_or((1, 1));
    Some(HevcSps {
        id,
        ptl,
        sub_layers: max_sub_layers_minus1 as u8 + 1,
        temporal_id_nesting,
        chroma_format,
        bit_depth_luma,
        bit_depth_chroma,
        width,
        height,
        sar,
    })
}

/// The sample aspect ratio in the VUI, which only walking the rest of the SPS reaches; None
/// when the SPS has no VUI, or ends or breaks first.
fn hevc_sar(r: &mut Bits, max_sub_layers_minus1: usize) -> Option<(u32, u32)> {
    let poc_lsb_bits = r.ue()?.checked_add(4).filter(|&n| n <= 16)?;
    let first = if r.bit()? == 1 { 0 } else { max_sub_layers_minus1 };
    // dec_pic_buffering, num_reorder_pics and max_latency_increase per sub-layer, then coding
    // and transform block sizes and transform hierarchy depths.
    for _ in 0..3 * (max_sub_layers_minus1 - first + 1) + 6 {
        r.ue()?;
    }
    if r.bit()? == 1 && r.bit()? == 1 {
        for size in 0..4 {
            for _ in (0..6).step_by(if size == 3 { 3 } else { 1 }) {
                if r.bit()? == 0 {
                    r.ue()?;
                } else {
                    if size > 1 {
                        r.se()?;
                    }
                    for _ in 0..64.min(1 << (4 + 2 * size)) {
                        r.se()?;
                    }
                }
            }
        }
    }
    r.bits(2)?; // amp, sample_adaptive_offset
    if r.bit()? == 1 {
        r.bits(8)?; // pcm sample bit depths
        r.ue()?;
        r.ue()?;
        r.bit()?;
    }
    // Short-term reference picture sets: one predicted from another counts that one's pictures.
    let mut counts: Vec<u32> = Vec::new();
    for i in 0..r.ue().filter(|&n| n <= 64)? {
        if i > 0 && r.bit()? == 1 {
            r.bit()?; // delta_rps_sign
            r.ue()?; // abs_delta_rps_minus1
            let mut n = 0;
            for _ in 0..=counts[i as usize - 1] {
                if r.bit()? == 1 || r.bit()? == 1 {
                    n += 1;
                }
            }
            counts.push(n);
        } else {
            let pics = r.ue().filter(|&n| n <= 16)? + r.ue().filter(|&n| n <= 16)?;
            for _ in 0..pics {
                r.ue()?;
                r.bit()?;
            }
            counts.push(pics);
        }
    }
    if r.bit()? == 1 {
        for _ in 0..r.ue().filter(|&n| n <= 32)? {
            r.bits(poc_lsb_bits)?; // lt_ref_pic_poc_lsb_sps
            r.bit()?;
        }
    }
    r.bits(2)?; // temporal_mvp, strong_intra_smoothing
    if r.bit()? == 0 || r.bit()? == 0 {
        return None; // no VUI, or no aspect ratio in it
    }
    let idc = r.bits(8)? as usize;
    let sar = if idc == 255 { (r.bits(16)?, r.bits(16)?) } else { *SAR.get(idc)? };
    (sar.0 > 0 && sar.1 > 0).then_some(sar)
}

/// An `stsd` holding one HEVC sample entry. With `complete` it is `hvc1`: these are all the
/// parameter sets, and the samples carry none. Otherwise it is `hev1`, and samples may too.
pub fn hevc_stsd(
    vpss: &[Vec<u8>],
    spss: &[Vec<u8>],
    ppss: &[Vec<u8>],
    sps: &HevcSps,
    complete: bool,
) -> Vec<u8> {
    let mut w = Wr::default();
    let stsd = w.open_full(b"stsd", 0, 0);
    w.u32(1);
    let entry = w.open(if complete { b"hvc1" } else { b"hev1" });
    visual_entry(&mut w, sps.width, sps.height);
    let hvcc = w.open(b"hvcC");
    w.bytes(&[1]);
    w.bytes(&sps.ptl);
    w.u16(0xf000); // min_spatial_segmentation_idc: unknown
    w.bytes(&[
        0xfc, // parallelismType: unknown
        0xfc | sps.chroma_format as u8,
        0xf8 | (sps.bit_depth_luma - 8) as u8,
        0xf8 | (sps.bit_depth_chroma - 8) as u8,
    ]);
    w.u16(0); // avgFrameRate: unknown
    w.bytes(&[sps.sub_layers.min(7) << 3 | u8::from(sps.temporal_id_nesting) << 2 | 3]);
    w.bytes(&[3]);
    for (t, list) in [(32u8, vpss), (33, spss), (34, ppss)] {
        let list: Vec<&Vec<u8>> =
            list.iter().filter(|n| n.len() <= usize::from(u16::MAX)).take(255).collect();
        w.bytes(&[if complete { 0x80 | t } else { t }]); // array_completeness, NAL unit type
        w.u16(list.len() as u16);
        for n in list {
            w.u16(n.len() as u16);
            w.bytes(n);
        }
    }
    w.close(hvcc);
    pasp(&mut w, sps.sar);
    w.close(entry);
    w.close(stsd);
    w.v
}

/// Whether an HEVC NAL unit is a VPS, SPS or PPS.
pub fn is_hevc_parameter_set(nal: &[u8]) -> bool {
    (32..=34).contains(&nal_type(nal[0], true))
}

/// A length-prefixed HEVC access unit without the parameter sets that equal one in `known`.
pub fn strip_parameter_sets(avcc: &[u8], known: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len());
    let mut p = 0;
    while p + 4 <= avcc.len() {
        let n = u32::from_be_bytes(avcc[p..p + 4].try_into().unwrap()) as usize;
        let end = (p + 4).saturating_add(n).min(avcc.len());
        let nal = &avcc[p + 4..end];

        let known_set = !nal.is_empty() && is_hevc_parameter_set(nal) && known.iter().any(|k| k == nal);
        if !known_set {
            out.extend_from_slice(&avcc[p..end]);
        }
        p = end;
    }
    out.extend_from_slice(&avcc[p..]);
    out
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

pub const AAC_RATES: [u32; 13] =
    [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

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

/// The header of an MPEG audio frame (MP1, MP2, MP3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mpa {
    /// 1 for MPEG-1, 2 for MPEG-2 and MPEG-2.5.
    pub version: u8,
    pub layer: u8,
    pub rate: u32,
    pub channels: u8,
    pub frame_len: usize,
    /// Samples each frame decodes to.
    pub samples: u32,
}

impl Mpa {
    pub fn parse(b: &[u8]) -> Option<Mpa> {
        if b.len() < 4 || b[0] != 0xff || b[1] & 0xe0 != 0xe0 {
            return None;
        }
        let ver = (b[1] >> 3) & 3; // 0: 2.5, 2: 2, 3: 1
        let layer = match (b[1] >> 1) & 3 {
            3 => 1,
            2 => 2,
            1 => 3,
            _ => return None,
        };
        let br_i = (b[2] >> 4) as usize;
        let sr_i = ((b[2] >> 2) & 3) as usize;
        if ver == 1 || br_i == 0 || br_i == 15 || sr_i == 3 {
            return None;
        }
        let mpeg1 = ver == 3;
        let kbps: [u32; 15] = match (mpeg1, layer) {
            (true, 1) => [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
            (true, 2) => [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
            (true, _) => [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
            (false, 1) => [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
            (false, _) => [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        };
        let rate = [44100, 48000, 32000][sr_i]
            >> match ver {
                3 => 0,
                2 => 1,
                _ => 2,
            };
        let bitrate = kbps[br_i] * 1000;
        let pad = ((b[2] >> 1) & 1) as u32;
        let (samples, frame_len) = match layer {
            1 => (384, (12 * bitrate / rate + pad) * 4),
            2 => (1152, 144 * bitrate / rate + pad),
            _ if mpeg1 => (1152, 144 * bitrate / rate + pad),
            _ => (576, 72 * bitrate / rate + pad),
        };
        if frame_len < 4 {
            return None;
        }
        Some(Mpa {
            version: if mpeg1 { 1 } else { 2 },
            layer,
            rate,
            channels: if b[3] >> 6 == 3 { 1 } else { 2 },
            frame_len: frame_len as usize,
            samples,
        })
    }

    /// Same stream parameters, so the frames can share one sample description.
    pub fn same_config(&self, o: &Mpa) -> bool {
        (self.version, self.layer, self.rate, self.channels) == (o.version, o.layer, o.rate, o.channels)
    }
}

/// An `stsd` holding one `mp4a` sample entry for MPEG audio: object type 0x6b for MPEG-1,
/// 0x69 for MPEG-2, and no decoder specific info.
pub fn mpa_stsd(m: &Mpa) -> Vec<u8> {
    mp4a_stsd(m.channels as u16, m.rate, 0, if m.version == 1 { 0x6b } else { 0x69 }, &[])
}

/// An `stsd` holding one `mp4a` sample entry for the AAC the ADTS headers describe.
pub fn aac_stsd(a: &Adts, es_id: u16) -> Vec<u8> {
    let config = [(a.object_type << 3) | (a.freq_index >> 1), ((a.freq_index & 1) << 7) | (a.channels << 3)];
    let channels = if a.channels == 0 { 2 } else { a.channels as u16 };
    mp4a_stsd(channels, a.rate(), es_id, 0x40, &config)
}

/// An `stsd` with one `mp4a` entry: an `esds` naming the object type, with the decoder
/// specific info when there is any (ISO 14496-1 7.2.6).
fn mp4a_stsd(channels: u16, rate: u32, es_id: u16, object_type: u8, info: &[u8]) -> Vec<u8> {
    let mut w = Wr::default();
    let stsd = w.open_full(b"stsd", 0, 0);
    w.u32(1);
    let entry = w.open(b"mp4a");
    w.zeros(6);
    w.u16(1); // data reference index
    w.zeros(8);
    w.u16(channels);
    w.u16(16); // sample size
    w.u32(0);
    w.u32(rate << 16);

    let info_len = if info.is_empty() { 0 } else { 2 + info.len() };
    let config_len = 13 + info_len;
    let es_len = 3 + 2 + config_len + 3;
    let esds = w.open_full(b"esds", 0, 0);
    w.bytes(&[0x03, es_len as u8]);
    w.u16(es_id);
    w.bytes(&[0, 0x04, config_len as u8, object_type, 0x15]);
    w.bytes(&[0, 0, 0]); // buffer size
    w.u32(0); // max bitrate: not known
    w.u32(0); // average bitrate: not known
    if !info.is_empty() {
        w.bytes(&[0x05, info.len() as u8]);
        w.bytes(info);
    }
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

pub fn nmhd() -> Vec<u8> {
    let mut w = Wr::default();
    let at = w.open_full(b"nmhd", 0, 0);
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

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn hevc_sps_of_three_encoders() {
        // x265: Main, 1280x720 coded and 718 shown, 4:3 pixels in the VUI.
        let open =
            hex("42010101600000030090000003000003005da00280802d1f596566924caf0e6808000003000800000300f040");
        let s = parse_hevc_sps(&open).unwrap();
        assert_eq!((s.width, s.height, s.sar, s.chroma_format, s.bit_depth_luma), (1280, 718, (4, 3), 1, 8));
        assert_eq!((s.ptl[0], s.ptl[11], s.sub_layers), (0x01, 93, 1));
        // x265: Main 4:2:2 10 (Rext), 640x360.
        let rext =
            hex("4201010408000003009d0800000300003fb00502016936595924932bc05a020000030002000003003210");
        let s = parse_hevc_sps(&rext).unwrap();
        assert_eq!((s.width, s.height, s.sar, s.chroma_format, s.bit_depth_luma), (640, 360, (1, 1), 2, 10));
        // Apple's bipbop: Main 10, 1920x1080, level 4.1.
        let apple = hex(
            "420101022000000300b0000003000003007ba003c08010e4d9c62e4912014e22224b3c929a49cb2449248b7246a923f2888ffc00040005a808080804",
        );
        let s = parse_hevc_sps(&apple).unwrap();
        assert_eq!((s.width, s.height, s.chroma_format, s.bit_depth_luma), (1920, 1080, 1, 10));
        assert_eq!((s.ptl[0], s.ptl[11]), (0x02, 123));
        // x265 with three temporal sub-layers before the VUI that holds 16:11 pixels.
        let layers = hex(
            "42010401600000030090000003000003005a0000a005820191f7965652b295964932bc11a020000003002000000303c1",
        );
        let s = parse_hevc_sps(&layers).unwrap();
        assert_eq!((s.width, s.height, s.sar, s.sub_layers), (704, 396, (16, 11), 3));
        // Cut short anywhere, it gives a size or nothing, and never panics.
        for n in 0..apple.len() {
            let _ = parse_hevc_sps(&apple[..n]);
        }
    }

    #[test]
    fn mp3_frame() {
        // MPEG-1 Layer III, 320 kbps, 44.1 kHz, no padding: 1044 bytes, 1152 samples.
        let m = Mpa::parse(&[0xff, 0xfb, 0xe0, 0x44]).unwrap();
        assert_eq!(
            (m.version, m.layer, m.rate, m.frame_len, m.samples, m.channels),
            (1, 3, 44100, 1044, 1152, 2)
        );
    }

    #[test]
    fn sps_1080p() {
        // The SPS of the Mux test stream: High profile, 1088 coded lines cropped to 1080.
        let sps = [
            0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x80, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03,
            0x00, 0x04, 0x00, 0x00, 0x03, 0x01, 0xe0, 0x3c, 0x60, 0xc6, 0x68,
        ];
        let s = parse_sps(&sps).unwrap();
        assert_eq!((s.width, s.height, s.sar, s.chroma_format), (1920, 1080, (1, 1), 1));
    }
}
