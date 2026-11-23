//! Lyrics for the line-synced lyrics view: timed lines, word timings when Apple has them,
//! translations and transliterations, from Apple's TTML.
//!
//! TTML shapes (`itunes:timing` on `<tt>`): `Line` (each `<p>` has `begin`/`end`), `Word`
//! (each word is a timed `<span>`; background vocals are spans inside a `ttm:role="x-bg"`
//! span), `None` (plain lines). Translations and transliterations are in
//! `head/metadata/iTunesMetadata`, keyed by each line's `itunes:key`.

use std::collections::HashMap;
use std::time::Duration;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use rustet_ampapi::lyrics::{LyricsFormat, LyricsRequest};
use rustet_ampapi::{Api, ErrorKind};
use tokio_util::sync::CancellationToken;

use super::error::LoadError;

/// A song's lyrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lyrics {
    pub lines: LyricLines,
    pub songwriters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LyricLines {
    /// Timed lines, in order.
    Synced(Vec<TimedLine>),
    /// Plain lines without timing: the view shows them as static text.
    Unsynced(Vec<String>),
}

/// One timed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedLine {
    pub start: Duration,
    pub end: Duration,
    /// The sung text, background vocals left out.
    pub text: String,
    /// Word timings; empty for line-timed lyrics.
    pub words: Vec<Word>,
    pub translation: Option<String>,
    pub transliteration: Option<String>,
    /// The singer (`v1`, `v2`, …) when Apple marks a duet.
    pub agent: Option<String>,
}

/// One timed word or syllable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
    /// A space follows (syllables of one word have none between them).
    pub space_after: bool,
    /// A background vocal.
    pub background: bool,
}

impl Lyrics {
    /// The line to highlight at `position`: the last line that has started. `None` before the
    /// first line and for unsynced lyrics.
    pub fn line_at(&self, position: Duration) -> Option<usize> {
        let LyricLines::Synced(lines) = &self.lines else {
            return None;
        };
        lines
            .partition_point(|l| l.start <= position)
            .checked_sub(1)
    }
}

/// Why TTML could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid lyrics TTML: {0}")]
pub struct TtmlError(String);

/// The song's lyrics: word-timed when Apple has them, else line-timed. Needs the music user
/// token (Apple answers 404 without it).
pub async fn load(api: &Api, song_id: &str) -> Result<Lyrics, LoadError> {
    let user = api.user_token().ok_or(LoadError::SignInRequired)?;
    let cancel = CancellationToken::new();
    let token = api.developer_token(&cancel).await?;
    let locale = api.locale();
    let mut last = LoadError::NotFound;
    for kind in ["syllable-lyrics", "lyrics"] {
        let request = LyricsRequest {
            storefront: &locale.storefront,
            song_id,
            kind,
            language: &locale.language,
            format: LyricsFormat::Ttml,
            token: token.secret(),
            media_user_token: user.secret(),
        };
        match api.client().lyrics(&cancel, &request).await {
            Ok(ttml) => return parse(&ttml).map_err(|e| LoadError::Decode(e.to_string())),
            Err(err) if err.kind() == ErrorKind::NotFound => last = LoadError::NotFound,
            Err(err) => return Err(err.into()),
        }
    }
    Err(last)
}

/// A minimal element tree.
#[derive(Debug, Default)]
struct Element {
    name: String,
    /// By local name (`itunes:key` → `key`).
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

#[derive(Debug)]
enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.name == name)
    }

    fn text(&self) -> String {
        let mut out = String::new();
        for node in &self.children {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => out.push_str(&e.text()),
            }
        }
        out
    }

    /// Every descendant named `name`, depth first.
    fn find_all<'a>(&'a self, name: &'a str, out: &mut Vec<&'a Element>) {
        for e in self.elements() {
            if e.name == name {
                out.push(e);
            }
            e.find_all(name, out);
        }
    }
}

fn local(qname: &[u8]) -> String {
    let name = String::from_utf8_lossy(qname);
    name.rsplit(':').next().unwrap_or_default().to_owned()
}

fn element(start: &BytesStart<'_>) -> Result<Element, TtmlError> {
    let err = |e: &dyn std::fmt::Display| TtmlError(e.to_string());
    let mut attrs = Vec::new();
    for a in start.attributes() {
        let a = a.map_err(|e| err(&e))?;
        let value = a.unescape_value().map_err(|e| err(&e))?.into_owned();
        attrs.push((local(a.key.into_inner()), value));
    }
    Ok(Element {
        name: local(start.name().into_inner()),
        attrs,
        children: Vec::new(),
    })
}

fn push_text(stack: &mut [Element], text: &str) {
    let Some(parent) = stack.last_mut() else {
        return;
    };
    match parent.children.last_mut() {
        Some(Node::Text(prev)) => prev.push_str(text),
        _ => parent.children.push(Node::Text(text.to_owned())),
    }
}

fn tree(xml: &str) -> Result<Element, TtmlError> {
    let err = |e: &dyn std::fmt::Display| TtmlError(e.to_string());
    let mut reader = Reader::from_str(xml);
    let mut stack = vec![Element::default()];
    loop {
        match reader.read_event().map_err(|e| err(&e))? {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => {
                let e = element(&start)?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(Node::Element(e));
                }
            }
            Event::End(_) => {
                let done = stack.pop().filter(|_| !stack.is_empty());
                let done = done.ok_or_else(|| TtmlError("unbalanced elements".into()))?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(Node::Element(done));
                }
            }
            Event::Text(t) => push_text(&mut stack, &t.decode().map_err(|e| err(&e))?),
            Event::CData(c) => push_text(&mut stack, &c.decode().map_err(|e| err(&e))?),
            Event::GeneralRef(r) => {
                let text = match r.resolve_char_ref().map_err(|e| err(&e))? {
                    Some(c) => c.to_string(),
                    None => {
                        let name = r.decode().map_err(|e| err(&e))?;
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| TtmlError(format!("unknown entity &{name};")))?
                            .to_owned()
                    }
                };
                push_text(&mut stack, &text);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    match <[Element; 1]>::try_from(stack) {
        Ok([doc]) => Ok(doc),
        Err(_) => Err(TtmlError("unclosed elements".into())),
    }
}

