//! Apple's typed resources (`rustet_ampapi::model`) into page models. Items without attributes
//! (bare references) and resource types no page draws are skipped, never an error.

use std::time::Duration;

use rustet_ampapi::api::{ResourceRef, ResourceType};
use rustet_ampapi::model::{
    self as am, DisplayStyle, EditorialNotes, LibrarySong, MusicVideo, Resource, Song, View,
};
use url::Url;

use super::model::{
    Artwork, Availability, LinkItem, LinkTarget, Linked, Notes, Palette, Rgb, Shelf, ShelfItems,
    Tile, TileKind, TileSize, TrackId, TrackKind, TrackRow,
};
use super::play::{PlayTarget, StationRef};
use crate::route::Route;

pub(crate) fn artwork(art: Option<&am::Artwork>) -> Option<Artwork> {
    let art = art.filter(|a| !a.url.is_empty())?;
    let color = |c: &Option<String>| c.as_deref().and_then(Rgb::from_hex);
    let palette = color(&art.bg_color)
        .zip(color(&art.text_color1))
        .map(|(bg, t1)| {
            let t2 = color(&art.text_color2).unwrap_or(t1);
            Palette {
                background: bg,
                text: [
                    t1,
                    t2,
                    color(&art.text_color3).unwrap_or(t1),
                    color(&art.text_color4).unwrap_or(t2),
                ],
            }
        });
    Some(Artwork {
        template: art.url.clone(),
        width: art.width,
        height: art.height,
        palette,
    })
}

/// Editorial HTML (`<i>`, `<br>`, entities) as plain text.
pub(crate) fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = &rest[start..];
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        if tag.starts_with("br") || tag == "/p" {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    decoded.trim().to_owned()
}

pub(crate) fn notes(notes: Option<&EditorialNotes>) -> Option<Notes> {
    let n = notes?;
    let text = |s: &Option<String>| s.as_deref().map(plain).filter(|s| !s.is_empty());
    let short = text(&n.short).or_else(|| text(&n.tagline));
    let full = text(&n.standard);
    (short.is_some() || full.is_some()).then_some(Notes { short, full })
}

pub(crate) fn explicit(rating: Option<&String>) -> bool {
    rating.is_some_and(|r| r == "explicit")
}

pub(crate) fn duration(ms: Option<u64>) -> Option<Duration> {
    ms.filter(|&ms| ms > 0).map(Duration::from_millis)
}

fn non_empty(s: Option<&String>) -> Option<String> {
    s.filter(|s| !s.is_empty()).cloned()
}

/// The artist route of an `artistUrl` (`https://music.apple.com/us/artist/x/159260351`).
pub(crate) fn artist_route(url: Option<&String>) -> Option<Route> {
    Route::from_link(url?).filter(|r| matches!(r, Route::Artist { .. }))
}

/// The album ID in a song URL (`…/album/slug/1441164426?i=1441164430`).
fn album_id_in_url(url: Option<&String>) -> Option<String> {
    let url = Url::parse(url?).ok()?;
    let parts: Vec<&str> = url.path_segments()?.collect();
    let at = parts.iter().position(|p| *p == "album")?;
    parts[at + 1..]
        .last()
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .map(|id| (*id).to_owned())
}

fn first_id<T>(page: Option<&am::Page<am::Item<T, impl Default, impl Default>>>) -> Option<String> {
    page?.data.first().map(|item| item.id.clone())
}

fn first_resource_id(page: Option<&am::Page<Resource>>) -> Option<String> {
    page?.data.first().map(|r| r.id().to_owned())
}

