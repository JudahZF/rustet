//! Catalog and editorial pages. Every loader here works signed out (developer token only);
//! signed in, New and Radio show the subscriber tab. Each is one request (Radio and Charts:
//! two in parallel) through `rustet-ampapi`'s response cache, so Back is instant.

use std::time::Duration;

use rustet_ampapi::model::{self as am, Resource, SearchGroup as AmGroup, Suggestion};
use rustet_ampapi::{Api, ArtistView, BrowsePage as AmBrowse};
use tokio_util::sync::CancellationToken;

use super::error::LoadError;
use super::map::{self, artwork, notes, plain};
use super::model::{
    Date, EmptyState, Header, Linked, List, Listing, QualityMarks, Shelf, ShelfItems, Tile,
    TileSize, TrackRow,
};
use super::more::LoadMore;
use super::play::{PlayTarget, StationRef};
use crate::route::Route;

/// Parameters an artist "see all" `next` link must keep.
const ARTIST_VIEW_PARAMS: &[(&str, &str)] = &[
    ("limit", "20"),
    ("with", "attributes"),
    ("include[albums]", "artists"),
    ("platform", "web"),
];
/// Parameters a room's `next` link must keep.
const ROOM_PARAMS: &[(&str, &str)] = &[
    ("include[albums]", "artists"),
    ("include[songs]", "artists"),
    ("relate[songs]", "albums"),
    ("platform", "web"),
];

/// A page of shelves: New, Radio, Home, the search landing page, a curator's page.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowsePage {
    pub title: String,
    pub shelves: Vec<Shelf>,
}

/// One group of search results.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SearchGroup {
    Top,
    Artists,
    Albums,
    Songs,
    Playlists,
    Stations,
    MusicVideos,
    RadioEpisodes,
    Curators,
    RecordLabels,
    /// A group Rustet has no name for; Apple's key.
    Other(String),
}

impl SearchGroup {
    pub(crate) fn from_key(key: &str) -> Self {
        match key {
            "top" => Self::Top,
            "artist" | "artists" | "library-artists" => Self::Artists,
            "album" | "albums" | "library-albums" => Self::Albums,
            "song" | "songs" | "library-songs" => Self::Songs,
            "playlist" | "playlists" | "library-playlists" => Self::Playlists,
            "station" | "stations" => Self::Stations,
            "music_video" | "music-videos" | "library-music-videos" => Self::MusicVideos,
            "radio_episode" => Self::RadioEpisodes,
            "curator" | "apple_curator" | "apple-curators" | "curators" => Self::Curators,
            "record_label" | "record-labels" => Self::RecordLabels,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The heading when Apple gives none.
    pub fn default_title(&self) -> &str {
        match self {
            Self::Top => "Top Results",
            Self::Artists => "Artists",
            Self::Albums => "Albums",
            Self::Songs => "Songs",
            Self::Playlists => "Playlists",
            Self::Stations => "Stations",
            Self::MusicVideos => "Music Videos",
            Self::RadioEpisodes => "Episodes",
            Self::Curators => "Curators",
            Self::RecordLabels => "Record Labels",
            Self::Other(key) => key,
        }
    }

    /// Songs and videos are drawn as a track list, the rest as tiles.
    fn is_tracks(&self) -> bool {
        matches!(self, Self::Songs | Self::MusicVideos)
    }
}

/// A section of the search results page, in Apple's order.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchSection {
    pub group: SearchGroup,
    pub title: String,
    pub listing: Listing,
}

/// Search results (catalog or library).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub term: String,
    pub sections: Vec<SearchSection>,
}

impl SearchPage {
    pub fn empty_state(&self) -> Option<EmptyState> {
        self.sections.is_empty().then(|| {
            EmptyState::new(
                &format!("No results for “{}”", self.term),
                "Check the spelling, or try another word.",
            )
        })
    }
}

