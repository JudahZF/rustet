//! Where the page stack can go (native-UI spec §6.1): one [`Route`] per screen.
//!
//! A route comes from a music.apple.com URL, an `rustet://` link or a page model, and turns back
//! into an `rustet://` link ([`Route::to_link`]) and, for catalog pages, into a music.apple.com
//! share link ([`Route::share_link`]).
//!
//! Item links (album, song, artist, playlist, music video) go through
//! [`rustet_desktop::links::parse_link`], the parser the shell already uses. The pages it leaves
//! as a plain music.apple.com URL (new, radio, search, station, room, curator, "see all",
//! library) are read here, and so are Rustet's own page links (`rustet://library/albums`, …).

use rustet_ampapi::ArtistView;
use rustet_desktop::links::{ItemType, Link, parse_link};
use url::Url;

/// Every artist section, for parsing [`ArtistView`] names.
pub const ARTIST_VIEWS: [ArtistView; 8] = [
    ArtistView::LatestRelease,
    ArtistView::TopSongs,
    ArtistView::FullAlbums,
    ArtistView::Singles,
    ArtistView::TopMusicVideos,
    ArtistView::FeaturedPlaylists,
    ArtistView::AppearsOnAlbums,
    ArtistView::SimilarArtists,
];

/// The view of the artist section named `name` (`full-albums`, …).
pub fn artist_view(name: &str) -> Option<ArtistView> {
    ARTIST_VIEWS.into_iter().find(|v| v.as_str() == name)
}

/// A screen of the main window.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Route {
    /// Search results for `term`; an empty term is the search landing page.
    Search {
        term: String,
    },
    /// The editorial "New" page.
    New,
    Radio,
    /// "Home" (Listen Now): needs the music user token.
    Home,
    /// Top charts, for one genre or (`None`) all music.
    Charts {
        genre: Option<String>,
    },
    Album {
        id: String,
    },
    Playlist {
        id: String,
    },
    Artist {
        id: String,
    },
    /// One artist section in full ("see all").
    ArtistView {
        id: String,
        view: ArtistView,
    },
    Song {
        id: String,
    },
    MusicVideo {
        id: String,
    },
    Station {
        id: String,
    },
    /// The "see all" list of an editorial shelf.
    Room {
        id: String,
    },
    /// An Apple curator or another curator.
    Curator {
        id: String,
    },
    /// A section of the user's library: needs the music user token.
    Library(LibrarySection),
    /// Library search for `term`: needs the music user token.
    LibrarySearch {
        term: String,
    },
    /// A library album (`l.…`): needs the music user token.
    LibraryAlbum {
        id: String,
    },
    /// A library playlist (`p.…`): needs the music user token.
    LibraryPlaylist {
        id: String,
    },
    Downloads,
    /// Settings, optionally opened at a section.
    Settings {
        section: Option<String>,
    },
}

/// A section of the user's library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibrarySection {
    RecentlyAdded,
    /// Apple's listening history: recently played albums, playlists and stations.
    RecentlyPlayed,
    Albums,
    Songs,
    Artists,
    Playlists,
    MusicVideos,
}

impl LibrarySection {
    /// The path word in `rustet://library/<word>` and music.apple.com `/library/<word>`.
    fn slug(self) -> &'static str {
        match self {
            Self::RecentlyAdded => "recently-added",
            Self::RecentlyPlayed => "recently-played",
            Self::Albums => "albums",
            Self::Songs => "songs",
            Self::Artists => "artists",
            Self::Playlists => "playlists",
            Self::MusicVideos => "music-videos",
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        Some(match slug {
            "recently-added" => Self::RecentlyAdded,
            "recently-played" => Self::RecentlyPlayed,
            "albums" => Self::Albums,
            "songs" => Self::Songs,
            "artists" => Self::Artists,
            // Apple's own path is `all-playlists`.
            "playlists" | "all-playlists" => Self::Playlists,
            "music-videos" => Self::MusicVideos,
            _ => return None,
        })
    }
}

impl Route {
    /// The route `raw` (a music.apple.com URL, an `rustet://` link or another Apple Music scheme)
    /// opens, or `None` when Rustet cannot show it.
    pub fn from_link(raw: &str) -> Option<Self> {
        let url = Url::parse(raw.trim()).ok()?;
        if url.scheme().eq_ignore_ascii_case("rustet")
            && let Some(page) = from_own_page(&url)
        {
            return page;
        }
        match parse_link(raw)? {
            Link::Intent(intent) => {
                let id = intent.id;
                Some(match intent.kind {
                    ItemType::Song => Self::Song { id },
                    ItemType::Album => Self::Album { id },
                    ItemType::Artist => Self::Artist { id },
                    ItemType::Playlist => Self::Playlist { id },
                    ItemType::MusicVideo => Self::MusicVideo { id },
                    ItemType::Station => Self::Station { id },
                })
            }
            Link::Navigate(url) => from_apple_page(&Url::parse(&url).ok()?),
            Link::Unsupported { .. } => None,
        }
    }

