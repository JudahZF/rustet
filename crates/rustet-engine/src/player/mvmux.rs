//! Joins a music video's decrypted video and audio streams into one fragmented MP4 for libvlc
//! (spec §4.9): libvlc then decodes both from one input and keeps them in sync itself.
//!
//! The two inputs are the session's own fMP4 streams (`Manager::stream`, one track each). The
//! output is one init segment with both tracks (video track 1, audio track 2, the moov length set
//! so libvlc does not scan the file first), then one fragment per input video fragment: a moof
//! with a traf for the video samples and one for the audio samples that start before the video
//! fragment ends, and one mdat with both. That is the layout FFmpeg writes; libvlc's seek loads
//! one moof and needs every track in it to land on time. Audio left after the video ends gets
//! fragments of its own. Samples are copied as they are: nothing is re-encoded, no FFmpeg runs.
//!
//! Seeking: after the moov comes a `sidx` with one reference that spans any time. With it,
//! libvlc's seek (and its `start-time`) jumps to the first moof of the stream, decodes from there
//! and shows nothing before the target; without an index libvlc would first read the whole file
//! to build one, which blocks on a file that is still growing. So a stream is only ever sought
//! near its start: a seek elsewhere opens a view that starts at the fragment holding the target
//! ([`MuxIndex::offset_for`]), or a new stream from the segment holding it.
//!
//! Timelines: Apple's video fragments keep a raw decode time that sits at a constant offset from
//! the playlist time, while the audio path writes the playlist time (or its own raw one). Each
//! track is moved so its first sample starts at the playlist time its stream started at (0, or
//! the segment boundary of a seek), so both tracks and libvlc's position share the playlist
//! timeline.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use rustet_mp4::{
    Children, Container, FourCC, Fragment, InitSegment, Mdat, Mfhd, Moof, Mp4Box, Tfdt, Tfhd, Traf,
    Trak, Trex, Trun, TrunSample, read_box_async,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

