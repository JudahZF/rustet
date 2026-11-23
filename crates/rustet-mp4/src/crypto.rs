//! Common Encryption: init-segment protection info, `senc` lookup and in-place
//! `cenc` / `cbcs` sample decryption (mp4ff `DecryptInit` / `DecryptFragment`).

use aes::Aes128;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit, StreamCipher};

use crate::boxes::{
    Children, Container, Mp4Box, RawBox, Schm, SencSamples, SubSample, Tenc, Traf, Trex,
};
use crate::{Error, FourCC, Fragment, InitSegment, Result};

/// The two Common Encryption schemes mp4ff decrypts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// AES-128 CTR.
    Cenc,
    /// AES-128 CBC with a constant IV per subsample and an optional crypt/skip pattern.
    Cbcs,
}

/// Protection of one encrypted sample entry: the `sinf` removed from it.
#[derive(Debug, Clone, PartialEq)]
pub struct Protection {
    pub scheme: Scheme,
    pub sinf: Container,
}

impl Protection {
    /// `sinf/schi/tenc`, if present.
    pub fn tenc(&self) -> Option<&Tenc> {
        self.sinf.container(FourCC::SCHI)?.first()
    }
}

/// Decryption info for one sample entry of a track (mp4ff `DecryptTrackInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub track_id: u32,
    /// `None` for a clear track.
    pub protection: Option<Protection>,
    pub trex: Option<Trex>,
}

/// Result of [`decrypt_init`] (mp4ff `DecryptInfo`).
///
/// A track with several encrypted sample entries (Apple's ALAC has two) has one
/// [`TrackInfo`] per entry, in stsd order; only the first of them gets the `trex`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DecryptInfo {
    /// The `pssh` boxes removed from `moov`.
    pub psshs: Vec<RawBox>,
    pub tracks: Vec<TrackInfo>,
}

impl DecryptInfo {
    /// The first track info for `track_id` (mp4ff `findTrackInfo`).
    pub fn track(&self, track_id: u32) -> Option<&TrackInfo> {
        self.tracks.iter().find(|t| t.track_id == track_id)
    }
}

/// Turns every `enca` / `encv` sample entry into its clear format, removes their `sinf`
/// and the `pssh` boxes of `moov`, and returns what is needed to decrypt the fragments
/// (mp4ff `DecryptInit`).
///
/// Unlike mp4ff, a missing `mvex` is not fatal: the tracks just get no `trex`.
pub fn decrypt_init(init: &mut InitSegment) -> Result<DecryptInfo> {
    let moov = init.moov_mut().ok_or(Error::MissingBox("moov"))?;
    let mut tracks = Vec::new();
    for trak in moov.traks_mut() {
        let track_id = trak.track_id().ok_or(Error::MissingBox("tkhd"))?;
        let stsd = trak.stsd_mut().ok_or(Error::MissingBox("stsd"))?;
        let mut encrypted = false;
        for entry in &mut stsd.entries {
            let Mp4Box::SampleEntry(entry) = entry else {
                continue;
            };
            if entry.kind != FourCC::ENCA && entry.kind != FourCC::ENCV {
                continue;
            }
            let sinf = entry.remove_encryption()?;
            let scheme_type = sinf
                .first::<Schm>()
                .ok_or(Error::MissingBox("schm"))?
                .scheme_type;
            let scheme = match scheme_type {
                FourCC::CENC => Scheme::Cenc,
                FourCC::CBCS => Scheme::Cbcs,
                other => return Err(Error::UnsupportedScheme(other)),
            };
            encrypted = true;
            tracks.push(TrackInfo {
                track_id,
                protection: Some(Protection { scheme, sinf }),
                trex: None,
            });
        }
        if !encrypted {
            tracks.push(TrackInfo {
                track_id,
                protection: None,
                trex: None,
            });
        }
    }
    for trex in moov.trexs() {
        if let Some(info) = tracks.iter_mut().find(|t| t.track_id == trex.track_id) {
            info.trex = Some(*trex);
        }
    }
    let psshs = moov.remove_psshs();
    Ok(DecryptInfo { psshs, tracks })
}

