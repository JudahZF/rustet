//! The rustet-ui gallery: every component in dark and light, and the Album, Artist, Search and
//! New pages composed from the content components over real recorded data (`rustet-app`'s
//! fixtures through `rustet_app`'s own loaders), plus the 5,000-tile and 10,000-row lists for
//! frame timing.
//!
//!     cargo run --release -p rustet-ui --example gallery -- [--light] [--scenario tour|bench|a11y|input]
//!         [--catalog /tmp/rustet-ui-data/catalog.json] [--state-dir DIR] [--ssd]
//!
//! With `--scenario` it drives itself (`scenario.rs`) and prints `REPORT {json}` lines. Run it
//! inside `scripts/headless.sh`, never on a desktop session.

mod fixtures;
mod scenario;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, KeyBinding, Role, ScrollHandle,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowDecorations, WindowOptions, actions,
    div, prelude::*, px, size,
};
use rustet_app::pages::catalog::{Hint, SearchGroup};
use rustet_app::pages::model::{EmptyState, Listing, Shelf, ShelfItems, Tile, TileKind, TileSize};
use rustet_app::pages::{ErrorState, LoadError, Recovery};
use rustet_app::route::Route;
use rustet_player::model::{Codec, Quality, QualityTier};
use rustet_ui::artwork::{ArtworkStore, DiskCache, StoreConfig};
use rustet_ui::components::{
    Button, Circle, CodeInput, ContextMenu, Dialog, EmptyStateView, ErrorStateView, FocusRing,
    IconButton, MenuItem, Placeholder, Popover, Scrollbar, SearchField, SearchFieldEvent, Select,
    Sheet, Slider, Suggestion, TextInput, Toast, Toasts, Toggle,
};
use rustet_ui::content::artwork::Shape;
use rustet_ui::content::{
    ArtworkView, ContentEvent, HeaderAction, LyricsEvent, LyricsView, OnContent, PageHeader,
    QualityStrip, ShelfView, TileGrid, TrackListOptions, TrackListView, quality_badge,
    quality_mark,
};
use rustet_ui::frame::{FrameParts, Layout, app_frame};
use rustet_ui::frame_stats::FrameStats;
use rustet_ui::icons::{Assets, Icon};
use rustet_ui::theme::{ActiveTheme, Appearance, BOLD, REGULAR, SEMIBOLD, Theme};
use serde_json::json;

actions!(gallery, [FocusSearch, Quit, NextFocus, PreviousFocus]);

/// Prints one machine-readable report line.
pub fn report(kind: &str, value: serde_json::Value) {
    println!("REPORT {}", json!({ "kind": kind, "value": value }));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Components,
    Album,
    Artist,
    Search,
    New,
    Lyrics,
    Lists,
}

impl Section {
    pub const ALL: [Section; 7] = [
        Section::Components,
        Section::Album,
        Section::Artist,
        Section::Search,
        Section::New,
        Section::Lyrics,
        Section::Lists,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Components => "Components",
            Section::Album => "Album",
            Section::Artist => "Artist",
            Section::Search => "Search",
            Section::New => "New",
            Section::Lyrics => "Lyrics and quality",
            Section::Lists => "Long lists",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Section::Components => Icon::Settings,
            Section::Album => Icon::Album,
            Section::Artist => Icon::Artist,
            Section::Search => Icon::Search,
            Section::New => Icon::New,
            Section::Lyrics => Icon::Lyrics,
            Section::Lists => Icon::Library,
        }
    }
}

pub struct Args {
    pub catalog: PathBuf,
    pub scenario: Option<String>,
    pub state_dir: PathBuf,
    pub light: bool,
    pub ssd: bool,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut out = Args {
        catalog: PathBuf::from("/tmp/rustet-ui-data/catalog.json"),
        scenario: None,
        state_dir: std::env::temp_dir().join("rustet-ui-gallery"),
        light: false,
        ssd: false,
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--catalog" => out.catalog = args.next().expect("--catalog PATH").into(),
            "--scenario" => out.scenario = args.next(),
            "--state-dir" => out.state_dir = args.next().expect("--state-dir DIR").into(),
            "--light" => out.light = true,
            "--ssd" => out.ssd = true,
            other => panic!("unknown argument {other}"),
        }
    }
    out
}

/// The streams the quality strip cycles through.
pub fn qualities() -> [Quality; 3] {
    let base = Quality {
        codec: Codec::Alac,
        tier: QualityTier::HiResLossless,
        bit_depth: Some(24),
        sample_rate: Some(96_000),
        bitrate: None,
        channels: Some(2),
        spatial: None,
        video_height: None,
        fell_back: false,
    };
    [
        base,
        Quality {
            tier: QualityTier::Lossless,
            bit_depth: Some(16),
            sample_rate: Some(44_100),
            ..base
        },
        Quality {
            codec: Codec::Aac,
            tier: QualityTier::Aac,
            bit_depth: None,
            sample_rate: None,
            bitrate: Some(256_000),
            fell_back: true,
            ..base
        },
    ]
}

pub struct Gallery {
    pub section: Section,
    focus: FocusHandle,
    pages: Option<Rc<fixtures::Pages>>,
    pub search: Entity<SearchField>,
    input: Entity<TextInput>,
    select: Entity<Select>,
    slider: Entity<Slider>,
    pub code: Entity<CodeInput>,
    pub toasts: Entity<Toasts>,
    popover: Popover,
    toggles: [bool; 3],
    pub sheet_open: bool,
    pub dialog_open: bool,
    album_tracks: Option<Entity<TrackListView>>,
    pub lyrics: Entity<LyricsView>,
    pub quality: Entity<QualityStrip>,
    pub quality_ix: usize,
    pub grid: Option<Entity<TileGrid>>,
    pub big_list: Option<Entity<TrackListView>>,
    pub lists_tab_rows: bool,
    pub scroll: ScrollHandle,
    pub frame_stats: FrameStats,
    pub autoscroll: Option<f32>,
    pub started: Instant,
    first_frame: bool,
    _subscriptions: Vec<gpui::Subscription>,
}

impl AsMut<Popover> for Gallery {
    fn as_mut(&mut self) -> &mut Popover {
        &mut self.popover
    }
}

