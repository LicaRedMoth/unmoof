//! The WebAssembly face of unmoof: plain exported functions over handles and byte buffers, so
//! the module needs no binding generator. `unmoof.js` wraps them.
//!
//! Calls that can fail return 0 on success and 1 on failure; the message of a failure, like the
//! bytes a call makes, waits in the handle's buffer (`*_buf_ptr`, `*_buf_len`) until the next call.

use std::fmt::Write as _;

use unmoof::{Format, Piece, Plan, Remuxer};

pub struct RState {
    rm: Option<Remuxer>,
    buf: Vec<u8>,
}

pub struct PState {
    plan: Plan,
    buf: Vec<u8>,
}

/// Memory for the caller to write into; give it back with `unmoof_dealloc`.
#[unsafe(no_mangle)]
pub extern "C" fn unmoof_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr` and `len` must come from one `unmoof_alloc` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(ptr, len) } }
}

/// 1 for fMP4, 2 for MPEG-TS, 3 for HLS packed audio, 4 for subtitles, 0 for none of them.
///
/// # Safety
/// `ptr` must point at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_sniff(ptr: *const u8, len: usize) -> u32 {
    match unmoof::sniff(unsafe { bytes(ptr, len) }) {
        Some(Format::Fmp4) => 1,
        Some(Format::Ts) => 2,
        Some(Format::PackedAudio) => 3,
        Some(Format::Subtitles) => 4,
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn unmoof_new() -> *mut RState {
    Box::into_raw(Box::new(RState { rm: Some(Remuxer::new()), buf: Vec::new() }))
}

/// # Safety
/// `h` must come from `unmoof_new` and not be used after.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_drop(h: *mut RState) {
    drop(unsafe { Box::from_raw(h) });
}

/// # Safety
/// `h` must come from `unmoof_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_buf_ptr(h: *mut RState) -> *const u8 {
    unsafe { (*h).buf.as_ptr() }
}

/// # Safety
/// `h` must come from `unmoof_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_buf_len(h: *mut RState) -> usize {
    unsafe { (*h).buf.len() }
}

fn fail(buf: &mut Vec<u8>, e: impl std::fmt::Display) -> u32 {
    buf.clear();
    let _ = write!(Text(buf), "{e}");
    1
}

struct Text<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Text<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// # Safety
/// `h` must come from `unmoof_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_set_chunk_ms(h: *mut RState, ms: f64) {
    if let Some(rm) = unsafe { (*h).rm.as_mut() } {
        rm.set_chunk_duration(ms / 1000.0);
    }
}

/// Opens a source and returns its index.
///
/// # Safety
/// `h` must come from `unmoof_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_add_source(h: *mut RState) -> u32 {
    unsafe { (*h).rm.as_mut().map_or(u32::MAX, |rm| rm.add_source() as u32) }
}

/// Names the language (ISO 639-2) and title of a source's tracks; an empty string leaves one as is.
///
/// # Safety
/// `h` must come from `unmoof_new`; the pointers must point at the UTF-8 bytes given.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_set_track_info(
    h: *mut RState,
    source: u32,
    lang: *const u8,
    lang_len: usize,
    title: *const u8,
    title_len: usize,
) -> u32 {
    let st = unsafe { &mut *h };
    let Some(rm) = st.rm.as_mut() else { return fail(&mut st.buf, "the remuxer has finished") };
    let text = |p, n| std::str::from_utf8(unsafe { bytes(p, n) }).ok().filter(|s| !s.is_empty());
    match rm.set_track_info(source as usize, text(lang, lang_len), text(title, title_len)) {
        Ok(()) => 0,
        Err(e) => fail(&mut st.buf, e),
    }
}

/// Tells the length of an input file, so samples past its end are refused while feeding.
///
/// # Safety
/// `h` must come from `unmoof_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_set_file_size(h: *mut RState, file: u32, len: f64) {
    if let Some(rm) = unsafe { (*h).rm.as_mut() } {
        rm.set_file_size(file, len as u64);
    }
}

/// Feeds whole top-level boxes of an fMP4 file; `offset` is where they start in the file.
///
/// # Safety
/// `h` must come from `unmoof_new`; `ptr` must point at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_feed(
    h: *mut RState,
    source: u32,
    file: u32,
    offset: f64,
    ptr: *const u8,
    len: usize,
) -> u32 {
    let st = unsafe { &mut *h };
    let Some(rm) = st.rm.as_mut() else { return fail(&mut st.buf, "the remuxer has finished") };
    match rm.feed(source as usize, file, offset as u64, unsafe { bytes(ptr, len) }) {
        Ok(()) => 0,
        Err(e) => fail(&mut st.buf, e),
    }
}

