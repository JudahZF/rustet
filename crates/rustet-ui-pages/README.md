# rustet-ui-pages

The page area of Rustet's window: one view per page model of `rustet_app::pages`, for every route
`pages::load` returns. Settings and Downloads (`PageState::Local`) belong to `rustet-ui-panels`.

    let view: AnyView = rustet_ui_pages::page_area(&app, window, cx);

The view follows `app.nav().state()` by itself. Everything a page does goes through `App`
(nav, player, `app.act`, downloads); menus, dialogs, toasts and links go to the window through
`rustet_ui::host::Host`. The shell shows the toasts of `app.outcomes()`; the area itself toasts
only what has no outcome (play failures, downloads queued, "Link copied").

Build and test inside `nix-shell nix/ui-shell.nix`:

    cargo test -p rustet-ui-pages --features test-support
    cargo clippy -p rustet-ui-pages --all-targets --features test-support -- -D warnings
    cargo build --release -p rustet-ui-pages --example pages

## Module map

| Module | What it is |
| --- | --- |
| `area` | `PageArea`: loading placeholders, the page, the error or sign-in state, the push motion, a view kept per history entry (Back returns to the same scroll position, selection and focus), the page tint (`set_page_tint`: the artwork colour of album, playlist and artist pages, none for the others; set again on Back, Forward and a theme change), and the `PageEvent` → `App` dispatch. |
| `view` | `PageView`: header, shelves, track lists, grids, "load more", the playing row, playlist folders, search "see all", drag-to-reorder of an editable playlist. |
| `menu` | Context menus of rows (one or several), tiles and pages, as testable `Entry` lists (with the glyph a row shows at its right), then the kit's `MenuItem`s. |
| `modal` | Playlist name sheet (new, rename), confirm dialog, playlist picker. |
| `history` | The mirror of the navigation stack that keeps each entry's view. |
| `event`, `context` | `PageEvent` and `Sink`; `PageContext` (account, storefront, playlists, playing track). |

## The look

The pages follow `docs/native-ui-look.md` with the kit's views. Each page draws from the top
of the content area under the window's 52 px strip: it starts `theme.size.strip` down (the
artist hero alone draws under the strip) and ends `theme.size.player_clearance` above its
bottom, so the player pill does not cover its last row. Side gutter 40 px, page titles 34 / 700
(New, Radio and Home carry the sidebar's names, not Apple's "Music").

- Album and playlist: the kit's header (meta "Rock · 2000 · Lossless"; the song count and
  length go under the tracks, with the release date and the label). Album rows show the artist
  only where it is not the album's; playlist rows add artwork and the album column.
- Artist: the hero, then "Latest Release" beside "Top Songs" (the song grid), then the shelves.
- Search: no title (the field in the strip is the title). "Top Results" are cards, artists
  circles, songs the song grid; a section's head shows it in full ("All results" goes back).
- Library, sign-in, error and empty states: the page title, then the kit's state view.

## The example

`examples/pages` is a minimal window around the page area and a real `App`. It plays both
roles of `rustet` (`<exe> engine --api PORT --tls-identity-stdin`), runs inside a temporary
home (never `~/.config/rustet`), gives the engine a free port, and keeps the desktop
integrations off. Ctrl+L takes a link or a search term; Alt+Left/Right go back and forward.

    BIN=target/release/examples/pages
    crates/rustet-ui/scripts/headless.sh wayland OUT -- bash -c '"$0" --scenario live > $RUSTET_UI_SHOT_DIR/report.txt' $BIN
    crates/rustet-ui/scripts/headless.sh wayland OUT -- bash -c '"$0" --fixtures --route rustet://home --scenario fixtures > $RUSTET_UI_SHOT_DIR/report.txt' $BIN
    crates/rustet-ui/scripts/headless.sh wayland OUT -- bash -c '"$0" --fixtures --songs 10000 --route rustet://library/songs --scenario bench > $RUSTET_UI_SHOT_DIR/report.txt' $BIN

- `live`: the signed-out catalog (Apple's servers), dark and light at 1360 and 900 px.
- `fixtures`: Home and the library from a local mock (`examples/pages/fixtures.rs`): the
  recorded catalog responses in `tests/data` and library responses built from them.
- `bench`: frame times while the 10,000-row library song list scrolls.
- `account` (with `--account`, for the user only): the user's own signed-in library, read only.

Frame times (headless sway, gles2 on this aarch64 machine, 60 Hz, 1360×860, 40 px per frame,
12 s): 10,000 library songs, interval p50 17.3 ms, p95 18.4, p99 19.1; CPU work per frame
p50 9.4 ms, p95 9.9, max 12.3; 1 frame over 25 ms.
