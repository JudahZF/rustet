//! The mini player: a small window without decorations, kept above other windows where the
//! platform lets an app ask (X11; Wayland compositors do it with their own window rules).
//!
//! It is drawn in the neutral look: a page's artwork tint (a global of the theme) is for the
//! main window only. [`Untinted`] gives everything inside the mini player the untinted theme
//! while it lays out and paints.

use gpui::{
    AnyElement, App, AppContext, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
    InspectorElementId, LayoutId, MouseButton, Pixels, Role, Subscription, TitlebarOptions, Window,
    WindowBounds, WindowDecorations, WindowKind, WindowOptions, div, point, prelude::*, px, size,
};
use rustet_app::player::unix_ms;
use rustet_player::model::Command;
use rustet_ui::components::IconButton;
use rustet_ui::content::ArtworkView;
use rustet_ui::content::artwork::Shape;
use rustet_ui::icons::Icon;
use rustet_ui::theme::{ActiveTheme, SEMIBOLD, Theme};
use tracing::{debug, info, warn};

use crate::deck::{Primary, Scrubber, deck_drawing};
use crate::shell::Shell;

/// The mini player's width at zoom 1. Its height follows the artwork and the padding.
const WIDTH: f32 = 400.;
/// The window title compositor rules match on.
pub const TITLE: &str = "Rustet Mini Player";

/// Lays out and paints its child with the theme's page tint removed, then puts the tint back.
/// Components read the theme global while they lay out (`RenderOnce` and views render then),
/// so the swap covers them; it does not notify anyone (nothing observes the theme global).
struct Untinted(AnyElement);

impl Untinted {
    fn with<R>(cx: &mut App, f: impl FnOnce(&mut App) -> R) -> R {
        let tinted = cx
            .theme()
            .tint
            .is_some()
            .then(|| cx.global::<Theme>().clone());
        if let Some(theme) = &tinted {
            cx.set_global(theme.with_tint(None));
        }
        let out = f(cx);
        if let Some(theme) = tinted {
            cx.set_global(theme);
        }
        out
    }
}

impl Element for Untinted {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (Self::with(cx, |cx| self.0.request_layout(window, cx)), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::with(cx, |cx| self.0.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::with(cx, |cx| self.0.paint(window, cx));
    }
}

pub struct MiniPlayer {
    shell: Entity<Shell>,
    scrubber: Entity<Scrubber>,
    _subscriptions: Vec<Subscription>,
}

impl MiniPlayer {
    fn new(shell: Entity<Shell>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (prefs, known) = {
            let s = shell.read(cx);
            (s.config.prefs.clone(), s.config.position_known)
        };
        let scrubber = cx.new(|cx| {
            let mut s = Scrubber::new(cx);
            s.compact = true;
            s
        });
        let state = shell.read(cx).player.clone();
        scrubber.update(cx, |s, cx| s.set_state(state, cx));
        let mut subscriptions = vec![
            cx.observe(&shell, |this: &mut Self, shell, cx| {
                let (state, signed_in) =
                    (shell.read(cx).player.clone(), shell.read(cx).signed_in());
                this.scrubber.update(cx, |s, cx| {
                    s.signed_in = signed_in;
                    s.set_state(state, cx)
                });
                cx.notify();
            }),
            cx.subscribe(&scrubber, |this, _, command: &Command, cx| {
                let command = command.clone();
                this.shell.update(cx, |s, cx| s.send(command, cx));
            }),
        ];
        if known {
            // Where it was left is where it opens next time (X11; Wayland does not say).
            subscriptions.push(cx.observe_window_bounds(window, move |_, window, _| {
                let origin = window.bounds().origin;
                let at = [
                    f32::from(origin.x).round() as i32,
                    f32::from(origin.y).round() as i32,
                ];
                if let Err(e) = prefs.update(|p| p.mini_player_position = Some(at)) {
                    warn!("cannot save the mini player's place: {e}");
                }
            }));
        }
        // Closed by the window manager: the shell forgets it.
        let closing = shell.clone();
        window.on_window_should_close(cx, move |_, cx| {
            closing.update(cx, |s, cx| {
                s.mini = None;
                cx.notify();
            });
            true
        });
        Self {
            shell,
            scrubber,
            _subscriptions: subscriptions,
        }
    }

    fn send(&self, command: Command, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.send(command, cx));
    }
}

/// The mini player's height: the artwork and the padding around it.
fn height(theme: &Theme) -> Pixels {
    theme.size.art_card + theme.space.m * 2.
}

