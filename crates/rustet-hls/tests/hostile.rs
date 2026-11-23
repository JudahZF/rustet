//! Playlists come from the network, and a panic in a background task would take down the engine. Every parser must
//! return, never panic, on any input. Ported from FuzzParseMedia / FuzzParseMaster and the m3u8safe tests.

use proptest::prelude::*;
use rustet_hls::{CbcsMedia, Master, Media};
use url::Url;

/// Runs every parser and every helper on the result. Only panics matter; errors are fine.
fn exercise(body: &str, seek: f64) {
    let base = Url::parse("https://example.test/a/p.m3u8").unwrap();
    if let Ok(m) = Master::parse(&base, body) {
        let _ = m.select_audio_variant(&["audio-atmos", "audio-stereo-256"]);
        let _ = m.select_by_codec("alac");
        let _ = m.video_heights();
        if let Ok(v) = m.select_video_variant(1080) {
            let _ = m.video_fallback(v);
        }
    }
    if let Ok(med) = Media::parse(&base, body) {
        let _ = med.urls_from(seek);
        let _ = med.urls_from_exact(seek);
        let _ = med.segment_for(seek);
        let _ = med.cumulative_segment_times();
    }
    let _ = CbcsMedia::parse(&base, body);
}

/// Decodes one Go fuzz corpus file (`go test fuzz v1` + a `[]byte("...")` literal).
fn decode_go_corpus(file: &str) -> Vec<u8> {
    let literal = file
        .lines()
        .find_map(|l| {
            l.strip_prefix("[]byte(\"")
                .and_then(|l| l.strip_suffix("\")"))
        })
        .expect("corpus entry");
    let mut out = Vec::new();
    let mut chars = literal.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&hex, 16).expect("hex escape"));
            }
            Some(other) => out.push(other as u8),
            None => {}
        }
    }
    out
}

#[test]
fn fuzz_corpus_regressions() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/fuzz/FuzzParseMedia");
    let mut entries = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let body = decode_go_corpus(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
        let body = String::from_utf8_lossy(&body);
        exercise(&body, 5.0);
        assert!(
            Media::parse(&Url::parse("https://example.test/a/p.m3u8").unwrap(), &body).is_err()
        );
        entries += 1;
    }
    assert!(entries > 0, "corpus is empty");
}

#[test]
fn hostile_inputs() {
    let cases = [
        "",
        "\n\n\r\n",
        "#EXTM3U",
        "#EXT-X-MAP:0\n0",
        "#EXTM3U\n#EXT-X-MAP:\n0\n",
        "#EXTM3U\n#EXT-X-KEY:URI=\"\nx\n",
        "#EXTM3U\n#EXT-X-BYTERANGE:5@\nx\n",
        "#EXTM3U\n#EXTINF:\nx\n",
        "#EXTM3U\n#EXTINF:nan,\nx\n#EXTINF:-inf,\ny\n",
        "#EXTM3U\n#EXTINF:1e308,\nx\n#EXTINF:1e308,\ny\n",
        "#EXTM3U\n#EXTINF:4,\n#EXT-X-BYTERANGE:9223372036854775807@9223372036854775807\nx\n",
        "#EXTM3U\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"-1@-9223372036854775808\"\n#EXTINF:1,\nx\n",
        "#EXTM3U\n#EXT-X-STREAM-INF:\n#EXT-X-STREAM-INF:BANDWIDTH=1\n",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=99999999999999,RESOLUTION=99999999999x99999999999\nhttp://[\n",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=x\n_99999999999999999999x99999999999999999999.m3u8\n",
        "#EXTM3U\n#EXT-X-MEDIA:URI=\"\u{ff}\u{0}\",GROUP-ID=\"audio-atmos\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\n\u{2028}\n",
        "#EXTM3U\n#EXT-X-MEDIA:URI=\"a_gr99999999999999999999_\",GROUP-ID=\"audio-atmos\"\n",
        "#EXTM3U\n#EXT-X-I-FRAME-STREAM-INF:URI=\"=\",,,=,\"\n",
        "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://a\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:1,\n#EXT-X-BYTERANGE:0\nf\n",
    ];
    for case in cases {
        for seek in [-1.0, 0.0, 3.5, 1e300, f64::NAN, f64::INFINITY] {
            exercise(case, seek);
        }
    }
}

/// Lines built from the tags the parser knows, mangled values and plain junk.
fn line() -> impl Strategy<Value = String> {
    let tag = prop::sample::select(vec![
        "#EXTM3U",
        "#EXT-X-ENDLIST",
        "#EXT-X-VERSION:",
        "#EXTINF:",
        "#EXT-X-BYTERANGE:",
        "#EXT-X-KEY:",
        "#EXT-X-MAP:",
        "#EXT-X-MEDIA:",
        "#EXT-X-STREAM-INF:",
        "#EXT-X-I-FRAME-STREAM-INF:",
        "",
    ]);
    let value = prop::sample::select(vec![
        "",
        "4,",
        "4.5",
        "100@0",
        "@",
        "-1@-1",
        "URI=\"seg.mp4\"",
        "URI=\"skd://k,a2lk\",METHOD=SAMPLE-AES,KEYFORMAT=\"com.apple.streamingkeydelivery\"",
        "BYTERANGE=\"10@5\",URI=\"i.mp4\"",
        "BANDWIDTH=10,RESOLUTION=1920x1080,CODECS=\"avc1,hvc1\"",
        "GROUP-ID=\"audio-atmos\",URI=\"a_gr3_.m3u8\"",
        "\"",
        "seg_1920x1080.mp4",
    ]);
    prop_oneof![
        (tag, value).prop_map(|(t, v)| format!("{t}{v}")),
        any::<String>(),
    ]
}

proptest! {
    #[test]
    fn parsers_never_panic(lines in prop::collection::vec(line(), 0..40), seek in any::<f64>()) {
        exercise(&lines.join("\n"), seek);
    }

    #[test]
    fn parsers_never_panic_on_raw_text(body in any::<String>()) {
        exercise(&body, 1.0);
    }
}
