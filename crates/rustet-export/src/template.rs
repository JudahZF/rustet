//! Filename templates: `{variable}` substitution into a path relative to the output folder.
//!
//! This is a security boundary. Templates and catalog metadata arrive from a web page, so a
//! substituted value can never add a path element: separators and pure-dot values are replaced
//! by look-alike characters. Only the template's own literal `/` creates folders, and the caller
//! still checks the joined result with [`contain`].
//!
//! Variables (case-insensitive names):
//!
//! | Variable | Value |
//! |---|---|
//! | `{title}`, `{song}` | track title |
//! | `{artist}` | primary artist |
//! | `{album_artist}` | album artist (falls back to artist) |
//! | `{album}` | album name |
//! | `{track_number}`, `{track}` | track number; `{track_number:02d}` zero-pads |
//! | `{disc_number}`, `{disc}` | disc number, same formats |
//! | `{year}` | 4-digit release year, empty when unknown |
//! | `{genre}` | first genre |
//! | `{codec}` | `alac`, `aac`, `atmos` or `mv` |
//! | `{ext}` | file extension without the dot |
//! | `{quality}` | `Lossless`, `Hi-Res Lossless`, `AAC`, `Atmos` or `MV` |
//! | `{tag}` | explicit / clean / mastered markers, for example `[E][M]` |
//! | `{release_date}`, `{releasedate}` | full `YYYY-MM-DD` |
//! | `{isrc}` | ISRC |
//! | `{id}`, `{song_id}` | catalog ID |
//! | `{url_artist}`, `{urlartist}` | lowercase hyphenated artist |
//!
//! Unknown variables stay as written. The extension is appended unless the path already ends
//! with it.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::path;

/// The template used when the request has none.
pub const DEFAULT_TEMPLATE: &str = "{album_artist}/{album}/{track_number:02d} - {title}";

/// The values available to a template.
#[derive(Debug, Clone, Default)]
pub(crate) struct TemplateVars<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album_artist: &'a str,
    pub album: &'a str,
    pub track_number: u32,
    pub disc_number: u32,
    /// 0 when unknown.
    pub year: u32,
    pub genre: &'a str,
    pub codec: &'a str,
    pub ext: &'a str,
    pub quality: &'a str,
    pub tag: &'a str,
    pub release_date: &'a str,
    pub isrc: &'a str,
    pub song_id: &'a str,
    pub url_artist: &'a str,
}

/// `{name}` or `{name:format}`.
static VAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([^}:]+)(?::([^}]+))?\}").expect("valid regex"));

/// Renders `template` (the default when empty) to a clean path relative to the output folder,
/// with the extension appended.
pub(crate) fn render(template: &str, v: &TemplateVars<'_>) -> String {
    let template = if template.is_empty() {
        DEFAULT_TEMPLATE
    } else {
        template
    };
    let substituted = VAR.replace_all(template, |caps: &Captures<'_>| {
        let format = caps.get(2).map_or("", |m| m.as_str());
        let number = |n: u32| {
            if format.is_empty() {
                n.to_string()
            } else {
                format_number(format, n)
            }
        };
        let value = match caps[1].to_lowercase().as_str() {
            "title" | "song" => v.title.to_owned(),
            "artist" => v.artist.to_owned(),
            "album_artist" if v.album_artist.is_empty() => v.artist.to_owned(),
            "album_artist" => v.album_artist.to_owned(),
            "album" => v.album.to_owned(),
            "track_number" | "track" => number(v.track_number),
            "disc_number" | "disc" => number(v.disc_number),
            "year" if v.year > 0 => v.year.to_string(),
            "year" => String::new(),
            "genre" => v.genre.to_owned(),
            "codec" => v.codec.to_owned(),
            "ext" => v.ext.to_owned(),
            "quality" => v.quality.to_owned(),
            "tag" => v.tag.to_owned(),
            "release_date" | "releasedate" => v.release_date.to_owned(),
            "isrc" => v.isrc.to_owned(),
            "id" | "song_id" => v.song_id.to_owned(),
            "url_artist" | "urlartist" => v.url_artist.to_owned(),
            _ => return caps[0].to_owned(),
        };
        sanitize_component(&value)
    });

    let mut out = path::clean(&substituted);
    // A path extension cannot decide this: "Mr. Brightside" has an "extension" of ". Brightside".
    // Only an actual trailing ".<ext>" means the template supplied one.
    if !v.ext.is_empty()
        && !out
            .to_lowercase()
            .ends_with(&format!(".{}", v.ext.to_lowercase()))
    {
        out.push('.');
        out.push_str(v.ext);
    }
    out
}