/// A catalog song as a row. `album` overrides the album link (album pages link to themselves).
pub(crate) fn song_row(song: &Song) -> Option<TrackRow> {
    let a = song.attributes.as_ref()?;
    let r = &song.relationships;
    let artist_route = first_id(r.artists.as_ref())
        .map(|id| Route::Artist { id })
        .or_else(|| artist_route(a.artist_url.as_ref()));
    let album_route = first_id(r.albums.as_ref())
        .or_else(|| album_id_in_url(a.url.as_ref()))
        .map(|id| Route::Album { id });
    Some(TrackRow {
        id: TrackId::Catalog(song.id.clone()),
        kind: TrackKind::Song,
        number: a.track_number,
        disc: a.disc_number,
        title: a.name.clone(),
        explicit: explicit(a.content_rating.as_ref()),
        artist: Linked {
            text: a.artist_name.clone(),
            route: artist_route,
        },
        album: non_empty(a.album_name.as_ref()).map(|text| Linked {
            text,
            route: album_route,
        }),
        duration: duration(a.duration_in_millis),
        traits: a.audio_traits.clone(),
        apple_digital_master: a.is_apple_digital_master,
        availability: availability(a.play_params.is_some()),
        artwork: artwork(a.artwork.as_ref()),
        popularity: song.meta.popularity.map(|p| p as f32),
    })
}

fn availability(playable: bool) -> Availability {
    if playable {
        Availability::Playable
    } else {
        Availability::Unavailable
    }
}

pub(crate) fn video_row(video: &MusicVideo) -> Option<TrackRow> {
    let a = video.attributes.as_ref()?;
    let r = &video.relationships;
    Some(TrackRow {
        id: TrackId::Catalog(video.id.clone()),
        kind: TrackKind::MusicVideo,
        number: None,
        disc: None,
        title: a.name.clone(),
        explicit: explicit(a.content_rating.as_ref()),
        artist: Linked {
            text: a.artist_name.clone(),
            route: first_id(r.artists.as_ref()).map(|id| Route::Artist { id }),
        },
        album: non_empty(a.album_name.as_ref()).map(|text| Linked {
            text,
            route: first_id(r.albums.as_ref()).map(|id| Route::Album { id }),
        }),
        duration: duration(a.duration_in_millis),
        traits: Vec::new(),
        apple_digital_master: false,
        availability: availability(a.play_params.is_some()),
        artwork: artwork(a.artwork.as_ref()),
        popularity: None,
    })
}

/// A library song or video as a row: its library ID, and its catalog ID when it has one.
fn library_row<C>(
    item: &am::Item<am::LibraryTrackAttributes, am::LibraryTrackRelationships<C>>,
    kind: TrackKind,
    catalog: impl Fn(&C) -> Option<TrackRow>,
) -> Option<TrackRow> {
    let a = item.attributes.as_ref()?;
    let r = &item.relationships;
    let catalog_row = r
        .catalog
        .as_ref()
        .and_then(|p| p.data.first())
        .and_then(catalog);
    let catalog_id = a
        .play_params
        .as_ref()
        .and_then(|p| p.stream_catalog_id().map(str::to_owned))
        .or_else(|| {
            catalog_row
                .as_ref()
                .and_then(|c| c.id.catalog_id().map(str::to_owned))
        });
    let album_route = first_resource_id(r.albums.as_ref())
        .map(|id| Route::LibraryAlbum { id })
        .or_else(|| {
            catalog_row
                .as_ref()
                .and_then(|c| c.album.as_ref()?.route.clone())
        });
    let artist_route = catalog_row.as_ref().and_then(|c| c.artist.route.clone());
    let (traits, adm) = catalog_row
        .as_ref()
        .map(|c| (c.traits.clone(), c.apple_digital_master))
        .unwrap_or_default();
    Some(TrackRow {
        availability: availability(catalog_id.is_some()),
        id: TrackId::Library {
            library_id: item.id.clone(),
            catalog_id,
        },
        kind,
        number: a.track_number,
        disc: a.disc_number,
        title: a.name.clone(),
        explicit: explicit(a.content_rating.as_ref()),
        artist: Linked {
            text: a.artist_name.clone().unwrap_or_default(),
            route: artist_route,
        },
        album: non_empty(a.album_name.as_ref()).map(|text| Linked {
            text,
            route: album_route,
        }),
        duration: duration(a.duration_in_millis),
        traits,
        apple_digital_master: adm,
        artwork: artwork(a.artwork.as_ref())
            .or_else(|| catalog_row.as_ref().and_then(|c| c.artwork.clone())),
        popularity: None,
    })
}

