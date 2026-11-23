//! End-to-end runs of the CBCS source against a fake FairPlay decryptor.
//!
//! `cbcs_track.mp4` is an Apple-style CBCS audio file made with mp4ff: 5 fragments of 4
//! samples, segment 0 under the shared prefetch key, the rest under the track key, one
//! constant IV, every tfdt left at 0. `cbcs_track.plain` holds its samples in the clear
//! (`[u32 BE len][bytes]` each). The `go_*.mp4` files are what the Go engine wrote for the
//! same runs; the Rust output must be byte-identical. See `tests/data/README.md`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{FakeFairPlay, Key, http_server};
use rustet_core::pipeline::{PipelineError, Source, StreamContext};
use rustet_fairplay::{CbcsSource, Error, PREFETCH_KEY, TcpDialer, alac, warm_cbcs_key};
use rustet_hls::{CbcsMedia, CbcsSegment};
use rustet_mp4::{Children, FourCC, Trex};
use tokio_util::sync::CancellationToken;

const ADAM_ID: &str = "1440833098";
const CONTENT_KEY: &str = "skd://itunes.apple.com/p1469029813/c6";
const SAMPLES_PER_FRAG: usize = 4;
const SAMPLE_DUR: u64 = 4096;
const TRACK_FRAGS: usize = 5;
/// The duration the Go `*_dur` runs patched into the moov.
const DURATION_MS: u32 = 2322;
const IV: [u8; 16] = [0xd1; 16];

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn track_keys() -> HashMap<String, Key> {
    HashMap::from([
        (
            PREFETCH_KEY.to_string(),
            Key {
                key: [0x11; 16],
                iv: IV,
            },
        ),
        (
            CONTENT_KEY.to_string(),
            Key {
                key: [0x22; 16],
                iv: IV,
            },
        ),
    ])
}

fn seg_dur() -> f64 {
    (SAMPLES_PER_FRAG as u64 * SAMPLE_DUR) as f64 / 44100.0
}

fn media(url: &str, keys: &[&str]) -> CbcsMedia {
    CbcsMedia {
        file_url: url.to_string(),
        kid_base64: None,
        segments: (0..TRACK_FRAGS)
            .map(|i| CbcsSegment {
                key_uri: keys.get(i).filter(|k| !k.is_empty()).map(|k| k.to_string()),
                duration: seg_dur(),
            })
            .collect(),
    }
}

fn source(fp: &FakeFairPlay, media: &CbcsMedia, duration_ms: u32) -> CbcsSource {
    CbcsSource::new(
        ADAM_ID,
        Arc::new(TcpDialer(fp.addr.clone())),
        reqwest::Client::new(),
        media,
        duration_ms,
    )
}

async fn run(src: &dyn Source) -> (Result<(), PipelineError>, Vec<u8>) {
    let mut out = Vec::new();
    let res = tokio::time::timeout(
        Duration::from_secs(20),
        src.stream(&StreamContext::default(), &mut out),
    )
    .await
    .expect("stream hung");
    (res, out)
}

/// The plaintext samples of `cbcs_track.mp4`, per fragment.
fn plain_samples() -> Vec<Vec<Vec<u8>>> {
    let raw = data("cbcs_track.plain");
    let mut samples = Vec::new();
    let mut rest = &raw[..];
    while !rest.is_empty() {
        let n = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        samples.push(rest[4..4 + n].to_vec());
        rest = &rest[4 + n..];
    }
    samples
        .chunks(SAMPLES_PER_FRAG)
        .map(<[_]>::to_vec)
        .collect()
}

/// Parses the output as clear fMP4: every fragment's decode time and samples.
async fn decode_output(out: &[u8]) -> Vec<(u64, Vec<Vec<u8>>)> {
    let mut r = out;
    let (init, mut offset) = alac::read_init_segment(&mut r).await.unwrap();
    let moov = init.moov().unwrap();
    let entry = moov.traks().next().unwrap().stsd().unwrap().entries[0].kind();
    assert_eq!(
        entry,
        FourCC(*b"mp4a"),
        "the output still advertises encryption"
    );
    let trex: Trex = *moov.trexs().next().unwrap();
    let mut frags = Vec::new();
    while let Some((frag, end)) = alac::read_next_fragment(&mut r, offset).await.unwrap() {
        offset = end;
        let traf = frag.moof().unwrap().traf().unwrap();
        assert!(
            traf.senc().is_none() && traf.saio().is_none() && traf.child(FourCC::SAIZ).is_none(),
            "encryption boxes left in an output fragment"
        );
        let mdat = frag.mdat().unwrap();
        let samples = frag
            .full_samples(Some(&trex))
            .unwrap()
            .into_iter()
            .map(|s| mdat.data[s.data].to_vec())
            .collect();
        frags.push((traf.tfdt().unwrap().base_media_decode_time(), samples));
    }
    frags
}

