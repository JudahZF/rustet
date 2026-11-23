#!/usr/bin/env bash
# build-appimage.sh — builds dist/Rustet-<version>-<arch>.AppImage from the package directory of
# scripts/build-package.sh: the directory as it is, plus AppRun, the desktop entry and the icon at
# the top, as the AppImage format wants them.
#
# It uses the static type2 runtime, which needs neither libfuse2 nor the host's libc to start
# (and falls back to extract-and-run). The runtime and appimagetool are pinned to a release and
# checked against its published SHA-256 before use.
#
# Usage: scripts/build-appimage.sh [--local] [--skip-build]
#   --local       passed to build-package.sh: an AppImage for this machine only
#   --skip-build  pack the existing dist/rustet-<arch>/ instead of building it again
# Needs: curl, sha256sum, and what build-package.sh needs.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="$(uname -m)"
PKG="$REPO/dist/rustet-$ARCH"
build_args=()
SKIP_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --local)      build_args+=(--local) ;;
        --skip-build) SKIP_BUILD=1 ;;
        -h|--help)    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg (see --help)" >&2; exit 2 ;;
    esac
done

die() { echo "Error: $*" >&2; exit 1; }

RUNTIME_RELEASE=https://github.com/AppImage/type2-runtime/releases/download/20251108
TOOL_RELEASE=https://github.com/AppImage/appimagetool/releases/download/1.9.1
case "$ARCH" in
    x86_64)
        RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
        TOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0 ;;
    aarch64)
        RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
        TOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158 ;;
    *) die "no AppImage runtime pinned for $ARCH" ;;
esac
for tool in curl sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || die "'$tool' is required but not installed"
done

if [ "$SKIP_BUILD" = 0 ]; then
    "$REPO/scripts/build-package.sh" "${build_args[@]}" --out "$PKG"
fi
[ -f "$PKG/BUILD_INFO" ] || die "$PKG is not a package directory (run without --skip-build)"
info() { sed -n "s/^$1=//p" "$PKG/BUILD_INFO"; }
[ "$(info arch)" = "$ARCH" ] || die "$PKG was built for $(info arch), not $ARCH"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fetch() { # url sha256 dest
    curl -fsSL --retry 3 -o "$3" "$1"
    echo "$2  $3" | sha256sum -c --status || die "checksum mismatch for $1"
}
fetch "$RUNTIME_RELEASE/runtime-$ARCH" "$RUNTIME_SHA256" "$work/runtime"
fetch "$TOOL_RELEASE/appimagetool-$ARCH.AppImage" "$TOOL_SHA256" "$work/appimagetool"
chmod +x "$work/appimagetool"

app="$work/Rustet.AppDir"
cp -a "$PKG" "$app"
cat > "$app/AppRun" <<'EOF'
#!/bin/sh
# Rustet finds everything it ships beside its own executable.
here="$(dirname "$(readlink -f "$0")")"
exec "$here/rustet" "$@"
EOF
chmod +x "$app/AppRun"
cp "$app/share/applications/rustet.desktop" "$app/rustet.desktop"
cp "$app/share/icons/hicolor/512x512/apps/rustet.png" "$app/rustet.png"
ln -s rustet.png "$app/.DirIcon"

mkdir -p "$REPO/dist"
OUT="$REPO/dist/Rustet-$(info version)-$ARCH.AppImage"
# Extract-and-run: no FUSE needed on the build machine either. (The variable, not the
# --appimage-extract-and-run flag: NixOS starts AppImages through appimage-run, which would pass
# the flag on to appimagetool.)
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$work/appimagetool" --no-appstream \
    --runtime-file "$work/runtime" "$app" "$OUT"
chmod +x "$OUT"

echo ""
echo "Done: $OUT ($(du -sh "$OUT" | cut -f1)), Rustet $(info version) ($(info commit)) for $ARCH"
