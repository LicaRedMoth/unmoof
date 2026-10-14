//! unmoof turns the segments of an HLS or DASH stream (fMP4, MPEG-TS, packed audio, subtitles)
//! into one MP4 with the `moov` first, the layout every player can seek and stream, or into a
//! bare `.mp3`.
//!
//! The result is a [`Plan`]: a header, then pieces that are byte ranges of the inputs, samples to
//! cut out of a TS file ([`Plan::render`]), or bytes made here. fMP4 samples are never copied
//! into memory, so in a browser most of the file is `Blob.slice` calls on files on disk.

mod bytes;
mod codec;
mod demux;
mod mux;
mod subs;
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
    /// HLS packed audio: AAC or MPEG audio frames, usually behind an ID3 tag.
    PackedAudio,
    /// SubRip, WebVTT or ASS/SSA subtitles.
    Subtitles,
}

/// Tells what kind of input a file is. `head` should be its first 64 KiB, or all of it.
pub fn sniff(head: &[u8]) -> Option<Format> {
    const BOXES: [&[u8; 4]; 11] =
        [b"ftyp", b"styp", b"moov", b"moof", b"sidx", b"free", b"skip", b"emsg", b"prft", b"mdat", b"uuid"];
    if head.len() >= 8 && BOXES.iter().any(|t| head[4..8] == t[..]) {
        Some(Format::Fmp4)
    } else if subs::sniff(head).is_some() {
        Some(Format::Subtitles)
    } else if ts::sniff(head) {
        Some(Format::Ts)
    } else if ts::sniff_packed(head) {
        Some(Format::PackedAudio)
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
/// A source is one stream in any number of files: an fMP4 init segment and its media segments,
/// TS segments, packed audio or subtitles. Audio delivered on its own, as in DASH, is another
/// source. Track IDs of sources may clash; the output numbers its tracks itself.
#[derive(Default)]
pub struct Remuxer {
    sources: Vec<demux::Source>,
    tracks: Vec<demux::Track>,
    warnings: Vec<String>,
    chunk_secs: Option<f64>,
    /// Lengths of input files, by file number, when the caller told them.
    file_sizes: Vec<Option<u64>>,
}

impl Remuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how much media time a chunk may span, which is how finely the tracks interleave.
    pub fn set_chunk_duration(&mut self, secs: f64) {
        self.chunk_secs = Some(secs);
    }

    /// Names the language (ISO 639-2: `rus`, `jpn`) and a title (the studio of a dub) of a
    /// source's tracks. Manifests carry these; segments do not.
    pub fn set_track_info(
        &mut self,
        source: usize,
        language: Option<&str>,
        title: Option<&str>,
    ) -> Result<()> {
        let Some(s) = self.sources.get_mut(source) else { bail!("source {source} was never opened") };
        if let Some(lang) = language {
            let b = lang.as_bytes();
            if b.len() != 3 || !b.iter().all(u8::is_ascii_lowercase) {
                bail!("language '{lang}' is not a three-letter ISO 639-2 code such as rus or jpn");
            }
            s.language = Some(b.iter().fold(0u16, |v, &c| v << 5 | (c - 0x60) as u16));
        }
        if let Some(t) = title {
            s.title = Some(t.to_string());
        }
        Ok(())
    }

    /// Tells the length of input file `file`. Samples that would lie past it are then refused
    /// while feeding, so a few damaged bytes cannot describe millions of samples.
    pub fn set_file_size(&mut self, file: u32, len: u64) {
        let i = file as usize;
        if self.file_sizes.len() <= i {
            self.file_sizes.resize(i + 1, None);
        }
        self.file_sizes[i] = Some(len);
    }

    /// Opens a new source and returns its index.
    pub fn add_source(&mut self) -> usize {
        self.sources.push(demux::Source::default());
        self.sources.len() - 1
    }

    /// Reads a whole MPEG-TS file or HLS packed audio segment, input file `file`. Its samples are
    /// cut out of it again when the output is written, through [`Plan::render`].
    pub fn feed_ts(&mut self, source: usize, file: u32, data: &[u8]) -> Result<()> {
        if source >= self.sources.len() {
            bail!("source {source} was never opened");
        }
        if self.sources[source].moov.is_some() {
            bail!("source {source} mixes fMP4 and MPEG-TS files");
        }
        self.read_ts(source, file, data)
    }

    /// Reads a SubRip, WebVTT or ASS/SSA file into the source's text track. Times count from the
    /// start of the movie, unless a WebVTT file carries the `X-TIMESTAMP-MAP` of HLS, which ties
    /// it to the clock of the stream. The files of segmented WebVTT join into one track.
    pub fn feed_subtitles(&mut self, source: usize, data: &[u8]) -> Result<()> {
        let Some(s) = self.sources.get_mut(source) else { bail!("source {source} was never opened") };
        if s.moov.is_some() || s.ts.is_some() {
            bail!("source {source} mixes subtitles with audio or video files");
        }
        let (kind, cues, tied) = subs::parse(data)?;
        let st = s.subs.get_or_insert(demux::SubSource { kind, cues: Vec::new(), tied });
        if st.tied != tied {
            self.warnings.push(format!(
                "source {source}: some WebVTT files are tied to the stream clock and some are not; timing may be off"
            ));
        }
        st.cues.extend(cues);
        Ok(())
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