/// The output decrypts to the original samples from `first` on, with monotonic decode times.
async fn check_plaintext(out: &[u8], first: usize) {
    let want = plain_samples();
    let got = decode_output(out).await;
    assert_eq!(got.len(), want.len() - first);
    for (i, (tfdt, samples)) in got.iter().enumerate() {
        let f = first + i;
        assert_eq!(
            *tfdt,
            (f * SAMPLES_PER_FRAG) as u64 * SAMPLE_DUR,
            "fragment {f} tfdt"
        );
        assert_eq!(
            samples, &want[f],
            "fragment {f} does not decrypt to the original"
        );
    }
}

#[tokio::test]
async fn full_stream_matches_go_and_opens_upstream_keys() {
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, track_keys(), None).await;
    let (url, _) = http_server(200, data("cbcs_track.mp4"), Duration::ZERO).await;
    let m = media(&url, &[PREFETCH_KEY, CONTENT_KEY]);

    let (res, out) = run(&source(&fp, &m, 0)).await;
    res.unwrap();
    assert!(
        out == data("go_full.mp4"),
        "output differs from the Go engine's"
    );
    check_plaintext(&out, 0).await;
    // Exactly upstream's key sequence: the prefetch key under "0", then the bare content
    // URI under the track's adamID, no ",<kid>" suffix.
    assert_eq!(
        fp.opened(),
        [
            format!("0 {PREFETCH_KEY}"),
            format!("{ADAM_ID} {CONTENT_KEY}")
        ]
    );

    let (res, out) = run(&source(&fp, &m, DURATION_MS)).await;
    res.unwrap();
    assert!(
        out == data("go_full_dur.mp4"),
        "patched duration differs from Go"
    );
}

#[tokio::test]
async fn seek_starts_mid_track_with_the_content_key() {
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, track_keys(), None).await;
    let (url, _) = http_server(200, data("cbcs_track.mp4"), Duration::ZERO).await;
    let m = media(&url, &[PREFETCH_KEY, CONTENT_KEY]);
    let full = source(&fp, &m, DURATION_MS);

    let (src, actual) = full.seekable().unwrap().source_from(2.5 * seg_dur());
    assert_eq!(
        actual,
        2.0 * seg_dur(),
        "the seek lands at the start of fragment 2"
    );
    let (res, out) = run(&*src).await;
    res.unwrap();
    assert!(
        out == data("go_seek2_dur.mp4"),
        "seek output differs from Go"
    );
    check_plaintext(&out, 2).await;
    assert_eq!(
        fp.opened(),
        [format!("{ADAM_ID} {CONTENT_KEY}")],
        "a seek past segment 1 opens only the content key"
    );

    // A seek inside segment 0 starts from fragment 0 with the seek-path patching.
    let (src, actual) = source(&fp, &m, 0)
        .seekable()
        .unwrap()
        .source_from(0.5 * seg_dur());
    assert_eq!(actual, 0.0);
    let (res, out) = run(&*src).await;
    res.unwrap();
    assert!(out == data("go_seek0.mp4"), "seek-0 output differs from Go");
}

/// Upstream only uses the key tags of segments 0 and 1; a later tag must not reach FairPlay
/// (here it is unknown to the server and would kill the stream).
#[tokio::test]
async fn key_tags_after_segment_one_are_ignored() {
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, track_keys(), None).await;
    let (url, _) = http_server(200, data("cbcs_track.mp4"), Duration::ZERO).await;
    let m = media(
        &url,
        &[
            PREFETCH_KEY,
            CONTENT_KEY,
            "",
            "skd://itunes.apple.com/p1/c99",
        ],
    );
    let (res, out) = run(&source(&fp, &m, 0)).await;
    res.unwrap();
    assert!(out == data("go_full.mp4"));
    assert_eq!(fp.opened().len(), 2);
}

