#!/usr/bin/env bash
# Drives the assembled app with real X11 input (xdotool), signed out: a context menu on a real
# album page by mouse and keys, its toast, a deck tooltip, a confirm dialog from Settings, Tab
# in a Settings form, and Tab in the sign-in sheet (a made-up account, never submitted). Run
# inside crates/rustet-ui/scripts/headless.sh x11 with openbox:
#
#   RUSTET_UI_WM=openbox headless.sh x11 OUT -- input-x11.sh RUSTET_BINARY [args...]
#
# The binary needs the `scenario` feature (`--scenario idle` keeps it open). Coordinates are
# relative to the window's content at its default size (1192×792).
set -uo pipefail
"$@" --no-desktop --scenario idle rustet://album/1440833098 >"$RUSTET_UI_SHOT_DIR/report.txt" 2>"$RUSTET_UI_SHOT_DIR/app.log" &
app=$!
for _ in $(seq 80); do grep -q idle.ready "$RUSTET_UI_SHOT_DIR/report.txt" 2>/dev/null && break; sleep 0.5; done
sleep 6
win=$(xdotool search --name '^Rustet$' | head -1)
eval "$(xdotool getwindowgeometry --shell "$win")"
ox=$X oy=$Y
at() { echo $((ox + $1)) $((oy + $2)); }
shot() { sleep 0.5; import -window "$win" "$RUSTET_UI_SHOT_DIR/input-$1.png"; }
key() { xdotool key --delay 80 "$@"; sleep 0.3; }

# A track row's menu on the album page: right-click the second row.
xdotool mousemove $(at 600 396) click 3
shot 01-row-menu
# Up wraps to the last item ("Share link"); Enter runs it: the "Link copied" toast.
key Up Return
shot 02-toast

# A deck tooltip: rest the pointer on Next.
xdotool mousemove $(at 510 746); sleep 1.4
shot 03-deck-tooltip

# A modal from a real page: Settings → Storage → Clear song cache.
xdotool mousemove $(at 82 722) click 1; sleep 1
xdotool mousemove $(at 307 320) click 1; sleep 1
xdotool mousemove $(at 1088 427) click 1
shot 04-confirm-dialog
key Escape
shot 05-dialog-closed

# Tab in a Settings form: Storage, from the button the closed dialog gave the focus back to.
key Tab
shot 06-settings-tab-1
key Tab
shot 07-settings-tab-2
key shift+Tab
shot 08-settings-shift-tab

# The sign-in sheet, by keys, with a made-up account. Enter is never pressed once a password
# is typed, and Escape closes the sheet: nothing is sent to Apple.
xdotool mousemove $(at 110 762) click 1; sleep 2
xdotool type --delay 40 'nobody@example.invalid'
key Tab
xdotool type --delay 40 'made-up-pass'
shot 09-signin-after-tab
key shift+Tab
shot 10-signin-shift-tab
for i in 1 2 3 4 5 6; do key Tab; shot "11-signin-tab-$i"; done
key Escape
shot 12-signin-escape

kill "$app" 2>/dev/null
wait "$app"
