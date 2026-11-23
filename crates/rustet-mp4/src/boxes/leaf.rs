//! Leaf boxes.
//!
//! Boxes the stream code edits (`mfhd`, `tfhd`, `tfdt`, `trun`, `trex`, `frma`) are fully
//! decoded and re-encoded from their fields. Boxes that are only read, or edited in one
//! or two fields (`mvhd`, `tkhd`, `mdhd`, `hdlr`, `schm`, `tenc`, `saio`, `sbgp`, `sgpd`,
//! `senc`), keep their payload bytes and expose typed accessors, so they re-encode
//! exactly as read.

use super::{Cursor, be_u32, be_u64};
use crate::{Error, FourCC, Result};

/// A box kept as its payload bytes (everything after the header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBox {
    pub kind: FourCC,
    pub payload: Vec<u8>,
}

/// `mdat`. `pos` is the absolute stream position it was decoded at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mdat {
    pub pos: u64,
    pub data: Vec<u8>,
    /// Encoded with a 64-bit `largesize` header (kept from the input).
    pub large: bool,
}

impl Mdat {
    /// 8, or 16 with a `largesize` header.
    pub fn header_size(&self) -> u64 {
        if self.large || self.data.len() as u64 + 8 > u64::from(u32::MAX) {
            16
        } else {
            8
        }
    }

    /// Absolute position of the first data byte.
    pub fn payload_pos(&self) -> u64 {
        self.pos + self.header_size()
    }
}

fn too_short(kind: FourCC) -> Error {
    Error::InvalidBox {
        kind,
        reason: "body too short",
    }
}

/// Validates that a full box payload is at least `v0` / `v1` bytes long for its version.
fn check_len(kind: FourCC, payload: &[u8], v0: usize, v1: usize) -> Result<()> {
    let need = if payload.first() == Some(&1) { v1 } else { v0 };
    if payload.len() < need {
        Err(too_short(kind))
    } else {
        Ok(())
    }
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_be_bytes());
}

/// `mvhd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mvhd {
    pub(super) payload: Vec<u8>,
}

impl Mvhd {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        check_len(FourCC::MVHD, &payload, 20, 32)?;
        Ok(Self { payload })
    }

    pub fn version(&self) -> u8 {
        self.payload[0]
    }

    pub fn timescale(&self) -> u32 {
        be_u32(&self.payload, if self.version() == 1 { 20 } else { 12 })
    }

    pub fn duration(&self) -> u64 {
        if self.version() == 1 {
            be_u64(&self.payload, 24)
        } else {
            u64::from(be_u32(&self.payload, 16))
        }
    }

    /// Sets the duration in the box's own field width (a version 0 box keeps 32 bits,
    /// as mp4ff writes it).
    pub fn set_duration(&mut self, duration: u64) {
        if self.version() == 1 {
            put_u64(&mut self.payload, 24, duration);
        } else {
            put_u32(&mut self.payload, 16, duration as u32);
        }
    }
}

/// `tkhd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tkhd {
    pub(super) payload: Vec<u8>,
}

impl Tkhd {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        check_len(FourCC::TKHD, &payload, 24, 36)?;
        Ok(Self { payload })
    }

    pub fn version(&self) -> u8 {
        self.payload[0]
    }

    pub fn track_id(&self) -> u32 {
        be_u32(&self.payload, if self.version() == 1 { 20 } else { 12 })
    }

    pub fn duration(&self) -> u64 {
        if self.version() == 1 {
            be_u64(&self.payload, 28)
        } else {
            u64::from(be_u32(&self.payload, 20))
        }
    }

    /// Sets the duration in the box's own field width (see [`Mvhd::set_duration`]).
    pub fn set_duration(&mut self, duration: u64) {
        if self.version() == 1 {
            put_u64(&mut self.payload, 28, duration);
        } else {
            put_u32(&mut self.payload, 20, duration as u32);
        }
    }
}

/// `mdhd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mdhd {
    pub(super) payload: Vec<u8>,
}

