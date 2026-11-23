# Development shell for crates/rustet-ui (the gpui UI).
#
#   nix-shell nix/ui-shell.nix
#
# gpui (Zed v1.29.2, crates/rustet-ui/Cargo.toml) renders through wgpu: Vulkan where the driver is
# conformant, OpenGL (EGL) otherwise. On Apple Silicon the app allows Mesa's Asahi Vulkan driver,
# which does not claim conformance (WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER, set by the app;
# =0 keeps OpenGL; third_party/gpui_wgpu/README.md). It loads the Vulkan loader, EGL/GL, fontconfig and
# libxkbcommon at run time and talks Wayland or X11 directly; NixOS has none of these on the
# default library path, so LD_LIBRARY_PATH carries them. The GPU drivers themselves come from the
# system (/run/opengl-driver: asahi on this machine, lavapipe/llvmpipe as the software fallback).
#
# libvlc is for the engine child the app starts (`rustet engine`); the UI does not load it (music
# video frames come through a shared-memory ring, crates/rustet-framering). The headless tools run
# the app without a window on the desktop (crates/rustet-ui/scripts/headless.sh); at-spi2-core and
# pyatspi let scripts/a11y-dump.py read the accessibility tree there. Builds go to the
# repository's target/ directory.
{ pkgs ? import <nixpkgs> { } }:

let
  runtimeLibs = with pkgs; [
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
  python = pkgs.python3.withPackages (ps: [ ps.pyatspi ps.pygobject3 ]);
in
pkgs.mkShell {
  packages = with pkgs; [
    pkg-config
    patchelf
    fontconfig
    freetype
    libxkbcommon
    libxcb
    wayland
    # Headless display servers, screenshots, inspection.
    sway
    grim
    xvfb-run
    xcompmgr
    xsetroot
    imagemagick
    xdotool
    wmctrl
    openbox
    wtype
    vulkan-tools
    dbus
    # Accessibility: the AT-SPI bus and a client to query it.
    at-spi2-core
    python
  ];

  VLC_PLUGIN_PATH = "${pkgs.libvlc}/lib/vlc/plugins";
  RUSTET_DBUS_SESSION_CONF = "${pkgs.dbus}/share/dbus-1/session.conf";
  # The private D-Bus session finds the AT-SPI bus launcher through XDG_DATA_DIRS.
  RUSTET_UI_ATSPI_DATA = "${pkgs.at-spi2-core}/share";
  GI_TYPELIB_PATH = pkgs.lib.concatMapStringsSep ":" (p: "${p}/lib/girepository-1.0") [
    pkgs.at-spi2-core
    pkgs.gobject-introspection
    pkgs.glib.out
  ];
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
}
