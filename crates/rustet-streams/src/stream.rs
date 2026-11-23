//! Fragment-by-fragment fMP4 rewriting: Widevine (CENC / CBCS) decryption and the clear-AAC passthrough.
//!
//! Each fragment is written as soon as it is processed, so the consumer starts reading before the input ends.
//!
//! Robustness:
//! - Init segment: Apple emits ftyp + moov, but pssh boxes can follow moov in some variants. Boxes are consumed
//!   until a moov is found; only ftyp and moov are kept.
//! - Fragments: moof (+ optional emsg / prft) + mdat. CMAF segment boxes (styp, sidx, ssix) between fragments
//!   are skipped.
//! - A fragment without a senc box is clear and passes through unchanged.

use bytes::Bytes;
use rustet_core::pipeline::StreamContext;
use rustet_mp4::{Children, FourCC, Fragment, InitSegment, Mp4Box, Tfdt, Trim, read_box_async};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, trace};

use crate::Error;

/// Read-ahead of the input.
const READ_BUFFER: usize = 1 << 20;
/// Apple emits at most a few boxes before moov; the bound stops a malformed stream from spinning.
const MAX_INIT_BOXES: usize = 64;
/// AAC-LC: 1024 PCM samples per frame.
const AAC_FRAME: u32 = 1024;
/// The timescale of standard AAC, used when the init segment declares none.
const DEFAULT_AUDIO_TIMESCALE: u64 = 44100;

type Input<'a> = BufReader<&'a mut (dyn AsyncRead + Send + Unpin)>;

/// Reads boxes until a moov, keeping only ftyp and moov. pssh and other top-level boxes are consumed but dropped: a
/// pssh in the output would make the browser try to set up a CDM for what is now clear content. Returns the init
/// segment and the stream offset after it.
async fn read_init_segment(r: &mut Input<'_>) -> Result<(InitSegment, u64), Error> {
    let mut init = InitSegment::default();
    let mut offset = 0;
    for _ in 0..MAX_INIT_BOXES {
        let b = read_box_async(r, offset)
            .await
            .map_err(Error::mp4("init segment"))?
            .ok_or(Error::NoMoov)?;
        offset += b.size();
        let kind = b.kind();
        if kind == FourCC::FTYP || kind == FourCC::MOOV {
            init.push(b);
        }
        if kind == FourCC::MOOV {
            return Ok((init, offset));
        }
    }
    Err(Error::NoMoov)
}

/// Reads one fragment (moof + optional emsg / prft + mdat). `None` at a clean end of input: the input ended
/// between fragments, or it ended exactly at a box boundary inside a fragment (that partial fragment is dropped).
async fn read_next_fragment(
    r: &mut Input<'_>,
    offset: &mut u64,
) -> Result<Option<Fragment>, Error> {
    let mut frag = Fragment::default();
    let mut has_moof = false;
    loop {
        let b = match read_box_async(r, *offset).await {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(None),
            Err(rustet_mp4::Error::Truncated) if !has_moof => return Ok(None),
            Err(e) => return Err(Error::mp4("read fragment")(e)),
        };
        *offset += b.size();
        match &b.kind().0 {
            b"moof" | b"emsg" | b"prft" => {
                frag.push(b);
                has_moof = true;
            }
            // An mdat without a moof cannot be placed; skip it.
            b"mdat" if !has_moof => {}
            b"mdat" => {
                frag.push(b);
                return Ok(Some(frag));
            }
            b"styp" | b"sidx" | b"ssix" => {}
            // Unexpected box inside a fragment: keep it to be safe. Before a moof: ignore it.
            _ if has_moof => frag.push(b),
            _ => {}
        }
    }
}

