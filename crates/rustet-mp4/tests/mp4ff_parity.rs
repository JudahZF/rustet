//! Byte-level parity with mp4ff on real encrypted streams (see `tests/data/README.md`).

use rustet_mp4::{
    Children, FourCC, Fragment, InitSegment, Mp4Box, Scheme, Tfdt, decrypt_fragment, decrypt_init,
    decrypt_sample_cbcs, read_box, read_box_async,
};
use sha2::{Digest, Sha256};

struct Case {
    name: &'static str,
    key: &'static str,
    /// mp4ff output after DecryptInit + DecryptFragment.
    dec: &'static str,
    /// mp4ff output after decryption plus the passthrough tfdt patch (see `patch_tfdt`).
    patched: &'static str,
}

const CASES: [Case; 3] = [
    Case {
        name: "cbcs_audio",
        key: "5ffd93861fa776e96cccd934898fc1c8",
        dec: "82afe7d8e48ccaaa5b0034185476e55b39f9a372dde63fdb64b88d8a3a8d9332",
        patched: "616d9f96eb5bf600213b1f418d9898674aae021ef05ddfaf11571323127b53f7",
    },
    Case {
        name: "cbcs_video",
        key: "22bdb0063805260307ee5045c0f3835a",
        dec: "3839257a48de273f3e6680f7eda0bac9d8fcbbaec959b86403c4c632f2ed0f43",
        patched: "66561993d31d4c7bf1a6c2345d3b56d781447ce708838a57bf39a4a427152c32",
    },
    Case {
        name: "cenc_video",
        key: "63cb5f7184dd4b689a5c5ff11ee6a328",
        dec: "49b2fc5c4e45035097d11ad51fc2fad988325146a47d27f4bec177a872f2740b",
        patched: "194819a0d1a5cf547b35e262b17e8d017d9a92dac6db8fb57489ef08fe5337ec",
    },
];