/// Joins `rel` (a rendered template) under `dir` and returns the result only if it stays strictly
/// inside `dir`. `dir` must be clean and absolute.
pub(crate) fn contain(dir: &str, rel: &str) -> Option<String> {
    let out = path::join(dir, rel);
    (out != dir && path::within(&out, dir)).then_some(out)
}

/// Applies a template number format. The format comes from the client, so only an optional zero
/// flag and a width of at most two digits before `d` are honoured ("d", "2d", "02d"); anything
/// else falls back to plain decimal.
pub(crate) fn format_number(format: &str, n: u32) -> String {
    let Some(spec) = format.strip_suffix('d') else {
        return n.to_string();
    };
    if !spec.bytes().all(|b| b.is_ascii_digit()) {
        return n.to_string();
    }
    // Leading zeros are flags (as in printf), the rest is the width.
    let digits = spec.trim_start_matches('0');
    let zero = digits.len() < spec.len();
    // The whole of `^0?[0-9]{0,2}d$`: one optional flag, then at most two digits.
    let flags = spec.len() - digits.len();
    if spec.len() > 3 || (spec.len() == 3 && flags == 0) {
        return n.to_string();
    }
    let width: usize = digits.parse().unwrap_or(0);
    if zero {
        format!("{n:0width$}")
    } else {
        format!("{n:width$}")
    }
}

/// The year of an Apple Music release date (`YYYY-MM-DD` or `YYYY`), 0 when unparseable.
pub(crate) fn year_from_date(s: &str) -> u32 {
    s.get(..4)
        .filter(|y| y.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|y| y.parse().ok())
        .unwrap_or(0)
}

