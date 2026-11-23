//! Signed-out requests: catalog and editorial reads. Every shape here was called against
//! Apple's servers (native-UI spec §2.2) and is cached by the response cache.

use reqwest::Method;
use tokio_util::sync::CancellationToken;

use super::{Api, Host};
use crate::Error;
use crate::model::{
    Album, Artist, Charts, Curator, EditorialPage, Genre, Grouping, MusicVideo, Page, Playlist,
    Resource, Room, SearchResults, Shelf, Song, Station, Storefront, Suggestion, View,
};
use crate::model::{ChartsResponse, SuggestionsResponse};

/// Album fields the editorial pages need (as the web player asks for them).
const EDITORIAL_ALBUM_FIELDS: &str = "artistName,artistUrl,artwork,contentRating,editorialArtwork,\
                                      plainEditorialNotes,name,playParams,releaseDate,url,trackCount";
/// Types the search page shows.
const SEARCH_TYPES: &str =
    "albums,artists,songs,playlists,music-videos,stations,apple-curators,curators,record-labels";
/// What the search page asks for besides the term, types and language. Apple's `next`
/// links leave these out and then answer with no results (verified), so
/// [`Api::search_next`] adds them back.
const SEARCH_PARAMS: [(&str, &str); 6] = [
    ("limit", "21"),
    ("platform", "web"),
    (
        "with",
        "lyricHighlights,lyrics,naturalLanguage,serverBubbles,subtitles",
    ),
    ("include[songs]", "artists"),
    ("relate[songs]", "albums"),
    ("omit[resource]", "autos"),
];
/// Types search hints may name.
const SUGGESTION_TYPES: &str = "albums,artists,songs,playlists,music-videos,stations";
/// Every artist section, in the order Rustet's artist page shows them.
const ARTIST_VIEWS: &str = "latest-release,top-songs,full-albums,singles,top-music-videos,\
                            featured-playlists,appears-on-albums,similar-artists";
/// Tracks returned with a playlist; the rest is paged ([`Api::playlist_tracks`]).
const PLAYLIST_INLINE_TRACKS: &str = "300";
/// Apple's largest page for playlist tracks.
const PLAYLIST_TRACKS_PAGE: u32 = 100;

/// The editorial pages of the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowsePage {
    /// "New" (`groupings?name=music`).
    New,
    /// "Radio" (`groupings?name=radio`).
    Radio,
}

/// A section of the artist page, also fetched on its own for "see all".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtistView {
    LatestRelease,
    TopSongs,
    FullAlbums,
    Singles,
    TopMusicVideos,
    FeaturedPlaylists,
    AppearsOnAlbums,
    SimilarArtists,
}

impl ArtistView {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LatestRelease => "latest-release",
            Self::TopSongs => "top-songs",
            Self::FullAlbums => "full-albums",
            Self::Singles => "singles",
            Self::TopMusicVideos => "top-music-videos",
            Self::FeaturedPlaylists => "featured-playlists",
            Self::AppearsOnAlbums => "appears-on-albums",
            Self::SimilarArtists => "similar-artists",
        }
    }
}

/// The first resource of a `{data: [...]}` answer.
fn first<T>(page: Page<T>) -> Result<T, Error> {
    page.data.into_iter().next().ok_or(Error::NotFound)
}

impl Api {
    /// The New or Radio page. Signed in, it asks for the subscriber tab.
    pub async fn browse(
        &self,
        cancel: &CancellationToken,
        page: BrowsePage,
    ) -> Result<EditorialPage, Error> {
        let locale = self.locale();
        let tabs = if self.is_signed_in() {
            "subscriber"
        } else {
            "nonsubscriber"
        };
        let common = [
            ("tabs", tabs),
            ("platform", "web"),
            ("l", &locale.language),
            ("extend", "artistUrl,editorialArtwork,plainEditorialNotes"),
            ("include[stations]", "events,radio-show"),
            ("omit[resource:artists]", "autos"),
        ];
        let mut query = Vec::from(common);
        match page {
            BrowsePage::New => query.extend([
                ("name", "music"),
                ("include[albums]", "artists"),
                ("include[songs]", "artists"),
                ("relate[songs]", "albums"),
                ("fields[albums]", EDITORIAL_ALBUM_FIELDS),
            ]),
            BrowsePage::Radio => query.push(("name", "radio")),
        }
        let segments = ["v1", "editorial", &locale.storefront, "groupings"];
        let call = self.call(Method::GET, Host::Edge, &segments, &query)?;
        let grouping: Page<Grouping> = self.fetch(cancel, call).await?;
        Ok(first(grouping)?.into())
    }

