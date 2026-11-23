//! Focused M3U8 decoder. It replaces `github.com/grafov/m3u8` plus the `m3u8safe` panic guard: it reads only
//! the tags and attributes the engine uses, and it never panics. Every malformed input is a [`ParseError`].
//!
//! Tag semantics follow grafov/m3u8 v0.11.1 in strict mode wherever the engine depends on them: the playlist type
//! is set by the last tag that names one, the playlist-level key and map are the first ones attached to a segment,
//! and an `EXT-X-KEY` / `EXT-X-MAP` / `EXT-X-BYTERANGE` attaches to the segment of the next URI line.

use std::collections::HashMap;

use crate::master::{Alternative, Variant};

/// Why a playlist could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("#EXTM3U absent")]
    MissingHeader,
    #[error("cannot detect playlist type")]
    UnknownType,
    #[error("line {line}: {reason}")]
    Invalid { line: usize, reason: &'static str },
}

pub(crate) enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// Master playlist with URIs as written (not resolved).
pub(crate) struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub alternatives: Vec<Alternative>,
}

pub(crate) struct MediaPlaylist {
    pub segments: Vec<RawSegment>,
    /// The first key attached to a segment (grafov's `MediaPlaylist.Key`).
    pub first_key: Option<Key>,
    /// The first map attached to a segment (grafov's `MediaPlaylist.Map`).
    pub first_map: Option<Map>,
}

pub(crate) struct RawSegment {
    pub uri: String,
    pub duration: f64,
    /// EXT-X-BYTERANGE length; 0 when the segment has none.
    pub limit: i64,
    pub offset: i64,
    /// The key declared just before this segment; `None` when the key did not change here.
    pub key: Option<Key>,
    pub map: Option<Map>,
}

#[derive(Clone)]
pub(crate) struct Key {
    pub method: String,
    pub uri: String,
    pub key_format: String,
}

#[derive(Clone)]
pub(crate) struct Map {
    pub uri: String,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Master,
    Media,
}

/// Media tags that only set the playlist type here: the engine does not read their values.
const MEDIA_TAGS: &[&str] = &[
    "#EXT-X-VERSION:",
    "#EXT-X-TARGETDURATION:",
    "#EXT-X-MEDIA-SEQUENCE:",
    "#EXT-X-PLAYLIST-TYPE:",
    "#EXT-X-DISCONTINUITY",
    "#EXT-X-START:",
    "#EXT-X-PROGRAM-DATE-TIME:",
    "#EXT-SCTE35:",
    "#EXT-X-I-FRAMES-ONLY",
    "#WV-",
];

