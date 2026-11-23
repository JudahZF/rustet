//! Link parsing (`deeplink.mjs`) for everything the app can be asked to open: its own `rustet://`
//! links and every URL form Apple Music uses (https links on any Apple Music host, and the
//! `music://`, `musics://`, `itms://`, `itmss://`, `itunes://` … schemes).

use serde::Serialize;
use url::Url;

/// Rustet's own scheme, always registered.
pub const OWN_SCHEME: &str = "rustet";

/// Settings → Links groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemeGroup {
    Music,
    Store,
    Radio,
    Other,
    Windows,
}

impl SchemeGroup {
    pub const ALL: [SchemeGroup; 5] = [
        Self::Music,
        Self::Store,
        Self::Radio,
        Self::Other,
        Self::Windows,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Music => "Apple Music links",
            Self::Store => "iTunes Store and library links",
            Self::Radio => "Radio",
            Self::Other => "Registered by Apple Music but not playable in Rustet",
            Self::Windows => "Windows legacy aliases",
        }
    }
}

/// Why a web client cannot act on a scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Unsupported {
    /// `itpc://` podcast subscriptions.
    Podcast,
    /// `daap://` shared iTunes libraries.
    Daap,
}

impl Unsupported {
    /// The notice the shell shows: title and detail.
    pub const fn notice(self) -> (&'static str, &'static str) {
        match self {
            Self::Podcast => (
                "Podcast subscriptions are not supported",
                "Rustet is an Apple Music client and cannot subscribe to podcast feeds (itpc:// links).",
            ),
            Self::Daap => (
                "Shared iTunes libraries are not supported",
                "Rustet cannot connect to iTunes library shares (daap:// links).",
            ),
        }
    }
}

/// What a scheme carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemeKind {
    /// Apple Music content, opened like an https link.
    Link,
    /// An iTunes Radio link, opened on the Radio page.
    Radio,
    /// Registered by Apple Music, but a web client cannot act on it (a notice is shown).
    Unsupported(Unsupported),
}

impl SchemeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Radio => "radio",
            Self::Unsupported(_) => "unsupported",
        }
    }
}

/// One URL scheme the Apple Music app registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemeDef {
    pub scheme: &'static str,
    pub group: SchemeGroup,
    pub kind: SchemeKind,
    pub windows_only: bool,
    pub note: &'static str,
}

const fn def(
    scheme: &'static str,
    group: SchemeGroup,
    kind: SchemeKind,
    note: &'static str,
) -> SchemeDef {
    SchemeDef {
        scheme,
        group,
        kind,
        windows_only: false,
        note,
    }
}

const fn alias(scheme: &'static str, kind: SchemeKind, note: &'static str) -> SchemeDef {
    SchemeDef {
        scheme,
        group: SchemeGroup::Windows,
        kind,
        windows_only: true,
        note,
    }
}

use SchemeGroup as G;
use SchemeKind as K;