/// Feeds a whole MPEG-TS file or HLS packed audio segment.
///
/// # Safety
/// `h` must come from `unmoof_new`; `ptr` must point at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_feed_ts(
    h: *mut RState,
    source: u32,
    file: u32,
    ptr: *const u8,
    len: usize,
) -> u32 {
    let st = unsafe { &mut *h };
    let Some(rm) = st.rm.as_mut() else { return fail(&mut st.buf, "the remuxer has finished") };
    match rm.feed_ts(source as usize, file, unsafe { bytes(ptr, len) }) {
        Ok(()) => 0,
        Err(e) => fail(&mut st.buf, e),
    }
}

/// Feeds a SubRip, WebVTT or ASS/SSA file to a source's text track.
///
/// # Safety
/// `h` must come from `unmoof_new`; `ptr` must point at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_feed_subtitles(
    h: *mut RState,
    source: u32,
    ptr: *const u8,
    len: usize,
) -> u32 {
    let st = unsafe { &mut *h };
    let Some(rm) = st.rm.as_mut() else { return fail(&mut st.buf, "the remuxer has finished") };
    match rm.feed_subtitles(source as usize, unsafe { bytes(ptr, len) }) {
        Ok(()) => 0,
        Err(e) => fail(&mut st.buf, e),
    }
}

/// Lays the file out. Returns a plan handle, or null with the message in the remuxer's buffer.
/// The remuxer takes no more input afterwards.
///
/// # Safety
/// `h` must come from `unmoof_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_finish(h: *mut RState) -> *mut PState {
    let st = unsafe { &mut *h };
    let Some(rm) = st.rm.take() else {
        fail(&mut st.buf, "the remuxer has finished");
        return std::ptr::null_mut();
    };
    match rm.finish() {
        Ok(plan) => Box::into_raw(Box::new(PState { plan, buf: Vec::new() })),
        Err(e) => {
            fail(&mut st.buf, e);
            std::ptr::null_mut()
        }
    }
}

/// Like `unmoof_finish`, for a bare `.mp3` file: the frames of the one MP3 track, in order.
///
/// # Safety
/// `h` must come from `unmoof_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_finish_mp3(h: *mut RState) -> *mut PState {
    let st = unsafe { &mut *h };
    let Some(rm) = st.rm.take() else {
        fail(&mut st.buf, "the remuxer has finished");
        return std::ptr::null_mut();
    };
    match rm.finish_mp3() {
        Ok(plan) => Box::into_raw(Box::new(PState { plan, buf: Vec::new() })),
        Err(e) => {
            fail(&mut st.buf, e);
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `p` must come from `unmoof_finish` and not be used after.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_drop(p: *mut PState) {
    drop(unsafe { Box::from_raw(p) });
}

/// # Safety
/// `p` must come from `unmoof_finish`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_header_ptr(p: *mut PState) -> *const u8 {
    unsafe { (*p).plan.header.as_ptr() }
}

/// # Safety
/// `p` must come from `unmoof_finish`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_header_len(p: *mut PState) -> usize {
    unsafe { (*p).plan.header.len() }
}

/// # Safety
/// `p` must come from `unmoof_finish`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_buf_ptr(p: *mut PState) -> *const u8 {
    unsafe { (*p).buf.as_ptr() }
}

/// # Safety
/// `p` must come from `unmoof_finish`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_buf_len(p: *mut PState) -> usize {
    unsafe { (*p).buf.len() }
}

/// Puts the plan as JSON in the plan's buffer: size, pieces (`[0, file, offset, len]` to copy,
/// `[1, file, len]` to render, `[2, len]` made here, see `unmoof_plan_data`), tracks and warnings.
///
/// # Safety
/// `p` must come from `unmoof_finish`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_info(p: *mut PState) {
    let st = unsafe { &mut *p };
    let plan = &st.plan;
    let mut s = String::with_capacity(64 + plan.pieces.len() * 24);
    let _ = write!(s, "{{\"size\":{},\"pieces\":[", plan.size);
    for (i, piece) in plan.pieces.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = match piece {
            Piece::Copy(r) => write!(s, "[0,{},{},{}]", r.file, r.offset, r.len),
            Piece::Render { file, len, .. } => write!(s, "[1,{file},{len}]"),
            Piece::Data(d) => write!(s, "[2,{}]", d.len()),
        };
    }
    s.push_str("],\"tracks\":[");
    for (i, t) in plan.tracks.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"id\":{},\"handler\":{},\"codec\":{},\"samples\":{},\"duration\":{},\"start\":{},\"fixes\":{},\"snapped\":{}}}",
            t.id,
            json_str(&t.handler),
            json_str(&t.codec),
            t.samples,
            t.duration,
            t.start,
            t.fixes,
            t.snapped.map_or("null".to_string(), |v| v.to_string())
        );
    }
    s.push_str("],\"warnings\":[");
    for (i, w) in plan.warnings.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&json_str(w));
    }
    s.push_str("]}");
    st.buf = s.into_bytes();
}

