//! Field-level behaviour on hand-built boxes: mp4ff's edge cases and the error paths.

use rustet_mp4::{
    BoxHeader, Children, Error, FourCC, Fragment, InitSegment, MediaType, Mp4Box, Senc, SubSample,
    Tfdt, Trim, Trun, TrunSample,
};

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32 + 8).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn decode(bytes: &[u8]) -> Result<Mp4Box, Error> {
    Mp4Box::decode(bytes, 0).map(|(b, _)| b)
}

fn senc(flags: u32, count: u32, raw: &[u8]) -> Senc {
    let mut payload = flags.to_be_bytes().to_vec();
    payload.extend_from_slice(&count.to_be_bytes());
    payload.extend_from_slice(raw);
    match decode(&boxed(b"senc", &payload)).unwrap() {
        Mp4Box::Senc(s) => s,
        other => panic!("not a senc: {other:?}"),
    }
}

#[test]
fn box_header_forms() {
    assert_eq!(BoxHeader::parse(&[0, 0, 0, 8, b'f']).unwrap(), None);
    let h = BoxHeader::parse(&boxed(b"free", &[])).unwrap().unwrap();
    assert_eq!((h.kind, h.size, h.header_len), (FourCC(*b"free"), 8, 8));

    let mut large = vec![0, 0, 0, 1];
    large.extend_from_slice(b"mdat");
    assert_eq!(
        BoxHeader::parse(&large).unwrap(),
        None,
        "largesize needs 16 bytes"
    );
    large.extend_from_slice(&20u64.to_be_bytes());
    let h = BoxHeader::parse(&large).unwrap().unwrap();
    assert_eq!((h.size, h.header_len, h.body_len()), (20, 16, 4));

    // Size 0 (to end of file) and sizes smaller than the header are rejected.
    assert!(matches!(
        BoxHeader::parse(b"\0\0\0\0mdat"),
        Err(Error::InvalidBox { .. })
    ));
    assert!(matches!(
        BoxHeader::parse(b"\0\0\0\x04free"),
        Err(Error::InvalidBox { .. })
    ));
}

#[test]
fn large_mdat_keeps_its_header_form() {
    let mut bytes = vec![0, 0, 0, 1];
    bytes.extend_from_slice(b"mdat");
    bytes.extend_from_slice(&19u64.to_be_bytes());
    bytes.extend_from_slice(b"abc");
    let (b, used) = Mp4Box::decode(&bytes, 100).unwrap();
    assert_eq!(used, 19);
    let Mp4Box::Mdat(mdat) = &b else { panic!() };
    assert_eq!(
        (mdat.pos, mdat.payload_pos(), mdat.data.as_slice()),
        (100, 116, &b"abc"[..])
    );
    assert_eq!(b.to_bytes().unwrap(), bytes);
}

#[test]
fn truncated_input() {
    let full = boxed(b"free", &[1, 2, 3]);
    assert!(matches!(
        Mp4Box::decode(&full[..9], 0),
        Err(Error::Truncated)
    ));
    let mut r = &full[..5];
    assert!(matches!(
        rustet_mp4::read_box(&mut r, 0),
        Err(Error::Truncated)
    ));
    let mut r = &full[..10];
    assert!(matches!(
        rustet_mp4::read_box(&mut r, 0),
        Err(Error::Truncated)
    ));
    let mut r = &[][..];
    assert!(
        rustet_mp4::read_box(&mut r, 0).unwrap().is_none(),
        "clean end"
    );
}

#[test]
fn tfdt_version_follows_mp4ff() {
    let mut t = Tfdt::new(5);
    assert_eq!(t.version, 0);
    t.set_base_media_decode_time(1 << 32);
    assert_eq!(t.version, 1);
    t.set_base_media_decode_time(7);
    assert_eq!(t.version, 0, "SetBaseMediaDecodeTime drops back to 32 bits");
    t.version = 1;
    let bytes = Mp4Box::from(t).to_bytes().unwrap();
    assert_eq!(bytes.len(), 20, "a forced v1 tfdt is 4 bytes longer");
    let Mp4Box::Tfdt(back) = decode(&bytes).unwrap() else {
        panic!()
    };
    assert_eq!((back.version, back.base_media_decode_time()), (1, 7));
}

#[test]
fn trun_size_must_match_flags() {
    // flags: data offset + sample size; one sample.
    let mut payload = 0x201u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&100i32.to_be_bytes());
    payload.extend_from_slice(&42u32.to_be_bytes());
    let Mp4Box::Trun(trun) = decode(&boxed(b"trun", &payload)).unwrap() else {
        panic!()
    };
    assert_eq!((trun.data_offset, trun.samples[0].size), (100, 42));
    payload.extend_from_slice(&[0; 4]);
    assert!(matches!(
        decode(&boxed(b"trun", &payload)),
        Err(Error::InvalidBox { .. })
    ));
}

