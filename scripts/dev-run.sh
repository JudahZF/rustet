#!/usr/bin/env bash
# Builds and starts Rustet (the Rust app) from a checkout:
#
#   scripts/dev-run.sh [--release] [RUSTET ARGS...]      e.g.  scripts/dev-run.sh rustet://album/1440833098
#
# What it sets up:
# - Where Nix is installed, re-runs itself inside `nix-shell` (repository shell.nix) when not
#   already in it: the libraries gpui loads at run time, and libvlc with VLC_PLUGIN_PATH for the
#   engine. Elsewhere the distribution's libraries and VLC are used (README.md, "Build from source").
# - `cargo build -p rustet`: one binary, the app and its engine (`rustet engine`).
# - libhybris for the FairPlay session, next to the executable where the engine looks for it:
#   libhybris-core.so and hybris-linker/q.so. They come from $RUSTET_HYBRIS_DIR if set, else
#   from this checkout: packaging/hybris-x86_64/ (x86-64) or packaging/hybris-aarch64/ (aarch64). On NixOS the
#   copies get a run path to the shell's libc and libstdc++. Without them AAC still plays and
#   a sign-in reports the missing file.
# - The FairPlay download: fetch-fairplay-libs.sh, the lock file for this CPU and the Android
#   stubs beside the executable. On first start the app downloads Apple's libraries into
#   <config>/rustet/drm/rootfs/system/lib64 (about 25 files; it needs the network).
# - Optional scratch profile: with RUSTET_DEV_HOME=<dir>, HOME, the config and cache directories
#   and the single-instance socket are inside <dir>, so the run shares nothing with an
#   installed Rustet (prefs, store, Apple session, library cache). Apple's FairPlay libraries
#   are then looked for in <dir>/.config/rustet/drm/rootfs/system/lib64. Without RUSTET_DEV_HOME
#   the real profile is used.
#
# Other variables the app reads: RUSTET_ENGINE_PORT (engine port, default 20025; set another one
# when an installed Rustet runs), RUST_LOG (log filter, e.g. `debug`).
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

if [ -z "${IN_NIX_SHELL:-}" ] && command -v nix-shell >/dev/null 2>&1; then
    exec nix-shell "$repo/shell.nix" --run "$(printf '%q ' "$0" "$@")"
fi

profile=debug
cargo_flags=()
if [ "${1:-}" = "--release" ]; then
    profile=release
    cargo_flags=(--release)
    shift
fi

cargo build --manifest-path "$repo/Cargo.toml" -p rustet "${cargo_flags[@]}"
bin_dir=$repo/target/$profile

hybris=${RUSTET_HYBRIS_DIR:-}
if [ -z "$hybris" ]; then
    case "$(uname -m)" in
        x86_64) hybris=$repo/packaging/hybris-x86_64 ;;
        aarch64) hybris=$repo/packaging/hybris-aarch64 ;;
    esac
fi
if [ -n "$hybris" ] && [ -f "$hybris/libhybris-core.so" ] && [ -f "$hybris/hybris-linker/q.so" ]; then
    mkdir -p "$bin_dir/hybris-linker"
    install -m755 "$hybris/libhybris-core.so" "$bin_dir/libhybris-core.so"
    install -m755 "$hybris/hybris-linker/q.so" "$bin_dir/hybris-linker/q.so"
    # NixOS has no libstdc++ on the loader's default path.
    if [ -e /etc/NIXOS ]; then
        rpath="$(dirname "$("${CC:-cc}" -print-file-name=libstdc++.so.6)"):$(dirname "$("${CC:-cc}" -print-file-name=libc.so.6)")"
        patchelf --set-rpath "$rpath" "$bin_dir/libhybris-core.so" "$bin_dir/hybris-linker/q.so"
    fi
else
    echo "dev-run: no libhybris for $(uname -m) (set RUSTET_HYBRIS_DIR); sign-in will not work" >&2
fi

# Apple's FairPlay libraries: the app downloads them on first start with the script beside the
# executable, as an installed Rustet does (`scripts/build-package.sh` lays out the same files).
case "$(uname -m)" in
    x86_64) lock=$repo/packaging/fairplay-libs-x86_64.lock ;;
    aarch64) lock=$repo/packaging/fairplay-libs-aarch64.lock ;;
    *) lock="" ;;
esac
if [ -n "$lock" ]; then
    install -m755 "$repo/scripts/fetch-fairplay-libs.sh" "$bin_dir/fetch-fairplay-libs.sh"
    install -m644 "$lock" "$bin_dir/fairplay-libs.lock"
    stubs=$bin_dir/android-stubs
    if [ ! -f "$stubs/libdl.so" ]; then
        mkdir -p "$stubs"
        # shellcheck disable=SC2054 # -Wl, takes a comma
        stub_flags=(-shared -fPIC -nostdlib -O2 -Wl,--hash-style=both)
        for lib in libandroid.so libOpenSLES.so; do
            "${CC:-cc}" "${stub_flags[@]}" -Wl,-soname,"$lib" -o "$stubs/$lib" "$repo/packaging/android-stubs/empty_stub.c"
        done
        "${CC:-cc}" "${stub_flags[@]}" -Wl,-soname,libdl.so -Wl,--version-script="$repo/packaging/android-stubs/libdl.map" \
            -o "$stubs/libdl.so" "$repo/packaging/android-stubs/empty_stub.c"
    fi
else
    echo "dev-run: no FairPlay library list for $(uname -m); lossless will not work" >&2
fi

if [ -n "${RUSTET_DEV_HOME:-}" ]; then
    mkdir -p "$RUSTET_DEV_HOME"/{.config,.cache,run}
    chmod 700 "$RUSTET_DEV_HOME/run"
    # The display and the audio server stay the desktop's: name them by full path before the
    # runtime directory moves.
    desktop_runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
    case ${WAYLAND_DISPLAY:-} in
        "" | /*) ;;
        *) export WAYLAND_DISPLAY=$desktop_runtime/$WAYLAND_DISPLAY ;;
    esac
    export PULSE_SERVER=${PULSE_SERVER:-unix:$desktop_runtime/pulse/native}
    export HOME=$RUSTET_DEV_HOME
    export XDG_CACHE_HOME=$RUSTET_DEV_HOME/.cache
    export XDG_RUNTIME_DIR=$RUSTET_DEV_HOME/run
fi

exec "$bin_dir/rustet" "$@"
