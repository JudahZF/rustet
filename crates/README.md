# Rustet Rust workspace

Rustet is one Rust binary, `rustet`. It has two roles:

- `rustet` starts the app: Rustet's own window, drawn with gpui. It calls Apple Music's API for
  what it shows and starts the engine as a child process (and restarts it after a crash).
- `rustet engine` starts the engine: a local HTTPS API (127.0.0.1:20025) that signs in, fetches,
  decrypts and plays through libvlc. The player (queue, state machine, quality choice) lives
  in the engine; the app sends commands and draws the state the engine publishes.

Run it from a checkout with `scripts/dev-run.sh`.

## Crates

| Crate | Purpose |
|---|---|
| `rustet` | The binary. Selects the app or the engine role. |
| `rustet-ui` | The UI kit, built with gpui: design tokens, components, content views over the page models, the window frame. |
| `rustet-ui-shell` | The main window: start-up, frame, rail, deck, queue, Now Playing, sign-in sheet, windows. |
| `rustet-ui-pages` | The page area: one view per catalog and library route. |
| `rustet-ui-panels` | The Settings and Downloads pages. |
| `rustet-app` | Application logic without a UI toolkit: routes, page models and loaders, library sync, engine link, account, settings, desktop integrations. `rustet-ui` draws what this crate provides. |
| `rustet-player` | The player controller: queue, state machine, quality choice, history, resume. Runs in the engine. |
| `rustet-signin` | The Apple Music web sign-in window (a web view that captures the music user token). Not wired into the binary: Linux signs in through FairPlay; it is the base for a platform without the FairPlay libraries. Builds in `nix/signin-shell.nix`. |
| `rustet-desktop` | Desktop state and services: preferences, encrypted store, themes, wallpaper lookup, links, engine supervisor. |
| `rustet-integrations` | MPRIS, system tray, Discord Rich Presence, Last.fm and ListenBrainz scrobbling, Windows media controls. |
| `rustet-engine` | HTTPS API server and request handlers. |
| `rustet-apple` | Apple Music media provider: catalog → HLS → playable sessions. |
| `rustet-playback` | Playback session lifecycle and the pre-warm scheduler. |
| `rustet-export` | Track and music-video export. |
| `rustet-fairplay` | Lossless path: CBCS fragment decryption pipeline and ALAC sample decryption through FairPlay. |
| `rustet-streams` | AAC and music-video path: segment download, Widevine keys, sample decryption, licence provider. |
| `rustet-drm` | DRM manager: FairPlay account state, sign-in, sign-out. |
| `rustet-drm-native` | Loads Apple's Android FairPlay libraries through libhybris. |
| `rustet-widevine` | Widevine CDM and key requests. |
| `rustet-framering` | Shared-memory ring of video frames: the engine writes, the UI reads. |
| `rustet-vlc` | In-process libvlc player for lossless playback. |
| `rustet-dsp` | AutoEQ, binaural rendering through FFmpeg, Cavern. |
| `rustet-library` | Encrypted local library cache (SQLite). |
| `rustet-diskcache` | Disk cache with LRU eviction. |
| `rustet-ampapi` | Apple Music catalog API client, lyrics, music-video labels. |
| `rustet-hls` | HLS playlist parsing. |
| `rustet-mp4` | MP4 / fragmented MP4 box parsing and rewriting. |
| `rustet-core` | Pipeline types, provider traits, app directories, config, small containers. |

## Conventions

- **Async**: tokio. Cancellation is a `tokio_util::sync::CancellationToken`. Deadlines use
  `tokio::time::timeout`. Blocking foreign calls (FairPlay, libvlc) run on their own threads or
  in `spawn_blocking`, never on a runtime worker.
- **Errors**: each library crate has a `thiserror` enum. Callers match on variants, not on
  message text. `anyhow` is for the binary and the request handlers only.
- **Logging**: `tracing`. Verbose diagnostics are `debug!` or `trace!`.
- **HTTP client**: one shared `reqwest::Client` (rustls) passed in by the caller.
- **Wire formats**: JSON field names, routes, status codes and headers of the engine API are a
  contract with `rustet-app`'s engine client (its contract test runs the real engine). Change
  both sides together.
- **Unsafe**: every crate has `#![forbid(unsafe_code)]`, except the FFI crates
  (`rustet-drm-native`, `rustet-vlc`). `rustet-framering` has one `unsafe` module (the mapping). A crate that must call a Windows API uses
  `deny(unsafe_code)` on Windows with one allowed module (`rustet-engine`: the peer check). Each
  `unsafe` block has a `// SAFETY:` comment.
- **Platforms**: Linux and Windows. Platform-specific code is behind `cfg`.
- **Tests**: a test pins behaviour that must not change: parsers, state machines, wire formats,
  security checks. Tests that need the network or an Apple account are `#[ignore]`.
