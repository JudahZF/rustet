# Rustet: what this package contains and under which licences

| Part | Files | Licence | Source |
|---|---|---|---|
| Rustet | `rustet`, `fetch-fairplay-libs.sh`, `fairplay-libs.lock`, `android-stubs/` | MIT (`LICENSE`) | https://github.com/JudahZF/rustet |
| Rust crates compiled into `rustet` | `rustet` | Each crate's own licence, listed in `rust-crates.txt` (mostly MIT and Apache-2.0, `LICENSE-APACHE-2.0`) | crates.io, and Zed's repository for gpui |
| Inter (the UI font, compiled into `rustet`) | `rustet` | SIL Open Font License 1.1 (`OFL.txt`) | https://github.com/rsms/inter |
| libvlc, libvlccore and the VLC plugins Rustet uses | `vlc/` | LGPL-2.1-or-later (`LGPL-2.1.txt`) | https://www.videolan.org/vlc/download-sources.html, the version in `vlc/libvlc.so.5.*` |
| FFmpeg libraries for VLC's plugins, and `ffmpeg/` (x86-64 only) | `vlc/libav*`, `vlc/libsw*`, `ffmpeg/` | LGPL-2.1-or-later (`LGPL-2.1.txt`), built without GPL parts | `ffmpeg/README.md`, or the distribution's FFmpeg source package |
| libhybris (Android library loader) | `libhybris-core.so`, `hybris-linker/q.so` | Apache-2.0 (`LICENSE-APACHE-2.0`) | https://github.com/libhybris/libhybris |

Apple's FairPlay libraries are not in this package. `fetch-fairplay-libs.sh` downloads them on
your machine from the sources pinned in `fairplay-libs.lock` and checks each file's SHA-256.