pub(crate) fn library_song_row(song: &LibrarySong) -> Option<TrackRow> {
    library_row(song, TrackKind::Song, song_row)
}

/// A song or music video (catalog or library) as a row.
pub(crate) fn track(resource: &Resource) -> Option<TrackRow> {
    match resource {
        Resource::Song(s) => song_row(s),
        Resource::MusicVideo(v) => video_row(v),
        Resource::LibrarySong(s) => library_song_row(s),
        Resource::LibraryMusicVideo(v) => library_row(v, TrackKind::MusicVideo, video_row),
        _ => None,
    }
}

fn tile_of(kind: TileKind, title: &str, route: Option<Route>) -> Tile {
    Tile {
        kind,
        title: title.to_owned(),
        subtitle: None,
        artwork: None,
        badge: None,
        explicit: false,
        route,
        play: None,
        item: None,
    }
}

/// Any resource a shelf or grid can show, as a tile.
pub(crate) fn tile(resource: &Resource) -> Option<Tile> {
    let item = |kind, id: &str| Some(ResourceRef::new(kind, id));
    Some(match resource {
        Resource::Album(r) => {
            let a = r.attributes.as_ref()?;
            let id = r.id.clone();
            Tile {
                subtitle: non_empty(Some(&a.artist_name)),
                artwork: artwork(a.artwork.as_ref()),
                explicit: explicit(a.content_rating.as_ref()),
                play: a
                    .play_params
                    .is_some()
                    .then(|| PlayTarget::Album { id: id.clone() }),
                item: item(ResourceType::Albums, &id),
                ..tile_of(TileKind::Album, &a.name, Some(Route::Album { id }))
            }
        }
        Resource::Playlist(r) => {
            let a = r.attributes.as_ref()?;
            let id = r.id.clone();
            Tile {
                subtitle: non_empty(a.curator_name.as_ref()),
                artwork: artwork(a.artwork.as_ref()),
                play: Some(PlayTarget::Playlist { id: id.clone() }),
                item: item(ResourceType::Playlists, &id),
                ..tile_of(TileKind::Playlist, &a.name, Some(Route::Playlist { id }))
            }
        }
        Resource::Station(r) => {
            let a = r.attributes.as_ref()?;
            let format = a.play_params.as_ref().and_then(|p| p.format.as_deref());
            let live = a.is_live || format == Some("stream");
            let id = r.id.clone();
            Tile {
                subtitle: non_empty(a.station_provider_name.as_ref())
                    .or_else(|| notes(a.editorial_notes.as_ref()).and_then(|n| n.short)),
                artwork: artwork(a.artwork.as_ref()),
                play: a.play_params.is_some().then(|| {
                    PlayTarget::Station(StationRef {
                        id: id.clone(),
                        name: a.name.clone(),
                        live,
                    })
                }),
                item: item(ResourceType::Stations, &id),
                ..tile_of(
                    TileKind::Station { live },
                    &a.name,
                    Some(Route::Station { id }),
                )
            }
        }
        Resource::Artist(r) => {
            let a = r.attributes.as_ref()?;
            Tile {
                artwork: artwork(a.artwork.as_ref()),
                item: item(ResourceType::Artists, &r.id),
                ..tile_of(
                    TileKind::Artist,
                    &a.name,
                    Some(Route::Artist { id: r.id.clone() }),
                )
            }
        }
        Resource::Song(s) => {
            let row = song_row(s)?;
            track_tile(TileKind::Song, row, Route::Song { id: s.id.clone() })
        }
        Resource::MusicVideo(v) => {
            let row = video_row(v)?;
            track_tile(
                TileKind::MusicVideo,
                row,
                Route::MusicVideo { id: v.id.clone() },
            )
        }
        Resource::AppleCurator(r) | Resource::Curator(r) => {
            let a = r.attributes.as_ref()?;
            Tile {
                subtitle: non_empty(a.show_host_name.as_ref()),
                artwork: artwork(a.artwork.as_ref())
                    .or_else(|| artwork(a.editorial_artwork.values().next())),
                ..tile_of(
                    TileKind::Curator,
                    &a.name,
                    Some(Route::Curator { id: r.id.clone() }),
                )
            }
        }
        Resource::RecordLabel(r) => {
            let a = r.attributes.as_ref()?;
            Tile {
                artwork: artwork(a.artwork.as_ref()),
                ..tile_of(TileKind::RecordLabel, &a.name, None)
            }
        }
        Resource::EditorialItem(r) => {
            let a = r.attributes.as_ref()?;
            let notes = a.editorial_notes.as_ref();
            let title = notes
                .and_then(|n| n.name.as_deref().or(n.short.as_deref()))
                .map(plain)
                .or_else(|| a.link.as_ref().map(|l| l.label.clone()))
                .filter(|t| !t.is_empty())?;
            let url = a.link.as_ref().map(|l| &l.url).or(a.url.as_ref());
            Tile {
                artwork: resource.artwork().and_then(|art| artwork(Some(art))),
                ..tile_of(
                    TileKind::Category,
                    &title,
                    url.and_then(|u| Route::from_link(u)),
                )
            }
        }
        Resource::LibraryAlbum(r) => {
            let a = r.attributes.as_ref()?;
            let id = r.id.clone();
            Tile {
                subtitle: non_empty(a.artist_name.as_ref()),
                artwork: artwork(a.artwork.as_ref()),
                explicit: explicit(a.content_rating.as_ref()),
                play: Some(PlayTarget::LibraryAlbum { id: id.clone() }),
                item: item(ResourceType::LibraryAlbums, &id),
                ..tile_of(TileKind::Album, &a.name, Some(Route::LibraryAlbum { id }))
            }
        }
        Resource::LibraryPlaylist(r) => {
            let a = r.attributes.as_ref()?;
            let id = r.id.clone();
            Tile {
                artwork: artwork(a.artwork.as_ref()),
                play: Some(PlayTarget::LibraryPlaylist { id: id.clone() }),
                item: item(ResourceType::LibraryPlaylists, &id),
                ..tile_of(
                    TileKind::Playlist,
                    &a.name,
                    Some(Route::LibraryPlaylist { id }),
                )
            }
        }
        Resource::LibraryArtist(r) => {
            let a = r.attributes.as_ref()?;
            let catalog = r
                .relationships
                .catalog
                .as_ref()
                .and_then(|p| p.data.first());
            let catalog_art = catalog
                .and_then(|c| c.attributes.as_ref())
                .and_then(|c| artwork(c.artwork.as_ref()));
            Tile {
                artwork: catalog_art,
                item: item(ResourceType::LibraryArtists, &r.id),
                ..tile_of(
                    TileKind::Artist,
                    &a.name,
                    catalog.map(|c| Route::Artist { id: c.id.clone() }),
                )
            }
        }
        Resource::LibrarySong(_) | Resource::LibraryMusicVideo(_) => {
            let row = track(resource)?;
            let kind = match row.kind {
                TrackKind::Song => TileKind::Song,
                TrackKind::MusicVideo => TileKind::MusicVideo,
            };
            let route = row.album.as_ref().and_then(|a| a.route.clone());
            let mut tile = track_tile(kind, row, route?);
            tile.item = tile.play.as_ref().and_then(|p| match p {
                PlayTarget::Track(row) => row.library_ref(),
                _ => None,
            });
            tile
        }
        Resource::Genre(_)
        | Resource::EditorialElement(_)
        | Resource::Room(_)
        | Resource::Grouping(_)
        | Resource::Recommendation(_)
        | Resource::UploadedVideo(_)
        | Resource::LibraryPlaylistFolder(_)
        | Resource::Unknown { .. } => return None,
    })
}

