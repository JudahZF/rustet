#!/usr/bin/env bash
# The AT-SPI check of the panels: runs the example's a11y scenario inside rustet-ui's headless
# compositor, dumps the accessibility tree at every Settings section and at Downloads (with a
# built job list), and fails when an interactive node has no name.
#
#   a11y-check.sh OUT_DIR        (inside nix-shell nix/ui-shell.nix, after building the example)
set -uo pipefail
out=$1
here=$(dirname "$(realpath "$0")")
kit=$here/../../rustet-ui/scripts
bin=${PANELS_BIN:-$here/../../../target/debug/examples/panels}
"$kit/headless.sh" wayland "$out" -- bash -c '
    kit=$0 bin=$1
    busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true
    "$RUSTET_UI_ATSPI_DATA/../libexec/at-spi2-registryd" --use-gnome-session=false 2>/dev/null &
    sleep 1
    "$bin" --scenario a11y --dark >"$RUSTET_UI_SHOT_DIR/report.txt" 2>"$RUSTET_UI_SHOT_DIR/app.log" &
    app=$!
    for state in account playback library downloads storage appearance history shortcuts integrations links page-downloads; do
        for _ in $(seq 120); do grep -q "a11y.ready.*\"$state\"" "$RUSTET_UI_SHOT_DIR/report.txt" 2>/dev/null && break; sleep 0.25; done
        sleep 1
        # The example restarts itself inside the state directory: the window is its child.
        pid=$(pgrep -P "$app" -f examples/panels | head -1)
        python3 -I "$kit/a11y-dump.py" "${pid:-$app}" >"$RUSTET_UI_SHOT_DIR/a11y-$state.jsonl" 2>"$RUSTET_UI_SHOT_DIR/a11y-$state.err"
    done
    wait $app' "$kit" "$bin" >/dev/null 2>&1
python3 -I "$here/a11y-unnamed.py" "$out"/a11y-*.jsonl
