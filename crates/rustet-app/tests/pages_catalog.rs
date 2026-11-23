//! Signed-out page loaders against trimmed real responses (`tests/data`, recorded from Apple's
//! servers): what each page model takes from Apple's JSON.

mod pages_common;

use pages_common::{EDGE, env, fixture, json};
use rustet_ampapi::ArtistView;
use rustet_app::pages::catalog::{self, Hint, SearchGroup};
use rustet_app::pages::model::{
    Availability, LinkTarget, Listing, Lossless, Rgb, ShelfItems, TileKind, TileSize, TrackId,
    TrackKind,
};
use rustet_app::pages::play::PlayTarget;
use rustet_app::pages::{LoadError, Recovery};
use rustet_app::route::Route;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
async fn album_page_has_header_tracks_marks_and_sections() {
    let env = env().await;
    env.get("/v1/catalog/us/albums/1889568617", fixture("album.json"))
        .await;
    let page = catalog::album(&env.api, "1889568617").await.unwrap();

    assert_eq!(page.header.title, "WARM UP REMIXES");
    let artist = page.header.subtitle.as_ref().unwrap();
    assert_eq!(
        artist.route,
        Some(Route::Artist {
            id: "254027430".into()
        })
    );
    let palette = page.header.palette().expect("colours from the artwork");
    assert_eq!(
        palette.background,
        Rgb {
            r: 255,
            g: 255,
            b: 255
        }
    );
    assert_eq!(page.header.quality.lossless, Some(Lossless::Lossless));
    assert_eq!(page.info.genre.as_deref(), Some("Dance"));
    assert_eq!(page.info.released.map(|d| d.year), Some(2026));
    assert_eq!(page.info.record_label.as_deref(), Some("Pixadelic"));

    assert_eq!(page.discs.len(), 1);
    let first = &page.discs[0].tracks[0];
    assert_eq!(first.id, TrackId::Catalog("1889568624".into()));
    assert_eq!(first.kind, TrackKind::Song);
    assert_eq!(first.number, Some(1));
    assert_eq!(first.duration.unwrap().as_millis(), 232_347);
    assert_eq!(first.availability, Availability::Playable);
    assert!(!first.explicit);
    assert_eq!(first.quality().lossless, Some(Lossless::Lossless));
    assert_eq!(
        first.album.as_ref().unwrap().route,
        Some(Route::Album {
            id: "1889568617".into()
        })
    );
    assert_eq!(page.tracks().count(), 3);
    assert!(page.duration.as_secs() > 0);

    // Empty views (other versions, related videos) are dropped; "more by" sees all albums.
    let ids: Vec<_> = page.shelves.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["more-by-artist", "you-might-also-like", "appears-on"]);
    assert_eq!(
        page.shelves[0].see_all,
        Some(Route::ArtistView {
            id: "254027430".into(),
            view: ArtistView::FullAlbums
        })
    );
}

