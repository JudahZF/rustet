//! Context menus (spec §6.2 `ContextMenu`) of track rows, tiles and pages.
//!
//! `rustet_app::pages::actions` says which entries an item has ([`track_menu`], [`tile_menu`]);
//! this module turns them into [`Entry`] lists whose entries carry the [`PageEvent`] they send,
//! so the contents can be tested without a window, and then into the kit's menu items. An
//! entry with a glyph of its own shows it at the row's right, as Apple's menus do.

use std::sync::Arc;

use rustet_ampapi::api::{Rating, ResourceRef, ResourceType};
use rustet_app::pages::actions::{Action, MenuContext, MenuItem, tile_menu, track_menu};
use rustet_app::pages::model::{Tile, TrackRow};
use rustet_app::pages::play::{self, How};
use rustet_app::pages::{LibraryPlaylistPage, Page};
use rustet_app::route::Route;
use rustet_ui::components::MenuItem as KitItem;
use rustet_ui::icons::Icon;

use crate::context::PageContext;
use crate::event::{DownloadRequest, PageEvent, PlayRequest, Sink};
use crate::view::page_play;

/// Playlists listed in the "Add to playlist" sub-menu; the rest are in the picker.
pub const SUBMENU_PLAYLISTS: usize = 10;

/// An entry of a context menu.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Item {
        label: String,
        event: PageEvent,
        icon: Option<Icon>,
    },
    Sub {
        label: String,
        entries: Vec<Entry>,
    },
    Separator,
}

impl Entry {
    fn item(label: &str, event: PageEvent) -> Self {
        Self::Item {
            label: label.to_owned(),
            event,
            icon: None,
        }
    }

    /// The entry with `glyph` at its right.
    fn icon(mut self, glyph: Icon) -> Self {
        if let Self::Item { icon, .. } = &mut self {
            *icon = Some(glyph);
        }
        self
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Item { label, .. } | Self::Sub { label, .. } => Some(label),
            Self::Separator => None,
        }
    }
}

/// The labels of `entries`, separators left out (tests, reports).
pub fn labels(entries: &[Entry]) -> Vec<&str> {
    entries.iter().filter_map(Entry::label).collect()
}

/// The kit's menu items for `entries`; each runs its event through `sink`.
pub fn to_kit(entries: Vec<Entry>, sink: &Sink) -> Vec<KitItem> {
    entries
        .into_iter()
        .map(|entry| match entry {
            Entry::Item { label, event, icon } => {
                let sink = sink.clone();
                let item = KitItem::action(label, move |w, cx| sink(event.clone(), w, cx));
                match icon {
                    Some(icon) => item.icon(icon),
                    None => item,
                }
            }
            Entry::Sub { label, entries } => KitItem::submenu(label, to_kit(entries, sink)),
            Entry::Separator => KitItem::Separator,
        })
        .collect()
}

/// Groups of entries with a separator between the non-empty ones.
fn grouped(groups: impl IntoIterator<Item = Vec<Entry>>) -> Vec<Entry> {
    let mut out = Vec::new();
    for group in groups.into_iter().filter(|g| !g.is_empty()) {
        if !out.is_empty() {
            out.push(Entry::Separator);
        }
        out.extend(group);
    }
    out
}

/// "Add to playlist ▸": a new playlist, the first playlists, then the picker for the rest.
fn add_to_playlist(tracks: Vec<ResourceRef>, ctx: &PageContext) -> Entry {
    let mut entries = vec![Entry::item(
        "New playlist…",
        PageEvent::NewPlaylist {
            tracks: tracks.clone(),
        },
    )];
    if !ctx.playlists.is_empty() {
        entries.push(Entry::Separator);
    }
    entries.extend(ctx.playlists.iter().take(SUBMENU_PLAYLISTS).map(|p| {
        Entry::item(
            &p.name,
            PageEvent::Act(Action::AddToPlaylist {
                playlist_id: p.id.clone(),
                tracks: tracks.clone(),
            }),
        )
    }));
    if ctx.playlists.len() > SUBMENU_PLAYLISTS {
        entries.push(Entry::item(
            "All playlists…",
            PageEvent::PickPlaylist { tracks },
        ));
    }
    Entry::Sub {
        label: "Add to playlist".to_owned(),
        entries,
    }
}

fn remove_confirm(what: &str, item: ResourceRef) -> PageEvent {
    PageEvent::Confirm {
        title: format!("Remove “{what}” from your library?"),
        message: "It also leaves your playlists. Downloads of it stay on this computer.".to_owned(),
        answer: "Remove".to_owned(),
        action: Action::RemoveFromLibrary(item),
        then_back: false,
    }
}