impl Gallery {
    fn new(args: &Args, started: Instant, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| SearchField::new("Artists, songs, lyrics and more", cx));
        let input = cx.new(|cx| TextInput::new("Playlist name", cx));
        let select = cx.new(|cx| {
            Select::new(
                "Streaming quality",
                vec![
                    "High quality (AAC 256 kbps)".into(),
                    "Lossless (up to 24-bit 48 kHz)".into(),
                    "Hi-Res Lossless (up to 24-bit 192 kHz)".into(),
                ],
                1,
                cx,
            )
        });
        let slider = cx.new(|cx| Slider::new(0.6, "Volume", cx));
        let code = cx.new(CodeInput::new);
        let toasts = cx.new(|_| Toasts::default());
        let lyrics = cx.new(|cx| LyricsView::new(fixtures::lyrics(), cx));
        let quality = cx.new(|cx| {
            let mut q = QualityStrip::new();
            q.set(Some(qualities()[0]), cx);
            q
        });

        let bench = fixtures::bench(&args.catalog);
        let (grid, big_list) = match bench {
            Some((tiles, rows)) => (
                Some(cx.new(|_| TileGrid::new("Albums", tiles.into()))),
                Some(cx.new(|cx| TrackListView::new(rows.into(), TrackListOptions::default(), cx))),
            ),
            None => (None, None),
        };

        let mut subscriptions = vec![
            cx.subscribe_in(&search, window, |this, _, e: &SearchFieldEvent, _, cx| {
                report("search", json!(format!("{e:?}")));
                if let SearchFieldEvent::Query(q) = e {
                    // The hints the fixtures answer for "phoebe", for any text: the field drops
                    // answers for text it no longer shows.
                    let suggestions = this
                        .pages
                        .as_ref()
                        .map(|p| hints_to_suggestions(&p.hints))
                        .unwrap_or_default();
                    let q = q.clone();
                    this.search
                        .update(cx, |s, cx| s.set_suggestions(&q, suggestions, cx));
                }
            }),
            cx.subscribe(&lyrics, |_, _, e: &LyricsEvent, _| {
                report("lyrics", json!(format!("{e:?}")))
            }),
        ];
        if let Some(grid) = &grid {
            subscriptions.push(cx.subscribe_in(
                grid,
                window,
                |this, _, e: &ContentEvent, w, cx| this.on_content(e, w, cx),
            ));
        }
        if let Some(list) = &big_list {
            subscriptions.push(cx.subscribe_in(
                list,
                window,
                |this, _, e: &ContentEvent, w, cx| this.on_content(e, w, cx),
            ));
        }

        // Load the pages through rustet_app's loaders, off the UI thread.
        let job = rustet_ui::runtime::spawn(cx, fixtures::load());
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(pages)) = job.await {
                this.update_in(cx, |this, window, cx| this.pages_loaded(pages, window, cx))
                    .ok();
            }
        })
        .detach();

        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            section: Section::Components,
            focus,
            pages: None,
            search,
            input,
            select,
            slider,
            code,
            toasts,
            popover: Popover::default(),
            toggles: [true, false, true],
            sheet_open: false,
            dialog_open: false,
            album_tracks: None,
            lyrics,
            quality,
            quality_ix: 0,
            grid,
            big_list,
            lists_tab_rows: false,
            scroll: ScrollHandle::new(),
            frame_stats: FrameStats::default(),
            autoscroll: None,
            started,
            first_frame: true,
            _subscriptions: subscriptions,
        }
    }

    fn pages_loaded(
        &mut self,
        pages: fixtures::Pages,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        report(
            "pages",
            json!({
                "album": pages.album.as_ref().map(|p| p.header.title.clone()).map_err(|e| format!("{e:?}")),
                "artist": pages.artist.as_ref().map(|p| p.header.title.clone()).map_err(|e| format!("{e:?}")),
                "search": pages.search.as_ref().map(|p| p.sections.len()).map_err(|e| format!("{e:?}")),
                "new": pages.new.as_ref().map(|p| p.shelves.len()).map_err(|e| format!("{e:?}")),
                "hints": pages.hints.len(),
            }),
        );
        if let Ok(album) = &pages.album {
            let rows: Arc<[_]> = album.tracks().cloned().collect();
            let list = cx.new(|cx| {
                TrackListView::new(
                    rows,
                    TrackListOptions {
                        label: "Tracks".into(),
                        artwork: false,
                        album: false,
                        fit_content: true,
                        album_artist: album
                            .header
                            .subtitle
                            .as_ref()
                            .map(|s| s.text.clone().into()),
                    },
                    cx,
                )
            });
            self._subscriptions.push(cx.subscribe_in(
                &list,
                window,
                |this, _, e: &ContentEvent, w, cx| this.on_content(e, w, cx),
            ));
            self.album_tracks = Some(list);
        }
        self.pages = Some(Rc::new(pages));
        cx.notify();
    }

    /// What a content component asked for: reported, and confirmed with a toast.
    pub fn on_content(&mut self, e: &ContentEvent, window: &mut Window, cx: &mut Context<Self>) {
        let summary = match e {
            ContentEvent::Navigate(r) => format!("Open {}", r.to_link()),
            ContentEvent::OpenExternal(u) => format!("Open {u} in the browser"),
            ContentEvent::Play(_) => "Play".to_owned(),
            ContentEvent::PlayTracks { rows, start } => format!("Play “{}”", rows[*start].title),
            ContentEvent::TileMenu { tile, position } => {
                self.open_menu(*position, Some(tile.title.clone()), window, cx);
                format!("Menu for “{}”", tile.title)
            }
            ContentEvent::TracksMenu { rows, position } => {
                self.open_menu(*position, None, window, cx);
                format!("Menu for {} song(s)", rows.len())
            }
        };
        report("content", json!(summary));
        if !matches!(
            e,
            ContentEvent::TileMenu { .. } | ContentEvent::TracksMenu { .. }
        ) {
            self.toasts
                .update(cx, |t, cx| t.push(Toast::new(summary), cx));
        }
    }

    /// The track and tile menu of spec §6.2, with the playlist sub-menu.
    pub fn open_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        _title: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let toasts = self.toasts.clone();
        let say = move |text: &'static str| {
            let toasts = toasts.clone();
            move |_: &mut Window, cx: &mut App| {
                report("menu", json!(text));
                toasts.update(cx, |t, cx| t.push(Toast::new(text), cx));
            }
        };
        let items = vec![
            MenuItem::action("Play next", say("Playing next")),
            MenuItem::action("Play later", say("Playing later")),
            MenuItem::Separator,
            MenuItem::action("Add to library", say("Added to library")),
            MenuItem::submenu(
                "Add to playlist",
                vec![
                    MenuItem::action("New playlist…", say("New playlist")),
                    MenuItem::Separator,
                    MenuItem::action("Late night", say("Added to Late night")),
                    MenuItem::action("Running", say("Added to Running")),
                    MenuItem::action("夜の電車", say("Added to 夜の電車")),
                ],
            ),
            MenuItem::Separator,
            MenuItem::action("Love", say("Loved")),
            MenuItem::action("Dislike", say("Disliked")),
            MenuItem::Separator,
            MenuItem::action("Go to album", say("Opening the album")),
            MenuItem::action("Go to artist", say("Opening the artist")),
            MenuItem::action("Download", say("Downloading")).disabled(),
            MenuItem::action("Share link", say("Link copied")),
        ];
        let menu = ContextMenu::new(items, window, cx);
        self.popover.show(menu, position, cx);
    }

    pub fn close_popover(&mut self, cx: &mut Context<Self>) {
        self.popover.close();
        cx.notify();
    }

    pub fn set_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        // Album and artist pages take their artwork's colour; the others are neutral.
        let palette = self.pages.as_ref().and_then(|p| match section {
            Section::Album => p.album.as_ref().ok()?.header.palette(),
            Section::Artist => p.artist.as_ref().ok()?.header.palette(),
            _ => None,
        });
        rustet_ui::theme::set_page_tint(palette.as_ref(), cx);
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        if section == Section::Lyrics {
            self.lyrics.update(cx, |l, cx| {
                l.set_position(Duration::from_secs(4), Instant::now(), true, cx)
            });
        } else {
            self.lyrics.update(cx, |l, cx| {
                l.set_position(Duration::from_secs(4), Instant::now(), false, cx)
            });
        }
        report("section", json!(section.label()));
        cx.notify();
    }

    pub fn next_quality(&mut self, cx: &mut Context<Self>) {
        self.quality_ix = (self.quality_ix + 1) % 3;
        let q = qualities()[self.quality_ix];
        self.quality.update(cx, |s, cx| s.set(Some(q), cx));
    }

    pub fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        Theme::new(appearance).apply(cx);
        report("appearance", json!(format!("{appearance:?}")));
    }

    fn on_event(&self, cx: &mut Context<Self>) -> OnContent {
        let this = cx.entity().downgrade();
        Rc::new(move |e, w, cx| {
            this.update(cx, |this, cx| this.on_content(e, w, cx)).ok();
        })
    }
}