impl Traf {
    /// Parses the `senc` of this traf the way mp4ff's `DecryptFragment` does
    /// (`Traf.ParseReadSenc`): the `saio` offset must point at the senc data, and a
    /// `seig` sample group overrides `default_iv_size`.
    ///
    /// [`Error::NoSenc`] when there is no senc.
    pub fn parse_senc(&self, default_iv_size: u8, moof_pos: u64) -> Result<SencSamples> {
        let senc = self.senc().ok_or(Error::NoSenc)?;
        // mp4ff skips all checks for an empty senc: it counts as parsed already.
        if senc.sample_count == 0 || senc.raw.is_empty() {
            return Ok(SencSamples::default());
        }
        if let Some(saio) = self.saio() {
            let first = *saio
                .offsets()
                .first()
                .ok_or(Error::Senc("saio without offsets"))?;
            let from_saio = first.wrapping_add(moof_pos);
            if from_saio != senc.pos + 16 {
                return Err(Error::SaioMismatch {
                    saio: from_saio,
                    senc: senc.pos + 16,
                });
            }
        }
        let mut iv_size = default_iv_size;
        if let (Some(sbgp), Some(sgpd)) = (self.sbgp(), self.sgpd())
            && sbgp.grouping_type == FourCC::SEIG
            && sgpd.grouping_type == FourCC::SEIG
        {
            let [(_, index)] = sbgp.entries[..] else {
                return Err(Error::SampleGroup("only one sbgp entry supported"));
            };
            // Group description indices above 0x10000 refer to the fragment-local sgpd.
            if index != 0x1_0001 {
                return Err(Error::SampleGroup(
                    "sgpd entry number must be first inside = 65536 + 1",
                ));
            }
            iv_size = sgpd
                .first_seig_iv_size
                .ok_or(Error::SampleGroup("sgpd has no seig entry"))?;
        }
        senc.parse(iv_size)
    }
}

/// Decrypts every encrypted track of `frag` in place with `key`, then removes the
/// encryption boxes and moof `pssh` boxes and moves the trun data offsets back by the
/// removed size (mp4ff `DecryptFragment`).
///
/// [`Error::NoSenc`] means an encrypted track's traf has no senc: the fragment is clear
/// and is left unchanged.
pub fn decrypt_fragment(frag: &mut Fragment, info: &DecryptInfo, key: &[u8]) -> Result<()> {
    let key: &[u8; 16] = key.try_into().map_err(|_| Error::KeyLength(key.len()))?;
    let moof = frag.moof().ok_or(Error::MissingBox("moof"))?;
    let traf_count = moof.trafs().count();
    let moof_pos = moof.pos;
    let mut removed = 0u64;
    for index in 0..traf_count {
        let traf = nth_traf(frag, index)?;
        let track_id = traf.tfhd().ok_or(Error::MissingBox("tfhd"))?.track_id;
        let Some(ti) = info.track(track_id) else {
            continue;
        };
        let Some(protection) = &ti.protection else {
            continue;
        };
        if traf.senc().is_none() {
            return Err(Error::NoSenc);
        }
        let tenc = protection.tenc().ok_or(Error::MissingBox("tenc"))?;
        let senc = traf.parse_senc(tenc.default_per_sample_iv_size, moof_pos)?;
        let samples = frag.full_samples(ti.trex.as_ref())?;
        let mdat = frag.mdat_mut().ok_or(Error::MissingBox("mdat"))?;

        let mut iv = [0u8; 16];
        copy_prefix(&mut iv, &tenc.default_constant_iv);
        for (i, sample) in samples.iter().enumerate() {
            if senc.ivs.len() == samples.len() {
                if senc.ivs[i].len() < 16 {
                    iv = [0; 16];
                }
                copy_prefix(&mut iv, &senc.ivs[i]);
            }
            let subsamples = match senc.subsamples.as_slice() {
                [] => None,
                all => Some(
                    all.get(i)
                        .ok_or(Error::Senc("fewer subsample entries than samples"))?
                        .as_slice(),
                ),
            };
            let data = &mut mdat.data[sample.data.clone()];
            match protection.scheme {
                Scheme::Cenc => decrypt_sample_cenc(data, key, &iv, subsamples)?,
                Scheme::Cbcs => decrypt_sample_cbcs(data, key, &iv, subsamples, tenc)?,
            }
        }
        removed += nth_traf_mut(frag, index)?.remove_encryption_boxes();
    }
    let moof = frag.moof_mut().ok_or(Error::MissingBox("moof"))?;
    removed += moof.remove_psshs();
    for trun in moof.trafs_mut().flat_map(|t| t.truns_mut()) {
        trun.data_offset = trun.data_offset.wrapping_sub(removed as i32);
    }
    let moof_pos = moof.pos;
    if let Some(mdat) = frag.mdat_mut().filter(|m| m.pos > moof_pos) {
        mdat.pos -= removed;
    }
    Ok(())
}