impl Mdhd {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        check_len(FourCC::MDHD, &payload, 20, 32)?;
        Ok(Self { payload })
    }

    pub fn version(&self) -> u8 {
        self.payload[0]
    }

    pub fn timescale(&self) -> u32 {
        be_u32(&self.payload, if self.version() == 1 { 20 } else { 12 })
    }

    pub fn duration(&self) -> u64 {
        if self.version() == 1 {
            be_u64(&self.payload, 24)
        } else {
            u64::from(be_u32(&self.payload, 16))
        }
    }

    /// Sets the duration in the box's own field width (see [`Mvhd::set_duration`]).
    pub fn set_duration(&mut self, duration: u64) {
        if self.version() == 1 {
            put_u64(&mut self.payload, 24, duration);
        } else {
            put_u32(&mut self.payload, 16, duration as u32);
        }
    }
}

/// `hdlr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hdlr {
    pub(super) payload: Vec<u8>,
}

impl Hdlr {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        check_len(FourCC::HDLR, &payload, 12, 12)?;
        Ok(Self { payload })
    }

    pub fn handler_type(&self) -> FourCC {
        FourCC([
            self.payload[8],
            self.payload[9],
            self.payload[10],
            self.payload[11],
        ])
    }
}

/// `trex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trex {
    pub version_flags: u32,
    pub track_id: u32,
    pub default_sample_description_index: u32,
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
}

impl Trex {
    /// A trex for `track_id` with sample description index 1 and zero defaults
    /// (mp4ff `CreateTrex`).
    pub fn new(track_id: u32) -> Self {
        Self {
            version_flags: 0,
            track_id,
            default_sample_description_index: 1,
            default_sample_duration: 0,
            default_sample_size: 0,
            default_sample_flags: 0,
        }
    }

    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body, FourCC::TREX);
        Ok(Self {
            version_flags: c.u32()?,
            track_id: c.u32()?,
            default_sample_description_index: c.u32()?,
            default_sample_duration: c.u32()?,
            default_sample_size: c.u32()?,
            default_sample_flags: c.u32()?,
        })
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        for v in [
            self.version_flags,
            self.track_id,
            self.default_sample_description_index,
            self.default_sample_duration,
            self.default_sample_size,
            self.default_sample_flags,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

/// `mfhd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mfhd {
    pub version_flags: u32,
    pub sequence_number: u32,
}

impl Mfhd {
    pub fn new(sequence_number: u32) -> Self {
        Self {
            version_flags: 0,
            sequence_number,
        }
    }

    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body, FourCC::MFHD);
        Ok(Self {
            version_flags: c.u32()?,
            sequence_number: c.u32()?,
        })
    }
}

/// `tfhd`. Optional fields are encoded only when their flag is set; a field whose flag is
/// clear keeps its value in memory but is not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tfhd {
    pub version: u8,
    pub flags: u32,
    pub track_id: u32,
    pub base_data_offset: u64,
    pub sample_description_index: u32,
    pub default_sample_duration: u32,
    pub default_sample_size: u32,
    pub default_sample_flags: u32,
}

impl Tfhd {
    pub const BASE_DATA_OFFSET_PRESENT: u32 = 0x01;
    pub const SAMPLE_DESCRIPTION_INDEX_PRESENT: u32 = 0x02;
    pub const DEFAULT_SAMPLE_DURATION_PRESENT: u32 = 0x08;
    pub const DEFAULT_SAMPLE_SIZE_PRESENT: u32 = 0x10;
    pub const DEFAULT_SAMPLE_FLAGS_PRESENT: u32 = 0x20;
    pub const DURATION_IS_EMPTY: u32 = 0x01_0000;
    pub const DEFAULT_BASE_IS_MOOF: u32 = 0x02_0000;

    /// A tfhd for `track_id` with default-base-is-moof set (mp4ff `CreateTfhd`).
    pub fn new(track_id: u32) -> Self {
        Self {
            version: 0,
            flags: Self::DEFAULT_BASE_IS_MOOF,
            track_id,
            base_data_offset: 0,
            sample_description_index: 1,
            default_sample_duration: 0,
            default_sample_size: 0,
            default_sample_flags: 0,
        }
    }

