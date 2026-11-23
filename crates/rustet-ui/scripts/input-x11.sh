#!/usr/bin/env bash
# Drives the gallery with real X11 input (xdotool) inside scripts/headless.sh x11, with openbox
# (RUSTET_UI_WM=openbox) and --ssd. Coordinates are relative to the window's content (read from
# the window, not assumed). The gallery prints a REPORT line for each effect; screenshots go
# to RUSTET_UI_SHOT_DIR.
#
#   RUSTET_UI_WM=openbox headless.sh x11 OUT -- input-x11.sh GALLERY_BINARY [gallery args...]
set -uo pipefail
"$@" --scenario input --ssd >"$RUSTET_UI_SHOT_DIR/report.txt" 2>"$RUSTET_UI_SHOT_DIR/app.log" &
app=$!
for _ in $(seq 60); do grep -q input.ready "$RUSTET_UI_SHOT_DIR/report.txt" 2>/dev/null && break; sleep 0.5; done
win=$(xdotool search --name '^Rustet gallery$' | head -1)
eval "$(xdotool getwindowgeometry --shell "$win")"
ox=$X oy=$Y
at() { echo $((ox + $1)) $((oy + $2)); }
shot() { sleep 0.4; import -window "$win" "$RUSTET_UI_SHOT_DIR/input-$1.png"; }
key() { xdotool key --delay 60 "$@"; sleep 0.3; }

# Search: Ctrl+K focuses the field; type, Down to the first suggestion, Enter chooses it.
key ctrl+k
xdotool type --delay 40 "phoebe"; sleep 0.8
key Down
shot 01-suggestions
key Return Escape

# Tooltip: rest the pointer on the player pill's Next button.
xdotool mousemove $(at 596 791); sleep 1.6
shot 02-tooltip

# The album page: click it in the sidebar, click the first row, Down, Enter plays the second.
xdotool mousemove $(at 80 138) click 1; sleep 1.2
xdotool mousemove $(at 600 385) click 1; sleep 0.2
key Down Return
shot 03-album-keyboard

# The track menu: right-click a row, then Down ×4 to "Add to playlist", Right, Down, Enter.
xdotool mousemove $(at 600 430) click 3; sleep 0.5
key Down Down Down Down Right
shot 04-menu
key Down Return

# Mouse wheel down the page.
xdotool mousemove $(at 800 500) click --repeat 8 --delay 30 5; sleep 0.5
shot 05-wheel

key ctrl+q
wait $app