/// A key FairPlay cannot open fails the stream; ciphertext is never passed through.
#[tokio::test]
async fn unknown_key_fails_instead_of_emitting_ciphertext() {
    let keys = HashMap::from([(
        PREFETCH_KEY.to_string(),
        Key {
            key: [0x11; 16],
            iv: IV,
        },
    )]);
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, keys, None).await;
    let (url, calls) = http_server(200, data("cbcs_track.mp4"), Duration::ZERO).await;
    let (res, out) = run(&source(&fp, &media(&url, &[PREFETCH_KEY, CONTENT_KEY]), 0)).await;

    let Err(PipelineError::Other(e)) = res else {
        panic!("stream succeeded although the content key could not be opened: {res:?}");
    };
    assert!(
        matches!(
            e.downcast_ref::<Error>(),
            Some(Error::Decrypt { index: 1, .. })
        ),
        "{e}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no retry once bytes are out"
    );
    let mut r = &out[..];
    let (_, offset) = alac::read_init_segment(&mut r).await.unwrap();
    let (_, end) = alac::read_next_fragment(&mut r, offset)
        .await
        .unwrap()
        .unwrap();
    assert!(
        alac::read_next_fragment(&mut r, end)
            .await
            .unwrap()
            .is_none(),
        "only fragment 0 may be written"
    );
}

/// A dialer that counts the key refreshes the stream asks for.
struct CountingRefresh {
    inner: TcpDialer,
    refreshes: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl rustet_fairplay::CbcsDialer for CountingRefresh {
    async fn dial_cbcs(
        &self,
        cancel: &CancellationToken,
    ) -> std::io::Result<rustet_fairplay::DrmStream> {
        self.inner.dial_cbcs(cancel).await
    }

    async fn refresh_keys(&self) -> std::io::Result<()> {
        self.refreshes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Apple's library can hand out an expired key as valid: the decryptor then returns bytes
/// that are not audio, without an error. On an ALAC stream this is seen (the samples do not
/// start like ALAC frames), fresh keys are requested and the fragment is decrypted again. If
/// it is still not audio the stream fails; the bad samples are never written.
#[tokio::test]
async fn samples_that_are_not_alac_refresh_the_keys_and_are_not_emitted() {
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, track_keys(), None).await;
    // The fixture is AAC: labelled ALAC, its correctly decrypted samples stand in for the
    // output of a wrong key.
    let mut file = data("cbcs_track.mp4");
    let frma = file.windows(8).position(|w| w == b"frmamp4a").unwrap();
    file[frma + 4..frma + 8].copy_from_slice(b"alac");
    let (url, _) = http_server(200, file, Duration::ZERO).await;
    let dialer = Arc::new(CountingRefresh {
        inner: TcpDialer(fp.addr.clone()),
        refreshes: Default::default(),
    });
    let src = CbcsSource::new(
        ADAM_ID,
        dialer.clone(),
        reqwest::Client::new(),
        &media(&url, &[PREFETCH_KEY, CONTENT_KEY]),
        0,
    );
    let (res, out) = run(&src).await;

    let Err(PipelineError::Other(e)) = res else {
        panic!("the stream succeeded with samples that are not ALAC: {res:?}");
    };
    assert!(
        matches!(
            e.downcast_ref::<Error>(),
            Some(Error::Decrypt {
                index: 0,
                source: alac::Error::WrongKey { .. }
            })
        ),
        "{e}"
    );
    assert_eq!(
        dialer.refreshes.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let mut r = &out[..];
    let (_, offset) = alac::read_init_segment(&mut r).await.unwrap();
    assert!(
        alac::read_next_fragment(&mut r, offset)
            .await
            .unwrap()
            .is_none(),
        "no fragment may be written"
    );
}

/// A decryptor connection that dies mid-stream is redialed, the key in force is selected
/// again, and the fragment is decrypted from its saved ciphertext: the output is unchanged.
#[tokio::test]
async fn dropped_drm_connection_is_redialed() {
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, track_keys(), Some(6)).await;
    let (url, _) = http_server(200, data("cbcs_track.mp4"), Duration::ZERO).await;
    let (res, out) = run(&source(&fp, &media(&url, &[PREFETCH_KEY, CONTENT_KEY]), 0)).await;
    res.unwrap();
    assert!(out == data("go_full.mp4"));
    assert_eq!(fp.connections.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        fp.opened(),
        [
            format!("0 {PREFETCH_KEY}"),
            format!("{ADAM_ID} {CONTENT_KEY}"),
            format!("{ADAM_ID} {CONTENT_KEY}"),
        ]
    );
}

/// Several truns per traf (real ALAC has 11): mp4ff's cbcs AAC test stream.
#[tokio::test]
async fn multi_trun_stream_matches_go() {
    let file = data("cbcs_audio.in.mp4");
    let mut init = alac::read_init_segment(&mut &file[..]).await.unwrap().0;
    let info = alac::transform_init(&mut init).unwrap();
    let tenc = info.tracks[0].protection.as_ref().unwrap().tenc().unwrap();
    let iv: [u8; 16] = tenc.default_constant_iv[..].try_into().unwrap();
    let key = [
        0x5f, 0xfd, 0x93, 0x86, 0x1f, 0xa7, 0x76, 0xe9, 0x6c, 0xcc, 0xd9, 0x34, 0x89, 0x8f, 0xc1,
        0xc8,
    ];
    let keys = HashMap::from([("skd://aac".to_string(), Key { key, iv })]);
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, keys, None).await;
    let (url, _) = http_server(200, file, Duration::ZERO).await;
    let (res, out) = run(&source(&fp, &media(&url, &["skd://aac"]), 0)).await;
    res.unwrap();
    assert!(
        out == data("go_multitrun.mp4"),
        "multi-trun output differs from Go"
    );
}

/// The track key is warmed ahead of playback; the shared prefetch key is warmed by the
/// backend itself and is not sent again per track.
#[tokio::test]
async fn warm_opens_only_track_keys() {
    let fp = FakeFairPlay::start(ADAM_ID, PREFETCH_KEY, track_keys(), None).await;
    let dialer = TcpDialer(fp.addr.clone());
    warm_cbcs_key(&dialer, ADAM_ID, PREFETCH_KEY).await;
    warm_cbcs_key(&dialer, ADAM_ID, CONTENT_KEY).await;
    warm_cbcs_key(&dialer, "", CONTENT_KEY).await;

    let deadline = Instant::now() + Duration::from_secs(2);
    while fp.opened().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fp.opened(), [format!("{ADAM_ID} {CONTENT_KEY}")]);
}