    pub fn has_base_data_offset(&self) -> bool {
        self.flags & Self::BASE_DATA_OFFSET_PRESENT != 0
    }

    pub fn has_sample_description_index(&self) -> bool {
        self.flags & Self::SAMPLE_DESCRIPTION_INDEX_PRESENT != 0
    }

    pub fn has_default_sample_duration(&self) -> bool {
        self.flags & Self::DEFAULT_SAMPLE_DURATION_PRESENT != 0
    }

    pub fn has_default_sample_size(&self) -> bool {
        self.flags & Self::DEFAULT_SAMPLE_SIZE_PRESENT != 0
    }

    pub fn has_default_sample_flags(&self) -> bool {
        self.flags & Self::DEFAULT_SAMPLE_FLAGS_PRESENT != 0
    }

    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body, FourCC::TFHD);
        let version_flags = c.u32()?;
        let mut t = Self {
            version: (version_flags >> 24) as u8,
            flags: version_flags & 0x00ff_ffff,
            track_id: c.u32()?,
            base_data_offset: 0,
            sample_description_index: 0,
            default_sample_duration: 0,
            default_sample_size: 0,
            default_sample_flags: 0,
        };
        if t.has_base_data_offset() {
            t.base_data_offset = c.u64()?;
        }
        if t.has_sample_description_index() {
            t.sample_description_index = c.u32()?;
        }
        if t.has_default_sample_duration() {
            t.default_sample_duration = c.u32()?;
        }
        if t.has_default_sample_size() {
            t.default_sample_size = c.u32()?;
        }
        if t.has_default_sample_flags() {
            t.default_sample_flags = c.u32()?;
        }
        Ok(t)
    }

    pub(super) fn body_size(&self) -> u64 {
        let mut n = 8;
        if self.has_base_data_offset() {
            n += 8;
        }
        for present in [
            self.has_sample_description_index(),
            self.has_default_sample_duration(),
            self.has_default_sample_size(),
            self.has_default_sample_flags(),
        ] {
            if present {
                n += 4;
            }
        }
        n
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&((u32::from(self.version) << 24) | self.flags).to_be_bytes());
        out.extend_from_slice(&self.track_id.to_be_bytes());
        if self.has_base_data_offset() {
            out.extend_from_slice(&self.base_data_offset.to_be_bytes());
        }
        if self.has_sample_description_index() {
            out.extend_from_slice(&self.sample_description_index.to_be_bytes());
        }
        if self.has_default_sample_duration() {
            out.extend_from_slice(&self.default_sample_duration.to_be_bytes());
        }
        if self.has_default_sample_size() {
            out.extend_from_slice(&self.default_sample_size.to_be_bytes());
        }
        if self.has_default_sample_flags() {
            out.extend_from_slice(&self.default_sample_flags.to_be_bytes());
        }
    }
}

/// `tfdt`. Version 1 stores 64 bits; version 0 stores 32 (the value is truncated on
/// encode, as in mp4ff).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tfdt {
    pub version: u8,
    pub flags: u32,
    base_media_decode_time: u64,
}

impl Tfdt {
    /// mp4ff `CreateTfdt`: version 1 only when the time needs 64 bits.
    pub fn new(base_media_decode_time: u64) -> Self {
        let mut t = Self {
            version: 0,
            flags: 0,
            base_media_decode_time: 0,
        };
        t.set_base_media_decode_time(base_media_decode_time);
        t
    }

    pub fn base_media_decode_time(&self) -> u64 {
        self.base_media_decode_time
    }

