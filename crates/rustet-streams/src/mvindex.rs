//! The fragment index of a growing music-video fMP4 file.
//!
//! While a producer writes the stream, [`MvLiveIndex::feed`] parses the top-level boxes inline and records each
//! fragment's start time and byte offset, so a seek can serve the init segment plus the fragment that covers a
//! requested time.
//!
//! - Inline and synchronous: pure byte arithmetic over data already in hand, so it can never stall the writer.
//! - Best effort: on any anomaly it disables itself, and seeks fall back to the slower path.
//! - Bounded memory: only header boxes (moov, moof: kilobytes) are buffered; mdat and everything else is skipped by
//!   offset arithmetic.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard};

use rustet_mp4::{FourCC, Mp4Box};
use serde::Serialize;
use tracing::debug;

/// No top-level box is larger than this.
const MAX_BOX_SIZE: u64 = 512 << 20;
/// A moov or moof larger than this is not a real header box (a misread size).
const MAX_COLLECT_SIZE: usize = 8 << 20;
const MAX_FRAGMENTS: usize = 1 << 16;
/// Per-index limit of diagnostic log lines.
const MAX_DIAGNOSTICS: u32 = 8;

/// One indexed fragment.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MvFragEntry {
    /// Start time in seconds, 0-based (the first fragment is at 0).
    pub t: f64,
    /// Byte offset of the moof.
    pub off: i64,
    /// Offset one past the fragment (the next moof's offset), so `[off, end)` is the complete moof + mdat. 0 while
    /// the fragment is still open.
    pub end: i64,
    /// Where the first mdat after this moof ends. Known as soon as that mdat's header is parsed, before the next
    /// moof arrives; for a single-track remux it equals `end`. Live serving uses it as an exact limit while `end`
    /// is still unknown.
    pub mdat_end: i64,
}

/// A fragment's start time and position in the index, as the renderer receives it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct VsegFragTiming {
    pub t: f64,
    pub n: usize,
}

/// What readers on other tasks see.
#[derive(Debug, Default)]
struct Shared {
    timescale: u64,
    init_size: Option<i64>,
    frags: Vec<MvFragEntry>,
}

/// The box parser. Only the feeding task uses it.
#[derive(Debug)]
struct Parser {
    disabled: bool,
    /// Bytes fed so far.
    pos: i64,
    /// Offset where the current box began.
    box_start: i64,
    /// The box header collected so far: 8 bytes, or 16 with a 64-bit size.
    hdr: Vec<u8>,
    hdr_need: usize,
    box_kind: FourCC,
    /// Body bytes left in the current box.
    remaining: i64,
    /// The whole current box (header and body), when it is a moov or moof.
    body: Option<Vec<u8>>,
    timescale: u64,
    /// The track times are taken from: Apple's raw input carries audio, video and subtitles.
    video_track_id: u32,
    /// Apple's tfdts are absolute media time (the first fragment is at about 10 s). Times are made 0-based so they
    /// run on the same clock as the player.
    base_time: Option<f64>,
    diagnostics: u32,
}

/// A fragment index fed with the bytes of a growing fMP4 file, queried concurrently by seeks.
#[derive(Debug)]
pub struct MvLiveIndex {
    parser: Mutex<Parser>,
    shared: RwLock<Shared>,
    /// The parser's position, readable without the lock.
    parsed: AtomicI64,
}