/// An Apple TTML time (`1:02.3`, `12.5`, `3.25s`, `01:02:03.450`) as a duration. The fraction
/// is read by digit position (`1.5` is 1500 ms), as `rustet-ampapi`'s converters do.
fn time(t: &str) -> Option<Duration> {
    let t = t.trim();
    let t = t.strip_suffix('s').unwrap_or(t);
    if !t.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    let mut secs = 0u64;
    for part in whole.split(':') {
        secs = secs.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    let millis = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<3}").get(..3)?.parse().ok()?
    };
    Some(Duration::from_secs(secs) + Duration::from_millis(millis))
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Words of a word-timed `<p>`, and its text without background vocals.
fn words(el: &Element, background: bool, words: &mut Vec<Word>, text: &mut String) {
    for node in &el.children {
        match node {
            Node::Text(t) => {
                if !background {
                    text.push_str(t);
                }
                if t.chars().next().is_some_and(char::is_whitespace)
                    && let Some(last) = words.last_mut()
                {
                    last.space_after = true;
                }
            }
            Node::Element(span) => {
                let bg = background || span.attr("role") == Some("x-bg");
                match (
                    span.attr("begin").and_then(time),
                    span.attr("end").and_then(time),
                ) {
                    (Some(start), Some(end)) if span.elements().next().is_none() => {
                        let word = span.text();
                        if !bg {
                            text.push_str(&word);
                        }
                        words.push(Word {
                            start,
                            end,
                            text: word.trim().to_owned(),
                            space_after: word.ends_with(char::is_whitespace),
                            background: bg,
                        });
                    }
                    _ => self::words(span, bg, words, text),
                }
            }
        }
    }
}

/// Parses Apple's lyrics TTML.
pub fn parse(ttml: &str) -> Result<Lyrics, TtmlError> {
    let doc = tree(ttml)?;
    let tt = doc.child("tt").ok_or_else(|| TtmlError("no <tt>".into()))?;
    let body = tt
        .child("body")
        .ok_or_else(|| TtmlError("no <body>".into()))?;
    let meta = tt
        .child("head")
        .and_then(|h| h.child("metadata"))
        .and_then(|m| m.child("iTunesMetadata"));
    let keyed = |list: &str, item: &str| -> HashMap<String, String> {
        let mut out = HashMap::new();
        let Some(list) = meta.and_then(|m| m.child(list)) else {
            return out;
        };
        // The first translation or transliteration only.
        if let Some(first) = list.child(item) {
            let mut texts = Vec::new();
            first.find_all("text", &mut texts);
            for t in texts {
                if let Some(key) = t.attr("for") {
                    out.entry(key.to_owned())
                        .or_insert_with(|| collapse(&t.text()));
                }
            }
        }
        out
    };
    let translations = keyed("translations", "translation");
    let transliterations = keyed("transliterations", "transliteration");
    let songwriters = meta
        .and_then(|m| m.child("songwriters"))
        .map(|s| {
            s.elements()
                .map(|w| collapse(&w.text()))
                .filter(|w| !w.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut paragraphs = Vec::new();
    body.find_all("p", &mut paragraphs);
    let word_timed = tt.attr("timing") == Some("Word");
    let mut lines = Vec::new();
    let mut synced = tt.attr("timing") != Some("None");
    for p in &paragraphs {
        let mut ws = Vec::new();
        let mut text = String::new();
        if word_timed {
            words(p, false, &mut ws, &mut text);
        } else {
            text = p.text();
        }
        let text = collapse(&text);
        let start = p
            .attr("begin")
            .and_then(time)
            .or_else(|| ws.first().map(|w| w.start));
        let end = p
            .attr("end")
            .and_then(time)
            .or_else(|| ws.iter().map(|w| w.end).max());
        let (Some(start), Some(end)) = (start, end) else {
            synced = false;
            lines.push((None, text, ws, p));
            continue;
        };
        lines.push((Some((start, end)), text, ws, p));
    }
    let lines = if synced {
        LyricLines::Synced(
            lines
                .into_iter()
                .filter_map(|(times, text, words, p)| {
                    let (start, end) = times?;
                    let key = p.attr("key").unwrap_or_default();
                    Some(TimedLine {
                        start,
                        end,
                        text,
                        words,
                        translation: translations.get(key).cloned(),
                        transliteration: transliterations.get(key).cloned(),
                        agent: p.attr("agent").map(str::to_owned),
                    })
                })
                .collect(),
        )
    } else {
        LyricLines::Unsynced(
            lines
                .into_iter()
                .map(|(_, text, _, _)| text)
                .filter(|t| !t.is_empty())
                .collect(),
        )
    };
    Ok(Lyrics { lines, songwriters })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_like_apples_converters() {
        for (input, ms) in [
            ("12.5", 12_500),
            ("01:05.5", 65_500),
            ("1:02:03.450", 3_723_450),
            ("9.527", 9_527),
            ("42", 42_000),
            ("3.25s", 3_250),
        ] {
            assert_eq!(time(input), Some(Duration::from_millis(ms)), "{input}");
        }
        assert_eq!(time("abc"), None);
    }
}
