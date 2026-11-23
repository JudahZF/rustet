//! TTML → LRC / SRT / WebVTT conversion.

use super::TtmlError;
use super::dom::{self, Element, Node};

/// `<tt>` and `<body>` of a document, or an error for TTML that lacks them (empty or
/// malformed lyrics).
fn ttml_root(doc: &Element) -> Result<(&Element, &Element), TtmlError> {
    let tt = doc.child("tt").ok_or(TtmlError::NoTt)?;
    let body = tt.child("body").ok_or(TtmlError::NoBody)?;
    Ok((tt, body))
}

/// The `<iTunesMetadata>` element, which holds translations and transliterations.
fn itunes_metadata(doc: &Element) -> Option<&Element> {
    doc.child("tt")?
        .child("head")?
        .child("metadata")?
        .child("iTunesMetadata")
}

/// Text from character data and from each child element's leading text.
fn element_child_text(el: &Element) -> String {
    el.children()
        .iter()
        .map(|n| match n {
            Node::Text(t) => t.clone(),
            Node::Element(e) => e.text(),
        })
        .collect()
}

/// The `text` attribute if present, else the element's content.
fn line_text(el: &Element) -> String {
    el.attr("text")
        .map_or_else(|| element_child_text(el), str::to_owned)
}

/// The translation node for a lyric key.
///
/// The translation text is searched document-wide (`//text[@for=key]` in the Go engine),
/// not only below `<translation>`; the first match breadth-first wins.
fn translation_node<'a>(
    doc: &'a Element,
    meta: Option<&Element>,
    key: &str,
) -> Option<&'a Element> {
    if key.is_empty() {
        return None;
    }
    meta?.child("translations")?.child("translation")?;
    doc.find_all("text").find(|e| e.attr("for") == Some(key))
}

fn transliteration_node<'a>(meta: Option<&'a Element>, key: &str) -> Option<&'a Element> {
    if key.is_empty() {
        return None;
    }
    meta?
        .child("transliterations")?
        .child("transliteration")?
        .text_for(key)
}

/// Converts Apple Music TTML to LRC. Word-timed TTML becomes enhanced (syllable) LRC,
/// untimed TTML becomes plain lines.
pub fn ttml_to_lrc(ttml: &str) -> Result<String, TtmlError> {
    let doc = dom::parse(ttml)?;
    let (tt, body) = ttml_root(&doc)?;
    match tt.attr("itunes:timing") {
        Some("Word") => return syllable_ttml_to_lrc(&doc, body),
        Some("None") => {
            let lines: Vec<_> = doc
                .find_all("p")
                .map(|p| p.text().trim().to_owned())
                .filter(|l| !l.is_empty())
                .collect();
            return Ok(lines.join("\n"));
        }
        _ => {}
    }

    let meta = itunes_metadata(&doc);
    let mut lines = Vec::new();
    for item in body.child_elements() {
        for lyric in item.child_elements() {
            let begin = lyric.attr("begin").ok_or(TtmlError::NotSynced)?;
            let stamp = Stamp::Line.format(begin)?;
            let key = lyric.attr("itunes:key").unwrap_or("");
            let translit = transliteration_node(meta, key)
                .map(line_text)
                .unwrap_or_default();
            let trans = translation_node(&doc, meta, key)
                .map(line_text)
                .unwrap_or_default();
            let text = line_text(lyric);
            if !trans.is_empty() {
                lines.push(format!("{stamp}{trans}"));
            }
            if !translit.is_empty() && contains_cjk(&text) {
                lines.push(format!("{stamp}{translit}"));
            } else {
                lines.push(format!("{stamp}{text}"));
            }
        }
    }
    Ok(lines.join("\n"))
}

/// Converts Apple Music TTML to SubRip (SRT).
pub fn ttml_to_srt(ttml: &str) -> Result<String, TtmlError> {
    ttml_to_subtitle(ttml, Subtitle::Srt)
}

