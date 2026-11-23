#!/bin/sh
# Rustet — self-extracting installer
# Usage: ./rustet-<arch>.run [--force] [--system] [--no-lossless] [--uninstall]
#
# --force        reinstall even if this build is already installed
# --system       install to /opt/rustet + /usr/local/bin (run it with sudo)
# --no-lossless  skip downloading Apple's FairPlay libraries (lossless/ALAC stays off)
# --uninstall    remove a previous installation (with --system: the system-wide one)
#
# Your settings, Apple session and caches (~/.config/rustet, ~/.cache/rustet) are never touched:
# not by an install, an upgrade (also from the older Electron-based Rustet) or an uninstall.

BUILD="__VERSION__ (__COMMIT__)"
ARCH="__ARCH__"

# ── defaults (user install) ───────────────────────────────────────────────────
# The same paths as every earlier installer, so an upgrade replaces the old install in place.
INSTALL_DIR="${XDG_DATA_HOME:-$HOME/.local/lib}/rustet"
BIN_DIR="$HOME/.local/bin"
SHARE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"

FORCE=0
SYSTEM=0
UNINSTALL=0
LOSSLESS=1

# ── launcher directory ───────────────────────────────────────────────────────
# The command is installed as `rustet`. The standard home for user executables is
# $HOME/.local/bin (XDG Base Directory spec 0.8, systemd file-hierarchy(7)); putting it on PATH
# is the distribution's job, so this installer does not edit shell startup files. The one
# adaptation is to the user's *current* PATH: if ~/.local/bin is not on it but ~/bin (the older
# convention) is, the launcher goes there so the command works immediately.
on_path() { case ":$PATH:" in *":$1:"*) return 0 ;; esac; return 1; }

for arg in "$@"; do
    case "$arg" in
        --force)       FORCE=1 ;;
        --system)      SYSTEM=1 ;;
        --uninstall)   UNINSTALL=1 ;;
        --no-lossless) LOSSLESS=0 ;;
        --help|-h)
            sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "Unknown option: $arg (see --help)"; exit 1 ;;
    esac
done

if [ "$SYSTEM" = "1" ]; then
    INSTALL_DIR="/opt/rustet"
    BIN_DIR="/usr/local/bin"
    SHARE_DIR="/usr/share"
fi
DESKTOP_DIR="$SHARE_DIR/applications"
ICON_ROOT="$SHARE_DIR/icons/hicolor"
ICON_SIZES="128x128 256x256 512x512"

if [ "$SYSTEM" != "1" ] && ! on_path "$BIN_DIR" && [ -d "$HOME/bin" ] && [ -w "$HOME/bin" ] && on_path "$HOME/bin"; then
    BIN_DIR="$HOME/bin"
fi

refresh_caches() {
    update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
    gtk-update-icon-cache -f -t "$ICON_ROOT" 2>/dev/null || true
}

# ── uninstall ─────────────────────────────────────────────────────────────────
if [ "$UNINSTALL" = "1" ]; then
    rm -rf "$INSTALL_DIR"
    for d in "$BIN_DIR" "$HOME/.local/bin" "$HOME/bin"; do
        # only a launcher this installer wrote (it execs the installed binary)
        if [ -f "$d/rustet" ] && grep -qF "$INSTALL_DIR/rustet" "$d/rustet" 2>/dev/null; then
            rm -f "$d/rustet"
        fi
    done
    rm -f "$DESKTOP_DIR/rustet.desktop"
    for size in $ICON_SIZES; do rm -f "$ICON_ROOT/$size/apps/rustet.png"; done
    rm -f "$ICON_ROOT/scalable/apps/rustet.svg"
    refresh_caches
    echo "Uninstalled Rustet."
    echo "Your settings and caches are still in ~/.config/rustet and ~/.cache/rustet; delete them to remove everything."
    exit 0
fi

# ── checks ────────────────────────────────────────────────────────────────────
host_arch=$(uname -m)
if [ "$host_arch" != "$ARCH" ]; then
    echo "Error: this installer is for $ARCH, but this machine is $host_arch."
    exit 1
fi

if [ "$FORCE" = "0" ] && [ -f "$INSTALL_DIR/.version" ] && [ "$(cat "$INSTALL_DIR/.version")" = "$BUILD" ]; then
    echo "Rustet $BUILD is already installed."
    echo "Run: rustet   or pass --force to reinstall."
    exit 0
fi

if ! command -v zstd >/dev/null 2>&1; then
    echo "Error: zstd is required for installation."
    echo "  Debian/Ubuntu:  sudo apt install zstd"
    echo "  Fedora/RHEL:    sudo dnf install zstd"
    echo "  Arch:           sudo pacman -S zstd"
    echo "  openSUSE:       sudo zypper install zstd"
    exit 1
fi

parent=$(dirname "$INSTALL_DIR")
mkdir -p "$parent" 2>/dev/null
if [ ! -w "$parent" ]; then
    echo "Error: cannot write to $parent$([ "$SYSTEM" = 1 ] && echo ' (run with sudo for --system)')."
    exit 1
fi

