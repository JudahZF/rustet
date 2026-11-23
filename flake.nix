{
  description = "Rustet, an Apple Music client for Linux with lossless playback";

  nixConfig = {
    extra-substituters = [ "https://judahzf.github.io/rustet" ];
    extra-trusted-public-keys = [
      "judahzf.github.io-rustet-1:a2doMiXEsZ/qMwXqkD5gs5XynSz3AKf9TNXS6IUarvE="
    ];
  };

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forSystems (pkgs: rec {
        rustet = pkgs.callPackage ./nix/package.nix { };
        default = rustet;
      });

      # The development shell of nix/ui-shell.nix with the Rust toolchain (CI uses it).
      devShells = forSystems (pkgs: {
        default = (import ./nix/ui-shell.nix { inherit pkgs; }).overrideAttrs (old: {
          nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.cmake
            pkgs.shellcheck
          ];
        });
      });
    };
}