    /// The `rustet://` link that opens this route again (`Route::from_link` reads it back).
    pub fn to_link(&self) -> String {
        let mut url = Url::parse("rustet://open").expect("static URL");
        let (host, path): (&str, Vec<&str>) = match self {
            Self::Search { .. } => ("search", vec![]),
            Self::New => ("new", vec![]),
            Self::Radio => ("radio", vec![]),
            Self::Home => ("home", vec![]),
            Self::Charts { .. } => ("charts", vec![]),
            Self::Album { id } => ("album", vec![id]),
            Self::Playlist { id } => ("playlist", vec![id]),
            Self::Artist { id } => ("artist", vec![id]),
            Self::ArtistView { id, view } => ("artist", vec![id, "see-all", view.as_str()]),
            Self::Song { id } => ("song", vec![id]),
            Self::MusicVideo { id } => ("music-video", vec![id]),
            Self::Station { id } => ("station", vec![id]),
            Self::Room { id } => ("room", vec![id]),
            Self::Curator { id } => ("curator", vec![id]),
            Self::Library(section) => ("library", vec![section.slug()]),
            Self::LibrarySearch { .. } => ("library", vec!["search"]),
            Self::LibraryAlbum { id } => ("library", vec!["album", id]),
            Self::LibraryPlaylist { id } => ("library", vec!["playlist", id]),
            Self::Downloads => ("downloads", vec![]),
            Self::Settings { section } => {
                ("settings", section.iter().map(String::as_str).collect())
            }
        };
        url.set_host(Some(host)).expect("valid host");
        url.path_segments_mut()
            .expect("rustet:// has a path")
            .clear()
            .extend(path);
        match self {
            Self::Search { term } | Self::LibrarySearch { term } => {
                url.query_pairs_mut().append_pair("term", term);
            }
            Self::Charts { genre: Some(genre) } => {
                url.query_pairs_mut().append_pair("genre", genre);
            }
            _ => {}
        }
        let link = String::from(url);
        // `rustet://new/` and `rustet://new` are the same page; keep the short form.
        link.strip_suffix('/').map(str::to_owned).unwrap_or(link)
    }

    /// The music.apple.com link to share for a catalog page in `storefront` (two letters).
    /// `None` for personal pages (Home, library, downloads, settings).
    pub fn share_link(&self, storefront: &str) -> Option<String> {
        let sf = storefront.to_ascii_lowercase();
        let mut url = Url::parse("https://music.apple.com/").expect("static URL");
        let mut query: Option<(&str, &str)> = None;
        let path: Vec<&str> = match self {
            Self::Search { term } => {
                query = Some(("term", term));
                vec!["search"]
            }
            Self::New => vec!["new"],
            Self::Radio => vec!["radio"],
            Self::Charts { genre } => {
                query = genre.as_deref().map(|g| ("genre", g));
                vec!["charts"]
            }
            Self::Album { id } => vec!["album", id],
            Self::Playlist { id } => vec!["playlist", id],
            Self::Artist { id } => vec!["artist", id],
            Self::ArtistView { id, view } => {
                query = Some(("section", view.as_str()));
                vec!["artist", id, "see-all"]
            }
            Self::Song { id } => vec!["song", id],
            Self::MusicVideo { id } => vec!["music-video", id],
            Self::Station { id } => vec!["station", id],
            Self::Room { id } => vec!["room", id],
            Self::Curator { id } => vec!["curator", id],
            Self::Home
            | Self::Library(_)
            | Self::LibrarySearch { .. }
            | Self::LibraryAlbum { .. }
            | Self::LibraryPlaylist { .. }
            | Self::Downloads
            | Self::Settings { .. } => return None,
        };
        url.path_segments_mut()
            .expect("https has a path")
            .clear()
            .push(&sf)
            .extend(path);
        if let Some((key, value)) = query {
            url.query_pairs_mut().append_pair(key, value);
        }
        Some(url.into())
    }

    /// The route's loader sends the music user token (it fails signed out).
    pub fn needs_user_token(&self) -> bool {
        matches!(
            self,
            Self::Home
                | Self::Library(_)
                | Self::LibrarySearch { .. }
                | Self::LibraryAlbum { .. }
                | Self::LibraryPlaylist { .. }
        )
    }

    /// What the route shows, for messages ("Rustet could not load this album.").
    pub fn noun(&self) -> &'static str {
        match self {
            Self::Search { term } if term.is_empty() => "the search page",
            Self::Search { .. } => "the search results",
            Self::New => "the New page",
            Self::Radio => "the Radio page",
            Self::Home => "Home",
            Self::Charts { .. } => "the charts",
            Self::Album { .. } | Self::LibraryAlbum { .. } => "this album",
            Self::Playlist { .. } | Self::LibraryPlaylist { .. } => "this playlist",
            Self::Artist { .. } | Self::ArtistView { .. } => "this artist",
            Self::Song { .. } => "this song",
            Self::MusicVideo { .. } => "this music video",
            Self::Station { .. } => "this station",
            Self::Room { .. } => "this list",
            Self::Curator { .. } => "this curator",
            Self::LibrarySearch { .. } => "the library search results",
            Self::Library(_) => "your library",
            Self::Downloads => "downloads",
            Self::Settings { .. } => "settings",
        }
    }
}