/// Decrypts an fMP4 stream fragment by fragment with the 16-byte content `key` and writes the clear stream to `w`.
///
/// Video fragments pass through with their timing untouched: their elst-aligned tfdts must not change, or Chrome
/// fails the append. Audio fragments get their IDs normalised and keep their absolute tfdts. On the seek path,
/// audio before [`StreamContext::seek_target`] is dropped to the sample, so the stream starts at the target and not
/// at the start of its (about 10 s long) fragment.
pub async fn decrypt_mp4_streaming(
    ctx: &StreamContext,
    r: &mut (dyn AsyncRead + Send + Unpin),
    key: &[u8],
    w: &mut (dyn AsyncWrite + Send + Unpin),
) -> Result<(), Error> {
    let mut r = BufReader::with_capacity(READ_BUFFER, r);
    let (mut init, mut offset) = read_init_segment(&mut r).await?;
    let info = rustet_mp4::decrypt_init(&mut init).map_err(Error::mp4("decrypt init"))?;
    trace!(tracks = ?info.tracks.iter().map(|t| (t.track_id, t.protection.as_ref().map(|p| p.scheme))).collect::<Vec<_>>(), "decrypt init");
    w.write_all(&init.to_bytes().map_err(Error::mp4("write init"))?)
        .await?;

    let is_video = init
        .moov()
        .is_some_and(|m| m.traks().any(|t| t.handler_type() == Some(FourCC::VIDE)));

    let mut seek_target = match is_video {
        true => None,
        false => ctx
            .seek_target
            .map(|t| (t * audio_timescale(&init) as f64) as u64),
    };

    let mut count = 0usize;
    loop {
        ctx.check().map_err(|_| Error::Cancelled)?;
        let Some(mut frag) = read_next_fragment(&mut r, &mut offset).await? else {
            debug!(fragments = count, "decrypt stream done");
            return Ok(());
        };
        count += 1;
        if !is_video {
            normalize_audio_frag(&mut frag);
        }
        match rustet_mp4::decrypt_fragment(&mut frag, &info, key) {
            Ok(()) | Err(rustet_mp4::Error::NoSenc) => {}
            Err(e) => return Err(Error::mp4("decrypt fragment")(e)),
        }
        // After the decryption, which needs every sample in place.
        if let Some(target) = seek_target {
            match frag.trim_start(target, AAC_FRAME) {
                Trim::Before => continue,
                Trim::Kept => seek_target = None,
            }
        }
        w.write_all(&frag.to_bytes().map_err(Error::mp4("write fragment"))?)
            .await?;
    }
}

/// Copies an fMP4 stream of clear AAC (content Apple serves without content-level encryption) to `w`. The init
/// segment loses its pssh, so the browser does not try to set up a CDM for clear content and block playback.
///
/// Every fragment gets a version-1 tfdt on a continuous timeline (Apple's AAC segments have none of their own),
/// track ID 1 and sample description index 1. On the seek path the timeline starts at
/// [`StreamContext::seek_actual_start`], and fragments that end at or before [`StreamContext::seek_target`] are
/// dropped, so audio starts within one fragment of the requested position.
pub async fn passthrough_streaming(
    ctx: &StreamContext,
    r: &mut (dyn AsyncRead + Send + Unpin),
    w: &mut (dyn AsyncWrite + Send + Unpin),
) -> Result<(), Error> {
    let mut r = BufReader::with_capacity(READ_BUFFER, r);
    let (init, _) = read_init_segment(&mut r).await?;
    w.write_all(&init.to_bytes().map_err(Error::mp4("write init"))?)
        .await?;

    let timescale = audio_timescale(&init);
    let mut accumulated = match ctx.seek_actual_start {
        Some(start) if start > 0.0 => (start * timescale as f64) as u64,
        _ => 0,
    };
    // Compare fragment end times, so the first fragment that ends after the target (it holds the target) is kept.
    let mut seek_target = ctx.seek_target.map(|t| (t * timescale as f64) as u64);

    // The stream offset restarts at 0 here, as in the Go engine: positions only feed box bookkeeping.
    let mut offset = 0;
    loop {
        ctx.check().map_err(|_| Error::Cancelled)?;
        let Some(mut frag) = read_next_fragment(&mut r, &mut offset).await? else {
            return Ok(());
        };
        if let Some(target) = seek_target {
            let dur = frag_duration_ticks(&frag);
            if dur > 0 && accumulated + dur <= target {
                accumulated += dur;
                continue;
            }
            seek_target = None;
        }
        accumulated = patch_frag_tfdt(&mut frag, accumulated);
        w.write_all(&frag.to_bytes().map_err(Error::mp4("write fragment"))?)
            .await?;
    }
}