/// A section of results; `more` is the group's `next` link and key (catalog search only).
pub(crate) fn search_section(
    group: SearchGroup,
    title: Option<String>,
    data: &[Resource],
    more: Option<(&am::Cursor, &str)>,
) -> Option<SearchSection> {
    let listing = if group.is_tracks() {
        Listing::Tracks(List {
            items: map::tracks(data),
            more: more.and_then(|(c, key)| LoadMore::search(Some(c.clone()), key)),
        })
    } else {
        Listing::Tiles(List {
            items: map::tiles(data),
            more: more.and_then(|(c, key)| LoadMore::search(Some(c.clone()), key)),
        })
    };
    (!listing.is_empty()).then(|| SearchSection {
        title: title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| group.default_title().to_owned()),
        group,
        listing,
    })
}

fn catalog_section(g: &AmGroup) -> Option<SearchSection> {
    let more = g.next.as_ref().map(|c| (c, g.group_id.as_str()));
    search_section(
        SearchGroup::from_key(&g.group_id),
        g.name.clone(),
        &g.data,
        more,
    )
}

/// A hint under the search field.
#[derive(Debug, Clone, PartialEq)]
pub enum Hint {
    /// A completed term: search for `term`, show `display`.
    Term { term: String, display: String },
    /// An item to open directly.
    Item(Box<Tile>),
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

/// Search hints while the user types (debounce in the view). Signed out works. An empty term
/// gives no hints and no request.
pub async fn hints(api: &Api, term: &str) -> Result<Vec<Hint>, LoadError> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    let suggestions = api.search_suggestions(&cancel(), term).await?;
    Ok(suggestions
        .iter()
        .filter_map(|s| match s {
            Suggestion::Term {
                search_term,
                display_term,
            } => Some(Hint::Term {
                term: search_term.clone(),
                display: display_term.clone(),
            }),
            Suggestion::TopResult { content } => {
                map::tile(content).map(|t| Hint::Item(Box::new(t)))
            }
            Suggestion::Unknown => None,
        })
        .collect())
}

/// Search results for `term`. Signed out works.
pub async fn search(api: &Api, term: &str) -> Result<SearchPage, LoadError> {
    let results = api.search(&cancel(), term.trim()).await?;
    Ok(SearchPage {
        term: term.trim().to_owned(),
        sections: results.groups.iter().filter_map(catalog_section).collect(),
    })
}

/// The search page before a term is typed: category tiles. Signed out works.
pub async fn search_landing(api: &Api) -> Result<BrowsePage, LoadError> {
    let shelves = api.search_landing(&cancel()).await?;
    Ok(BrowsePage {
        title: "Search".to_owned(),
        shelves: map::editorial_shelves(shelves),
    })
}

/// The New page. Signed out works.
pub async fn new(api: &Api) -> Result<BrowsePage, LoadError> {
    let page = api.browse(&cancel(), AmBrowse::New).await?;
    Ok(BrowsePage {
        title: page.title,
        shelves: map::editorial_shelves(page.shelves),
    })
}

/// The Radio page: the live stations, then Apple's radio shelves. Signed out works.
pub async fn radio(api: &Api) -> Result<BrowsePage, LoadError> {
    let cancel = cancel();
    let (page, live) = futures::join!(
        api.browse(&cancel, AmBrowse::Radio),
        api.live_stations(&cancel)
    );
    let page = page?;
    // The live row is a bonus: the page still shows without it.
    let live = live
        .inspect_err(|err| tracing::debug!("live stations: {err}"))
        .unwrap_or_default();
    let live_tiles = live
        .into_iter()
        .filter_map(|s| map::tile(&Resource::Station(Box::new(s))))
        .collect();
    // Square station tiles, as the web player's "Listen to Live Radio" row.
    let live_items = ShelfItems::Tiles {
        tiles: live_tiles,
        size: TileSize::Regular,
    };
    let mut shelves: Vec<Shelf> = map::shelf(
        "live-stations",
        Some("Live Radio".to_owned()),
        live_items,
        None,
    )
    .into_iter()
    .collect();
    shelves.extend(map::editorial_shelves(page.shelves));
    Ok(BrowsePage {
        title: page.title,
        shelves,
    })
}

/// A genre the charts can be filtered by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenreChoice {
    pub name: String,
    pub route: Route,
}

/// Top charts.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartsPage {
    pub genre: Option<String>,
    /// Songs, albums, playlists, music videos, daily top 100, city charts.
    pub shelves: Vec<Shelf>,
    pub genres: Vec<GenreChoice>,
}

