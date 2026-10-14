//! unmoof rebuilds fragmented MP4 (the fMP4 of HLS and DASH) as a progressive MP4 with the
//! `moov` first, the layout Telegram, Android and every other player can seek in.
//!
//! Sample data is never copied or changed. The caller hands over the `moov` and `moof` boxes
//! of its files, and gets back a [`Plan`]: a header (`ftyp`, `moov`, `mdat` header) and the
//! byte ranges of the inputs that follow it. Writing the header and then the ranges, in order,
//! gives the file. In a browser the ranges become `Blob.slice` calls on files that never have
//! to be read into memory.

mod bytes;
mod codec;
mod demux;
mod mux;
mod ts;
mod tsfeed;

pub use mux::{Piece, Plan, Range, TrackReport};

/// What a file holds, judged from its first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// ISO BMFF boxes: fMP4 init or media segments, or a whole fMP4 file.
    Fmp4,
    /// MPEG-TS packets, possibly behind junk such as a fake image header.
    Ts,
}

/// Tells fMP4 from MPEG-TS. `head` should be the first few kilobytes of the file, or all of it.
pub fn sniff(head: &[u8]) -> Option<Format> {
    const BOXES: [&[u8; 4]; 11] =
        [b"ftyp", b"styp", b"moov", b"moof", b"sidx", b"free", b"skip", b"emsg", b"prft", b"mdat", b"uuid"];
    if head.len() >= 8 && BOXES.iter().any(|t| head[4..8] == t[..]) {
        Some(Format::Fmp4)
    } else if ts::sniff(head) {
        Some(Format::Ts)
    } else {
        None
    }
}

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! bail {
    ($($t:tt)*) => { return Err($crate::Error(format!($($t)*))) };
}
pub(crate) use bail;

/// Collects the tracks and samples of one or more sources, then lays out the progressive file.
///
/// A source is one fMP4 stream: its init segment followed by its media segments, in any number
/// of files. A stream whose audio is delivered separately, as DASH and some HLS do, is two
/// sources. Their track IDs may clash; the output numbers its tracks itself.
#[derive(Default)]
pub struct Remuxer {
    sources: Vec<demux::Source>,
    tracks: Vec<demux::Track>,
    warnings: Vec<String>,
    chunk_secs: Option<f64>,
}

impl Remuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how much media time a chunk may span, which is how finely the tracks interleave.
    pub fn set_chunk_duration(&mut self, secs: f64) {
        self.chunk_secs = Some(secs);
    }

    /// Opens a new source and returns its index.
    pub fn add_source(&mut self) -> usize {
        self.sources.push(demux::Source::default());
        self.sources.len() - 1
    }

    /// Reads a whole MPEG-TS file, input file `file`. Its samples are cut out of it again when
    /// the output is written, through [`Plan::render`].
    pub fn feed_ts(&mut self, source: usize, file: u32, data: &[u8]) -> Result<()> {
        if source >= self.sources.len() {
            bail!("source {source} was never opened");
        }
        if self.sources[source].moov.is_some() {
            bail!("source {source} mixes fMP4 and MPEG-TS files");
        }
        self.read_ts(source, file, data)
    }

    /// Reads the boxes in `data`, which starts at byte `offset` of input file `file`.
    ///
    /// `data` holds whole top-level boxes. Only `moov` and `moof` are read, so a caller may hand
    /// those over alone and skip the `mdat` bodies. Fragments must come in decode order.
    pub fn feed(&mut self, source: usize, file: u32, offset: u64, data: &[u8]) -> Result<()> {
        if source >= self.sources.len() {
            bail!("source {source} was never opened");
        }
        for b in bytes::boxes(data) {
            let b = b?;
            let at = offset + b.start as u64;
            match &b.typ {
                b"moov" => self.read_moov(source, b.payload, b.raw)?,
                b"moof" => self.read_moof(source, file, at, b.payload)?,
                _ => {}
            }
        }
        Ok(())
    }
}
