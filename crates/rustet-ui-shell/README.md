# rustet-ui-shell

Rustet's main window and start-up: `run(Args, Views)`. The page region is drawn by the views the
binary hands in (`rustet-ui-pages`, `rustet-ui-panels`); everything around it is here.

| Module | What it is |
| --- | --- |
| `startup` | Runtime, config directory and migration, single instance, prefs, theme, store, `App::start`, the kit, the artwork store, quit and restart. |
| `shell` | The root entity: follows `rustet_app`'s models, host requests (menus, modals, toasts, links), intents, links, windows, shortcuts. The strip (back and forward as glass circles; the window buttons come from the frame), the search field (at the top centre of the content on Search pages only; the Search item and the search key open the page and focus it), the engine banner (a glass card above the pill), the open right-side panel (`Panel::{Queue, Lyrics}`). |
| `rail` | The sidebar: the "Rustet" wordmark, Search, Home (signed in), New, Radio; "Library" (Recently Added, Artists, Albums, Songs); "Playlists" (the folder tree); pinned at the bottom Downloads, Settings and the account (signed out: the "Sign In" key pill). An icon rail in the narrow window, a menu in the narrowest. |
| `deck` | The player pill's content. `deck_drawing` (every `PlayerState`); `Room` (what fits the pill's width: controls go before the LCD gets narrower than `size.player_lcd_min`); the transport; the LCD (artwork that opens Now Playing, title, "Artist — Album" with the quality badge, the stream's format in its place under the pointer, the status or failure in its place while opening, loading or failed, "Resume" for the last session's queue, one tertiary note when idle); the `Scrubber` (the 3 px progress line, times under the pointer, position on a timer); lyrics, queue, volume (the slider opens beside the speaker on hover or keyboard focus), mini player. |
| `queue` | The right-side panels: Playing Next (reorder, remove, jump, Clear, Autoplay in the head), History, Lyrics. |
| `now_playing` | Artwork over the blurred backdrop, lyrics, stream details; the lyrics for the lyrics panel. |
| `signin` | `sheet_drawing` and the sign-in sheet; the web-window note. |
| `mini` | The mini player window, always in the untinted theme (`Untinted`: a page's artwork colour is for the main window only). |
| `keys` | gpui keys to the stored DOM key names; when a shortcut may fire. |
| `scenario` | Scripted runs for screenshots and the AT-SPI check (feature `scenario`; not in a release build). |

## Build and check

Inside `nix-shell nix/ui-shell.nix`:

    cargo test -p rustet-ui-shell --features test-support
    cargo clippy -p rustet-ui-shell --all-targets --features test-support,scenario -- -D warnings
    cargo build -p rustet --features scenario     # the app with `--scenario tour|pages|a11y|idle`

The `tour` scenario shows the sidebar at three widths, the pill in every player state and at
four widths, the volume slider, the LCD under the pointer, the queue and lyrics panels, Now
Playing, the mini player (also over a tinted album page), the sign-in sheet, the Search page
with its focused field, and the same in the light appearance.

Headless run of the assembled app (private display, D-Bus and runtime directory; never the
desktop), with a throw-away home and an engine port of its own:

    export HOME=/tmp/rustet-shell-run/home XDG_CACHE_HOME=/tmp/rustet-shell-run/home/.cache RUSTET_ENGINE_PORT=20125
    crates/rustet-ui/scripts/headless.sh wayland /tmp/shots -- "$CARGO_TARGET_DIR/debug/rustet" --no-desktop --scenario tour
    crates/rustet-ui/scripts/headless.sh wayland /tmp/a11y -- crates/rustet-ui-shell/scripts/a11y-shell.sh "$CARGO_TARGET_DIR/debug/rustet" --no-desktop

The config directory is `$HOME/.config/rustet` (`XDG_CONFIG_HOME` counts only in Flatpak); the
cache follows `XDG_CACHE_HOME`; the single-instance socket is in `XDG_RUNTIME_DIR`.