/// Read-ahead of each input.
const READ_BUFFER: usize = 256 << 10;
/// Boxes before the moov an input may have (pssh and the like).
const MAX_INIT_BOXES: usize = 64;
/// Audio left after the video ends goes out in fragments of this many samples (AAC: ~2 s).
const AUDIO_TAIL_SAMPLES: usize = 96;
const NON_SYNC: u32 = 0x0001_0000;
const VIDEO_ID: u32 = 1;
const AUDIO_ID: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub(crate) enum MuxError {
    #[error("music-video mux: {0} stream: {1}")]
    Input(&'static str, rustet_mp4::Error),
    #[error("music-video mux: {0} stream has no moov")]
    NoMoov(&'static str),
    #[error("music-video mux: {0} init has no track")]
    NoTrack(&'static str),
    #[error("music-video mux: write: {0}")]
    Write(#[from] std::io::Error),
    #[error("music-video mux: encode: {0}")]
    Encode(rustet_mp4::Error),
}

/// Where each input starts on the playlist timeline, and the length for the moov.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MuxPlan {
    pub video_start_s: f64,
    pub audio_start_s: f64,
    pub duration_ms: u64,
}

/// A fragment in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Written {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Byte offset of its moof in the output.
    pub offset: u64,
    /// It starts with a video sync sample (a keyframe), or has no video.
    pub sync: bool,
}

/// The output's head and where its fragments are, for views that start mid-stream.
#[derive(Debug, Default)]
pub(crate) struct MuxIndex {
    /// The init segment and the `sidx`: the start of every view.
    pub head: Vec<u8>,
    pub frags: Vec<Written>,
}

impl MuxIndex {
    /// The output offset a view must start at to play from `at_ms`: the fragment holding it, or
    /// the last one before it that starts with a keyframe. `None` until it is written.
    pub fn offset_for(&self, at_ms: u64) -> Option<u64> {
        if !self
            .frags
            .iter()
            .any(|f| f.start_ms <= at_ms && at_ms < f.end_ms)
        {
            return None;
        }
        self.frags
            .iter()
            .filter(|f| f.sync && f.start_ms <= at_ms)
            .map(|f| f.offset)
            .next_back()
    }
}

/// A `sidx` with one reference spanning any time: libvlc's seeks go to the first moof after it.
fn sidx(start_ms: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(44);
    b.extend_from_slice(&44u32.to_be_bytes());
    b.extend_from_slice(b"sidx");
    b.extend_from_slice(&0u32.to_be_bytes()); // version 0, flags
    b.extend_from_slice(&VIDEO_ID.to_be_bytes()); // reference_ID
    b.extend_from_slice(&1000u32.to_be_bytes()); // timescale: ms
    b.extend_from_slice(&u32::try_from(start_ms).unwrap_or(u32::MAX).to_be_bytes()); // earliest_presentation_time
    b.extend_from_slice(&0u32.to_be_bytes()); // first_offset: the next box
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&1u16.to_be_bytes()); // reference_count
    b.extend_from_slice(&0x7fff_ffffu32.to_be_bytes()); // media reference, size (never used)
    b.extend_from_slice(&u32::MAX.to_be_bytes()); // subsegment_duration
    b.extend_from_slice(&0x9000_0000u32.to_be_bytes()); // starts with SAP, type 1
    b
}

/// One sample, on the output timeline, with its bytes.
#[derive(Debug, Clone)]
struct Sample {
    dts: u64,
    duration: u32,
    flags: u32,
    cto: i32,
    data: Bytes,
}

impl Sample {
    fn end(&self) -> u64 {
        self.dts + u64::from(self.duration)
    }
}

/// One input stream, as it is read.
struct Input<R> {
    name: &'static str,
    r: BufReader<R>,
    offset: u64,
    timescale: u64,
    /// The input's own trex, for the sample defaults.
    trex: Option<Trex>,
    /// The playlist time of the first sample, in ticks.
    start: u64,
    /// Ticks subtracted from every decode time; set at the first sample.
    shift: Option<i64>,
}

impl<R: AsyncRead + Unpin> Input<R> {
    fn new(name: &'static str, r: R) -> Self {
        Self {
            name,
            r: BufReader::with_capacity(READ_BUFFER, r),
            offset: 0,
            timescale: 1,
            trex: None,
            start: 0,
            shift: None,
        }
    }

    async fn read_box(&mut self) -> Result<Option<Mp4Box>, MuxError> {
        let b = read_box_async(&mut self.r, self.offset)
            .await
            .map_err(|e| MuxError::Input(self.name, e))?;
        if let Some(b) = &b {
            self.offset += b.size();
        }
        Ok(b)
    }

    /// Reads up to the moov; returns the ftyp (if any), the moov, and its first trak and trex.
    async fn read_init(
        &mut self,
        start_s: f64,
    ) -> Result<(Option<Mp4Box>, Mp4Box, Trak, Option<Trex>), MuxError> {
        let mut ftyp = None;
        for _ in 0..MAX_INIT_BOXES {
            match self.read_box().await? {
                Some(b) if b.kind() == FourCC::FTYP => ftyp = Some(b),
                Some(Mp4Box::Moov(moov)) => {
                    let trak = moov
                        .traks()
                        .next()
                        .cloned()
                        .ok_or(MuxError::NoTrack(self.name))?;
                    self.trex = moov.trexs().next().copied();
                    self.timescale = trak.mdhd().map_or(1, |m| u64::from(m.timescale().max(1)));
                    self.start = (start_s.max(0.0) * self.timescale as f64).round() as u64;
                    return Ok((ftyp, Mp4Box::Moov(moov), trak, self.trex));
                }
                Some(_) => {}
                None => break,
            }
        }
        Err(MuxError::NoMoov(self.name))
    }

    /// Reads the next fragment's samples, rebased onto the output timeline. `None` at the end of
    /// the input, also when it ends inside a box (a cancelled or failed stream: the producer
    /// reports that).
    async fn next_samples(&mut self) -> Result<Option<Vec<Sample>>, MuxError> {
        let mut frag = Fragment::default();
        loop {
            let b = match self.read_box().await {
                Ok(Some(b)) => b,
                Ok(None) | Err(MuxError::Input(_, rustet_mp4::Error::Truncated)) => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            match &b.kind().0 {
                b"moof" => frag.push(b),
                b"mdat" if frag.moof().is_some() => {
                    frag.push(b);
                    let samples = self.samples(frag)?;
                    if samples.is_empty() {
                        frag = Fragment::default();
                        continue;
                    }
                    return Ok(Some(samples));
                }
                // styp, sidx, ssix, emsg, prft, an mdat without a moof.
                _ => {}
            }
        }
    }

    fn samples(&mut self, frag: Fragment) -> Result<Vec<Sample>, MuxError> {
        let located = frag
            .full_samples(self.trex.as_ref())
            .map_err(|e| MuxError::Input(self.name, e))?;
        let Some(first) = located.first() else {
            return Ok(Vec::new());
        };
        let shift = *self
            .shift
            .get_or_insert(first.decode_time as i64 - self.start as i64);
        let data = Bytes::from(frag.mdat().map(|m| m.data.clone()).unwrap_or_default());
        Ok(located
            .into_iter()
            .map(|s| Sample {
                dts: u64::try_from(s.decode_time as i64 - shift).unwrap_or(0),
                duration: s.duration,
                flags: s.flags,
                cto: s.composition_time_offset,
                data: data.slice(s.data),
            })
            .collect())
    }

    /// Reads fragments into `tx` until the input or the writer ends.
    async fn feed(mut self, tx: mpsc::Sender<Vec<Sample>>) -> Result<(), MuxError> {
        while let Some(samples) = self.next_samples().await? {
            if tx.send(samples).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

/// A traf for `samples` of `track` whose data sits at `data_at` in the mdat payload.
fn traf(track: u32, samples: &[Sample], data_at: usize) -> Traf {
    let mut t = Tfdt::new(samples[0].dts);
    t.version = 1;
    let mut trun = Trun::new();
    trun.data_offset = data_at as i32; // moved by the moof size once it is known
    trun.samples = samples
        .iter()
        .map(|s| TrunSample {
            duration: s.duration,
            size: s.data.len() as u32,
            flags: s.flags,
            composition_time_offset: s.cto,
        })
        .collect();
    Traf {
        children: vec![Tfhd::new(track).into(), t.into(), trun.into()],
    }
}

/// One output fragment: a traf per non-empty track, one mdat with their data in that order.
fn fragment(sequence: u32, tracks: &[(u32, &[Sample])]) -> Result<Vec<u8>, MuxError> {
    let mut trafs = Vec::new();
    let mut data = Vec::new();
    for &(track, samples) in tracks.iter().filter(|(_, s)| !s.is_empty()) {
        trafs.push(traf(track, samples, data.len()));
        samples.iter().for_each(|s| data.extend_from_slice(&s.data));
    }
    let mut moof = Moof {
        pos: 0,
        children: vec![Mfhd::new(sequence).into()],
    };
    moof.children.extend(trafs.into_iter().map(Mp4Box::Traf));
    let mdat = Mdat {
        pos: 0,
        data,
        large: false,
    };
    // Data offsets count from the moof (default-base-is-moof): its size plus the mdat header.
    let base = (moof.size() + mdat.header_size()) as i32;
    for t in moof.trafs_mut() {
        if let Some(trun) = t.trun_mut() {
            trun.data_offset += base;
        }
    }
    let mut out = Mp4Box::Moof(moof).to_bytes().map_err(MuxError::Encode)?;
    out.extend(Mp4Box::Mdat(mdat).to_bytes().map_err(MuxError::Encode)?);
    Ok(out)
}

/// Muxes `video` and `audio` into `out` until both end. `video_size` gets the video's visible
/// size (from its sample entry) once the init segment is out; `index` records where each
/// fragment went.
///
/// Each input is read in its own future (a box read cannot be cancelled halfway) and sends whole
/// fragments; the writer writes a video fragment as soon as the audio it needs has arrived.
pub(crate) async fn mux(
    video: impl AsyncRead + Unpin,
    audio: impl AsyncRead + Unpin,
    plan: MuxPlan,
    out: &mut (dyn AsyncWrite + Send + Unpin),
    video_size: &watch::Sender<Option<(u32, u32)>>,
    index: &Mutex<MuxIndex>,
) -> Result<(), MuxError> {
    let mut v = Input::new("video", video);
    let mut a = Input::new("audio", audio);
    let (vi, ai) = tokio::join!(
        v.read_init(plan.video_start_s),
        a.read_init(plan.audio_start_s)
    );
    let ((ftyp, moov, vtrak, vtrex), (_, _, atrak, atrex)) = (vi?, ai?);
    let size = visible_size(&vtrak);
    let from_ms = (plan.video_start_s.min(plan.audio_start_s).max(0.0) * 1000.0) as u64;
    let mut head = init_segment(ftyp, moov, (vtrak, vtrex), (atrak, atrex), plan.duration_ms)?;
    head.extend(sidx(from_ms));
    out.write_all(&head).await?;
    let mut offset = head.len() as u64;
    lock(index).head = head;
    video_size.send_replace(size);

    let (v_ts, a_ts) = (v.timescale, a.timescale);
    let (vtx, mut vrx) = mpsc::channel::<Vec<Sample>>(1);
    let (atx, mut arx) = mpsc::channel::<Vec<Sample>>(1);
    let write = async {
        let mut sequence = 0u32;
        let mut queued: VecDeque<Sample> = VecDeque::new();
        let mut audio_done = false;
        // The audio is in up to here (audio ticks).
        let mut audio_end = (plan.audio_start_s.max(0.0) * a_ts as f64) as u64;
        let mut emit = async |video: &[Sample], audio: &[Sample]| -> Result<(), MuxError> {
            sequence += 1;
            let bytes = fragment(sequence, &[(VIDEO_ID, video), (AUDIO_ID, audio)])?;
            out.write_all(&bytes).await?;
            let span = |s: &[Sample], ts: u64| {
                s.first()
                    .map(|f| (f.dts * 1000 / ts, s[s.len() - 1].end() * 1000 / ts))
            };
            let (start_ms, end_ms) = span(video, v_ts)
                .or_else(|| span(audio, a_ts))
                .expect("a fragment has samples");
            let sync = video.first().is_none_or(|s| s.flags & NON_SYNC == 0);
            lock(index).frags.push(Written {
                start_ms,
                end_ms,
                offset,
                sync,
            });
            offset += bytes.len() as u64;
            Ok(())
        };
        while let Some(video) = vrx.recv().await {
            let v_end = video[video.len() - 1].end();
            // The audio that starts before this video fragment ends goes with it.
            while !audio_done
                && (audio_end as u128) * (v_ts as u128) < (v_end as u128) * (a_ts as u128)
            {
                match arx.recv().await {
                    Some(samples) => {
                        audio_end = samples[samples.len() - 1].end();
                        queued.extend(samples);
                    }
                    None => audio_done = true,
                }
            }
            let n = queued
                .iter()
                .take_while(|s| (s.dts as u128) * (v_ts as u128) < (v_end as u128) * (a_ts as u128))
                .count();
            let audio: Vec<Sample> = queued.drain(..n).collect();
            emit(&video, &audio).await?;
        }
        // The audio after the end of the video.
        loop {
            if queued.len() < AUDIO_TAIL_SAMPLES && !audio_done {
                match arx.recv().await {
                    Some(samples) => queued.extend(samples),
                    None => audio_done = true,
                }
                continue;
            }
            if queued.is_empty() {
                break;
            }
            let audio: Vec<Sample> = queued
                .drain(..queued.len().min(AUDIO_TAIL_SAMPLES))
                .collect();
            emit(&[], &audio).await?;
        }
        out.flush().await?;
        Ok::<_, MuxError>(())
    };
    let (fed_v, fed_a, written) = tokio::join!(v.feed(vtx), a.feed(atx), write);
    written.and(fed_v).and(fed_a)
}

fn lock(index: &Mutex<MuxIndex>) -> MutexGuard<'_, MuxIndex> {
    index.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Re-encodes `b` with the big-endian `u32` at `at` (from the start of the box) set to `v`.
fn patch_u32(b: &Mp4Box, at: usize, v: u32) -> Result<Mp4Box, MuxError> {
    let mut bytes = b.to_bytes().map_err(MuxError::Encode)?;
    if let Some(field) = bytes.get_mut(at..at + 4) {
        field.copy_from_slice(&v.to_be_bytes());
    }
    Mp4Box::decode(&bytes, 0)
        .map(|(b, _)| b)
        .map_err(MuxError::Encode)
}

/// `trak` with its tkhd's track ID set to `id`.
fn renumber(mut trak: Trak, id: u32) -> Result<Trak, MuxError> {
    for child in &mut trak.children {
        if let Mp4Box::Tkhd(tkhd) = child {
            // Box header (8), version and flags (4), two times (4 or 8 each).
            let at = 8 + if tkhd.version() == 1 { 20 } else { 12 };
            *child = patch_u32(child, at, id)?;
        }
    }
    Ok(trak)
}

/// The visible size of a visual sample entry (width and height at bytes 24 and 26 of its body).
fn visible_size(trak: &Trak) -> Option<(u32, u32)> {
    let entry = trak.stsd()?.entries.first()?;
    let body = match entry {
        Mp4Box::SampleEntry(e) => e.header.as_slice(),
        Mp4Box::Raw(r) => r.payload.as_slice(),
        _ => return None,
    };
    let be = |at: usize| {
        body.get(at..at + 2)
            .map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])))
    };
    Some((be(24)?, be(26)?)).filter(|&(w, h)| w > 0 && h > 0)
}

/// The output init segment: the video's ftyp and moov with the audio track added.
fn init_segment(
    ftyp: Option<Mp4Box>,
    video_moov: Mp4Box,
    (video_trak, video_trex): (Trak, Option<Trex>),
    (audio_trak, audio_trex): (Trak, Option<Trex>),
    duration_ms: u64,
) -> Result<Vec<u8>, MuxError> {
    let Mp4Box::Moov(mut moov) = video_moov else {
        unreachable!("read_init returns a moov")
    };
    moov.children
        .retain(|c| !matches!(c.kind(), FourCC::TRAK | FourCC::MVEX | FourCC::PSSH));
    let trex = |t: Option<Trex>, id| {
        Mp4Box::Trex(Trex {
            track_id: id,
            ..t.unwrap_or_else(|| Trex::new(id))
        })
    };
    let mut traks = [
        renumber(video_trak, VIDEO_ID)?,
        renumber(audio_trak, AUDIO_ID)?,
    ];
    let movie_timescale = moov
        .mvhd()
        .map_or(1000, |m| u64::from(m.timescale().max(1)));
    if duration_ms > 0 {
        // libvlc's MP4 demuxer reads every fragment before it plays a moov whose length is 0.
        if let Some(m) = moov.mvhd_mut() {
            m.set_duration(duration_ms * movie_timescale / 1000);
        }
        for t in &mut traks {
            if let Some(h) = t.tkhd_mut() {
                h.set_duration(duration_ms * movie_timescale / 1000);
            }
            if let Some(m) = t.mdhd_mut() {
                let ts = u64::from(m.timescale());
                m.set_duration(duration_ms * ts / 1000);
            }
        }
    }
    // mvhd's last field is next_track_ID.
    if let Some(i) = moov.children.iter().position(|c| c.kind() == FourCC::MVHD) {
        let at = usize::try_from(moov.children[i].size())
            .unwrap_or(0)
            .saturating_sub(4);
        moov.children[i] = patch_u32(&moov.children[i], at, AUDIO_ID + 1)?;
    }
    let after_mvhd = moov
        .children
        .iter()
        .position(|c| c.kind() == FourCC::MVHD)
        .map_or(0, |i| i + 1);
    let [v, a] = traks;
    moov.children
        .splice(after_mvhd..after_mvhd, [Mp4Box::Trak(v), Mp4Box::Trak(a)]);
    moov.push(Mp4Box::Container(Container::new(
        FourCC::MVEX,
        vec![trex(video_trex, VIDEO_ID), trex(audio_trex, AUDIO_ID)],
    )));
    let mut init = InitSegment::default();
    init.children.extend(ftyp);
    init.push(Mp4Box::Moov(moov));
    init.to_bytes().map_err(MuxError::Encode)
}

#[cfg(test)]
mod tests;
