//! Real page models for the gallery: `rustet-app`'s recorded Apple responses
//! (`crates/rustet-app/tests/data`), served by a local HTTP mock of Apple's API so that
//! `rustet_app`'s own loaders build the models (no sign-in, no Apple request).

use std::path::PathBuf;
use std::time::Duration;

use rustet_ampapi::api::ApiConfig;
use rustet_ampapi::model::AudioTrait;
use rustet_ampapi::{Api, Client, DeveloperToken, Locale};
use rustet_app::pages::catalog::{self, Hint};
use rustet_app::pages::lyrics::{self, Lyrics};
use rustet_app::pages::model::{Artwork, Linked, Tile, TileKind, TrackId, TrackKind, TrackRow};
use rustet_app::pages::{AlbumPage, ArtistPage, BrowsePage, LoadError, SearchPage};
use rustet_app::route::Route;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The pages the gallery composes.
pub struct Pages {
    pub album: Result<AlbumPage, LoadError>,
    pub artist: Result<ArtistPage, LoadError>,
    pub search: Result<SearchPage, LoadError>,
    pub new: Result<BrowsePage, LoadError>,
    pub hints: Vec<Hint>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rustet-app/tests/data")
}

/// The fixture that answers `path`, as `rustet-app`'s tests mount them.
fn fixture_for(path: &str) -> Option<&'static str> {
    Some(match path {
        "/v1/catalog/us/albums/1889568617" => "album.json",
        "/v1/catalog/us/artists/697833299" => "artist_v.json",
        "/edge/v1/catalog/us/search" => "search.json",
        "/edge/v1/catalog/us/search/suggestions" => "suggest.json",
        "/edge/v1/editorial/us/groupings" => "new.json",
        _ => return None,
    })
}

/// Serves the fixtures over plain HTTP on 127.0.0.1 and returns the base URL.
async fn serve() -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 2048];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tcp.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head);
                let target = head.split_whitespace().nth(1).unwrap_or("/");
                let path = target.split('?').next().unwrap_or(target);
                let body =
                    fixture_for(path).and_then(|f| std::fs::read(fixtures_dir().join(f)).ok());
                let response = match body {
                    Some(body) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend(body);
                        r
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = tcp.write_all(&response).await;
                let _ = tcp.shutdown().await;
            });
        }
    });
    Ok(base)
}

/// Loads the four pages and the hints for "phoebe" through `rustet_app`'s loaders.
pub async fn load() -> std::io::Result<Pages> {
    let base = serve().await?;
    let client = Client::with_base_urls(reqwest::Client::new(), base.clone(), base.clone());
    let config = ApiConfig {
        edge_base: format!("{base}/edge"),
        play_activity_url: format!("{base}/play"),
        ..ApiConfig::default()
    };
    let api = Api::with_config(client, Locale::new("us", "en-US"), config);
    api.set_developer_token(DeveloperToken::new("dev")).await;
    let (album, artist, search, new, hints) = tokio::join!(
        catalog::album(&api, "1889568617"),
        catalog::artist(&api, "697833299"),
        catalog::search(&api, "radiohead"),
        catalog::new(&api),
        catalog::hints(&api, "phoebe"),
    );
    Ok(Pages {
        album,
        artist,
        search,
        new,
        hints: hints.unwrap_or_default(),
    })
}

/// Lyrics for the lyrics page: word-timed for the first lines, line-timed after, with a
/// translation. Original text.
pub fn lyrics() -> Lyrics {
    let mut ttml = String::from(r#"<tt xmlns:ttm="m" itunes:timing="Word"><body><div>"#);
    let lines = [
        "Night trains hum beneath the city lights",
        "I count the windows as they pass me by",
        "夜の電車が街の灯りの下で歌う",
        "Every station is a door I never open",
        "Every echo is a name I almost know",
        "القطار يغني تحت أضواء المدينة",
        "Hold the last note until the morning",
        "We were the static between two songs",
    ];
    let mut t = 2.0f64;
    for round in 0..6 {
        for line in lines {
            let words: Vec<&str> = if line.contains(' ') {
                line.split(' ').collect()
            } else {
                vec![line]
            };
            let step = 2.6 / words.len() as f64;
            ttml.push_str(&format!(r#"<p begin="{t:.2}" end="{:.2}">"#, t + 2.6));
            for (i, w) in words.iter().enumerate() {
                let b = t + step * i as f64;
                ttml.push_str(&format!(
                    r#"<span begin="{b:.2}" end="{:.2}">{w}</span>"#,
                    b + step
                ));
                if i + 1 < words.len() {
                    ttml.push(' ');
                }
            }
            ttml.push_str("</p>");
            t += 3.0;
        }
        let _ = round;
    }
    ttml.push_str("</div></body></tt>");
    lyrics::parse(&ttml).expect("gallery lyrics")
}

/// The bench data: 5,000 tiles and 10,000 rows from the recorded catalog of the first proof
/// (`scripts/record-catalog.py`), real Apple artwork templates.
pub fn bench(catalog: &std::path::Path) -> Option<(Vec<Tile>, Vec<TrackRow>)> {
    #[derive(serde::Deserialize)]
    struct Catalog {
        albums: Vec<Rec>,
        tracks: Vec<Rec>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Rec {
        id: String,
        name: String,
        artist: String,
        #[serde(default)]
        album: String,
        #[serde(default)]
        duration_ms: u64,
        artwork: Option<String>,
    }
    let c: Catalog = serde_json::from_slice(&std::fs::read(catalog).ok()?).ok()?;
    let art = |t: &Option<String>| {
        t.as_ref().map(|t| Artwork {
            template: t.clone(),
            width: None,
            height: None,
            palette: None,
        })
    };
    let tiles = c
        .albums
        .iter()
        .map(|a| Tile {
            kind: TileKind::Album,
            title: a.name.clone(),
            subtitle: Some(a.artist.clone()),
            artwork: art(&a.artwork),
            badge: None,
            explicit: false,
            route: Some(Route::Album { id: a.id.clone() }),
            play: None,
            item: None,
        })
        .collect();
    let rows = c
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| TrackRow {
            id: TrackId::Catalog(t.id.clone()),
            kind: TrackKind::Song,
            number: Some(i as u32 % 14 + 1),
            disc: None,
            title: t.name.clone(),
            explicit: i % 9 == 0,
            artist: Linked {
                text: t.artist.clone(),
                route: None,
            },
            album: Some(Linked {
                text: t.album.clone(),
                route: None,
            }),
            duration: Some(Duration::from_millis(t.duration_ms)),
            traits: match i % 3 {
                0 => vec![AudioTrait::Lossless, AudioTrait::HiResLossless],
                1 => vec![AudioTrait::Lossless],
                _ => vec![],
            },
            apple_digital_master: false,
            availability: rustet_app::pages::model::Availability::Playable,
            artwork: art(&t.artwork),
            popularity: None,
        })
        .collect();
    Some((tiles, rows))
}