impl Render for MiniPlayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().with_tint(None);
        let colors = theme.colors;
        let shell = self.shell.read(cx);
        let ready = shell.connection == rustet_app::engine::Connection::Ready;
        let d = deck_drawing(shell.player.as_ref(), shell.signed_in(), unix_ms());
        let has_track = d.track.is_some();
        let (title, byline) = d
            .track
            .as_ref()
            .map_or(("Nothing playing".to_owned(), String::new()), |t| {
                (t.title.clone(), t.byline())
            });
        let primary = match d.primary {
            Primary::Pause => IconButton::new("mini-play", Icon::Pause, "Pause"),
            Primary::Play => IconButton::new("mini-play", Icon::Play, "Play"),
            Primary::Resume => IconButton::new("mini-play", Icon::Play, "Resume"),
        }
        .large();
        let window_buttons = div()
            .flex()
            .flex_none()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                IconButton::new("mini-expand", Icon::Maximize, "Show Rustet").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.shell.update(cx, |s, cx| {
                            s.show_main(cx);
                            s.toggle_mini(cx);
                        })
                    }),
                ),
            )
            .child(
                IconButton::new("mini-close", Icon::Close, "Close mini player").on_click(
                    cx.listener(|this, _, _, cx| this.shell.update(cx, |s, cx| s.toggle_mini(cx))),
                ),
            );
        let transport = div()
            .flex()
            .items_center()
            .gap(theme.space.s)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                IconButton::new("mini-previous", Icon::Previous, "Previous")
                    .disabled(!(ready && has_track))
                    .on_click(cx.listener(|this, _, _, cx| this.send(Command::Previous, cx))),
            )
            .child(
                primary
                    .disabled(!(ready && has_track))
                    .on_click(cx.listener(|this, _, _, cx| this.send(Command::Toggle, cx))),
            )
            .child(
                IconButton::new("mini-next", Icon::Next, "Next")
                    .disabled(!(ready && d.can_next))
                    .on_click(cx.listener(|this, _, _, cx| this.send(Command::Next, cx))),
            )
            .child(div().flex_1().min_w_0().child(self.scrubber.clone()));
        Untinted(
            rustet_ui::focus::traverse(div().id("mini-player"))
                .role(Role::Application)
                .aria_label(TITLE)
                .size_full()
                .flex()
                .items_center()
                .gap(theme.space.m)
                .p(theme.space.m)
                .bg(colors.sidebar)
                .border(theme.size.hairline)
                .border_color(colors.glass_stroke)
                .rounded(theme.radius.sheet)
                .text_color(colors.text)
                .font_family(rustet_ui::theme::FONT_FAMILY)
                .text_size(theme.text.body)
                .line_height(gpui::relative(theme.text.leading))
                .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                .child(
                    ArtworkView::new(
                        d.track.as_ref().and_then(|t| t.artwork.clone()),
                        theme.size.art_card,
                    )
                    .shape(Shape::Card),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .items_start()
                                .gap(theme.space.xs)
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .child(div().truncate().font_weight(SEMIBOLD).child(title))
                                        .when(!byline.is_empty(), |d| {
                                            d.child(
                                                div()
                                                    .truncate()
                                                    .text_size(theme.text.callout)
                                                    .text_color(colors.text_secondary)
                                                    .child(byline),
                                            )
                                        }),
                                )
                                .child(window_buttons),
                        )
                        .child(transport),
                )
                .into_any_element(),
        )
    }
}

impl Shell {
    /// Opens the mini player, or closes it.
    pub(crate) fn toggle_mini(&mut self, cx: &mut Context<Self>) {
        let open = match self.mini.take() {
            Some(mini) => {
                cx.update_window(mini.into(), |_, window, _| window.remove_window())
                    .ok();
                false
            }
            None => self.open_mini(cx),
        };
        let app = self.app.clone();
        let job =
            rustet_ui::runtime::spawn(cx, async move { app.set_mini_player_open(open).await });
        cx.background_spawn(async move { drop(job.await) }).detach();
        cx.notify();
    }

    fn open_mini(&mut self, cx: &mut Context<Self>) -> bool {
        if std::mem::replace(&mut self.opening_mini, true) {
            return true;
        }
        let theme = cx.theme();
        let sz = size(px(WIDTH * theme.zoom), height(theme));
        let saved = self.config.prefs.load().mini_player_position;
        let bounds = match saved {
            Some([x, y]) => Bounds::new(point(px(x as f32), px(y as f32)), sz),
            None => Bounds::centered(None, sz, cx),
        };
        let shell = cx.entity();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(TITLE.into()),
                ..Default::default()
            }),
            kind: WindowKind::Normal,
            is_resizable: false,
            is_minimizable: false,
            window_decorations: Some(WindowDecorations::Client),
            window_background: gpui::WindowBackgroundAppearance::Transparent,
            app_id: Some("rustet-mini-player".into()),
            ..Default::default()
        };
        // The mini player draws from this entity: open it once the update is over.
        cx.defer(move |cx| {
            let root = shell.clone();
            let opened = cx.open_window(options, move |window, cx| {
                cx.new(|cx| MiniPlayer::new(root, window, cx))
            });
            let handle = match opened {
                Ok(h) => h,
                Err(e) => {
                    shell.update(cx, |s, _| s.opening_mini = false);
                    return warn!("cannot open the mini player: {e}");
                }
            };
            shell.update(cx, |s, cx| {
                s.opening_mini = false;
                s.mini = Some(handle);
                cx.notify();
            });
            #[cfg(target_os = "linux")]
            cx.update_window(handle.into(), |_, window, _| {
                match rustet_ui::platform::set_always_on_top(window, true) {
                    Ok(()) => info!("mini player kept on top"),
                    Err(e) => debug!("mini player on top: {e}"),
                }
            })
            .ok();
        });
        true
    }
}