/// One of `rustet_app`'s menu items for `rows` (one row, or a tile's item) as an entry.
fn entry_of(
    item: MenuItem,
    what: &str,
    play: &dyn Fn(How) -> PageEvent,
    ctx: &PageContext,
) -> Entry {
    match item {
        MenuItem::PlayNext => Entry::item("Play next", play(How::Next)),
        MenuItem::PlayLater => Entry::item("Play later", play(How::Later)),
        MenuItem::AddToLibrary(r) => Entry::item(
            "Add to library",
            PageEvent::Act(Action::AddToLibrary(vec![r])),
        )
        .icon(Icon::Plus),
        MenuItem::RemoveFromLibrary(r) => {
            Entry::item("Remove from library", remove_confirm(what, r))
        }
        MenuItem::AddToPlaylist(r) => add_to_playlist(vec![r], ctx),
        MenuItem::Love(item) => Entry::item(
            "Love",
            PageEvent::Act(Action::Rate {
                item,
                rating: Some(Rating::Love),
            }),
        )
        .icon(Icon::Heart),
        MenuItem::Dislike(item) => Entry::item(
            "Dislike",
            PageEvent::Act(Action::Rate {
                item,
                rating: Some(Rating::Dislike),
            }),
        ),
        MenuItem::GoToAlbum(route) => {
            Entry::item("Go to album", PageEvent::Navigate(route)).icon(Icon::Album)
        }
        MenuItem::GoToArtist(route) => {
            Entry::item("Go to artist", PageEvent::Navigate(route)).icon(Icon::Artist)
        }
        MenuItem::Download(r) => {
            Entry::item("Download", PageEvent::Download(DownloadRequest::Item(r)))
                .icon(Icon::Download)
        }
        MenuItem::ShareLink(link) => Entry::item("Share link", PageEvent::CopyLink(link)),
        MenuItem::RemoveFromPlaylist {
            playlist_id,
            library_song_id,
        } => Entry::item(
            "Remove from playlist",
            PageEvent::Act(Action::RemoveFromPlaylist {
                id: playlist_id,
                library_song_id,
            }),
        ),
    }
}

/// Which group an entry belongs to: play, library, go to, share, playlist edits.
fn group_of(item: &MenuItem) -> usize {
    match item {
        MenuItem::PlayNext | MenuItem::PlayLater => 0,
        MenuItem::AddToLibrary(_)
        | MenuItem::RemoveFromLibrary(_)
        | MenuItem::AddToPlaylist(_)
        | MenuItem::Love(_)
        | MenuItem::Dislike(_) => 1,
        MenuItem::GoToAlbum(_) | MenuItem::GoToArtist(_) => 2,
        MenuItem::Download(_) | MenuItem::ShareLink(_) => 3,
        MenuItem::RemoveFromPlaylist { .. } => 4,
    }
}

fn group_items(
    items: Vec<MenuItem>,
    what: &str,
    play: &dyn Fn(How) -> PageEvent,
    ctx: &PageContext,
) -> [Vec<Entry>; 5] {
    let mut groups: [Vec<Entry>; 5] = Default::default();
    for item in items {
        let g = group_of(&item);
        groups[g].push(entry_of(item, what, play, ctx));
    }
    groups
}

fn menu_context<'a>(ctx: &'a PageContext, playlist: Option<&'a str>) -> MenuContext<'a> {
    MenuContext {
        signed_in: ctx.signed_in,
        storefront: &ctx.storefront,
        playlist,
    }
}

/// The editable library playlist rows are shown in, with every track loaded: rows can be
/// moved and removed.
#[derive(Clone, Copy)]
pub struct Editing<'a> {
    pub page: &'a LibraryPlaylistPage,
    /// The row's index in the playlist.
    pub index: usize,
}

/// "Move up" and "Move down" for one row of an editable playlist: the keyboard's way to
/// reorder (the mouse drags).
fn move_entries(edit: Editing<'_>) -> Vec<Entry> {
    let order = |to: usize| {
        let rows = edit.page.moved(edit.index, to)?;
        Some(PageEvent::Act(Action::SetPlaylistOrder {
            id: edit.page.id.clone(),
            tracks: rows.iter().filter_map(playlist_ref).collect(),
        }))
    };
    let mut out = Vec::new();
    if let Some(e) = edit.index.checked_sub(1).and_then(order) {
        out.push(Entry::item("Move up", e));
    }
    if let Some(e) = order(edit.index + 1) {
        out.push(Entry::item("Move down", e));
    }
    out
}