fn nth_traf(frag: &Fragment, index: usize) -> Result<&Traf> {
    frag.moof()
        .and_then(|m| m.trafs().nth(index))
        .ok_or(Error::MissingBox("traf"))
}

fn nth_traf_mut(frag: &mut Fragment, index: usize) -> Result<&mut Traf> {
    frag.moof_mut()
        .and_then(|m| m.trafs_mut().nth(index))
        .ok_or(Error::MissingBox("traf"))
}

/// Go's `copy(dst, src)`.
fn copy_prefix(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    dst[..n].copy_from_slice(&src[..n]);
}

/// The protected ranges of a sample: each subsample's protected bytes, or the whole
/// sample without subsamples. Errors instead of slicing past the sample.
fn protected_ranges(
    len: usize,
    subsamples: Option<&[SubSample]>,
) -> Result<Vec<std::ops::Range<usize>>> {
    let Some(subsamples) = subsamples else {
        return Ok(std::iter::once(0..len).collect());
    };
    let mut ranges = Vec::with_capacity(subsamples.len());
    let mut pos = 0u64;
    for (index, ss) in subsamples.iter().enumerate() {
        pos += u64::from(ss.clear);
        let end = pos + u64::from(ss.protected);
        if ss.protected > 0 {
            if end > len as u64 {
                return Err(Error::SubsampleOutOfRange { index, end, len });
            }
            ranges.push(pos as usize..end as usize);
        }
        pos = end;
    }
    Ok(ranges)
}

/// AES-128-CTR over the protected ranges; the counter runs on across subsamples
/// (mp4ff `CryptSampleCenc`).
pub fn decrypt_sample_cenc(
    sample: &mut [u8],
    key: &[u8; 16],
    iv: &[u8; 16],
    subsamples: Option<&[SubSample]>,
) -> Result<()> {
    let mut ctr = ctr::Ctr128BE::<Aes128>::new(key.into(), iv.into());
    for range in protected_ranges(sample.len(), subsamples)? {
        ctr.apply_keystream(&mut sample[range]);
    }
    Ok(())
}

/// AES-128-CBC over each protected range with `tenc`'s crypt/skip pattern, restarting
/// from `iv` for every subsample (mp4ff `DecryptSampleCbcs`). Without a skip pattern the
/// whole 16-byte blocks of each range are decrypted and the tail stays clear.
pub fn decrypt_sample_cbcs(
    sample: &mut [u8],
    key: &[u8; 16],
    iv: &[u8; 16],
    subsamples: Option<&[SubSample]>,
    tenc: &Tenc,
) -> Result<()> {
    let crypt = usize::from(tenc.default_crypt_byte_block) * 16;
    let skip = usize::from(tenc.default_skip_byte_block) * 16;
    for range in protected_ranges(sample.len(), subsamples)? {
        let data = &mut sample[range];
        let mut cbc = cbc::Decryptor::<Aes128>::new(key.into(), iv.into());
        if skip == 0 {
            let whole = data.len() & !0xf;
            decrypt_blocks(&mut cbc, &mut data[..whole]);
            continue;
        }
        let mut pos = 0;
        while data.len() - pos >= crypt {
            decrypt_blocks(&mut cbc, &mut data[pos..pos + crypt]);
            pos += crypt;
            if data.len() - pos < skip {
                break;
            }
            pos += skip;
        }
    }
    Ok(())
}

/// Decrypts `data` (a multiple of 16 bytes) in place, continuing the CBC chain.
fn decrypt_blocks(cbc: &mut cbc::Decryptor<Aes128>, data: &mut [u8]) {
    let (blocks, _) = aes::cipher::Array::slice_as_chunks_mut(data);
    cbc.decrypt_blocks(blocks);
}