fn hints_to_suggestions(hints: &[Hint]) -> Vec<Suggestion> {
    hints
        .iter()
        .map(|h| match h {
            Hint::Term { display, .. } => Suggestion {
                title: display.clone().into(),
                subtitle: None,
                artwork: None,
            },
            Hint::Item(tile) => Suggestion {
                title: tile.title.clone().into(),
                subtitle: tile.subtitle.clone().map(Into::into),
                artwork: tile.artwork.clone(),
            },
        })
        .collect()
}

/// A titled block of the components page.
fn block(theme: &Theme, title: &'static str, body: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(theme.space.m)
        .child(
            div()
                .text_size(theme.text.title2)
                .font_weight(BOLD)
                .child(title),
        )
        .child(body)
        .into_any_element()
}

impl Gallery {
    /// The sidebar in the look of the brief: the "Rustet" wordmark, the sections as navigation
    /// items, and the signed-out account control (a full-width key pill) at the bottom.
    fn sidebar(&self, layout: Layout, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let full = layout == Layout::Full;
        div()
            .id("sidebar")
            .role(Role::Navigation)
            .aria_label("Gallery")
            .w(layout.sidebar_width(&theme))
            .h_full()
            .flex()
            .flex_col()
            .pt(theme.size.wordmark_top)
            .pb(theme.space.l)
            .child(
                div()
                    .h(theme.size.nav_top - theme.size.wordmark_top)
                    .pl(theme.size.wordmark_left)
                    .text_size(theme.text.title1)
                    .font_weight(BOLD)
                    .text_color(colors.text)
                    .when(full, |d| d.child("Rustet")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.size.nav_gap)
                    .pl(theme.size.nav_inset_left)
                    .pr(theme.size.nav_inset_right)
                    .children(Section::ALL.iter().map(|&section| {
                        let active = section == self.section;
                        div()
                            .id(section.label())
                            .role(Role::Tab)
                            .aria_label(section.label())
                            .aria_selected(active)
                            .tab_index(0)
                            .focus_ring_on(colors.sidebar, cx)
                            .h(theme.size.nav_item)
                            .px(theme.size.nav_icon_pad)
                            .flex()
                            .items_center()
                            .gap(theme.space.s)
                            .rounded(theme.radius.card)
                            .text_size(theme.text.title3)
                            .text_color(colors.text)
                            .when(active, |d| d.bg(colors.selected).font_weight(SEMIBOLD))
                            .when(!active, |d| d.hover(|s| s.bg(colors.selected)))
                            .cursor_pointer()
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.set_section(section, cx)),
                            )
                            .child(
                                section
                                    .icon()
                                    .element()
                                    .size(theme.size.icon)
                                    .text_color(colors.nav_icon),
                            )
                            .when(full, |d| d.child(section.label()))
                    })),
            )
            .child(div().flex_1())
            .when(full, |d| {
                d.child(
                    div()
                        .mx(theme.space.m)
                        .pt(theme.space.m)
                        .border_t(theme.size.hairline)
                        .border_color(colors.sidebar_border)
                        .child(
                            Button::new("sign-in", "Sign In")
                                .primary()
                                .large()
                                .full_width()
                                .icon(Icon::Person),
                        ),
                )
            })
            .into_any_element()
    }

    /// The player pill's content, as the shell is to draw it: transport at the left, the
    /// "LCD" (artwork, title, artist and album, progress line) in the middle, lyrics, queue
    /// and volume at the right.
    fn player(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let now = self.pages.as_ref().and_then(|p| p.album.as_ref().ok());
        let track = now.and_then(|a| a.tracks().next());
        let lcd = div()
            .id("lcd")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap(theme.space.s)
            .px(theme.space.s)
            .child(
                ArtworkView::new(
                    now.and_then(|a| a.header.artwork.clone()),
                    theme.size.art_row,
                )
                .shape(Shape::Thumb),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .child(
                        div()
                            .max_w_full()
                            .truncate()
                            .text_size(theme.text.body)
                            .font_weight(SEMIBOLD)
                            .child(
                                track
                                    .map(|t| t.title.clone())
                                    .unwrap_or_else(|| "Nothing playing".into()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.space.s6)
                            .max_w_full()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.text.callout)
                                    .text_color(colors.text_secondary)
                                    .child(
                                        now.map(|a| {
                                            format!(
                                                "{} — {}",
                                                a.header
                                                    .subtitle
                                                    .clone()
                                                    .map(|s| s.text)
                                                    .unwrap_or_default(),
                                                a.header.title
                                            )
                                        })
                                        .unwrap_or_default(),
                                    ),
                            )
                            .children(
                                quality_badge(&qualities()[self.quality_ix])
                                    .map(|b| quality_mark(&theme, b)),
                            ),
                    ),
            )
            // The progress line along the LCD's bottom edge.
            .child(
                div()
                    .absolute()
                    .left(theme.space.l)
                    .right(theme.space.l)
                    .bottom(theme.space.xs)
                    .h(theme.size.progress)
                    .rounded_full()
                    .bg(colors.fill)
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(0.32))
                            .rounded_full()
                            .bg(colors.text_secondary),
                    ),
            );
        div()
            .id("player-bar")
            .role(Role::Toolbar)
            .aria_label("Player")
            .size_full()
            .flex()
            .items_center()
            .gap(theme.space.s)
            .px(theme.space.m)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme.space.xs)
                    .child(
                        IconButton::new("p-shuffle", Icon::Shuffle, "Shuffle")
                            .selected(false)
                            .tooltip_above(),
                    )
                    .child(IconButton::new("p-prev", Icon::Previous, "Previous").tooltip_above())
                    .child(
                        IconButton::new("p-play", Icon::Play, "Play")
                            .large()
                            .tooltip_above(),
                    )
                    .child(IconButton::new("p-next", Icon::Next, "Next").tooltip_above())
                    .child(
                        IconButton::new("p-repeat", Icon::Repeat, "Repeat")
                            .selected(true)
                            .tooltip_above(),
                    ),
            )
            .child(lcd)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(theme.space.xs)
                    .child(
                        IconButton::new("p-lyrics", Icon::Lyrics, "Lyrics")
                            .selected(self.section == Section::Lyrics)
                            .tooltip_above(),
                    )
                    .child(
                        IconButton::new("p-queue", Icon::Queue, "Playing Next")
                            .selected(false)
                            .tooltip_above(),
                    )
                    .child(IconButton::new("p-volume", Icon::Volume, "Volume").tooltip_above()),
            )
            .into_any_element()
    }
}