fn track_tile(kind: TileKind, row: TrackRow, route: Route) -> Tile {
    Tile {
        subtitle: Some(row.artist.text.clone()).filter(|s| !s.is_empty()),
        artwork: row.artwork.clone(),
        explicit: row.explicit,
        item: row.catalog_ref(),
        play: row
            .is_playable()
            .then(|| PlayTarget::Track(Box::new(row.clone()))),
        ..tile_of(kind, &row.title, Some(route))
    }
}

pub(crate) fn tiles(resources: &[Resource]) -> Vec<Tile> {
    resources.iter().filter_map(tile).collect()
}

pub(crate) fn tracks(resources: &[Resource]) -> Vec<TrackRow> {
    resources.iter().filter_map(track).collect()
}

/// A shelf, or `None` when it has nothing to show.
pub(crate) fn shelf(
    id: impl Into<String>,
    title: Option<String>,
    items: ShelfItems,
    see_all: Option<Route>,
) -> Option<Shelf> {
    (!items.is_empty()).then(|| Shelf {
        id: id.into(),
        title,
        items,
        see_all,
    })
}

/// A view ("more by artist", "top songs") as a shelf of tracks or tiles.
pub(crate) fn view_shelf(
    id: &str,
    view: Option<&View>,
    as_tracks: bool,
    see_all: impl FnOnce(&View) -> Option<Route>,
) -> Option<Shelf> {
    let view = view?;
    let items = if as_tracks {
        ShelfItems::Tracks(tracks(&view.data))
    } else {
        ShelfItems::Tiles {
            tiles: tiles(&view.data),
            size: TileSize::Regular,
        }
    };
    shelf(id, view.title().map(str::to_owned), items, see_all(view))
}

