#!/usr/bin/env bash
# Runs a gallery scenario (tour, bench, a11y) headless and prints its REPORT lines; the
# screenshots land in OUT_DIR.
#
#   run-proof.sh wayland|x11 SCENARIO OUT_DIR [extra gallery args...]
#
# Needs nix/ui-shell.nix, a release build of the gallery (CARGO_TARGET_DIR) and, for the long
# lists, the recorded catalog (record-catalog.py) in /tmp/rustet-ui-data.
set -euo pipefail
mode=$1 scenario=$2 out=$3
shift 3
here=$(dirname "$(realpath "$0")")
bin=${CARGO_TARGET_DIR:-$(git rev-parse --show-toplevel)/target}/release/examples/gallery
data=${RUSTET_UI_DATA:-/tmp/rustet-ui-data}
mkdir -p "$out"
"$here/headless.sh" "$mode" "$out" -- bash -c '
    "$0" --catalog "$1/catalog.json" --state-dir "$RUSTET_UI_SHOT_DIR/state" --scenario "$2" "${@:3}" \
        >"$RUSTET_UI_SHOT_DIR/report.txt" 2>"$RUSTET_UI_SHOT_DIR/app.log"' \
    "$bin" "$data" "$scenario" "$@"
grep '^REPORT' "$out/report.txt" | sed 's/^REPORT //'