impl Default for MvLiveIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl MvLiveIndex {
    pub fn new() -> Self {
        Self {
            parser: Mutex::new(Parser {
                disabled: false,
                pos: 0,
                box_start: 0,
                hdr: Vec::with_capacity(16),
                hdr_need: 8,
                box_kind: FourCC(*b"\0\0\0\0"),
                remaining: 0,
                body: None,
                timescale: 0,
                video_track_id: 0,
                base_time: None,
                diagnostics: 0,
            }),
            shared: RwLock::new(Shared::default()),
            parsed: AtomicI64::new(0),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Shared> {
        self.shared.read().unwrap_or_else(|e| e.into_inner())
    }

    fn parser(&self) -> MutexGuard<'_, Parser> {
        self.parser.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Advances the index over `p`, the next bytes of the file. Never fails and never blocks on readers for long;
    /// on malformed input the index stops growing.
    pub fn feed(&self, p: &[u8]) {
        let mut parser = self.parser();
        parser.feed(p, &self.shared);
        self.parsed.store(parser.pos, Ordering::Release);
    }

    /// The latest fragment that starts at or before `t`. Before serving it from the growing file, check that it is
    /// complete and written (`end > 0 && end <= written`).
    pub fn lookup(&self, t: f64) -> Option<MvFragEntry> {
        let s = self.read();
        let i = s.frags.partition_point(|f| f.t <= t);
        i.checked_sub(1).map(|i| s.frags[i])
    }

    /// The latest fragment at or before `t` that is also complete and written (`end > 0 && end <= written`). When
    /// the fragment covering `t` is not complete yet, an earlier complete one is returned, so its bytes are already
    /// on disk and the first chunk of a seek is served at once.
    pub fn lookup_complete(&self, t: f64, written: i64) -> Option<MvFragEntry> {
        self.frag_index_for_time(t, written).map(|(_, f)| f)
    }

    /// [`lookup_complete`](Self::lookup_complete) with the fragment's position in the index.
    pub fn frag_index_for_time(&self, t: f64, written: i64) -> Option<(usize, MvFragEntry)> {
        let s = self.read();
        s.frags
            .iter()
            .enumerate()
            .take_while(|(_, f)| f.t <= t)
            .filter(|(_, f)| f.end > 0 && f.end <= written)
            .last()
            .map(|(i, f)| (i, *f))
    }

    /// The init segment's size (ftyp + moov), once the first moof has been seen.
    pub fn init_size(&self) -> Option<i64> {
        self.read().init_size
    }

    /// Fragment `n` if it is complete and written (`end > 0 && end <= written`).
    pub fn frag_by_index(&self, n: usize, written: i64) -> Option<MvFragEntry> {
        self.read()
            .frags
            .get(n)
            .copied()
            .filter(|f| f.end > 0 && f.end <= written)
    }

    /// The moof offset of fragment `n` as soon as it is indexed, even while its end is unknown. Lets a response
    /// for the fragment start before the next moof is parsed.
    pub fn frag_off_by_index(&self, n: usize) -> Option<i64> {
        self.read().frags.get(n).map(|f| f.off)
    }

    /// The end of fragment `n` once known: when the next moof is parsed, or [`finalize`](Self::finalize) ran.
    pub fn frag_end_by_index(&self, n: usize) -> Option<i64> {
        self.read().frags.get(n).map(|f| f.end).filter(|&e| e > 0)
    }

    /// The exact end of fragment `n` when it can be known without waiting for the next moof: its end if set, else
    /// the end of its mdat once that header is parsed. `None` means neither is known yet, and readers must not
    /// stream past [`parsed_bytes`](Self::parsed_bytes).
    pub fn frag_limit_by_index(&self, n: usize) -> Option<i64> {
        let f = *self.read().frags.get(n)?;
        [f.end, f.mdat_end].into_iter().find(|&e| e > 0)
    }

    /// How many bytes the index has examined. Bytes past it may hold box headers (a following moof) the index does
    /// not know yet, so live readers must not treat them as part of an open fragment.
    pub fn parsed_bytes(&self) -> i64 {
        self.parsed.load(Ordering::Acquire)
    }

    /// The number of indexed fragments (grows while the producer runs).
    pub fn frag_count(&self) -> usize {
        self.read().frags.len()
    }

    /// The video timescale from the moov, 0 until parsed.
    pub fn timescale(&self) -> u64 {
        self.read().timescale
    }

    /// Closes the last open fragment at `total_written`. Call once the producer has written everything.
    pub fn finalize(&self, total_written: i64) {
        let mut s = self.shared.write().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = s.frags.last_mut()
            && last.end == 0
        {
            last.end = total_written;
        }
    }

    /// Every fragment's start time and position.
    pub fn all_frag_timings(&self) -> Vec<VsegFragTiming> {
        self.read()
            .frags
            .iter()
            .enumerate()
            .map(|(n, f)| VsegFragTiming { t: f.t, n })
            .collect()
    }
}

impl Parser {
    fn feed(&mut self, mut p: &[u8], shared: &RwLock<Shared>) {
        while !self.disabled && !p.is_empty() {
            if self.remaining == 0 {
                // Reading a box header.
                if self.hdr.is_empty() {
                    self.box_start = self.pos;
                }
                let take = (self.hdr_need - self.hdr.len()).min(p.len());
                self.hdr.extend_from_slice(&p[..take]);
                self.pos += take as i64;
                p = &p[take..];
                if self.hdr.len() < self.hdr_need {
                    return;
                }
                let size32 =
                    u32::from_be_bytes([self.hdr[0], self.hdr[1], self.hdr[2], self.hdr[3]]);
                let size = if self.hdr_need == 8 {
                    if size32 == 1 {
                        self.hdr_need = 16; // a 64-bit largesize follows
                        continue;
                    }
                    u64::from(size32)
                } else {
                    let mut b = [0; 8];
                    b.copy_from_slice(&self.hdr[8..16]);
                    u64::from_be_bytes(b)
                };
                let kind = FourCC([self.hdr[4], self.hdr[5], self.hdr[6], self.hdr[7]]);
                if !self.start_box(size, kind, shared) {
                    return;
                }
                if self.remaining == 0 {
                    // A header-only box (an empty `free`): complete it now, or the loop would re-read it forever.
                    self.end_box(shared);
                }
                continue;
            }

            // Consuming a box body.
            let take = self.remaining.min(p.len() as i64) as usize;
            if let Some(body) = &mut self.body {
                body.extend_from_slice(&p[..take]);
                if body.len() > MAX_COLLECT_SIZE {
                    debug!(kind = %self.box_kind, limit = MAX_COLLECT_SIZE, "mv index disabled: header box too large (misread size?)");
                    self.disabled = true;
                    return;
                }
            }
            self.remaining -= take as i64;
            self.pos += take as i64;
            p = &p[take..];
            if self.remaining == 0 {
                self.end_box(shared);
            }
        }
    }

    /// Called once a box header is complete. Returns false when the index disabled itself.
    fn start_box(&mut self, size: u64, kind: FourCC, shared: &RwLock<Shared>) -> bool {
        // Size 0 means "to end of file" (a trailing mdat): nothing more to index.
        let hdr_len = self.hdr.len() as u64;
        if size == 0 || size < hdr_len || size > MAX_BOX_SIZE {
            debug!(%kind, size, off = self.box_start, "mv index disabled: bad box size");
            self.disabled = true;
            return false;
        }
        self.box_kind = kind;
        self.remaining = (size - hdr_len) as i64;
        let box_end = self.box_start + size as i64;
        let is_header = kind == FourCC::MOOV || kind == FourCC::MOOF;
        self.body = is_header.then(|| self.hdr.clone());
        if kind == FourCC::MDAT || kind == FourCC::MOOF {
            let mut s = write(shared);
            if kind == FourCC::MDAT {
                if let Some(last) = s.frags.last_mut()
                    && last.end == 0
                    && last.mdat_end == 0
                {
                    last.mdat_end = box_end;
                }
            } else if s.init_size.is_none() {
                // The first moof marks the end of the init segment.
                s.init_size = Some(self.box_start);
            }
        }
        true
    }

    /// Finishes the current box and resets for the next header.
    fn end_box(&mut self, shared: &RwLock<Shared>) {
        if let Some(body) = self.body.take() {
            self.index_box(&body, shared);
        }
        self.hdr.clear();
        self.hdr_need = 8;
    }

    fn diagnose(&mut self, what: std::fmt::Arguments<'_>) {
        if self.diagnostics < MAX_DIAGNOSTICS {
            self.diagnostics += 1;
            debug!("mv index: {what}");
        }
    }

    /// Indexes a complete moov or moof. A box that fails to decode is skipped.
    fn index_box(&mut self, body: &[u8], shared: &RwLock<Shared>) {
        let b = match Mp4Box::decode(body, 0) {
            Ok((b, _)) => b,
            Err(e) => {
                let kind = self.box_kind;
                self.diagnose(format_args!(
                    "decode {kind} ({} bytes) failed: {e}",
                    body.len()
                ));
                return;
            }
        };
        match b {
            Mp4Box::Moov(moov) => {
                // Times come from the video track: the first trak of Apple's raw input is often audio, with another
                // timescale. A single-track (or handler-less) input falls back to the first track with a timescale.
                let timescale = |t: &rustet_mp4::Trak| t.mdhd().map_or(0, |m| m.timescale());
                let video = moov
                    .traks()
                    .find(|t| t.handler_type() == Some(FourCC::VIDE) && timescale(t) > 0)
                    .or_else(|| moov.traks().find(|t| timescale(t) > 0));
                if let Some(trak) = video {
                    self.timescale = u64::from(timescale(trak));
                    self.video_track_id = trak.track_id().unwrap_or(0);
                    write(shared).timescale = self.timescale;
                }
            }
            Mp4Box::Moof(moof) => {
                if self.timescale == 0 {
                    self.diagnose(format_args!(
                        "moof skipped: no timescale (moov not indexed)"
                    ));
                    return;
                }
                let traf = moof
                    .trafs()
                    .find(|t| {
                        self.video_track_id != 0
                            && t.tfhd().is_some_and(|h| h.track_id == self.video_track_id)
                    })
                    .or_else(|| moof.traf());
                let Some(tfdt) = traf.and_then(|t| t.tfdt()) else {
                    self.diagnose(format_args!("moof skipped: no traf with a tfdt"));
                    return;
                };
                let t = tfdt.base_media_decode_time() as f64 / self.timescale as f64;
                let mut s = write(shared);
                if s.frags.len() >= MAX_FRAGMENTS {
                    self.disabled = true;
                    return;
                }
                let t = t - *self.base_time.get_or_insert(t);
                // This moof begins where the previous fragment ends.
                if let Some(last) = s.frags.last_mut() {
                    last.end = self.box_start;
                }
                s.frags.push(MvFragEntry {
                    t,
                    off: self.box_start,
                    ..MvFragEntry::default()
                });
            }
            _ => {}
        }
    }
}

fn write(shared: &RwLock<Shared>) -> std::sync::RwLockWriteGuard<'_, Shared> {
    shared.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustet_mp4::{Fragment, InitSegment, MediaType, TrunSample};

    /// A remux-shaped fMP4 (ftyp + moov, then moof + mdat per fragment), with its init size and the moof offsets.
    fn mv_stream(timescale: u32, dec_times: &[u64]) -> (Vec<u8>, i64, Vec<i64>) {
        let mut out = InitSegment::single_track(timescale, MediaType::Video)
            .unwrap()
            .to_bytes()
            .unwrap();
        let init_size = out.len() as i64;
        let mut offsets = Vec::new();
        for (i, &t) in dec_times.iter().enumerate() {
            let mut frag = Fragment::new(i as u32 + 1, 1);
            let sample = TrunSample {
                duration: 1000,
                size: 6,
                ..TrunSample::default()
            };
            frag.add_full_sample(sample, t, &[0, 1, 2, 3, 4, 5]);
            offsets.push(out.len() as i64);
            frag.encode_into(&mut out).unwrap();
        }
        (out, init_size, offsets)
    }

    fn fed(stream: &[u8]) -> MvLiveIndex {
        let ix = MvLiveIndex::new();
        ix.feed(stream);
        ix
    }

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(payload);
        b
    }