/// Every URL scheme the Apple Music app registers on Windows. Rustet only claims the ones the user
/// turns on: claiming a scheme takes the link away from any other app that handles it.
pub const APPLE_SCHEMES: [SchemeDef; 24] = [
    def(
        "music",
        G::Music,
        K::Link,
        "Includes the \"Open in Music\" button on music.apple.com",
    ),
    def("musics", G::Music, K::Link, "Secure variant of music://"),
    def(
        "itms",
        G::Music,
        K::Link,
        "Older Apple Music and iTunes Store links",
    ),
    def("itmss", G::Music, K::Link, "Secure variant of itms://"),
    def("itunes", G::Music, K::Link, "iTunes links"),
    def("itals", G::Store, K::Link, "iTunes library link"),
    def("italss", G::Store, K::Link, "Secure variant of itals://"),
    def("itvls", G::Store, K::Link, "iTunes video link"),
    def("itvlss", G::Store, K::Link, "Secure variant of itvls://"),
    def("itsradio", G::Radio, K::Radio, "iTunes Radio station links"),
    def(
        "itunesradio",
        G::Radio,
        K::Radio,
        "iTunes Radio station links",
    ),
    def(
        "daap",
        G::Other,
        K::Unsupported(Unsupported::Daap),
        "Shared iTunes libraries on your network (Rustet shows a notice)",
    ),
    def(
        "itpc",
        G::Other,
        K::Unsupported(Unsupported::Podcast),
        "Podcast subscriptions (Rustet shows a notice)",
    ),
    alias(
        "itunes.assocprotocol.daap",
        K::Unsupported(Unsupported::Daap),
        "Windows alias of daap://",
    ),
    alias(
        "itunes.assocprotocol.itals",
        K::Link,
        "Windows alias of itals://",
    ),
    alias(
        "itunes.assocprotocol.italss",
        K::Link,
        "Windows alias of italss://",
    ),
    alias(
        "itunes.assocprotocol.itls",
        K::Link,
        "Windows alias of itls://",
    ),
    alias(
        "itunes.assocprotocol.itlss",
        K::Link,
        "Windows alias of itlss://",
    ),
    alias(
        "itunes.assocprotocol.itms",
        K::Link,
        "Windows alias of itms://",
    ),
    alias(
        "itunes.assocprotocol.itmss",
        K::Link,
        "Windows alias of itmss://",
    ),
    alias(
        "itunes.assocprotocol.itpc",
        K::Unsupported(Unsupported::Podcast),
        "Windows alias of itpc://",
    ),
    alias(
        "itunes.assocprotocol.itsradio",
        K::Radio,
        "Windows alias of itsradio://",
    ),
    alias(
        "itunes.assocprotocol.itvls",
        K::Link,
        "Windows alias of itvls://",
    ),
    alias(
        "itunes.assocprotocol.itvlss",
        K::Link,
        "Windows alias of itvlss://",
    ),
];

/// A scheme's kind; `None` for schemes Apple Music does not register (`itls` and `itlss` exist
/// only as Windows aliases).
fn scheme_kind(scheme: &str) -> Option<SchemeKind> {
    APPLE_SCHEMES
        .iter()
        .find(|s| s.scheme == scheme)
        .map(|s| s.kind)
}

/// Every host Apple serves Apple Music pages from; all of them open as music.apple.com.
const APPLE_HOSTS: [&str; 8] = [
    "music.apple.com",
    "beta.music.apple.com",
    "classical.music.apple.com",
    "embed.music.apple.com",
    "geo.music.apple.com",
    "itunes.apple.com",
    "geo.itunes.apple.com",
    "beta.itunes.apple.com",
];

/// A catalog item type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ItemType {
    Song,
    Album,
    Artist,
    Playlist,
    MusicVideo,
    Station,
}

impl ItemType {
    /// A path segment of a music.apple.com item URL (stations are not play intents).
    fn from_path(s: &str) -> Option<Self> {
        Some(match s {
            "song" => Self::Song,
            "album" => Self::Album,
            "artist" => Self::Artist,
            "playlist" => Self::Playlist,
            "music-video" => Self::MusicVideo,
            _ => return None,
        })
    }

    /// The short code in `rustet://play/<code>/<id>` and `rustet://open/<code>/<id>`.
    fn from_short(s: &str) -> Option<Self> {
        Some(match s {
            "s" => Self::Song,
            "a" => Self::Album,
            "p" => Self::Playlist,
            "r" => Self::Station,
            "v" => Self::MusicVideo,
            "ar" => Self::Artist,
            _ => return None,
        })
    }

    const fn path(self) -> &'static str {
        match self {
            Self::Song => "song",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::MusicVideo => "music-video",
            Self::Station => "station",
        }
    }

    /// Numeric ids, `pl.` playlists, `ra.` (or numeric) stations.
    fn valid_id(self, id: &str) -> bool {
        let numeric =
            |s: &str| (1..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
        let dotted = |prefix: &str| {
            id.strip_prefix(prefix).is_some_and(|r| {
                (1..=80).contains(&r.len())
                    && r.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            })
        };
        match self {
            Self::Playlist => dotted("pl."),
            Self::Station => dotted("ra.") || numeric(id),
            _ => numeric(id),
        }
    }
}

