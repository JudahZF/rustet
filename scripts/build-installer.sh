#!/usr/bin/env bash
# build-installer.sh — builds dist/rustet-<arch>.run, the self-extracting installer (the version is
# inside, not in the name): scripts/installer-header.sh followed by the package directory of
# scripts/build-package.sh as a zstd-compressed tar.
#
# Usage: scripts/build-installer.sh [--local] [--skip-build]
#   --local       passed to build-package.sh: an installer for this machine only
#   --skip-build  pack the existing dist/rustet-<arch>/ instead of building it again
# Every other input is build-package.sh's (RUSTET_HYBRIS_DIR, RUSTET_VLC_*, RUSTET_MAX_GLIBC).
# Needs: zstd, tar, and what build-package.sh needs.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="$(uname -m)"
PKG="$REPO/dist/rustet-$ARCH"
OUTFILE="$REPO/dist/rustet-$ARCH.run"
build_args=()
SKIP_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --local)      build_args+=(--local) ;;
        --skip-build) SKIP_BUILD=1 ;;
        -h|--help)    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg (see --help)" >&2; exit 2 ;;
    esac
done

die() { echo "Error: $*" >&2; exit 1; }
for tool in zstd tar; do
    command -v "$tool" >/dev/null 2>&1 || die "'$tool' is required but not installed"
done

if [ "$SKIP_BUILD" = 0 ]; then
    "$REPO/scripts/build-package.sh" "${build_args[@]}" --out "$PKG"
fi
[ -f "$PKG/BUILD_INFO" ] || die "$PKG is not a package directory (run without --skip-build)"
info() { sed -n "s/^$1=//p" "$PKG/BUILD_INFO"; }
[ "$(info arch)" = "$ARCH" ] || die "$PKG was built for $(info arch), not $ARCH"

echo "Packing $OUTFILE (zstd -19 --long=31, a minute or two)..."
sed -e "s/__VERSION__/$(info version)/g" -e "s/__COMMIT__/$(info commit)/g" -e "s/__ARCH__/$ARCH/g" \
    "$REPO/scripts/installer-header.sh" > "$OUTFILE.part"
# One top-level directory in the archive; the header extracts with --strip-components=1.
tar -cf - -C "$(dirname "$PKG")" "$(basename "$PKG")" | zstd -19 --long=31 -T0 -q >> "$OUTFILE.part"
chmod +x "$OUTFILE.part"
mv -f "$OUTFILE.part" "$OUTFILE"

echo ""
echo "Done: $OUTFILE ($(du -sh "$OUTFILE" | cut -f1)), Rustet $(info version) ($(info commit)) for $ARCH"
echo "Test: $OUTFILE --help"
