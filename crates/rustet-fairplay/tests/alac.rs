//! The init / fragment reader, the init sanitising and the DRM connection wire format.

use rustet_fairplay::alac::{self, Error};
use rustet_mp4::{Children, FourCC, Fragment, InitSegment, MediaType, Mp4Box, TrunSample};

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(payload);
    b
}

fn ftyp() -> Vec<u8> {
    boxed(b"ftyp", b"isom\0\0\0\0isom")
}

fn moov() -> Vec<u8> {
    boxed(b"moov", &[])
}

fn free() -> Vec<u8> {
    boxed(b"free", &[])
}

fn kinds(init: &InitSegment) -> Vec<FourCC> {
    init.children.iter().map(Mp4Box::kind).collect()
}

#[tokio::test]
async fn init_in_standard_order() {
    let input = [ftyp(), moov()].concat();
    let (init, offset) = alac::read_init_segment(&mut &input[..]).await.unwrap();
    assert_eq!(kinds(&init), [FourCC::FTYP, FourCC::MOOV]);
    assert_eq!(offset, input.len() as u64);
}

/// Boxes other than ftyp / moov before or between them are consumed and dropped, and still
/// count toward the offset (an old reader rejected them).
#[tokio::test]
async fn init_skips_other_boxes() {
    let input = [free(), free(), ftyp(), free(), moov(), ftyp()].concat();
    let (init, offset) = alac::read_init_segment(&mut &input[..]).await.unwrap();
    assert_eq!(kinds(&init), [FourCC::FTYP, FourCC::MOOV]);
    assert_eq!(
        offset,
        (input.len() - ftyp().len()) as u64,
        "reading stops after moov"
    );
}

#[tokio::test]
async fn init_without_ftyp_is_accepted() {
    let (init, _) = alac::read_init_segment(&mut &moov()[..]).await.unwrap();
    assert_eq!(kinds(&init), [FourCC::MOOV]);
}

#[tokio::test]
async fn init_without_moov_is_an_error() {
    let input = [ftyp(), free()].concat();
    let err = alac::read_init_segment(&mut &input[..]).await.unwrap_err();
    assert!(matches!(err, Error::NoMoov), "{err}");
    let many = free().repeat(64);
    let err = alac::read_init_segment(&mut &[many, moov()].concat()[..])
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::NoMoov),
        "moov must come within 64 boxes: {err}"
    );
}

#[tokio::test]
async fn fragment_reader() {
    assert!(
        alac::read_next_fragment(&mut &[][..], 0)
            .await
            .unwrap()
            .is_none()
    );

    let mut frag = Fragment::new(1, 1);
    let sample = TrunSample {
        duration: 1024,
        size: 3,
        ..TrunSample::default()
    };
    frag.add_full_sample(sample, 0, &[1, 2, 3]);
    let bytes = frag.to_bytes().unwrap();
    // A box between fragments is skipped; a trailing one after the mdat is left unread.
    let input = [free(), bytes.clone(), free()].concat();
    let (got, end) = alac::read_next_fragment(&mut &input[..], 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        end,
        (free().len() + bytes.len()) as u64,
        "offset is the end of the mdat"
    );
    assert_eq!(got.mdat().unwrap().data, [1, 2, 3]);
    assert_eq!(got.moof().unwrap().pos, free().len() as u64);

    let err = alac::read_next_fragment(&mut &boxed(b"mdat", b"x")[..], 0)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::MdatWithoutMoof(9)), "{err}");
}

#[test]
fn sanitize_init_adds_mvex_and_keeps_one_sample_entry() {
    let mut init = InitSegment::single_track(44100, MediaType::Audio).unwrap();
    let moov = init.moov_mut().unwrap();
    moov.remove_kind(FourCC::MVEX);
    let entry = Mp4Box::decode(&boxed(b"alac", &[0; 28]), 0).unwrap().0;
    let stsd = moov.traks_mut().next().unwrap().stsd_mut().unwrap();
    stsd.entries = vec![entry.clone(), entry];

    alac::sanitize_init(&mut init).unwrap();
    let moov = init.moov().unwrap();
    assert_eq!(
        moov.traks().next().unwrap().stsd().unwrap().entries.len(),
        1
    );
    let trex = moov.trexs().next().expect("mvex/trex added");
    assert_eq!((trex.track_id, trex.default_sample_duration), (1, 4096));
}

#[test]
fn sanitize_init_rejects_mixed_entries() {
    let mut init = InitSegment::single_track(44100, MediaType::Audio).unwrap();
    let a = Mp4Box::decode(&boxed(b"alac", &[0; 28]), 0).unwrap().0;
    let b = Mp4Box::decode(&boxed(b"mp4a", &[0; 28]), 0).unwrap().0;
    let moov = init.moov_mut().unwrap();
    moov.traks_mut().next().unwrap().stsd_mut().unwrap().entries = vec![a, b];
    assert!(matches!(
        alac::sanitize_init(&mut init),
        Err(Error::MixedSampleEntries)
    ));
}

// ── Wire format ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn switch_keys_is_four_zero_bytes() {
    let mut buf = Vec::new();
    alac::switch_keys(&mut buf).await.unwrap();
    assert_eq!(buf, [0; 4]);
}

/// Close is a key switch plus an empty adamID: the server ends both of its loops.
#[tokio::test]
async fn close_is_five_zero_bytes() {
    let mut buf = Vec::new();
    alac::close(&mut buf).await.unwrap();
    assert_eq!(buf, [0; 5]);
}

#[tokio::test]
async fn send_string_is_length_prefixed() {
    for (s, want) in [("0", &b"\x010"[..]), ("abc", b"\x03abc"), ("", b"\x00")] {
        let mut buf = Vec::new();
        alac::send_string(&mut buf, s).await.unwrap();
        assert_eq!(buf, want, "{s:?}");
    }
    let mut buf = Vec::new();
    assert!(alac::send_string(&mut buf, &"x".repeat(256)).await.is_err());
    assert!(
        buf.is_empty(),
        "nothing is sent for a string that does not fit"
    );
}

/// A key rotation: switch, then adamID and key URI.
#[tokio::test]
async fn key_rotation_sequence() {
    let mut buf = Vec::new();
    alac::switch_keys(&mut buf).await.unwrap();
    alac::send_string(&mut buf, "1488408568").await.unwrap();
    alac::send_string(&mut buf, "skd://itunes.apple.com/p1238037727/c6")
        .await
        .unwrap();
    let mut want = vec![0, 0, 0, 0, 10];
    want.extend_from_slice(b"1488408568");
    want.push(37);
    want.extend_from_slice(b"skd://itunes.apple.com/p1238037727/c6");
    assert_eq!(buf, want);
}