/// Writes the track length into an init segment whose moov says 0, as Apple's CMAF moov does. libvlc's MP4
/// demuxer reads every fragment before it plays a moov without a length, so a file that is still downloading
/// would only start once it is complete. mvhd, tkhd and mdhd get the length (as the lossless path's
/// `patch_moov_duration` writes it); every other byte stays. MSE ignores these fields: the renderer sets the
/// `MediaSource` duration itself. Data that is not a whole init segment, a moov that has a length, or a length
/// of 0 come back unchanged.
pub(crate) fn patch_init_duration(data: Bytes, duration_ms: u64) -> Bytes {
    if duration_ms == 0 {
        return data;
    }
    let mut init = InitSegment::default();
    let mut at = 0;
    while at < data.len() {
        match Mp4Box::decode(&data[at..], at as u64) {
            Ok((b, used)) => {
                init.push(b);
                at += used;
            }
            Err(_) => return data,
        }
    }
    let Some(moov) = init.moov_mut() else {
        return data;
    };
    let Some(mvhd) = moov.mvhd_mut().filter(|m| m.duration() == 0) else {
        return data;
    };
    let timescale = match mvhd.timescale() {
        0 => 1000,
        ts => u64::from(ts),
    };
    let total = duration_ms * timescale / 1000;
    mvhd.set_duration(total);
    for trak in moov.traks_mut() {
        if let Some(tkhd) = trak.tkhd_mut() {
            tkhd.set_duration(total);
        }
        if let Some(mdhd) = trak.mdhd_mut() {
            let ticks = duration_ms * u64::from(mdhd.timescale()) / 1000;
            mdhd.set_duration(ticks);
        }
    }
    init.to_bytes().map_or(data, Bytes::from)
}

/// The timescale of the first track that declares one, else 44100.
fn audio_timescale(init: &InitSegment) -> u64 {
    init.moov()
        .and_then(|m| {
            m.traks()
                .filter_map(|t| t.mdhd())
                .map(|m| m.timescale())
                .find(|&t| t > 0)
        })
        .map_or(DEFAULT_AUDIO_TIMESCALE, u64::from)
}

/// The duration of `frag` in timescale ticks. The default sample duration comes from tfhd (ISO 14496-12), not the
/// stream timescale: Apple's AAC segments carry tfhd default duration 1024 and no per-sample durations, so using
/// the timescale (44100) would make a ~10 s fragment ~430 s long.
fn frag_duration_ticks(frag: &Fragment) -> u64 {
    let Some(traf) = frag.moof().and_then(|m| m.traf()) else {
        return 0;
    };
    let Some(trun) = traf.trun() else { return 0 };
    let default = traf
        .tfhd()
        .filter(|t| t.has_default_sample_duration() && t.default_sample_duration > 0)
        .map_or(AAC_FRAME, |t| t.default_sample_duration);
    trun.duration(default)
}

/// Sets the tfdt of `frag` to `accumulated` (version 1, inserted before each trun when missing), sets track ID
/// and sample description index to 1, moves the trun data offset by any moof size change, and returns the
/// timeline position after the fragment.
fn patch_frag_tfdt(frag: &mut Fragment, accumulated: u64) -> u64 {
    let Some(moof) = frag.moof_mut() else {
        return accumulated;
    };
    let old_size = moof.size();
    let Some(traf) = moof.traf_mut() else {
        return accumulated;
    };
    if let Some(tfdt) = traf.tfdt_mut() {
        tfdt.set_base_media_decode_time(accumulated);
        tfdt.version = 1;
    } else {
        let mut tfdt = Tfdt::new(accumulated);
        tfdt.version = 1;
        let children = std::mem::take(&mut traf.children);
        for child in children {
            if child.kind() == FourCC::TRUN {
                traf.children.push(Mp4Box::Tfdt(tfdt));
            }
            traf.children.push(child);
        }
    }
    normalize_ids(traf);
    let diff = moof.size() as i64 - old_size as i64;
    if diff != 0
        && let Some(trun) = moof.traf_mut().and_then(|t| t.trun_mut())
        && trun.has_data_offset()
    {
        trun.data_offset = trun.data_offset.wrapping_add(diff as i32);
    }
    accumulated + frag_duration_ticks(frag)
}