/// The resource a playlist edit names a row by: its library song, else its catalog song.
pub fn playlist_ref(row: &TrackRow) -> Option<ResourceRef> {
    row.library_ref().or_else(|| row.catalog_ref())
}

/// The menu of the selected `rows`. One row gets `track_menu`'s entries; several get the
/// entries that make sense for all of them.
pub fn rows_menu(rows: &[TrackRow], ctx: &PageContext, editing: Option<Editing<'_>>) -> Vec<Entry> {
    let playable: Arc<[TrackRow]> = rows.to_vec().into();
    let play =
        move |how: How| PageEvent::Play(PlayRequest::Intent(play::play(&playable, None, how)));
    let playlist = editing.map(|e| e.page.id.as_str());
    if let [row] = rows {
        let items = track_menu(row, menu_context(ctx, playlist));
        let [p, mut library, go, mut share, remove] = group_items(items, &row.title, &play, ctx);
        // The row's own download carries its title for the job list.
        for entry in &mut share {
            if let Entry::Item { label, event, .. } = entry
                && label == "Download"
            {
                *event = PageEvent::Download(DownloadRequest::Tracks(vec![row.clone()]));
            }
        }
        // `track_menu` offers removal of a library song from the library; inside an editable
        // playlist the row's own removal is the one that belongs there.
        if editing.is_some() {
            library.retain(|e| e.label() != Some("Remove from library"));
        }
        let moves = editing.map(move_entries).unwrap_or_default();
        return grouped([p, library, go, share, [moves, remove].concat()]);
    }

    let mut groups: [Vec<Entry>; 4] = Default::default();
    if rows.iter().any(TrackRow::is_playable) {
        groups[0] = vec![
            Entry::item("Play next", play(How::Next)),
            Entry::item("Play later", play(How::Later)),
        ];
    }
    if ctx.signed_in {
        let to_add: Vec<ResourceRef> = rows
            .iter()
            .filter(|r| r.library_ref().is_none())
            .filter_map(TrackRow::catalog_ref)
            .collect();
        if !to_add.is_empty() {
            groups[1].push(
                Entry::item(
                    "Add to library",
                    PageEvent::Act(Action::AddToLibrary(to_add)),
                )
                .icon(Icon::Plus),
            );
        }
        let tracks: Vec<ResourceRef> = rows.iter().filter_map(playlist_ref).collect();
        if !tracks.is_empty() {
            groups[1].push(add_to_playlist(tracks, ctx));
        }
    }
    let downloadable: Vec<TrackRow> = rows.iter().filter(|r| r.is_playable()).cloned().collect();
    if !downloadable.is_empty() {
        groups[2].push(
            Entry::item(
                "Download",
                PageEvent::Download(DownloadRequest::Tracks(downloadable)),
            )
            .icon(Icon::Download),
        );
    }
    if let (true, Some(edit)) = (ctx.signed_in, editing) {
        let removals: Vec<Action> = rows
            .iter()
            .filter_map(|r| r.id.library_id())
            .map(|lid| Action::RemoveFromPlaylist {
                id: edit.page.id.clone(),
                library_song_id: lid.to_owned(),
            })
            .collect();
        if !removals.is_empty() {
            groups[3].push(Entry::item(
                "Remove from playlist",
                PageEvent::ActAll(removals),
            ));
        }
    }
    grouped(groups)
}

/// The menu of a tile.
pub fn tile_entries(tile: &Tile, ctx: &PageContext) -> Vec<Entry> {
    let target = tile.play.clone();
    let play = move |how: How| match &target {
        Some(t) => PageEvent::Play(PlayRequest::Target(t.clone(), how)),
        None => PageEvent::Play(PlayRequest::Intent(Err(play::PlayError::NothingPlayable))),
    };
    let items = tile_menu(tile, menu_context(ctx, None));
    let [p, library, go, share, remove] = group_items(items, &tile.title, &play, ctx);
    grouped([p, library, go, share, remove])
}

fn catalog_item(route: &Route) -> Option<ResourceRef> {
    let (kind, id) = match route {
        Route::Album { id } => (ResourceType::Albums, id),
        Route::Playlist { id } => (ResourceType::Playlists, id),
        Route::Song { id } => (ResourceType::Songs, id),
        Route::MusicVideo { id } => (ResourceType::MusicVideos, id),
        _ => return None,
    };
    Some(ResourceRef::new(kind, id.clone()))
}