/// An editorial, Home or search-landing shelf.
pub(crate) fn editorial_shelf(shelf_in: am::Shelf) -> Option<Shelf> {
    match shelf_in {
        am::Shelf::Featured { id, title, tiles } => {
            let tiles = tiles.iter().filter_map(featured_tile).collect();
            shelf(id, title, ShelfItems::Featured(tiles), None)
        }
        am::Shelf::Grid {
            id,
            title,
            style,
            items,
            room,
        } => {
            let size = match style {
                DisplayStyle::Expanded => TileSize::Large,
                _ => TileSize::Regular,
            };
            let tiles = self::tiles(&items);
            shelf(
                id,
                title,
                ShelfItems::Tiles { tiles, size },
                room.map(|id| Route::Room { id }),
            )
        }
        am::Shelf::Tracks {
            id,
            title,
            items,
            room,
        } => shelf(
            id,
            title,
            ShelfItems::Tracks(tracks(&items)),
            room.map(|id| Route::Room { id }),
        ),
        am::Shelf::Links { id, title, links } => {
            let links = links
                .into_iter()
                .filter(|l| !l.label.is_empty() && !l.url.is_empty())
                .map(|l| LinkItem {
                    target: Route::from_link(&l.url)
                        .map_or(LinkTarget::External(l.url), LinkTarget::Route),
                    label: l.label,
                })
                .collect();
            shelf(id, title, ShelfItems::Links(links), None)
        }
        am::Shelf::Unknown {
            id, title, items, ..
        } => shelf(
            id,
            title,
            ShelfItems::Tiles {
                tiles: tiles(&items),
                size: TileSize::Regular,
            },
            None,
        ),
    }
}

fn featured_tile(t: &am::Tile) -> Option<Tile> {
    let mut tile = tile(&t.item)?;
    tile.badge = t.badge.clone().filter(|b| !b.is_empty());
    if tile.subtitle.is_none() {
        tile.subtitle = t.caption.clone();
    }
    if let Some(art) = artwork(t.artwork.as_ref()) {
        tile.artwork = Some(art);
    }
    Some(tile)
}

pub(crate) fn editorial_shelves(shelves: Vec<am::Shelf>) -> Vec<Shelf> {
    shelves.into_iter().filter_map(editorial_shelf).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editorial_html_becomes_plain_text() {
        assert_eq!(
            plain("Taylor’s <i>folklore</i> &amp; more.<br/>Second line"),
            "Taylor’s folklore & more.\nSecond line"
        );
        assert_eq!(plain("a < b"), "a < b");
    }

    #[test]
    fn album_id_is_read_from_a_song_url() {
        let url = "https://music.apple.com/us/album/abbey-road/1441164426?i=1441164430".to_owned();
        assert_eq!(album_id_in_url(Some(&url)).as_deref(), Some("1441164426"));
        assert_eq!(album_id_in_url(Some(&"https://x/song/1".to_owned())), None);
    }
}