    #[test]
    fn correct_and_chunk_invariant() {
        let dec_times = [0, 1000, 2000, 3000, 4000];
        let (stream, init_size, offsets) = mv_stream(1000, &dec_times);
        for chunk in [1, 2, 3, 7, 64, 1024, stream.len()] {
            let ix = MvLiveIndex::new();
            stream.chunks(chunk).for_each(|c| ix.feed(c));
            assert!(
                !ix.parser().disabled,
                "chunk={chunk}: disabled on valid input"
            );
            assert_eq!(ix.timescale(), 1000, "chunk={chunk}");
            assert_eq!(ix.init_size(), Some(init_size), "chunk={chunk}");
            let frags = ix.read().frags.clone();
            assert_eq!(frags.len(), dec_times.len(), "chunk={chunk}");
            for (i, f) in frags.iter().enumerate() {
                assert_eq!(f.off, offsets[i], "chunk={chunk} frag {i}");
                assert_eq!(f.t, dec_times[i] as f64 / 1000.0, "chunk={chunk} frag {i}");
            }
        }
    }

    #[test]
    fn lookup_and_end() {
        let dec_times = [0, 2000, 4000, 6000];
        let (stream, init_size, offsets) = mv_stream(1000, &dec_times);
        let m = fed(&stream);
        assert_eq!(m.init_size(), Some(init_size));

        // Each fragment ends at the next moof; the last is unbounded.
        for i in 0..dec_times.len() {
            let f = m.lookup(dec_times[i] as f64 / 1000.0).unwrap();
            assert_eq!(f.off, offsets[i]);
            assert_eq!(f.end, offsets.get(i + 1).copied().unwrap_or(0));
        }
        for (t, want) in [
            (-1.0, None),
            (0.0, Some(0)),
            (1.5, Some(0)),
            (2.0, Some(1)),
            (3.9, Some(1)),
            (4.0, Some(2)),
            (6.0, Some(3)),
            (99.0, Some(3)),
        ] {
            assert_eq!(
                m.lookup(t).map(|f| f.off),
                want.map(|i: usize| offsets[i]),
                "lookup({t})"
            );
        }
    }