fn input(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/data/{name}.in.mp4",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn key(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The callers' init reader: boxes until moov, keeping ftyp and moov.
fn read_init(r: &mut &[u8], pos: &mut u64) -> InitSegment {
    let mut init = InitSegment::default();
    loop {
        let b = read_box(r, *pos).unwrap().expect("moov");
        *pos += b.size();
        let kind = b.kind();
        if kind == FourCC::FTYP || kind == FourCC::MOOV {
            init.push(b);
        }
        if kind == FourCC::MOOV {
            return init;
        }
    }
}

/// The callers' fragment reader: boxes up to and including mdat; `None` at the end.
fn read_fragment(r: &mut &[u8], pos: &mut u64) -> Option<Fragment> {
    let mut frag = Fragment::default();
    loop {
        let b = read_box(r, *pos).unwrap()?;
        *pos += b.size();
        let is_mdat = b.kind() == FourCC::MDAT;
        frag.push(b);
        if is_mdat {
            return Some(frag);
        }
    }
}

/// Runs `init_edit` on the init segment and `edit` over every fragment (with the state
/// `init_edit` returned), and returns the re-encoded stream.
fn transform<S>(
    data: &[u8],
    init_edit: impl FnOnce(&mut InitSegment) -> S,
    mut edit: impl FnMut(&mut Fragment, &mut S),
) -> Vec<u8> {
    let mut r = data;
    let mut pos = 0;
    let mut init = read_init(&mut r, &mut pos);
    let mut state = init_edit(&mut init);
    let mut out = init.to_bytes().unwrap();
    while let Some(mut frag) = read_fragment(&mut r, &mut pos) {
        edit(&mut frag, &mut state);
        frag.encode_into(&mut out).unwrap();
    }
    out
}

/// Port of aacstream's `patchFragTfdt` (also the shape of cbcs.go's `patchAlacFragment`),
/// written against the porting table.
fn patch_tfdt(frag: &mut Fragment, acc: u64) -> u64 {
    let moof = frag.moof_mut().unwrap();
    let old_size = moof.size();
    let traf = moof.traf_mut().unwrap();
    if let Some(tfdt) = traf.tfdt_mut() {
        tfdt.set_base_media_decode_time(acc);
        tfdt.version = 1;
    } else {
        let mut tfdt = Tfdt::new(acc);
        tfdt.version = 1;
        for child in std::mem::take(&mut traf.children) {
            if child.kind() == FourCC::TRUN {
                traf.children.push(tfdt.into());
            }
            traf.children.push(child);
        }
    }
    let tfhd = traf.tfhd_mut().unwrap();
    tfhd.track_id = 1;
    if tfhd.has_sample_description_index() {
        tfhd.sample_description_index = 1;
    }
    let dur = if tfhd.has_default_sample_duration() && tfhd.default_sample_duration > 0 {
        tfhd.default_sample_duration
    } else {
        1024
    };
    let diff = moof.size() as i32 - old_size as i32;
    let traf = moof.traf_mut().unwrap();
    for trun in traf.truns_mut().filter(|t| t.has_data_offset()) {
        trun.data_offset += diff;
    }
    acc + traf.truns().map(|t| t.duration(dur)).sum::<u64>()
}

#[test]
fn decode_encode_round_trip_is_lossless() {
    for case in &CASES {
        let data = input(case.name);
        assert_eq!(transform(&data, |_| (), |_, _| ()), data, "{}", case.name);
    }
}

#[test]
fn decryption_matches_mp4ff() {
    for case in &CASES {
        let key = key(case.key);
        let out = transform(
            &input(case.name),
            |init| decrypt_init(init).unwrap(),
            |frag, info| decrypt_fragment(frag, info, &key).unwrap(),
        );
        assert_eq!(sha(&out), case.dec, "{}", case.name);
    }
}

#[test]
fn decrypt_then_tfdt_patch_matches_mp4ff() {
    for case in &CASES {
        let key = key(case.key);
        let mut acc = 0;
        let out = transform(
            &input(case.name),
            |init| decrypt_init(init).unwrap(),
            |frag, info| {
                decrypt_fragment(frag, info, &key).unwrap();
                acc = patch_tfdt(frag, acc);
                frag.moof_mut().unwrap().mfhd_mut().unwrap().sequence_number = 0;
            },
        );
        assert_eq!(sha(&out), case.patched, "{}", case.name);
    }
}

/// Apple's AAC fragments have no tfdt: the patch inserts one before the trun.
#[test]
fn tfdt_insertion_matches_mp4ff() {
    // Remove the first traf's tfdt, keeping every data offset valid.
    let without = transform(
        &input("cenc_video"),
        |_| (),
        |frag, _| {
            let moof = frag.moof_mut().unwrap();
            let removed: u64 = moof
                .traf_mut()
                .unwrap()
                .remove_kind(FourCC::TFDT)
                .iter()
                .map(Mp4Box::size)
                .sum();
            for trun in moof.trafs_mut().flat_map(|t| t.truns_mut()) {
                trun.data_offset -= removed as i32;
            }
        },
    );
    assert_eq!(
        sha(&without),
        "9a58ba6235e82c2a71de562fe33f7cb1d96238005e8a91c0c24f67392f03ddb1"
    );

    let mut acc = 7;
    let patched = transform(&without, |_| (), |frag, _| acc = patch_tfdt(frag, acc));
    assert_eq!(
        sha(&patched),
        "102f9be80fcbdfd988201e999488069f9787dc995a03e34838093795bb60e96f"
    );
}

/// alacstream's `DecryptFragment` built from the pieces (the DRM socket replaced by
/// AES-CBC with the known key) must produce mp4ff's decryption.
#[test]
fn socket_style_cbcs_decryption_matches_mp4ff() {
    let case = &CASES[0];
    let key: [u8; 16] = key(case.key).try_into().unwrap();
    let out = transform(
        &input(case.name),
        |init| decrypt_init(init).unwrap(),
        |frag, info| {
            let mut removed = 0;
            for i in 0..frag.moof().unwrap().trafs().count() {
                let traf = frag.moof().unwrap().trafs().nth(i).unwrap();
                let id = traf.tfhd().unwrap().track_id;
                let ti = info.tracks.iter().rev().find(|t| t.track_id == id).unwrap();
                let protection = ti.protection.as_ref().unwrap();
                assert_eq!(protection.scheme, Scheme::Cbcs);
                let tenc = protection.tenc().unwrap();
                let senc = traf
                    .senc()
                    .unwrap()
                    .parse(tenc.default_per_sample_iv_size)
                    .unwrap();
                let samples = frag.full_samples(ti.trex.as_ref()).unwrap();
                let mut iv = [0u8; 16];
                iv.copy_from_slice(&tenc.default_constant_iv);
                let mdat = frag.mdat_mut().unwrap();
                for (n, s) in samples.iter().enumerate() {
                    let subs = senc.subsamples.get(n).map(Vec::as_slice);
                    decrypt_sample_cbcs(&mut mdat.data[s.data.clone()], &key, &iv, subs, tenc)
                        .unwrap();
                }
                removed += frag
                    .moof_mut()
                    .unwrap()
                    .trafs_mut()
                    .nth(i)
                    .unwrap()
                    .remove_encryption_boxes();
            }
            let moof = frag.moof_mut().unwrap();
            removed += moof.remove_psshs();
            for trun in moof.trafs_mut().flat_map(|t| t.truns_mut()) {
                trun.data_offset -= removed as i32;
            }
        },
    );
    assert_eq!(sha(&out), case.dec);
}

#[test]
fn fields_match_mp4ff_dump() {
    let data = input("cbcs_audio");
    let mut r = data.as_slice();
    let mut pos = 0;
    let mut init = read_init(&mut r, &mut pos);
    let moov = init.moov().unwrap();
    assert_eq!(moov.mvhd().unwrap().timescale(), 48000);
    let trak = moov.traks().next().unwrap();
    assert_eq!(trak.track_id(), Some(1));
    assert_eq!(trak.handler_type(), Some(FourCC::SOUN));
    assert_eq!(trak.mdhd().unwrap().timescale(), 48000);
    let kinds: Vec<_> = trak
        .stsd()
        .unwrap()
        .entries
        .iter()
        .map(Mp4Box::kind)
        .collect();
    assert_eq!(kinds, [FourCC::ENCA]);

    let info = decrypt_init(&mut init).unwrap();
    assert_eq!(info.psshs.len(), 2);
    assert!(init.moov().unwrap().child(FourCC::PSSH).is_none());
    let kinds: Vec<_> = init
        .moov()
        .unwrap()
        .traks()
        .next()
        .unwrap()
        .stsd()
        .unwrap()
        .entries
        .iter()
        .map(Mp4Box::kind)
        .collect();
    assert_eq!(kinds, [FourCC(*b"mp4a")]);
    let ti = info.track(1).unwrap();
    assert!(ti.trex.is_some());
    let tenc = ti.protection.as_ref().unwrap().tenc().unwrap();
    assert_eq!(
        (
            tenc.default_per_sample_iv_size,
            tenc.default_crypt_byte_block,
            tenc.default_skip_byte_block
        ),
        (0, 0, 0)
    );
    assert_eq!(
        tenc.default_constant_iv,
        key("94dc7d3f974d9087cb38c90f121b246d")
    );

    let frag = read_fragment(&mut r, &mut pos).unwrap();
    let moof = frag.moof().unwrap();
    assert_eq!(moof.pos, 1338);
    assert_eq!(moof.mfhd().unwrap().sequence_number, 1);
    let traf = moof.traf().unwrap();
    let tfhd = traf.tfhd().unwrap();
    assert_eq!((tfhd.track_id, tfhd.flags), (1, 0x20018));
    let tfdt = traf.tfdt().unwrap();
    assert_eq!((tfdt.base_media_decode_time(), tfdt.version), (480000, 1));
    assert_eq!(traf.truns().count(), 7);
    let trun = traf.trun().unwrap();
    assert_eq!((trun.sample_count(), trun.data_offset), (70, 5893));
    assert_eq!(frag.mdat().unwrap().data.len(), 168732);
    let kinds: Vec<_> = traf.children.iter().map(|c| c.kind().to_string()).collect();
    assert_eq!(kinds[..5], ["tfhd", "tfdt", "senc", "saiz", "saio"]);

    // Both other streams: pattern, IV size and track order as mp4ff reports them.
    let mut r = &input("cbcs_video")[..];
    let info = decrypt_init(&mut read_init(&mut r, &mut 0)).unwrap();
    let tenc = info
        .track(1)
        .unwrap()
        .protection
        .as_ref()
        .unwrap()
        .tenc()
        .unwrap();
    assert_eq!(
        (tenc.default_crypt_byte_block, tenc.default_skip_byte_block),
        (1, 9)
    );
    assert!(info.track(2).unwrap().protection.is_none());
    let mut r = &input("cenc_video")[..];
    let info = decrypt_init(&mut read_init(&mut r, &mut 0)).unwrap();
    let ids: Vec<_> = info.tracks.iter().map(|t| t.track_id).collect();
    assert_eq!(ids, [2, 1]);
    let protection = info.track(2).unwrap().protection.as_ref().unwrap();
    assert_eq!(
        (
            protection.scheme,
            protection.tenc().unwrap().default_per_sample_iv_size
        ),
        (Scheme::Cenc, 8)
    );
}

#[tokio::test]
async fn async_reader_matches_sync_reader() {
    let data = input("cbcs_video");
    let (mut sync_r, mut async_r) = (data.as_slice(), data.as_slice());
    let mut pos = 0;
    loop {
        let a = read_box(&mut sync_r, pos).unwrap();
        let b = read_box_async(&mut async_r, pos).await.unwrap();
        assert_eq!(a, b);
        let Some(a) = a else { break };
        pos += a.size();
    }
    assert_eq!(pos, data.len() as u64);
}

/// Every prefix and a spread of single-byte corruptions of real streams must yield
/// errors, never panics, through decode, sample lookup and decryption.
#[test]
fn malformed_input_never_panics() {
    for case in &CASES {
        let data = input(case.name);
        let key = key(case.key);
        let exercise = |bytes: &[u8]| {
            let mut r = bytes;
            let mut pos = 0;
            let mut init = InitSegment::default();
            let mut info = None;
            let mut frag = Fragment::default();
            loop {
                let b = match read_box(&mut r, pos) {
                    Ok(Some(b)) => b,
                    Ok(None) | Err(_) => return,
                };
                pos += b.size();
                match b.kind() {
                    FourCC::MOOV => {
                        init.push(b);
                        info = decrypt_init(&mut init).ok();
                        let _ = init.to_bytes();
                    }
                    FourCC::MDAT => {
                        frag.push(b);
                        if let Some(info) = &info {
                            let _ = frag.full_samples(info.tracks[0].trex.as_ref());
                            let _ = decrypt_fragment(&mut frag, info, &key);
                        }
                        let _ = frag.to_bytes();
                        frag = Fragment::default();
                    }
                    _ => frag.push(b),
                }
            }
        };
        // Prefixes around box boundaries and inside the moov / first moof.
        for len in (0..data.len().min(8000)).step_by(7) {
            exercise(&data[..len]);
        }
        let mut corrupt = data.clone();
        let mut seed = 0x9e37_79b9_u32;
        for _ in 0..150 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            // Corrupt only the box structure region (init + first moof).
            let at = seed as usize % data.len().min(8000);
            let old = corrupt[at];
            corrupt[at] = old ^ (seed >> 24) as u8 | 1;
            exercise(&corrupt);
            corrupt[at] = old;
        }
    }
}