impl Gallery {
    fn components_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let entity = cx.entity();
        let swatches = [
            ("Base", colors.base),
            ("Sidebar", colors.sidebar),
            ("Text", colors.text),
            ("Secondary", colors.text_secondary),
            ("Tertiary", colors.text_tertiary),
            ("Fill", colors.fill),
            ("Key", colors.key),
            ("Glass", colors.glass),
            ("Nav icon", colors.nav_icon),
            ("Focus", colors.focus),
            ("Switch", colors.switch_on),
            ("Live", colors.live),
        ];
        let toggles = self.toggles;
        let toggle = |ix: usize, label: &'static str, desc: &'static str| {
            let entity = entity.clone();
            Toggle::new(("toggle", ix), label, toggles[ix])
                .description(desc)
                .on_change(move |on, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.toggles[ix] = on;
                        cx.notify();
                    })
                })
        };
        let open_sheet = entity.clone();
        let open_dialog = entity.clone();
        let toast = self.toasts.clone();
        let menu = entity.clone();
        let samples = [
            "Sigur Rós, Ágætis byrjun (Remastered Deluxe Edition)",
            "宇多田ヒカル — 初恋 / 坂本龍一「戦場のメリークリスマス」",
            "아이유 (IU) — 꽃갈피 둘 (A flower bookmark 2)",
            "فيروز — كيفك إنت (من ألبوم كيفك إنت، ريماستر ٢٠٢٤)",
            "עומר אדם — שיר אהבה בגרסה אקוסטית מיוחדת",
            "अरिजीत सिंह — तुम ही हो (आशिकी 2) [Original Motion Picture Soundtrack]",
            "ลิซ่า — LALISA (Deluxe Edition) ธงไชย แมคอินไตย์",
            "Coldplay — Music of the Spheres 🌍🪐 👩‍🎤 🇯🇵",
        ];
        let type_sample = |size, weight, text: &'static str| {
            div().text_size(size).font_weight(weight).child(text)
        };
        page(&theme)
            .gap(theme.space.xxl)
            .child(page_title(&theme, "Components"))
            .child(block(
                &theme,
                "Colour roles",
                div()
                    .flex()
                    .flex_wrap()
                    .gap(theme.space.m)
                    .children(swatches.into_iter().map(|(name, c)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.space.xs)
                            .child(
                                div()
                                    .size(theme.size.key_circle)
                                    .rounded(theme.radius.card)
                                    .bg(c)
                                    .border(theme.size.hairline)
                                    .border_color(colors.divider),
                            )
                            .child(
                                div()
                                    .text_size(theme.text.callout)
                                    .text_color(colors.text_secondary)
                                    .child(name),
                            )
                    })),
            ))
            .child(block(
                &theme,
                "Type",
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.s)
                    .child(type_sample(theme.text.page_title, BOLD, "Page title, 34"))
                    .child(type_sample(theme.text.large_title, BOLD, "Large title, 26"))
                    .child(type_sample(theme.text.title1, BOLD, "Sheet title, 22"))
                    .child(type_sample(theme.text.title2, BOLD, "Shelf head, 17"))
                    .child(type_sample(theme.text.title3, SEMIBOLD, "Sidebar label, 15"))
                    .child(type_sample(
                        theme.text.body,
                        REGULAR,
                        "Body, track titles and menus, 13. The quick brown fox jumps over the lazy dog.",
                    ))
                    .child(
                        div()
                            .text_size(theme.text.callout)
                            .text_color(colors.text_secondary)
                            .child("Callout, tiles and artist lines, 12"),
                    )
                    .child(
                        div()
                            .text_size(theme.text.subhead)
                            .font_weight(SEMIBOLD)
                            .text_color(colors.text_secondary)
                            .child("EYEBROW, 11"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.space.l)
                            .text_size(theme.text.body)
                            .font_features(theme.tabular())
                            .child("1:11 4:00 12:48")
                            .child(quality_mark(&theme, "Hi-Res Lossless"))
                            .child(
                                div()
                                    .text_color(colors.text_secondary)
                                    .child("ALAC 24-bit 96 kHz"),
                            )
                            .child(rustet_ui::content::tile::explicit_mark(&theme)),
                    ),
            ))
            .child(block(
                &theme,
                "Buttons",
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.m)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(theme.space.m)
                            .child(
                                Button::new("b-play", "Play")
                                    .primary()
                                    .large()
                                    .icon(Icon::Play),
                            )
                            .child(
                                Button::new("b-shuffle", "Shuffle")
                                    .primary()
                                    .large()
                                    .icon(Icon::Shuffle),
                            )
                            .child(Button::new("b-quiet", "Sign Out"))
                            .child(Button::new("b-save", "Save").primary())
                            .child(Button::new("b-disabled", "Download").disabled(true)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(theme.space.m)
                            .child(
                                IconButton::new("b-back", Icon::ChevronLeft, "Back")
                                    .glass(Circle::Small),
                            )
                            .child(
                                IconButton::new("b-add", Icon::Plus, "Add to library")
                                    .glass(Circle::Regular),
                            )
                            .child(
                                IconButton::new("b-more", Icon::More, "More")
                                    .glass(Circle::Large),
                            )
                            .child(
                                IconButton::new("b-key", Icon::Play, "Play").key(Circle::Key),
                            )
                            .child(IconButton::new("b-next", Icon::Next, "Next"))
                            .child(
                                IconButton::new("b-repeat", Icon::Repeat, "Repeat").selected(true),
                            )
                            .child(
                                IconButton::new("b-shuffle-off", Icon::Shuffle, "Shuffle")
                                    .selected(false),
                            ),
                    ),
            ))
            .child(block(
                &theme,
                "Switches and select",
                div()
                    .flex()
                    .flex_col()
                    .max_w(theme.size.player_max_width)
                    .px(theme.space.m)
                    .rounded(theme.radius.menu)
                    .bg(colors.fill)
                    .child(toggle(0, "Lossless", "Plays ALAC when the track has it."))
                    .child(div().h(theme.size.hairline).bg(colors.divider))
                    .child(toggle(
                        1,
                        "Reduce animations",
                        "Turns off sheet, page and artwork motion.",
                    ))
                    .child(div().h(theme.size.hairline).bg(colors.divider))
                    .child(toggle(
                        2,
                        "Album-art colours",
                        "Pages take their colour from the artwork.",
                    ))
                    .child(div().h(theme.size.hairline).bg(colors.divider))
                    .child(
                        div()
                            .h(theme.size.settings_row)
                            .flex()
                            .items_center()
                            .justify_between()
                            .child("Streaming quality")
                            .child(self.select.clone()),
                    ),
            ))
            .child(block(
                &theme,
                "Fields",
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.m)
                    .max_w(theme.size.search_max_width)
                    .child(self.search.clone())
                    .child(self.input.clone())
                    .child(self.slider.clone())
                    .child(self.code.clone()),
            ))
            .child(block(
                &theme,
                "Sheets, menus and toasts",
                div()
                    .flex()
                    .flex_wrap()
                    .gap(theme.space.s)
                    .child(Button::new("open-sheet", "Sign in for lossless").on_click(
                        move |_, _, cx| {
                            open_sheet.update(cx, |this, cx| {
                                this.sheet_open = true;
                                this.code.update(cx, |c, cx| c.clear(cx));
                                cx.notify();
                            });
                        },
                    ))
                    .child(Button::new("open-dialog", "Remove from library").on_click(
                        move |_, _, cx| {
                            open_dialog.update(cx, |this, cx| {
                                this.dialog_open = true;
                                cx.notify();
                            })
                        },
                    ))
                    .child(
                        Button::new("open-toast", "Show a toast").on_click(move |_, _, cx| {
                            toast.update(cx, |t, cx| {
                                t.push(
                                    Toast::new("Added “Motion Sickness” to Playing Next")
                                        .action("Undo", |_, _| report("toast", json!("undo"))),
                                    cx,
                                )
                            })
                        }),
                    )
                    .child(
                        Button::new("open-menu", "Open a menu").on_click(move |e, w, cx| {
                            let at = e.position();
                            menu.update(cx, |this, cx| this.open_menu(at, None, w, cx))
                        }),
                    ),
            ))
            .child(block(
                &theme,
                "Titles cut to fit",
                div()
                    .flex()
                    .flex_wrap()
                    .gap(theme.space.m)
                    .children(samples.iter().map(|s| {
                        div()
                            .w(theme.size.tile_min)
                            .px(theme.space.s)
                            .py(theme.space.xs)
                            .rounded(theme.radius.medium)
                            .bg(colors.fill)
                            .truncate()
                            .text_size(theme.text.body)
                            .child(*s)
                    })),
            ))
            .child(self.shelf_demo(cx))
            .child(block(
                &theme,
                "Empty, error and loading",
                div()
                    .flex()
                    .gap(theme.space.xxl)
                    .child(EmptyStateView::new(&EmptyState {
                        title: "No results for “radioheed”".into(),
                        message: "Check the spelling, or try another word.".into(),
                    }))
                    .child(
                        ErrorStateView::new(
                            &LoadError::Offline("offline".into())
                                .state(&Route::Album { id: "1".into() }),
                        )
                        .on_recover(|r, _, _| report("recover", json!(format!("{r:?}")))),
                    ),
            ))
            .child(
                rustet_ui::components::loading("loading-demo")
                    .flex()
                    .flex_col()
                    .gap(theme.space.l)
                    .child(Placeholder::Tiles {
                        count: 4,
                        size: theme.size.tile_min,
                    })
                    .child(
                        div()
                            .max_w(theme.size.player_max_width)
                            .child(Placeholder::Rows(3)),
                    ),
            )
            .child(block(
                &theme,
                "Icons",
                div()
                    .flex()
                    .flex_wrap()
                    .gap(theme.space.l)
                    .children(Icon::ALL.iter().map(|i| {
                        i.element()
                            .size(theme.size.icon)
                            .text_color(colors.text)
                    })),
            ))
            .into_any_element()
    }

    /// Every tile kind, then a long shelf that pages (the fixtures' shelves hold 3 items).
    fn shelf_demo(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let on_event = self.on_event(cx);
        let tiles: Vec<Tile> = self
            .grid
            .as_ref()
            .map(|g| g.read(cx).tiles().iter().take(40).cloned().collect())
            .unwrap_or_default();
        let kinds = [
            (TileKind::Album, "Album", None),
            (TileKind::Playlist, "Playlist", None),
            (TileKind::Station { live: true }, "Station", Some("Live")),
            (TileKind::Artist, "Artist", None),
            (TileKind::MusicVideo, "Music video", None),
            (TileKind::Song, "Song", None),
            (TileKind::Curator, "Curator", None),
            (TileKind::RecordLabel, "Record label", None),
            (TileKind::Category, "Category", None),
        ];
        let every_kind: Vec<Tile> = kinds
            .into_iter()
            .zip(tiles.iter().cycle())
            .map(|((kind, label, badge), t)| Tile {
                kind,
                subtitle: Some(label.into()),
                badge: badge.map(Into::into),
                ..t.clone()
            })
            .collect();
        let shelf = |id: &str, title: &str, tiles: Vec<Tile>, see_all: bool| Shelf {
            id: id.into(),
            title: Some(title.into()),
            items: ShelfItems::Tiles {
                tiles,
                size: TileSize::Regular,
            },
            see_all: see_all.then_some(Route::New),
        };
        div()
            .flex()
            .flex_col()
            .gap(theme.space.xxl)
            .child(
                ShelfView::new(shelf("kinds", "Every tile kind", every_kind, false))
                    .on_event(on_event.clone()),
            )
            .child(ShelfView::new(shelf("paged", "A long shelf", tiles, true)).on_event(on_event))
            .into_any_element()
    }

    fn album_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(pages) = self.pages.clone() else {
            return loading(&theme);
        };
        let album = match &pages.album {
            Ok(a) => a,
            Err(e) => {
                return error_view(
                    &theme,
                    e,
                    &Route::Album {
                        id: "1889568617".into(),
                    },
                );
            }
        };
        let mut facts: Vec<SharedString> = Vec::new();
        if let Some(g) = &album.info.genre {
            facts.push(g.clone().into());
        }
        if let Some(d) = album.info.released {
            facts.push(d.year.to_string().into());
        }
        let on_event = self.on_event(cx);
        div()
            .flex()
            .flex_col()
            .pb(theme.size.player_clearance)
            .child(
                PageHeader::new(album.header.clone())
                    .facts(facts)
                    .actions([
                        HeaderAction::Play,
                        HeaderAction::Shuffle,
                        HeaderAction::AddToLibrary,
                        HeaderAction::Download,
                        HeaderAction::More,
                    ])
                    .on_action(|a, _, _| report("header", json!(format!("{a:?}"))))
                    .on_event(on_event.clone()),
            )
            .children(self.album_tracks.clone())
            .child(
                div()
                    .px(theme.space.xl)
                    .pt(theme.space.l)
                    .text_size(theme.text.callout)
                    .text_color(theme.colors.text_secondary)
                    .children(album.info.record_label.clone())
                    .children(album.info.copyright.clone().map(|c| div().child(c))),
            )
            .child(shelves(&theme, &album.shelves, on_event))
            .into_any_element()
    }

    fn artist_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(pages) = self.pages.clone() else {
            return loading(&theme);
        };
        let artist = match &pages.artist {
            Ok(a) => a,
            Err(e) => {
                return error_view(
                    &theme,
                    e,
                    &Route::Artist {
                        id: "697833299".into(),
                    },
                );
            }
        };
        let on_event = self.on_event(cx);
        div()
            .flex()
            .flex_col()
            .pb(theme.size.player_clearance)
            .child(
                PageHeader::new(artist.header.clone())
                    .hero()
                    .actions([
                        HeaderAction::Shuffle,
                        HeaderAction::Play,
                        HeaderAction::More,
                    ])
                    .on_action(|a, _, _| report("header", json!(format!("{a:?}"))))
                    .on_event(on_event.clone()),
            )
            .child(shelves(&theme, &artist.shelves, on_event))
            .into_any_element()
    }

    fn search_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let field = div()
            .flex()
            .justify_center()
            .px(theme.space.xl)
            .pt((theme.size.strip - theme.size.search) / 2.)
            .child(
                div()
                    .w_full()
                    .max_w(theme.size.search_max_width)
                    .child(self.search.clone()),
            );
        let Some(pages) = self.pages.clone() else {
            return div().child(field).child(loading(&theme)).into_any_element();
        };
        let search = match &pages.search {
            Ok(s) => s,
            Err(e) => {
                return div()
                    .child(field)
                    .child(error_view(
                        &theme,
                        e,
                        &Route::Search {
                            term: "radiohead".into(),
                        },
                    ))
                    .into_any_element();
            }
        };
        let on_event = self.on_event(cx);
        let sections = search.sections.iter().map(|section| {
            let items = match &section.listing {
                Listing::Tiles(list) => ShelfItems::Tiles {
                    tiles: list.items.clone(),
                    size: TileSize::Regular,
                },
                Listing::Tracks(list) => ShelfItems::Tracks(list.items.clone()),
            };
            ShelfView::new(Shelf {
                id: format!("search-{}", section.title),
                title: Some(section.title.clone()),
                items,
                see_all: Some(Route::Search {
                    term: search.term.clone(),
                }),
            })
            .on_event(on_event.clone())
            .when(section.group == SearchGroup::Top, ShelfView::cards)
        });
        div()
            .flex()
            .flex_col()
            .child(field)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.xxl)
                    .px(theme.space.xl)
                    .pt(theme.space.l28)
                    .pb(theme.size.player_clearance)
                    .children(sections),
            )
            .into_any_element()
    }

    fn new_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(pages) = self.pages.clone() else {
            return loading(&theme);
        };
        let new = match &pages.new {
            Ok(n) => n,
            Err(e) => return error_view(&theme, e, &Route::New),
        };
        let on_event = self.on_event(cx);
        div()
            .flex()
            .flex_col()
            .pb(theme.size.player_clearance)
            .child(
                div()
                    .px(theme.space.xl)
                    .pt(theme.size.strip)
                    .child(page_title(&theme, new.title.clone())),
            )
            .child(shelves(&theme, &new.shelves, on_event))
            .into_any_element()
    }

    fn lyrics_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        div()
            .size_full()
            .flex()
            .flex_col()
            .pt(theme.size.strip)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.space.m)
                    .px(theme.space.xl)
                    .py(theme.space.m)
                    .child(
                        div()
                            .text_size(theme.text.title2)
                            .font_weight(BOLD)
                            .child("Quality strip"),
                    )
                    .child(self.quality.clone())
                    .child(
                        Button::new("next-quality", "Next stream").on_click(move |_, _, cx| {
                            entity.update(cx, |t, cx| t.next_quality(cx))
                        }),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.lyrics.clone()))
            .into_any_element()
    }

    fn lists_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let rows = self.lists_tab_rows;
        let tab = |id: &'static str, label: &'static str, on: bool, to_rows: bool| {
            let entity = entity.clone();
            div()
                .id(id)
                .role(Role::Tab)
                .aria_label(label)
                .aria_selected(on)
                .tab_index(0)
                .focus_ring(cx)
                .px(theme.space.m)
                .h(theme.size.control)
                .flex()
                .items_center()
                .rounded_full()
                .cursor_pointer()
                .when(on, |d| d.bg(theme.colors.fill).font_weight(SEMIBOLD))
                .on_click(move |_, _, cx| {
                    entity.update(cx, |t, cx| {
                        t.lists_tab_rows = to_rows;
                        cx.notify();
                    })
                })
                .child(label)
        };
        let body = if rows {
            self.big_list.clone().map(|l| l.into_any_element())
        } else {
            self.grid.clone().map(|g| g.into_any_element())
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .pt(theme.size.strip)
            .child(
                div()
                    .flex()
                    .gap(theme.space.s)
                    .px(theme.space.xl)
                    .pb(theme.space.s)
                    .child(tab("tab-grid", "5,000 albums", !rows, false))
                    .child(tab("tab-rows", "10,000 songs", rows, true)),
            )
            .child(div().flex_1().min_h_0().children(body.or_else(|| {
                Some(
                    EmptyStateView::new(&EmptyState {
                        title: "No recorded catalog".into(),
                        message: "Run scripts/record-catalog.py, then pass --catalog.".into(),
                    })
                    .into_any_element(),
                )
            })))
            .into_any_element()
    }
}

