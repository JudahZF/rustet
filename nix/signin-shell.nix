# Development shell for `crates/rustet-signin` (the wry / WebKitGTK sign-in window).
#
#   nix-shell nix/signin-shell.nix
#
# Build: wry 0.57 links WebKitGTK 4.1 (GTK 3, libsoup 3) through pkg-config.
# Run: WebKitGTK does HTTPS through GIO, which needs the glib-networking TLS module
# (GIO_EXTRA_MODULES); without it every page fails with a TLS error. GStreamer is not needed
# for the sign-in page (no media plays there).
# The headless compositor tools match ../shell.nix (crates/rustet-shell/scripts/headless.sh);
# wtype types into the headless sway session for the "form accepts input" check.
{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [ pkg-config ];
  buildInputs = with pkgs; [
    webkitgtk_4_1
    gtk3
    libsoup_3
    glib
    glib-networking
  ];
  packages = with pkgs; [
    sway
    grim
    wtype
    xvfb-run
    xcompmgr
    xsetroot
    imagemagick
    dbus
  ];

  GIO_EXTRA_MODULES = "${pkgs.glib-networking}/lib/gio/modules";
  RUSTET_DBUS_SESSION_CONF = "${pkgs.dbus}/share/dbus-1/session.conf";
}
