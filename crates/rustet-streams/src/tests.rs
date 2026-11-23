//! Download, direct-seek and licence tests against a local HTTP server.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use rustet_core::pipeline::{Seekable, StreamContext};
use rustet_mp4::{Fragment, InitSegment, MediaType, TrunSample};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::{Error, LicenceRequest, Streams};

/// Serves files with optional Range support and counts requests and bytes.
#[derive(Clone, Default)]
struct Files {
    files: Arc<HashMap<String, Vec<u8>>>,
    honour_range: bool,
    requests: Arc<AtomicUsize>,
    served: Arc<AtomicU64>,
}

impl Files {
    fn new(files: &[(&str, &[u8])], honour_range: bool) -> Self {
        Self {
            files: Arc::new(
                files
                    .iter()
                    .map(|(p, d)| (p.to_string(), d.to_vec()))
                    .collect(),
            ),
            honour_range,
            ..Self::default()
        }
    }

    async fn serve(self) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(self).mount(&server).await;
        server
    }
}

impl Respond for Files {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let Some(data) = self.files.get(req.url.path()) else {
            return ResponseTemplate::new(404);
        };
        let range = req.headers.get("range").and_then(|v| v.to_str().ok());
        let Some(range) = range.filter(|_| self.honour_range) else {
            self.served.fetch_add(data.len() as u64, Ordering::SeqCst);
            return ResponseTemplate::new(200).set_body_bytes(data.clone());
        };
        let (a, b) = range.trim_start_matches("bytes=").split_once('-').unwrap();
        let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
        if a >= data.len() {
            return ResponseTemplate::new(416);
        }
        let b = b.min(data.len() - 1);
        self.served.fetch_add((b - a + 1) as u64, Ordering::SeqCst);
        ResponseTemplate::new(206)
            .insert_header("content-range", format!("bytes {a}-{b}/{}", data.len()))
            .set_body_bytes(data[a..=b].to_vec())
    }
}

fn streams(tmp: &tempfile::TempDir) -> Arc<Streams> {
    Arc::new(Streams::for_tests(tmp.path(), "http://127.0.0.1:9/licence"))
}

fn urls(server: &MockServer, paths: &[&str]) -> Vec<String> {
    paths
        .iter()
        .map(|p| format!("{}{p}", server.uri()))
        .collect()
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ── Context ──────────────────────────────────────────────────────────────────

/// The engine shares one context across tasks.
#[test]
fn streams_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Streams>();
}

/// Creating the context must not touch the disk: the caches are created on first use.
#[test]
fn construction_has_no_filesystem_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let _s = Streams::with_cache_root(reqwest::Client::new(), &root);
    assert!(!root.exists());
}

// ── Segment downloads ────────────────────────────────────────────────────────