    #[test]
    fn lookup_complete_falls_back_to_written_fragments() {
        let (stream, _, offsets) = mv_stream(1000, &[0, 2000, 4000, 6000]);
        let m = fed(&stream);
        let all = stream.len() as i64;
        assert_eq!(m.lookup_complete(5.0, all).map(|f| f.off), Some(offsets[2]));
        // Fragment 2 is not fully written: the latest complete one before the target is fragment 1.
        assert_eq!(
            m.lookup_complete(5.0, offsets[2]).map(|f| f.off),
            Some(offsets[1])
        );
        assert_eq!(
            m.lookup_complete(5.0, offsets[2] - 1).map(|f| f.off),
            Some(offsets[0])
        );
        assert_eq!(m.lookup_complete(-1.0, all), None);
    }

    #[test]
    fn frag_by_index_needs_a_written_end() {
        let (stream, init_size, offsets) = mv_stream(1000, &[0, 1000, 2000, 3000]);
        let total = stream.len() as i64;
        let m = fed(&stream);

        let last = m.frag_count() - 1;
        assert_eq!(
            m.frag_by_index(last, total),
            None,
            "open last fragment reported ready"
        );
        m.finalize(total);
        assert_eq!(m.frag_by_index(last, total).map(|f| f.end), Some(total));
        assert_eq!(m.frag_by_index(last, total - 1), None);

        let f0 = m.frag_by_index(0, total).unwrap();
        assert_eq!(f0.off, offsets[0]);
        assert!(f0.off >= init_size);
        let f1 = m.frag_by_index(1, total).unwrap();
        assert_eq!(m.frag_by_index(1, f1.end - 1), None);
        for bad in [m.frag_count(), m.frag_count() + 100] {
            assert_eq!(m.frag_by_index(bad, total), None);
        }
    }