    /// mp4ff `SetBaseMediaDecodeTime`: also sets the version to 0 or 1 from the value's
    /// magnitude. Set [`version`](Self::version) afterwards to force 64 bits.
    pub fn set_base_media_decode_time(&mut self, t: u64) {
        self.version = u8::from(t > u64::from(u32::MAX));
        self.base_media_decode_time = t;
    }

    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(body, FourCC::TFDT);
        let version_flags = c.u32()?;
        let version = (version_flags >> 24) as u8;
        let base_media_decode_time = if version == 1 {
            c.u64()?
        } else {
            u64::from(c.u32()?)
        };
        Ok(Self {
            version,
            flags: version_flags & 0x00ff_ffff,
            base_media_decode_time,
        })
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&((u32::from(self.version) << 24) | self.flags).to_be_bytes());
        if self.version == 1 {
            out.extend_from_slice(&self.base_media_decode_time.to_be_bytes());
        } else {
            out.extend_from_slice(&(self.base_media_decode_time as u32).to_be_bytes());
        }
    }
}

/// One `trun` sample. Fields the trun does not carry are 0 (defaults come from tfhd/trex,
/// see [`Fragment::full_samples`](crate::Fragment::full_samples)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrunSample {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
    pub composition_time_offset: i32,
}

/// `trun`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trun {
    pub version: u8,
    pub flags: u32,
    pub data_offset: i32,
    pub first_sample_flags: u32,
    pub samples: Vec<TrunSample>,
}

impl Trun {
    pub const DATA_OFFSET_PRESENT: u32 = 0x01;
    pub const FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0x04;
    pub const SAMPLE_DURATION_PRESENT: u32 = 0x100;
    pub const SAMPLE_SIZE_PRESENT: u32 = 0x200;
    pub const SAMPLE_FLAGS_PRESENT: u32 = 0x400;
    pub const SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT: u32 = 0x800;

    /// An empty version 1 trun with a data offset and every per-sample field
    /// (mp4ff `CreateTrun`).
    pub fn new() -> Self {
        Self {
            version: 1,
            flags: 0xf01,
            data_offset: 0,
            first_sample_flags: 0,
            samples: Vec::new(),
        }
    }

    pub fn has_data_offset(&self) -> bool {
        self.flags & Self::DATA_OFFSET_PRESENT != 0
    }

    pub fn has_first_sample_flags(&self) -> bool {
        self.flags & Self::FIRST_SAMPLE_FLAGS_PRESENT != 0
    }

    pub fn has_sample_duration(&self) -> bool {
        self.flags & Self::SAMPLE_DURATION_PRESENT != 0
    }

    pub fn has_sample_size(&self) -> bool {
        self.flags & Self::SAMPLE_SIZE_PRESENT != 0
    }

    pub fn has_sample_flags(&self) -> bool {
        self.flags & Self::SAMPLE_FLAGS_PRESENT != 0
    }

    pub fn has_sample_composition_time_offset(&self) -> bool {
        self.flags & Self::SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT != 0
    }

    pub fn sample_count(&self) -> u32 {
        self.samples.len() as u32
    }

    /// Total duration: per-sample durations if present, else `default_sample_duration`
    /// for every sample (mp4ff `Trun.Duration`).
    pub fn duration(&self, default_sample_duration: u32) -> u64 {
        if self.has_sample_duration() {
            self.samples.iter().map(|s| u64::from(s.duration)).sum()
        } else {
            u64::from(default_sample_duration) * self.samples.len() as u64
        }
    }

    /// Sum of the per-sample sizes held in memory.
    pub fn size_of_data(&self) -> u64 {
        self.samples.iter().map(|s| u64::from(s.size)).sum()
    }

    fn bytes_per_sample(&self) -> u64 {
        [
            self.has_sample_duration(),
            self.has_sample_size(),
            self.has_sample_flags(),
            self.has_sample_composition_time_offset(),
        ]
        .into_iter()
        .map(|present| if present { 4 } else { 0 })
        .sum()
    }

    fn body_size_for(&self, count: u64) -> u64 {
        8 + if self.has_data_offset() { 4 } else { 0 }
            + if self.has_first_sample_flags() { 4 } else { 0 }
            + count * self.bytes_per_sample()
    }

