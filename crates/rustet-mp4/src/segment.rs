//! Init segments and fragments: ordered top-level boxes with typed access.

use std::ops::Range;

use crate::boxes::{Children, Mdat, Moof, Moov, Mp4Box, Traf, Trex};
use crate::{Error, Result};

/// An init segment (mp4ff `InitSegment`): the top-level boxes the caller kept,
/// normally `ftyp` and `moov`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InitSegment {
    pub children: Vec<Mp4Box>,
}

impl Children for InitSegment {
    fn children(&self) -> &[Mp4Box] {
        &self.children
    }
    fn children_mut(&mut self) -> &mut Vec<Mp4Box> {
        &mut self.children
    }
}

impl InitSegment {
    pub fn moov(&self) -> Option<&Moov> {
        self.first()
    }

    pub fn moov_mut(&mut self) -> Option<&mut Moov> {
        self.first_mut()
    }

    pub fn size(&self) -> u64 {
        self.children_size()
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<()> {
        self.children.iter().try_for_each(|c| c.encode_into(out))
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.size() as usize);
        self.encode_into(&mut out)?;
        Ok(out)
    }
}

/// A media fragment (mp4ff `Fragment`): `moof`, `mdat` and any other top-level boxes the
/// caller kept (`emsg`, `prft`, ...), in stream order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fragment {
    pub children: Vec<Mp4Box>,
}

impl Children for Fragment {
    fn children(&self) -> &[Mp4Box] {
        &self.children
    }
    fn children_mut(&mut self) -> &mut Vec<Mp4Box> {
        &mut self.children
    }
}

/// What [`Fragment::trim_start`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trim {
    /// Every sample ends at or before the target: the caller drops the fragment.
    Before,
    /// The fragment holds the target or lies after it; samples before the target are gone.
    Kept,
}

/// The samples at the start of a fragment that a seek skips.
struct Leading {
    count: usize,
    bytes: usize,
    /// The decode time after them.
    end: u64,
}

/// The first traf and the mdat of a fragment, both mutable.
fn split_traf_mdat(children: &mut [Mp4Box]) -> (Option<&mut Traf>, Option<&mut Mdat>) {
    let (mut traf, mut mdat) = (None, None);
    for child in children {
        match child {
            Mp4Box::Moof(moof) if traf.is_none() => traf = moof.traf_mut(),
            Mp4Box::Mdat(m) if mdat.is_none() => mdat = Some(m),
            _ => {}
        }
    }
    (traf, mdat)
}

/// One sample located in a fragment (mp4ff `FullSample`), with the tfhd / trex defaults
/// applied. `data` indexes [`Mdat::data`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullSample {
    pub data: Range<usize>,
    pub decode_time: u64,
    pub duration: u32,
    pub flags: u32,
    pub composition_time_offset: i32,
}

impl Fragment {
    pub fn moof(&self) -> Option<&Moof> {
        self.first()
    }

    pub fn moof_mut(&mut self) -> Option<&mut Moof> {
        self.first_mut()
    }

    pub fn mdat(&self) -> Option<&Mdat> {
        self.first()
    }

    pub fn mdat_mut(&mut self) -> Option<&mut Mdat> {
        self.first_mut()
    }

    pub fn size(&self) -> u64 {
        self.children_size()
    }

    /// Locates the samples of one track (mp4ff `Fragment.GetFullSamples`).
    ///
    /// With a `trex`, the traf with its track ID is used and a fragment without that
    /// track has no samples; without one, the first traf is used. Positions are the ones
    /// recorded at decode time ([`Moof::pos`], [`Mdat::pos`]), so call this before edits
    /// that change the moof size.
    pub fn full_samples(&self, trex: Option<&Trex>) -> Result<Vec<FullSample>> {
        let moof = self.moof().ok_or(Error::MissingBox("moof"))?;
        let mdat = self.mdat().ok_or(Error::MissingBox("mdat"))?;
        let traf = match trex {
            Some(trex) => {
                let mut found = None;
                for traf in moof.trafs() {
                    if traf.tfhd().ok_or(Error::MissingBox("tfhd"))?.track_id == trex.track_id {
                        found = Some(traf);
                        break;
                    }
                }
                match found {
                    Some(traf) => traf,
                    None => return Ok(Vec::new()),
                }
            }
            None => moof.traf().ok_or(Error::MissingBox("traf"))?,
        };
        let tfhd = traf.tfhd().ok_or(Error::MissingBox("tfhd"))?;
        let pick = |present: bool, own: u32, from_trex: fn(&Trex) -> u32| {
            if present {
                own
            } else {
                trex.map_or(0, from_trex)
            }
        };
        let default_duration = pick(
            tfhd.has_default_sample_duration(),
            tfhd.default_sample_duration,
            |t| t.default_sample_duration,
        );
        let default_size = pick(
            tfhd.has_default_sample_size(),
            tfhd.default_sample_size,
            |t| t.default_sample_size,
        );
        let default_flags = pick(
            tfhd.has_default_sample_flags(),
            tfhd.default_sample_flags,
            |t| t.default_sample_flags,
        );

        let mut base_time = traf.tfdt().map_or(0, |t| t.base_media_decode_time());
        let truns: Vec<_> = traf.truns().collect();
        let mut samples = Vec::new();
        for (i, trun) in truns.iter().enumerate() {
            // A trun without sample sizes followed by another trun: share the gap between
            // their data offsets evenly (mp4ff does the same).
            let mut inferred_size = None;
            if !trun.has_sample_size()
                && trun.sample_count() > 0
                && let Some(next) = truns.get(i + 1)
                && trun.has_data_offset()
                && next.has_data_offset()
            {
                let gap = next.data_offset.wrapping_sub(trun.data_offset);
                inferred_size = Some(gap.wrapping_div(trun.sample_count() as i32) as u32);
            }
            let mut base = if tfhd.has_base_data_offset() {
                tfhd.base_data_offset
            } else {
                moof.pos
            };
            if trun.has_data_offset() {
                base = (i64::from(trun.data_offset)).wrapping_add(base as i64) as u64;
            }
            let mut offset = if base > 0 {
                let off = base.wrapping_sub(mdat.payload_pos());
                if off > mdat.data.len() as u64 {
                    return Err(Error::SampleOutOfRange("offset in mdat beyond size"));
                }
                off
            } else {
                0
            };
            for (j, s) in trun.samples.iter().enumerate() {
                let duration = if trun.has_sample_duration() {
                    s.duration
                } else {
                    default_duration
                };
                let size = match inferred_size {
                    Some(n) => n,
                    None if trun.has_sample_size() => s.size,
                    None => default_size,
                };
                let flags = if trun.has_sample_flags() || (j == 0 && trun.has_first_sample_flags())
                {
                    s.flags
                } else {
                    default_flags
                };
                let end = offset + u64::from(size);
                if end > mdat.data.len() as u64 {
                    return Err(Error::SampleOutOfRange("sample extends past mdat"));
                }
                samples.push(FullSample {
                    data: offset as usize..end as usize,
                    decode_time: base_time,
                    duration,
                    flags,
                    composition_time_offset: s.composition_time_offset,
                });
                base_time += u64::from(duration);
                offset = end;
            }
        }
        Ok(samples)
    }

