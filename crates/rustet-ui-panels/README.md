# rustet-ui-panels

The Settings and Downloads pages of Rustet's UI (spec §6.4, §1.5), over `rustet_app::App`.

    settings_page(&app, window, cx) -> AnyView    // Route::Settings { section }
    downloads_page(&app, window, cx) -> AnyView   // Route::Downloads

Every Settings control writes through `app.settings().update` and shows the stored value; a
refused value keeps the stored one and says why under the control. Dialogs, toasts and links
go to the window through `rustet_ui::host::Host::request`. The theme and link schemes are kept in
`App::prefs()`. `preview` (feature `preview`) draws built model values: the states that need an
Apple account or a running download.

The look is `docs/native-ui-look.md`, "Settings and Downloads". Both pages draw from the top of
the content area: they start `size.strip` down (the strip floats over them), end
`size.player_clearance` above the bottom (the player pill), and keep the 40 px gutter.

- Settings: the 34 / 700 title, a row of section tabs (quiet pills; Tab and Enter or Space),
  then the section's groups (`ui::group`): a 13 / 600 secondary label over a card of 44 px
  rows split by hairlines (`ui::row`, `ui::fact`, `ui::note`, the kit's `Toggle`). A row too
  narrow for its label and control puts the control under the label. A refusal is a row of
  its own under its control.
- Downloads: the title with "Open folder", then "Downloading" and "Finished" (17 / 700 heads;
  "Clear finished" at the right of its head). Jobs are 46 px rows in the track-list style:
  artwork, title, a secondary line with a progress line while it runs, the state, and a "more"
  glyph (or a right click) with the job's actions.
- Confirmations are the kit's `Dialog`: radius 14, the confirming answer is the key pill.

Build and check inside `nix-shell nix/ui-shell.nix`, with `CARGO_TARGET_DIR` unset:

    cargo test -p rustet-ui-panels --features test-support
    cargo clippy -p rustet-ui-panels --all-targets --features test-support -- -D warnings
    cargo build -p rustet-ui-panels --example panels --features preview

The example starts a real, signed-out `App` whose engine is the example itself
(`panels engine --api <port>`), in a state directory of its own (never `~/.config/rustet`):

    scripts/run.sh wayland tour    OUT --dark        # every section and Downloads, 1360, 820 and 480 px
    scripts/run.sh wayland preview OUT --light       # signed in, sync, scrobblers, jobs: built values
    scripts/run.sh wayland write   OUT --state-dir S # one control of each kind, by keyboard
    scripts/run.sh wayland verify  OUT --state-dir S # the same values after a restart
    scripts/a11y-check.sh OUT                        # AT-SPI: no interactive node without a name

Run them only in the headless compositor (`crates/rustet-ui/scripts/headless.sh`).