    pub(super) fn body_size(&self) -> u64 {
        self.body_size_for(self.samples.len() as u64)
    }

    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        let kind = FourCC::TRUN;
        let mut c = Cursor::new(body, kind);
        let version_flags = c.u32()?;
        let count = c.u32()?;
        let mut t = Self {
            version: (version_flags >> 24) as u8,
            flags: version_flags & 0x00ff_ffff,
            data_offset: 0,
            first_sample_flags: 0,
            samples: Vec::new(),
        };
        if t.body_size_for(u64::from(count)) != body.len() as u64 {
            return Err(Error::InvalidBox {
                kind,
                reason: "size does not match sample count and flags",
            });
        }
        if count > 1024 && t.bytes_per_sample() == 0 {
            return Err(Error::InvalidBox {
                kind,
                reason: "big sample count but no sample data present",
            });
        }
        if t.has_data_offset() {
            t.data_offset = c.u32()? as i32;
        }
        if t.has_first_sample_flags() {
            t.first_sample_flags = c.u32()?;
        }
        t.samples.reserve(count as usize);
        for i in 0..count {
            let mut s = TrunSample::default();
            if t.has_sample_duration() {
                s.duration = c.u32()?;
            }
            if t.has_sample_size() {
                s.size = c.u32()?;
            }
            if t.has_sample_flags() {
                s.flags = c.u32()?;
            } else if t.has_first_sample_flags() && i == 0 {
                s.flags = t.first_sample_flags;
            }
            if t.has_sample_composition_time_offset() {
                s.composition_time_offset = c.u32()? as i32;
            }
            t.samples.push(s);
        }
        Ok(t)
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&((u32::from(self.version) << 24) | self.flags).to_be_bytes());
        out.extend_from_slice(&self.sample_count().to_be_bytes());
        if self.has_data_offset() {
            // mp4ff refuses (panics) here: an unset offset would point into the moof.
            if self.data_offset == 0 {
                return Err(Error::TrunDataOffsetNotSet);
            }
            out.extend_from_slice(&self.data_offset.to_be_bytes());
        }
        if self.has_first_sample_flags() {
            out.extend_from_slice(&self.first_sample_flags.to_be_bytes());
        }
        for s in &self.samples {
            if self.has_sample_duration() {
                out.extend_from_slice(&s.duration.to_be_bytes());
            }
            if self.has_sample_size() {
                out.extend_from_slice(&s.size.to_be_bytes());
            }
            if self.has_sample_flags() {
                out.extend_from_slice(&s.flags.to_be_bytes());
            }
            if self.has_sample_composition_time_offset() {
                out.extend_from_slice(&s.composition_time_offset.to_be_bytes());
            }
        }
        Ok(())
    }
}

impl Default for Trun {
    fn default() -> Self {
        Self::new()
    }
}

/// One subsample of an encrypted sample: clear bytes followed by protected bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubSample {
    pub clear: u16,
    pub protected: u32,
}

/// Parsed `senc` content, as two parallel lists like mp4ff's `SencBox.IVs` and
/// `SencBox.SubSamples`. Either list may be empty (constant IV, full-sample encryption).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SencSamples {
    pub ivs: Vec<Vec<u8>>,
    pub subsamples: Vec<Vec<SubSample>>,
}

/// `senc`. The per-sample data is parsed on demand ([`parse`](Self::parse)) because the
/// IV size comes from the init segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Senc {
    /// Absolute stream position of the box.
    pub pos: u64,
    pub version_flags: u32,
    pub sample_count: u32,
    /// Everything after the sample count.
    pub raw: Vec<u8>,
}

impl Senc {
    pub const USE_SUBSAMPLE_ENCRYPTION: u32 = 0x2;

    pub(super) fn decode(body: Vec<u8>, pos: u64) -> Result<Self> {
        let kind = FourCC::SENC;
        if body.len() < 8 {
            return Err(Error::InvalidBox {
                kind,
                reason: "box size less than 16",
            });
        }
        let version_flags = be_u32(&body, 0);
        if version_flags >> 24 > 0 {
            return Err(Error::InvalidBox {
                kind,
                reason: "version not supported",
            });
        }
        let sample_count = be_u32(&body, 4);
        let raw = body[8..].to_vec();
        if version_flags & Self::USE_SUBSAMPLE_ENCRYPTION != 0
            && (raw.len() as u64) < 2 * u64::from(sample_count)
        {
            return Err(Error::InvalidBox {
                kind,
                reason: "too small for its samples and subsample encryption",
            });
        }
        Ok(Self {
            pos,
            version_flags,
            sample_count,
            raw,
        })
    }