#[tokio::test]
async fn playlist_tracks_continue_by_offset_until_apple_answers_404() {
    let env = env().await;
    let id = "pl.f083772167b24f5a9d691e3c3e16f968";
    // The recorded playlist was trimmed to 3 tracks; give it a `next` as a long one has.
    let mut body: serde_json::Value = serde_json::from_str(&fixture("playlist.json")).unwrap();
    body["data"][0]["relationships"]["tracks"]["next"] =
        format!("/v1/catalog/us/playlists/{id}/tracks?offset=3").into();
    env.get(&format!("/v1/catalog/us/playlists/{id}"), body.to_string())
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/catalog/us/playlists/{id}/tracks")))
        .and(query_param("offset", "3"))
        .respond_with(json(fixture("pl_tracks.json")))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/catalog/us/playlists/{id}/tracks")))
        .and(query_param("offset", "6"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&env.server)
        .await;

    let mut page = catalog::playlist(&env.api, id).await.unwrap();
    assert_eq!(page.header.title, "Top 25: Chicago");
    assert_eq!(page.header.subtitle.as_ref().unwrap().text, "Apple Music");
    let row = &page.tracks.items[0];
    assert_eq!(row.title, "Housewife");
    assert_eq!(
        row.artist.route,
        Some(Route::Artist {
            id: "1446365464".into()
        })
    );
    // The album link comes from the song URL when the album is not included.
    assert_eq!(
        row.album.as_ref().unwrap().route,
        Some(Route::Album {
            id: "6822497770".into()
        })
    );

    let more = page.tracks.more.take().expect("more tracks");
    let next = more.load(&env.api).await.unwrap();
    assert_eq!(next.items.len(), 3);
    let last = next.more.clone().expect("Apple gave a next link");
    page.tracks.extend(next);
    assert_eq!(page.tracks.items.len(), 6);
    let end = last.load(&env.api).await.unwrap();
    assert!(
        end.items.is_empty() && end.more.is_none(),
        "404 past the end is the end"
    );
}

#[tokio::test]
async fn artist_page_orders_sections_and_offers_see_all_when_apple_has_more() {
    let env = env().await;
    env.get("/v1/catalog/us/artists/697833299", fixture("artist_v.json"))
        .await;
    let page = catalog::artist(&env.api, "697833299").await.unwrap();

    assert_eq!(page.header.title, "Phoebe Bridgers");
    let ids: Vec<_> = page.shelves.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "top-songs",
            "full-albums",
            "singles",
            "top-music-videos",
            "featured-playlists",
            "appears-on-albums",
            "similar-artists"
        ]
    );
    let top = &page.shelves[0];
    assert!(matches!(&top.items, ShelfItems::Tracks(rows) if rows[0].title == "I Know the End"));
    assert_eq!(
        top.see_all,
        Some(Route::ArtistView {
            id: "697833299".into(),
            view: ArtistView::TopSongs
        })
    );
    assert_eq!(page.shelves[1].see_all, None, "full albums: no next link");
    assert_eq!(page.top_songs().len(), 3);
    assert!(page.latest_release.is_some());
}

#[tokio::test]
async fn artist_see_all_is_a_grid_of_tiles() {
    let env = env().await;
    env.get(
        "/v1/catalog/us/artists/1/view/full-albums",
        fixture("artist_all.json"),
    )
    .await;
    let page = catalog::artist_view(&env.api, "1", ArtistView::FullAlbums)
        .await
        .unwrap();
    let Listing::Tiles(list) = &page.listing else {
        panic!("tiles")
    };
    assert_eq!(list.items.len(), 3);
    assert!(list.items.iter().all(|t| t.kind == TileKind::Album));
    assert!(matches!(&list.items[0].play, Some(PlayTarget::Album { id }) if id == "6836585808"));
    assert!(list.more.is_none());
}

#[tokio::test]
async fn song_page_has_credits_album_and_lyrics_flags() {
    let env = env().await;
    env.get("/v1/catalog/us/songs/6781051513", fixture("song.json"))
        .await;
    let page = catalog::song(&env.api, "6781051513").await.unwrap();
    assert_eq!(page.track.title, "Still Standing");
    assert!(page.has_lyrics && page.has_synced_lyrics);
    let titles: Vec<_> = page.credits.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "PERFORMING ARTISTS",
            "COMPOSITION & LYRICS",
            "PRODUCTION & ENGINEERING"
        ]
    );
    assert!(page.credits[0].credits.iter().all(|c| !c.name.is_empty()));
    assert_eq!(page.album.as_ref().map(|a| a.kind), Some(TileKind::Album));
}

#[tokio::test]
async fn music_video_page_is_a_video_row_with_sections() {
    let env = env().await;
    env.get("/v1/catalog/us/music-videos/6834539294", fixture("mv.json"))
        .await;
    let page = catalog::music_video(&env.api, "6834539294").await.unwrap();
    assert_eq!(page.video.kind, TrackKind::MusicVideo);
    assert_eq!(page.video.id, TrackId::Catalog("6834539294".into()));
    assert!(!page.shelves.is_empty());
}