/// A scrolling page's column: the gutter at the sides, the strip above, the player's
/// clearance below.
fn page(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .px(theme.space.xl)
        .pt(theme.size.strip)
        .pb(theme.size.player_clearance)
}

/// A page's title: 34 / 700.
fn page_title(theme: &Theme, title: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(theme.text.page_title)
        .line_height(theme.text.page_title_line)
        .font_weight(BOLD)
        .child(title.into())
}

fn loading(theme: &Theme) -> AnyElement {
    rustet_ui::components::loading("page-loading")
        .flex()
        .flex_col()
        .gap(theme.space.xl)
        .px(theme.space.xl)
        .pt(theme.size.strip)
        .child(Placeholder::Tiles {
            count: 4,
            size: theme.size.tile_min,
        })
        .child(Placeholder::Rows(6))
        .into_any_element()
}

fn error_view(theme: &Theme, e: &LoadError, route: &Route) -> AnyElement {
    let state: ErrorState = e.state(route);
    div()
        .px(theme.space.xl)
        .pt(theme.size.strip)
        .child(
            ErrorStateView::new(&state)
                .on_recover(|r: Recovery, _, _| report("recover", json!(format!("{r:?}")))),
        )
        .into_any_element()
}

fn shelves(theme: &Theme, shelves: &[Shelf], on_event: OnContent) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(theme.space.xxl)
        .px(theme.space.xl)
        .pt(theme.space.l28)
        .children(
            shelves
                .iter()
                .map(|s| ShelfView::new(s.clone()).on_event(on_event.clone())),
        )
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame_stats.begin();
        if std::mem::take(&mut self.first_frame) {
            let ms = self.started.elapsed().as_secs_f64() * 1000.;
            window.on_next_frame(move |_, _| report("startup.first_frame_ms", json!(ms)));
        }
        if let Some(speed) = self.autoscroll {
            let handle = match (self.lists_tab_rows, &self.grid, &self.big_list) {
                (false, Some(g), _) => Some((g.read(cx).scroll.clone(), g.entity_id())),
                (true, _, Some(l)) => Some((l.read(cx).scroll.clone(), l.entity_id())),
                _ => None,
            };
            if let Some((handle, view)) = handle {
                let base = handle.0.borrow().base_handle.clone();
                let max = base.max_offset().y;
                let mut offset = base.offset();
                offset.y -= px(speed);
                if -offset.y > max {
                    offset.y = px(0.);
                }
                base.set_offset(offset);
                App::notify(cx, view);
            }
            window.request_animation_frame();
        }
        ArtworkStore::global(cx).update(cx, |a, _| a.release(window));

        let theme = cx.theme().clone();
        let layout = Layout::for_width(window.viewport_size().width / theme.zoom);
        let entity = cx.entity();
        let appearance = theme.appearance;
        let trailing = div()
            .flex()
            .gap(theme.space.xs)
            .child(
                IconButton::new(
                    "appearance",
                    Icon::Settings,
                    if appearance == Appearance::Dark {
                        "Light appearance"
                    } else {
                        "Dark appearance"
                    },
                )
                .on_click(move |_, _, cx| {
                    entity.update(cx, |t, cx| {
                        t.set_appearance(
                            if appearance == Appearance::Dark {
                                Appearance::Light
                            } else {
                                Appearance::Dark
                            },
                            cx,
                        )
                    })
                }),
            )
            .into_any_element();
        let leading = div()
            .flex()
            .gap(theme.space.s)
            .child(IconButton::new("back", Icon::ChevronLeft, "Back").glass(Circle::Small))
            .child(
                IconButton::new("forward", Icon::ChevronRight, "Forward")
                    .glass(Circle::Small)
                    .disabled(true),
            )
            .into_any_element();

        let fills = matches!(self.section, Section::Lyrics | Section::Lists);
        let content = match self.section {
            Section::Components => self.components_page(cx),
            Section::Album => self.album_page(cx),
            Section::Artist => self.artist_page(cx),
            Section::Search => self.search_page(cx),
            Section::New => self.new_page(cx),
            Section::Lyrics => self.lyrics_page(cx),
            Section::Lists => self.lists_page(cx),
        };
        let page = div()
            .size_full()
            .flex()
            .flex_col()
            .child(if fills {
                div().flex_1().min_h_0().child(content).into_any_element()
            } else {
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("page-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .child(content),
                    )
                    .child(Scrollbar::new("page-scrollbar", self.scroll.clone()))
                    .into_any_element()
            })
            .into_any_element();

        let mut overlays: Vec<AnyElement> = self.popover.render(cx).into_iter().collect();
        if self.sheet_open {
            let close = cx.entity();
            overlays.push(
                Sheet::new("sign-in", "Sign in for lossless")
                    .on_dismiss(move |window, cx| {
                        close.update(cx, |t, cx| {
                            t.sheet_open = false;
                            window.focus(&t.focus, cx);
                            cx.notify();
                        })
                    })
                    .child(
                        div()
                            .text_size(theme.text.body)
                            .text_color(theme.colors.text_secondary)
                            .child("Enter the six-digit code Apple sent to your trusted devices."),
                    )
                    .child(self.code.clone())
                    .child(
                        div()
                            .flex()
                            .gap(theme.space.s)
                            .pt(theme.space.s)
                            .child(
                                Button::new("verify", "Verify")
                                    .primary()
                                    .large()
                                    .disabled(self.code.read(cx).code().len() < 6),
                            )
                            .child(Button::new("resend", "Send a new code").large()),
                    )
                    .into_any_element(),
            );
        }
        if self.dialog_open {
            let (yes, no, dismiss) = (cx.entity(), cx.entity(), cx.entity());
            let close = |e: Entity<Gallery>| {
                move |window: &mut Window, cx: &mut App| {
                    e.update(cx, |t, cx| {
                        t.dialog_open = false;
                        window.focus(&t.focus, cx);
                        cx.notify();
                    })
                }
            };
            overlays.push(
                Dialog::new(
                    "remove",
                    "Remove “Punisher” from your library?",
                    "It will also leave your playlists. Downloads of it stay on this computer.",
                )
                .answer("Remove", close(yes))
                .answer("Cancel", close(no))
                .on_dismiss(close(dismiss))
                .into_any_element(),
            );
        }

        div()
            .key_context("Gallery")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let handle = this.search.focus_handle(cx);
                window.focus(&handle, cx);
            }))
            .on_action(|_: &NextFocus, window, cx| window.focus_next(cx))
            .on_action(|_: &PreviousFocus, window, cx| window.focus_prev(cx))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .size_full()
            .child(app_frame(
                FrameParts {
                    sidebar: (layout != Layout::Compact).then(|| self.sidebar(layout, cx)),
                    page,
                    leading: Some(leading),
                    trailing: Some(trailing),
                    panel: None,
                    player: Some(self.player(cx)),
                    backdrop: None,
                    content_overlays: vec![self.toasts.clone().into_any_element()],
                    overlays,
                },
                window,
                cx,
            ))
            .child(self.frame_stats.probe())
    }
}