    pub fn flags(&self) -> u32 {
        self.version_flags & 0x00ff_ffff
    }

    /// mp4ff `SencBox.ParseReadBox`. `per_sample_iv_size` 0 means "infer it": from the
    /// data length without subsamples, else by trying 0, 8 and 16.
    ///
    /// A senc without samples or data parses to empty lists (mp4ff treats it as already
    /// parsed).
    pub fn parse(&self, per_sample_iv_size: u8) -> Result<SencSamples> {
        let count = self.sample_count as usize;
        if count == 0 || self.raw.is_empty() {
            return Ok(SencSamples::default());
        }
        if self.flags() & Self::USE_SUBSAMPLE_ENCRYPTION == 0 {
            let iv_size = match per_sample_iv_size {
                0 => self.raw.len() / count,
                n => usize::from(n),
            };
            let ivs = match iv_size {
                0 => Vec::new(),
                8 | 16 => {
                    let need = iv_size * count;
                    let data = self
                        .raw
                        .get(..need)
                        .ok_or(Error::Senc("IVs exceed the box"))?;
                    data.chunks_exact(iv_size).map(<[u8]>::to_vec).collect()
                }
                _ => return Err(Error::Senc("strange derived per-sample IV size")),
            };
            return Ok(SencSamples {
                ivs,
                subsamples: Vec::new(),
            });
        }
        if per_sample_iv_size != 0 {
            return self
                .parse_subsamples(usize::from(per_sample_iv_size))
                .ok_or(Error::Senc("bad subsample data"));
        }
        [0, 8, 16]
            .into_iter()
            .find_map(|n| self.parse_subsamples(n))
            .ok_or(Error::Senc("could not decode senc"))
    }

    /// Parses every sample with subsamples; `None` unless the data is consumed exactly.
    fn parse_subsamples(&self, iv_size: usize) -> Option<SencSamples> {
        let mut c = Cursor::new(&self.raw, FourCC::SENC);
        let mut out = SencSamples::default();
        for _ in 0..self.sample_count {
            if iv_size > 0 {
                out.ivs.push(c.bytes(iv_size).ok()?.to_vec());
            }
            let n = c.u16().ok()?;
            if c.remaining() < usize::from(n) * 6 {
                return None;
            }
            let subs = (0..n)
                .map(|_| {
                    Some(SubSample {
                        clear: c.u16().ok()?,
                        protected: c.u32().ok()?,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            out.subsamples.push(subs);
        }
        (c.remaining() == 0).then_some(out)
    }
}

/// `saio`. Read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saio {
    pub(super) payload: Vec<u8>,
    offsets: Vec<u64>,
}

impl Saio {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        let mut c = Cursor::new(&payload, FourCC::SAIO);
        let version_flags = c.u32()?;
        if version_flags & 1 != 0 {
            c.bytes(8)?; // aux_info_type + aux_info_type_parameter
        }
        let count = c.u32()?;
        let width = if version_flags >> 24 == 0 { 4 } else { 8 };
        if (c.remaining() as u64) < u64::from(count) * width {
            return Err(too_short(FourCC::SAIO));
        }
        let offsets = (0..count)
            .map(|_| {
                if width == 4 {
                    c.u32().map(u64::from)
                } else {
                    c.u64()
                }
            })
            .collect::<Result<_>>()?;
        Ok(Self { payload, offsets })
    }

    pub fn offsets(&self) -> &[u64] {
        &self.offsets
    }
}

/// `sbgp`. Read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sbgp {
    pub(super) payload: Vec<u8>,
    pub grouping_type: FourCC,
    /// `(sample_count, group_description_index)` entries.
    pub entries: Vec<(u32, u32)>,
}

impl Sbgp {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        let mut c = Cursor::new(&payload, FourCC::SBGP);
        let version = c.u32()? >> 24;
        let grouping_type = c.fourcc()?;
        if version == 1 {
            c.u32()?; // grouping_type_parameter
        }
        let count = c.u32()?;
        if (c.remaining() as u64) < u64::from(count) * 8 {
            return Err(too_short(FourCC::SBGP));
        }
        let entries = (0..count)
            .map(|_| Ok((c.u32()?, c.u32()?)))
            .collect::<Result<_>>()?;
        Ok(Self {
            payload,
            grouping_type,
            entries,
        })
    }
}

