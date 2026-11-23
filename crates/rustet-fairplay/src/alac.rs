//! Reading and decrypting a FairPlay CBCS fMP4 (ALAC) through the DRM connection (Go
//! `engine/utils/alacstream`).
//!
//! The DRM connection speaks the protocol of the wrapper's `handle()`:
//!
//! ```text
//! repeat:  [u8 len][adamID] [u8 len][keyURI]          select a key
//!          ([u32 LE size][bytes] → [decrypted bytes])*  decrypt
//!          [u32 LE 0]                                   end of this key (SwitchKeys)
//! ```
//!
//! An empty adamID ends the session, so [`close`] is `SwitchKeys` plus a zero length byte.

use std::io;

use rustet_mp4::{
    Children, Container, DecryptInfo, FourCC, Fragment, Frma, InitSegment, Mp4Box, Scheme,
    SubSample, Tenc, TrackInfo, Trex, decrypt_init, is_encryption_group, read_box_async,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Boxes read before giving up on finding the init segment's `moov`.
const MAX_INIT_BOXES: usize = 64;

/// Errors of the ALAC fragment reader and the socket decryption.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Mp4(#[from] rustet_mp4::Error),
    /// Reading from or writing to the DRM connection failed.
    #[error("drm connection: {0}")]
    Drm(#[source] io::Error),
    #[error("no moov box found in init segment")]
    NoMoov,
    #[error("mdat without a preceding moof (box ends @ offset {0})")]
    MdatWithoutMoof(u64),
    #[error("more than 1 track found")]
    TooManyTracks,
    #[error("children in stsd are not of the same type")]
    MixedSampleEntries,
    #[error("expected only 1 or 2 entries in stsd, got {0}")]
    SampleEntryCount(usize),
    #[error("could not find decryption info for track {0}")]
    UnknownTrack(u32),
    #[error("track {0}: incomplete sinf (schm/schi/tenc)")]
    IncompleteSinf(u32),
    #[error("scheme type cenc not supported")]
    UnsupportedScheme,
    #[error("subsample {index} exceeds sample ({pos}+{protected} > {len})")]
    SubsampleOutOfRange {
        index: usize,
        pos: u64,
        protected: u32,
        len: usize,
    },
    #[error("senc has {entries} subsample entries for {samples} samples")]
    ShortSenc { entries: usize, samples: usize },
    /// The DRM backend decrypted without an error, but the result is not audio: Apple's
    /// library hands out an expired cached key as if it were valid.
    #[error("{bad} of {samples} decrypted samples are not ALAC frames (wrong or expired key)")]
    WrongKey { bad: usize, samples: usize },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

// ── Init segment ─────────────────────────────────────────────────────────────

/// Reads boxes from `r` until the `moov`. Only `ftyp` and `moov` are kept; `pssh` and any
/// other top-level box before or between them is consumed and dropped (Apple variants put
/// boxes there). Returns the init segment and the number of bytes consumed.
pub async fn read_init_segment<R: AsyncRead + Unpin>(r: &mut R) -> Result<(InitSegment, u64)> {
    let mut offset = 0;
    let mut init = InitSegment::default();
    for _ in 0..MAX_INIT_BOXES {
        let b = read_box_async(r, offset).await?.ok_or(Error::NoMoov)?;
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

/// Turns the encrypted sample entries into clear ones, removes the encryption sample groups
/// (`seig` / `seam` `sbgp` and `sgpd`; others such as `roll` stay) from every `stbl`, and
/// returns what [`decrypt_fragment`] needs.
pub fn transform_init(init: &mut InitSegment) -> Result<DecryptInfo> {
    let info = decrypt_init(init)?;
    let moov = init
        .moov_mut()
        .ok_or(rustet_mp4::Error::MissingBox("moov"))?;
    for stbl in moov.traks_mut().filter_map(|t| t.stbl_mut()) {
        stbl.children.retain(|c| match c {
            Mp4Box::Sbgp(b) => !is_encryption_group(b.grouping_type),
            Mp4Box::Sgpd(b) => !is_encryption_group(b.grouping_type),
            _ => true,
        });
    }
    Ok(info)
}

/// Removes what trips up downstream players and muxers, after [`transform_init`]:
///
/// - Apple's audio tracks have two identical sample entries (two IVs are needed to decrypt
///   the track); some programs (cuetools) reject more than one, so the second is dropped.
/// - Without `mvex`, players (VLC, mpv, ffmpeg) do not treat the file as fragmented and stop
///   after the first fragment, so one is added with a 4096-sample default duration.
pub fn sanitize_init(init: &mut InitSegment) -> Result<()> {
    let moov = init
        .moov_mut()
        .ok_or(rustet_mp4::Error::MissingBox("moov"))?;
    if moov.traks().count() > 1 {
        return Err(Error::TooManyTracks);
    }
    let trak = moov
        .traks_mut()
        .next()
        .ok_or(rustet_mp4::Error::MissingBox("trak"))?;
    let track_id = trak
        .track_id()
        .ok_or(rustet_mp4::Error::MissingBox("tkhd"))?;
    let stsd = trak
        .stsd_mut()
        .ok_or(rustet_mp4::Error::MissingBox("stsd"))?;
    match stsd.entries.as_slice() {
        [a, b] if a.kind() != b.kind() => return Err(Error::MixedSampleEntries),
        [_, _] => stsd.entries.truncate(1),
        [] | [_] => {}
        more => return Err(Error::SampleEntryCount(more.len())),
    }
    if moov.mvex().is_none() {
        let trex = Trex {
            default_sample_duration: 4096,
            ..Trex::new(track_id)
        };
        moov.push(Container::new(FourCC::MVEX, vec![trex.into()]));
    }
    Ok(())
}

// ── Fragments ────────────────────────────────────────────────────────────────

/// Reads the next fragment from `r`, which is at stream position `offset`: the `moof` and any
/// `emsg` / `prft` boxes up to and including the next `mdat`. Other boxes are skipped.
///
/// Returns `None` at the end of the input (also when it ends between a `moof` and its
/// `mdat`), else the fragment and the position after its `mdat`.
pub async fn read_next_fragment<R: AsyncRead + Unpin>(
    r: &mut R,
    mut offset: u64,
) -> Result<Option<(Fragment, u64)>> {
    let mut frag = Fragment::default();
    loop {
        let Some(b) = read_box_async(r, offset).await? else {
            return Ok(None);
        };
        offset += b.size();
        let kind = b.kind();
        if kind == FourCC::MOOF || kind == FourCC::EMSG || kind == FourCC::PRFT {
            frag.push(b);
        } else if kind == FourCC::MDAT {
            frag.push(b);
            break;
        } else {
            tracing::warn!(%kind, "ignoring box mid-stream");
        }
    }
    if frag.moof().is_none() {
        return Err(Error::MdatWithoutMoof(offset));
    }
    Ok(Some((frag, offset)))
}

/// The decryption info of a track. Like Go's `map[trackID]` built from the track infos, the
/// last entry wins; for Apple's two-entry audio tracks that is the second entry, which has no
/// `trex`.
fn track_info(info: &DecryptInfo, track_id: u32) -> Option<&TrackInfo> {
    info.tracks.iter().rev().find(|t| t.track_id == track_id)
}

/// Decrypts every encrypted track of `frag` in place through the DRM connection `conn`, then
/// removes the encryption boxes and moof `pssh` boxes and moves every trun data offset back
/// by the removed size. The key must already be selected on `conn`.
pub async fn decrypt_fragment<S: AsyncRead + AsyncWrite + Unpin>(
    frag: &mut Fragment,
    info: &DecryptInfo,
    conn: &mut S,
) -> Result<()> {
    let traf_count = frag
        .moof()
        .ok_or(rustet_mp4::Error::MissingBox("moof"))?
        .trafs()
        .count();
    let mut removed = 0u64;
    for index in 0..traf_count {
        let traf = frag
            .moof()
            .and_then(|m| m.trafs().nth(index))
            .ok_or(rustet_mp4::Error::MissingBox("traf"))?;
        let track_id = traf
            .tfhd()
            .ok_or(rustet_mp4::Error::MissingBox("tfhd"))?
            .track_id;
        let ti = track_info(info, track_id).ok_or(Error::UnknownTrack(track_id))?;
        let Some(protection) = &ti.protection else {
            continue; // clear track
        };
        let tenc = protection.tenc().ok_or(Error::IncompleteSinf(track_id))?;
        if protection.scheme != Scheme::Cbcs {
            return Err(Error::UnsupportedScheme);
        }
        // sbgp / sgpd are ignored: on Apple's streams they only signal key rotation, which the
        // caller handles by switching keys on the connection.
        let senc = traf
            .senc()
            .ok_or(rustet_mp4::Error::NoSenc)?
            .parse(tenc.default_per_sample_iv_size)?;
        let samples = frag.full_samples(ti.trex.as_ref())?;
        let mdat = frag
            .mdat_mut()
            .ok_or(rustet_mp4::Error::MissingBox("mdat"))?;

        if !senc.subsamples.is_empty() && senc.subsamples.len() < samples.len() {
            return Err(Error::ShortSenc {
                entries: senc.subsamples.len(),
                samples: samples.len(),
            });
        }
        for (i, sample) in samples.iter().enumerate() {
            let subsamples = senc.subsamples.get(i).map_or(&[][..], Vec::as_slice);
            decrypt_sample(&mut mdat.data[sample.data.clone()], conn, subsamples, tenc).await?;
        }
        let alac = protection
            .sinf
            .first::<Frma>()
            .is_some_and(|f| f.data_format == FourCC::ALAC);
        if alac {
            let bad = samples
                .iter()
                .filter(|s| !starts_like_alac(&mdat.data[s.data.clone()]))
                .count();
            // A wrong key fails every sample; a real stream fails none.
            if bad * 2 > samples.len() {
                return Err(Error::WrongKey {
                    bad,
                    samples: samples.len(),
                });
            }
        }

        removed += frag
            .moof_mut()
            .and_then(|m| m.trafs_mut().nth(index))
            .ok_or(rustet_mp4::Error::MissingBox("traf"))?
            .remove_encryption_boxes();
    }
    let moof = frag
        .moof_mut()
        .ok_or(rustet_mp4::Error::MissingBox("moof"))?;
    removed += moof.remove_psshs();
    for trun in moof.trafs_mut().flat_map(|t| t.truns_mut()) {
        trun.data_offset = trun.data_offset.wrapping_sub(removed as i32);
    }
    Ok(())
}

/// Whether `sample` starts like an ALAC frame: a 3-bit element tag, a 4-bit instance tag, then
/// 12 header bits that are always zero (Apple's decoder refuses anything else). Bytes
/// decrypted with a wrong key pass 1 time in 4096. A sample under one AES block is never
/// encrypted, so it passes.
fn starts_like_alac(sample: &[u8]) -> bool {
    sample.len() < 16 || matches!(sample, [a, 0, c, ..] if a & 0x01 == 0 && c & 0xE0 == 0)
}

/// Decrypts one cbcs sample in place. Without subsamples the whole sample is protected.
async fn decrypt_sample<S: AsyncRead + AsyncWrite + Unpin>(
    sample: &mut [u8],
    conn: &mut S,
    subsamples: &[SubSample],
    tenc: &Tenc,
) -> Result<()> {
    let crypt = usize::from(tenc.default_crypt_byte_block) * 16;
    let skip = usize::from(tenc.default_skip_byte_block) * 16;
    if subsamples.is_empty() {
        return decrypt_raw(sample, conn, crypt, skip)
            .await
            .map_err(Error::Drm);
    }
    let mut pos = 0u64;
    for (index, ss) in subsamples.iter().enumerate() {
        pos += u64::from(ss.clear);
        if ss.protected == 0 {
            continue;
        }
        // Sizes come from the file: a malformed senc is an error, not a slice panic.
        let end = pos + u64::from(ss.protected);
        if end > sample.len() as u64 {
            return Err(Error::SubsampleOutOfRange {
                index,
                pos,
                protected: ss.protected,
                len: sample.len(),
            });
        }
        decrypt_raw(&mut sample[pos as usize..end as usize], conn, crypt, skip)
            .await
            .map_err(Error::Drm)?;
        pos = end;
    }
    Ok(())
}

/// Sends one protected range to the decryptor and reads it back in place. With a skip
/// pattern only the crypt blocks travel (stripe encryption, video); without one the range's
/// whole 16-byte blocks do (full encryption, Apple Music ALAC) and the tail stays clear.
async fn decrypt_raw<S: AsyncRead + AsyncWrite + Unpin>(
    data: &mut [u8],
    conn: &mut S,
    crypt: usize,
    skip: usize,
) -> io::Result<()> {
    if skip == 0 {
        // The tail would come back unchanged anyway; truncating keeps packets block-sized.
        // A range under 16 bytes still sends its zero size, as the Go code does.
        let len = data.len() & !0xf;
        conn.write_all(&packet_len(len)?).await?;
        conn.write_all(&data[..len]).await?;
        conn.flush().await?;
        conn.read_exact(&mut data[..len]).await?;
        return Ok(());
    }

    let size = data.len();
    if size < crypt {
        return Ok(()); // too small to hold an encrypted block
    }
    let count = (size - crypt) / (crypt + skip) + 1;
    conn.write_all(&packet_len(count * crypt)?).await?;
    for block in stripes(size, crypt, skip) {
        conn.write_all(&data[block]).await?;
    }
    conn.flush().await?;
    for block in stripes(size, crypt, skip) {
        conn.read_exact(&mut data[block]).await?;
    }
    Ok(())
}

/// The crypt-block ranges of a stripe-encrypted range of `size` bytes.
fn stripes(size: usize, crypt: usize, skip: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos > size || size - pos < crypt {
            return None;
        }
        let block = pos..pos + crypt;
        pos += crypt;
        // Not enough left for a skip block: end after this one.
        pos = if size - pos < skip {
            usize::MAX
        } else {
            pos + skip
        };
        Some(block)
    })
}

fn packet_len(len: usize) -> io::Result<[u8; 4]> {
    u32::try_from(len)
        .map(u32::to_le_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sample too large"))
}

// ── Connection protocol ──────────────────────────────────────────────────────

/// Ends the current key on the connection; the next header selects a new key.
pub async fn switch_keys<W: AsyncWrite + Unpin>(conn: &mut W) -> io::Result<()> {
    conn.write_all(&[0; 4]).await
}

/// Sends an adamID or key URI as `[u8 len][bytes]`.
pub async fn send_string<W: AsyncWrite + Unpin>(conn: &mut W, s: &str) -> io::Result<()> {
    conn.write_all(&[short_len(s)?]).await?;
    conn.write_all(s.as_bytes()).await
}

/// The one-byte length prefix of a short string. Go wrapped longer strings silently, which
/// desynchronises the server; here they are an error.
pub(crate) fn short_len(s: &str) -> io::Result<u8> {
    u8::try_from(s.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "string longer than 255 bytes"))
}

/// Ends the session (a key switch, then an empty adamID, so the server's loops stop) and
/// shuts the connection down. Write it to the raw connection: anything still buffered is
/// meant to be dropped.
pub async fn close<W: AsyncWrite + Unpin>(conn: &mut W) -> io::Result<()> {
    let res = conn.write_all(&[0; 5]).await;
    let _ = conn.shutdown().await;
    res
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cbc::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
    use rustet_mp4::decrypt_sample_cbcs;
    use tokio::io::DuplexStream;

    use super::*;

    const KEY: [u8; 16] = [0x5a; 16];
    const IV: [u8; 16] = [0xd1; 16];

    /// A version 1 tenc with the crypt:skip pattern and the constant IV.
    fn tenc(crypt: u8, skip: u8) -> Tenc {
        let mut payload = vec![1, 0, 0, 0, 0, (crypt << 4) | skip, 1, 0];
        payload.extend_from_slice(&[0; 16]);
        payload.push(16);
        payload.extend_from_slice(&IV);
        let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(b"tenc");
        b.extend_from_slice(&payload);
        match Mp4Box::decode(&b, 0).unwrap().0 {
            Mp4Box::Tenc(t) => t,
            other => panic!("not a tenc: {other:?}"),
        }
    }

    /// Deterministic filler bytes (xorshift).
    fn bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// cbcs encryption: CBC from the constant IV over each protected range, chained across
    /// the crypt blocks of a stripe pattern.
    fn encrypt(sample: &[u8], subsamples: &[SubSample], tenc: &Tenc) -> Vec<u8> {
        let crypt = usize::from(tenc.default_crypt_byte_block) * 16;
        let skip = usize::from(tenc.default_skip_byte_block) * 16;
        let mut out = sample.to_vec();
        let ranges: Vec<_> = if subsamples.is_empty() {
            std::iter::once(0..out.len()).collect()
        } else {
            let mut pos = 0;
            subsamples
                .iter()
                .filter_map(|ss| {
                    pos += usize::from(ss.clear);
                    let r = pos..pos + ss.protected as usize;
                    pos = r.end;
                    (ss.protected > 0).then_some(r)
                })
                .collect()
        };
        for r in ranges {
            let data = &mut out[r];
            let mut enc = cbc::Encryptor::<aes::Aes128>::new(&KEY.into(), &IV.into());
            let mut blocks: Vec<std::ops::Range<usize>> = Vec::new();
            if skip == 0 {
                blocks.extend((0..data.len() / 16).map(|b| b * 16..b * 16 + 16));
            } else {
                let mut pos = 0;
                while data.len() - pos >= crypt {
                    blocks.extend((pos..pos + crypt).step_by(16).map(|b| b..b + 16));
                    pos += crypt;
                    if data.len() - pos < skip {
                        break;
                    }
                    pos += skip;
                }
            }
            for b in blocks {
                let block: &mut [u8; 16] = (&mut data[b]).try_into().unwrap();
                enc.encrypt_block(block.into());
            }
        }
        // Cross-check the helper against rustet-mp4's key-based decryption.
        let mut check = out.clone();
        let subs = (!subsamples.is_empty()).then_some(subsamples);
        decrypt_sample_cbcs(&mut check, &KEY, &IV, subs, tenc).unwrap();
        assert_eq!(check, sample, "test encryption helper is wrong");
        out
    }

    /// Answers `[u32 LE size][bytes]` packets as the FairPlay decryptor does: AES-CBC over
    /// the packet from the constant IV. Returns the client end and a packet counter.
    fn fake_decryptor() -> (DuplexStream, Arc<AtomicUsize>) {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let packets = Arc::new(AtomicUsize::new(0));
        let count = packets.clone();
        tokio::spawn(async move {
            loop {
                let mut size = [0; 4];
                if server.read_exact(&mut size).await.is_err() {
                    return;
                }
                let mut buf = vec![0; u32::from_le_bytes(size) as usize];
                if server.read_exact(&mut buf).await.is_err() {
                    return;
                }
                let mut dec = cbc::Decryptor::<aes::Aes128>::new(&KEY.into(), &IV.into());
                for block in buf.as_chunks_mut::<16>().0 {
                    dec.decrypt_block(block.into());
                }
                count.fetch_add(1, Ordering::SeqCst);
                if server.write_all(&buf).await.is_err() {
                    return;
                }
            }
        });
        (client, packets)
    }

    #[test]
    fn stripes_match_the_pattern_walk() {
        // 1:9 pattern over 700 bytes: a crypt block every 160 bytes, the last one at 640.
        let got: Vec<_> = stripes(700, 16, 144).collect();
        let starts: Vec<_> = got.iter().map(|r| r.start).collect();
        assert_eq!(starts, [0, 160, 320, 480, 640]);
        assert!(got.iter().all(|r| r.len() == 16));
        // Exactly crypt + skip: the next crypt block would start at the end.
        assert_eq!(stripes(32, 16, 16).count(), 1);
        assert_eq!(stripes(15, 16, 144).count(), 0);
    }

    /// ALAC: whole-sample encryption (pattern 0:0), no subsamples; the tail that is not a
    /// whole AES block stays clear. One packet per sample.
    #[tokio::test]
    async fn full_sample_alac() {
        let tenc = tenc(0, 0);
        for size in [16, 333, 4096] {
            let plain = bytes(size, size as u64);
            let mut enc = encrypt(&plain, &[], &tenc);
            let (mut conn, packets) = fake_decryptor();
            decrypt_sample(&mut enc, &mut conn, &[], &tenc)
                .await
                .unwrap();
            assert_eq!(enc, plain, "size {size}");
            assert_eq!(packets.load(Ordering::SeqCst), 1, "size {size}");
        }
    }

    /// A sample under one AES block still sends its (zero) size, as the Go code does, and
    /// comes back unchanged.
    #[tokio::test]
    async fn short_sample_sends_a_zero_size() {
        let mut wire = Vec::new();
        let mut conn = tokio::io::join(&[][..], &mut wire);
        let mut sample = bytes(15, 1);
        let plain = sample.clone();
        decrypt_sample(&mut sample, &mut conn, &[], &tenc(0, 0))
            .await
            .unwrap();
        assert_eq!(sample, plain);
        assert_eq!(wire, [0, 0, 0, 0]);
    }

    /// Video: 1:9 stripe pattern inside protected subsamples, clear headers between.
    #[tokio::test]
    async fn stripe_pattern_with_subsamples() {
        let tenc = tenc(1, 9);
        let subs = [
            SubSample {
                clear: 5,
                protected: 700,
            },
            SubSample {
                clear: 0,
                protected: 0,
            },
            SubSample {
                clear: 33,
                protected: 170,
            },
        ];
        let plain = bytes(5 + 700 + 33 + 170, 7);
        let mut enc = encrypt(&plain, &subs, &tenc);
        assert_ne!(enc, plain);
        let (mut conn, packets) = fake_decryptor();
        decrypt_sample(&mut enc, &mut conn, &subs, &tenc)
            .await
            .unwrap();
        assert_eq!(enc, plain);
        assert_eq!(
            packets.load(Ordering::SeqCst),
            2,
            "one packet per protected subsample"
        );
    }

    /// A senc subsample that claims more protected bytes than the sample holds is an error,
    /// not a panic.
    #[tokio::test]
    async fn oversized_subsample_is_an_error() {
        let (mut conn, _) = fake_decryptor();
        let subs = [SubSample {
            clear: 8,
            protected: 1000,
        }];
        let err = decrypt_sample(&mut [0; 64], &mut conn, &subs, &tenc(1, 0))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::SubsampleOutOfRange { index: 0, .. }),
            "{err}"
        );
    }
}