#[tokio::test]
async fn a_stream_station_is_live() {
    let env = env().await;
    env.get(
        "/v1/catalog/us/stations/ra.6833682596",
        fixture("station.json"),
    )
    .await;
    let page = catalog::station(&env.api, "ra.6833682596").await.unwrap();
    assert_eq!(page.header.title, "Gabriel Jacoby");
    // `isLive` is false, but `playParams.format` is "stream": the player treats it as live.
    assert!(page.live);
    assert!(
        matches!(&page.play, Some(PlayTarget::Station(s)) if s.live && s.id == "ra.6833682596")
    );
}

#[tokio::test]
async fn room_and_curator_pages() {
    let env = env().await;
    env.get(
        &format!("{EDGE}/v1/editorial/us/rooms/6834266064"),
        fixture("room.json"),
    )
    .await;
    env.get("/v1/catalog/us", fixture("curator.json")).await;

    let room = catalog::room(&env.api, "6834266064").await.unwrap();
    assert_eq!(room.title.as_deref(), Some("Everyone’s Listening To..."));
    assert!(matches!(&room.listing, Listing::Tiles(l) if l.items.len() == 3));

    let curator = catalog::curator(&env.api, "976439548").await.unwrap();
    assert_eq!(curator.header.title, "Apple Music Pop");
    assert!(
        !curator.shelves.is_empty(),
        "an Apple curator's editorial page"
    );
    assert_eq!(curator.playlists.items.len(), 3);
    assert!(curator.playlists.more.is_some());
}

#[tokio::test]
async fn charts_list_songs_first_and_offer_genres() {
    let env = env().await;
    env.get("/v1/catalog/us/charts", fixture("charts.json"))
        .await;
    env.get("/v1/catalog/us/genres", fixture("genres.json"))
        .await;
    let page = catalog::charts(&env.api, None).await.unwrap();
    assert!(matches!(&page.shelves[0].items, ShelfItems::Tracks(rows) if !rows.is_empty()));
    assert_eq!(page.shelves[0].title.as_deref(), Some("Top Songs"));
    assert!(page.genres.iter().any(|g| g.name == "Alternative"
        && g.route
            == Route::Charts {
                genre: Some("20".into())
            }));
}

#[tokio::test]
async fn new_page_shelves_are_typed_and_link_to_rooms() {
    let env = env().await;
    env.get(
        &format!("{EDGE}/v1/editorial/us/groupings"),
        fixture("new.json"),
    )
    .await;
    let page = catalog::new(&env.api).await.unwrap();
    assert!(matches!(&page.shelves[0].items, ShelfItems::Featured(t) if !t.is_empty()));
    let best = page
        .shelves
        .iter()
        .find(|s| s.title.as_deref() == Some("Best New Songs"))
        .unwrap();
    assert!(matches!(best.items, ShelfItems::Tracks(_)));
    assert_eq!(
        best.see_all,
        Some(Route::Room {
            id: "6834267408".into()
        })
    );
    let links = page
        .shelves
        .iter()
        .find_map(|s| match &s.items {
            ShelfItems::Links(l) => Some(l),
            _ => None,
        })
        .unwrap();
    // Old iTunes grouping links have no page in Rustet.
    assert!(
        links
            .iter()
            .any(|l| matches!(&l.target, LinkTarget::External(u) if u.contains("viewGrouping")))
    );
}

