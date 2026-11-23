//! The lyrics model from Apple's TTML, and its loader.

mod pages_common;

use std::time::Duration;

use pages_common::{env, json};
use rustet_ampapi::api::UserToken;
use rustet_app::pages::LoadError;
use rustet_app::pages::lyrics::{self, LyricLines, Lyrics};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn synced(lyrics: &Lyrics) -> &[lyrics::TimedLine] {
    match &lyrics.lines {
        LyricLines::Synced(lines) => lines,
        LyricLines::Unsynced(_) => panic!("synced lyrics"),
    }
}

/// Line-timed, shaped like Apple's, with a translation, a transliteration and a duet.
const LINE_TTML: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" itunes:timing="Line">
<head><metadata><iTunesMetadata xmlns="http://music.apple.com/lyric-ttml-internal">
<translations><translation type="replacement" xml:lang="en"><text for="L1">Hello</text></translation></translations>
<transliterations><transliteration xml:lang="ja-Latn"><text for="L1">konnichiwa</text></transliteration></transliterations>
<songwriters><songwriter>A. Writer</songwriter></songwriters>
</iTunesMetadata></metadata></head>
<body><div begin="1.5" end="9"><p begin="1.5" end="3.25" itunes:key="L1" ttm:agent="v1">こんにちは</p><p begin="1:02.3" end="1:04" itunes:key="L2" ttm:agent="v2">A &amp; B</p></div></body></tt>"#;

#[test]
fn line_timed_lyrics_have_times_translations_and_singers() {
    let lyrics = lyrics::parse(LINE_TTML).unwrap();
    let lines = synced(&lyrics);
    assert_eq!(lines.len(), 2);
    assert_eq!((lines[0].start, lines[0].end), (ms(1_500), ms(3_250)));
    assert_eq!(lines[0].text, "こんにちは");
    assert_eq!(lines[0].translation.as_deref(), Some("Hello"));
    assert_eq!(lines[0].transliteration.as_deref(), Some("konnichiwa"));
    assert_eq!(lines[0].agent.as_deref(), Some("v1"));
    assert!(lines[0].words.is_empty());
    assert_eq!(lines[1].start, ms(62_300));
    assert_eq!(lines[1].text, "A & B");
    assert_eq!(lines[1].translation, None);
    assert_eq!(lyrics.songwriters, ["A. Writer"]);

    assert_eq!(lyrics.line_at(ms(0)), None, "before the first line");
    assert_eq!(lyrics.line_at(ms(1_500)), Some(0));
    assert_eq!(
        lyrics.line_at(ms(30_000)),
        Some(0),
        "between lines the last one stays"
    );
    assert_eq!(lyrics.line_at(ms(70_000)), Some(1));
}

#[test]
fn word_timed_lyrics_keep_syllables_spaces_and_background_vocals() {
    let ttml = r#"<tt xmlns:ttm="m" itunes:timing="Word"><body><div>
<p begin="1" end="4" itunes:key="L1"><span begin="1" end="1.5">Hel</span><span begin="1.5" end="2">lo</span> <span begin="2.25" end="3">you</span><span ttm:role="x-bg"><span begin="3" end="4">(ooh)</span></span></p>
</div></body></tt>"#;
    let lyrics = lyrics::parse(ttml).unwrap();
    let line = &synced(&lyrics)[0];
    assert_eq!(
        line.text, "Hello you",
        "background vocals are not in the line text"
    );
    let words: Vec<_> = line
        .words
        .iter()
        .map(|w| (w.text.as_str(), w.space_after, w.background))
        .collect();
    assert_eq!(
        words,
        [
            ("Hel", false, false),
            ("lo", true, false),
            ("you", false, false),
            ("(ooh)", false, true)
        ]
    );
    assert_eq!(
        (line.words[2].start, line.words[2].end),
        (ms(2_250), ms(3_000))
    );
}

#[test]
fn a_word_timed_line_without_its_own_times_takes_its_words_times() {
    let ttml = r#"<tt itunes:timing="Word"><body><div><p><span begin="5" end="6">a</span> <span begin="6" end="7.5">b</span></p></div></body></tt>"#;
    let lyrics = lyrics::parse(ttml).unwrap();
    let line = &synced(&lyrics)[0];
    assert_eq!((line.start, line.end), (ms(5_000), ms(7_500)));
}

#[test]
fn untimed_lyrics_are_plain_lines() {
    let ttml =
        r#"<tt itunes:timing="None"><body><div><p> One </p><p></p><p>Two</p></div></body></tt>"#;
    let lyrics = lyrics::parse(ttml).unwrap();
    assert_eq!(
        lyrics.lines,
        LyricLines::Unsynced(vec!["One".into(), "Two".into()])
    );
    assert_eq!(lyrics.line_at(ms(1_000)), None);
}

#[test]
fn broken_ttml_is_an_error() {
    for bad in ["<x/>", "<tt>", "<tt><body><p>&nope;</p></body></tt>"] {
        assert!(lyrics::parse(bad).is_err(), "{bad}");
    }
}

/// A token as long as a real one (`rustet-ampapi` rejects shorter ones).
fn sign_in(api: &rustet_ampapi::Api) {
    api.set_user_token(Some(UserToken::new("m".repeat(60))));
}

#[tokio::test]
async fn the_loader_falls_back_to_line_lyrics() {
    let env = env().await;
    Mock::given(method("GET"))
        .and(path("/v1/catalog/us/songs/1/syllable-lyrics"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/catalog/us/songs/1/lyrics"))
        .respond_with(json(
            json!({"data": [{"attributes": {"ttml": LINE_TTML}}]}).to_string(),
        ))
        .expect(1)
        .mount(&env.server)
        .await;
    sign_in(&env.api);
    let lyrics = lyrics::load(&env.api, "1").await.unwrap();
    assert_eq!(synced(&lyrics).len(), 2);
}

#[tokio::test]
async fn no_lyrics_is_not_found_and_a_missing_token_asks_to_sign_in() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&env.server)
        .await;
    assert_eq!(
        lyrics::load(&env.api, "1").await,
        Err(LoadError::SignInRequired)
    );
    sign_in(&env.api);
    assert_eq!(lyrics::load(&env.api, "1").await, Err(LoadError::NotFound));
}