#[tokio::test]
async fn mv_segments_arrive_in_order() {
    let segs: [&[u8]; 4] = [
        b"segment-zero-bytes",
        b"segment-one-bytes",
        b"segment-two-bytes",
        b"segment-three-bytes",
    ];
    let server = Files::new(
        &[
            ("/0", segs[0]),
            ("/1", segs[1]),
            ("/2", segs[2]),
            ("/3", segs[3]),
        ],
        false,
    )
    .serve()
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let mut out = Vec::new();
    s.download_mv_segments_streaming(
        &urls(&server, &["/0", "/1", "/2", "/3"]),
        &mut out,
        3,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(out, segs.concat());

    // The MV cache now serves a replay under a fresh signed URL, with no request.
    let before = server.received_requests().await.unwrap().len();
    let replay: Vec<String> = urls(&server, &["/0", "/1", "/2", "/3"])
        .iter()
        .map(|u| format!("{u}?accessKey=new"))
        .collect();
    let mut again = Vec::new();
    s.download_mv_segments_streaming(&replay, &mut again, 3, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(again, out);
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn mv_segments_empty_and_single() {
    let server = Files::new(&[("/only", b"only-segment")], false)
        .serve()
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let mut out = Vec::new();
    s.download_mv_segments_streaming(&[], &mut out, 3, &CancellationToken::new())
        .await
        .unwrap();
    assert!(out.is_empty());
    s.download_mv_segments_streaming(
        &urls(&server, &["/only"]),
        &mut out,
        3,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(out, b"only-segment");
}

#[tokio::test]
async fn mv_segments_stop_on_cancellation() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel();
    });
    let res = tokio::time::timeout(
        Duration::from_secs(2),
        s.download_mv_segments_streaming(
            &urls(&server, &["/0", "/1"]),
            &mut Vec::new(),
            2,
            &cancel,
        ),
    )
    .await
    .expect("cancellation did not stop the download");
    assert!(res.is_err());
}

/// While segment 1 still drains, segment 2's request must already be open, so the boundary between them does not
/// leave the link idle for a request round trip.
#[tokio::test]
async fn next_mv_segment_is_requested_while_draining() {
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(move |req: &Request| {
            let _ = seen_tx.send(req.url.path().to_owned());
            let size = if req.url.path() == "/s0" { 8 } else { 4096 };
            ResponseTemplate::new(200).set_body_bytes(vec![0; size])
        })
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    s.set_mv_cache_max_bytes(0);

    // A pipe far smaller than a segment, read by nobody yet: segment 1 stays mid-drain.
    let (mut w, mut r) = tokio::io::duplex(64);
    let list = urls(&server, &["/s0", "/s1", "/s2", "/s3"]);
    let download = tokio::spawn(async move {
        s.download_mv_segments_streaming(&list, &mut w, 5, &CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while seen_rx.recv().await.as_deref() != Some("/s2") {}
    })
    .await
    .expect("segment 2 was not requested while segment 1 was still draining");

    let mut out = Vec::new();
    r.read_to_end(&mut out).await.unwrap();
    download.await.unwrap().unwrap();
    assert_eq!(out.len(), 8 + 3 * 4096);
}

#[tokio::test]
async fn audio_segments_keep_playlist_order() {
    let server = MockServer::start().await;
    for (i, delay) in [300u64, 0, 100, 0, 50].into_iter().enumerate() {
        Mock::given(wiremock::matchers::path(format!("/{i}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(format!("<{i}>"))
                    .set_delay(Duration::from_millis(delay)),
            )
            .mount(&server)
            .await;
    }
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let list = urls(&server, &["/0", "/1", "/2", "/3", "/4"]);
    let mut out = Vec::new();
    s.download_segments_parallel(&list, &mut out, 3, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out, b"<0><1><2><3><4>");

    // A failing segment names its index.
    let mut list = list;
    list.insert(2, format!("{}/missing", server.uri()));
    let err = s
        .download_segments_parallel(&list, &mut Vec::new(), 3, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Segment { index: 2, .. }), "{err}");
}

/// A CDN that ignores Range answers 200 with the whole file; that must not be accepted (and cached) as the
/// requested segment.
#[tokio::test]
async fn a_200_for_a_range_is_rejected() {
    let server = Files::new(&[("/seg.mp4", &[b'x'; 1000])], false)
        .serve()
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    s.segments.set_max_bytes(0);
    let url = format!("{}/seg.mp4#bytes=0-99", server.uri());
    let err = s
        .fetch_segment(&url, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::RangeIgnored(_)), "{err}");
}

#[tokio::test]
async fn byte_ranges_are_requested_and_cached_per_range() {
    let file: Vec<u8> = (0..=255).collect();
    let files = Files::new(&[("/f.mp4", &file)], true);
    let server = files.clone().serve().await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let base = format!("{}/f.mp4?accessKey=1", server.uri());
    let a = s
        .fetch_segment(&format!("{base}#bytes=0-9"), &CancellationToken::new())
        .await
        .unwrap();
    let b = s
        .fetch_segment(&format!("{base}#bytes=10-19"), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!((&a[..], &b[..]), (&file[..10], &file[10..20]));
    let again = s
        .fetch_segment(
            &format!("{}/f.mp4?accessKey=2#bytes=0-9", server.uri()),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(again, a);
    assert_eq!(
        files.requests.load(Ordering::SeqCst),
        2,
        "the replay was not served from the cache"
    );
}

#[tokio::test]
async fn failed_requests_are_retried() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes("ok"))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let data = s
        .fetch_segment(&format!("{}/x", server.uri()), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(&data[..], b"ok");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

// ── Seek priority ────────────────────────────────────────────────────────────

#[tokio::test]
async fn background_downloads_wait_for_a_hold_but_seek_downloads_do_not() {
    let server = Files::new(&[("/a", b"aaaa"), ("/b", b"bbbb")], false)
        .serve()
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let hold = s.hold_background(Duration::from_secs(60));

    let (list, mut sink, cancel) = (
        urls(&server, &["/a", "/b"]),
        Vec::new(),
        CancellationToken::new(),
    );
    let prio =
        crate::with_seek_priority(s.download_mv_segments_streaming(&list, &mut sink, 2, &cancel));
    tokio::time::timeout(Duration::from_secs(2), prio)
        .await
        .expect("seek download was parked")
        .unwrap();

    let bg = {
        let (s, list) = (Arc::clone(&s), urls(&server, &["/a", "/b"]));
        tokio::spawn(async move {
            let mut out = Vec::new();
            s.download_mv_segments_streaming(&list, &mut out, 2, &CancellationToken::new())
                .await
                .map(|()| out)
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!bg.is_finished(), "background download ran during a hold");
    drop(hold);
    let out = tokio::time::timeout(Duration::from_secs(2), bg)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(out, b"aaaabbbb");
}

// ── Direct seek ──────────────────────────────────────────────────────────────

const DIRECT_INIT: &[u8] = include_bytes!("../tests/data/direct_init.mp4");
const DIRECT_SEG: &[u8] = include_bytes!("../tests/data/direct_seg.m4s");
const DIRECT_NEXT: &[u8] = include_bytes!("../tests/data/direct_next.m4s");
/// Moof offsets of the five one-second fragments of `direct_seg.m4s` (raw time 10..15 s, timescale 24).
const DIRECT_OFFS: [u64; 5] = [0, 4108, 8216, 12324, 16432];

async fn plan(
    s: &Arc<Streams>,
    server: &MockServer,
    seg: &str,
    rest: &[&str],
    offset: f64,
) -> Result<crate::DirectPlan, Error> {
    let u = |p: &str| format!("{}{p}", server.uri());
    s.plan_direct_seek(
        &u("/init.mp4"),
        &u(seg),
        rest.iter().map(|p| u(p)).collect(),
        offset,
        &CancellationToken::new(),
    )
    .await
}

/// Known answers from the Go engine for the mp4ff-built fixtures.
#[tokio::test]
async fn direct_plan_matches_go() {
    let server = Files::new(
        &[
            ("/init.mp4", DIRECT_INIT),
            ("/s1.m4s", DIRECT_SEG),
            ("/s2.m4s", DIRECT_NEXT),
        ],
        true,
    )
    .serve()
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    s.set_mv_cache_max_bytes(0);
    // (offset into the segment, chosen fragment, probes, Go's stream length and SHA-256)
    let cases = [
        (
            -5.0,
            0,
            2,
            27284,
            "a616714be496e3af4ab83386f995b2dbc0546f383fe05aed277a5c5a271f1e17",
        ),
        (
            0.2,
            0,
            2,
            27284,
            "a616714be496e3af4ab83386f995b2dbc0546f383fe05aed277a5c5a271f1e17",
        ),
        (
            2.5,
            2,
            4,
            19068,
            "dea426dedd44f106b4c5d9955eef4cae37d7a8e48aab7bb29792cfc1a2d54f8a",
        ),
        (
            3.0,
            3,
            5,
            14960,
            "886f40e3e3ede7e3d4a12ae8f6ec9ef039d45a195d5b3c922374f6ad2c55bb36",
        ),
        (
            3.97,
            4,
            5,
            10852,
            "0a7051f234a91f5fcf8e0b7a6616cb8f0d49b0e33995a877135c5baad5a966ac",
        ),
        (
            4.9,
            4,
            5,
            10852,
            "0a7051f234a91f5fcf8e0b7a6616cb8f0d49b0e33995a877135c5baad5a966ac",
        ),
        (
            89.0,
            4,
            5,
            10852,
            "0a7051f234a91f5fcf8e0b7a6616cb8f0d49b0e33995a877135c5baad5a966ac",
        ),
    ];
    for (offset, frag, probes, len, hash) in cases {
        let p = plan(&s, &server, "/s1.m4s", &["/s2.m4s"], offset)
            .await
            .unwrap();
        assert_eq!(p.frag_off, DIRECT_OFFS[frag], "offset {offset}");
        assert_eq!(
            (p.raw_frag_start, p.raw_seg_start),
            (10.0 + frag as f64, 10.0),
            "offset {offset}"
        );
        assert_eq!(p.probes, probes, "offset {offset}");
        assert_eq!(p.seg_size, DIRECT_SEG.len() as u64);
        let mut out = Vec::new();
        p.stream(&mut out, &CancellationToken::new()).await.unwrap();
        assert_eq!(
            (out.len(), sha(&out).as_str()),
            (len, hash),
            "offset {offset}"
        );
        let want = [
            DIRECT_INIT,
            &DIRECT_SEG[DIRECT_OFFS[frag] as usize..],
            DIRECT_NEXT,
        ]
        .concat();
        assert_eq!(out, want);
    }

    // The last segment has nothing to continue with.
    let p = plan(&s, &server, "/s2.m4s", &[], 1.2).await.unwrap();
    let mut out = Vec::new();
    p.stream(&mut out, &CancellationToken::new()).await.unwrap();
    assert_eq!(
        out,
        [DIRECT_INIT, &DIRECT_NEXT[p.frag_off as usize..]].concat()
    );
    assert!(p.frag_off > 0);
}

/// Planning reads 16 KiB windows, never the fragments' payloads.
#[tokio::test]
async fn direct_plan_only_fetches_headers() {
    let init = InitSegment::single_track(24, MediaType::Video)
        .unwrap()
        .to_bytes()
        .unwrap();
    let mut seg = Vec::new();
    for i in 0..5u32 {
        let mut frag = Fragment::new(i + 1, 1);
        let sample = TrunSample {
            duration: 24,
            size: 200_000,
            flags: 0x0200_0000,
            ..TrunSample::default()
        };
        frag.add_full_sample(sample, u64::from(10 + i) * 24, &vec![i as u8; 200_000]);
        frag.encode_into(&mut seg).unwrap();
    }
    let files = Files::new(&[("/init.mp4", &init), ("/s1.m4s", &seg)], true);
    let server = files.clone().serve().await;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let p = plan(&s, &server, "/s1.m4s", &[], 3.5).await.unwrap();
    assert_eq!(p.raw_frag_start, 13.0);
    let segment_bytes = files.served.load(Ordering::SeqCst) - init.len() as u64;
    assert!(
        segment_bytes <= 5 * (16 << 10),
        "planning transferred {segment_bytes} bytes"
    );
}

#[tokio::test]
async fn direct_plan_signals_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let ignores_range = Files::new(
        &[("/init.mp4", DIRECT_INIT), ("/s1.m4s", DIRECT_SEG)],
        false,
    )
    .serve()
    .await;
    let err = plan(&s, &ignores_range, "/s1.m4s", &[], 1.0)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::DirectUnsupported(_)),
        "server ignoring Range: {err}"
    );
    let err = plan(&s, &ignores_range, "/s1.m4s#bytes=0-100", &[], 1.0)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::DirectUnsupported(_)),
        "byte-range URL: {err}"
    );

    let not_fmp4 = Files::new(
        &[("/init.mp4", DIRECT_INIT), ("/s1.m4s", &[b'x'; 100])],
        true,
    )
    .serve()
    .await;
    let err = plan(&s, &not_fmp4, "/s1.m4s", &[], 1.0).await.unwrap_err();
    assert!(
        matches!(err, Error::DirectUnsupported(_)),
        "non-fMP4 segment: {err}"
    );
}

/// The source maps the playlist time to the fragment: the raw timeline here is 10 s ahead of the playlist's.
#[tokio::test]
async fn mv_video_source_direct_seek_reports_playlist_times() {
    use rustet_core::pipeline::DirectSeekable;
    let server = Files::new(
        &[
            ("/init.mp4", DIRECT_INIT),
            ("/s1.m4s", DIRECT_SEG),
            ("/s2.m4s", DIRECT_NEXT),
        ],
        true,
    )
    .serve()
    .await;
    let media = rustet_hls::Media {
        init_url: Some(format!("{}/init.mp4", server.uri())),
        segments: vec![
            rustet_hls::Segment {
                url: format!("{}/s1.m4s", server.uri()),
                duration: 5.0,
            },
            rustet_hls::Segment {
                url: format!("{}/s2.m4s", server.uri()),
                duration: 2.0,
            },
        ],
        encryption: None,
    };
    let tmp = tempfile::tempdir().unwrap();
    let source = crate::license::MvVideoSource::new(streams(&tmp), media);
    let (src, info) = source
        .direct_source_from(&CancellationToken::new(), 2.5)
        .await
        .unwrap();
    assert_eq!((info.start_sec, info.ts_offset), (2.0, -10.0));
    let mut out = Vec::new();
    src.stream(&StreamContext::default(), &mut out)
        .await
        .unwrap();
    assert_eq!(
        out,
        [
            DIRECT_INIT,
            &DIRECT_SEG[DIRECT_OFFS[2] as usize..],
            DIRECT_NEXT
        ]
        .concat()
    );

    assert_eq!(source.segment_timings(), Some(vec![0.0, 5.0]));
    let (_, actual) = source.source_from(6.0);
    assert_eq!(actual, 5.0);
}

// ── Licence ──────────────────────────────────────────────────────────────────

const TRACK: LicenceRequest<'static> = LicenceRequest {
    adam_id: "1440833098",
    kid_base64: "MDEyMzQ1Njc4OWFiY2RlZg==",
    uri_prefix: "skd://itunes",
    token: "dev-token",
    media_user_token: "user-token",
};

