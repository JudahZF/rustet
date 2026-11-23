//! Ported from engine/core/hls: fixture parsing over a local HTTP server, selection rules, key info, HDCP and
//! ALLOWED-CPC, licence fallbacks.

use rustet_hls::{Alternative, Auth, CbcsMedia, HlsError, Master, Media, Variant, cpc_summary};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Serves `tests/data/<name>` over HTTP so tests never touch the real network. Paths under `/auth/` answer 403
/// unless the Apple Music headers are present. Returns the base URL.
async fn fixture_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match conn.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let authed = head.contains("authorization: bearer dev-token")
                    && head.contains("x-apple-music-user-token: user-token")
                    && head.contains("origin: https://music.apple.com");
                let (name, allowed) = match path.strip_prefix("/auth/") {
                    Some(name) => (name.to_owned(), authed),
                    None => (path.trim_start_matches('/').to_owned(), true),
                };
                let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/data")
                    .join(name);
                let response = match (allowed, std::fs::read(file)) {
                    (false, _) => {
                        b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec()
                    }
                    (true, Err(_)) => {
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec()
                    }
                    (true, Ok(body)) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.apple.mpegurl\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(&body);
                        r
                    }
                };
                let _ = conn.write_all(&response).await;
            });
        }
    });
    base
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name),
    )
    .unwrap()
}

fn base() -> Url {
    Url::parse("https://example.test/a/master.m3u8").unwrap()
}