#[tokio::test]
async fn radio_page_starts_with_the_live_stations() {
    let env = env().await;
    env.get(
        &format!("{EDGE}/v1/editorial/us/groupings"),
        fixture("radio.json"),
    )
    .await;
    env.get("/v1/catalog/us/stations", fixture("live_st.json"))
        .await;
    let page = catalog::radio(&env.api).await.unwrap();
    let live = &page.shelves[0];
    assert_eq!(live.id, "live-stations");
    let ShelfItems::Tiles {
        tiles,
        size: TileSize::Regular,
    } = &live.items
    else {
        panic!("square tiles")
    };
    assert_eq!(tiles.len(), 3);
    assert!(
        tiles
            .iter()
            .all(|t| matches!(t.kind, TileKind::Station { live: true }))
    );
    assert!(page.shelves.len() > 1);
}

#[tokio::test]
async fn search_sections_follow_apple_and_more_results_keep_the_with_parameter() {
    let env = env().await;
    env.get(
        &format!("{EDGE}/v1/catalog/us/search"),
        fixture("search.json"),
    )
    .await;
    let page = catalog::search(&env.api, " radiohead ").await.unwrap();
    assert_eq!(page.term, "radiohead");
    assert_eq!(page.sections[0].group, SearchGroup::Top);
    let songs = page
        .sections
        .iter()
        .find(|s| s.group == SearchGroup::Songs)
        .unwrap();
    assert_eq!(songs.title, "Songs");
    let Listing::Tracks(list) = &songs.listing else {
        panic!("songs are a track list")
    };
    assert_eq!(list.items.len(), 3);

    // Apple's `next` link drops `with=…` and then answers with nothing: it is added back.
    Mock::given(method("GET"))
        .and(path(format!("{EDGE}/v1/catalog/us/search")))
        .and(query_param("offset", "21"))
        .and(query_param(
            "with",
            "lyricHighlights,lyrics,naturalLanguage,serverBubbles,subtitles",
        ))
        .respond_with(json(fixture("search.json")))
        .with_priority(1)
        .expect(1)
        .mount(&env.server)
        .await;
    let more = list.more.as_ref().unwrap().load(&env.api).await.unwrap();
    assert_eq!(more.items.len(), 3);
}

#[tokio::test]
async fn hints_are_terms_and_items() {
    let env = env().await;
    env.get(
        &format!("{EDGE}/v1/catalog/us/search/suggestions"),
        fixture("suggest.json"),
    )
    .await;
    let hints = catalog::hints(&env.api, "phoebe").await.unwrap();
    assert!(matches!(&hints[0], Hint::Term { term, .. } if term == "phoebe bridgers"));
    let items: Vec<_> = hints
        .iter()
        .filter_map(|h| match h {
            Hint::Item(t) => Some(t.kind),
            Hint::Term { .. } => None,
        })
        .collect();
    assert_eq!(items, [TileKind::Artist, TileKind::Song]);
    assert!(
        catalog::hints(&env.api, "  ").await.unwrap().is_empty(),
        "no request for blank"
    );
}

#[tokio::test]
async fn search_landing_shows_category_tiles() {
    let env = env().await;
    env.get(
        &format!("{EDGE}/v1/recommendations/us"),
        fixture("landing.json"),
    )
    .await;
    let page = catalog::search_landing(&env.api).await.unwrap();
    let tiles: Vec<_> = page
        .shelves
        .iter()
        .flat_map(|s| match &s.items {
            ShelfItems::Tiles { tiles, .. } => tiles.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(!tiles.is_empty());
    assert!(
        tiles
            .iter()
            .all(|t| matches!(t.route, Some(Route::Curator { .. })))
    );
}

#[tokio::test]
async fn a_missing_item_says_so_and_offers_going_back() {
    let env = env().await;
    Mock::given(method("GET"))
        .and(path("/v1/catalog/us/albums/1"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&env.server)
        .await;
    let err = catalog::album(&env.api, "1").await.unwrap_err();
    assert_eq!(err, LoadError::NotFound);
    let state = err.state(&Route::Album { id: "1".into() });
    assert_eq!(state.title, "Rustet could not load this album.");
    assert_eq!(state.action, Recovery::GoBack);
}