# ── extract ───────────────────────────────────────────────────────────────────
# Into a new directory beside the install, which then replaces the old one whole: nothing of
# an earlier version (the Electron app's resources/, locales/, chrome-sandbox ...) survives,
# and a failed extraction leaves the old install as it was.
# Payload starts after the __PAYLOAD_BELOW__ sentinel line (byte offset, binary-safe).
SENTINEL_LINE=$(grep -an '^__PAYLOAD_BELOW__$' "$0" | tail -1 | cut -d: -f1)
HEADER_BYTES=$(head -n "$SENTINEL_LINE" "$0" | wc -c)
PAYLOAD_START=$((HEADER_BYTES + 1))

NEW_DIR="$INSTALL_DIR.new.$$"
OLD_DIR="$INSTALL_DIR.old.$$"
trap 'rm -rf "$NEW_DIR"' EXIT
rm -rf "$NEW_DIR"
mkdir -p "$NEW_DIR"

echo "Installing Rustet $BUILD..."
if ! tail -c +"$PAYLOAD_START" "$0" | zstd -d --long=31 -q | tar -x --strip-components=1 -C "$NEW_DIR"; then
    echo "Error: the installer is damaged (extraction failed); nothing was changed."
    exit 1
fi
echo "$BUILD" > "$NEW_DIR/.version"

if [ -d "$INSTALL_DIR" ]; then
    # Builds before the session moved to ~/.config/rustet kept it in the install directory;
    # Rustet copies it from there once (`<install>/drm/files`), so it moves with the upgrade.
    if [ -d "$INSTALL_DIR/drm" ] && [ ! -e "$NEW_DIR/drm" ]; then
        cp -a "$INSTALL_DIR/drm" "$NEW_DIR/drm"
    fi
    [ -e "$INSTALL_DIR/resources/app.asar" ] && echo "Replacing the Electron-based Rustet in $INSTALL_DIR."
    mv "$INSTALL_DIR" "$OLD_DIR" || { echo "Error: cannot move the old install aside."; exit 1; }
fi
if ! mv "$NEW_DIR" "$INSTALL_DIR"; then
    [ -d "$OLD_DIR" ] && mv "$OLD_DIR" "$INSTALL_DIR"
    echo "Error: cannot put the new install in place; the old one is unchanged."
    exit 1
fi
rm -rf "$OLD_DIR"

# ── launcher ──────────────────────────────────────────────────────────────────
mkdir -p "$BIN_DIR"
cat > "$BIN_DIR/rustet" <<EOF
#!/bin/sh
exec "$INSTALL_DIR/rustet" "\$@"
EOF
chmod +x "$BIN_DIR/rustet"

# ── desktop entry + icons ─────────────────────────────────────────────────────
# Icons go to the hicolor theme (desktops that look Rustet up by name: tray, MPRIS); the desktop
# entry names its icon by absolute path, which no stale icon cache can hide.
for size in $ICON_SIZES; do
    mkdir -p "$ICON_ROOT/$size/apps"
    cp "$INSTALL_DIR/share/icons/hicolor/$size/apps/rustet.png" "$ICON_ROOT/$size/apps/rustet.png"
done
mkdir -p "$ICON_ROOT/scalable/apps"
cp "$INSTALL_DIR/share/icons/hicolor/scalable/apps/rustet.svg" "$ICON_ROOT/scalable/apps/rustet.svg"

mkdir -p "$DESKTOP_DIR"
sed -e "s|^Exec=.*|Exec=\"$INSTALL_DIR/rustet\" %u|" \
    -e "s|^Icon=.*|Icon=$INSTALL_DIR/share/icons/hicolor/512x512/apps/rustet.png|" \
    "$INSTALL_DIR/share/applications/rustet.desktop" > "$DESKTOP_DIR/rustet.desktop"
refresh_caches
# rustet:// is always Rustet's; Apple's schemes are claimed only from Settings → Links.
[ "$SYSTEM" = "1" ] || xdg-mime default rustet.desktop x-scheme-handler/rustet 2>/dev/null || true

# ── FairPlay libraries (lossless) ─────────────────────────────────────────────
# Apple's Android FairPlay libraries cannot be redistributed with Rustet, so they are downloaded
# here from pinned sources and checked against SHA-256 sums (fairplay-libs.lock). They live per
# user, so a system-wide install leaves them to each user's first launch, which fetches them.
FETCH="$INSTALL_DIR/fetch-fairplay-libs.sh"
if [ "$LOSSLESS" = "1" ] && [ "$SYSTEM" != "1" ]; then
    echo "Downloading Apple's FairPlay libraries for lossless playback (~65 MB)..."
    if ! sh "$FETCH"; then
        echo "Lossless playback is off for now; Rustet will retry on its next launch,"
        echo "or run: sh \"$FETCH\""
    fi
fi

echo ""
echo "Rustet $BUILD installed to $INSTALL_DIR"
echo "Launcher: $BIN_DIR/rustet"

if ! on_path "$BIN_DIR"; then
    echo ""
    echo "Note: $BIN_DIR is not on your PATH in this session, so 'rustet' will not be"
    echo "found by name yet. It is the standard per-user location for commands, and most systems"
    echo "add it automatically at your next login once the directory exists. If the command is"
    echo "still not found after logging in again, add $BIN_DIR to PATH in your shell's startup file."
    echo "Until then:  $BIN_DIR/rustet"
fi

exit 0
# shellcheck disable=SC2317 # the sentinel before the payload, never run
__PAYLOAD_BELOW__
