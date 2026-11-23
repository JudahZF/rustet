#!/usr/bin/env bash
# build-package.sh — builds Rustet for this machine's CPU and lays it out as an installable directory,
# dist/rustet-<arch>/. build-installer.sh and build-appimage.sh pack that directory.
#
#   rustet                     the app; `rustet engine` is the engine it starts
#   libhybris-core.so        Android library loader for FairPlay (beside the executable: rustet-drm)
#   hybris-linker/q.so
#   fetch-fairplay-libs.sh   downloads Apple's FairPlay libraries on first run (rustet-app runs it
#   fairplay-libs.lock         from beside the executable); the lock is the one for this CPU
#   android-stubs/           stand-ins for three Android libraries, installed with Apple's
#   vlc/                     libvlc subset, plugins and the FFmpeg libraries they link
#   ffmpeg/                  ffmpeg for downloads (x86-64 only: elsewhere the engine uses PATH)
#   share/applications/rustet.desktop, share/icons/hicolor/*/apps/rustet.*
#   LICENSE, NOTICE.md, licenses/, BUILD_INFO
#
# Usage: scripts/build-package.sh [--local] [--out DIR]
#   --local   a package for this machine only: skip the portability checks (glibc baseline, a
#             standard ELF interpreter). Needed on NixOS, where everything links /nix/store.
#   --out     the package directory (default dist/rustet-<arch>)
#
# Inputs:
#   libhybris  this checkout's packaging/hybris-x86_64/ (x86-64) or packaging/hybris-aarch64/ (aarch64), or
#              RUSTET_HYBRIS_DIR: a directory with libhybris-core.so and hybris-linker/q.so
#   VLC        the system VLC, see scripts/bundle-vlc.sh (RUSTET_VLC_LIBDIR, RUSTET_VLC_PLUGINS,
#              RUSTET_VLC_FFMPEG_LIBDIR)
#   RUSTET_MAX_GLIBC  newest glibc any packaged file may need (default 2.35, Ubuntu 22.04): build
#              release packages on a system that old
# Needs: cargo and the libraries gpui builds against (see README.md), cc, binutils, ldd.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="$(uname -m)"
LOCAL=0
OUT="$REPO/dist/rustet-$ARCH"
while [ $# -gt 0 ]; do
    case "$1" in
        --local) LOCAL=1; shift ;;
        --out)   OUT="${2:?--out needs a directory}"; shift 2 ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

die() { echo "Error: $*" >&2; exit 1; }

for tool in cargo "${CC:-cc}" objdump readelf ldd od; do
    command -v "$tool" >/dev/null 2>&1 || die "'$tool' is required but not installed"
done

# The CPU an ELF file is built for, from its e_machine field (little-endian).
elf_arch() {
    case "$(od -An -tx2 -j18 -N2 "$1" | tr -d ' ')" in
        003e) echo x86_64 ;;
        00b7) echo aarch64 ;;
        *) echo unknown ;;
    esac
}
# A Git LFS pointer is ~130 bytes of text, not a library: a clone made without `git lfs pull`
# would otherwise package it.
real_elf() {
    [ -s "$1" ] && [ "$(head -c4 "$1" | od -An -c | tr -d ' ')" = '177ELF' ] \
        || die "$1 is missing or not a binary (an unresolved Git LFS pointer? run 'git lfs pull')"
    [ "$(elf_arch "$1")" = "$ARCH" ] || die "$1 is built for $(elf_arch "$1"), not $ARCH"
}

case "$ARCH" in
    x86_64)  LOCK="$REPO/packaging/fairplay-libs-x86_64.lock" ;;
    aarch64) LOCK="$REPO/packaging/fairplay-libs-aarch64.lock" ;;
    *) die "no FairPlay library list for $ARCH: Apple's Android libraries exist for x86_64 and aarch64" ;;
esac
HYBRIS="${RUSTET_HYBRIS_DIR:-}"
if [ -z "$HYBRIS" ]; then
    case "$ARCH" in
        x86_64) HYBRIS="$REPO/packaging/hybris-x86_64" ;;
        aarch64) HYBRIS="$REPO/packaging/hybris-aarch64" ;;
    esac
fi
real_elf "$HYBRIS/libhybris-core.so"
real_elf "$HYBRIS/hybris-linker/q.so"