/// A lowercase URL slug: ASCII letters and digits kept, space, `_` and `.` become `-`, the rest
/// is dropped; repeated hyphens collapse and the ends are trimmed. Used for `{url_artist}`.
pub(crate) fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.to_lowercase().chars() {
        let c = match c {
            'a'..='z' | '0'..='9' => c,
            ' ' | '_' | '.' => '-',
            _ => continue,
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_owned()
}

/// Makes one substituted value safe as (part of) a single path element: characters forbidden in
/// file names on common systems become Unicode look-alikes, surrounding whitespace is trimmed,
/// and a value of only dots (`.`, `..`), which would be a path element, becomes full-width dots.
pub(crate) fn sanitize_component(s: &str) -> String {
    let replaced: String = s
        .chars()
        .map(|c| match c {
            ':' => '꞉',  // U+A789
            '/' => '⁄',  // U+2044
            '\\' => '⧵', // U+29F5
            '?' => '？',
            '*' => '＊',
            '"' => '＂',
            '<' => '＜',
            '>' => '＞',
            '|' => '｜',
            c => c,
        })
        .collect();
    let mut out = replaced.trim().to_owned();
    if !out.is_empty() && out.bytes().all(|b| b == b'.') {
        out = out.replace('.', "．");
    }
    if cfg!(windows) {
        out = windows_safe_name(&out);
    }
    out
}

/// Makes one path element valid on Windows: no trailing dots or spaces (Windows strips them, so
/// the file would not land under the name the template made) and no reserved device names (CON,
/// NUL, COM1, LPT1, ...). It only changes names that would otherwise fail or move.
pub(crate) fn windows_safe_name(s: &str) -> String {
    let trimmed = s.trim_end_matches(['.', ' ']);
    let s = if trimmed != s && !trimmed.is_empty() {
        trimmed
    } else {
        s
    };
    let stem = s.split('.').next().unwrap_or(s);
    let up = stem.trim_end_matches(' ').to_uppercase();
    let reserved = matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (up.len() == 4
            && (up.starts_with("COM") || up.starts_with("LPT"))
            && matches!(up.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        format!("_{s}")
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn vars() -> TemplateVars<'static> {
        TemplateVars {
            title: "Song",
            artist: "Artist",
            album_artist: "Album Artist",
            album: "Album",
            track_number: 3,
            disc_number: 2,
            year: 2024,
            genre: "Pop",
            codec: "alac",
            ext: "m4a",
            ..TemplateVars::default()
        }
    }

    fn sep(p: &str) -> String {
        p.replace('/', &path::SEP.to_string())
    }

    #[test]
    fn default_template() {
        let v = TemplateVars {
            title: "Fearless",
            artist: "Taylor Swift",
            album_artist: "Taylor Swift",
            album: "Fearless (Platinum Edition)",
            track_number: 1,
            ext: "m4a",
            ..TemplateVars::default()
        };
        assert_eq!(
            render("", &v),
            sep("Taylor Swift/Fearless (Platinum Edition)/01 - Fearless.m4a")
        );
    }

    #[test]
    fn all_variables() {
        let v = vars();
        for (template, want) in [
            ("{title}", "Song.m4a"),
            ("{artist}", "Artist.m4a"),
            ("{album_artist}", "Album Artist.m4a"),
            ("{album}", "Album.m4a"),
            ("{track_number}", "3.m4a"),
            ("{track_number:02d}", "03.m4a"),
            ("{disc_number}", "2.m4a"),
            ("{year}", "2024.m4a"),
            ("{genre}", "Pop.m4a"),
            ("{codec}", "alac.m4a"),
            // {ext} in the body, then the appended extension.
            ("{ext}", "m4a.m4a"),
            (
                "{disc_number:02d} {track_number:02d} - {title}",
                "02 03 - Song.m4a",
            ),
            ("{TITLE}", "Song.m4a"),
        ] {
            assert_eq!(render(template, &v), want, "template {template:?}");
        }
    }

    #[test]
    fn album_artist_falls_back_to_artist() {
        let v = TemplateVars {
            title: "T",
            artist: "A",
            album: "AL",
            ext: "m4a",
            ..TemplateVars::default()
        };
        assert!(
            render("{album_artist}/{album}/{title}", &v).starts_with(&format!("A{}", path::SEP))
        );
    }

    #[test]
    fn unknown_variable_is_kept() {
        let v = TemplateVars {
            title: "T",
            ext: "m4a",
            ..TemplateVars::default()
        };
        assert!(render("{unknown} {title}", &v).contains("{unknown}"));
        assert!(render("{x:y} {title}", &v).contains("{x:y}"));
    }

    #[test]
    fn extension_is_appended_once() {
        for title in [
            "Mr. Brightside",
            "St. Elmo's Fire",
            "Hello (feat. Adele)",
            "A.B.C.",
        ] {
            let v = TemplateVars {
                title,
                ext: "m4a",
                ..TemplateVars::default()
            };
            assert!(render("{title}", &v).ends_with(".m4a"), "{title}");
        }
        let v = TemplateVars {
            title: "Song",
            ext: "m4a",
            ..TemplateVars::default()
        };
        assert_eq!(render("{title}.{ext}", &v), "Song.m4a");
        assert_eq!(render("{title}.m4a", &v), "Song.m4a");
        assert_eq!(render("{title}.M4A", &v), "Song.M4A");
    }

    #[test]
    fn unicode_passes_through() {
        let v = TemplateVars {
            title: "曲名",
            artist: "アーティスト",
            album_artist: "アーティスト",
            album: "アルバム",
            track_number: 1,
            ext: "m4a",
            ..TemplateVars::default()
        };
        assert!(render(DEFAULT_TEMPLATE, &v).contains("アーティスト"));
        for s in [
            "こんにちは",
            "Ação",
            "Rammstein – Du Hast",
            "AC⚡DC",
            "Beyoncé",
        ] {
            assert_eq!(sanitize_component(s), s);
        }
    }

    #[test]
    fn sanitize_replaces_forbidden_characters() {
        for c in [':', '/', '\\', '?', '*', '"', '<', '>', '|'] {
            let got = sanitize_component(&format!("a{c}b"));
            assert!(!got.contains(c) && got.chars().count() == 3, "{c}: {got}");
        }
        for s in [
            "hello world",
            "Taylor Swift",
            "Fearless",
            "2024",
            "Pop-Rock",
        ] {
            assert_eq!(sanitize_component(s), s);
        }
        assert_eq!(sanitize_component("  padded \t"), "padded");
        assert_eq!(sanitize_component(".."), "．．");
        assert_eq!(sanitize_component("."), "．");
        assert_eq!(sanitize_component("..a"), "..a");
    }

    #[test]
    fn windows_safe_names() {
        for (input, want) in [
            ("Song", "Song"),
            ("Song...", "Song"),
            ("Song. ", "Song"),
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("Com1", "_Com1"),
            ("LPT9.m4a", "_LPT9.m4a"),
            ("COM0", "COM0"),
            ("Console", "Console"),
            ("Mr. Brightside", "Mr. Brightside"),
            ("...", "..."),
        ] {
            assert_eq!(windows_safe_name(input), want, "{input:?}");
        }
    }

    #[test]
    fn number_format_only_accepts_zero_padding() {
        for (format, want) in [
            ("d", "7"),
            ("2d", " 7"),
            ("02d", "07"),
            ("03d", "007"),
            ("0d", "7"),
            ("999999999d", "7"),
            ("[9]d", "7"),
            ("s", "7"),
            ("x", "7"),
            ("+d", "7"),
            ("", "7"),
            ("02d ", "7"),
            ("123d", "7"),
            ("012d", "000000000007"),
        ] {
            assert_eq!(format_number(format, 7), want, "{format:?}");
        }
    }

    #[test]
    fn year_parsing() {
        for (input, want) in [
            ("2024-03-14", 2024),
            ("1989-10-27", 1989),
            ("2024", 2024),
            ("", 0),
            ("invalid", 0),
            ("99-01-01", 0),
        ] {
            assert_eq!(year_from_date(input), want, "{input:?}");
        }
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Taylor Swift"), "taylor-swift");
        assert_eq!(slugify("  AC/DC  "), "acdc");
        assert_eq!(slugify("Mr. _ Big"), "mr-big");
        assert_eq!(slugify("Beyoncé"), "beyonc");
    }

    #[cfg(unix)]
    #[test]
    fn contain_rejects_escapes() {
        assert_eq!(contain("/out", "a/b.m4a").as_deref(), Some("/out/a/b.m4a"));
        assert_eq!(
            contain("/out", "/etc/x.m4a").as_deref(),
            Some("/out/etc/x.m4a")
        );
        assert_eq!(contain("/out", "../x.m4a"), None);
        assert_eq!(contain("/out", "a/../../x.m4a"), None);
        assert_eq!(contain("/out", "."), None);
        assert_eq!(contain("/out", "../out-sibling/x"), None);
    }

    // The Go fuzz corpus of FuzzRenderTemplateContainment.
    #[test]
    fn fuzz_corpus_stays_contained() {
        for (title, artist, album, track) in [
            ("Title", "Artist", "Album", 3),
            ("..", "..", "..", 1),
            ("a/../../b", "x", "y", 0),
        ] {
            let v = TemplateVars {
                title,
                artist,
                album,
                track_number: track,
                ext: "m4a",
                ..TemplateVars::default()
            };
            let rel = render("", &v);
            assert!(contain(&root(), &rel).is_some(), "{rel:?}");
        }
    }

    fn root() -> String {
        path::clean(&std::env::temp_dir().join("export").to_string_lossy())
    }

    proptest! {
        // Catalog metadata must never move the default template's output outside the folder,
        // and must never add a path element: the default template always has three.
        #[test]
        fn metadata_never_escapes(
            title in any::<String>(),
            artist in any::<String>(),
            album in any::<String>(),
            track in any::<u32>(),
        ) {
            let v = TemplateVars {
                title: &title,
                artist: &artist,
                album: &album,
                track_number: track,
                ext: "m4a",
                ..TemplateVars::default()
            };
            let rel = render("", &v);
            // A leading separator (empty album artist) is fine: joining never replaces the root.
            prop_assert!(contain(&root(), &rel).is_some(), "escaped: {rel:?}");
            prop_assert!(rel.split(path::is_sep).filter(|e| !e.is_empty()).count() <= 3, "extra element: {rel:?}");
            prop_assert!(rel.split(path::is_sep).all(|e| e != ".."), "dot-dot: {rel:?}");
        }

        // Whatever the template, a path that `contain` accepts lies strictly inside the folder.
        #[test]
        fn contained_paths_stay_inside(
            template in "[a-z{}/._:\\\\ -]{0,40}",
            title in any::<String>(),
        ) {
            let v = TemplateVars { title: &title, ext: "m4a", ..TemplateVars::default() };
            let root = root();
            if let Some(out) = contain(&root, &render(&template, &v)) {
                let out = std::path::Path::new(&out);
                prop_assert!(out.starts_with(&root) && out != std::path::Path::new(&root));
                let rest = out.strip_prefix(&root).unwrap();
                prop_assert!(rest.components().all(|c| matches!(c, std::path::Component::Normal(_))));
            }
        }
    }
}