#[test]
fn trun_with_unset_data_offset_does_not_encode() {
    let trun = Trun {
        samples: vec![TrunSample::default()],
        ..Trun::new()
    };
    assert!(matches!(
        Mp4Box::from(trun).to_bytes(),
        Err(Error::TrunDataOffsetNotSet)
    ));
}

#[test]
fn stsd_count_must_match_entries() {
    let mut payload = vec![0, 0, 0, 0, 0, 0, 0, 2];
    payload.extend_from_slice(&boxed(b"mp4a", &[0; 28]));
    assert!(matches!(
        decode(&boxed(b"stsd", &payload)),
        Err(Error::InvalidBox { .. })
    ));
}

#[test]
fn senc_iv_size_inference() {
    // No subsamples: IV size from the data length (2 samples × 8 bytes).
    let s = senc(0, 2, &[1; 16]).parse(0).unwrap();
    assert_eq!((s.ivs.len(), s.ivs[0].len(), s.subsamples.len()), (2, 8, 0));
    assert!(matches!(senc(0, 2, &[1; 10]).parse(0), Err(Error::Senc(_))));

    // Subsamples with 8-byte IVs, found by trying 0, 8, 16.
    let mut raw = Vec::new();
    for _ in 0..2 {
        raw.extend_from_slice(&[9; 8]);
        raw.extend_from_slice(&1u16.to_be_bytes());
        raw.extend_from_slice(&3u16.to_be_bytes());
        raw.extend_from_slice(&32u32.to_be_bytes());
    }
    let s = senc(2, 2, &raw).parse(0).unwrap();
    assert_eq!(s.ivs, vec![vec![9; 8]; 2]);
    assert_eq!(
        s.subsamples[1],
        [SubSample {
            clear: 3,
            protected: 32
        }]
    );
    assert!(
        senc(2, 2, &raw).parse(16).is_err(),
        "explicit wrong IV size fails"
    );

    // Empty senc parses to nothing.
    assert_eq!(senc(0, 0, &[]).parse(16).unwrap(), Default::default());
    // Subsample flag with too little data fails at decode, like mp4ff.
    let mut payload = 2u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&4u32.to_be_bytes());
    assert!(matches!(
        decode(&boxed(b"senc", &payload)),
        Err(Error::InvalidBox { .. })
    ));
}

#[test]
fn sample_entry_without_frma_is_an_error() {
    let sinf = boxed(
        b"sinf",
        &boxed(b"schm", &[0, 0, 0, 0, b'c', b'b', b'c', b's', 0, 1, 0, 0]),
    );
    let mut enca = vec![0; 28];
    enca.extend_from_slice(&sinf);
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend_from_slice(&boxed(b"enca", &enca));
    let Mp4Box::Stsd(mut stsd) = decode(&boxed(b"stsd", &stsd)).unwrap() else {
        panic!()
    };
    let Mp4Box::SampleEntry(entry) = &mut stsd.entries[0] else {
        panic!()
    };
    assert!(matches!(
        entry.remove_encryption(),
        Err(Error::MissingBox("frma"))
    ));
    assert_eq!(
        entry.kind,
        FourCC::ENCA,
        "a failed removal leaves the entry alone"
    );
}

#[test]
fn lone_trun_gets_its_data_offset_on_encode() {
    let mut frag = Fragment::new(1, 1);
    frag.add_full_sample(
        TrunSample {
            duration: 1024,
            size: 3,
            ..Default::default()
        },
        90,
        b"abc",
    );
    let moof_size = frag.moof().unwrap().size();
    let bytes = frag.to_bytes().unwrap();
    let trun = frag.moof().unwrap().traf().unwrap().trun().unwrap();
    assert_eq!(trun.data_offset as u64, moof_size + 8);

    // Decoded back, the sample is found at that offset.
    let mut r = bytes.as_slice();
    let mut back = Fragment::default();
    let mut pos = 0;
    while let Some(b) = rustet_mp4::read_box(&mut r, pos).unwrap() {
        pos += b.size();
        back.push(b);
    }
    let samples = back.full_samples(None).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(&back.mdat().unwrap().data[samples[0].data.clone()], b"abc");
    assert_eq!((samples[0].decode_time, samples[0].duration), (90, 1024));

    // With two truns the offsets are left alone (mp4ff skips them for decoded fragments).
    let traf = back.moof_mut().unwrap().traf_mut().unwrap();
    traf.push(Trun {
        data_offset: 1234,
        ..Trun::new()
    });
    back.to_bytes().unwrap();
    let offsets: Vec<_> = back
        .moof()
        .unwrap()
        .traf()
        .unwrap()
        .truns()
        .map(|t| t.data_offset)
        .collect();
    assert_eq!(offsets, [moof_size as i32 + 8, 1234]);
}

