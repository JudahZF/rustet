#!/usr/bin/env bash
# The kit's AT-SPI check on the real window: runs the app's a11y scenario (a build with the
# `scenario` feature: `cargo build -p rustet --features scenario`) and dumps its tree at
# each state into RUSTET_UI_SHOT_DIR/a11y-<state>.jsonl, then lists interactive nodes without a
# name (there should be none). Run inside crates/rustet-ui/scripts/headless.sh:
#
#   headless.sh wayland OUT -- a11y-shell.sh RUSTET_BINARY [args...]
set -uo pipefail
here=$(dirname "$(realpath "$0")")
kit="$here/../../rustet-ui/scripts"
busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true
"$RUSTET_UI_ATSPI_DATA/../libexec/at-spi2-registryd" --use-gnome-session=false 2>/dev/null &
sleep 1
"$@" --scenario a11y >"$RUSTET_UI_SHOT_DIR/report.txt" 2>"$RUSTET_UI_SHOT_DIR/app.log" &
app=$!
for state in main signin code nowplaying; do
    for _ in $(seq 120); do grep -q "a11y.ready.*$state" "$RUSTET_UI_SHOT_DIR/report.txt" 2>/dev/null && break; sleep 0.25; done
    python3 -I "$kit/a11y-dump.py" "$app" >"$RUSTET_UI_SHOT_DIR/a11y-$state.jsonl" 2>"$RUSTET_UI_SHOT_DIR/a11y-$state.err"
done
wait $app
python3 -I - "$RUSTET_UI_SHOT_DIR" <<'PY'
import json, pathlib, sys
interactive = {"button", "push button", "toggle button", "link", "page tab", "check box", "entry", "text",
               "password text", "slider", "list item", "tree item", "menu item", "combo box", "switch"}
for f in sorted(pathlib.Path(sys.argv[1]).glob("a11y-*.jsonl")):
    nodes = [json.loads(l) for l in f.read_text().splitlines()[1:] if l.strip()]
    unnamed = [n for n in nodes if n.get("role") in interactive and not n.get("name")]
    print(f"{f.stem}: {len(nodes)} nodes, {sum(n.get('role') in interactive for n in nodes)} interactive, {len(unnamed)} without a name")
    for n in unnamed:
        print("   ", n)
PY