/// The charts, for a genre or all music, with the genre list. Signed out works.
pub async fn charts(api: &Api, genre: Option<&str>) -> Result<ChartsPage, LoadError> {
    let cancel = cancel();
    let (charts, genres) = futures::join!(api.charts(&cancel, genre), api.genres(&cancel));
    let charts = charts?;
    let chart_shelf = |chart: &am::Chart, tracks: bool| {
        let items = if tracks {
            ShelfItems::Tracks(map::tracks(&chart.data))
        } else {
            ShelfItems::Tiles {
                tiles: map::tiles(&chart.data),
                size: TileSize::Regular,
            }
        };
        let id = chart.chart.clone().unwrap_or_else(|| chart.name.clone());
        map::shelf(
            id,
            Some(chart.name.clone()).filter(|n| !n.is_empty()),
            items,
            None,
        )
    };
    let groups = [
        (&charts.songs, true),
        (&charts.albums, false),
        (&charts.playlists, false),
        (&charts.music_videos, true),
        (&charts.daily_global_top_charts, false),
        (&charts.city_charts, false),
    ];
    let shelves = groups
        .into_iter()
        .flat_map(|(group, tracks)| group.iter().filter_map(move |c| chart_shelf(c, tracks)))
        .collect();
    let genres = genres
        .inspect_err(|err| tracing::debug!("genres: {err}"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|g| {
            let name = g.attributes?.name;
            Some(GenreChoice {
                name,
                route: Route::Charts { genre: Some(g.id) },
            })
        })
        .collect();
    Ok(ChartsPage {
        genre: genre.map(str::to_owned),
        shelves,
        genres,
    })
}

/// One disc of an album.
#[derive(Debug, Clone, PartialEq)]
pub struct Disc {
    pub number: u32,
    pub tracks: Vec<TrackRow>,
}

/// Consecutive tracks with the same disc number, as discs (a track without one is disc 1).
pub(crate) fn discs(rows: Vec<TrackRow>) -> Vec<Disc> {
    let mut discs: Vec<Disc> = Vec::new();
    for row in rows {
        let number = row.disc.unwrap_or(1);
        match discs.last_mut() {
            Some(disc) if disc.number == number => disc.tracks.push(row),
            _ => discs.push(Disc {
                number,
                tracks: vec![row],
            }),
        }
    }
    discs
}

fn total_duration<'a>(rows: impl Iterator<Item = &'a TrackRow>) -> Duration {
    rows.filter_map(|r| r.duration).sum()
}

/// Album facts under the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumInfo {
    pub genre: Option<String>,
    pub released: Option<Date>,
    pub record_label: Option<String>,
    pub copyright: Option<String>,
    pub single: bool,
    pub compilation: bool,
    /// Not released yet.
    pub prerelease: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlbumPage {
    pub id: String,
    pub header: Header,
    pub info: AlbumInfo,
    /// Tracks by disc; one disc for most albums.
    pub discs: Vec<Disc>,
    pub duration: Duration,
    /// More by the artist, other versions, you might also like, featured on, music videos.
    pub shelves: Vec<Shelf>,
}

impl AlbumPage {
    /// Every track in album order.
    pub fn tracks(&self) -> impl Iterator<Item = &TrackRow> {
        self.discs.iter().flat_map(|d| &d.tracks)
    }
}