    /// Apple's live radio stations (Apple Music 1, …).
    pub async fn live_stations(&self, cancel: &CancellationToken) -> Result<Vec<Station>, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "stations"];
        let query = [
            ("filter[featured]", "apple-music-live-radio"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        Ok(self.fetch::<Page<Station>>(cancel, call).await?.data)
    }

    /// The search page before a term is typed: category tiles.
    pub async fn search_landing(&self, cancel: &CancellationToken) -> Result<Vec<Shelf>, Error> {
        let locale = self.locale();
        let segments = ["v1", "recommendations", &locale.storefront];
        let query = [
            ("name", "search-landing"),
            ("platform", "web"),
            ("l", &locale.language),
            ("types", "activities,apple-curators,editorial-items"),
            ("extend", "editorialArtwork"),
            ("omit[resource]", "autos"),
        ];
        let call = self.call(Method::GET, Host::Edge, &segments, &query)?;
        let page: Page<Resource> = self.fetch(cancel, call).await?;
        Ok(recommendation_shelves(page.data))
    }

    /// Hints while the user types: completed terms and top results.
    pub async fn search_suggestions(
        &self,
        cancel: &CancellationToken,
        term: &str,
    ) -> Result<Vec<Suggestion>, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "search", "suggestions"];
        let query = [
            ("term", term),
            ("kinds", "terms,topResults"),
            ("types", SUGGESTION_TYPES),
            ("limit[results:terms]", "5"),
            ("limit[results:topResults]", "10"),
            ("platform", "web"),
            ("l", &locale.language),
            ("omit[resource]", "autos"),
            ("with", "naturalLanguage"),
        ];
        let call = self.call(Method::GET, Host::Edge, &segments, &query)?;
        let response: SuggestionsResponse = self.fetch(cancel, call).await?;
        Ok(response.results.suggestions)
    }

    /// Search results grouped as the web player shows them (`with=…` shape: the group
    /// keys are singular, `song`, `music_video`, and `top` holds the top results).
    pub async fn search(
        &self,
        cancel: &CancellationToken,
        term: &str,
    ) -> Result<SearchResults, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "search"];
        let mut query = vec![
            ("term", term),
            ("types", SEARCH_TYPES),
            ("l", &locale.language),
        ];
        query.extend(SEARCH_PARAMS);
        let call = self.call(Method::GET, Host::Edge, &segments, &query)?;
        self.fetch(cancel, call).await
    }

    /// More results of one search group ([`SearchGroup::next`](crate::model::SearchGroup)).
    pub async fn search_next(
        &self,
        cancel: &CancellationToken,
        cursor: &crate::model::Cursor,
    ) -> Result<SearchResults, Error> {
        let mut call = self.cursor_call(Host::Edge, cursor)?;
        let present: Vec<String> = call
            .url
            .query_pairs()
            .map(|(k, _)| k.into_owned())
            .collect();
        let missing = SEARCH_PARAMS
            .iter()
            .filter(|(k, _)| !present.iter().any(|p| p == k));
        call.url.query_pairs_mut().extend_pairs(missing);
        self.fetch(cancel, call).await
    }

    /// An artist with every section of [`ArtistView`] in `views`, and the biography.
    pub async fn artist(&self, cancel: &CancellationToken, id: &str) -> Result<Artist, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "artists", id];
        let query = [
            ("views", ARTIST_VIEWS),
            (
                "extend",
                "artistBio,editorialArtwork,editorialVideo,plainEditorialNotes",
            ),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// One artist section on its own ("see all"); more with [`Api::next_page`] on
    /// `View::next`.
    pub async fn artist_view(
        &self,
        cancel: &CancellationToken,
        id: &str,
        view: ArtistView,
    ) -> Result<View, Error> {
        let locale = self.locale();
        let segments = [
            "v1",
            "catalog",
            &locale.storefront,
            "artists",
            id,
            "view",
            view.as_str(),
        ];
        let query = [
            ("limit", "20"),
            ("with", "attributes"),
            ("include[albums]", "artists"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        self.fetch(cancel, call).await
    }

    /// An album with its tracks (and their popularity), artists, label and the
    /// "more by", "other versions", "you might also like" sections.
    pub async fn album(&self, cancel: &CancellationToken, id: &str) -> Result<Album, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "albums", id];
        let query = [
            (
                "views",
                "appears-on,more-by-artist,other-versions,related-videos,you-might-also-like",
            ),
            ("include", "record-labels,artists"),
            ("include[songs]", "artists,composers,albums"),
            (
                "extend",
                "editorialArtwork,editorialVideo,extendedAssetUrls,offers",
            ),
            ("meta[albums:tracks]", "popularity"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// A catalog playlist with its first 300 tracks, curator and sections. Later tracks:
    /// [`Api::playlist_tracks`] or [`Api::next_page`] on the tracks' `next`.
    pub async fn playlist(&self, cancel: &CancellationToken, id: &str) -> Result<Playlist, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "playlists", id];
        let query = [
            ("include", "tracks,curator"),
            ("include[songs]", "artists"),
            ("limit[tracks]", PLAYLIST_INLINE_TRACKS),
            ("views", "featured-artists,more-by-curator"),
            ("extend", "editorialArtwork,editorialVideo,trackCount"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// Up to 100 tracks of a catalog playlist from `offset`. An offset past the end is
    /// [`Error::NotFound`] (Apple answers 404).
    pub async fn playlist_tracks(
        &self,
        cancel: &CancellationToken,
        id: &str,
        offset: u32,
    ) -> Result<Page<Resource>, Error> {
        let locale = self.locale();
        let segments = [
            "v1",
            "catalog",
            &locale.storefront,
            "playlists",
            id,
            "tracks",
        ];
        let offset = offset.to_string();
        let limit = PLAYLIST_TRACKS_PAGE.to_string();
        let query = [
            ("offset", offset.as_str()),
            ("limit", &limit),
            ("include[songs]", "artists"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        self.fetch(cancel, call).await
    }

    /// A song with its album, artists, credits, music videos and a lyrics excerpt.
    pub async fn song(&self, cancel: &CancellationToken, id: &str) -> Result<Song, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "songs", id];
        let query = [
            ("include", "albums,artists,credits,music-videos"),
            ("extend", "lyricsExcerpt,extendedAssetUrls,offers"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// A music video with its artists and the "more by artist" / "more in genre" sections.
    pub async fn music_video(
        &self,
        cancel: &CancellationToken,
        id: &str,
    ) -> Result<MusicVideo, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "music-videos", id];
        let query = [
            ("include[music-videos]", "artists"),
            ("views", "more-by-artist,more-in-genre"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// A station (or radio episode) with its show.
    pub async fn station(&self, cancel: &CancellationToken, id: &str) -> Result<Station, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "stations", id];
        let query = [
            ("include", "radio-show"),
            ("extend", "editorialArtwork"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// The "see all" list of an editorial shelf ([`Shelf::Grid`]'s `room`).
    pub async fn room(&self, cancel: &CancellationToken, id: &str) -> Result<Room, Error> {
        let locale = self.locale();
        let segments = ["v1", "editorial", &locale.storefront, "rooms", id];
        let query = [
            ("include[albums]", "artists"),
            ("include[songs]", "artists"),
            ("relate[songs]", "albums"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Edge, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// A curator (Apple's or another) with its playlists and, for Apple curators, its
    /// editorial page (`relationships.grouping`, into [`EditorialPage`]).
    pub async fn curator(&self, cancel: &CancellationToken, id: &str) -> Result<Curator, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront];
        let query = [
            ("ids[apple-curators]", id),
            ("ids[curators]", id),
            ("include", "grouping,playlists"),
            ("extend", "editorialArtwork"),
            ("extend[apple-curators]", "playlistCount"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        if !super::valid_segment(id) {
            return Err(Error::NotFound);
        }
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        first(self.fetch(cancel, call).await?)
    }

    /// Top songs, albums, playlists and videos, city charts and daily top 100, for a genre
    /// (`None`: all music).
    pub async fn charts(
        &self,
        cancel: &CancellationToken,
        genre: Option<&str>,
    ) -> Result<Charts, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "charts"];
        let mut query = vec![
            ("types", "albums,songs,music-videos,playlists"),
            ("limit", "50"),
            ("include[songs]", "artists"),
            ("with", "cityCharts,dailyGlobalTopCharts"),
            ("extend", "artistUrl"),
            ("l", &locale.language),
        ];
        query.extend(genre.map(|g| ("genre", g)));
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        Ok(self.fetch::<ChartsResponse>(cancel, call).await?.results)
    }

    /// The storefront's top-level genres.
    pub async fn genres(&self, cancel: &CancellationToken) -> Result<Vec<Genre>, Error> {
        let locale = self.locale();
        let segments = ["v1", "catalog", &locale.storefront, "genres"];
        let call = self.call(
            Method::GET,
            Host::Amp,
            &segments,
            &[("l", &locale.language)],
        )?;
        Ok(self.fetch::<Page<Genre>>(cancel, call).await?.data)
    }

    /// One storefront with its languages.
    pub async fn storefront(
        &self,
        cancel: &CancellationToken,
        id: &str,
    ) -> Result<Storefront, Error> {
        let call = self.call(Method::GET, Host::Amp, &["v1", "storefronts", id], &[])?;
        first(self.fetch(cancel, call).await?)
    }

    /// Every storefront (for the storefront picker and FairPlay storefront mapping).
    pub async fn storefronts(&self, cancel: &CancellationToken) -> Result<Vec<Storefront>, Error> {
        let call = self.call(Method::GET, Host::Amp, &["v1", "storefronts"], &[])?;
        Ok(self.fetch::<Page<Storefront>>(cancel, call).await?.data)
    }
}

/// Recommendation and editorial resources as shelves; other resources are skipped.
pub(crate) fn recommendation_shelves(resources: Vec<Resource>) -> Vec<Shelf> {
    resources
        .into_iter()
        .filter_map(|r| match r {
            Resource::Recommendation(r) => Some(Shelf::from(*r)),
            Resource::EditorialElement(e) => Some(Shelf::from(*e)),
            _ => None,
        })
        .collect()
}