VERSION="$(cd "$REPO" && cargo pkgid -p rustet | sed 's/.*[#@]//')"
COMMIT="$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ -n "$(git -C "$REPO" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    COMMIT="$COMMIT+dirty"
fi
echo "Building Rustet $VERSION ($COMMIT) for $ARCH → $OUT"

# ── 1. the binary ─────────────────────────────────────────────────────────────
echo "[1/5] cargo build --release -p rustet"
cargo build --manifest-path "$REPO/Cargo.toml" --release -p rustet
BIN="${CARGO_TARGET_DIR:-$REPO/target}/release/rustet"
real_elf "$BIN"

rm -rf "$OUT"
mkdir -p "$OUT"
install -m755 "$BIN" "$OUT/rustet"

# ── 2. FairPlay: libhybris, the fetch script, its lock and the Android stubs ──
echo "[2/5] libhybris, FairPlay fetch script and Android stubs"
install -Dm755 "$HYBRIS/libhybris-core.so" "$OUT/libhybris-core.so"
install -Dm755 "$HYBRIS/hybris-linker/q.so" "$OUT/hybris-linker/q.so"
# A NixOS package for this machine: NixOS has no libstdc++ on the loader's default path.
if [ "$LOCAL" = 1 ] && [ -e /etc/NIXOS ]; then
    rpath="$(dirname "$("${CC:-cc}" -print-file-name=libstdc++.so.6)"):$(dirname "$("${CC:-cc}" -print-file-name=libc.so.6)")"
    patchelf --set-rpath "$rpath" "$OUT/libhybris-core.so" "$OUT/hybris-linker/q.so"
fi
install -m755 "$REPO/scripts/fetch-fairplay-libs.sh" "$OUT/fetch-fairplay-libs.sh"
install -m644 "$LOCK" "$OUT/fairplay-libs.lock"
# Apple's libraries list libandroid, libOpenSLES and libdl but import nothing from them; these
# empty libraries stand in (the same build as `make -C drm android-stubs`).
STUBS="$OUT/android-stubs"
mkdir -p "$STUBS"
# shellcheck disable=SC2054 # -Wl, takes a comma
stub_flags=(-shared -fPIC -nostdlib -O2 -Wl,--hash-style=both)
for lib in libandroid.so libOpenSLES.so; do
    "${CC:-cc}" "${stub_flags[@]}" -Wl,-soname,"$lib" -o "$STUBS/$lib" "$REPO/packaging/android-stubs/empty_stub.c"
done
"${CC:-cc}" "${stub_flags[@]}" -Wl,-soname,libdl.so -Wl,--version-script="$REPO/packaging/android-stubs/libdl.map" \
    -o "$STUBS/libdl.so" "$REPO/packaging/android-stubs/empty_stub.c"
chmod 644 "$STUBS"/*.so

# ── 3. VLC and FFmpeg ─────────────────────────────────────────────────────────
echo "[3/5] VLC"
MAX_GLIBC="${RUSTET_MAX_GLIBC:-2.35}"
[ "$LOCAL" = 1 ] && MAX_GLIBC=none
RUSTET_MAX_GLIBC="$MAX_GLIBC" "$REPO/scripts/bundle-vlc.sh" "$OUT/vlc"
if [ "$ARCH" = x86_64 ]; then
    real_elf "$REPO/third_party/ffmpeg/bin/ffmpeg"
    cp -a "$REPO/third_party/ffmpeg" "$OUT/ffmpeg"
fi

# ── 4. desktop entry, icons, licences ─────────────────────────────────────────
echo "[4/5] desktop entry, icons and licences"
install -Dm644 "$REPO/packaging/rustet.desktop" "$OUT/share/applications/rustet.desktop"
for size in 128 256 512; do
    install -Dm644 "$REPO/packaging/icons/rustet-$size.png" "$OUT/share/icons/hicolor/${size}x$size/apps/rustet.png"
done
install -Dm644 "$REPO/packaging/icons/rustet.svg" "$OUT/share/icons/hicolor/scalable/apps/rustet.svg"
install -m644 "$REPO/LICENSE" "$OUT/LICENSE"
install -m644 "$REPO/packaging/NOTICE.md" "$OUT/NOTICE.md"
L="$OUT/licenses"
install -Dm644 "$REPO/crates/rustet-ui/assets/fonts/OFL.txt" "$L/OFL.txt"
install -m644 "$REPO/third_party/ffmpeg/COPYING.LGPLv2.1" "$L/LGPL-2.1.txt"
install -m644 "$REPO/third_party/gpui_wgpu/LICENSE-APACHE" "$L/LICENSE-APACHE-2.0"
(cd "$REPO" && cargo tree -p rustet -e normal --prefix none -f '{p} | {l}') \
    | sed -E 's/ \((\/|[a-z+]+:)[^)]*\)//; s/ \(\*\)$//' | sort -u > "$L/rust-crates.txt"

printf 'version=%s\ncommit=%s\narch=%s\nbuilt=%s\n' \
    "$VERSION" "$COMMIT" "$ARCH" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$OUT/BUILD_INFO"

# ── 5. checks ─────────────────────────────────────────────────────────────────
echo "[5/5] checking the package"
for f in rustet libhybris-core.so hybris-linker/q.so fetch-fairplay-libs.sh fairplay-libs.lock \
         android-stubs/libandroid.so android-stubs/libOpenSLES.so android-stubs/libdl.so \
         vlc/libvlc.so.5 vlc/libvlccore.so.9 vlc/plugins/access/libimem_plugin.so \
         share/applications/rustet.desktop; do
    [ -e "$OUT/$f" ] || die "$f is missing from the package"
done
[ ! -e "$OUT/rootfs" ] || die "rootfs/ is in the package: Apple's libraries must never ship"
if [ "$LOCAL" = 0 ]; then
    interp="$(readelf -l "$OUT/rustet" | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')"
    case "$interp" in
        /lib/*|/lib64/*) ;;
        *) die "rustet asks for the ELF interpreter '$interp', which other systems do not have (built on NixOS? use --local for a package for this machine)" ;;
    esac
    "$REPO/scripts/check-glibc.sh" "$OUT" "$MAX_GLIBC" \
        || die "files need a glibc newer than $MAX_GLIBC: build on an older system, or use --local"
fi

echo ""
echo "Done: $OUT ($(du -sh "$OUT" | cut -f1)), Rustet $VERSION ($COMMIT) for $ARCH"
[ "$LOCAL" = 1 ] && echo "Built with --local: it runs on this machine, not necessarily on others."
exit 0