/// Decodes a master or media playlist. `keep` sees each raw line first; lines it rejects are skipped (line numbers in
/// errors still count them).
pub(crate) fn parse(body: &str, keep: impl Fn(&str) -> bool) -> Result<Playlist, ParseError> {
    let mut header = false;
    let mut kind = None;

    let mut variants: Vec<Variant> = Vec::new();
    let mut alternatives = Vec::new();
    let mut pending_variant: Option<usize> = None;

    let mut segments: Vec<RawSegment> = Vec::new();
    let mut first_key = None;
    let mut first_map = None;
    let mut pending_inf = false;
    // Like grafov, an EXTINF with an empty duration reuses the previous one.
    let mut duration = 0.0;
    let mut pending_range: Option<(i64, i64)> = None;
    let mut pending_key: Option<Key> = None;
    let mut pending_map: Option<Map> = None;

    for (index, raw) in body.split('\n').enumerate() {
        if !keep(raw) {
            continue;
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let invalid = |reason| ParseError::Invalid {
            line: index + 1,
            reason,
        };

        if line == "#EXTM3U" {
            header = true;
        } else if line == "#EXT-X-ENDLIST" {
            kind = Some(Kind::Media);
        } else if let Some(list) = line.strip_prefix("#EXT-X-MEDIA:") {
            kind = Some(Kind::Master);
            let a = attributes(list);
            alternatives.push(Alternative {
                group_id: attr(&a, "GROUP-ID"),
                name: attr(&a, "NAME"),
                uri: attr(&a, "URI"),
            });
        } else if let Some(list) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            // A second STREAM-INF before the URI line is ignored, as in grafov.
            if pending_variant.is_none() {
                kind = Some(Kind::Master);
                variants.push(variant(&attributes(list), String::new()).map_err(invalid)?);
                pending_variant = Some(variants.len() - 1);
            }
        } else if let Some(list) = line.strip_prefix("#EXT-X-I-FRAME-STREAM-INF:") {
            kind = Some(Kind::Master);
            let a = attributes(list);
            variants.push(variant(&a, attr(&a, "URI")).map_err(invalid)?);
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            if !pending_inf {
                kind = Some(Kind::Media);
                let value = rest
                    .split_once(',')
                    .ok_or(invalid("EXTINF without a comma"))?
                    .0;
                if !value.is_empty() {
                    duration = value.parse().map_err(|_| invalid("bad EXTINF duration"))?;
                }
                pending_inf = true;
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            if pending_range.is_none() {
                kind = Some(Kind::Media);
                let (limit, offset) = rest.split_once('@').unwrap_or((rest, "0"));
                let limit = limit
                    .parse()
                    .map_err(|_| invalid("bad EXT-X-BYTERANGE length"))?;
                let offset = offset
                    .parse()
                    .map_err(|_| invalid("bad EXT-X-BYTERANGE offset"))?;
                pending_range = Some((limit, offset));
            }
        } else if let Some(list) = line.strip_prefix("#EXT-X-KEY:") {
            kind = Some(Kind::Media);
            let a = attributes(list);
            pending_key = Some(Key {
                method: attr(&a, "METHOD"),
                uri: attr(&a, "URI"),
                key_format: attr(&a, "KEYFORMAT"),
            });
        } else if let Some(list) = line.strip_prefix("#EXT-X-MAP:") {
            kind = Some(Kind::Media);
            let a = attributes(list);
            let (limit, offset) = match a.get("BYTERANGE") {
                Some(range) => map_byte_range(range).ok_or(invalid("bad EXT-X-MAP BYTERANGE"))?,
                None => (0, 0),
            };
            pending_map = Some(Map {
                uri: attr(&a, "URI"),
                limit,
                offset,
            });
        } else if line.starts_with('#') {
            if MEDIA_TAGS.iter().any(|tag| line.starts_with(tag)) {
                kind = Some(Kind::Media);
            }
        } else {
            if let Some(i) = pending_variant.take() {
                variants[i].url = line.to_owned();
            }
            if pending_inf {
                segments.push(RawSegment {
                    uri: line.to_owned(),
                    duration,
                    limit: 0,
                    offset: 0,
                    key: None,
                    map: None,
                });
                pending_inf = false;
            }
            // Pending tags attach to the newest segment, even when this URI line had no EXTINF. With no segment at
            // all grafov dereferenced nil here (the panic m3u8safe guarded against).
            if let Some((limit, offset)) = pending_range.take() {
                let seg = segments
                    .last_mut()
                    .ok_or(invalid("EXT-X-BYTERANGE before any segment"))?;
                seg.limit = limit;
                seg.offset = offset;
            }
            if let Some(key) = pending_key.take() {
                let seg = segments
                    .last_mut()
                    .ok_or(invalid("EXT-X-KEY before any segment"))?;
                first_key.get_or_insert_with(|| key.clone());
                seg.key = Some(key);
            }
            if let Some(map) = pending_map.take() {
                let seg = segments
                    .last_mut()
                    .ok_or(invalid("EXT-X-MAP before any segment"))?;
                first_map.get_or_insert_with(|| map.clone());
                seg.map = Some(map);
            }
        }
    }

    if !header {
        return Err(ParseError::MissingHeader);
    }
    match kind {
        Some(Kind::Master) => Ok(Playlist::Master(MasterPlaylist {
            variants,
            alternatives,
        })),
        Some(Kind::Media) => Ok(Playlist::Media(MediaPlaylist {
            segments,
            first_key,
            first_map,
        })),
        None => Err(ParseError::UnknownType),
    }
}

fn variant(a: &HashMap<&str, &str>, url: String) -> Result<Variant, &'static str> {
    // grafov reads BANDWIDTH with Atoi and casts to uint32; keep that truncation.
    let number = |name, reason| match a.get(name) {
        Some(v) => v.parse::<i64>().map(|n| n as u32).map_err(|_| reason),
        None => Ok(0),
    };
    Ok(Variant {
        url,
        bandwidth: number("BANDWIDTH", "bad BANDWIDTH")?,
        average_bandwidth: number("AVERAGE-BANDWIDTH", "bad AVERAGE-BANDWIDTH")?,
        codecs: attr(a, "CODECS"),
        resolution: attr(a, "RESOLUTION"),
        video_range: attr(a, "VIDEO-RANGE"),
        hdcp_level: attr(a, "HDCP-LEVEL"),
        allowed_cpc: attr(a, "ALLOWED-CPC"),
    })
}