/// Rewrites the fields MSE needs constant (track ID, sample description index, tfdt version 1) without moving the
/// decode time: a seek stream starting at segment N keeps its absolute tfdts, so the renderer's currentTime lands
/// inside the buffered range.
fn normalize_audio_frag(frag: &mut Fragment) {
    let Some(traf) = frag.moof_mut().and_then(|m| m.traf_mut()) else {
        return;
    };
    normalize_ids(traf);
    if let Some(tfdt) = traf.tfdt_mut() {
        tfdt.set_base_media_decode_time(tfdt.base_media_decode_time());
        tfdt.version = 1;
    }
}

fn normalize_ids(traf: &mut rustet_mp4::Traf) {
    if let Some(tfhd) = traf.tfhd_mut() {
        tfhd.track_id = 1;
        if tfhd.has_sample_description_index() {
            tfhd.sample_description_index = 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustet_mp4::{Moof, Tfhd, Traf};

    /// The durations and timescales of the first moov in `data`: mvhd, then (tkhd, mdhd) per trak.
    struct Durations {
        mvhd: (u64, u64),
        traks: Vec<(u64, (u64, u64))>,
    }

    fn durations(data: &[u8]) -> Durations {
        let mut at = 0;
        while at < data.len() {
            let (b, used) = Mp4Box::decode(&data[at..], at as u64).unwrap();
            if let Mp4Box::Moov(m) = &b {
                let mvhd = m.mvhd().unwrap();
                let traks = m.traks().map(|t| {
                    let mdhd = t.mdhd().unwrap();
                    (
                        t.tkhd().unwrap().duration(),
                        (mdhd.duration(), u64::from(mdhd.timescale())),
                    )
                });
                return Durations {
                    mvhd: (mvhd.duration(), u64::from(mvhd.timescale())),
                    traks: traks.collect(),
                };
            }
            at += used;
        }
        panic!("no moov");
    }

    // libvlc starts a growing AAC file only once it is complete when the moov says 0, and Apple's says 0.
    // The patch must leave every other byte alone: the encrypted init still has to decrypt.
    #[test]
    fn init_duration_is_written_and_nothing_else_changes() {
        let fixture = include_bytes!("../tests/data/cbcs_audio.in.mp4");
        let mut init_len = 0;
        loop {
            let (b, used) = Mp4Box::decode(&fixture[init_len..], init_len as u64).unwrap();
            init_len += used;
            if b.kind() == FourCC::MOOV {
                break;
            }
        }
        let init = Bytes::copy_from_slice(&fixture[..init_len]);
        assert_eq!(
            durations(&init).mvhd.0,
            0,
            "the fixture's moov says 0, like Apple's"
        );

        let patched = patch_init_duration(init.clone(), 30_000);
        assert_eq!(patched.len(), init.len());
        let d = durations(&patched);
        assert_eq!(d.mvhd.0, 30 * d.mvhd.1);
        assert!(!d.traks.is_empty());
        for (tkhd, (mdhd, mdhd_ts)) in d.traks {
            assert_eq!((tkhd, mdhd), (d.mvhd.0, 30 * mdhd_ts));
        }
        let changed = init
            .iter()
            .zip(patched.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed <= 3 * 4,
            "{changed} bytes changed: only the three duration fields may"
        );

        assert_eq!(
            patch_init_duration(patched.clone(), 60_000),
            patched,
            "a moov with a length keeps it"
        );
        assert_eq!(
            patch_init_duration(init.clone(), 0),
            init,
            "no length known: unchanged"
        );
        let media = Bytes::copy_from_slice(&fixture[init_len..init_len + 64]);
        assert_eq!(
            patch_init_duration(media.clone(), 30_000),
            media,
            "not an init segment: unchanged"
        );
    }

    #[test]
    fn normalize_audio_frag_keeps_decode_time() {
        let traf = Traf {
            children: vec![Tfhd::new(7).into(), Tfdt::new(441_000).into()],
        };
        let mut frag = Fragment {
            children: vec![
                Moof {
                    pos: 0,
                    children: vec![traf.into()],
                }
                .into(),
            ],
        };
        normalize_audio_frag(&mut frag);
        let traf = frag.moof().unwrap().traf().unwrap();
        let tfdt = traf.tfdt().unwrap();
        assert_eq!(
            tfdt.base_media_decode_time(),
            441_000,
            "seek streams need absolute decode times"
        );
        assert_eq!((traf.tfhd().unwrap().track_id, tfdt.version), (1, 1));
    }
}