fn json_str(v: &str) -> String {
    let mut s = String::with_capacity(v.len() + 2);
    s.push('"');
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Puts the bytes of a data piece (subtitle samples) in the plan's buffer. Returns 1 if the
/// piece is of another kind.
///
/// # Safety
/// `p` must come from `unmoof_finish`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_data(p: *mut PState, piece: u32) -> u32 {
    let st = unsafe { &mut *p };
    match st.plan.pieces.get(piece as usize) {
        Some(Piece::Data(d)) => {
            st.buf = d.clone();
            0
        }
        _ => fail(&mut st.buf, format!("piece {piece} is not a data piece")),
    }
}

/// Cuts the samples of a render piece out of the TS file it names; the bytes go to the plan's
/// buffer.
///
/// # Safety
/// `p` must come from `unmoof_finish`; `ptr` must point at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unmoof_plan_render(p: *mut PState, piece: u32, ptr: *const u8, len: usize) -> u32 {
    let st = unsafe { &mut *p };
    match st.plan.render(piece as usize, unsafe { bytes(ptr, len) }) {
        Ok(b) => {
            st.buf = b;
            0
        }
        Err(e) => fail(&mut st.buf, e),
    }
}

/// The calls in the order `unmoof.js` makes them, so Miri can check the pointer handling.
#[cfg(test)]
mod tests {
    use super::*;

    /// Copies `data` into module memory the way `unmoof.js` does, and runs `f` on it.
    fn with(data: &[u8], f: impl FnOnce(*const u8, usize) -> u32) -> u32 {
        let p = unmoof_alloc(data.len());
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len()) };
        let r = f(p, data.len());
        unsafe { unmoof_dealloc(p, data.len()) };
        r
    }

    fn buf(h: *mut RState) -> String {
        unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(unmoof_buf_ptr(h), unmoof_buf_len(h)))
                .into_owned()
        }
    }

    fn plan_buf(p: *mut PState) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(unmoof_plan_buf_ptr(p), unmoof_plan_buf_len(p)).to_vec() }
    }

    const SRT: &[u8] = "1\n00:00:00,500 --> 00:00:01,000\nПривет, <i>мир</i>\n".as_bytes();

    #[test]
    fn subtitles_round_trip() {
        unsafe {
            assert_eq!(with(SRT, |p, n| unmoof_sniff(p, n)), 4);
            let h = unmoof_new();
            let s = unmoof_add_source(h);
            assert_eq!(
                with(b"rus", |lp, ln| with("Название".as_bytes(), |tp, tn| unmoof_set_track_info(
                    h, s, lp, ln, tp, tn
                ))),
                0
            );
            assert_eq!(with(SRT, |p, n| unmoof_feed_subtitles(h, s, p, n)), 0);
            let p = unmoof_finish(h);
            assert!(!p.is_null(), "{}", buf(h));
            assert!(unmoof_plan_header_len(p) > 0);
            unmoof_plan_info(p);
            let info = String::from_utf8(plan_buf(p)).unwrap();
            assert!(info.starts_with("{\"size\":") && info.contains("[2,"), "{info}");
            assert_eq!(unmoof_plan_data(p, 0), 0);
            assert!(!plan_buf(p).is_empty());
            assert_eq!(unmoof_plan_data(p, 99), 1);
            assert!(String::from_utf8(plan_buf(p)).unwrap().contains("not a data piece"));
            unmoof_plan_drop(p);
            unmoof_drop(h);
        }
    }

    #[test]
    fn errors_come_back_as_text() {
        unsafe {
            let h = unmoof_new();
            assert_eq!(with(b"not a box", |p, n| unmoof_feed(h, 7, 0, 0.0, p, n)), 1);
            assert!(buf(h).contains("never opened"), "{}", buf(h));
            assert_eq!(with(b"xx", |lp, ln| unmoof_set_track_info(h, 0, lp, ln, std::ptr::null(), 0)), 1);
            assert!(unmoof_finish(h).is_null());
            assert!(unmoof_finish(h).is_null(), "a second finish must fail cleanly, not use freed state");
            assert!(buf(h).contains("finished"));
            unmoof_drop(h);
        }
    }

    #[test]
    fn empty_input_is_handled() {
        unsafe {
            assert_eq!(unmoof_sniff(std::ptr::null(), 0), 0);
            let h = unmoof_new();
            let s = unmoof_add_source(h);
            assert_eq!(unmoof_feed_ts(h, s, 0, std::ptr::null(), 0), 1);
            assert_eq!(unmoof_feed(h, s, 0, 0.0, std::ptr::null(), 0), 0);
            assert!(unmoof_finish_mp3(h).is_null());
            unmoof_drop(h);
        }
    }
}