fn attr(a: &HashMap<&str, &str>, name: &str) -> String {
    a.get(name).map_or_else(String::new, |v| (*v).to_owned())
}

/// Splits an attribute list (`KEY=VALUE,KEY="quoted, value"`) into a map. A repeated key keeps its last value.
fn attributes(list: &str) -> HashMap<&str, &str> {
    let mut out = HashMap::new();
    let mut rest = list;
    loop {
        rest = rest.trim_start_matches([',', ' ', '\t']);
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => after.split_once(',').unwrap_or((after, "")),
        };
        out.insert(key.trim(), value.trim_matches([' ', '"']));
        rest = next;
    }
    out
}

/// Parses `<length>@<offset>` the way Go's `fmt.Sscanf(v, "%d@%d")` does: trailing text is ignored.
fn map_byte_range(v: &str) -> Option<(i64, i64)> {
    let (limit, rest) = scan_int(v)?;
    let (offset, _) = scan_int(rest.strip_prefix('@')?)?;
    Some((limit, offset))
}

/// Reads a leading decimal integer (optional sign), like a Go `%d` scan verb. Returns it and the text after it.
pub(crate) fn scan_int(s: &str) -> Option<(i64, &str)> {
    let s = s.trim_start();
    let sign = usize::from(s.starts_with(['+', '-']));
    let digits = s[sign..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let end = sign + digits;
    Some((s[..end].parse().ok()?, &s[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_handle_quotes_commas_and_empty_values() {
        let a =
            attributes(r#"METHOD=SAMPLE-AES,URI="skd://x,a2lk",KEYFORMAT="",IV=0xAB , NAME="x""#);
        assert_eq!(a["METHOD"], "SAMPLE-AES");
        assert_eq!(a["URI"], "skd://x,a2lk");
        assert_eq!(a["KEYFORMAT"], "");
        assert_eq!(a["IV"], "0xAB");
        assert_eq!(a["NAME"], "x");
    }

    #[test]
    fn scan_int_reads_a_go_style_prefix() {
        assert_eq!(scan_int("3872@0"), Some((3872, "@0")));
        assert_eq!(scan_int(" -5x"), Some((-5, "x")));
        assert_eq!(scan_int("x"), None);
        assert_eq!(scan_int("99999999999999999999"), None);
        assert_eq!(map_byte_range("100"), None);
        assert_eq!(map_byte_range("100@7junk"), Some((100, 7)));
    }

    // Ported from m3u8safe: the upstream decoder nil-derefed on this input (found by FuzzParseMedia).
    #[test]
    fn map_without_segment_is_an_error() {
        assert!(parse("#EXT-X-MAP:0\n0", |_| true).is_err());
        assert!(matches!(
            parse("#EXTM3U\n#EXT-X-MAP:URI=\"i.mp4\"\nseg.mp4\n", |_| true),
            Err(ParseError::Invalid { line: 3, .. })
        ));
    }

    #[test]
    fn valid_media_playlist_decodes() {
        let pl = parse(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\na.mp4\n#EXT-X-ENDLIST\n",
            |_| true,
        );
        assert!(matches!(pl, Ok(Playlist::Media(m)) if m.segments.len() == 1));
    }

    #[test]
    fn last_type_tag_wins_and_header_is_required() {
        assert!(matches!(
            parse("#EXTM3U\n#EXT-X-VERSION:6\n", |_| true),
            Ok(Playlist::Media(_))
        ));
        assert!(matches!(
            parse(
                "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n",
                |_| true
            ),
            Ok(Playlist::Master(_))
        ));
        assert_eq!(
            parse("#EXTM3U\n# just a comment\n", |_| true).err(),
            Some(ParseError::UnknownType)
        );
        assert_eq!(
            parse("#EXT-X-ENDLIST\n", |_| true).err(),
            Some(ParseError::MissingHeader)
        );
    }
}