    /// Makes a stream that seeks to `target` (track timescale) start at the sample that holds
    /// it: drops the leading samples that end at or before `target`, with their bytes, and
    /// moves the tfdt to the first sample kept. Apple's AAC fragments are about 10 s long, so
    /// without this a seek lands up to 10 s early.
    ///
    /// `fallback_duration` is the sample duration when neither the trun nor the tfhd has
    /// one. The fragment is left as it is when its layout is not the plain one: a single
    /// traf with a tfdt and one trun, known sample sizes, sample data at the start of the
    /// mdat (the layout [`encode_into`](Self::encode_into) writes).
    pub fn trim_start(&mut self, target: u64, fallback_duration: u32) -> Trim {
        let Some(lead) = self.leading_samples(target, fallback_duration) else {
            return Trim::Kept;
        };
        let (Some(traf), Some(mdat)) = split_traf_mdat(&mut self.children) else {
            return Trim::Kept;
        };
        let (Some(trun), true) = (traf.trun_mut(), mdat.data.len() >= lead.bytes) else {
            return Trim::Kept;
        };
        if lead.count == trun.samples.len() && lead.count > 0 {
            return Trim::Before;
        }
        trun.samples.drain(..lead.count);
        mdat.data.drain(..lead.bytes);
        if let Some(tfdt) = traf.tfdt_mut() {
            tfdt.set_base_media_decode_time(lead.end);
        }
        Trim::Kept
    }

    /// The leading samples that end at or before `target`. `None` when the layout is not the
    /// plain one (see [`trim_start`](Self::trim_start)).
    fn leading_samples(&self, target: u64, fallback_duration: u32) -> Option<Leading> {
        let mut trafs = self.moof()?.trafs();
        let (traf, None) = (trafs.next()?, trafs.next()) else {
            return None;
        };
        let mut truns = traf.truns();
        let (trun, None) = (truns.next()?, truns.next()) else {
            return None;
        };
        let tfhd = traf.tfhd()?;
        let default_duration =
            match tfhd.has_default_sample_duration() && tfhd.default_sample_duration > 0 {
                true => tfhd.default_sample_duration,
                false => fallback_duration,
            };
        let mut lead = Leading {
            count: 0,
            bytes: 0,
            end: traf.tfdt()?.base_media_decode_time(),
        };
        for sample in &trun.samples {
            let duration = if trun.has_sample_duration() {
                sample.duration
            } else {
                default_duration
            };
            if lead.end + u64::from(duration) > target {
                break;
            }
            let size = match (trun.has_sample_size(), tfhd.has_default_sample_size()) {
                (true, _) => sample.size,
                (false, true) => tfhd.default_sample_size,
                (false, false) => return None,
            };
            lead.count += 1;
            lead.bytes += size as usize;
            lead.end += u64::from(duration);
        }
        Some(lead)
    }

    /// Sets the trun data offset when the fragment has exactly one trun, as mp4ff's
    /// `Fragment.Encode` does: the moof size plus the mdat header size (sample data
    /// starts the mdat payload). With several truns the offsets are left as they are.
    pub fn set_trun_data_offsets(&mut self) {
        let Some(header) = self.mdat().map(Mdat::header_size) else {
            return;
        };
        let Some(moof) = self.moof_mut() else { return };
        if moof.trafs().map(|t| t.truns().count()).sum::<usize>() != 1 {
            return;
        }
        let offset = (moof.size() + header) as i32;
        if let Some(trun) = moof.trafs_mut().find_map(|t| t.trun_mut()) {
            trun.data_offset = offset;
        }
    }

    /// Encodes the fragment after [`set_trun_data_offsets`](Self::set_trun_data_offsets)
    /// (mp4ff `Fragment.Encode`). Fails without a moof or mdat.
    pub fn encode_into(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if self.moof().is_none() {
            return Err(Error::MissingBox("moof"));
        }
        if self.mdat().is_none() {
            return Err(Error::MissingBox("mdat"));
        }
        self.set_trun_data_offsets();
        self.children.iter().try_for_each(|c| c.encode_into(out))
    }

    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.size() as usize);
        self.encode_into(&mut out)?;
        Ok(out)
    }
}