#[test]
fn trun_without_sizes_shares_the_gap_to_the_next_trun() {
    // Two truns with data offsets but no per-sample sizes; the moof sits at 0 and the
    // mdat right after it.
    let offsets_only = |offset, n| Trun {
        flags: Trun::DATA_OFFSET_PRESENT,
        data_offset: offset,
        samples: vec![TrunSample::default(); n],
        ..Trun::new()
    };
    let mut frag = Fragment::new(1, 1);
    let traf = frag.moof_mut().unwrap().traf_mut().unwrap();
    traf.remove_kind(FourCC::TRUN);
    traf.push(offsets_only(200, 2));
    traf.push(offsets_only(210, 1));
    let moof_size = frag.moof().unwrap().size();
    let mdat = frag.mdat_mut().unwrap();
    mdat.pos = moof_size;
    mdat.data = vec![0; 300];

    let ranges: Vec<_> = frag
        .full_samples(None)
        .unwrap()
        .into_iter()
        .map(|s| s.data)
        .collect();
    let first = 200 - (moof_size as usize + 8);
    assert_eq!(
        ranges[..2],
        [first..first + 5, first + 5..first + 10],
        "10-byte gap over 2 samples"
    );
    assert_eq!(
        ranges[2],
        first + 10..first + 10,
        "the last trun keeps the default size 0"
    );
}

#[test]
fn sample_outside_mdat_is_an_error() {
    let mut frag = Fragment::new(1, 1);
    frag.add_full_sample(
        TrunSample {
            size: 50,
            ..Default::default()
        },
        0,
        b"short",
    );
    frag.to_bytes().unwrap();
    assert!(matches!(
        frag.full_samples(None),
        Err(Error::SampleOutOfRange(_))
    ));
}

#[test]
fn synthetic_init_segment_round_trips() {
    let init = InitSegment::single_track(90000, MediaType::Video).unwrap();
    let bytes = init.to_bytes().unwrap();
    let mut r = bytes.as_slice();
    let mut back = InitSegment::default();
    while let Some(b) = rustet_mp4::read_box(&mut r, 0).unwrap() {
        back.push(b);
    }
    assert_eq!(back, init);
    let trak = back.moov().unwrap().traks().next().unwrap();
    assert_eq!(trak.track_id(), Some(1));
    assert_eq!(trak.handler_type(), Some(FourCC::VIDE));
    assert_eq!(trak.mdhd().unwrap().timescale(), 90000);
    assert!(trak.stsd().unwrap().entries.is_empty());
    assert_eq!(
        back.moov()
            .unwrap()
            .trexs()
            .map(|t| t.track_id)
            .collect::<Vec<_>>(),
        [1]
    );
}

/// A seek stream starts at the sample that holds the target, not at the start of the fragment.
#[test]
fn trim_start_drops_the_samples_before_the_target() {
    let fragment = || {
        let mut frag = Fragment::new(1, 1);
        for (i, data) in [b"aa", b"bb", b"cc"].into_iter().enumerate() {
            let sample = TrunSample {
                duration: 1024,
                size: 2,
                ..Default::default()
            };
            frag.add_full_sample(sample, 5000 + 1024 * i as u64, data);
        }
        frag
    };
    let state = |frag: &Fragment| {
        let traf = frag.moof().unwrap().traf().unwrap();
        (
            traf.tfdt().unwrap().base_media_decode_time(),
            traf.trun().unwrap().samples.len(),
            frag.mdat().unwrap().data.clone(),
        )
    };

    // The target is inside the second sample: the first one goes, with its bytes.
    let mut frag = fragment();
    assert_eq!(frag.trim_start(5000 + 1024 + 10, 1024), Trim::Kept);
    assert_eq!(state(&frag), (5000 + 1024, 2, b"bbcc".to_vec()));
    // Still a valid fragment whose samples are where the trun says.
    let bytes = frag.to_bytes().unwrap();
    let (mut r, mut back, mut pos) = (bytes.as_slice(), Fragment::default(), 0);
    while let Some(b) = rustet_mp4::read_box(&mut r, pos).unwrap() {
        pos += b.size();
        back.push(b);
    }
    let samples = back.full_samples(None).unwrap();
    assert_eq!(&back.mdat().unwrap().data[samples[0].data.clone()], b"bb");

    // A target before the fragment, or inside its first sample, changes nothing.
    let mut frag = fragment();
    assert_eq!(frag.trim_start(5000 + 1023, 1024), Trim::Kept);
    assert_eq!(state(&frag), (5000, 3, b"aabbcc".to_vec()));

    // A fragment that ends at or before the target is dropped whole by the caller.
    let mut frag = fragment();
    assert_eq!(frag.trim_start(5000 + 3 * 1024, 1024), Trim::Before);
    assert_eq!(state(&frag).1, 3, "untouched");
}
