#!/usr/bin/env bash
# Runs a command inside a private headless display server, without touching the desktop session:
# own XDG_RUNTIME_DIR, own D-Bus session, no DISPLAY or WAYLAND_DISPLAY from the parent.
#
#   headless.sh wayland|x11 OUT_DIR -- COMMAND [ARGS...]
#
# The command takes its own screenshots: RUSTET_UI_SHOT_DIR is OUT_DIR, and `grim` (Wayland) or
# `import -window root` (X11) work against the private server. Both servers have a magenta
# background, so a transparent window shows it.
#
# Wayland: sway, headless wlroots backend. WLR_RENDERER defaults to gles2 on the first render
# node (needed for dmabuf, which hardware Vulkan WSI uses); set WLR_RENDERER=pixman for a pure
# software compositor. X11: Xvfb with xcompmgr, plus the window manager in RUSTET_UI_WM if set
# (e.g. openbox). Needs nix/ui-shell.nix.
set -euo pipefail

mode=$1 out=$2
shift 2
[ "${1:-}" = "--" ] && shift
mkdir -p "$out"
out=$(realpath "$out")

runtime=$(mktemp -d /tmp/rustet-ui-headless.XXXXXX)
chmod 700 "$runtime"
cleanup() { kill "${server_pid:-}" 2>/dev/null || true; rm -rf "$runtime"; }
trap cleanup EXIT
unset WAYLAND_DISPLAY DISPLAY HYPRLAND_INSTANCE_SIGNATURE SWAYSOCK XDG_SESSION_TYPE
export XDG_RUNTIME_DIR=$runtime RUSTET_UI_SHOT_DIR=$out
# The private D-Bus session activates the AT-SPI bus from at-spi2-core's service file.
[ -n "${RUSTET_UI_ATSPI_DATA:-}" ] && export XDG_DATA_DIRS="$RUSTET_UI_ATSPI_DATA:${XDG_DATA_DIRS:-/run/current-system/sw/share}"

case $mode in
wayland)
    cat >"$runtime/sway.conf" <<EOF
output HEADLESS-1 mode ${RUSTET_UI_OUTPUT:-1600x1000}@${RUSTET_UI_REFRESH:-60}Hz bg #ff00ff solid_color
default_border none
default_floating_border none
for_window [title="Rustet Mini Player"] floating enable, move position 1180 40
for_window [title=".*"] floating enable
EOF
    WLR_BACKENDS=headless WLR_RENDERER=${WLR_RENDERER:-gles2} WLR_LIBINPUT_NO_DEVICES=1 \
        sway -c "$runtime/sway.conf" >"$out/sway.log" 2>&1 &
    server_pid=$!
    for _ in $(seq 50); do
        socket=$(find "$runtime" -maxdepth 1 -name 'wayland-*' ! -name '*.lock' -print -quit)
        [ -n "$socket" ] && break
        sleep 0.1
    done
    [ -n "${socket:-}" ] || { echo "sway did not start" >&2; cat "$out/sway.log" >&2; exit 1; }
    export WAYLAND_DISPLAY=${socket##*/} XDG_SESSION_TYPE=wayland
    export SWAYSOCK=$(find "$runtime" -maxdepth 1 -name 'sway-ipc.*' -print -quit)
    dbus-run-session --config-file="$RUSTET_DBUS_SESSION_CONF" -- "$@"
    ;;
x11)
    xvfb-run -a -s "-screen 0 ${RUSTET_UI_OUTPUT:-1600x1000}x24" bash -c '
        xsetroot -solid "#ff00ff"
        xcompmgr -n &
        # An EWMH window manager when asked (always-on-top needs one); none by default.
        [ -n "${RUSTET_UI_WM:-}" ] && { $RUSTET_UI_WM >/dev/null 2>&1 & sleep 0.5; }
        export XDG_SESSION_TYPE=x11
        dbus-run-session --config-file="$RUSTET_DBUS_SESSION_CONF" -- "$@"' _ "$@"
    ;;
*)
    echo "usage: $0 wayland|x11 OUT_DIR -- COMMAND..." >&2
    exit 2
    ;;
esac