    #[test]
    fn init_size_appears_with_the_first_moof() {
        let (stream, init_size, _) = mv_stream(1000, &[0, 1000]);
        let m = MvLiveIndex::new();
        assert_eq!(m.init_size(), None);
        m.feed(&stream);
        assert_eq!(m.init_size(), Some(init_size));
    }

    /// The handler's "base is past the target" guard: a later fragment exists only for a backward seek.
    #[test]
    fn frag_index_for_time_forward_and_backward() {
        let times: Vec<u64> = (0..10).map(|i| i * 10_000).collect();
        let (stream, _, _) = mv_stream(1000, &times[..5]);
        let written = stream.len() as i64;
        let m = fed(&stream);
        m.finalize(written);
        let (n, f) = m.frag_index_for_time(164.0, written).unwrap();
        assert!(f.t < 164.0);
        assert!(
            m.frag_count() <= n + 1,
            "forward seek must not look served from base"
        );

        let (stream, _, _) = mv_stream(1000, &times);
        let written = stream.len() as i64;
        let m = fed(&stream);
        m.finalize(written);
        let (n, f) = m.frag_index_for_time(30.0, written).unwrap();
        assert!(f.t <= 30.0);
        assert!(
            m.frag_count() > n + 1,
            "backward seek must find a later fragment"
        );

        let (n, _) = m.frag_index_for_time(90.0, written).unwrap();
        assert_eq!(m.frag_count(), n + 1, "the last fragment has no successor");
    }

