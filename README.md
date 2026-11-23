<div align="center">

# Rustet

[![GitHub release](https://img.shields.io/github/v/release/JudahZF/rustet?include_prereleases&style=for-the-badge)](https://github.com/JudahZF/rustet/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue?style=for-the-badge)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-linux-blue?style=for-the-badge)](https://github.com/JudahZF/rustet/releases/latest)

An Apple Music client for Linux: lossless and hi-res audio, music videos, downloads, and its own native UI.

</div>

> [!IMPORTANT]
> **Disclaimer**
>
> **No Affiliation**
>
> This project and its contributors are not affiliated with, authorized by, endorsed by, or in any way officially connected with Apple Inc. or any of its subsidiaries or affiliates. This is an independent, unofficial client developed for personal use.
>
> **Trademarks**
>
> "Apple Music", "Apple", and related names, marks, and logos are registered trademarks of Apple Inc. Any use of these trademarks is for identification and reference purposes only and does not imply any association with the trademark holder.
>
> **Limitation of Liability**
>
> This application is provided "AS IS". The developers are not liable for any claim, damages, or legal consequences arising from its use. You use it entirely at your own risk. An active Apple Music subscription is required.

## Contents

[What Rustet is](#what-rustet-is) · [Features](#features) · [Not in this version](#not-in-this-version) · [Requirements](#requirements) · [Install](#install) · [Sign in](#sign-in) · [Opening links](#opening-links-in-rustet) · [Where your data lives](#where-your-data-lives) · [Build from source](#build-from-source) · [Packaging](#packaging) · [Project structure](#project-structure) · [References](#references)

## What Rustet is

Rustet is one program, `rustet`, written in Rust. It draws its own window with [gpui](https://www.gpui.rs) and calls Apple Music's API itself. There is no browser and no web player inside.

`rustet` starts a second copy of itself, `rustet engine`, as a helper process. The engine signs in, fetches and decrypts streams, and plays them through libvlc. The window sends it commands and shows what it plays.

Rustet began as a rewrite of [Adam](https://github.com/silentone12725/apple-music-linux), an Electron app around Apple's web player, and keeps its FairPlay approach. It is a separate app with its own name, folders and links.

## Features

### Playback
- **Lossless and Hi-Res Lossless**: ALAC up to 24-bit 192 kHz, decrypted through Apple's FairPlay libraries.
- **AAC 256 kbps** for everything else, also played through libvlc.
- **Music videos** in the app window, 480p to 4K, with full screen.
- **Quality choice**: AAC, Lossless (up to 24/48) or Hi-Res Lossless (up to 24/192).
- **Queue**: play next, play later, Up Next (jump, remove), shuffle, repeat, autoplay when the queue ends, stations.
- **Lyrics**: line-synced, with word timings where Apple has them, and translations.
- **Resume on launch**: the queue and the track you were playing come back.
- **Play history** in Up Next, and plays reported to Apple so Recently Played and recommendations keep working.
- A song cache and a music-video cache, with size limits you set.

### Browse and library
- Search, New, Radio, Home, charts, and album, playlist, artist, station, curator and music-video pages.
- Your library: recently added, recently played, albums, songs, artists, playlists, music videos, library search.
- Add to and remove from your library, love and dislike, share links.
- Playlists: create, rename, delete, and add, remove and reorder tracks.
- An encrypted local copy of your library for fast pages (Settings → Library → Sync now).

### Downloads
- Save a track, an album or a playlist; music videos download as one file with sound and picture.
- Quality, artwork size, embedded artwork and lyrics, file and folder name templates, and a folder you choose.
- Optional conversion of lossless downloads to FLAC with FFmpeg.
- A Downloads page with live progress, cancel, try again and clear finished.

### Appearance
- Light, dark or follow the system.
- Pages can take their colours from the artwork.
- Theme presets and your own colours, sidebar opacity, zoom, reduce animations, hide Radio.
- Window blur, where the compositor blurs windows.

### Desktop
- **MPRIS**: your desktop's media controls and bars such as Waybar see and control Rustet.
- **Media keys**, also on bare X11 window managers (Rustet grabs them itself there).
- **System tray** with playback controls.
- **Mini player**, an always-on-top window.
- **Discord Rich Presence**, **Last.fm** and **ListenBrainz** scrobbling.
- **Keyboard shortcuts** you can reassign.
- **Links**: `rustet://` links, Apple Music URLs and Apple's own schemes ([below](#opening-links-in-rustet)). Only one Rustet runs; a second start hands its link to the first.
- Wayland and X11.

## Not in this version

These existed in Adam (the Electron app), or are planned, and Rustet does not have them:

- **Windows.** Rustet has no Windows build or installer. Linux only.
- **Crossfade** and **Sound Check**: the settings are shown but switched off ("Coming in a later version").
- **Custom CSS themes**: dropped. Theme presets and your own colours replace them.
- **Music-video subtitles.**
- **Animated artwork** and **timed lyrics** in downloads: the settings are saved, downloads do not use them yet.
- **Gapless playback**: the next track starts after about 0.1 s of silence.
- **Dolby Atmos**, **AutoEQ** and **Cavern** binaural rendering: the code exists (`crates/rustet-dsp`), playback does not use it yet.
- **Flatpak**: no longer built (see [Packaging](#packaging)).

## Requirements

- Linux on x86-64 or aarch64 (arm64), with a Wayland compositor or an X server.
- Graphics through Vulkan or OpenGL (Mesa or your GPU vendor's driver).
- PulseAudio or PipeWire (with its PulseAudio service).
- `curl` or `wget`, and `sha256sum`, to download Apple's FairPlay libraries. `zstd` for the `.run` installer.
- On aarch64: `ffmpeg` on your `PATH` for music-video downloads and FLAC conversion. The x86-64 packages bundle a minimal FFmpeg.
- An Apple Music subscription.

The packages bundle the parts of VLC that Rustet uses, so you do not need VLC installed. The other libraries Rustet loads (xkbcommon, xcb, Wayland, fontconfig, freetype, Vulkan or EGL, libpulse, GnuTLS, libxml2) are part of every desktop installation.

## Install

### `.run` installer (recommended)

No root needed for a user install.

```bash
chmod +x rustet-x86_64.run      # rustet-aarch64.run on arm64
./rustet-x86_64.run
```

It installs to `~/.local/lib/rustet`, puts the `rustet` command in `~/.local/bin`, and adds a desktop entry and icons. Then it downloads Apple's FairPlay libraries ([below](#apples-fairplay-libraries)).

```bash
./rustet-x86_64.run --system       # install to /opt/rustet and /usr/local/bin (run with sudo)
./rustet-x86_64.run --force        # reinstall the same build
./rustet-x86_64.run --no-lossless  # skip the FairPlay download
./rustet-x86_64.run --uninstall    # remove it (add --system for a system install)
./rustet-x86_64.run --help
```

Uninstalling leaves your settings and caches in `~/.config/rustet` and `~/.cache/rustet`; delete them yourself to remove everything.

**Coming from Adam:** Rustet does not touch Adam's folders. To keep your Adam settings, Apple session and library copy, copy them before Rustet's first start:

```bash
cp -a ~/.config/adam ~/.config/rustet
cp -a ~/.cache/adam ~/.cache/rustet
```

A few saved values (the resume point, play counts) start fresh, because they are stored under new names.

### AppImage

```bash
chmod +x Rustet-*-x86_64.AppImage
./Rustet-*-x86_64.AppImage
```

It uses the static AppImage runtime, so it needs no `libfuse2`. If mounting fails, add `--appimage-extract-and-run`. It does not add a desktop entry; [AppImageLauncher](https://github.com/TheAssassin/AppImageLauncher) can.

### Nix (NixOS)

The repository is a flake with `packages.<system>.rustet` for `x86_64-linux` and `aarch64-linux`. The libhybris builds are in Git LFS, so ask for LFS in the URL:

```bash
nix run 'git+https://github.com/JudahZF/rustet?ref=main&lfs=1'
```

In a NixOS flake, add the input `rustet.url = "git+https://github.com/JudahZF/rustet?ref=main&lfs=1";` and put `inputs.rustet.packages.${pkgs.stdenv.hostPlatform.system}.rustet` in `environment.systemPackages`.

Prebuilt binaries for `x86_64-linux` and `aarch64-linux` are published to the signed binary cache on GitHub Pages so you do not have to build from source:

```nix
nix.settings = {
  extra-substituters = [ "https://judahzf.github.io/rustet" ];
  extra-trusted-public-keys = [
    "judahzf.github.io-rustet-1:a2doMiXEsZ/qMwXqkD5gs5XynSz3AKf9TNXS6IUarvE="
  ];
};
```

### Apple's FairPlay libraries

Rustet signs in and decrypts through Apple's own Android libraries, which Rustet may not redistribute. They are downloaded to your machine instead: about 65 MB, from the [wrapper project](https://github.com/WorldObservationLog/wrapper) with [a fork](https://github.com/silentone12725/wrapper) as fallback, pinned to fixed commits. Each file is checked against its SHA-256 in `fairplay-libs.lock`; a file that does not match is refused. There is one list for x86-64 and one for aarch64.

- The `.run` installer downloads them at the end of a user install.
- Otherwise Rustet downloads them on its first start, then restarts its engine to use them.
- They go to `~/.config/rustet/drm/rootfs/system/lib64`. To repair them, run `sh ~/.local/lib/rustet/fetch-fairplay-libs.sh`.

Without them you can browse the catalog, but you cannot sign in or play.

## Sign in

There is one sign-in: your Apple Account. Open **Settings → Account → Sign in** and enter your email and password, then the six-digit code Apple sends you. If you cannot reach a trusted device, Apple can text the code to your phone number instead.

The same sign-in covers the catalog, your library, lyrics, playback, lossless and downloads. Settings → Account shows the engine, FairPlay and session status.

## Opening links in Rustet

Rustet opens Apple Music links from other apps and from the command line (`rustet <link>`):

- **`rustet://`** links always work: `rustet://song/<id>`, `rustet://album/<id>`, `rustet://playlist/<id>`, `rustet://play/<s|a|p|r>/<id>`, `rustet://open/<s|a|p|r>/<id>` (add `?sf=us` for a storefront), or `rustet://music.apple.com/...` with any Apple Music path.
- **Apple Music URLs** (`https://music.apple.com/...` on any Apple Music host, including old `itunes.apple.com` links and `?i=` song links) play when they point at a song, album, playlist, artist or music video, and open in the app for any other page.
- **Apple's own schemes**: `music://` `musics://` `itms://` `itmss://` `itunes://` `itals://` `italss://` `itvls://` `itvlss://` `itsradio://` `itunesradio://` `daap://` `itpc://`. **Rustet claims none of them until you turn them on in Settings → Links**, one by one, because claiming a scheme takes it away from the app that handles it now. `daap://` (shared iTunes libraries) and `itpc://` (podcasts) cannot be played, so Rustet shows a notice for them.

A plain `https://music.apple.com` link clicked in another app opens in your browser: the system does not let an app claim another company's website. To get from the browser into Rustet, turn on `music://` and click **Open in Music** on the website, or use a browser extension that rewrites `music.apple.com` to `rustet://music.apple.com`.

## Where your data lives

| What | Where |
|---|---|
| Settings, encrypted store (tokens, scrobbler keys) | `~/.config/rustet` |
| Apple session | `~/.config/rustet/drm/files` |
| Apple's FairPlay libraries | `~/.config/rustet/drm/rootfs/system/lib64` |
| Library copy, song and music-video caches, artwork | `~/.cache/rustet` |
| The app (`.run` install) | `~/.local/lib/rustet`, or `/opt/rustet` with `--system` |

`XDG_CONFIG_HOME` and `XDG_CACHE_HOME` move the first two.

## Build from source

The libhybris builds (x86-64 and aarch64) and the x86-64 FFmpeg are stored with [Git LFS](https://git-lfs.com). Install it before you clone:

```bash
git lfs install
git clone https://github.com/JudahZF/rustet
cd rustet
```

You need current stable Rust (`rustup`), a C compiler, `pkg-config`, the development files of fontconfig, freetype, xkbcommon (with its X11 part), xcb and Wayland, and libvlc with its plugins.

```bash
# Debian / Ubuntu
sudo apt install build-essential pkg-config libfontconfig-dev libfreetype-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libwayland-dev libvlc-dev vlc-plugin-base
# Fedora
sudo dnf install gcc pkgconf-pkg-config fontconfig-devel freetype-devel \
  libxkbcommon-devel libxkbcommon-x11-devel libxcb-devel wayland-devel vlc-devel
# Arch
sudo pacman -S base-devel fontconfig freetype2 libxkbcommon libxkbcommon-x11 libxcb wayland vlc
```

With Nix, `shell.nix` has all of this; `scripts/dev-run.sh` enters it by itself.

Build and start:

```bash
scripts/dev-run.sh                 # debug build, then start Rustet
scripts/dev-run.sh --release       # optimised build
scripts/dev-run.sh rustet://album/1440833098
```

`dev-run.sh` puts libhybris beside the built executable, where the engine looks for it: `packaging/hybris-x86_64/` or `packaging/hybris-aarch64/`. `RUSTET_HYBRIS_DIR` overrides it with another directory that has `libhybris-core.so` and `hybris-linker/q.so`. Without them you can browse, but not sign in.

Useful variables:

- `RUSTET_DEV_HOME=<dir>`: keep settings, caches and the Apple session inside `<dir>`, apart from an installed Rustet.
- `RUSTET_ENGINE_PORT=<port>`: the engine's port (default 20025). Set another one while an installed Rustet runs.
- `RUST_LOG=debug`: more log output.

Checks: `scripts/lint.sh` runs rustfmt, clippy and shellcheck. The crate map and the code conventions are in [crates/README.md](crates/README.md).

## Packaging

```bash
scripts/build-package.sh       # dist/rustet-<arch>/: the installable directory
scripts/build-installer.sh     # dist/rustet-<arch>.run
scripts/build-appimage.sh      # dist/Rustet-<version>-<arch>.AppImage
```

All three build for the machine's own CPU. They copy the needed parts of the system VLC (`scripts/bundle-vlc.sh`) and check that every bundled file runs on the glibc in `RUSTET_MAX_GLIBC`. libhybris needs glibc 2.38, so release packages are built on Ubuntu 24.04 with `RUSTET_MAX_GLIBC=2.39` (the release workflow does this for both CPUs). A distribution's VLC needs a matching minimal FFmpeg: build it with `scripts/build-vlc-ffmpeg.sh DIR` and set `RUSTET_VLC_FFMPEG_LIBDIR=DIR`. `--local` skips the portability checks, for a package that only has to run on the machine that built it (for example on NixOS).

The package layout is described at the top of `scripts/build-package.sh`; `NOTICE.md` in each package lists what it bundles and the licences. A Flatpak is not built: it would need Rustet and VLC built inside the Flatpak SDK.

## Project structure

```
rustet/
├── crates/        The Rust workspace: the rustet binary, its UI, app logic, engine and libraries
│                  (map in crates/README.md)
├── packaging/     Desktop entry, icons, licence notice, libhybris for each CPU (Git LFS),
│                  the FairPlay library lists and the Android stub sources
├── scripts/       dev-run, packaging, VLC and FFmpeg bundling, FairPlay download, lint, Nix cache
├── third_party/   Minimal FFmpeg (x86-64, LGPL, Git LFS), patched gpui_wgpu
├── nix/, flake.nix, shell.nix  The Nix package and development shells
└── assets/        Tray icon
```


## References

<details>
<summary>Libraries and projects Rustet builds on</summary>

**App**
- [gpui](https://www.gpui.rs) from [Zed](https://github.com/zed-industries/zed): the UI toolkit
- [Inter](https://github.com/rsms/inter): the UI font (OFL)
- [tokio](https://tokio.rs), [reqwest](https://github.com/seanmonstar/reqwest), [rustls](https://github.com/rustls/rustls), [hyper](https://hyper.rs) and [axum](https://github.com/tokio-rs/axum): async runtime, HTTP and TLS
- [rusqlite](https://github.com/rusqlite/rusqlite): the encrypted library cache
- [zbus](https://github.com/dbus2/zbus) and [ksni](https://github.com/iovxw/ksni): MPRIS and the system tray
- [RustCrypto](https://github.com/RustCrypto): AES, RSA, hashes
- [Last.fm](https://www.last.fm/api) and [ListenBrainz](https://listenbrainz.readthedocs.io): scrobbling APIs

**Playback**
- [libvlc](https://www.videolan.org/vlc/libvlc.html): audio and music-video playback, bundled as a subset
- [FFmpeg](https://ffmpeg.org): a minimal LGPL build for downloads, and the codec libraries VLC loads
- [libhybris](https://github.com/libhybris/libhybris): loads Apple's Android FairPlay libraries
- [wrapper](https://github.com/WorldObservationLog/wrapper): the source of those libraries and the FairPlay approach
- [musickit-sdk-linux](https://github.com/silentone12725/musickit-sdk-linux): the engine as a standalone SDK
- [AutoEQ](https://autoeq.app) and [Cavern](https://github.com/VoidXH/Cavern): headphone correction and Atmos rendering (not used by playback yet)

**Adam**: Rustet began as a rewrite of [silentone12725/apple-music-linux](https://github.com/silentone12725/apple-music-linux) ([Electron](https://electronjs.org), [MusicKit JS](https://developer.apple.com/documentation/musickitjs), [mp4ff](https://github.com/Eyevinn/mp4ff), [go-mp4tag](https://github.com/zhaarey/go-mp4tag), [grafov/m3u8](https://github.com/grafov/m3u8), [mpris-service](https://github.com/dbkr/mpris-service), [esbuild](https://esbuild.github.io), [electron-builder](https://www.electron.build)).

Contributors are listed in [CONTRIBUTORS.md](CONTRIBUTORS.md).

</details>
