#!/usr/bin/env bash
# bundle-vlc.sh — copies the subset of the system VLC that the engine uses (libvlc, libvlccore,
# the audio plugins and the music-video plugins) into DEST, the package's `vlc/` directory.
#
# The engine loads libvlc from `vlc/` beside the executable (crates/rustet-vlc), and the app points
# VLC_PLUGIN_PATH and LD_LIBRARY_PATH at it (rustet-desktop `bundled_libs`). Without the bundle the
# engine falls back to whatever libvlc the system has.
#
# VLC's avcodec, avformat and swscale plugins only load against the FFmpeg sonames they were
# built with, so the bundle must carry those FFmpeg libraries too:
#   x86_64  third_party/ffmpeg/lib (FFmpeg 9: matches the VLC of a rolling distribution), or
#   any     RUSTET_VLC_FFMPEG_LIBDIR: libraries made by scripts/build-vlc-ffmpeg.sh for this VLC
#           (needed on Ubuntu 22.04, whose VLC links FFmpeg 4, and on every CPU but x86_64).
#
# Usage: scripts/bundle-vlc.sh DEST
#   RUSTET_VLC_LIBDIR   directory with libvlc.so.5.* (default: the first of /usr/lib/<multiarch>,
#                     /usr/lib64, /usr/lib that has it)
#   RUSTET_VLC_PLUGINS  VLC's plugin directory (default: $RUSTET_VLC_LIBDIR/vlc/plugins)
#   RUSTET_MAX_GLIBC    newest glibc a bundled library may need (default 2.38); `none` skips the
#                     check, for a package that only has to run on the machine that built it

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${1:?usage: bundle-vlc.sh DEST}"
ARCH="$(uname -m)"

LIBDIR="${RUSTET_VLC_LIBDIR:-}"
if [ -z "$LIBDIR" ]; then
    for d in "/usr/lib/$ARCH-linux-gnu" /usr/lib64 /usr/lib; do
        if compgen -G "$d/libvlc.so.5.*" >/dev/null; then LIBDIR=$d; break; fi
    done
fi
SRC="${RUSTET_VLC_PLUGINS:-$LIBDIR/vlc/plugins}"
if [ -z "$LIBDIR" ] || ! compgen -G "$LIBDIR/libvlc.so.5.*" >/dev/null || [ ! -d "$SRC" ]; then
    echo "error: system VLC not found (looked for libvlc.so.5.* in ${LIBDIR:-/usr/lib/$ARCH-linux-gnu, /usr/lib64, /usr/lib} and plugins in $SRC)." >&2
    echo "  Install it (e.g. 'sudo apt install libvlc-dev vlc-plugin-base' / 'sudo pacman -S vlc') or set RUSTET_VLC_LIBDIR / RUSTET_VLC_PLUGINS." >&2
    exit 1
fi

FFLIB=""
if [ "$ARCH" = x86_64 ]; then
    FFLIB="$REPO/third_party/ffmpeg/lib"
    [ -d "$FFLIB" ] || { echo "error: $FFLIB is missing (git lfs pull?)" >&2; exit 1; }
elif [ -z "${RUSTET_VLC_FFMPEG_LIBDIR:-}" ]; then
    echo "error: there is no bundled FFmpeg for $ARCH: build the libraries VLC's plugins link with" >&2
    echo "  scripts/build-vlc-ffmpeg.sh DIR and set RUSTET_VLC_FFMPEG_LIBDIR=DIR" >&2
    exit 1
fi

echo "Bundling VLC from $LIBDIR → $DEST ..."
rm -rf "$DEST"
mkdir -p "$DEST"
cp "$LIBDIR"/libvlc.so.5.* "$LIBDIR"/libvlccore.so.9.* "$DEST/"
(cd "$DEST" && ln -sf libvlc.so.5.*.* libvlc.so.5 && ln -sf libvlccore.so.9.*.* libvlccore.so.9)

P="$DEST/plugins"
mkdir -p "$P"

# Each group is best-effort: a plugin a distribution does not ship is skipped. The ones the
# engine cannot do without are checked below.
cp_plugins() { # <dir> <plugin>...
    local dir="$1"; shift
    local f
    mkdir -p "$P/$dir"
    for f in "$@"; do cp "$SRC/$dir/$f" "$P/$dir/" 2>/dev/null || true; done
}

# access — HTTP/HTTPS/TCP/filesystem only (imem = the engine's callback source)
cp_plugins access libhttp_plugin.so libhttps_plugin.so libtcp_plugin.so libfilesystem_plugin.so \
    libimem_plugin.so libidummy_plugin.so libattachment_plugin.so libudp_plugin.so
# demux — HLS (adaptive), MP4/M4A, TS, elementary streams, FLAC, fallback avformat
cp_plugins demux libadaptive_plugin.so libmp4_plugin.so libts_plugin.so libes_plugin.so \
    libavformat_plugin.so libflacsys_plugin.so libogg_plugin.so libwav_plugin.so \
    librawaud_plugin.so libplaylist_plugin.so
# codec — ALAC/AAC/H.264 via FFmpeg plus individual fallbacks (no faad: libavcodec decodes AAC)
cp_plugins codec libavcodec_plugin.so libaraw_plugin.so libmpg123_plugin.so liba52_plugin.so \
    libflac_plugin.so libvorbis_plugin.so libopus_plugin.so libspdif_plugin.so \
    libddummy_plugin.so libedummy_plugin.so liblpcm_plugin.so libg711_plugin.so
# audio_output — PulseAudio (PipeWire serves it too) + ALSA + memory/dummy outputs
cp_plugins audio_output libpulse_plugin.so libalsa_plugin.so libamem_plugin.so \
    libadummy_plugin.so libafile_plugin.so