    /// A live reader can bound an open fragment from its mdat header before the next moof arrives, and the parsed
    /// frontier never runs ahead of the bytes fed.
    #[test]
    fn frag_limit_from_mdat_before_next_moof() {
        let (stream, _, offsets) = mv_stream(1000, &[0, 1000, 2000]);
        let m = MvLiveIndex::new();
        let cut = offsets[1] as usize;
        m.feed(&stream[..cut]);
        assert_eq!(
            m.frag_end_by_index(0),
            None,
            "end known before the next moof"
        );
        assert_eq!(m.frag_limit_by_index(0), Some(offsets[1]));
        assert_eq!(m.parsed_bytes(), cut as i64);

        m.feed(&stream[cut..cut + 5]);
        assert_eq!(m.frag_limit_by_index(1), None);
        assert_eq!(m.parsed_bytes(), cut as i64 + 5);

        m.feed(&stream[cut + 5..]);
        m.finalize(stream.len() as i64);
        for (n, want) in [offsets[1], offsets[2], stream.len() as i64]
            .into_iter()
            .enumerate()
        {
            assert_eq!(m.frag_limit_by_index(n), Some(want), "frag {n}");
        }
        assert_eq!(m.frag_off_by_index(2), Some(offsets[2]));
        assert_eq!(m.frag_off_by_index(3), None);
    }

    #[test]
    fn header_only_box_terminates() {
        let m = MvLiveIndex::new();
        m.feed(&mp4_box(b"free", &[]));
        let mut rest = mp4_box(b"moov", &[]);
        rest.extend(mp4_box(b"moof", &[]));
        m.feed(&rest);
        assert_eq!(m.parsed_bytes(), 24);
    }

    /// Port of the Go fuzz target: arbitrary bytes, split anywhere, never panic in any query.
    #[test]
    fn arbitrary_input_never_panics() {
        let mut seed = mp4_box(b"ftyp", b"isom\x00\x00\x02\x00");
        seed.extend(mp4_box(b"moov", &[]));
        seed.extend(mp4_box(b"moof", &mp4_box(b"traf", &[])));
        seed.extend(mp4_box(b"mdat", &[0; 32]));
        let largesize = [
            0, 0, 0, 1, b'm', b'o', b'o', b'f', 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        let (valid, _, _) = mv_stream(1000, &[0, 1000, 2000]);

        // A deterministic xorshift corpus of mutations of the seeds.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut corpus = vec![seed.clone(), largesize.to_vec(), valid.clone()];
        for _ in 0..400 {
            let mut d = match next() % 3 {
                0 => seed.clone(),
                1 => valid.clone(),
                _ => (0..next() % 200).map(|_| next() as u8).collect(),
            };
            for _ in 0..(next() % 8) {
                if !d.is_empty() {
                    let i = next() as usize % d.len();
                    d[i] = next() as u8;
                }
            }
            if next() % 4 == 0 && !d.is_empty() {
                d.truncate(next() as usize % d.len());
            }
            corpus.push(d);
        }
        for (i, data) in corpus.iter().enumerate() {
            let ix = MvLiveIndex::new();
            let step = (i % 64) + 1;
            data.chunks(step).for_each(|c| ix.feed(c));
            let w = data.len() as i64;
            ix.init_size();
            ix.lookup(1.5);
            ix.lookup_complete(1.5, w);
            ix.frag_by_index(0, w);
            ix.frag_off_by_index(0);
            ix.frag_end_by_index(0);
            ix.frag_limit_by_index(0);
            ix.frag_index_for_time(1.5, w);
            ix.all_frag_timings();
            ix.finalize(w);
            ix.frag_count();
            ix.timescale();
            assert!(ix.parsed_bytes() <= w);
        }
    }
}
