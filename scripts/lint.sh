#!/bin/sh
# Run the style and static checks for the Rust workspace and the shell scripts.
#   scripts/lint.sh        check
#   scripts/lint.sh --fix  apply rustfmt first
# On NixOS run it inside `nix-shell` (the gpui crates build against the libraries it provides).
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
fix=0; [ "${1:-}" = "--fix" ] && fix=1
fail=0
step() { printf '\n== %s\n' "$1"; }
run() { "$@" || fail=1; }

cd "$repo"
step "Rust: rustfmt"
[ "$fix" = 1 ] && cargo fmt --all
run cargo fmt --all --check

step "Rust: clippy"
run cargo clippy --workspace --all-targets -- -D warnings

step "Shell: shellcheck"
if command -v shellcheck >/dev/null 2>&1; then
    run shellcheck scripts/*.sh
else
    echo "shellcheck not installed (e.g. apt install shellcheck, or nix shell nixpkgs#shellcheck)"; fail=1
fi

if [ "$fail" = 0 ]; then printf '\nAll checks passed.\n'; else printf '\nSome checks failed.\n'; exit 1; fi