/// Play or open one catalog item; the page handles it (`rustet:open`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Intent {
    #[serde(rename = "type")]
    pub kind: ItemType,
    pub id: String,
    /// Two-letter storefront, lower case.
    pub sf: Option<String>,
}

/// What a link asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Intent(Intent),
    /// Any other music.apple.com page (browse, radio, search, …): always an
    /// `https://music.apple.com` URL.
    Navigate(String),
    /// A scheme Apple Music registers but a web client cannot act on.
    Unsupported {
        reason: Unsupported,
        scheme: String,
    },
}

fn intent(kind: ItemType, id: &str, sf: Option<String>) -> Option<Link> {
    Some(Link::Intent(Intent {
        kind,
        id: id.to_owned(),
        sf,
    }))
}

fn navigate(path_and_query: &str) -> Option<Link> {
    let mut url = Url::parse(&format!("https://music.apple.com{path_and_query}")).ok()?;
    url.set_fragment(None);
    Some(Link::Navigate(url.into()))
}

fn storefront(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic())
}

/// `?…` when the URL has a non-empty query (JavaScript's `url.search`).
fn search(u: &Url) -> String {
    u.query()
        .filter(|q| !q.is_empty())
        .map(|q| format!("?{q}"))
        .unwrap_or_default()
}

fn query_param(u: &Url, name: &str) -> Option<String> {
    u.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// A URL whose host is an Apple Music host (whatever its scheme).
fn from_apple_url(u: &Url) -> Option<Link> {
    // Old iTunes store routes (WebObjects/MZStore.woa/…) have no music.apple.com equivalent.
    if u.path().to_ascii_lowercase().contains("webobjects") {
        return None;
    }
    let parts: Vec<&str> = u.path().split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return navigate("/");
    }
    let (sf, i) = if storefront(parts[0]) {
        (Some(parts[0].to_ascii_lowercase()), 1)
    } else {
        (None, 0)
    };
    if let Some(kind) = parts.get(i).and_then(|t| ItemType::from_path(t))
        && let Some(last) = parts.get(i + 1..).and_then(<[&str]>::last)
    {
        // Legacy iTunes ids: id123 → 123.
        let id = last
            .strip_prefix("id")
            .filter(|r| r.starts_with(|c: char| c.is_ascii_digit()))
            .unwrap_or(last);
        // An album link with ?i=<id> points at one song on that album.
        if kind == ItemType::Album
            && let Some(track) = query_param(u, "i").filter(|t| ItemType::Song.valid_id(t))
        {
            return intent(ItemType::Song, &track, sf);
        }
        if kind.valid_id(id) {
            return intent(kind, id, sf);
        }
    }
    navigate(&format!("{}{}", u.path(), search(u)))
}

fn is_apple_host(host: &str) -> bool {
    APPLE_HOSTS.contains(&host)
}

fn from_own_url(u: &Url, host: &str) -> Option<Link> {
    if is_apple_host(host) {
        return from_apple_url(u); // rustet://music.apple.com/us/album/…
    }
    let parts: Vec<&str> = u.path().split('/').filter(|p| !p.is_empty()).collect();
    let sf = query_param(u, "sf")
        .filter(|s| storefront(s))
        .map(|s| s.to_ascii_lowercase());
    if host == "play" || host == "open" {
        let kind = ItemType::from_short(
            &parts
                .first()
                .copied()
                .unwrap_or_default()
                .to_ascii_lowercase(),
        )?;
        let id = parts.get(1).copied().unwrap_or_default();
        if !kind.valid_id(id) {
            return None;
        }
        if host == "play" && kind != ItemType::Station {
            return intent(kind, id, sf);
        }
        return navigate(&format!(
            "/{}/{}/{id}",
            sf.as_deref().unwrap_or("us"),
            kind.path()
        ));
    }
    // rustet://<type>/<id>: the host is the type.
    let kind = ItemType::from_path(host)?;
    let id = parts.first().copied().unwrap_or_default();
    if kind.valid_id(id) {
        intent(kind, id, None)
    } else {
        None
    }
}