/// The resource "Add to library" in a page's header adds, when the page has one.
pub fn page_item(page: &Page) -> Option<ResourceRef> {
    let route = match page {
        Page::Album(p) => Route::Album { id: p.id.clone() },
        Page::Playlist(p) => Route::Playlist { id: p.id.clone() },
        Page::Song(p) => Route::Song { id: p.id.clone() },
        Page::MusicVideo(p) => Route::MusicVideo { id: p.id.clone() },
        _ => return None,
    };
    catalog_item(&route)
}

/// Whether the page's own item can be downloaded from its header.
pub fn page_downloadable(page: &Page) -> bool {
    match page {
        Page::Album(_) | Page::Playlist(_) | Page::LibraryPlaylist(_) => true,
        Page::LibraryAlbum(p) => p.catalog.is_some(),
        Page::Song(p) => p.track.is_playable(),
        Page::MusicVideo(p) => p.video.is_playable(),
        _ => false,
    }
}

/// The menu of a page's "More" button.
pub fn page_entries(page: &Arc<Page>, route: &Route, ctx: &PageContext) -> Vec<Entry> {
    let mut playing = Vec::new();
    for (label, how) in [("Play next", How::Next), ("Play later", How::Later)] {
        if let Some(intent) = page_play(page, how).filter(Result::is_ok) {
            playing.push(Entry::item(
                label,
                PageEvent::Play(PlayRequest::Intent(intent)),
            ));
        }
    }
    let mut library = Vec::new();
    let mut go = Vec::new();
    let mut edit = Vec::new();
    match &**page {
        Page::Song(p) => {
            if ctx.signed_in
                && let Some(r) = p.track.catalog_ref()
            {
                library.push(add_to_playlist(vec![r], ctx));
            }
            go.extend(
                p.album
                    .as_ref()
                    .and_then(|a| a.route.clone())
                    .map(|r| Entry::item("Go to album", PageEvent::Navigate(r)).icon(Icon::Album)),
            );
        }
        Page::LibraryAlbum(p) => {
            go.extend(
                p.catalog
                    .clone()
                    .map(|r| Entry::item("Show in Apple Music", PageEvent::Navigate(r))),
            );
            if ctx.signed_in {
                library.push(Entry::item(
                    "Remove from library",
                    remove_confirm(
                        &p.header.title,
                        ResourceRef::new(ResourceType::LibraryAlbums, p.id.clone()),
                    ),
                ));
            }
        }
        Page::LibraryPlaylist(p) => {
            go.extend(
                p.catalog
                    .clone()
                    .map(|r| Entry::item("Show in Apple Music", PageEvent::Navigate(r))),
            );
            if ctx.signed_in && p.editable {
                edit.push(Entry::item(
                    "Rename…",
                    PageEvent::RenamePlaylist {
                        id: p.id.clone(),
                        name: p.header.title.clone(),
                    },
                ));
                edit.push(Entry::item(
                    "Delete playlist…",
                    PageEvent::Confirm {
                        title: format!("Delete “{}”?", p.header.title),
                        message: "The playlist leaves your library on every device. Its songs stay in your library."
                            .to_owned(),
                        answer: "Delete".to_owned(),
                        action: Action::DeletePlaylist { id: p.id.clone() },
                        then_back: true,
                    },
                ));
            }
        }
        _ => {}
    }
    if ctx.signed_in
        && let Some(item) = page_item(page)
    {
        library.insert(
            0,
            Entry::item(
                "Add to library",
                PageEvent::Act(Action::AddToLibrary(vec![item])),
            )
            .icon(Icon::Plus),
        );
    }
    let subtitle_route = match &**page {
        Page::Album(p) => p.header.subtitle.as_ref().and_then(|s| s.route.clone()),
        Page::Song(p) => p.track.artist.route.clone(),
        Page::MusicVideo(p) => p.video.artist.route.clone(),
        Page::LibraryAlbum(p) => p.header.subtitle.as_ref().and_then(|s| s.route.clone()),
        _ => None,
    };
    go.extend(
        subtitle_route
            .map(|r| Entry::item("Go to artist", PageEvent::Navigate(r)).icon(Icon::Artist)),
    );
    let mut share = Vec::new();
    if page_downloadable(page) {
        share.push(
            Entry::item(
                "Download",
                PageEvent::Download(DownloadRequest::Page(page.clone())),
            )
            .icon(Icon::Download),
        );
    }
    share.extend(
        route
            .share_link(&ctx.storefront)
            .map(|l| Entry::item("Share link", PageEvent::CopyLink(l))),
    );
    grouped([playing, library, go, share, edit])
}
