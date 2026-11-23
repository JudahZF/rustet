//! Synthetic init segments and fragments for tests in other crates (feature `test-util`).
//! They mirror mp4ff's `CreateEmptyInit` + `AddEmptyTrack` and `CreateFragment` +
//! `AddFullSample`, but are not byte-identical to them.

use crate::boxes::{
    Container, Mdat, Mfhd, Moof, Moov, Mp4Box, RawBox, Tfdt, Tfhd, Traf, Trak, Trex, Trun,
    TrunSample,
};
use crate::{FourCC, Fragment, InitSegment, Result};

/// Media type of a synthetic track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Audio,
    Video,
}

fn raw(kind: &[u8; 4], payload: Vec<u8>) -> Mp4Box {
    Mp4Box::Raw(RawBox {
        kind: FourCC(*kind),
        payload,
    })
}

/// A typed box, built by decoding its bytes so the payload is validated.
fn decoded(kind: &[u8; 4], payload: Vec<u8>) -> Result<Mp4Box> {
    Ok(Mp4Box::decode(&wrap(kind, payload), 0)?.0)
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

impl InitSegment {
    /// `ftyp` + `moov` with one empty track (ID 1, no sample entries) and its `trex`.
    pub fn single_track(timescale: u32, media: MediaType) -> Result<Self> {
        let mut ftyp = b"cmfc".to_vec();
        ftyp.extend_from_slice(&[0, 0, 0, 0]);
        ftyp.extend_from_slice(b"dashiso6");

        // mvhd v0: times 0, timescale 1000, duration 0, rate 1.0, volume 1.0, matrix, next track 2.
        let mut mvhd = words(&[0, 0, 0, 1000, 0, 0x0001_0000]);
        mvhd.extend_from_slice(&[0x01, 0x00, 0, 0]);
        mvhd.extend_from_slice(&[0; 8]);
        mvhd.extend_from_slice(&words(&[
            0x0001_0000,
            0,
            0,
            0,
            0x0001_0000,
            0,
            0,
            0,
            0x4000_0000,
        ]));
        mvhd.extend_from_slice(&[0; 24]);
        mvhd.extend_from_slice(&words(&[2]));

        // tkhd v0, flags enabled|in-movie|in-preview, track 1.
        let volume: u32 = if media == MediaType::Audio {
            0x0100_0000
        } else {
            0
        };
        let mut tkhd = words(&[7, 0, 0, 1, 0, 0, 0, 0, 0, volume]);
        tkhd.extend_from_slice(&words(&[
            0x0001_0000,
            0,
            0,
            0,
            0x0001_0000,
            0,
            0,
            0,
            0x4000_0000,
            0,
            0,
        ]));

        // mdhd v0 with language "und".
        let mdhd = words(&[0, 0, 0, timescale, 0, 0x55c4_0000]);
        let (handler, name, media_header) = match media {
            MediaType::Audio => (b"soun", "SoundHandler", raw(b"smhd", words(&[0, 0]))),
            MediaType::Video => (b"vide", "VideoHandler", raw(b"vmhd", words(&[1, 0, 0]))),
        };
        let mut hdlr = words(&[0, 0]);
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.extend_from_slice(name.as_bytes());
        hdlr.push(0);

        let mut dref = words(&[0, 1]);
        dref.extend_from_slice(&words(&[12]));
        dref.extend_from_slice(b"url ");
        dref.extend_from_slice(&words(&[1]));

        let stbl = Container::new(
            FourCC::STBL,
            vec![
                decoded(b"stsd", words(&[0, 0]))?,
                raw(b"stts", words(&[0, 0])),
                raw(b"stsc", words(&[0, 0])),
                raw(b"stsz", words(&[0, 0, 0])),
                raw(b"stco", words(&[0, 0])),
            ],
        );
        let minf = Container::new(
            FourCC::MINF,
            vec![media_header, raw(b"dinf", wrap(b"dref", dref)), stbl.into()],
        );
        let mdia = Container::new(
            FourCC::MDIA,
            vec![
                decoded(b"mdhd", mdhd)?,
                decoded(b"hdlr", hdlr)?,
                minf.into(),
            ],
        );
        let trak = Trak {
            children: vec![decoded(b"tkhd", tkhd)?, mdia.into()],
        };
        let mvex = Container::new(FourCC::MVEX, vec![Trex::new(1).into()]);
        let moov = Moov {
            children: vec![decoded(b"mvhd", mvhd)?, mvex.into(), trak.into()],
        };
        Ok(Self {
            children: vec![raw(b"ftyp", ftyp), moov.into()],
        })
    }
}

/// A complete box (header + payload) as bytes.
fn wrap(kind: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut out = (payload.len() as u32 + 8).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(&payload);
    out
}

impl Fragment {
    /// `moof` (mfhd, traf with a default-base-is-moof tfhd, a tfdt and an empty
    /// [`Trun::new`]) followed by an empty `mdat` (mp4ff `CreateFragment`).
    pub fn new(sequence_number: u32, track_id: u32) -> Self {
        let traf = Traf {
            children: vec![
                Tfhd::new(track_id).into(),
                Tfdt::new(0).into(),
                Trun::new().into(),
            ],
        };
        let moof = Moof {
            pos: 0,
            children: vec![Mfhd::new(sequence_number).into(), traf.into()],
        };
        Self {
            children: vec![moof.into(), Mdat::default().into()],
        }
    }

    /// Appends a sample to the first trun and its bytes to the mdat; the first sample
    /// also sets the tfdt (mp4ff `AddFullSample`). `sample.size` should be `data.len()`.
    pub fn add_full_sample(&mut self, sample: TrunSample, decode_time: u64, data: &[u8]) {
        if let Some(traf) = self.moof_mut().and_then(Moof::traf_mut) {
            if traf.trun().is_some_and(|t| t.samples.is_empty())
                && let Some(tfdt) = traf.tfdt_mut()
            {
                tfdt.set_base_media_decode_time(decode_time);
            }
            if let Some(trun) = traf.trun_mut() {
                trun.samples.push(sample);
            }
        }
        if let Some(mdat) = self.mdat_mut() {
            mdat.data.extend_from_slice(data);
        }
    }
}