# audio_filter — format conversion + resampling (rate/format matching)
cp_plugins audio_filter libaudio_format_plugin.so libsimple_channel_mixer_plugin.so \
    libtrivial_channel_mixer_plugin.so libsamplerate_plugin.so libsoxr_plugin.so \
    libspeex_resampler_plugin.so libscaletempo_plugin.so libgain_plugin.so libnormvol_plugin.so \
    libmono_plugin.so libugly_resampler_plugin.so
# video — music videos: frames go to memory (vmem) for the UI's frame ring; chroma conversion
cp_plugins video_output libvmem_plugin.so
cp_plugins video_chroma libswscale_plugin.so libchain_plugin.so
# packetizer — H.264/HEVC (video), AAC/ALAC, AC-3/E-AC-3 (Atmos), FLAC, MP3, copy, generic
cp_plugins packetizer libpacketizer_h264_plugin.so libpacketizer_hevc_plugin.so \
    libpacketizer_mpeg4audio_plugin.so libpacketizer_a52_plugin.so \
    libpacketizer_flac_plugin.so libpacketizer_mpegaudio_plugin.so libpacketizer_copy_plugin.so \
    libpacketizer_avparser_plugin.so libpacketizer_mlp_plugin.so libpacketizer_dts_plugin.so
# misc — TLS (HTTPS), XML (HLS manifests), logger
cp_plugins misc libgnutls_plugin.so libxml_plugin.so liblogger_plugin.so
# stream_filter — HLS segment stitching
cp_plugins stream_filter libinflate_plugin.so libprefetch_plugin.so librecord_plugin.so \
    libskip_plugin.so libcache_read_plugin.so libcache_block_plugin.so
# control — dummy interface (VLC needs at least one)
cp_plugins control libdummy_plugin.so

# Plugins the engine cannot do without: fail the build rather than ship a silent player.
# (PulseAudio or ALSA: one of the two outputs is enough.)
missing=0
for need in access/libimem_plugin.so demux/libmp4_plugin.so codec/libavcodec_plugin.so \
            video_output/libvmem_plugin.so video_chroma/libswscale_plugin.so; do
    [ -f "$P/$need" ] || { echo "error: required VLC plugin $need was not found under $SRC" >&2; missing=1; }
done
if [ ! -f "$P/audio_output/libpulse_plugin.so" ] && [ ! -f "$P/audio_output/libalsa_plugin.so" ]; then
    echo "error: neither VLC audio output (pulse, alsa) was found under $SRC" >&2; missing=1
fi
[ "$missing" = 0 ] || exit 1

# The FFmpeg libraries matching this VLC's plugins go next to libvlc, which the engine's
# LD_LIBRARY_PATH covers.
if [ -n "${RUSTET_VLC_FFMPEG_LIBDIR:-}" ]; then
    cp -a "$RUSTET_VLC_FFMPEG_LIBDIR"/lib*.so.* "$DEST/"
fi

# Libraries the plugins link that other distributions do not provide.
for lib in libvlc_pulse.so.0 libsoxr.so.0 libdvbpsi.so.10 libaribb24.so.0 liba52.so.0; do
    real=""
    for d in "$LIBDIR" "$LIBDIR/vlc"; do   # libvlc_pulse lives in the vlc subdirectory
        [ -e "$d/$lib" ] && { real="$(readlink -f "$d/$lib")"; break; }
    done
    if [ -n "$real" ] && [ -f "$real" ]; then
        cp "$real" "$DEST/$lib"
    else
        echo "warning: $lib not found under $LIBDIR; the plugin that needs it will not load" >&2
    fi
done

# Every FFmpeg library a bundled plugin links must resolve inside the bundle: resolving from
# /usr/lib works on the build machine and fails on the next one.
bad=0
for plug in codec/libavcodec_plugin.so demux/libavformat_plugin.so video_chroma/libswscale_plugin.so \
            audio_output/libpulse_plugin.so; do
    [ -f "$P/$plug" ] || continue
    out="$(LD_LIBRARY_PATH="$DEST${FFLIB:+:$FFLIB}" ldd "$P/$plug" 2>/dev/null)"
    for dep in libavcodec libavformat libavutil libswscale libswresample libvlc_pulse; do
        line="$(grep "^[[:space:]]*$dep\." <<<"$out" || true)"
        [ -z "$line" ] && continue
        case "$line" in
            *"$DEST"/*) ;;
            *) if [ -z "$FFLIB" ] || [[ "$line" != *"$FFLIB"/* ]]; then
                   echo "error: $(basename "$plug") resolves $dep outside the bundle: $line" >&2; bad=1
               fi ;;
        esac
    done
done
[ "$bad" = 0 ] || exit 1

# Nothing bundled may need a newer glibc than the rest of the package: a library copied from a
# rolling distribution can require symbols that older systems lack.
MAX_GLIBC="${RUSTET_MAX_GLIBC:-2.38}"
if [ "$MAX_GLIBC" != none ] && ! report="$("$REPO/scripts/check-glibc.sh" "$DEST" "$MAX_GLIBC")"; then
    grep FAIL <<<"$report" >&2
    exit 1
fi

# The plugin cache, so VLC does not scan the plugins on every start.
for gen in "$LIBDIR/vlc/vlc-cache-gen" "$(command -v vlc-cache-gen || true)"; do
    if [ -x "$gen" ]; then "$gen" "$P" 2>/dev/null || true; break; fi
done

echo "VLC bundled: $(du -sh "$DEST" | cut -f1)"