/// An album with its tracks and sections. Signed out works.
pub async fn album(api: &Api, id: &str) -> Result<AlbumPage, LoadError> {
    let album = api.album(&cancel(), id).await?;
    let a = album.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let r = &album.relationships;
    let artist_id = r
        .artists
        .as_ref()
        .and_then(|p| p.data.first())
        .map(|artist| artist.id.clone());
    let artist_route = artist_id
        .clone()
        .map(|id| Route::Artist { id })
        .or_else(|| map::artist_route(a.artist_url.as_ref()));
    let rows = r
        .tracks
        .as_ref()
        .map(|t| map::tracks(&t.data))
        .unwrap_or_default();
    let discs = discs(rows);
    let v = &album.views;
    let more_by_all = |_: &am::View| {
        artist_id.clone().map(|id| Route::ArtistView {
            id,
            view: ArtistView::FullAlbums,
        })
    };
    let shelves = [
        map::view_shelf(
            "more-by-artist",
            v.more_by_artist.as_ref(),
            false,
            more_by_all,
        ),
        map::view_shelf("other-versions", v.other_versions.as_ref(), false, |_| None),
        map::view_shelf(
            "you-might-also-like",
            v.you_might_also_like.as_ref(),
            false,
            |_| None,
        ),
        map::view_shelf("appears-on", v.appears_on.as_ref(), false, |_| None),
        map::view_shelf("related-videos", v.related_videos.as_ref(), false, |_| None),
    ];
    Ok(AlbumPage {
        id: album.id.clone(),
        header: Header {
            title: a.name.clone(),
            subtitle: Some(Linked {
                text: a.artist_name.clone(),
                route: artist_route,
            }),
            artwork: artwork(a.artwork.as_ref()),
            notes: notes(a.editorial_notes.as_ref()),
            explicit: map::explicit(a.content_rating.as_ref()),
            quality: QualityMarks::new(&a.audio_traits, a.is_apple_digital_master),
        },
        info: AlbumInfo {
            genre: a.genre_names.first().cloned(),
            released: a.release_date.as_deref().and_then(Date::parse),
            record_label: a.record_label.clone().filter(|s| !s.is_empty()),
            copyright: a.copyright.clone().filter(|s| !s.is_empty()),
            single: a.is_single,
            compilation: a.is_compilation,
            prerelease: a.is_prerelease,
        },
        duration: total_duration(discs.iter().flat_map(|d| &d.tracks)),
        discs,
        shelves: shelves.into_iter().flatten().collect(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistPage {
    pub id: String,
    pub header: Header,
    pub updated: Option<Date>,
    pub track_count: Option<u32>,
    /// The first 300 tracks; more with `tracks.more`.
    pub tracks: List<TrackRow>,
    /// Featured artists, more by the curator.
    pub shelves: Vec<Shelf>,
}

/// A catalog playlist with its first 300 tracks. Signed out works.
pub async fn playlist(api: &Api, id: &str) -> Result<PlaylistPage, LoadError> {
    let playlist = api.playlist(&cancel(), id).await?;
    let a = playlist.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let r = &playlist.relationships;
    let curator_route = r
        .curator
        .as_ref()
        .and_then(|c| c.data.first())
        .map(|c| Route::Curator {
            id: c.id().to_owned(),
        });
    let tracks = r.tracks.as_ref();
    let fetched = tracks.map_or(0, |t| t.data.len()) as u32;
    let more = tracks
        .filter(|t| t.next.is_some() && fetched > 0)
        .map(|_| LoadMore::playlist_tracks(&playlist.id, fetched));
    let v = &playlist.views;
    let shelves = [
        map::view_shelf(
            "featured-artists",
            v.featured_artists.as_ref(),
            false,
            |_| None,
        ),
        map::view_shelf("more-by-curator", v.more_by_curator.as_ref(), false, |_| {
            curator_route.clone()
        }),
    ];
    Ok(PlaylistPage {
        id: playlist.id.clone(),
        header: Header {
            title: a.name.clone(),
            subtitle: a
                .curator_name
                .clone()
                .filter(|n| !n.is_empty())
                .map(|text| Linked {
                    text,
                    route: curator_route.clone(),
                }),
            artwork: artwork(a.artwork.as_ref()),
            notes: notes(a.description.as_ref()).or_else(|| notes(a.editorial_notes.as_ref())),
            explicit: false,
            quality: QualityMarks::new(&a.audio_traits, false),
        },
        updated: a.last_modified_date.as_deref().and_then(Date::parse),
        track_count: a.track_count,
        tracks: List {
            items: tracks.map(|t| map::tracks(&t.data)).unwrap_or_default(),
            more,
        },
        shelves: shelves.into_iter().flatten().collect(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtistPage {
    pub id: String,
    pub header: Header,
    pub genre: Option<String>,
    /// The biography, plain text.
    pub bio: Option<String>,
    pub latest_release: Option<Tile>,
    /// Top songs (tracks), albums, singles, music videos, playlists, appears on, similar
    /// artists; each with "see all" when Apple has more.
    pub shelves: Vec<Shelf>,
}

impl ArtistPage {
    /// The top-songs shelf's rows (for play intents).
    pub fn top_songs(&self) -> &[TrackRow] {
        self.shelves
            .iter()
            .find(|s| s.id == ArtistView::TopSongs.as_str())
            .and_then(|s| match &s.items {
                ShelfItems::Tracks(rows) => Some(&rows[..]),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// An artist with every section. Signed out works.
pub async fn artist(api: &Api, id: &str) -> Result<ArtistPage, LoadError> {
    let artist = api.artist(&cancel(), id).await?;
    let a = artist.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let v = &artist.views;
    let see_all = |view: ArtistView| {
        let id = artist.id.clone();
        move |shown: &am::View| {
            shown
                .next
                .is_some()
                .then_some(Route::ArtistView { id, view })
        }
    };
    let section = |view: ArtistView, shown: Option<&am::View>| {
        let tracks = matches!(view, ArtistView::TopSongs);
        map::view_shelf(view.as_str(), shown, tracks, see_all(view))
    };
    let shelves = [
        section(ArtistView::TopSongs, v.top_songs.as_ref()),
        section(ArtistView::FullAlbums, v.full_albums.as_ref()),
        section(ArtistView::Singles, v.singles.as_ref()),
        section(ArtistView::TopMusicVideos, v.top_music_videos.as_ref()),
        section(ArtistView::FeaturedPlaylists, v.featured_playlists.as_ref()),
        section(ArtistView::AppearsOnAlbums, v.appears_on_albums.as_ref()),
        section(ArtistView::SimilarArtists, v.similar_artists.as_ref()),
    ];
    let latest_release = v
        .latest_release
        .as_ref()
        .and_then(|l| l.data.first())
        .and_then(map::tile);
    Ok(ArtistPage {
        id: artist.id.clone(),
        header: Header {
            title: a.name.clone(),
            subtitle: None,
            artwork: artwork(a.artwork.as_ref()),
            notes: notes(a.editorial_notes.as_ref()),
            explicit: false,
            quality: QualityMarks::default(),
        },
        genre: a.genre_names.first().cloned(),
        bio: a.artist_bio.as_deref().map(plain).filter(|b| !b.is_empty()),
        latest_release,
        shelves: shelves.into_iter().flatten().collect(),
    })
}

/// A full list: an artist section, a room.
#[derive(Debug, Clone, PartialEq)]
pub struct ListPage {
    pub title: Option<String>,
    pub listing: Listing,
}

/// One artist section in full, 20 at a time. Signed out works.
pub async fn artist_view(api: &Api, id: &str, view: ArtistView) -> Result<ListPage, LoadError> {
    let shown = api.artist_view(&cancel(), id, view).await?;
    let listing = match view {
        ArtistView::TopSongs | ArtistView::TopMusicVideos => Listing::Tracks(List {
            items: map::tracks(&shown.data),
            more: LoadMore::next(shown.next.clone(), ARTIST_VIEW_PARAMS),
        }),
        _ => Listing::Tiles(List {
            items: map::tiles(&shown.data),
            more: LoadMore::next(shown.next.clone(), ARTIST_VIEW_PARAMS),
        }),
    };
    Ok(ListPage {
        title: shown.title().map(str::to_owned),
        listing,
    })
}

/// A credited person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    pub name: String,
    pub roles: Vec<String>,
}

/// A credits group ("Performing Artists", "Composition & Lyrics", …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditGroup {
    pub title: String,
    pub credits: Vec<Credit>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SongPage {
    pub id: String,
    pub track: TrackRow,
    pub header: Header,
    pub album: Option<Tile>,
    pub released: Option<Date>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub credits: Vec<CreditGroup>,
    pub has_lyrics: bool,
    pub has_synced_lyrics: bool,
    pub lyrics_excerpt: Option<String>,
    pub music_videos: Option<Shelf>,
}

/// A song with its album, credits and videos. Signed out works.
pub async fn song(api: &Api, id: &str) -> Result<SongPage, LoadError> {
    let song = api.song(&cancel(), id).await?;
    let track = map::song_row(&song).ok_or(LoadError::NotFound)?;
    let a = song.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let r = &song.relationships;
    let album = r
        .albums
        .as_ref()
        .and_then(|p| p.data.first())
        .and_then(|al| map::tile(&Resource::Album(Box::new(al.clone()))));
    let credits = r
        .credits
        .as_ref()
        .map(|p| {
            p.data
                .iter()
                .filter_map(|group| {
                    let credits: Vec<Credit> = group
                        .relationships
                        .credit_artists
                        .as_ref()?
                        .data
                        .iter()
                        .filter_map(|c| {
                            let c = c.attributes.as_ref()?;
                            Some(Credit {
                                name: c.name.clone(),
                                roles: c.role_names.clone(),
                            })
                        })
                        .collect();
                    let title = group.attributes.as_ref()?.title.clone()?;
                    (!credits.is_empty()).then_some(CreditGroup { title, credits })
                })
                .collect()
        })
        .unwrap_or_default();
    let videos = r
        .music_videos
        .as_ref()
        .map(|p| {
            p.data
                .iter()
                .filter_map(|v| map::tile(&Resource::MusicVideo(Box::new(v.clone()))))
                .collect()
        })
        .unwrap_or_default();
    Ok(SongPage {
        id: song.id.clone(),
        header: Header {
            title: track.title.clone(),
            subtitle: Some(track.artist.clone()),
            artwork: track.artwork.clone(),
            notes: notes(a.editorial_notes.as_ref()),
            explicit: track.explicit,
            quality: track.quality(),
        },
        album,
        released: a.release_date.as_deref().and_then(Date::parse),
        genre: a.genre_names.first().cloned(),
        composer: a.composer_name.clone().filter(|c| !c.is_empty()),
        credits,
        has_lyrics: a.has_lyrics,
        has_synced_lyrics: a.has_time_synced_lyrics,
        lyrics_excerpt: a.lyrics_excerpt.clone().filter(|l| !l.is_empty()),
        music_videos: map::shelf(
            "music-videos",
            None,
            ShelfItems::Tiles {
                tiles: videos,
                size: TileSize::Regular,
            },
            None,
        ),
        track,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct MusicVideoPage {
    pub id: String,
    pub video: TrackRow,
    pub header: Header,
    pub released: Option<Date>,
    pub genre: Option<String>,
    /// 4K, HDR.
    pub video_traits: Vec<String>,
    /// More by the artist, more in the genre.
    pub shelves: Vec<Shelf>,
}

/// A music video (static page: the video plays through the player). Signed out works.
pub async fn music_video(api: &Api, id: &str) -> Result<MusicVideoPage, LoadError> {
    let mv = api.music_video(&cancel(), id).await?;
    let video = map::video_row(&mv).ok_or(LoadError::NotFound)?;
    let a = mv.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let v = &mv.views;
    let shelves = [
        map::view_shelf("more-by-artist", v.more_by_artist.as_ref(), false, |_| None),
        map::view_shelf("more-in-genre", v.more_in_genre.as_ref(), false, |_| None),
    ];
    let mut video_traits = a.video_traits.clone();
    for (flag, name) in [(a.has_4k, "4K"), (a.has_hdr, "HDR")] {
        if flag && !video_traits.iter().any(|t| t.eq_ignore_ascii_case(name)) {
            video_traits.push(name.to_owned());
        }
    }
    Ok(MusicVideoPage {
        id: mv.id.clone(),
        header: Header {
            title: video.title.clone(),
            subtitle: Some(video.artist.clone()),
            artwork: video.artwork.clone(),
            notes: notes(a.editorial_notes.as_ref()),
            explicit: video.explicit,
            quality: QualityMarks::default(),
        },
        released: a.release_date.as_deref().and_then(Date::parse),
        genre: a.genre_names.first().cloned(),
        video_traits,
        shelves: shelves.into_iter().flatten().collect(),
        video,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct StationPage {
    pub id: String,
    pub header: Header,
    /// A live broadcast (no seeking, no skipping).
    pub live: bool,
    pub requires_subscription: bool,
    /// What Play starts; `None` when Apple gives no play parameters.
    pub play: Option<PlayTarget>,
    /// The radio show the station or episode belongs to.
    pub show: Option<Tile>,
}

/// A station. Signed out works (playing it needs the music user token).
pub async fn station(api: &Api, id: &str) -> Result<StationPage, LoadError> {
    let station = api.station(&cancel(), id).await?;
    let a = station.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let format = a.play_params.as_ref().and_then(|p| p.format.as_deref());
    let live = a.is_live || format == Some("stream");
    let show = station
        .relationships
        .radio_show
        .as_ref()
        .and_then(|p| p.data.first())
        .and_then(map::tile);
    Ok(StationPage {
        id: station.id.clone(),
        header: Header {
            title: a.name.clone(),
            subtitle: a
                .station_provider_name
                .clone()
                .filter(|s| !s.is_empty())
                .map(|text| Linked { text, route: None }),
            artwork: artwork(a.artwork.as_ref())
                .or_else(|| artwork(a.editorial_artwork.values().next())),
            notes: notes(a.editorial_notes.as_ref()),
            explicit: false,
            quality: QualityMarks::default(),
        },
        live,
        requires_subscription: a.requires_subscription,
        play: a.play_params.is_some().then(|| {
            PlayTarget::Station(StationRef {
                id: station.id.clone(),
                name: a.name.clone(),
                live,
            })
        }),
        show,
    })
}

/// The "see all" list of an editorial shelf. Signed out works.
pub async fn room(api: &Api, id: &str) -> Result<ListPage, LoadError> {
    let room = api.room(&cancel(), id).await?;
    let title = room.attributes.as_ref().and_then(|a| a.title.clone());
    let contents = room.relationships.contents.unwrap_or_default();
    let all_tracks = !contents.data.is_empty()
        && contents
            .data
            .iter()
            .all(|r| matches!(r, Resource::Song(_) | Resource::MusicVideo(_)));
    let listing = if all_tracks {
        Listing::Tracks(List {
            items: map::tracks(&contents.data),
            more: LoadMore::next(contents.next, ROOM_PARAMS),
        })
    } else {
        Listing::Tiles(List {
            items: map::tiles(&contents.data),
            more: LoadMore::next(contents.next, ROOM_PARAMS),
        })
    };
    Ok(ListPage { title, listing })
}

#[derive(Debug, Clone, PartialEq)]
pub struct CuratorPage {
    pub id: String,
    pub header: Header,
    /// `Genre`, `Show`, `Curator`, …
    pub kind: Option<String>,
    /// An Apple curator's editorial page.
    pub shelves: Vec<Shelf>,
    pub playlists: List<Tile>,
}

/// An Apple curator or another curator, with its playlists. Signed out works.
pub async fn curator(api: &Api, id: &str) -> Result<CuratorPage, LoadError> {
    let curator = api.curator(&cancel(), id).await?;
    let a = curator.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let r = curator.relationships;
    let shelves = r
        .grouping
        .and_then(|g| g.data.into_iter().next())
        .map(|g| map::editorial_shelves(am::EditorialPage::from(g).shelves))
        .unwrap_or_default();
    let playlists = r.playlists.unwrap_or_default();
    Ok(CuratorPage {
        id: curator.id.clone(),
        header: Header {
            title: a.name.clone(),
            subtitle: a
                .show_host_name
                .clone()
                .filter(|s| !s.is_empty())
                .map(|text| Linked { text, route: None }),
            artwork: artwork(a.artwork.as_ref())
                .or_else(|| artwork(a.editorial_artwork.values().next())),
            notes: notes(a.editorial_notes.as_ref()),
            explicit: false,
            quality: QualityMarks::default(),
        },
        kind: a.kind.clone(),
        shelves,
        playlists: List {
            items: playlists
                .data
                .into_iter()
                .filter_map(|p| map::tile(&Resource::Playlist(Box::new(p))))
                .collect(),
            more: LoadMore::next(playlists.next, &[]),
        },
    })
}
