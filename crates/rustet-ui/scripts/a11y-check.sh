#!/usr/bin/env bash
# Runs the gallery's a11y scenario inside scripts/headless.sh and dumps its AT-SPI tree at each
# state into RUSTET_UI_SHOT_DIR/a11y-<state>.jsonl.
#
#   headless.sh wayland|x11 OUT -- a11y-check.sh GALLERY_BINARY [gallery args...]
set -uo pipefail
here=$(dirname "$(realpath "$0")")
# Turn accessibility on, as a running screen reader would (starts the AT-SPI bus).
busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true
# The registry is normally started by systemd, which the private session does not have.
"$RUSTET_UI_ATSPI_DATA/../libexec/at-spi2-registryd" --use-gnome-session=false 2>/dev/null &
sleep 1
"$@" --scenario a11y >"$RUSTET_UI_SHOT_DIR/report.txt" 2>"$RUSTET_UI_SHOT_DIR/app.log" &
app=$!
for state in components album menu; do
    for _ in $(seq 80); do grep -q "a11y.ready.*$state" "$RUSTET_UI_SHOT_DIR/report.txt" 2>/dev/null && break; sleep 0.25; done
    python3 -I "$here/a11y-dump.py" "$app" >"$RUSTET_UI_SHOT_DIR/a11y-$state.jsonl" 2>"$RUSTET_UI_SHOT_DIR/a11y-$state.err"
done
wait $app