/// What `raw` asks for, or `None` for anything that is not a link Rustet handles.
pub fn parse_link(raw: &str) -> Option<Link> {
    let text = raw.trim();
    if text.is_empty() || text.encode_utf16().count() > 2048 {
        return None;
    }
    let u = Url::parse(text).ok()?;
    let scheme = u.scheme().to_ascii_lowercase();
    let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
    match scheme.as_str() {
        OWN_SCHEME => from_own_url(&u, &host),
        "https" | "http" => is_apple_host(&host).then(|| from_apple_url(&u)).flatten(),
        _ => match scheme_kind(&scheme)? {
            SchemeKind::Unsupported(reason) => Some(Link::Unsupported { reason, scheme }),
            _ if is_apple_host(&host) => from_apple_url(&u),
            SchemeKind::Radio => navigate("/radio"),
            SchemeKind::Link => None,
        },
    }
}

/// The first command-line argument that is a link Rustet handles.
pub fn find_link_arg<S: AsRef<str>>(args: &[S]) -> Option<&str> {
    args.iter()
        .map(AsRef::as_ref)
        .find(|a| parse_link(a).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(kind: ItemType, id: &str, sf: Option<&str>) -> Option<Link> {
        Some(Link::Intent(Intent {
            kind,
            id: id.into(),
            sf: sf.map(Into::into),
        }))
    }
    fn nav(url: &str) -> Option<Link> {
        Some(Link::Navigate(url.into()))
    }
    use ItemType::*;

    #[test]
    fn https_links_resolve_to_play_intents() {
        assert_eq!(
            parse_link("https://music.apple.com/us/album/abbey-road/1441164426"),
            i(Album, "1441164426", Some("us"))
        );
        assert_eq!(
            parse_link("https://music.apple.com/in/song/something/1441164430"),
            i(Song, "1441164430", Some("in"))
        );
        assert_eq!(
            parse_link("https://music.apple.com/us/artist/the-beatles/136975"),
            i(Artist, "136975", Some("us"))
        );
        assert_eq!(
            parse_link(
                "https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb"
            ),
            i(Playlist, "pl.f4d106fed2bd41149aaacabb233eb5eb", Some("us"))
        );
        assert_eq!(
            parse_link("https://music.apple.com/us/music-video/hey-jude/1440123456"),
            i(MusicVideo, "1440123456", Some("us"))
        );
        assert_eq!(
            parse_link("https://music.apple.com/us/album/1441164426"),
            i(Album, "1441164426", Some("us"))
        );
    }

    #[test]
    fn album_link_with_i_means_that_song() {
        assert_eq!(
            parse_link("https://music.apple.com/us/album/abbey-road/1441164426?i=1441164430"),
            i(Song, "1441164430", Some("us"))
        );
        assert_eq!(
            parse_link("https://music.apple.com/us/album/abbey-road/1441164426?i=abc"),
            i(Album, "1441164426", Some("us"))
        );
    }

    #[test]
    fn every_apple_host_and_tracking_parameters_ignored() {
        for host in [
            "beta.music.apple.com",
            "classical.music.apple.com",
            "embed.music.apple.com",
            "geo.music.apple.com",
            "itunes.apple.com",
            "geo.itunes.apple.com",
        ] {
            assert_eq!(
                parse_link(&format!(
                    "https://{host}/us/album/x/123?app=music&ls=1&at=abc"
                )),
                i(Album, "123", Some("us")),
                "{host}"
            );
        }
        assert_eq!(
            parse_link("http://music.apple.com/us/album/x/123"),
            i(Album, "123", Some("us"))
        );
    }

    #[test]
    fn legacy_ids_are_normalised() {
        assert_eq!(
            parse_link("https://itunes.apple.com/us/album/abbey-road/id1441164426"),
            i(Album, "1441164426", Some("us"))
        );
        assert_eq!(
            parse_link("https://itunes.apple.com/WebObjects/MZStore.woa/wa/viewAlbum?id=1"),
            None
        );
    }

    #[test]
    fn every_link_scheme_and_windows_alias_carries_the_same_links() {
        let schemes = [
            "music", "musics", "itms", "itmss", "itunes", "itals", "italss", "itvls", "itvlss",
        ];
        let aliases = [
            "itunes.assocprotocol.itms",
            "itunes.assocprotocol.itls",
            "itunes.assocprotocol.itlss",
            "itunes.assocprotocol.itvls",
        ];
        for s in schemes.iter().chain(&aliases) {
            assert_eq!(
                parse_link(&format!("{s}://music.apple.com/us/album/x/123")),
                i(Album, "123", Some("us")),
                "{s}"
            );
            assert_eq!(
                parse_link(&format!("{s}://itunes.apple.com/us/album/x/id123?i=456")),
                i(Song, "456", Some("us")),
                "{s}"
            );
            assert_eq!(
                parse_link(&format!("{s}://geo.itunes.apple.com/us/album/123")),
                i(Album, "123", Some("us")),
                "{s}"
            );
            assert_eq!(
                parse_link(&format!("{s}://example.com/us/album/x/123")),
                None,
                "{s} with a foreign host"
            );
        }
        assert_eq!(
            parse_link("itls://music.apple.com/us/album/x/1"),
            None,
            "itls exists only as an alias"
        );
    }

    #[test]
    fn radio_schemes() {
        for s in ["itsradio", "itunesradio", "itunes.assocprotocol.itsradio"] {
            assert_eq!(
                parse_link(&format!("{s}://")),
                nav("https://music.apple.com/radio"),
                "{s}"
            );
            assert_eq!(
                parse_link(&format!("{s}://music.apple.com/us/station/pop/ra.123")),
                nav("https://music.apple.com/us/station/pop/ra.123"),
                "{s}"
            );
        }
    }

    #[test]
    fn unsupported_schemes_are_reported() {
        assert_eq!(
            parse_link("itpc://example.com/feed.xml"),
            Some(Link::Unsupported {
                reason: Unsupported::Podcast,
                scheme: "itpc".into()
            })
        );
        assert_eq!(
            parse_link("daap://192.168.1.5:3689/"),
            Some(Link::Unsupported {
                reason: Unsupported::Daap,
                scheme: "daap".into()
            })
        );
        assert!(matches!(
            parse_link("itunes.assocprotocol.itpc://x.com/f"),
            Some(Link::Unsupported {
                reason: Unsupported::Podcast,
                ..
            })
        ));
        assert!(matches!(
            parse_link("itunes.assocprotocol.daap://x/"),
            Some(Link::Unsupported {
                reason: Unsupported::Daap,
                ..
            })
        ));
    }

    #[test]
    fn the_scheme_table_is_what_apple_music_registers() {
        let mut base: Vec<_> = APPLE_SCHEMES
            .iter()
            .filter(|s| !s.windows_only)
            .map(|s| s.scheme)
            .collect();
        base.sort();
        let mut want = [
            "daap",
            "itals",
            "italss",
            "itmss",
            "itms",
            "itpc",
            "itsradio",
            "itunes",
            "itunesradio",
            "itvls",
            "itvlss",
            "music",
            "musics",
        ];
        want.sort();
        assert_eq!(base, want);
        let mut aliases: Vec<_> = APPLE_SCHEMES
            .iter()
            .filter(|s| s.windows_only)
            .map(|s| s.scheme.trim_start_matches("itunes.assocprotocol."))
            .collect();
        aliases.sort();
        assert_eq!(
            aliases,
            [
                "daap", "itals", "italss", "itls", "itlss", "itms", "itmss", "itpc", "itsradio",
                "itvls", "itvlss"
            ]
        );
        for s in APPLE_SCHEMES.iter().filter(|s| s.windows_only) {
            let base = s.scheme.trim_start_matches("itunes.assocprotocol.");
            assert_eq!(s.note, format!("Windows alias of {base}://"));
            assert_eq!(
                s.kind,
                scheme_kind(base).unwrap_or(SchemeKind::Link),
                "{base}"
            );
        }
    }

    #[test]
    fn other_apple_pages_open_inside_the_app() {
        for p in [
            "/us/browse",
            "/us/radio",
            "/us/search?term=beatles",
            "/us/new",
            "/us/curator/apple-music-pop/976439514",
            "/us/room/6748797380",
            "/library/playlist/p.abc123",
            "/us/station/pop/ra.123",
            "/us",
            "/",
        ] {
            assert_eq!(
                parse_link(&format!("https://music.apple.com{p}")),
                nav(&format!("https://music.apple.com{p}")),
                "{p}"
            );
        }
        assert_eq!(
            parse_link("https://beta.music.apple.com/us/browse#top"),
            nav("https://music.apple.com/us/browse")
        );
    }

    #[test]
    fn an_invalid_id_never_becomes_a_play_intent() {
        assert_eq!(
            parse_link("https://music.apple.com/us/album/x/not-a-number"),
            nav("https://music.apple.com/us/album/x/not-a-number")
        );
        assert_eq!(
            parse_link("https://music.apple.com/us/playlist/x/p.library"),
            nav("https://music.apple.com/us/playlist/x/p.library")
        );
    }

    #[test]
    fn rustet_forms() {
        assert_eq!(parse_link("rustet://song/123"), i(Song, "123", None));
        assert_eq!(parse_link("rustet://album/123"), i(Album, "123", None));
        assert_eq!(
            parse_link("rustet://playlist/pl.abc"),
            i(Playlist, "pl.abc", None)
        );
        assert_eq!(parse_link("rustet://play/s/123"), i(Song, "123", None));
        assert_eq!(
            parse_link("rustet://play/a/123?sf=in"),
            i(Album, "123", Some("in"))
        );
        assert_eq!(
            parse_link("rustet://play/p/pl.abc"),
            i(Playlist, "pl.abc", None)
        );
        assert_eq!(
            parse_link("rustet://play/r/ra.123"),
            nav("https://music.apple.com/us/station/ra.123")
        );
        assert_eq!(
            parse_link("rustet://open/a/123?sf=gb"),
            nav("https://music.apple.com/gb/album/123")
        );
        assert_eq!(
            parse_link("rustet://music.apple.com/us/album/x/123"),
            i(Album, "123", Some("us"))
        );
        assert_eq!(parse_link("rustet://Song/123"), i(Song, "123", None));
        for bad in [
            "rustet://song/",
            "rustet://song/abc",
            "rustet://play/x/1",
            "rustet://unknown/1",
        ] {
            assert_eq!(parse_link(bad), None, "{bad}");
        }
    }

    #[test]
    fn hostile_and_malformed_input_is_rejected() {
        let long = "x".repeat(5000);
        for bad in [
            "",
            "   ",
            "not a url",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://evil.example/music.apple.com/us/album/x/1",
            "https://music.apple.com.evil.example/us/album/x/1",
            "https://evil.example/",
            "ftp://music.apple.com/us/album/x/1",
            &long,
        ] {
            assert_eq!(parse_link(bad), None, "{}", &bad[..bad.len().min(40)]);
        }
        let Some(Link::Navigate(url)) = parse_link("music://geo.itunes.apple.com/us/browse") else {
            panic!()
        };
        assert!(url.starts_with("https://music.apple.com/"));
    }

    #[test]
    fn intent_wire_format() {
        let Some(Link::Intent(it)) = parse_link("https://music.apple.com/us/music-video/x/1")
        else {
            panic!()
        };
        assert_eq!(
            serde_json::to_value(it).unwrap(),
            serde_json::json!({ "type": "music-video", "id": "1", "sf": "us" })
        );
        let Some(Link::Intent(it)) = parse_link("rustet://song/1") else {
            panic!()
        };
        assert_eq!(
            serde_json::to_value(it).unwrap(),
            serde_json::json!({ "type": "song", "id": "1", "sf": null })
        );
    }

    #[test]
    fn find_link_arg_picks_links_out_of_argv() {
        let argv = [
            "/usr/bin/rustet",
            "--no-sandbox",
            "music://music.apple.com/us/album/x/1",
        ];
        assert_eq!(find_link_arg(&argv), Some(argv[2]));
        assert_eq!(find_link_arg(&["--hidden", "--aml-cmd=next"]), None);
    }
}
