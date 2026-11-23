# Rustet as a Nix package, for x86_64-linux and aarch64-linux (flake.nix: packages.<system>.rustet).
#
# Layout: $out/libexec/rustet/ holds the binary with the files it finds beside itself
# (scripts/build-package.sh lays out the same set):
#   rustet                    the app; `rustet engine` is the engine it starts
#   libhybris-core.so       the Android library loader for FairPlay, and hybris-linker/q.so
#   fetch-fairplay-libs.sh  downloads Apple's FairPlay libraries on first start, checked
#   fairplay-libs.lock        against the lock for this CPU; they cannot ship with Rustet
#   android-stubs/          stand-ins for three Android libraries Apple's libraries list
# $out/bin/rustet is a wrapper that gives the app the libraries gpui and libvlc load at run time.
#
# The libhybris files are prebuilt and stored with Git LFS. A flake input that fetches this
# repository must ask for LFS (`git+https://…?lfs=1`); without it the build stops with a message.
{
  lib,
  stdenv,
  rustPlatform,
  autoPatchelfHook,
  aws-lc,
  makeWrapper,
  pkg-config,
  fontconfig,
  freetype,
  libxkbcommon,
  libxcb,
  wayland,
  vulkan-loader,
  libGL,
  libx11,
  libxcursor,
  libxi,
  libvlc,
  sqlite,
  curl,
  coreutils,
  ffmpeg-headless,
}:

let
  workspace = lib.importTOML ../Cargo.toml;

  # The libhybris build and the FairPlay library list for this CPU.
  # Only the current CPU's LFS files are included in `src` so a CI build that pulls
  # just its own arch's LFS files produces the exact same store path as a full `?lfs=1` fetch.
  arch =
    {
      x86_64-linux = {
        hybris = "packaging/hybris-x86_64";
        lock = "packaging/fairplay-libs-x86_64.lock";
        hybrisFileset = lib.fileset.unions [
          ../packaging/fairplay-libs-x86_64.lock
          ../packaging/hybris-x86_64
        ];
      };
      aarch64-linux = {
        hybris = "packaging/hybris-aarch64";
        lock = "packaging/fairplay-libs-aarch64.lock";
        hybrisFileset = lib.fileset.unions [
          ../packaging/fairplay-libs-aarch64.lock
          ../packaging/hybris-aarch64
        ];
      };
    }
    .${stdenv.hostPlatform.system}
      or (throw "rustet: FairPlay has Android libraries for x86_64-linux and aarch64-linux only");

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../assets/tray-icon.png
      ../third_party/gpui_wgpu
      ../packaging/android-stubs
      ../packaging/rustet.desktop
      ../packaging/icons
      ../scripts/fetch-fairplay-libs.sh
      arch.hybrisFileset
    ];
  };

  # gpui opens these with dlopen (Vulkan, EGL, Wayland, X11, xkbcommon); the engine opens libvlc.
  runtimeLibs = [
    vulkan-loader
    libGL
    wayland
    libxkbcommon
    libxcb
    libx11
    libxcursor
    libxi
    fontconfig
    freetype
    libvlc
  ];
