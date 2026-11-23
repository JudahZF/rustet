#!/usr/bin/env bash
# Runs a scenario of the panels example inside rustet-ui's headless compositor and prints its
# REPORT lines (screenshots are listed in OUT_DIR/report.txt). Needs nix/ui-shell.nix and a
# build of the example (`cargo build -p rustet-ui-panels --example panels`).
#
#   run.sh wayland|x11 SCENARIO OUT_DIR [example args: --dark|--light, --state-dir DIR, ...]
set -euo pipefail
mode=$1 scenario=$2 out=$3
shift 3
here=$(dirname "$(realpath "$0")")
bin=${PANELS_BIN:-$here/../../../target/debug/examples/panels}
"$here/../../rustet-ui/scripts/headless.sh" "$mode" "$out" -- bash -c '
    "$0" --scenario "$1" "${@:2}" >"$RUSTET_UI_SHOT_DIR/report.txt" 2>"$RUSTET_UI_SHOT_DIR/app.log"' \
    "$bin" "$scenario" "$@" >/dev/null 2>&1
grep '^REPORT' "$out/report.txt" | sed 's/^REPORT //' | grep -v '"kind":"shot"' || true
if grep -q panicked "$out/app.log"; then grep -A3 panicked "$out/app.log"; exit 1; fi
