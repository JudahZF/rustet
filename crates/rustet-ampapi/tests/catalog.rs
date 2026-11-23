//! Signed-out requests against a mock server, decoding responses recorded from Apple
//! (2026-11-19, trimmed: lists cut to three items).

mod common;

use std::time::Duration;

use common::{EDGE, env, expect, fixture, json, query, signed_in};
use futures::TryStreamExt;
use rustet_ampapi::model::{DisplayStyle, Resource, Shelf, Suggestion};
use rustet_ampapi::{ArtistView, BrowsePage, Error, ErrorKind};
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

const NEW_ALBUM_FIELDS: &str = "artistName,artistUrl,artwork,contentRating,editorialArtwork,\
                                plainEditorialNotes,name,playParams,releaseDate,url,trackCount";

#[tokio::test]
async fn browse_new_reads_every_shelf_kind() {
    let env = env().await;
    expect(
        "GET",
        &format!("{EDGE}/v1/editorial/us/groupings"),
        query(&[
            ("name", "music"),
            ("tabs", "nonsubscriber"),
            ("platform", "web"),
            ("l", "en-US"),
            ("extend", "artistUrl,editorialArtwork,plainEditorialNotes"),
            ("include[albums]", "artists"),
            ("include[songs]", "artists"),
            ("include[stations]", "events,radio-show"),
            ("relate[songs]", "albums"),
            ("omit[resource:artists]", "autos"),
            ("fields[albums]", NEW_ALBUM_FIELDS),
        ]),
        false,
    )
    .respond_with(json(fixture("new.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let page = env
        .api
        .browse(&CancellationToken::new(), BrowsePage::New)
        .await
        .unwrap();
    assert_eq!(page.title, "Music");
    let [hero, songs, grid, group, expanded, links, genres] = &page.shelves[..] else {
        panic!("seven shelves, got {:#?}", page.shelves)
    };
    let Shelf::Featured { tiles, .. } = hero else {
        panic!("hero")
    };
    assert_eq!(tiles.len(), 3);
    assert_eq!(tiles[0].badge.as_deref(), Some("NEW ALBUM"));
    let Resource::Album(album) = &tiles[0].item else {
        panic!("album tile")
    };
    let art = album.attributes.as_ref().unwrap().artwork.as_ref().unwrap();
    assert!(art.url(400, 400).ends_with("/400x400bb.jpg"), "{}", art.url);

    assert!(
        matches!(songs, Shelf::Tracks { title: Some(t), items, room: Some(r), .. }
        if t == "Best New Songs" && items.len() == 3 && r == "6834267408"
            && matches!(items[0], Resource::Song(_)))
    );
    assert!(
        matches!(grid, Shelf::Grid { title: Some(t), style: DisplayStyle::Compact, .. }
        if t == "New This Week")
    );
    assert!(
        matches!(group, Shelf::Featured { title: Some(t), tiles, .. }
        if t == "Nonstop Holiday Hits" && tiles.len() == 3
            && matches!(tiles[0].item, Resource::Station(_)))
    );
    assert!(matches!(
        expanded,
        Shelf::Grid {
            style: DisplayStyle::Expanded,
            ..
        }
    ));
    assert!(matches!(links, Shelf::Links { title: Some(t), links, .. }
        if t == "More to Explore" && links[0].label == "Concerts"));
    assert!(matches!(genres, Shelf::Links { title: None, links, .. } if links.len() == 3));
}

#[tokio::test]
async fn browse_radio_signed_in_asks_for_the_subscriber_tab() {
    let env = signed_in().await;
    expect(
        "GET",
        &format!("{EDGE}/v1/editorial/us/groupings"),
        query(&[
            ("name", "radio"),
            ("tabs", "subscriber"),
            ("platform", "web"),
            ("l", "en-US"),
            ("extend", "artistUrl,editorialArtwork,plainEditorialNotes"),
            ("include[stations]", "events,radio-show"),
            ("omit[resource:artists]", "autos"),
        ]),
        // Catalog reads never send the user token.
        false,
    )
    .respond_with(json(fixture("radio.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let page = env
        .api
        .browse(&CancellationToken::new(), BrowsePage::Radio)
        .await
        .unwrap();
    let kinds: Vec<_> = page
        .shelves
        .iter()
        .map(|s| match s {
            Shelf::Featured { tiles, .. } => format!("featured:{}", tiles.len()),
            Shelf::Grid { style, .. } => format!("grid:{style:?}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        ["featured:3", "featured:3", "grid:Compact", "grid:Expanded"]
    );
    assert_eq!(
        page.shelves[1].title(),
        Some("Listen to Live Radio for Free")
    );
}

#[tokio::test]
async fn live_stations() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/stations",
        query(&[
            ("filter[featured]", "apple-music-live-radio"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("live_st.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let stations = env
        .api
        .live_stations(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(stations.len(), 3);
    let a = stations[0].attributes.as_ref().unwrap();
    assert!(a.is_live);
    assert_eq!(a.name, "Holiday Hits");
    assert_eq!(
        a.play_params.as_ref().unwrap().format.as_deref(),
        Some("stream")
    );
}

#[tokio::test]
async fn search_landing_shelves() {
    let env = env().await;
    expect(
        "GET",
        &format!("{EDGE}/v1/recommendations/us"),
        query(&[
            ("name", "search-landing"),
            ("platform", "web"),
            ("l", "en-US"),
            ("types", "activities,apple-curators,editorial-items"),
            ("extend", "editorialArtwork"),
            ("omit[resource]", "autos"),
        ]),
        false,
    )
    .respond_with(json(fixture("landing.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let shelves = env
        .api
        .search_landing(&CancellationToken::new())
        .await
        .unwrap();
    let [categories, tiles] = &shelves[..] else {
        panic!("two shelves")
    };
    assert!(
        matches!(categories, Shelf::Grid { title: Some(t), style: DisplayStyle::Other(k), items, .. }
        if t == "Browse Categories" && k == "MusicNamedBrickShelf"
            && matches!(&items[0], Resource::AppleCurator(c) if c.attributes.as_ref().unwrap().name == "Apple Music Radio"))
    );
    assert!(matches!(tiles, Shelf::Grid { title: None, items, .. } if items.len() == 3));
}

#[tokio::test]
async fn search_suggestions() {
    let env = env().await;
    expect(
        "GET",
        &format!("{EDGE}/v1/catalog/us/search/suggestions"),
        query(&[
            ("term", "phoebe"),
            ("kinds", "terms,topResults"),
            (
                "types",
                "albums,artists,songs,playlists,music-videos,stations",
            ),
            ("limit[results:terms]", "5"),
            ("limit[results:topResults]", "10"),
            ("platform", "web"),
            ("l", "en-US"),
            ("omit[resource]", "autos"),
            ("with", "naturalLanguage"),
        ]),
        false,
    )
    .respond_with(json(fixture("suggest.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let hints = env
        .api
        .search_suggestions(&CancellationToken::new(), "phoebe")
        .await
        .unwrap();
    let [term, _, artist, song] = &hints[..] else {
        panic!("four hints")
    };
    assert!(
        matches!(term, Suggestion::Term { search_term, .. } if search_term == "phoebe bridgers")
    );
    assert!(
        matches!(artist, Suggestion::TopResult { content: Resource::Artist(a) }
        if a.id == "697833299")
    );
    assert!(matches!(song, Suggestion::TopResult { content } if content.name() == Some("Phoebe")));
}

#[tokio::test]
async fn search_and_its_next_page() {
    let env = env().await;
    expect(
        "GET",
        &format!("{EDGE}/v1/catalog/us/search"),
        query(&[
            ("term", "radiohead"),
            (
                "types",
                "albums,artists,songs,playlists,music-videos,stations,apple-curators,curators,record-labels",
            ),
            ("limit", "21"),
            ("platform", "web"),
            ("l", "en-US"),
            (
                "with",
                "lyricHighlights,lyrics,naturalLanguage,serverBubbles,subtitles",
            ),
            ("include[songs]", "artists"),
            ("relate[songs]", "albums"),
            ("omit[resource]", "autos"),
        ]),
        false,
    )
    .respond_with(json(fixture("search.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let cancel = CancellationToken::new();
    let results = env.api.search(&cancel, "radiohead").await.unwrap();
    let order: Vec<_> = results.groups.iter().map(|g| g.group_id.as_str()).collect();
    assert_eq!(
        order,
        [
            "top",
            "artist",
            "album",
            "song",
            "playlist",
            "radio_episode",
            "station",
            "music_video"
        ]
    );
    let top = results.group("top").unwrap();
    assert_eq!(top.name.as_deref(), Some("Top Results"));
    assert!(
        matches!(&top.data[0], Resource::Artist(a) if a.attributes.as_ref().unwrap().name == "Radiohead")
    );
    let songs = results.group("song").unwrap();
    assert_eq!(songs.data[0].name(), Some("Creep"));

    // The group's `next` goes back to the edge host with Apple's own query.
    expect(
        "GET",
        &format!("{EDGE}/v1/catalog/us/search"),
        query(&[
            ("groups", "song"),
            ("l", "en-US"),
            ("offset", "21"),
            ("term", "radiohead"),
            (
                "types",
                "albums,artists,songs,playlists,music-videos,stations,apple-curators,curators,record-labels",
            ),
            // Apple's `next` omits these and then returns nothing.
            ("limit", "21"),
            ("platform", "web"),
            (
                "with",
                "lyricHighlights,lyrics,naturalLanguage,serverBubbles,subtitles",
            ),
            ("include[songs]", "artists"),
            ("relate[songs]", "albums"),
            ("omit[resource]", "autos"),
        ]),
        false,
    )
    .respond_with(json(
        r#"{"results":{"song":{"groupId":"song","data":[{"id":"9","type":"songs"}]}}}"#.into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;
    let more = env
        .api
        .search_next(&cancel, songs.next.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(more.group("song").unwrap().data[0].id(), "9");
}

#[tokio::test]
async fn artist_with_views() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/artists/697833299",
        query(&[
            (
                "views",
                "latest-release,top-songs,full-albums,singles,top-music-videos,featured-playlists,appears-on-albums,similar-artists",
            ),
            (
                "extend",
                "artistBio,editorialArtwork,editorialVideo,plainEditorialNotes",
            ),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("artist_v.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let artist = env
        .api
        .artist(&CancellationToken::new(), "697833299")
        .await
        .unwrap();
    assert_eq!(artist.attributes.as_ref().unwrap().name, "Phoebe Bridgers");
    let v = &artist.views;
    let top = v.top_songs.as_ref().unwrap();
    assert_eq!(top.title(), Some("Top Songs"));
    assert!(matches!(top.data[0], Resource::Song(_)));
    assert!(top.next.is_some());
    assert_eq!(v.latest_release.as_ref().unwrap().data.len(), 1);
    assert_eq!(
        v.similar_artists.as_ref().unwrap().title(),
        Some("Similar Artists")
    );
    assert_eq!(
        artist.meta.views.unwrap().order[..3],
        ["latest-release", "top-songs", "full-albums"]
    );
}

#[tokio::test]
async fn artist_see_all_view() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/artists/697833299/view/full-albums",
        query(&[
            ("limit", "20"),
            ("with", "attributes"),
            ("include[albums]", "artists"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("artist_all.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let view = env
        .api
        .artist_view(
            &CancellationToken::new(),
            "697833299",
            ArtistView::FullAlbums,
        )
        .await
        .unwrap();
    assert_eq!(view.title(), Some("Albums"));
    assert_eq!(view.data.len(), 3);
    assert_eq!(view.data[0].name(), Some("Anything"));
}

#[tokio::test]
async fn album_with_tracks_popularity_and_views() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/albums/1889568617",
        query(&[
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
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("album.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let album = env
        .api
        .album(&CancellationToken::new(), "1889568617")
        .await
        .unwrap();
    let a = album.attributes.as_ref().unwrap();
    assert_eq!(
        (a.name.as_str(), a.artist_name.as_str()),
        ("WARM UP REMIXES", "Etienne de Crécy")
    );
    assert_eq!(
        a.audio_traits,
        [
            rustet_ampapi::model::AudioTrait::Lossless,
            rustet_ampapi::model::AudioTrait::LossyStereo
        ]
    );
    // `editorialNotes: null` reads as absent.
    assert!(a.editorial_notes.is_none());
    let tracks = &album.relationships.tracks.as_ref().unwrap().data;
    let Resource::Song(first) = &tracks[0] else {
        panic!("song track")
    };
    assert!(first.meta.popularity.unwrap() > 0.7);
    assert_eq!(
        album.views.more_by_artist.as_ref().unwrap().title(),
        Some("More By Etienne de Crécy")
    );
    assert!(album.views.other_versions.as_ref().unwrap().data.is_empty());
}

#[tokio::test]
async fn playlist_tracks_and_paging() {
    let env = env().await;
    let id = "pl.f083772167b24f5a9d691e3c3e16f968";
    expect(
        "GET",
        &format!("/v1/catalog/us/playlists/{id}"),
        query(&[
            ("include", "tracks,curator"),
            ("include[songs]", "artists"),
            ("limit[tracks]", "300"),
            ("views", "featured-artists,more-by-curator"),
            ("extend", "editorialArtwork,editorialVideo,trackCount"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("playlist.json")))
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "GET",
        &format!("/v1/catalog/us/playlists/{id}/tracks"),
        query(&[
            ("offset", "100"),
            ("limit", "100"),
            ("include[songs]", "artists"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("pl_tracks.json")))
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "GET",
        &format!("/v1/catalog/us/playlists/{id}/tracks"),
        query(&[("l", "en-US"), ("offset", "20")]),
        false,
    )
    .respond_with(json(
        r#"{"data":[{"id":"7","type":"music-videos"}]}"#.into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;

    let cancel = CancellationToken::new();
    let playlist = env.api.playlist(&cancel, id).await.unwrap();
    let a = playlist.attributes.as_ref().unwrap();
    assert_eq!(a.name, "Top 25: Chicago");
    assert_eq!(a.curator_name.as_deref(), Some("Apple Music"));
    assert_eq!(a.track_count, Some(25));
    assert!(a.play_params.as_ref().unwrap().version_hash.is_some());
    assert_eq!(
        playlist.relationships.tracks.as_ref().unwrap().data.len(),
        3
    );

    let page = env.api.playlist_tracks(&cancel, id, 100).await.unwrap();
    assert_eq!(page.data[0].name(), Some("In A Minute"));
    let pages: Vec<Vec<Resource>> = env.api.pages(&cancel, page).try_collect().await.unwrap();
    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [3, 1]);
    assert!(matches!(pages[1][0], Resource::MusicVideo(_)));
}

#[tokio::test]
async fn song_with_credits() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/songs/6781051513",
        query(&[
            ("include", "albums,artists,credits,music-videos"),
            ("extend", "lyricsExcerpt,extendedAssetUrls,offers"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("song.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let song = env
        .api
        .song(&CancellationToken::new(), "6781051513")
        .await
        .unwrap();
    let a = song.attributes.as_ref().unwrap();
    assert_eq!(a.name, "Still Standing");
    assert!(
        a.lyrics_excerpt
            .as_deref()
            .unwrap()
            .starts_with("Pushed headfirst")
    );
    assert!(
        a.extended_asset_urls
            .as_ref()
            .unwrap()
            .enhanced_hls
            .is_some()
    );
    let credits = &song.relationships.credits.as_ref().unwrap().data;
    let titles: Vec<_> = credits
        .iter()
        .map(|c| c.attributes.as_ref().unwrap().title.as_deref().unwrap())
        .collect();
    assert_eq!(
        titles,
        [
            "PERFORMING ARTISTS",
            "COMPOSITION & LYRICS",
            "PRODUCTION & ENGINEERING"
        ]
    );
    let artists = &credits[0]
        .relationships
        .credit_artists
        .as_ref()
        .unwrap()
        .data;
    let first = artists[0].attributes.as_ref().unwrap();
    assert_eq!(first.name, "Phoebe Bridgers");
    assert_eq!(first.role_names[0], "Vocals");
    let album = &song.relationships.albums.as_ref().unwrap().data[0];
    assert_eq!(album.attributes.as_ref().unwrap().name, "Lost Weekend");
}

#[tokio::test]
async fn music_video() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/music-videos/6834539294",
        query(&[
            ("include[music-videos]", "artists"),
            ("views", "more-by-artist,more-in-genre"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("mv.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let mv = env
        .api
        .music_video(&CancellationToken::new(), "6834539294")
        .await
        .unwrap();
    let a = mv.attributes.as_ref().unwrap();
    assert_eq!(a.name, "Garden");
    assert!(a.has_4k && !a.has_hdr);
    assert_eq!(
        mv.views.more_by_artist.as_ref().unwrap().title(),
        Some("More By Shenseea")
    );
    assert_eq!(mv.views.more_in_genre.as_ref().unwrap().data.len(), 3);
}

#[tokio::test]
async fn station_with_show() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/stations/ra.6833682596",
        query(&[
            ("include", "radio-show"),
            ("extend", "editorialArtwork"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("station.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let station = env
        .api
        .station(&CancellationToken::new(), "ra.6833682596")
        .await
        .unwrap();
    let a = station.attributes.as_ref().unwrap();
    let play = a.play_params.as_ref().unwrap();
    assert_eq!(play.kind, "radioStation");
    assert_eq!(play.station_hash.as_deref(), Some("CgkIBRoFpPHGuhkQBA"));
    assert_eq!(
        a.air_time.as_ref().unwrap().end.as_deref(),
        Some("2026-11-17T18:00:00Z")
    );
    let show = &station.relationships.radio_show.as_ref().unwrap().data[0];
    assert!(matches!(show, Resource::AppleCurator(c) if c.id == "990050553"));
}

#[tokio::test]
async fn room() {
    let env = env().await;
    expect(
        "GET",
        &format!("{EDGE}/v1/editorial/us/rooms/6834266064"),
        query(&[
            ("include[albums]", "artists"),
            ("include[songs]", "artists"),
            ("relate[songs]", "albums"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("room.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let room = env
        .api
        .room(&CancellationToken::new(), "6834266064")
        .await
        .unwrap();
    let a = room.attributes.as_ref().unwrap();
    assert_eq!(a.title.as_deref(), Some("Everyone’s Listening To..."));
    assert_eq!(a.sorts, ["alphabet", "releaseDate", "featured"]);
    let items = &room.relationships.contents.as_ref().unwrap().data;
    assert!(items.iter().all(|i| matches!(i, Resource::Album(_))));
}

#[tokio::test]
async fn curator_with_editorial_page() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us",
        query(&[
            ("ids[apple-curators]", "976439548"),
            ("ids[curators]", "976439548"),
            ("include", "grouping,playlists"),
            ("extend", "editorialArtwork"),
            ("extend[apple-curators]", "playlistCount"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("curator.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let curator = env
        .api
        .curator(&CancellationToken::new(), "976439548")
        .await
        .unwrap();
    let a = curator.attributes.as_ref().unwrap();
    assert_eq!(
        (a.name.as_str(), a.playlist_count),
        ("Apple Music Pop", Some(4612))
    );
    let playlists = curator.relationships.playlists.as_ref().unwrap();
    assert_eq!(playlists.data.len(), 3);
    assert!(playlists.next.is_some());
    let grouping = curator.relationships.grouping.unwrap().data.remove(0);
    let page = rustet_ampapi::model::EditorialPage::from(grouping);
    assert_eq!(page.shelves.len(), 3);
}

#[tokio::test]
async fn charts_for_a_genre() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/charts",
        query(&[
            ("types", "albums,songs,music-videos,playlists"),
            ("limit", "50"),
            ("genre", "34"),
            ("include[songs]", "artists"),
            ("with", "cityCharts,dailyGlobalTopCharts"),
            ("extend", "artistUrl"),
            ("l", "en-US"),
        ]),
        false,
    )
    .respond_with(json(fixture("charts.json")))
    .expect(1)
    .mount(&env.server)
    .await;

    let charts = env
        .api
        .charts(&CancellationToken::new(), Some("34"))
        .await
        .unwrap();
    assert_eq!(charts.songs[0].name, "Top Songs");
    assert!(matches!(charts.songs[0].data[0], Resource::Song(_)));
    assert!(charts.songs[0].next.is_some());
    assert!(matches!(
        charts.music_videos[0].data[0],
        Resource::MusicVideo(_)
    ));
    assert_eq!(charts.city_charts[0].name, "City Charts");
    assert_eq!(charts.daily_global_top_charts[0].name, "Daily Top 100");
}

#[tokio::test]
async fn genres_and_storefronts() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/genres",
        query(&[("l", "en-US")]),
        false,
    )
    .respond_with(json(fixture("genres.json")))
    .expect(1)
    .mount(&env.server)
    .await;
    expect("GET", "/v1/storefronts/gb", query(&[]), false)
        .respond_with(json(fixture("storefront.json")))
        .expect(1)
        .mount(&env.server)
        .await;
    expect("GET", "/v1/storefronts", query(&[]), false)
        .respond_with(json(fixture("storefront.json")))
        .expect(1)
        .mount(&env.server)
        .await;

    let cancel = CancellationToken::new();
    let genres = env.api.genres(&cancel).await.unwrap();
    assert_eq!(genres[1].attributes.as_ref().unwrap().name, "Alternative");
    let gb = env.api.storefront(&cancel, "gb").await.unwrap();
    let a = gb.attributes.as_ref().unwrap();
    assert_eq!(a.default_language_tag, "en-GB");
    let locale = rustet_ampapi::Locale::for_storefront(&gb, "fr-FR");
    assert_eq!(locale, rustet_ampapi::Locale::new("gb", "en-GB"));
    assert_eq!(env.api.storefronts(&cancel).await.unwrap().len(), 1);
}

#[tokio::test]
async fn going_back_to_a_page_does_not_hit_the_network() {
    let env = env().await;
    expect(
        "GET",
        "/v1/catalog/us/genres",
        query(&[("l", "en-US")]),
        false,
    )
    .respond_with(json(fixture("genres.json")))
    .expect(1)
    .mount(&env.server)
    .await;
    let cancel = CancellationToken::new();
    let first = env.api.genres(&cancel).await.unwrap();
    let again = env.api.genres(&cancel).await.unwrap();
    assert_eq!(first.len(), again.len());

    // Another language is another page.
    env.api
        .set_locale(rustet_ampapi::Locale::new("us", "es-MX"));
    expect(
        "GET",
        "/v1/catalog/us/genres",
        query(&[("l", "es-MX")]),
        false,
    )
    .respond_with(json(fixture("genres.json")))
    .expect(1)
    .mount(&env.server)
    .await;
    env.api.genres(&cancel).await.unwrap();
}

#[tokio::test]
async fn a_stale_answer_is_revalidated_with_its_etag() {
    let env = env().await;
    let stale = ResponseTemplate::new(200)
        .insert_header("cache-control", "max-age=0")
        .insert_header("etag", "\"v1\"")
        .set_body_string(fixture("genres.json"));
    wiremock::Mock::given(wiremock::matchers::path("/v1/catalog/us/genres"))
        .and(|r: &wiremock::Request| !r.headers.contains_key("if-none-match"))
        .respond_with(stale)
        .expect(1)
        .mount(&env.server)
        .await;
    wiremock::Mock::given(wiremock::matchers::path("/v1/catalog/us/genres"))
        .and(wiremock::matchers::header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("cache-control", "max-age=60"))
        .expect(1)
        .mount(&env.server)
        .await;

    let cancel = CancellationToken::new();
    env.api.genres(&cancel).await.unwrap();
    let revalidated = env.api.genres(&cancel).await.unwrap();
    assert_eq!(revalidated.len(), 3);
    // Fresh again for 60 s: no third request.
    env.api.genres(&cancel).await.unwrap();
}

#[tokio::test]
async fn errors_are_classified() {
    let env = env().await;
    let cancel = CancellationToken::new();
    let route = |p: &str| wiremock::Mock::given(wiremock::matchers::path(p.to_owned()));

    route("/v1/catalog/us/albums/404")
        .respond_with(ResponseTemplate::new(404))
        .mount(&env.server)
        .await;
    let err = env.api.album(&cancel, "404").await.unwrap_err();
    assert!(matches!(err, Error::NotFound) && err.kind() == ErrorKind::NotFound);

    route("/v1/catalog/us/albums/bad")
        .respond_with(json(r#"{"data":{"not":"a list"}}"#.into()))
        .mount(&env.server)
        .await;
    let err = env.api.album(&cancel, "bad").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Decode);

    // A fresh developer token refused: no scrape storm, the caller learns it.
    route("/v1/catalog/us/albums/401")
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&env.server)
        .await;
    let err = env.api.album(&cancel, "401").await.unwrap_err();
    assert!(matches!(err, Error::DeveloperTokenRejected(s) if s == 401));

    // An ID that would change the path never reaches the network.
    let err = env.api.album(&cancel, "../me/library").await.unwrap_err();
    assert!(matches!(err, Error::NotFound));

    // 429 twice, then 200: the GET is retried after Retry-After.
    route("/v1/catalog/us/albums/busy")
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(2)
        .mount(&env.server)
        .await;
    route("/v1/catalog/us/albums/busy")
        .respond_with(json(fixture("album.json")))
        .mount(&env.server)
        .await;
    env.api.album(&cancel, "busy").await.unwrap();

    // 429 every time: three retries, then RateLimited with Apple's Retry-After.
    route("/v1/catalog/us/albums/limit")
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .expect(4)
        .mount(&env.server)
        .await;
    let err = env.api.album(&cancel, "limit").await.unwrap_err();
    assert!(matches!(err, Error::RateLimited { retry_after: Some(d) } if d == Duration::ZERO));
    assert_eq!(err.kind(), ErrorKind::RateLimited);

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let err = env.api.album(&cancelled, "1").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
}

#[tokio::test]
async fn transport_failures_are_transport() {
    // Nothing listens on a port that was just freed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    let client = rustet_ampapi::Client::with_base_urls(reqwest::Client::new(), &base, &base);
    let api = rustet_ampapi::Api::new(client, rustet_ampapi::Locale::default());
    api.set_developer_token(rustet_ampapi::DeveloperToken::new("dev"))
        .await;
    let err = api.genres(&CancellationToken::new()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Transport, "{err:?}");
}