#[tokio::test]
async fn open_master_resolves_audio_variants() {
    let srv = fixture_server().await;
    let m = Master::open(
        &reqwest::Client::new(),
        &format!("{srv}/master_audio.m3u8"),
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(m.variants.len(), 3);
    for v in &m.variants {
        assert!(
            v.url.starts_with(&srv),
            "variant URL {} not resolved to absolute",
            v.url
        );
        assert!(v.bandwidth > 0, "variant {} has zero bandwidth", v.url);
    }
}

#[tokio::test]
async fn open_master_with_alternatives() {
    let srv = fixture_server().await;
    let m = Master::open(
        &reqwest::Client::new(),
        &format!("{srv}/master_video.m3u8"),
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let group = |id: &str| m.alternatives.iter().find(|a| a.group_id == id);
    assert!(group("audio-stereo-256").is_some());
    let atmos = group("audio-atmos").expect("atmos group");
    assert!(
        atmos.uri.contains("_gr3_"),
        "atmos URI {} lost its _gr3_ rank marker",
        atmos.uri
    );
    assert!(atmos.uri.starts_with(&srv));
}

#[tokio::test]
async fn open_master_wraps_a_media_playlist() {
    let srv = fixture_server().await;
    let url = format!("{srv}/media_unencrypted.m3u8");
    let m = Master::open(
        &reqwest::Client::new(),
        &url,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(m.variants.len(), 1);
    assert_eq!(m.variants[0].url, url);
}

#[tokio::test]
async fn open_sends_apple_auth_headers() {
    let srv = fixture_server().await;
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("{srv}/auth/media_encrypted.m3u8");
    let auth = Auth {
        developer_token: "dev-token",
        media_user_token: "user-token",
    };
    assert!(
        Media::open(&client, &url, Some(auth), &cancel)
            .await
            .is_ok()
    );
    assert!(
        Master::open(&client, &url, Some(auth), &cancel)
            .await
            .is_ok()
    );
    assert!(matches!(
        Media::open(&client, &url, None, &cancel).await,
        Err(HlsError::Status { status, .. }) if status == reqwest::StatusCode::FORBIDDEN
    ));
}

#[tokio::test]
async fn open_honours_cancellation() {
    let srv = fixture_server().await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = Media::open(
        &reqwest::Client::new(),
        &format!("{srv}/media_encrypted.m3u8"),
        None,
        &cancel,
    )
    .await;
    assert!(matches!(result, Err(HlsError::Cancelled)));
}

#[tokio::test]
async fn open_media_encrypted() {
    let srv = fixture_server().await;
    let med = Media::open(
        &reqwest::Client::new(),
        &format!("{srv}/media_encrypted.m3u8"),
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(med.init_url.as_deref().unwrap().ends_with("/init.mp4"));
    assert_eq!(med.segments.len(), 3);
    let enc = med.encryption.as_ref().expect("encryption info");
    assert_eq!(enc.kid_base64, "base64kidhere==");
    assert_eq!(enc.uri_prefix, "skd://key001");

    let all = med.all_urls();
    assert_eq!(all.len(), 4);
    for (url, suffix) in all
        .iter()
        .zip(["/init.mp4", "/seg0.mp4", "/seg1.mp4", "/seg2.mp4"])
    {
        assert!(url.ends_with(suffix), "{url} lacks {suffix}");
    }
}

#[test]
fn media_unencrypted() {
    let med = Media::parse(&base(), &fixture("media_unencrypted.m3u8")).unwrap();
    assert!(med.encryption.is_none());
    assert_eq!(med.segments.len(), 2);
}

#[test]
fn media_rejects_master() {
    assert!(matches!(
        Media::parse(&base(), &fixture("master_audio.m3u8")),
        Err(HlsError::NotMedia { .. })
    ));
}

#[test]
fn media_byte_ranges_and_last_init_map() {
    // Clear-leader CBCS: the init segment switches mid-playlist and the last one is the one that decrypts.
    let body = "#EXTM3U\n#EXT-X-MAP:URI=\"clear.mp4\",BYTERANGE=\"100@0\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:500@100\nfile.mp4\n\
                #EXT-X-MAP:URI=\"enc.mp4\",BYTERANGE=\"200@0\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:600\nfile.mp4\n";
    let med = Media::parse(&base(), body).unwrap();
    assert_eq!(
        med.init_url.as_deref(),
        Some("https://example.test/a/enc.mp4#bytes=0-199")
    );
    assert_eq!(
        med.segments[0].url,
        "https://example.test/a/file.mp4#bytes=100-599"
    );
    // grafov reads a BYTERANGE without @offset as offset 0.
    assert_eq!(
        med.segments[1].url,
        "https://example.test/a/file.mp4#bytes=0-599"
    );
}

const KEY_INFO_PLAYLIST: &str = r#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:10
#EXT-X-MAP:URI="init.mp4"
#EXT-X-KEY:METHOD=SAMPLE-AES,URI="data:;base64,QUJDREVGR0hJSktMTU5PUA==",KEYFORMAT="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed",KEYFORMATVERSIONS="1"
#EXTINF:5.0,
seg0.mp4
{mid}#EXTINF:5.0,
seg1.mp4
#EXT-X-ENDLIST
"#;

// The key attributes are kept for diagnostics: they show which key system a playlist declares, and a key that
// changes between segments shows as more than one distinct key URI.
#[test]
fn media_records_key_info() {
    let second = "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"data:;base64,UVJTVFVWV1hZWjAxMjM0NQ==\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n";
    for (mid, want_keys) in [("", 1), (second, 2)] {
        let med = Media::parse(&base(), &KEY_INFO_PLAYLIST.replace("{mid}", mid)).unwrap();
        let e = med.encryption.expect("encryption info");
        assert_eq!(e.method, "SAMPLE-AES");
        assert_eq!(
            e.key_format,
            "urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"
        );
        assert_eq!(e.uri_prefix, "data:;base64");
        assert_eq!(e.kid_base64, "QUJDREVGR0hJSktMTU5PUA==");
        assert_eq!(e.key_count, want_keys);
    }
}

fn timed(durations: &[f64]) -> Media {
    Media {
        init_url: Some("init".into()),
        segments: durations
            .iter()
            .enumerate()
            .map(|(i, &duration)| rustet_hls::Segment {
                url: format!("s{i}"),
                duration,
            })
            .collect(),
        encryption: None,
    }
}

#[test]
fn urls_from_past_end_reports_last_segments_start() {
    let m = Media {
        init_url: None,
        ..timed(&[10.0, 10.0, 10.0])
    };
    // Clamped to the last segment (index 2), then stepped back one: index 1 at 10 s.
    assert_eq!(m.urls_from(999.0), (vec!["s1".into(), "s2".into()], 10.0));
}

#[test]
fn seek_helpers() {
    let m = timed(&[4.0, 4.0, 4.0]);
    assert_eq!(m.urls_from(0.0), (m.all_urls(), 0.0));
    assert_eq!(
        m.urls_from(5.0),
        (
            vec!["init".into(), "s0".into(), "s1".into(), "s2".into()],
            0.0
        )
    );
    assert_eq!(
        m.urls_from(9.0),
        (vec!["init".into(), "s1".into(), "s2".into()], 4.0)
    );
    assert_eq!(
        m.urls_from_exact(5.0),
        (vec!["init".into(), "s1".into(), "s2".into()], 4.0)
    );
    assert_eq!(
        m.urls_from_exact(99.0),
        (vec!["init".into(), "s2".into()], 8.0)
    );
    assert_eq!(m.segment_for(-3.0), Some((0, 0.0)));
    assert_eq!(m.segment_for(8.0), Some((2, 8.0)));
    assert_eq!(timed(&[]).segment_for(1.0), None);
    assert_eq!(m.cumulative_segment_times(), vec![0.0, 4.0, 8.0]);
}

// ── Selection ────────────────────────────────────────────────────────────────

fn variant(url: &str, resolution: &str, codecs: &str, average_bandwidth: u32) -> Variant {
    Variant {
        url: url.into(),
        resolution: resolution.into(),
        codecs: codecs.into(),
        average_bandwidth,
        ..Variant::default()
    }
}

fn by_bandwidth(variants: &[(&str, u32, &str)]) -> Master {
    Master {
        variants: variants
            .iter()
            .map(|&(url, bandwidth, codecs)| Variant {
                url: url.into(),
                bandwidth,
                codecs: codecs.into(),
                ..Variant::default()
            })
            .collect(),
        alternatives: Vec::new(),
    }
}

#[test]
fn select_by_codec() {
    let m = by_bandwidth(&[
        ("a", 256000, "mp4a.40.2"),
        ("b", 320000, "mp4a.40.2"),
        ("c", 900000, "alac"),
    ]);
    assert_eq!(m.select_by_codec("mp4a.40.2"), Some("b"));
    assert_eq!(m.select_by_codec("ALAC"), Some("c"));
    // No ec-3 variant: the highest bandwidth overall.
    let m = by_bandwidth(&[("a", 256000, "mp4a.40.2"), ("b", 320000, "mp4a.40.2")]);
    assert_eq!(m.select_by_codec("ec-3"), Some("b"));
    // A media playlist wrapped as a master declares no bandwidth.
    assert_eq!(by_bandwidth(&[("a", 0, "")]).select_by_codec("alac"), None);
}

fn alternatives(alts: &[(&str, &str)]) -> Master {
    Master {
        variants: Vec::new(),
        alternatives: alts
            .iter()
            .map(|&(group_id, uri)| Alternative {
                group_id: group_id.into(),
                uri: uri.into(),
                ..Alternative::default()
            })
            .collect(),
    }
}

#[test]
fn select_audio_variant() {
    let m = alternatives(&[
        ("audio-stereo-256", "s_gr1_.m3u8"),
        ("audio-atmos", "a_gr1_.m3u8"),
    ]);
    assert_eq!(
        m.select_audio_variant(&["audio-atmos", "audio-stereo-256"])
            .unwrap(),
        "a_gr1_.m3u8"
    );

    let m = alternatives(&[
        ("audio-atmos", "a_gr1_.m3u8"),
        ("audio-atmos", "a_gr3_.m3u8"),
    ]);
    assert_eq!(
        m.select_audio_variant(&["audio-atmos"]).unwrap(),
        "a_gr3_.m3u8"
    );

    // No priority matches: the first rendition.
    let m = alternatives(&[("audio-stereo-256", "s_gr1_.m3u8")]);
    assert_eq!(
        m.select_audio_variant(&["audio-atmos"]).unwrap(),
        "s_gr1_.m3u8"
    );

    assert!(matches!(
        Master::default().select_audio_variant(&["audio-atmos"]),
        Err(HlsError::NoAudioAlternative)
    ));
}

#[test]
fn select_video_4k_needs_hevc() {
    // Apple's 4K variants: HEVC-only at 3840x1920, H.264 up to 1080p.
    let m = Master {
        variants: vec![
            variant(
                "http://x/4k.m3u8",
                "3840x1920",
                "hvc1.2.20000000.H150.B0",
                15000000,
            ),
            variant("http://x/1080p.m3u8", "1920x1080", "avc1.640032", 8000000),
            variant("http://x/720p.m3u8", "1280x720", "avc1.64001F", 4000000),
        ],
        alternatives: Vec::new(),
    };
    // video_heights offers 1920 for the HEVC tier, so the quality menu sends max height 1920.
    assert_eq!(
        m.select_video_variant(1920).unwrap().url,
        "http://x/4k.m3u8"
    );
    assert_eq!(
        m.select_video_variant(1080).unwrap().url,
        "http://x/1080p.m3u8"
    );
    assert_eq!(m.video_heights(), vec![720, 1080, 1920]);
}

// A request above the H.264 ceiling is only answered by an HEVC variant taller than that ceiling; an HEVC 1080p
// must never displace the H.264 1080p.
#[test]
fn select_video_above_ceiling_does_not_pick_lower_hevc() {
    let m = Master {
        variants: vec![
            variant(
                "http://x/hevc1080.m3u8",
                "1920x1080",
                "hvc1.2.4.L120",
                9000000,
            ),
            variant("http://x/avc1080.m3u8", "1920x1080", "avc1.640032", 8000000),
            variant("http://x/avc720.m3u8", "1280x720", "avc1.64001F", 4000000),
        ],
        alternatives: Vec::new(),
    };
    assert_eq!(
        m.select_video_variant(1440).unwrap().url,
        "http://x/avc1080.m3u8"
    );
}

#[test]
fn select_video_without_codecs() {
    let m = Master {
        variants: vec![
            Variant {
                url: "http://x/a.m3u8".into(),
                resolution: "1920x1080".into(),
                bandwidth: 8000000,
                ..Variant::default()
            },
            Variant {
                url: "http://x/b.m3u8".into(),
                resolution: "1280x720".into(),
                bandwidth: 4000000,
                ..Variant::default()
            },
        ],
        alternatives: Vec::new(),
    };
    assert_eq!(m.select_video_variant(720).unwrap().url, "http://x/b.m3u8");
    assert!(matches!(
        m.select_video_variant(480),
        Err(HlsError::NoVideoVariant { max_height: 480 })
    ));
}

#[test]
fn video_fallback() {
    let v = |url: &str, resolution: &str, codecs: &str, bandwidth: u32| Variant {
        url: format!("https://example.test/{url}.m3u8"),
        resolution: resolution.into(),
        codecs: codecs.into(),
        bandwidth,
        ..Variant::default()
    };
    let m = Master {
        variants: vec![
            v("uhd", "3840x1598", "hvc1.2.4.L153.B0", 20000000),
            v("hevc1080", "1920x806", "hvc1.1.6.L120.90", 9000000),
            v("hd", "1918x798", "avc1.640028", 10000000),
            v("hd-low", "1918x798", "avc1.640028", 6000000),
            v("sd", "1024x428", "avc1.64001f", 3000000),
            v("small", "864x360", "avc1.64001e", 1000000),
        ],
        alternatives: Vec::new(),
    };
    let fallback = |failed: &str| {
        let failed = m
            .variant_for(&format!("https://example.test/{failed}.m3u8"))
            .expect("test variant");
        m.video_fallback(failed)
            .map(|v| v.url.trim_start_matches("https://example.test/").to_owned())
    };
    // 4K falls back to the tallest H.264, not HEVC; then lower and lower.
    assert_eq!(fallback("uhd").as_deref(), Some("hd.m3u8"));
    assert_eq!(fallback("hd").as_deref(), Some("sd.m3u8"));
    assert_eq!(fallback("sd").as_deref(), Some("small.m3u8"));
    assert_eq!(fallback("small"), None);
    // A variant with no height produces no fallback.
    assert_eq!(
        m.video_fallback(&Variant {
            url: "https://example.test/unknown.m3u8".into(),
            ..Variant::default()
        }),
        None
    );
    assert!(m.variant_for("https://example.test/none.m3u8").is_none());
}

#[test]
fn variant_height() {
    let h = |v: Variant| v.height();
    assert_eq!(
        h(Variant {
            resolution: "3840x1598".into(),
            ..Variant::default()
        }),
        1598
    );
    assert_eq!(
        h(Variant {
            url: "https://cdn.test/x/MV_video_gr290_sdr_1918x798_cbcs.m3u8".into(),
            ..Variant::default()
        }),
        798
    );
    assert_eq!(h(Variant::default()), 0);
}

// ── HDCP and ALLOWED-CPC ─────────────────────────────────────────────────────

// A variant's HDCP-LEVEL says what output protection it demands: what separates a tier a software client may play
// from one it may not.
#[test]
fn master_keeps_hdcp_level() {
    let body = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=20000000,AVERAGE-BANDWIDTH=19000000,CODECS="hvc1.2.20000000.H150.B0,mp4a.40.2",RESOLUTION=3840x1598,VIDEO-RANGE=SDR,HDCP-LEVEL=TYPE-1
uhd.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=10000000,AVERAGE-BANDWIDTH=9500000,CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1918x798,VIDEO-RANGE=SDR
hd.m3u8
"#;
    let m = Master::parse(&base(), body).unwrap();
    assert_eq!(m.variants.len(), 2);
    assert_eq!(m.variants[0].hdcp_level, "TYPE-1");
    assert_eq!(m.variants[0].codecs, "hvc1.2.20000000.H150.B0,mp4a.40.2");
    assert_eq!(m.variants[0].average_bandwidth, 19000000);
    assert_eq!(m.variants[0].url, "https://example.test/a/uhd.m3u8");
    assert_eq!(m.variants[1].hdcp_level, "");
}

// ALLOWED-CPC names the key systems (and their security levels) that may play a variant.
#[test]
fn master_keeps_allowed_cpc() {
    let body = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=20000000,CODECS="hvc1.2.4,mp4a.40.2",RESOLUTION=3840x1598,ALLOWED-CPC="com.apple.streamingkeydelivery:Main/AppleMain,com.microsoft.playready:SL3000"
uhd.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=10000000,CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1918x798,ALLOWED-CPC="com.microsoft.playready:SL2000,urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed:WIDEVINE_SOFTWARE"
hd.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1000000,CODECS="avc1.64001e,mp4a.40.2",RESOLUTION=864x360
sd.m3u8
"#;
    let m = Master::parse(&base(), body).unwrap();
    let cpc: Vec<&str> = m.variants.iter().map(|v| v.allowed_cpc.as_str()).collect();
    assert_eq!(
        cpc,
        [
            "com.apple.streamingkeydelivery:Main/AppleMain,com.microsoft.playready:SL3000",
            "com.microsoft.playready:SL2000,urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed:WIDEVINE_SOFTWARE",
            "",
        ]
    );
}

#[test]
fn cpc_summary_names_key_systems() {
    assert_eq!(cpc_summary(""), "unspecified");
    assert_eq!(
        cpc_summary("com.apple.streamingkeydelivery:Main/AppleMain,com.microsoft.playready:SL3000"),
        "fairplay:Main/AppleMain playready:SL3000"
    );
    assert_eq!(
        cpc_summary("urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed:WIDEVINE_SOFTWARE"),
        "widevine:WIDEVINE_SOFTWARE"
    );
}

// ── CBCS ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn open_cbcs_alac_byterange() {
    let srv = fixture_server().await;
    let med = CbcsMedia::open(
        &reqwest::Client::new(),
        &format!("{srv}/media_cbcs_alac.m3u8"),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(med.file_url, format!("{srv}/track_m.mp4"));
    let keys: Vec<Option<&str>> = med.segments.iter().map(|s| s.key_uri.as_deref()).collect();
    // Segment 0: prefetch key; segment 1: content key; later segments inherit (no key change).
    assert_eq!(
        keys,
        [
            Some("skd://itunes.apple.com/P000000000/s1/e1"),
            Some("skd://itunes.apple.com/p1238037727/c6"),
            None,
            None
        ]
    );
    assert_eq!(med.segments[0].duration, 14.951836735);
    // skd:// URIs carry no comma, so there is no key ID.
    assert_eq!(med.kid_base64, None);
}

#[test]
fn cbcs_drops_foreign_keys() {
    // Identity and streamingkeydelivery keys side by side: only the FairPlay URIs survive.
    let med = CbcsMedia::parse(&base(), &fixture("media_cbcs_mixed_keys.m3u8")).unwrap();
    let keys: Vec<Option<&str>> = med.segments.iter().map(|s| s.key_uri.as_deref()).collect();
    assert_eq!(
        keys,
        [
            Some("skd://itunes.apple.com/P000000000/s1/e1"),
            Some("skd://itunes.apple.com/p1238037727/c6")
        ]
    );
}

#[test]
fn cbcs_requires_byte_ranges() {
    assert!(matches!(
        CbcsMedia::parse(&base(), &fixture("media_unencrypted.m3u8")),
        Err(HlsError::CbcsNotByteRange { .. })
    ));
    assert!(matches!(
        CbcsMedia::parse(
            &base(),
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-ENDLIST\n"
        ),
        Err(HlsError::CbcsNoSegments { .. })
    ));
}
