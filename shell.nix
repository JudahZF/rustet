# Development shell for Rustet: `nix-shell`, then `cargo build -p rustet`, or use scripts/dev-run.sh.
# The contents are in nix/ui-shell.nix (the libraries gpui loads at run time, libvlc for the
# engine, the headless tools for the UI checks).
{ pkgs ? import <nixpkgs> { } }: import ./nix/ui-shell.nix { inherit pkgs; }