/// `sgpd`. Read-only; for `seig` groups the first entry's per-sample IV size is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sgpd {
    pub(super) payload: Vec<u8>,
    pub grouping_type: FourCC,
    /// `seig` only: per-sample IV size of the first entry.
    pub first_seig_iv_size: Option<u8>,
}

impl Sgpd {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        let mut c = Cursor::new(&payload, FourCC::SGPD);
        let version = c.u32()? >> 24;
        let grouping_type = c.fourcc()?;
        let default_length = if version == 1 { c.u32()? } else { 0 };
        if version >= 2 {
            c.u32()?; // default_sample_description_index
        }
        let count = c.u32()?;
        let mut first_seig_iv_size = None;
        if grouping_type == FourCC::SEIG && count > 0 {
            if version == 1 && default_length == 0 {
                c.u32()?; // description_length
            }
            // reserved, crypt/skip byte, isProtected, perSampleIVSize
            first_seig_iv_size = Some(c.bytes(4)?[3]);
        }
        Ok(Self {
            payload,
            grouping_type,
            first_seig_iv_size,
        })
    }
}

/// `frma`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frma {
    pub data_format: FourCC,
}

impl Frma {
    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        Ok(Self {
            data_format: Cursor::new(body, FourCC::FRMA).fourcc()?,
        })
    }
}

/// `schm`. Read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schm {
    pub(super) payload: Vec<u8>,
    pub scheme_type: FourCC,
    pub scheme_version: u32,
}

impl Schm {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        let mut c = Cursor::new(&payload, FourCC::SCHM);
        c.u32()?;
        let scheme_type = c.fourcc()?;
        let scheme_version = c.u32()?;
        Ok(Self {
            payload,
            scheme_type,
            scheme_version,
        })
    }
}

/// `tenc`. Read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenc {
    pub(super) payload: Vec<u8>,
    pub version: u8,
    pub default_crypt_byte_block: u8,
    pub default_skip_byte_block: u8,
    pub default_is_protected: u8,
    pub default_per_sample_iv_size: u8,
    pub default_kid: [u8; 16],
    /// Empty unless the track is protected with a constant IV (`cbcs`).
    pub default_constant_iv: Vec<u8>,
}

impl Tenc {
    pub(super) fn new(payload: Vec<u8>) -> Result<Self> {
        let mut c = Cursor::new(&payload, FourCC::TENC);
        let version = (c.u32()? >> 24) as u8;
        c.u8()?; // reserved
        let pattern = c.u8()?;
        let (default_crypt_byte_block, default_skip_byte_block) = if version > 0 {
            (pattern >> 4, pattern & 0x0f)
        } else {
            (0, 0)
        };
        let default_is_protected = c.u8()?;
        let default_per_sample_iv_size = c.u8()?;
        let mut default_kid = [0; 16];
        default_kid.copy_from_slice(c.bytes(16)?);
        let default_constant_iv = if default_is_protected == 1 && default_per_sample_iv_size == 0 {
            let n = c.u8()?;
            c.bytes(usize::from(n))?.to_vec()
        } else {
            Vec::new()
        };
        Ok(Self {
            payload,
            version,
            default_crypt_byte_block,
            default_skip_byte_block,
            default_is_protected,
            default_per_sample_iv_size,
            default_kid,
            default_constant_iv,
        })
    }
}