/// Converts Apple Music TTML to WebVTT.
pub fn ttml_to_vtt(ttml: &str) -> Result<String, TtmlError> {
    ttml_to_subtitle(ttml, Subtitle::Vtt)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Subtitle {
    Srt,
    Vtt,
}

/// Untimed TTML, and TTML without any timed line, give an empty string.
fn ttml_to_subtitle(ttml: &str, format: Subtitle) -> Result<String, TtmlError> {
    let doc = dom::parse(ttml)?;
    let (tt, body) = ttml_root(&doc)?;
    if tt.attr("itunes:timing") == Some("None") {
        return Ok(String::new());
    }

    let mut entries = Vec::new();
    for p in body.children_named("div").flat_map(Element::child_elements) {
        let (Some(begin), Some(end)) = (p.attr("begin"), p.attr("end")) else {
            continue;
        };
        if begin.is_empty() || end.is_empty() {
            continue;
        }
        let (begin, end) = (parse_ttml_ms(begin), parse_ttml_ms(end));
        let text = match p.attr("text") {
            Some(t) if !t.is_empty() => t.to_owned(),
            _ => element_child_text(p).trim().to_owned(),
        };
        if text.is_empty() {
            continue;
        }
        entries.push(match format {
            Subtitle::Srt => format!(
                "{}\n{} --> {}\n{text}",
                entries.len() + 1,
                sub_time(begin, ','),
                sub_time(end, ',')
            ),
            Subtitle::Vtt => format!(
                "{} --> {}\n{text}",
                sub_time(begin, '.'),
                sub_time(end, '.')
            ),
        });
    }
    if entries.is_empty() {
        return Ok(String::new());
    }
    let joined = entries.join("\n\n") + "\n";
    Ok(match format {
        Subtitle::Vtt => format!("WEBVTT\n\n{joined}"),
        Subtitle::Srt => joined,
    })
}

fn sub_time(ms: i64, sep: char) -> String {
    let (h, rest) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rest) = (rest / 60_000, rest % 60_000);
    let (s, ms) = (rest / 1000, rest % 1000);
    format!("{h:02}:{m:02}:{s:02}{sep}{ms:03}")
}

/// Parses an Apple TTML time value to milliseconds: `HH:MM:SS.mmm`, `MM:SS.mmm`,
/// `SS.mmm`, an optional `s` suffix, with or without a fraction. The fraction is read by
/// digit position (`"1.5"` is 1500 ms). Malformed components count as 0.
fn parse_ttml_ms(t: &str) -> i64 {
    let t = t.trim();
    let t = t.strip_suffix('s').unwrap_or(t);
    let (int_part, frac) = t.split_once('.').unwrap_or((t, ""));
    let secs = int_part
        .split(':')
        .map(|p| p.parse::<i64>().unwrap_or(0))
        .fold(0i64, |total, n| total.wrapping_mul(60).wrapping_add(n));
    let mut ms = secs.wrapping_mul(1000);
    if !frac.is_empty() {
        let frac = format!("{frac:0<3}");
        // A non-ASCII byte in the first three makes the fraction malformed (0).
        ms = ms.wrapping_add(frac.get(..3).and_then(|f| f.parse().ok()).unwrap_or(0));
    }
    ms
}

/// Validates and parses a TTML time value to milliseconds.
fn ttml_time_ms(v: &str) -> Result<i64, TtmlError> {
    let t = v.trim();
    let t = t.strip_suffix('s').unwrap_or(t);
    if !t.chars().any(char::is_numeric) {
        return Err(TtmlError::InvalidTime(t.to_owned()));
    }
    Ok(parse_ttml_ms(t))
}

/// A TTML time as LRC `(minutes, seconds, centiseconds)`.
fn lrc_time(v: &str) -> Result<(i64, i64, i64), TtmlError> {
    let total = ttml_time_ms(v)?;
    Ok((total / 60_000, (total / 1000) % 60, (total % 1000) / 10))
}