fn query_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// Apple's IDs: digits, `pl.…`, `ra.…`, `l.…`, `p.…`.
fn valid_id(id: &str) -> bool {
    (1..=100).contains(&id.len())
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn id_route(id: &str, route: impl FnOnce(String) -> Route) -> Option<Route> {
    valid_id(id).then(|| route(id.to_owned()))
}

/// The last path segment that is an ID (`/us/station/apple-music-1/ra.978194965`).
fn last_id(rest: &[&str], valid: impl Fn(&str) -> bool) -> Option<String> {
    rest.last()
        .filter(|id| valid(id))
        .map(|id| (*id).to_owned())
}

fn numeric(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// The page hosts of `rustet://` links that [`parse_link`] does not know.
const OWN_PAGES: [&str; 11] = [
    "search",
    "new",
    "radio",
    "home",
    "charts",
    "station",
    "room",
    "curator",
    "library",
    "downloads",
    "settings",
];

/// `rustet://<page>/…` links for pages the shell's parser does not know: `None` when the link is
/// not one (item links such as `rustet://album/<id>` and `rustet://play/…` go to [`parse_link`]),
/// `Some(None)` when it is one Rustet cannot show.
fn from_own_page(url: &Url) -> Option<Option<Route>> {
    let host = url.host_str()?.to_ascii_lowercase();
    // `rustet://search?term=x` has an empty path, which `path_segments` does not split.
    let parts: Vec<&str> = url.path().split('/').filter(|p| !p.is_empty()).collect();
    let see_all = host == "artist" && parts.get(1) == Some(&"see-all");
    if !see_all && !OWN_PAGES.contains(&host.as_str()) {
        return None;
    }
    let term = || query_param(url, "term").unwrap_or_default();
    Some(match (host.as_str(), parts.as_slice()) {
        ("search", []) => Some(Route::Search { term: term() }),
        ("new", []) => Some(Route::New),
        ("radio", []) => Some(Route::Radio),
        ("home", []) => Some(Route::Home),
        ("charts", []) => Some(Route::Charts {
            genre: query_param(url, "genre").filter(|g| valid_id(g)),
        }),
        ("artist", [id, "see-all", view]) if numeric(id) => {
            artist_view(view).map(|view| Route::ArtistView {
                id: (*id).to_owned(),
                view,
            })
        }
        ("station", [id]) => id_route(id, |id| Route::Station { id }),
        ("room", [id]) => id_route(id, |id| Route::Room { id }),
        ("curator", [id]) => id_route(id, |id| Route::Curator { id }),
        ("library", ["album", id]) => id_route(id, |id| Route::LibraryAlbum { id }),
        ("library", ["playlist", id]) => id_route(id, |id| Route::LibraryPlaylist { id }),
        ("library", ["search"]) => Some(Route::LibrarySearch { term: term() }),
        ("library", [section]) => LibrarySection::from_slug(section).map(Route::Library),
        ("downloads", []) => Some(Route::Downloads),
        ("settings", []) => Some(Route::Settings { section: None }),
        ("settings", [section]) => Some(Route::Settings {
            section: Some((*section).to_owned()),
        }),
        _ => None,
    })
}

/// A music.apple.com page that is not a single catalog item.
fn from_apple_page(url: &Url) -> Option<Route> {
    let parts: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
    let storefront = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic());
    let parts = match parts.split_first() {
        Some((first, rest)) if storefront(first) => rest,
        _ => &parts[..],
    };
    let Some((page, rest)) = parts.split_first() else {
        return Some(Route::New);
    };
    match (*page, rest) {
        ("new" | "browse", []) => Some(Route::New),
        ("radio", []) => Some(Route::Radio),
        ("home" | "listen-now" | "for-you", []) => Some(Route::Home),
        ("search", []) => Some(Route::Search {
            term: query_param(url, "term").unwrap_or_default(),
        }),
        ("charts", _) => Some(Route::Charts {
            genre: query_param(url, "genre").filter(|g| valid_id(g)),
        }),
        // `/us/artist/<slug>/<id>/see-all?section=full-albums`
        ("artist", [.., id, "see-all"]) if numeric(id) => Some(Route::ArtistView {
            id: (*id).to_owned(),
            view: artist_view(&query_param(url, "section")?)?,
        }),
        ("station", rest) => last_id(rest, |id| {
            numeric(id) || id.strip_prefix("ra.").is_some_and(valid_id)
        })
        .map(|id| Route::Station { id }),
        ("room", rest) => last_id(rest, numeric).map(|id| Route::Room { id }),
        ("curator" | "apple-curator", rest) => {
            last_id(rest, numeric).map(|id| Route::Curator { id })
        }
        ("library", ["playlist" | "playlists", id]) if id.starts_with("p.") => {
            id_route(id, |id| Route::LibraryPlaylist { id })
        }
        ("library", ["albums" | "album", id]) if id.starts_with("l.") => {
            id_route(id, |id| Route::LibraryAlbum { id })
        }
        ("library", [section]) => LibrarySection::from_slug(section).map(Route::Library),
        _ => None,
    }
}
