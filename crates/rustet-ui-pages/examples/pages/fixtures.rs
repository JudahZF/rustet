//! Signed-in pages without an account: a local mock of Apple's API that answers the catalog
//! requests with the recorded responses (`tests/data`, copied from `rustet-app`) and the
//! `/v1/me/…` requests with library responses built from those recordings (the same albums,
//! songs and playlists, in the library's shape as `rustet-app`'s tests write them). `rustet_app`'s
//! own loaders and `Nav` build the page models, so the views get exactly what they get live.
//!
//! The library shapes follow Apple's documentation and the web player; they were not seen
//! from Apple with a real account.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rustet_ampapi::api::{ApiConfig, UserToken};
use rustet_ampapi::{Api, Client, DeveloperToken, Locale};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The editable playlist of the fixtures.
pub const MY_PLAYLIST: &str = "p.MyMix";
/// The library album with the recorded album's tracks.
pub const MY_ALBUM: &str = "l.1889568617";

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn recorded(name: &str) -> Value {
    let path = data_dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("recorded JSON")
}

/// Every catalog resource with attributes in the recordings, by type, first seen first.
fn harvest() -> BTreeMap<String, Vec<Value>> {
    fn walk(v: &Value, out: &mut BTreeMap<String, Vec<Value>>) {
        match v {
            Value::Object(map) => {
                if let (Some(Value::String(kind)), Some(Value::String(id)), Some(_)) =
                    (map.get("type"), map.get("id"), map.get("attributes"))
                {
                    let list = out.entry(kind.clone()).or_default();
                    if !list.iter().any(|r| r["id"] == *id) {
                        list.push(v.clone());
                    }
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for f in [
        "album.json",
        "new.json",
        "search.json",
        "artist_v.json",
        "charts.json",
        "playlist.json",
        "mv.json",
    ] {
        walk(&recorded(f), &mut out);
    }
    out
}

/// The library's view of the recordings.
pub struct Library {
    pub songs: Vec<Value>,
    pub albums: Vec<Value>,
    pub artists: Vec<Value>,
    pub playlists: Vec<Value>,
    pub videos: Vec<Value>,
    /// Catalog albums and playlists, as Apple's history answers.
    pub recent: Vec<Value>,
    album_tracks: Vec<Value>,
}

fn attr<'a>(r: &'a Value, key: &str) -> &'a Value {
    &r["attributes"][key]
}

fn library_song(song: &Value, prefix: &str) -> Value {
    let cid = song["id"].as_str().unwrap_or_default();
    let lid = format!("i.{prefix}{cid}");
    let album = song["relationships"]["albums"]["data"][0]["id"]
        .as_str()
        .map(|a| json!([{"id": format!("l.{a}"), "type": "library-albums"}]))
        .unwrap_or_else(|| json!([]));
    json!({
        "id": lid, "type": "library-songs",
        "attributes": {
            "name": attr(song, "name"), "artistName": attr(song, "artistName"),
            "albumName": attr(song, "albumName"), "durationInMillis": attr(song, "durationInMillis"),
            "trackNumber": attr(song, "trackNumber"), "discNumber": attr(song, "discNumber"),
            "contentRating": attr(song, "contentRating"), "artwork": attr(song, "artwork"),
            "playParams": {"id": lid, "kind": "song", "isLibrary": true, "catalogId": cid}
        },
        "relationships": {"catalog": {"data": [song]}, "albums": {"data": album}}
    })
}

fn library_album(album: &Value, tracks: &[Value]) -> Value {
    let cid = album["id"].as_str().unwrap_or_default();
    let lid = format!("l.{cid}");
    json!({
        "id": lid, "type": "library-albums",
        "attributes": {
            "name": attr(album, "name"), "artistName": attr(album, "artistName"),
            "artwork": attr(album, "artwork"), "trackCount": tracks.len().max(1),
            "contentRating": attr(album, "contentRating"),
            "playParams": {"id": lid, "kind": "album", "isLibrary": true}
        },
        "relationships": {"catalog": {"data": [album]}, "tracks": {"data": tracks}}
    })
}

fn library_playlist(
    id: &str,
    name: &str,
    artwork: &Value,
    editable: bool,
    tracks: &[Value],
    description: &str,
) -> Value {
    json!({
        "id": id, "type": "library-playlists",
        "attributes": {
            "name": name, "canEdit": editable, "artwork": artwork,
            "description": {"standard": description},
            "playParams": {"id": id, "kind": "playlist", "isLibrary": true}
        },
        "relationships": {"tracks": {"data": tracks}}
    })
}

impl Library {
    pub fn build() -> Self {
        let catalog = harvest();
        let get = |k: &str| catalog.get(k).cloned().unwrap_or_default();
        let album_json = recorded("album.json");
        let album = album_json["data"][0].clone();
        let album_tracks: Vec<Value> = album["relationships"]["tracks"]["data"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|s| library_song(s, ""))
            .collect();
        let songs: Vec<Value> = get("songs").iter().map(|s| library_song(s, "")).collect();
        let albums: Vec<Value> = get("albums")
            .iter()
            .map(|a| {
                if a["id"] == album["id"] {
                    library_album(a, &album_tracks)
                } else {
                    library_album(a, &[])
                }
            })
            .collect();
        let artists = get("artists")
            .iter()
            .map(|a| {
                json!({"id": format!("r.{}", a["id"].as_str().unwrap_or_default()), "type": "library-artists",
                       "attributes": {"name": attr(a, "name")},
                       "relationships": {"catalog": {"data": [a]}}})
            })
            .collect();
        let catalog_playlists = get("playlists");
        let art = |i: usize| {
            catalog_playlists
                .get(i)
                .map(|p| p["attributes"]["artwork"].clone())
                .unwrap_or(Value::Null)
        };
        let mine: Vec<Value> = songs.iter().take(12).cloned().collect();
        let mut playlists = vec![library_playlist(
            MY_PLAYLIST,
            "Late night drive",
            &art(0),
            true,
            &mine,
            "Songs for the road after midnight.",
        )];
        playlists.extend(catalog_playlists.iter().enumerate().map(|(i, p)| {
            let id = format!(
                "p.{}",
                p["id"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches("pl.")
            );
            let tracks: Vec<Value> = songs.iter().skip(i % 5).take(6).cloned().collect();
            library_playlist(
                &id,
                attr(p, "name").as_str().unwrap_or("Playlist"),
                &art(i),
                false,
                &tracks,
                "",
            )
        }));
        let videos = get("music-videos")
            .iter()
            .map(|v| {
                let cid = v["id"].as_str().unwrap_or_default();
                let lid = format!("i.mv{cid}");
                json!({"id": lid, "type": "library-music-videos",
                    "attributes": {"name": attr(v, "name"), "artistName": attr(v, "artistName"),
                        "durationInMillis": attr(v, "durationInMillis"), "artwork": attr(v, "artwork"),
                        "playParams": {"id": lid, "kind": "musicVideo", "isLibrary": true, "catalogId": cid}},
                    "relationships": {"catalog": {"data": [v]}}})
            })
            .collect();
        let mut recent = get("albums");
        recent.extend(get("playlists"));
        Self {
            songs,
            albums,
            artists,
            playlists,
            videos,
            recent,
            album_tracks,
        }
    }

    /// `n` library songs made from the recorded ones (the 10,000-row list).
    pub fn many_songs(&self, n: usize) -> Vec<Value> {
        let base: Vec<Value> = self
            .songs
            .iter()
            .chain(&self.album_tracks)
            .cloned()
            .collect();
        (0..n)
            .map(|i| {
                let mut s = base[i % base.len()].clone();
                let id = format!("i.gen{i}");
                s["id"] = json!(id);
                s["attributes"]["playParams"]["id"] = json!(id);
                s["attributes"]["name"] = json!(format!("{} ({})", s["attributes"]["name"].as_str().unwrap_or(""), i + 1));
                // Only what the row shows: 10,000 full catalog songs pass Api's 32 MiB cap.
                let catalog = &s["relationships"]["catalog"]["data"][0];
                let a = &catalog["attributes"];
                let slim = json!({"id": catalog["id"], "type": "songs", "attributes": {
                    "name": a["name"], "artistName": a["artistName"], "audioTraits": a["audioTraits"],
                    "playParams": a["playParams"], "durationInMillis": a["durationInMillis"]}});
                s["relationships"]["catalog"]["data"] = json!([slim]);
                s
            })
            .collect()
    }

    fn home(&self) -> Value {
        let shelf = |id: &str, title: &str, items: &[Value]| {
            json!({"id": id, "type": "personal-recommendation",
                "attributes": {"title": {"stringForDisplay": title}, "display": {"kind": "MusicCoverShelf"}},
                "relationships": {"contents": {"data": items}}})
        };
        let catalog = harvest();
        let albums = catalog.get("albums").cloned().unwrap_or_default();
        let playlists = catalog.get("playlists").cloned().unwrap_or_default();
        let stations = catalog.get("stations").cloned().unwrap_or_default();
        json!({"data": [
            shelf("made-for-you", "Made for You", &playlists[..playlists.len().min(8)]),
            shelf("recently-played", "Recently Played", &albums[..albums.len().min(12)]),
            shelf("stations-for-you", "Stations for You", &stations),
            shelf("more-albums", "Albums You Might Like", &albums[albums.len().min(8)..]),
        ]})
    }

    fn search(&self, term: &str) -> Value {
        let term = term.to_lowercase();
        let hit = |r: &&Value| {
            attr(r, "name")
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains(&term)
                || attr(r, "artistName")
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&term)
        };
        let pick = |list: &[Value]| json!({"data": list.iter().filter(hit).take(25).cloned().collect::<Vec<_>>()});
        json!({"results": {
            "library-songs": pick(&self.songs),
            "library-albums": pick(&self.albums),
            "library-artists": pick(&self.artists),
            "library-playlists": pick(&self.playlists),
        }})
    }

    fn folders(&self, id: &str) -> Value {
        let folder = |id: &str, name: &str| json!({"id": id, "type": "library-playlist-folders", "attributes": {"name": name}});
        match id {
            "p.playlistsroot" => {
                let mut data = vec![
                    folder("p.FolderRoad", "Road trips"),
                    folder("p.FolderWork", "Work"),
                ];
                data.extend(self.playlists.iter().take(3).cloned());
                json!({"data": data})
            }
            "p.FolderRoad" => {
                json!({"data": self.playlists.iter().skip(3).take(4).cloned().collect::<Vec<_>>()})
            }
            _ => {
                json!({"data": self.playlists.iter().skip(7).take(3).cloned().collect::<Vec<_>>()})
            }
        }
    }
}

/// The answer to `GET path?query` for the mock, `None` for a 404.
fn answer(lib: &Library, songs: &[Value], path: &str, query: &str) -> Option<Value> {
    let param = |k: &str| {
        query.split('&').find_map(|kv| {
            let (key, v) = kv.split_once('=')?;
            (key == k).then(|| v.replace('+', " ").replace("%20", " "))
        })
    };
    let offset: usize = param("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
    let paged = |all: &[Value], base: &str, size: usize| {
        let end = (offset + size).min(all.len());
        let mut v = json!({"data": all.get(offset..end).unwrap_or_default()});
        if end < all.len() {
            v["next"] = json!(format!("{base}?offset={end}"));
        }
        v
    };
    Some(match path {
        "/v1/catalog/us/albums/1889568617" => recorded("album.json"),
        "/v1/catalog/us/artists/697833299" => recorded("artist_v.json"),
        "/v1/catalog/us/songs/6781051513" => recorded("song.json"),
        "/v1/catalog/us/music-videos/6834539294" => recorded("mv.json"),
        "/v1/catalog/us/stations/ra.6833682596" => recorded("station.json"),
        "/v1/catalog/us/charts" => recorded("charts.json"),
        "/v1/catalog/us/genres" => recorded("genres.json"),
        "/v1/catalog/us/stations" => recorded("live_st.json"),
        "/edge/v1/catalog/us/search" => recorded("search.json"),
        "/edge/v1/catalog/us/search/suggestions" => recorded("suggest.json"),
        "/edge/v1/recommendations/us" => recorded("landing.json"),
        "/edge/v1/editorial/us/groupings" if param("name").as_deref() == Some("radio") => {
            recorded("radio.json")
        }
        "/edge/v1/editorial/us/groupings" => recorded("new.json"),
        "/edge/v1/editorial/us/rooms/6834266064" => recorded("room.json"),
        "/v1/catalog/us" => recorded("curator.json"),
        "/v1/me/recommendations" => lib.home(),
        // The frame-time run gets every song at once.
        "/v1/me/library/songs" => paged(
            songs,
            "/v1/me/library/songs",
            if songs.len() > 1000 { songs.len() } else { 10 },
        ),
        "/v1/me/library/albums" => paged(&lib.albums, "/v1/me/library/albums", 100),
        "/v1/me/library/artists" => paged(&lib.artists, "/v1/me/library/artists", 100),
        "/v1/me/library/playlists" => paged(&lib.playlists, "/v1/me/library/playlists", 100),
        "/v1/me/library/music-videos" => paged(&lib.videos, "/v1/me/library/music-videos", 100),
        "/v1/me/library/recently-added" => {
            let mut items: Vec<Value> = lib.albums.iter().take(15).cloned().collect();
            items.extend(lib.playlists.iter().take(10).cloned());
            json!({"data": items})
        }
        "/v1/me/recent/played" => paged(&lib.recent, "/v1/me/recent/played", 20),
        "/v1/me/library/search" => lib.search(&param("term").unwrap_or_default()),
        _ => {
            if path.starts_with("/v1/catalog/us/artists/") && path.contains("/view/") {
                return Some(recorded("artist_all.json"));
            }
            if let Some(rest) = path.strip_prefix("/v1/catalog/us/playlists/") {
                if rest.ends_with("/tracks") {
                    return (offset == 3).then(|| recorded("pl_tracks.json"));
                }
                // The recorded playlist was trimmed to 3 tracks; it continues as a long one does.
                let mut body = recorded("playlist.json");
                body["data"][0]["relationships"]["tracks"]["next"] =
                    json!(format!("{path}/tracks?offset=3"));
                return Some(body);
            }
            if let Some(rest) = path.strip_prefix("/v1/me/library/playlist-folders/") {
                return Some(lib.folders(rest.trim_end_matches("/children")));
            }
            if let Some(id) = path.strip_prefix("/v1/me/library/albums/") {
                let album = lib.albums.iter().find(|a| a["id"] == id)?;
                return Some(json!({"data": [album]}));
            }
            if let Some(id) = path.strip_prefix("/v1/me/library/playlists/") {
                let playlist = lib.playlists.iter().find(|p| p["id"] == id)?;
                return Some(json!({"data": [playlist]}));
            }
            return None;
        }
    })
}

/// Serves the fixtures over plain HTTP on 127.0.0.1 and returns the base URL. `songs` is the
/// library's song list (the recorded songs, or 10,000 generated ones for the frame-time run).
pub async fn serve(songs: Option<usize>) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let lib = Arc::new(Library::build());
    let songs: Arc<Vec<Value>> = Arc::new(match songs {
        Some(n) => lib.many_songs(n),
        None => lib.songs.iter().chain(&lib.album_tracks).cloned().collect(),
    });
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let (lib, songs) = (lib.clone(), songs.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tcp.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let mut words = head.split_whitespace();
                let method = words.next().unwrap_or("GET").to_owned();
                let target = words.next().unwrap_or("/");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let body = if method == "GET" {
                    answer(&lib, &songs, path, query).map(|v| v.to_string().into_bytes())
                } else {
                    // Writes succeed with no content (the fixtures do not change).
                    Some(Vec::new())
                };
                let response = match body {
                    Some(body) => {
                        let status = if body.is_empty() {
                            "204 No Content"
                        } else {
                            "200 OK"
                        };
                        let mut r = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
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

/// An `Api` on the mock, signed in with a made-up token (no Apple request, no account).
pub async fn api(songs: Option<usize>) -> std::io::Result<Api> {
    let base = serve(songs).await?;
    let client = Client::with_base_urls(reqwest::Client::new(), base.clone(), base.clone());
    let config = ApiConfig {
        edge_base: format!("{base}/edge"),
        play_activity_url: format!("{base}/play"),
        ..ApiConfig::default()
    };
    let api = Api::with_config(client, Locale::new("us", "en-US"), config);
    api.set_developer_token(DeveloperToken::new("dev")).await;
    api.set_user_token(Some(UserToken::new("fixture")));
    Ok(api)
}

/// The fixtures' playlists as "Add to playlist" choices.
pub fn playlist_choices() -> Vec<rustet_ui_pages::PlaylistChoice> {
    Library::build()
        .playlists
        .iter()
        .map(|p| rustet_ui_pages::PlaylistChoice {
            id: p["id"].as_str().unwrap_or_default().to_owned(),
            name: p["attributes"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
        .collect()
}