/// The LRC timestamp forms.
#[derive(Clone, Copy)]
enum Stamp {
    /// `[mm:ss.cs]`
    Line,
    /// `<mm:ss.cs>`
    Word,
    /// `[mm:ss.cs]<mm:ss.cs>`: the first syllable of an enhanced-LRC line.
    LineAndWord,
}

impl Stamp {
    fn format(self, v: &str) -> Result<String, TtmlError> {
        let (m, s, cs) = lrc_time(v)?;
        Ok(match self {
            Self::Line => format!("[{m:02}:{s:02}.{cs:02}]"),
            Self::Word => format!("<{m:02}:{s:02}.{cs:02}>"),
            Self::LineAndWord => format!("[{m:02}:{s:02}.{cs:02}]<{m:02}:{s:02}.{cs:02}>"),
        })
    }
}

fn syllable_ttml_to_lrc(doc: &Element, body: &Element) -> Result<String, TtmlError> {
    let meta = itunes_metadata(doc);
    let mut lines = Vec::new();
    for item in body.children_named("div").flat_map(Element::child_elements) {
        let key = item.attr("itunes:key").unwrap_or("");
        let mut syllables = String::new();
        let mut timed = 0usize;
        let mut end_time = String::new();
        let (mut translit_line, mut trans_line) = (String::new(), String::new());
        for node in item.children() {
            let span = match node {
                Node::Text(_) => {
                    if timed > 0 {
                        syllables.push(' ');
                    }
                    continue;
                }
                Node::Element(e) => e,
            };
            // Untimed spans carry no syllable.
            let Some(begin) = span.attr("begin") else {
                continue;
            };
            let stamp = if timed == 0 {
                Stamp::LineAndWord
            } else {
                Stamp::Word
            };
            syllables += &stamp.format(begin)?;
            end_time = Stamp::Word.format(span.attr("end").unwrap_or(""))?;
            syllables += &line_text(span);
            if timed == 0 {
                let line_begin = Stamp::Line.format(begin)?;
                (translit_line, trans_line) = syllabic_annotations(doc, meta, key, &line_begin);
            }
            timed += 1;
        }
        if !trans_line.is_empty() {
            lines.push(trans_line);
        }
        if !translit_line.is_empty() && contains_cjk(&syllables) {
            lines.push(translit_line);
        } else {
            lines.push(syllables + &end_time);
        }
    }
    Ok(lines.join("\n"))
}

/// The transliteration and translation lines of a syllabic TTML line.
fn syllabic_annotations(
    doc: &Element,
    meta: Option<&Element>,
    key: &str,
    line_begin: &str,
) -> (String, String) {
    let mut shared_stamp = String::new();
    // Syllabic transliteration: per-span timestamps.
    let translit_line = transliteration_node(meta, key).map_or_else(String::new, |node| {
        let mut parts = Vec::new();
        let mut start = String::new();
        for (i, span) in node.child_elements().enumerate() {
            if span.local_name() != "span" {
                continue;
            }
            let Some(begin) = span.attr("begin").filter(|b| !b.is_empty()) else {
                continue;
            };
            let Ok(stamp) = Stamp::Word.format(begin) else {
                continue;
            };
            if i == 0 {
                start = Stamp::Line.format(begin).unwrap_or_default();
                shared_stamp.clone_from(&start);
            }
            parts.push(stamp + &span.text());
        }
        start + &parts.join(" ")
    });
    // Translation: one text string, from the attribute or direct character data.
    let trans_line = translation_node(doc, meta, key).map_or_else(String::new, |node| {
        let text = node.attr("text").map_or_else(
            || {
                node.children()
                    .iter()
                    .filter_map(|n| match n {
                        Node::Text(t) => Some(t.as_str()),
                        Node::Element(_) => None,
                    })
                    .collect()
            },
            str::to_owned,
        );
        let prefix = if shared_stamp.is_empty() {
            line_begin
        } else {
            &shared_stamp
        };
        format!("{prefix}{text}")
    });
    (translit_line, trans_line)
}