in
rustPlatform.buildRustPackage {
  pname = "rustet";
  inherit (workspace.workspace.package) version;
  inherit src;

  cargoLock = {
    lockFile = ../Cargo.lock;
    # gpui and its forks come from Git at the revisions Cargo.lock pins: one hash per repository
    # (any crate of it). After a gpui update, set the changed ones to lib.fakeHash and build:
    # Nix prints the right hash.
    outputHashes = {
      "async-process-2.5.0" = "sha256-Y37QTr6sjayv+JjGz00v0AMLQtsix1nH1lNKmky+6bU=";
      "async-task-4.7.1" = "sha256-503Yewi444XtYodEWIggMmVN3NKqCgtMBNegiMyJcCI=";
      "gpui-0.2.2" = "sha256-H5gYZ5uPc9kwuy27RMfxT70RdLaIWM5n9h0hOo1+LW0=";
      "calloop-0.14.3" = "sha256-5lWGGm6Wehf3RNFnhll0hMlUCXeVFHeoyHN4pbMS9zQ=";
      "khronos-egl-6.0.0" = "sha256-VeDtEMFI7C9b5INaIIVI3AO9akPxXiE11TiM3Gw/knU=";
      "wgpu-30.0.0" = "sha256-OqPeK1cr3OSZgIl+F3GMuXWz31PxFoOO0u4INN9GmgM=";
      "oo7-0.6.0" = "sha256-QXblWsnq+graWOSW0iA8pUzpwizDLi/kOSWDdSQVZ/0=";
      "wasm_thread-0.3.3" = "sha256-+lRLCIk0S6Y5ORYjDKsYYHia2FtoSoh+rWkQh7mnPBE=";
      "windows-capture-1.4.3" = "sha256-k07Jpk4mcDo1JsZDF7AZMHRqlqFKHt60G2X3BUjMhBc=";
      "zed-xim-0.4.0-zed" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
      "zed-font-kit-0.14.1-zed" = "sha256-KXygi0olNQi5yM8eaJVykNDtbPMDjT+cWPBF8UrtXR4=";
      "zed-scap-0.0.8-zed" = "sha256-WVcjceVESUhPKL9z/1y+l/vmtHRJS4EecaMOaVN686o=";
    };
  };

  cargoBuildFlags = [
    "-p"
    "rustet"
  ];
  # CI runs the tests (.github/workflows/ci.yml); here they would need a display and libvlc.
  doCheck = false;

  nativeBuildInputs = [
    autoPatchelfHook
    makeWrapper
    pkg-config
  ];
  buildInputs = [
    aws-lc # rustls' crypto; nixpkgs' Rust builds link the system one
    fontconfig
    freetype
    libxkbcommon
    libxcb
    sqlite
    wayland
    stdenv.cc.cc.lib # libstdc++ for libhybris
  ];

  postInstall = ''
    real_elf() {
      [ "$(head -c4 "$1" | od -An -c | tr -d ' ')" = '177ELF' ] \
        || { echo "$1 is not a binary: fetch this repository with Git LFS (?lfs=1)" >&2; exit 1; }
    }
    real_elf ${arch.hybris}/libhybris-core.so
    real_elf ${arch.hybris}/hybris-linker/q.so

    dir=$out/libexec/rustet
    mkdir -p $dir/hybris-linker $dir/android-stubs
    mv $out/bin/rustet $dir/rustet
    install -m755 ${arch.hybris}/libhybris-core.so $dir/libhybris-core.so
    install -m755 ${arch.hybris}/hybris-linker/q.so $dir/hybris-linker/q.so
    install -m755 scripts/fetch-fairplay-libs.sh $dir/fetch-fairplay-libs.sh
    install -m644 ${arch.lock} $dir/fairplay-libs.lock

    # Apple's libraries list libandroid, libOpenSLES and libdl but import nothing from them.
    flags="-shared -fPIC -nostdlib -O2 -Wl,--hash-style=both"
    for name in libandroid.so libOpenSLES.so; do
      $CC $flags -Wl,-soname,$name -o $dir/android-stubs/$name packaging/android-stubs/empty_stub.c
    done
    $CC $flags -Wl,-soname,libdl.so -Wl,--version-script=packaging/android-stubs/libdl.map \
      -o $dir/android-stubs/libdl.so packaging/android-stubs/empty_stub.c

    makeWrapper $dir/rustet $out/bin/rustet \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs} \
      --set-default VLC_PLUGIN_PATH ${libvlc}/lib/vlc/plugins \
      --prefix PATH : ${
        lib.makeBinPath [
          curl
          coreutils
          ffmpeg-headless
        ]
      }

    install -Dm644 packaging/rustet.desktop $out/share/applications/rustet.desktop
    install -Dm644 packaging/icons/rustet.svg $out/share/icons/hicolor/scalable/apps/rustet.svg
    for size in 128 256 512; do
      install -Dm644 packaging/icons/rustet-$size.png $out/share/icons/hicolor/''${size}x''${size}/apps/rustet.png
    done
  '';

  meta = {
    description = "Apple Music client with lossless playback";
    homepage = "https://github.com/JudahZF/rustet";
    license = lib.licenses.mit;
    mainProgram = "rustet";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