// ── Retry ────────────────────────────────────────────────────────────────────

fn unreachable_source(url: &str) -> CbcsSource {
    CbcsSource::new(
        "0",
        Arc::new(TcpDialer("127.0.0.1:1".into())),
        reqwest::Client::new(),
        &media(url, &[]),
        0,
    )
}

/// A failing download is retried: 3 attempts in all, and the HTTP status is reported.
#[tokio::test]
async fn http_failure_is_retried_three_times() {
    let (url, calls) = http_server(404, b"nope".to_vec(), Duration::ZERO).await;
    let (res, _) = run(&unreachable_source(&url)).await;
    let Err(PipelineError::Other(e)) = res else {
        panic!("expected an error, got {res:?}");
    };
    assert!(matches!(e.downcast_ref::<Error>(), Some(Error::Status(s)) if s.as_u16() == 404));
    assert!(e.to_string().contains("404"), "{e}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

/// Cancelling during the 1 s back-off returns promptly.
#[tokio::test]
async fn cancel_during_backoff_returns_promptly() {
    let (url, _) = http_server(503, b"fail".to_vec(), Duration::ZERO).await;
    let src = unreachable_source(&url);
    let ctx = StreamContext::new(CancellationToken::new());
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let start = Instant::now();
    let res = src.stream(&ctx, &mut Vec::new()).await;
    assert!(matches!(res, Err(PipelineError::Cancelled)), "{res:?}");
    assert!(
        start.elapsed() < Duration::from_millis(800),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn cancelled_context_returns_at_once() {
    let (url, _) = http_server(200, b"data".to_vec(), Duration::from_millis(500)).await;
    let ctx = StreamContext::new(CancellationToken::new());
    ctx.cancel.cancel();
    let start = Instant::now();
    let res = unreachable_source(&url).stream(&ctx, &mut Vec::new()).await;
    assert!(matches!(res, Err(PipelineError::Cancelled)), "{res:?}");
    assert!(start.elapsed() < Duration::from_millis(300));
}
