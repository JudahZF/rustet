# rustet-ui

Rustet's own user interface, on gpui from Zed's repository. gpui only: logic that needs no UI
toolkit lives in `rustet-app` (see PORTING.md, "Native UI layering").

Build and test inside `nix-shell nix/ui-shell.nix` (it sets `CARGO_TARGET_DIR`):

    cargo test -p rustet-ui --features test-support
    cargo build --release -p rustet-ui --example gallery
    scripts/run-proof.sh wayland tour /tmp/rustet-ui-shots       # headless only, never on a desktop

The gallery (`examples/gallery`) shows every component in dark and light, and the Album,
Artist, Search and New pages (album and artist with their page tint) built from `rustet-app`'s recorded responses through `rustet_app`'s
own loaders (a local mock of Apple's API; no sign-in). `--scenario tour|bench|a11y|input`
drives it for screenshots, frame times and the AT-SPI check.

## Module map

| Module | What it is |
| --- | --- |
| `theme` | The design tokens (below), `ActiveTheme`, `Theme::from_settings`, `Theme::apply`, the page tint (`set_page_tint`). |
| `fonts` | Inter 400, 600 and 700, bundled (`assets/fonts`, SIL OFL 1.1, `fonts::LICENSE`). |
| `icons` | `Icon` (one SVG set, Rustet's own drawings: filled transport glyphs, 1.5 px rounded strokes for the rest; `Icon::element()`), the `Assets` source. |
| `format` | `duration` ("3:52", "1:02:10"), `length` ("48 minutes"), `kbps` (bits per second as "256 kbps"). |
| `components` | Generic controls of spec §6.2 (list below). |
| `content` | Views over `rustet_app::pages::model` (list below). They take models and emit `ContentEvent`; they never fetch. |
| `artwork` | `ArtworkStore`: sized requests, memory and disk cache, decode off the UI thread. |
| `frame` | The window layout: `Layout`, `app_frame(FrameParts)` (sidebar, page, strip, panel, player pill), client-side decorations. |
| `window_state`, `platform`, `backdrop`, `video`, `runtime`, `host` | Window plumbing for the shell; how pages ask the window for menus, modals and toasts. |
| `frame_stats`, `grid`, `reorder` | Frame timing, grid geometry, drag-to-reorder maths. |

Start-up: `rustet_ui::init(Theme::from_settings(&render, &prefs, cx.window_appearance()), runtime, cx)`
(Tokio: the app's runtime handle, or `None` for one of the kit's own; fonts, theme, key
bindings), then `ArtworkStore::install(http, disk, config, cx)`. Call
`theme::follow_system_appearance(window)` on each window so "System" follows the desktop live.
`rustet_ui::KEY_CONTEXTS` lists the components' key contexts: app-wide shortcuts must not fire
inside them.

## Design tokens (`docs/native-ui-look.md`)

The look is the Apple Music web player's. Every colour, size, weight and duration comes from
`cx.theme()`. **Do not write a hex value, a pixel size or a font weight in a view**; if a
token is missing, add it in `theme`.

Colour roles (`theme.colors.*`). Translucent roles are white or black at an alpha, so they read
on the neutral base and on any page tint:

| Role | Dark | Light | Use |
| --- | --- | --- | --- |
| `base` | `#1F1F1F` | `#FFFFFF` | Content area (`theme.base()` adds Blur translucency). The page tint replaces it. |
| `sidebar` | `#232324` | `#FAFAFB` | Sidebar, right-side panels, sheets. |
| `sidebar_border` | `#2E2E2F` | black 10 % | The sidebar's and the panels' hairline. |
| `selected` | white 5.5 % | black 3.6 % | The selected sidebar item (`#2F2F30` / `#F1F1F2` on the sidebar). |
| `text` / `text_secondary` / `text_tertiary` | white 85 / 55 / 25 % | black 85 / 50 / 25 % | Titles; artist lines, durations, numbers, quality marks; disabled, idle player glyphs. |
| `fill` | white 10 % | black 10 % | Cards, fields, quiet pills, switch track (off), row selection. |
| `divider` | white 10 % | black 15 % | Row hairlines. |
| `hover` | white 10 % | black 10 % | Rows, menu rows, glyph buttons under the pointer. |
| `key` / `on_key` | white 85 % / the base | black 85 % / the base | Primary pills and key circles, and their text (the page colour on a tinted page). |
| `nav_icon` | `#5AC8F5` | `#007AFF` | Sidebar icons only; a preset's own accent replaces it (`theme.accent`). |
| `focus` | `#0A84FF` 60 % | `#007AFF` 60 % | The focus ring: `.focus_ring(cx)` on the page base, `.focus_ring_on(surface, cx)` elsewhere; `theme.focus_ring(surface)` for a ring drawn by hand. 3 px, 2 px out. |
| `text_selection` | `#0A84FF` 40 % | `#007AFF` 25 % | Selected text in a field. |
| `glass` / `glass_stroke` / `glass_shadow` | `#2D2D2D` 97 % / white 20 % / black 35 % | white 97 % / black 5 % / black 10 % | Player pill, menus, toasts, tooltips (`.glass(cx)`, trait `components::Glass`, with `theme.glass_shadow()`); round buttons (fill and stroke only). gpui cannot blur behind a view, so glass is a near-opaque fill. |
| `live` | `#FF2D55` | `#FF2D55` | "LIVE" badges only. |
| `switch_on` / `switch_knob` | `#30D158` / white | `#34C759` / white | Switches. |
| `on_artwork` | white | white | Text and glyphs over a picture (the artist name). |
| `artwork_dim` | black 20 % | black 20 % | Over a tile's artwork under the pointer. |
| `scrim` | black 60 % | black 40 % | Behind sheets and dialogs. |
| `placeholder` | | | An image's place when the artwork gives no colour. |

There is no brand hue in the chrome and no brass: quality marks are `text_secondary`.

**Page tint.** With album-art colours on (`theme.art_colors`), an album, playlist or artist page
takes its artwork's `bgColor` as the whole window's base: call
`theme::set_page_tint(header.palette().as_ref(), cx)` when such a page shows, and
`theme::set_page_tint(None, cx)` for every other page. The global theme then has
`theme.tint: Option<PageTint>` (`background`, `sidebar` = the background 8 % darker, `text`:
`TextSet::White` or `TextSet::Black` by luminance), and every role above follows it: base,
sidebar, the text set, glass. `Theme::page_tint(palette)` computes a tint without applying it,
`Theme::with_tint(tint)` builds the tinted tokens. `Theme::apply` (a settings change) keeps the
tint in effect. `theme.on(fill)` is the text colour on any fill.

Type (`theme.text.*`), Inter only, weights `REGULAR` (400), `SEMIBOLD` (600), `BOLD` (700):
`footnote` 10 (player times), `subhead` 11 (eyebrows, 600 uppercase), `callout` 12 (tiles, artist
lines, meta), `body` 13 (track titles, menus), `title3` 15 (sidebar labels, buttons), `title2` 17
(shelf heads 700, editorial titles 400), `title1` 22 (sheet titles), `large_title` 26 (album
titles), `page_title` 34 (page titles), `hero` 40 (artist name). Line heights: `body_line` 18,
`large_title_line` 32, `page_title_line` 41; any other line is `relative(theme.text.leading)`
(1.25; `app_frame` sets it on the window). Times, counts and formats use
`.font_features(theme.tabular())`.

Spacing (`theme.space.*`): `xxs` 2, `xs` 4, `s6` 6, `s` 8, `m` 12, `m16` 16, `l` 20 (between tiles),
`l28` 28, `xl` 40 (the content gutter), `xxl` 48 (between page sections).

Sizes (`theme.size.*`), the layout and the controls: `sidebar` 260, `rail` 64, `strip` 52, `panel`
320, `player_height` 56, `player_max_width` 668, `player_bottom` 18, **`player_clearance`** (100:
the bottom padding a page adds so its last row clears the player pill), `player_lcd_min` 220 (the
player's centre part never gets narrower: the pill drops controls first), `player_volume` 96 (the
volume slider), `nav_item` 36, `nav_gap`
4, `nav_top` 80, `nav_inset_left` 20, `nav_inset_right` 13, `nav_icon_pad` 10, `wordmark_left` 28,
`wordmark_top` 30, `control` 28 (quiet pills, selects), `button` 36 (large pills), `field` 36,
`search` 38, `search_max_width` 500, `row` 46 (track rows), `song_row` 55, `settings_row` 44,
`menu_row` 30, `menu_min_width` 200, `card` 118 (search cards), `circle_small` 28, `circle` 36,
`circle_large` 48, `key_circle` 72, `icon_small` 16, `icon` 18, `icon_large` 20, `art_row` 40,
`art_card` 90, `art_artist` 120, `art_header` 270, `header_gap` 34, `number_column` 20,
`duration_column` 52, `album_column_min` 720 (a track list shows its album column from this
width), `sheet_list_max_height` 320 (a list in a sheet), `tile_min` 180, `tile_large_min` 400, `song_column_min` 300,
`switch_width` × `switch_height` 51 × 31, `badge` 14, `focus_ring` 3, `focus_gap` 2, `progress` 3,
`slider_track` 4, `slider_knob` 12, `code_width` × `code_height` 42 × 52, `state_max_width` 520,
`toast_max_width` 440, `notes_max_width` 640, `lyrics_max_width` 820, `lyrics_lead` 120,
`lyrics_tail` 320, `hairline` 1; Settings: `settings_max_width` 760 (the column of groups),
`settings_label` 180 (a row's narrowest label; below it the control moves under the label),
`settings_field` 280 (a text field or a slider with its value), `settings_value` 64 (a slider's
value, the narrowest shortcut-key pill).

Radii (`theme.radius.*`): `small` 3 (badges, row artwork), `thumb` 4 (the player's artwork),
`medium` 6 (row and menu-row hover), `tile` 7 (tile artwork), `card` 8 (cards, fields, sidebar
items, header artwork), `menu` 10 (menus, settings groups, editorial pictures), `sheet` 14. Pills
and circles use `rounded_full()`.

Shadows: `theme.glass_shadow()` (`0 10px 40px`), `theme.artwork_shadow()` (`0 10px 30px` black
30 %), `theme.focus_ring(surface)`.

Motion (`theme.motion(Motion::…)`), only in answer to an action, zero with Reduce animations:
`PagePush` 120 ms, `SheetRise` 160, `Toast` 160, `QualityChange` 200, `ArtworkFade` 240.

Zoom: every size token is already multiplied by `theme.zoom`.

Copy: sentence case ("Add to library", "Try again"); eyebrows over editorial tiles are
uppercase, as Apple's are.

## Window layout (`frame`)

`app_frame(FrameParts { .. }, window, cx)`:

| Field | What it holds |
| --- | --- |
| `sidebar: Option<AnyElement>` | The sidebar, on `colors.sidebar` with its right hairline; width `layout.sidebar_width(&theme)` (`Layout::Full` 260, `Rail` 64, `Compact` none). |
| `page: AnyElement` | The page. It fills the content area **from the top edge**: the 52 px strip floats over it. A page starts its content `theme.size.strip` down (or draws its hero under the strip) and ends `theme.size.player_clearance` above its bottom. |
| `leading: Option<AnyElement>` | The strip's left: back and forward, `IconButton::new(..).glass(Circle::Small)`. |
| `trailing: Option<AnyElement>` | The strip's right, before the window buttons (drawn when Rustet draws the decorations). |
| `panel: Option<AnyElement>` | The right-side panel (queue, lyrics): 320 px, `colors.sidebar`, left hairline. |
| `player: Option<AnyElement>` | The player pill's content. The frame draws the glass pill (56 px high, at most 668 px wide, centred in the content area, 18 px above the bottom). |
| `backdrop: Option<AnyElement>` | Drawn behind everything (Now Playing's blurred artwork). |
| `content_overlays: Vec<AnyElement>` | Drawn over the content area only: the `Toasts` view (top centre of the content). |
| `overlays: Vec<AnyElement>` | Menus (`Popover::render(cx)`), sheets, dialogs, over the whole window. |

A click on the strip's empty band drags the window (double click maximises, right click opens
the window menu). The drag area is behind the page, so a control a page places in the top
52 px must stop the mouse-down (`.on_mouse_down(MouseButton::Left, |_, _, cx|
cx.stop_propagation())` on its container), or a click on it also starts a window move.

## Components

`components` (generic):

- `Button::new(id, label).primary().large().full_width().icon(Icon).disabled(bool).on_click(f)`: a quiet fill-coloured pill 28 px high (13 / 600) by default; `primary` makes it a key pill; `large` makes it 36 px high with 15 / 600 text (a header's Play and Shuffle, the sidebar's Sign In).
- `IconButton::new(id, Icon, label).glass(Circle).key(Circle).large().selected(bool).disabled(bool).tooltip_above().on_click(f)`: a plain glyph (28 px, 16 px glyph, text colour; `large`: 36 px with the 20 px glyph for play and pause) by default; `glass(Circle::{Small 28, Regular 36, Large 48, Key 72})` a glass circle; `key(Circle)` a key circle. `selected` makes it a toggle: on, text colour with a 4 px dot below; off, text secondary. The label is the tooltip and the accessible name; `tooltip_above` for the player. Disabled: faint, out of the Tab order, no click, described as "Unavailable" (gpui cannot set AccessKit's disabled flag).
- `Toggle::new(id, label, on).description(text).disabled(bool).on_change(f(bool))`: Apple's switch, 51 × 31, green when on; role Switch; the row is `size.settings_row` high.
- `Select::new(label, options, selected, cx)` entity, `set_options`, `set_selected`; a fill-coloured pill 28 px high (in a narrower space the choice is cut with an ellipsis), the list a glass menu; emits `SelectEvent::Changed(ix)`.
- `Slider::new(value, label, cx)` entity (`.progress()`: the player's 3 px progress line), `set_value`, `set_buffered`; emits `SliderEvent::{Changed, Committed}`.
- `TextInput::new(placeholder, cx).with_label(name).password().pill()` entity: a fill-coloured field 36 px high with radius 8 (`pill`: the 38 px search pill), the focus ring on any focus; IME, grapheme editing, `set_disabled`, `set_revealed` (password); emits `TextInputEvent::{Changed, Submitted}`. A password is masked, never copied, never reported, and never handed to an input method.
- `SearchField::new(placeholder, cx)` entity: the 38 px pill (give it at most `size.search_max_width`); emits `SearchFieldEvent::{Query (150 ms debounce), Submit, Choose(ix)}`; answer with `set_suggestions(query, Vec<Suggestion>, cx)` (stale answers are dropped); the suggestions are a glass menu.
- `CodeInput::new(cx)` entity, six digits (full-width too); emits `CodeInputEvent::Complete(code)`.
- `Sheet::new(id, title).width(px).focus_on_open(handle).on_dismiss(f).child(..)`: sidebar-coloured, radius 14, 22 / 700 title (Escape is its `sheet::Dismiss`, in the `Sheet` context, so it comes before the window's Escape; Tab stays inside it); `Dialog::new(id, title, message).answer(label, f).on_dismiss(f)` (large pills; the first answer is the key pill).
- `Toasts` entity, `push(Toast::new(msg).action(label, f), cx)`; `Toast::failure(msg)`. Glass pills at the top centre of the content: draw the view in `FrameParts::content_overlays`.
- `Tooltip::text(label)` for `.tooltip(..)`.
- `ContextMenu::new(Vec<MenuItem>, window, cx)` (named "Actions", or `set_label`; a sub-menu is named after its item), `MenuItem::{action(label, f), submenu(label, items), Separator}.icon(Icon).disabled()`: a glass menu, radius 10, 30 px rows, the item's glyph at the right; keep it in a `Popover` (`show`, `close`, `render(cx)`).
- `Glass` (trait): `.glass(cx)` gives any element the glass fill, stroke and shadow. `FocusRing` (trait): `.focus_ring(cx)`, `.focus_ring_on(surface, cx)`.
- `Scrollbar::new(id, ScrollHandle)`, last child of a `relative()` container.
- `EmptyStateView::new(&EmptyState).action(label, f)`, `ErrorStateView::new(&ErrorState).on_recover(f(Recovery))`, `Placeholder::{Rows(n), Tiles{count, size}}` (drawing only) inside one `loading(id)` region per loading area (its "Loading" node).

`content` (over `rustet_app` models; events are `ContentEvent::{Navigate, OpenExternal, Play, PlayTracks, TileMenu, TracksMenu}`):

- `ArtworkView::new(Option<Artwork>, size).aspect(f32).shape(Shape)`: `Shape::{Rounded (7, tiles), Round, Small (3, rows), Thumb (4, the player), Card (8, headers and cards), Editorial (10), Square}`; an inset hairline; the sleeve colour until the image arrives (a note when there is no artwork at all); fades in once.
- `TileView::new(tile, index, width).large().card().focus_handle(h).on_event(OnContent)`: every `TileKind`; title and subtitle 12 px under the artwork; `large` puts an uppercase eyebrow, a 17 px title and subtitle above a 16:9 picture; `card` is a search "Top Results" card `width` wide (118 px high, fill colour with a hairline, radius 8, 90 px artwork, a circle for artists, title 15, "Song · Paramore" 12 secondary, "more" at its right). Under the pointer the artwork dims and shows play (lower left) and "more" (lower right) glass circles. The Menu key or Shift+F10 opens its menu; arrow keys move between tiles in a `TileGrid` or a `ShelfView`. `tile::explicit_mark(theme)` is the "E" badge.
- `ShelfView::new(shelf).on_event(OnContent).on_see_all(f).cards()`: head 17 / 700 with a chevron when there is more (the head is then the "see all" link: the shelf's route, or `f`); previous and next are 36 px glass circles over the shelf's edges under the pointer; five album tiles in 1100 px; song shelves are the song grid (columns of four 55 px rows with a hairline above, artwork 40, title 13, artist 12, a "more" glyph that opens `TracksMenu`); a shelf of artists only shows 120 px circles (eight in 1100 px); `cards` shows tiles as search cards in two rows of as many 300 px columns as fit; link shelves as quiet pills.
- `TileGrid::new(label, Arc<[Tile]>)` entity, virtualised; emits `ContentEvent`; ends with `player_clearance`.
- `TrackListView::new(rows, TrackListOptions{label, artwork, album, fit_content, album_artist}, cx)` entity, virtualised; 46 px rows with a hairline from the number column, number (play glyph under the pointer, bars when playing) or 40 px artwork, title with the explicit badge and quality marks, the artist line when it differs from `album_artist`, album column when wide, duration, a "more" glyph; `set_rows` (keeps the selection when rows are appended), `set_now_playing`, `now_playing_row`, `selection`; keyboard, multi-select, drag source (`DraggedTracks`); emits `ContentEvent`.
- `PageHeader::new(header).facts(..).actions([HeaderAction]).round().hero().on_action(f).on_event(f)`: the album and playlist header (270 px artwork, title 26 / 700, artist 26 / 400, meta 12 / 600 joined with " · ", notes clamped to three lines with "MORE", Play and Shuffle key pills; add, download and more as 36 px glass circles at the header's top right), or with `hero` the artist hero (the picture across the content, fading into the page colour, the name 40 / 700, play as a 72 px key circle between 48 px glass circles). The page colour comes from `set_page_tint`.
- `QualityStrip::new()` entity, `set(Option<Quality>, cx)`: "ALAC 24-bit 96 kHz" or "AAC 256 kbps" in text secondary, 200 ms crossfade. `quality_text(&Quality) -> SharedString`, `quality_badge(&Quality) -> Option<&str>` ("Hi-Res Lossless", "Lossless", "Dolby Atmos"), `quality_mark(theme, label)` (the neutral 12 / 600 mark).
- `LyricsView::new(Lyrics, cx)` entity, `set_position(position, at, playing, cx)`; lines 26 / 700; emits `LyricsEvent::Seek(position)`.

The gallery shows how the shell draws the sidebar (wordmark, navigation, Sign In pill) and the
player pill's content (transport, the centred "LCD" with its progress line, lyrics, queue,
volume): `examples/gallery/main.rs`, `Gallery::sidebar` and `Gallery::player`.

## For the shell and page work

- Add new modules (`src/shell/`, `src/pages/`, or a crate of your own that depends on this
  one). Do not edit `theme`, `fonts`, `icons`, `components` or `content`; if one lacks
  something, say what and why, and it is added there for everybody.
- Pages compose `content` views from `rustet_app::pages` models and turn `ContentEvent` into
  navigation and playback; the views never fetch.
- Key bindings: components bind their keys in their own context (`TextInput`, `TrackList`,
  `Menu`, `Select`, `SearchField`, `Slider`, `CodeInput`, `Sheet`). Space and Enter click a
  focused button. A `TextInput` passes Escape on unless it is built with `clear_on_escape()`
  (the search field).
- Tab and Shift+Tab (`focus`): put `focus::traverse` on each window's root (`app_frame` does
  it). A control is a tab stop through `tab_index(0)`, or through its own handle made with
  `tab_stop(true)`. A list of items is one tab stop (`focus::rove`); its arrow keys move inside
  it. A global Space for play/pause must be bound so that it does not fire inside those
  contexts.
- Draw `Popover::render(cx)`, sheets and dialogs as `FrameParts::overlays`, and the `Toasts` view
  as `FrameParts::content_overlays`.
- When a sheet or dialog closes, focus what had focus before (its `on_dismiss`): otherwise
  focus stays on a removed element and keys go nowhere.
- Call `ArtworkStore::global(cx).update(cx, |a, _| a.release(window))` once per frame in the
  root view.

## The gpui pin

`Cargo.toml` pins `gpui` and `gpui_platform` to the commit of a Zed **stable release tag**
(now v1.29.2, `2de9e435`). gpui has no API stability promise, so the pin moves on purpose only.

- `version = "0.2.2"` on the `gpui` dependency is needed: Zed's repository also holds a lint test
  fixture named `gpui`, and Cargo would pick it.
- Zed patches some crates in its own workspace (`[patch.crates-io]` in Zed's `Cargo.toml`). A git
  dependency does not inherit them, so the root `Cargo.toml` repeats the ones in our graph:
  async-process, async-task, calloop, khronos-egl, windows-capture.
- `gpui_web` pins `unicode-properties = "=0.1.3"`; the workspace lock follows.
- One gpui crate is patched: `gpui_wgpu` comes from `third_party/gpui_wgpu`, a copy of the crate
  at the pin with one change (its wgpu instance reads `WGPU_*` variables), wired by
  `[patch."https://github.com/zed-industries/zed"]` in the root `Cargo.toml`. It lets
  `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1` enable Mesa's Asahi Vulkan driver; the app sets
  the variable itself on Apple Silicon Linux unless the user set it (`0` keeps OpenGL). See
  `third_party/gpui_wgpu/README.md`.

### Moving to a later Zed release

1. Pick the newest stable tag (`gh api repos/zed-industries/zed/releases`), take its commit
   (`gh api repos/zed-industries/zed/git/ref/tags/vX.Y.Z`).
2. Set that `rev` on both dependencies.
3. Copy the matching entries of Zed's `[patch.crates-io]` at that tag into the root
   `Cargo.toml` (only crates that appear in our `Cargo.lock`; `cargo metadata` warns about
   unused patches).
4. Move the patched copy with the pin (`third_party/gpui_wgpu/README.md`, "Moving the pin"): copy
   the crate at the new commit, apply the one change again, and write its manifest's inherited
   fields from Zed's root `Cargo.toml` at that commit. If the new gpui already reads the
   variables (or wgpu accepts the driver), delete the copy and the `[patch]` entry instead.
5. `cargo update -p gpui -p gpui_platform`; read the lock diff for packages other crates use.
   `cargo tree -d` must show no second copy of a Zed crate or of wgpu.
6. Fix the compile errors (they have been mechanical so far), run the tests, then the headless
   gallery (`scripts/run-proof.sh … tour|bench`, `scripts/input-x11.sh`, `scripts/a11y-check.sh`)
   and compare the screenshots and numbers with the previous pin.

## Rules the components follow

- Accessibility: an element reaches screen readers (AccessKit → AT-SPI) only with an `id` and a
  `role`; name it with `aria_label`, report state with `aria_selected` / `aria_value` /
  `aria_numeric_value`, handle `on_a11y_action` where a key would do something. Do not use
  `AnyView::cached`: reused subtrees lose their accessibility nodes, and entity views already
  re-render only when they notify.
- Windows that show lyrics or video set `inactive_frame_interval: None`: gpui caps unfocused
  windows at 30 fps by default.
- Music video: `video::VideoView::new(window, cx)`, then `set_source(Option<VideoSource>)` with
  the player state's `VideoSurface` and the queue item's uid. It reads the engine's frame ring
  (`rustet-framering`) on a background thread, redraws only for a new frame, keeps the last frame
  while paused, across a new ring of the same video and after a ring ends, and starts clean for
  another video. Set `None` when the view is off screen: nothing is read or uploaded then. Cost
  with gpui's OpenGL path on this machine: each frame upload takes 25–30 ms of the main thread
  whatever its size (about 20 fps at one core for 720p and 1080p, 5 fps for 2160p); see the
  shell's `video` scenario.
- The app runs with `QuitMode::Explicit`: closing the main window hides it to the tray.