/// A refusal is told apart from a transport failure, and nothing is cached for it.
#[tokio::test]
async fn a_refused_licence_is_reported_as_such() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"errorCode":-42604,"status":-1021}"#),
        )
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let s = Arc::new(Streams::for_tests(tmp.path(), &server.uri()));
    let err = s
        .open_licence(&TRACK, &CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(err.is_licence_refused(), "{err}");
    assert_eq!(
        err.to_string(),
        "fairplay licence: error in license response, code: -42604, status: -1021"
    );
    assert!(!s.has_cached_key(TRACK.kid_base64, TRACK.uri_prefix));

    let unreachable = Arc::new(Streams::for_tests(tmp.path(), "http://127.0.0.1:9/licence"));
    let err = unreachable
        .open_licence(&TRACK, &CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(!err.is_licence_refused(), "{err}");
}

/// A decryption failure evicts the cached key, so the next open goes back to the licence server.
#[tokio::test]
async fn a_failed_decrypt_evicts_the_key() {
    use rustet_core::pipeline::Decryptor;
    let tmp = tempfile::tempdir().unwrap();
    let s = streams(&tmp);
    let key = rustet_widevine::wvkey::ContentKey {
        key: vec![1; 16],
        iv: [0; 16],
    };
    s.seed_key(TRACK.kid_base64, TRACK.uri_prefix, key);
    let dec = s
        .open_licence(&TRACK, &CancellationToken::new())
        .await
        .unwrap();

    let ctx = StreamContext::default();
    let res = dec
        .decrypt(&ctx, &mut &b"not an mp4 stream"[..], &mut Vec::new())
        .await;
    assert!(res.is_err());
    assert!(
        !s.has_cached_key(TRACK.kid_base64, TRACK.uri_prefix),
        "stale key kept after a decrypt failure"
    );
}

// ── AAC init length ──────────────────────────────────────────────────────────

/// The mvhd duration and timescale of the init segment at the start of `data`.
fn mvhd_of(data: &[u8]) -> (u64, u64) {
    let mut at = 0;
    loop {
        let (b, used) = rustet_mp4::Mp4Box::decode(&data[at..], at as u64).unwrap();
        if let rustet_mp4::Mp4Box::Moov(m) = &b {
            let mvhd = m.mvhd().unwrap();
            return (mvhd.duration(), u64::from(mvhd.timescale()));
        }
        at += used;
    }
}

/// Both the whole-track source and the seek source write the playlist's length into the init segment, so libvlc
/// can play the cache file while it grows. The cached segment keeps Apple's bytes.
#[tokio::test]
async fn audio_sources_write_the_track_length_into_the_init() {
    use rustet_core::pipeline::Source;
    let fixture = include_bytes!("../tests/data/cbcs_audio.in.mp4");
    let mut init_len = 0;
    loop {
        let (b, used) = rustet_mp4::Mp4Box::decode(&fixture[init_len..], init_len as u64).unwrap();
        init_len += used;
        if b.kind() == rustet_mp4::FourCC::MOOV {
            break;
        }
    }
    let (init, seg) = (&fixture[..init_len], b"media-bytes".as_slice());
    let server = Files::new(
        &[("/init.mp4", init), ("/s0.m4s", seg), ("/s1.m4s", seg)],
        false,
    )
    .serve()
    .await;
    let at = |p: &str| format!("{}{p}", server.uri());
    let media = rustet_hls::Media {
        init_url: Some(at("/init.mp4")),
        segments: vec![
            rustet_hls::Segment {
                url: at("/s0.m4s"),
                duration: 10.0,
            },
            rustet_hls::Segment {
                url: at("/s1.m4s"),
                duration: 2.5,
            },
        ],
        encryption: None,
    };
    let tmp = tempfile::tempdir().unwrap();
    let source = crate::license::HlsSource::new(streams(&tmp), media);
    let ctx = StreamContext::new(CancellationToken::new());

    let mut out = Vec::new();
    source.stream(&ctx, &mut out).await.unwrap();
    let (duration, timescale) = mvhd_of(&out);
    assert_eq!(duration, 12_500 * timescale / 1000);
    assert!(out.ends_with(b"media-bytesmedia-bytes"));

    let (seek, _) = source.source_from(11.0);
    let mut out = Vec::new();
    seek.stream(&ctx, &mut out).await.unwrap();
    assert_eq!(
        mvhd_of(&out).0,
        duration,
        "the seek stream keeps the whole track's length"
    );

    // A replay reads the cached init: the patch is applied again, the cache holds Apple's bytes.
    let mut out = Vec::new();
    source.stream(&ctx, &mut out).await.unwrap();
    assert_eq!(mvhd_of(&out).0, duration);
}