use gpui::Focusable as _;

fn main() -> anyhow::Result<()> {
    let started = Instant::now();
    let args = parse_args();
    std::fs::create_dir_all(&args.state_dir)?;
    let scenario = args.scenario.clone();
    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            let theme = Theme::new(if args.light {
                Appearance::Light
            } else {
                Appearance::Dark
            });
            rustet_ui::init(theme, None, cx).expect("init");
            cx.bind_keys([
                KeyBinding::new("ctrl-k", FocusSearch, Some("Gallery")),
                KeyBinding::new("/", FocusSearch, Some("Gallery && !TextInput")),
                KeyBinding::new("tab", NextFocus, None),
                KeyBinding::new("shift-tab", PreviousFocus, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);
            let http = reqwest::Client::builder()
                .user_agent("rustet-ui-gallery")
                .build()
                .expect("http client");
            let disk = DiskCache::open(args.state_dir.join("artwork"), 256 << 20)
                .ok()
                .map(Arc::new);
            ArtworkStore::install(http, disk, StoreConfig::default(), cx);

            let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rustet gallery".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(480.), px(400.))),
                window_decorations: Some(if args.ssd {
                    WindowDecorations::Server
                } else {
                    WindowDecorations::Client
                }),
                window_background: cx.theme().window_background(!args.ssd),
                app_id: Some("rustet-gallery".into()),
                inactive_frame_interval: None,
                ..Default::default()
            };
            let window = cx
                .open_window(options, |window, cx| {
                    cx.new(|cx| Gallery::new(&args, started, window, cx))
                })
                .expect("open window");
            if let Some(name) = scenario.clone() {
                scenario::run(name, window, cx);
            }
        });
    Ok(())
}