/// CJK, Hangul, Hiragana, Katakana and related Unicode blocks.
const CJK_RANGES: &[(u32, u32)] = &[
    (0x1100, 0x11FF),   // Hangul Jamo
    (0x2E80, 0x2EFF),   // CJK Radicals Supplement
    (0x2F00, 0x2FDF),   // Kangxi Radicals
    (0x2FF0, 0x2FFF),   // Ideographic Description Characters
    (0x3000, 0x303F),   // CJK Symbols and Punctuation
    (0x3040, 0x309F),   // Hiragana
    (0x30A0, 0x30FF),   // Katakana
    (0x3130, 0x318F),   // Hangul Compatibility Jamo
    (0x31C0, 0x31EF),   // CJK Strokes
    (0x31F0, 0x31FF),   // Katakana Phonetic Extensions
    (0x3200, 0x32FF),   // Enclosed CJK Letters and Months
    (0x3300, 0x33FF),   // CJK Compatibility
    (0x3400, 0x4DBF),   // CJK Unified Ideographs Extension A
    (0x4E00, 0x9FFF),   // CJK Unified Ideographs
    (0xA960, 0xA97F),   // Hangul Jamo Extended-A
    (0xAC00, 0xD7AF),   // Hangul Syllables
    (0xD7B0, 0xD7FF),   // Hangul Jamo Extended-B
    (0xF900, 0xFAFF),   // CJK Compatibility Ideographs
    (0xFE30, 0xFE4F),   // CJK Compatibility Forms
    (0xFF65, 0xFF9F),   // Halfwidth Katakana
    (0xFFA0, 0xFFDC),   // Halfwidth Jamo
    (0x1AFF0, 0x1AFFF), // Kana Extended-B
    (0x1B000, 0x1B0FF), // Kana Supplement
    (0x1B100, 0x1B12F), // Kana Extended-A
    (0x1B130, 0x1B16F), // Small Kana Extension
    (0x1F200, 0x1F2FF), // Enclosed Ideographic Supplement
    (0x20000, 0x2A6DF), // CJK Unified Ideographs Extension B
    (0x2A700, 0x2B73F), // CJK Unified Ideographs Extension C
    (0x2B740, 0x2B81F), // CJK Unified Ideographs Extension D
    (0x2B820, 0x2CEAF), // CJK Unified Ideographs Extension E
    (0x2CEB0, 0x2EBEF), // CJK Unified Ideographs Extension F
    (0x2EBF0, 0x2EE5F), // CJK Unified Ideographs Extension I
    (0x2F800, 0x2FA1F), // CJK Compatibility Ideographs Supplement
    (0x30000, 0x3134F), // CJK Unified Ideographs Extension G
    (0x31350, 0x323AF), // CJK Unified Ideographs Extension H
];

fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        CJK_RANGES
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&u32::from(c)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converters_reject_malformed_ttml() {
        for input in ["<x/>", "<tt/>", r#"<tt itunes:timing="Word"/>"#] {
            assert!(ttml_to_lrc(input).is_err(), "ttml_to_lrc({input:?})");
            assert!(ttml_to_vtt(input).is_err(), "ttml_to_vtt({input:?})");
        }
    }

    #[test]
    fn lrc_time_reads_fractions_by_digit_position() {
        for (input, want) in [
            ("12.5", (0, 12, 50)),
            ("1.05", (0, 1, 5)),
            ("9.527", (0, 9, 52)),
            ("1:02.3", (1, 2, 30)),
            ("01:02:03.450", (62, 3, 45)),
            ("7s", (0, 7, 0)),
        ] {
            assert_eq!(lrc_time(input).unwrap(), want, "lrc_time({input:?})");
        }
        assert!(lrc_time("abc").is_err());
    }

    #[test]
    fn syllable_converter_skips_comments() {
        let ttml = r#"<tt itunes:timing="Word"><body><div><p><!-- note --><span begin="1.5" end="2">Hi</span></p></div></body></tt>"#;
        assert_eq!(
            ttml_to_lrc(ttml).unwrap(),
            "[00:01.50]<00:01.50>Hi<00:02.00>"
        );
    }

    #[test]
    fn parse_ttml_ms_formats() {
        for (input, want) in [
            ("12.5", 12_500),
            ("01:05.5", 65_500),
            ("1:02:03.450", 3_723_450),
            ("9.527", 9_527),
            ("42", 42_000),
            ("3.25s", 3_250),
        ] {
            assert_eq!(parse_ttml_ms(input), want, "parse_ttml_ms({input:?})");
        }
    }

    /// Line-timed TTML with a translation and a transliteration, shaped like Apple's.
    const LINE_TTML: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" itunes:timing="Line">
<head><metadata><iTunesMetadata xmlns="http://music.apple.com/lyric-ttml-internal">
<translations><translation type="replacement" xml:lang="en"><text for="L1">Hello</text></translation></translations>
<transliterations><transliteration xml:lang="ja-Latn"><text for="L1">konnichiwa</text></transliteration></transliterations>
</iTunesMetadata></metadata></head>
<body><div begin="1.5" end="9"><p begin="1.5" end="3.25" itunes:key="L1">こんにちは</p><p begin="1:02.3" end="1:04" itunes:key="L2">A &amp; B</p></div></body></tt>"#;

    #[test]
    fn line_lrc_with_translation_and_cjk_transliteration() {
        assert_eq!(
            ttml_to_lrc(LINE_TTML).unwrap(),
            "[00:01.50]Hello\n[00:01.50]konnichiwa\n[01:02.30]A & B"
        );
    }

    #[test]
    fn subtitles_from_line_ttml() {
        assert_eq!(
            ttml_to_srt(LINE_TTML).unwrap(),
            "1\n00:00:01,500 --> 00:00:03,250\nこんにちは\n\n2\n00:01:02,300 --> 00:01:04,000\nA & B\n"
        );
        assert_eq!(
            ttml_to_vtt(LINE_TTML).unwrap(),
            "WEBVTT\n\n00:00:01.500 --> 00:00:03.250\nこんにちは\n\n00:01:02.300 --> 00:01:04.000\nA & B\n"
        );
    }

    #[test]
    fn untimed_ttml_gives_plain_lines_and_no_subtitles() {
        let ttml = r#"<tt itunes:timing="None"><body><div><p> One </p><p></p><p>Two</p></div></body></tt>"#;
        assert_eq!(ttml_to_lrc(ttml).unwrap(), "One\nTwo");
        assert_eq!(ttml_to_srt(ttml).unwrap(), "");
    }

    #[test]
    fn syllable_lrc_joins_words_and_adds_translation() {
        let ttml = r#"<tt itunes:timing="Word"><head><metadata><iTunesMetadata>
<translations><translation><text for="L1">Hi there</text></translation></translations>
</iTunesMetadata></metadata></head><body><div>
<p itunes:key="L1"><span begin="1" end="1.5">Hel</span><span begin="1.5" end="2">lo</span> <span begin="2.25" end="3">you</span></p>
</div></body></tt>"#;
        assert_eq!(
            ttml_to_lrc(ttml).unwrap(),
            "[00:01.00]Hi there\n[00:01.00]<00:01.00>Hel<00:01.50>lo <00:02.25>you<00:03.00>"
        );
    }

    #[test]
    fn unsynced_line_ttml_is_an_error() {
        let ttml = "<tt><body><div><p>No timing</p></div></body></tt>";
        assert!(matches!(ttml_to_lrc(ttml), Err(TtmlError::NotSynced)));
    }
}
