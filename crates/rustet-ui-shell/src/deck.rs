//! The player pill (`docs/native-ui-look.md`, "Player pill"): the transport at the left
//! (shuffle, previous, play or pause, next, repeat), the centred "LCD" (artwork, title, the
//! artist line with the quality badge, the progress line along its bottom), and at the right
//! lyrics, the queue, volume and the mini player. The frame draws the glass pill; this module
//! draws what is in it. [`Room`] decides what fits the pill's width.
//!
//! [`deck_drawing`] turns a [`PlayerState`] into what the pill shows, for every status. The
//! [`Scrubber`] (the progress line) redraws the position on a timer while playing (the engine
//! sends positions about once a second and may stop near a track's end), without redrawing the
//! window.

use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, Focusable, Pixels, Role, SharedString, Subscription,
    Task, Window, div, prelude::*,
};
use rustet_app::pages::model::Artwork;
use rustet_app::player::{position_now, unix_ms};
use rustet_app::route::Route;
use rustet_player::model::{Command, ItemKind, Mode, PlayerState, Repeat, Seek, Status, Volume};
use rustet_ui::components::{FocusRing, Glass, IconButton, Slider, SliderEvent};
use rustet_ui::content::artwork::Shape;
use rustet_ui::content::{ArtworkView, quality_badge, quality_mark};
use rustet_ui::format;
use rustet_ui::icons::Icon;
use rustet_ui::theme::{ActiveTheme, SEMIBOLD, Theme};

use crate::shell::{Panel, Shell};

/// How often the position redraws while playing.
const TICK: Duration = Duration::from_millis(250);

/// The big button of the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primary {
    Play,
    Pause,
    /// The queue of the last session is back, paused where it stopped.
    Resume,
}

/// The playing track as the deck names it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackLine {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub artwork: Option<Artwork>,
    /// Where the title goes.
    pub route: Route,
}

/// What the deck draws for a player state.
#[derive(Debug, Clone, PartialEq)]
pub struct DeckDrawing {
    pub track: Option<TrackLine>,
    pub primary: Primary,
    /// Said in place of the artist line: "Opening…", "Loading…", or what failed.
    pub status: Option<String>,
    pub failed: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// The scrubber takes a seek.
    pub seekable: bool,
    pub can_previous: bool,
    pub can_next: bool,
}

/// What to say about the player's failure. Signed out, a track fails for want of the sign-in,
/// whatever the engine's words for it ("not signed in: no music user token", "Couldn't play 3
/// tracks in a row").
pub fn failure_message(state: &PlayerState, signed_in: bool) -> String {
    use rustet_player::model::ErrorCode;
    match &state.error {
        Some(e)
            if !signed_in
                && matches!(
                    e.code,
                    ErrorCode::OpenFailed | ErrorCode::TooManyFailures | ErrorCode::AuthExpired
                ) =>
        {
            "Sign in to Apple Music to play music.".to_owned()
        }
        Some(e) if !e.message.is_empty() => e.message.clone(),
        _ => "This track could not be played. Try again or play another.".to_owned(),
    }
}

/// The deck for `state` at `now_ms`; `None` is the engine before its first snapshot.
pub fn deck_drawing(state: Option<&PlayerState>, signed_in: bool, now_ms: u64) -> DeckDrawing {
    let Some(state) = state else {
        return DeckDrawing {
            track: None,
            primary: Primary::Play,
            status: None,
            failed: false,
            position_ms: 0,
            duration_ms: 0,
            seekable: false,
            can_previous: false,
            can_next: false,
        };
    };
    let track = state.track.as_ref().map(|t| TrackLine {
        title: t.meta.name.clone(),
        artist: t.meta.artist_name.clone(),
        album: t.meta.album_name.clone(),
        artwork: (!t.meta.artwork_url.is_empty()).then(|| Artwork {
            template: t.meta.artwork_url.clone(),
            width: None,
            height: None,
            palette: None,
        }),
        route: match t.kind {
            ItemKind::Song => Route::Song { id: t.id.0.clone() },
            ItemKind::MusicVideo => Route::MusicVideo { id: t.id.0.clone() },
        },
    });
    let duration_ms = if state.duration_ms > 0 {
        state.duration_ms
    } else {
        state.track.as_ref().map_or(0, |t| t.meta.duration_ms)
    };
    let (primary, status, failed) = match state.status {
        Status::Opening => (Primary::Pause, Some("Opening…".to_owned()), false),
        Status::Loading => (Primary::Pause, Some("Loading…".to_owned()), false),
        Status::Playing => (Primary::Pause, None, false),
        Status::Failed => (Primary::Play, Some(failure_message(state, signed_in)), true),
        Status::Idle | Status::Paused | Status::Stopped if state.restored && track.is_some() => {
            (Primary::Resume, None, false)
        }
        Status::Idle | Status::Paused | Status::Stopped => (Primary::Play, None, false),
    };
    let has_track = track.is_some();
    DeckDrawing {
        track,
        primary,
        status,
        failed,
        position_ms: if has_track {
            position_now(state, now_ms)
        } else {
            0
        },
        duration_ms,
        seekable: has_track
            && duration_ms > 0
            && !matches!(state.status, Status::Opening | Status::Failed),
        can_previous: state.can_previous,
        can_next: state.can_next,
    }
}

/// The next repeat mode of the repeat button.
pub fn next_repeat(repeat: Repeat) -> Repeat {
    match repeat {
        Repeat::Off => Repeat::All,
        Repeat::All => Repeat::One,
        Repeat::One => Repeat::Off,
    }
}

impl TrackLine {
    /// "Artist — Album", or whichever of the two the track has.
    pub fn byline(&self) -> String {
        match (self.artist.is_empty(), self.album.is_empty()) {
            (false, false) => format!("{} — {}", self.artist, self.album),
            (false, true) => self.artist.clone(),
            (true, _) => self.album.clone(),
        }
    }
}

/// What the pill has room for at its width, fewest controls first. The LCD never gets
/// narrower than `size.player_lcd_min`: the pill drops controls instead, and every text in it
/// truncates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Room {
    /// Play or pause, the LCD, the queue.
    Tiny,
    /// Previous, play or pause, next; the LCD; the queue.
    Narrow,
    /// Adds lyrics and volume.
    Medium,
    /// Everything: shuffle and repeat, the mini player.
    Full,
}

impl Room {
    /// The room in a pill `width` wide (zoom included, as the theme's sizes are).
    pub fn for_width(width: Pixels, theme: &Theme) -> Self {
        let (button, play) = (theme.size.control, theme.size.circle);
        // Side padding, the gaps around the LCD, the LCD, and play: in every room.
        let fixed = theme.space.m * 2. + theme.space.s * 2. + theme.size.player_lcd_min + play;
        // Full: four transport buttons and four at the right; Medium: two and three;
        // Narrow: two and one.
        [(Self::Full, 8.), (Self::Medium, 5.), (Self::Narrow, 3.)]
            .into_iter()
            .find(|&(_, buttons)| width >= fixed + button * buttons)
            .map_or(Self::Tiny, |(room, _)| room)
    }
}

/// The progress line along the bottom of the LCD: the seek slider drawn 3 px thin, with the
/// elapsed and remaining times at its ends while the pointer is on the LCD (hover group
/// `lcd`; the knob shows on the line itself). In the mini player (`compact`) the times always
/// show.
pub struct Scrubber {
    state: Option<PlayerState>,
    seek: Entity<Slider>,
    /// The position under the pointer while the user drags.
    dragging_ms: Option<u64>,
    /// The mini player's line: the times always show.
    pub compact: bool,
    /// Failures read differently signed out.
    pub signed_in: bool,
    ticker: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<Command> for Scrubber {}

impl Scrubber {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let seek = cx.new(|cx| Slider::new(0., "Position", cx).progress());
        let subscription = cx.subscribe(&seek, |this: &mut Self, _, e: &SliderEvent, cx| {
            let drawing = deck_drawing(this.state.as_ref(), this.signed_in, unix_ms());
            if !drawing.seekable {
                return;
            }
            let at = |v: f32| (f64::from(v) * drawing.duration_ms as f64).round() as u64;
            match *e {
                SliderEvent::Changed(v) => this.dragging_ms = Some(at(v)),
                SliderEvent::Committed(v) => {
                    this.dragging_ms = None;
                    let generation = this.state.as_ref().map(|s| s.generation);
                    cx.emit(Command::Seek(Seek {
                        position_ms: at(v),
                        expect_generation: generation,
                    }));
                }
            }
            cx.notify();
        });
        Self {
            state: None,
            seek,
            dragging_ms: None,
            compact: false,
            signed_in: false,
            ticker: None,
            _subscription: subscription,
        }
    }

    pub fn set_state(&mut self, state: Option<PlayerState>, cx: &mut Context<Self>) {
        let playing = state.as_ref().is_some_and(|s| s.status == Status::Playing);
        self.state = state;
        if playing && self.ticker.is_none() {
            self.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    if this.update(cx, |s, cx| s.tick(cx)).is_err() {
                        return;
                    }
                }
            }));
        } else if !playing {
            self.ticker = None;
        }
        self.tick(cx);
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn seek_slider(&self) -> Entity<Slider> {
        self.seek.clone()
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let d = deck_drawing(self.state.as_ref(), self.signed_in, unix_ms());
        let value = if d.duration_ms > 0 {
            d.position_ms as f32 / d.duration_ms as f32
        } else {
            0.
        };
        self.seek.update(cx, |s, cx| s.set_value(value, cx));
        cx.notify();
    }
}

impl Render for Scrubber {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let d = deck_drawing(self.state.as_ref(), self.signed_in, unix_ms());
        let time = |ms: u64| format::duration(Duration::from_millis(ms));
        let (elapsed, remaining) = if d.track.is_some() && d.duration_ms > 0 {
            let at = self.dragging_ms.unwrap_or(d.position_ms).min(d.duration_ms);
            (time(at), format!("-{}", time(d.duration_ms - at)))
        } else {
            (String::new(), String::new())
        };
        let compact = self.compact;
        let label = |text: String| {
            div()
                .flex_none()
                .text_size(theme.text.footnote)
                .font_features(theme.tabular())
                .text_color(theme.colors.text_secondary)
                .when(!compact, |d| {
                    d.invisible().group_hover("lcd", |s| s.visible())
                })
                .child(text)
        };
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(theme.space.s6)
            .child(label(elapsed))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(!d.seekable, |e| e.opacity(0.4))
                    .child(self.seek.clone()),
            )
            .child(label(remaining))
    }
}

impl Shell {
    /// The pill's centre: artwork, title and artist line over the progress line. With nothing
    /// loaded, one tertiary note.
    fn lcd(&self, d: &DeckDrawing, room: Room, ready: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let lcd = div()
            .id("lcd")
            .group("lcd")
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.lcd_hover = *hovered;
                cx.notify();
            }))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap(theme.space.s);
        let Some(track) = &d.track else {
            let label = if ready || self.player.is_some() {
                "Nothing playing"
            } else {
                "Connecting to the player…"
            };
            return lcd
                .justify_center()
                .child(
                    div()
                        .id("deck-idle")
                        .role(Role::Status)
                        .aria_label(label)
                        .child(
                            Icon::Song
                                .element()
                                .size(theme.size.icon_large)
                                .text_color(colors.text_tertiary),
                        ),
                )
                .into_any_element();
        };

        let art = div()
            .id("deck-artwork")
            .role(Role::Button)
            .aria_label("Now Playing")
            .tab_index(0)
            .focus_ring_on(colors.glass, cx)
            .flex_none()
            .cursor_pointer()
            .rounded(theme.radius.thumb)
            .on_click(cx.listener(|this, _, _, cx| {
                this.now_playing_open = !this.now_playing_open;
                cx.notify();
            }))
            .child(ArtworkView::new(track.artwork.clone(), theme.size.art_row).shape(Shape::Thumb));

        let route = track.route.clone();
        let title = div()
            .id("deck-title")
            .role(Role::Link)
            .aria_label(SharedString::from(track.title.clone()))
            .tab_index(0)
            .max_w_full()
            .truncate()
            .text_size(theme.text.body)
            .font_weight(SEMIBOLD)
            .text_color(colors.text)
            .cursor_pointer()
            .hover(|s| s.underline())
            .on_click(cx.listener(move |this, _, _, cx| this.go(route.clone(), cx)))
            .child(track.title.clone());

        // The track carries no artist ID: the artist line searches for the artist.
        let artist = track.artist.clone();
        let byline = div()
            .id("deck-artist")
            .role(Role::Link)
            .aria_label(SharedString::from(track.byline()))
            .tab_index(0)
            .min_w_0()
            .truncate()
            .cursor_pointer()
            .hover(|s| s.underline())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.go(
                    Route::Search {
                        term: artist.clone(),
                    },
                    cx,
                )
            }))
            .child(track.byline());
        let quality = self.player.as_ref().and_then(|s| s.quality);
        let second: AnyElement = match &d.status {
            // What the player is doing, or what failed, in place of the artist line.
            Some(status) => div()
                .id("deck-status")
                .role(Role::Status)
                .aria_label(SharedString::from(status.clone()))
                .max_w_full()
                .flex()
                .items_center()
                .gap(theme.space.xs)
                .text_color(if d.failed {
                    colors.text
                } else {
                    colors.text_secondary
                })
                .when(d.failed, |e| {
                    e.child(
                        Icon::Alert
                            .element()
                            .size(theme.text.callout)
                            .text_color(colors.text_secondary),
                    )
                })
                .child(div().min_w_0().truncate().child(status.clone()))
                .into_any_element(),
            None => div()
                .max_w_full()
                .flex()
                .items_center()
                .gap(theme.space.s6)
                .text_color(colors.text_secondary)
                // The queue of the last session, paused where it stopped.
                .when(d.primary == Primary::Resume, |e| {
                    e.child(
                        div()
                            .id("deck-resume")
                            .role(Role::Button)
                            .aria_label("Resume")
                            .tab_index(0)
                            .flex_none()
                            .font_weight(SEMIBOLD)
                            .text_color(colors.text)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_click(cx.listener(|this, _, _, cx| this.send(Command::Play, cx)))
                            .child("Resume"),
                    )
                })
                .child(byline)
                // At rest the quality badge; under the pointer the stream's real format
                // ("ALAC 24-bit 96 kHz") takes its place. Both at once leave the artist line
                // no room in the pill.
                .when(room > Room::Tiny && !self.lcd_hover, |e| {
                    e.children(
                        quality
                            .and_then(|q| quality_badge(&q))
                            .map(|b| quality_mark(&theme, b)),
                    )
                })
                .when(
                    room > Room::Tiny && self.lcd_hover && quality.is_some(),
                    |e| e.child(div().flex_none().child(self.quality.clone())),
                )
                .into_any_element(),
        };

        lcd.child(art)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    // The two lines sit above the progress line.
                    .pb(theme.space.xs)
                    .text_size(theme.text.callout)
                    .child(title)
                    .child(second)
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .child(self.scrubber.clone()),
                    ),
            )
            .into_any_element()
    }

    /// The speaker; on hover, or with the keyboard on the slider, a glass capsule over it
    /// holds the slider beside the speaker. Closed, the slider stays in the Tab order at zero
    /// width, so Tab reaches it (and opens the capsule).
    fn volume_control(
        &mut self,
        ready: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let (level, muted) = self
            .player
            .as_ref()
            .map_or((100, false), |s| (s.volume, s.muted));
        let keyboard =
            self.volume.focus_handle(cx).is_focused(window) && window.last_input_was_keyboard();
        let open = ready && (self.volume_hover || keyboard || self.volume.read(cx).is_dragging());
        let speaker = |id: &'static str, cx: &mut Context<Self>| {
            IconButton::new(
                id,
                if muted {
                    Icon::VolumeMuted
                } else {
                    Icon::Volume
                },
                if muted { "Unmute" } else { "Mute" },
            )
            .tooltip_above()
            .disabled(!ready)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.send(
                    Command::Volume(Volume {
                        level,
                        muted: Some(!muted),
                    }),
                    cx,
                )
            }))
        };
        let slider = div()
            .w(theme.size.player_volume)
            .when(muted, |e| e.opacity(0.5))
            .child(self.volume.clone());
        div()
            .id("volume")
            .relative()
            .flex_none()
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                // Once open, the capsule covers the speaker: only it says the pointer left.
                if *hovered || !open {
                    this.volume_hover = *hovered;
                    cx.notify();
                }
            }))
            .child(speaker("deck-mute", cx))
            .child(if open {
                div()
                    .id("volume-capsule")
                    .occlude()
                    .absolute()
                    .top(-theme.space.xs)
                    .right(-theme.space.xs)
                    .h(theme.size.control + theme.space.xs * 2.)
                    .pl(theme.space.m)
                    .pr(theme.space.xs)
                    .flex()
                    .items_center()
                    .gap(theme.space.s)
                    .rounded_full()
                    .glass(cx)
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.volume_hover = *hovered;
                        cx.notify();
                    }))
                    .child(slider)
                    .child(speaker("deck-mute-open", cx))
                    .into_any_element()
            } else {
                div()
                    .absolute()
                    .w_0()
                    .overflow_hidden()
                    .child(slider)
                    .into_any_element()
            })
            .into_any_element()
    }

    /// The player pill's content (`FrameParts::player`), with what fits `room`.
    pub(crate) fn deck(
        &mut self,
        room: Room,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let d = deck_drawing(self.player.as_ref(), self.signed_in(), unix_ms());
        let ready = self.connection == rustet_app::engine::Connection::Ready;
        // With nothing loaded every control is faint.
        let live = ready && d.track.is_some();
        let (shuffle, repeat) = self
            .player
            .as_ref()
            .map_or((false, Repeat::Off), |s| (s.shuffle, s.repeat));

        let primary = match d.primary {
            Primary::Play => IconButton::new("deck-play", Icon::Play, "Play"),
            Primary::Pause => IconButton::new("deck-play", Icon::Pause, "Pause"),
            Primary::Resume => IconButton::new("deck-play", Icon::Play, "Resume"),
        }
        .large()
        .tooltip_above()
        .disabled(!live)
        .on_click(cx.listener(|this, _, _, cx| this.send(Command::Toggle, cx)));

        let transport = div()
            .flex()
            .flex_none()
            .items_center()
            .when(room == Room::Full, |e| {
                e.child(
                    IconButton::new("deck-shuffle", Icon::Shuffle, "Shuffle")
                        .tooltip_above()
                        .selected(shuffle)
                        .disabled(!live)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let mode = Mode {
                                shuffle: Some(!shuffle),
                                ..Mode::default()
                            };
                            this.send(Command::Mode(mode), cx)
                        })),
                )
            })
            .when(room >= Room::Narrow, |e| {
                e.child(
                    IconButton::new("deck-previous", Icon::Previous, "Previous")
                        .tooltip_above()
                        .disabled(!live)
                        .on_click(cx.listener(|this, _, _, cx| this.send(Command::Previous, cx))),
                )
            })
            .child(primary)
            .when(room >= Room::Narrow, |e| {
                e.child(
                    IconButton::new("deck-next", Icon::Next, "Next")
                        .tooltip_above()
                        .disabled(!(ready && d.can_next))
                        .on_click(cx.listener(|this, _, _, cx| this.send(Command::Next, cx))),
                )
            })
            .when(room == Room::Full, |e| {
                e.child(
                    div()
                        .relative()
                        .child(
                            IconButton::new(
                                "deck-repeat",
                                Icon::Repeat,
                                match repeat {
                                    Repeat::Off => "Repeat",
                                    Repeat::All => "Repeat all",
                                    Repeat::One => "Repeat one",
                                },
                            )
                            .tooltip_above()
                            .selected(repeat != Repeat::Off)
                            .disabled(!live)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    let mode = Mode {
                                        repeat: Some(next_repeat(repeat)),
                                        ..Mode::default()
                                    };
                                    this.send(Command::Mode(mode), cx)
                                },
                            )),
                        )
                        // Repeat one: a small "1" at the glyph's corner.
                        .when(repeat == Repeat::One, |e| {
                            e.child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .right(theme.space.xxs)
                                    .text_size(theme.text.footnote)
                                    .font_weight(SEMIBOLD)
                                    .text_color(theme.colors.text)
                                    .child("1"),
                            )
                        }),
                )
            });

        let queue_open = |panel| self.panel == Some(panel) && !self.now_playing_open;
        let extras = div()
            .flex()
            .flex_none()
            .items_center()
            .when(room >= Room::Medium, |e| {
                e.child(
                    IconButton::new("deck-lyrics", Icon::Lyrics, "Lyrics")
                        .tooltip_above()
                        .selected(queue_open(Panel::Lyrics))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.toggle_panel(Panel::Lyrics, cx)),
                        ),
                )
            })
            .child(
                IconButton::new("deck-queue", Icon::Queue, "Playing Next")
                    .tooltip_above()
                    .selected(queue_open(Panel::Queue))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_panel(Panel::Queue, cx))),
            )
            .when(room >= Room::Medium, |e| {
                e.child(self.volume_control(ready, window, cx))
            })
            .when(room == Room::Full, |e| {
                e.child(
                    IconButton::new("deck-mini", Icon::MiniPlayer, "Mini player")
                        .tooltip_above()
                        .selected(self.mini.is_some())
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_mini(cx))),
                )
            });

        div()
            .id("deck")
            .role(Role::Toolbar)
            .aria_label("Player")
            .size_full()
            .px(theme.space.m)
            .flex()
            .items_center()
            .gap(theme.space.s)
            .child(transport)
            .child(self.lcd(&d, room, ready, cx))
            .child(extras)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use rustet_player::model::{CatalogId, ErrorCode, PlayerError, QueueItem, TrackMeta};

    use super::*;

    fn track() -> QueueItem {
        QueueItem {
            uid: 7,
            id: CatalogId("1440833098".into()),
            library_id: None,
            kind: ItemKind::Song,
            source: rustet_player::model::ItemSource::User,
            meta: TrackMeta {
                name: "Motion Sickness".into(),
                artist_name: "Phoebe Bridgers".into(),
                album_name: "Stranger in the Alps".into(),
                artwork_url: "https://is1.example/{w}x{h}bb.jpg".into(),
                duration_ms: 229_000,
                audio_traits: vec![],
                content_rating: None,
            },
            container_id: None,
        }
    }

    fn state(status: Status) -> PlayerState {
        PlayerState {
            status,
            track: Some(track()),
            position_ms: 60_000,
            position_at: 1_000_000,
            duration_ms: 229_000,
            can_next: true,
            ..PlayerState::default()
        }
    }

    #[test]
    fn every_status_has_a_drawing() {
        let before = deck_drawing(None, true, 0);
        assert_eq!(
            (before.track, before.seekable),
            (None, false),
            "no snapshot yet: nothing to drive"
        );

        let idle = deck_drawing(Some(&PlayerState::default()), true, 0);
        assert_eq!(
            (idle.track, idle.primary, idle.status),
            (None, Primary::Play, None)
        );

        let opening = deck_drawing(Some(&state(Status::Opening)), true, 1_000_000);
        assert_eq!(
            (opening.primary, opening.status.as_deref()),
            (Primary::Pause, Some("Opening…"))
        );
        assert!(!opening.seekable, "no seek before the stream is open");

        let loading = deck_drawing(Some(&state(Status::Loading)), true, 1_000_000);
        assert_eq!(loading.status.as_deref(), Some("Loading…"));
        assert!(loading.seekable);

        let playing = deck_drawing(Some(&state(Status::Playing)), true, 1_002_000);
        assert_eq!(
            (playing.primary, playing.status.clone()),
            (Primary::Pause, None)
        );
        assert_eq!(
            playing.position_ms, 62_000,
            "the position runs on between events"
        );
        assert_eq!(
            playing.track.unwrap().route,
            Route::Song {
                id: "1440833098".into()
            }
        );

        let paused = deck_drawing(Some(&state(Status::Paused)), true, 1_002_000);
        assert_eq!(
            (paused.primary, paused.position_ms),
            (Primary::Play, 60_000)
        );

        let mut restored = state(Status::Paused);
        restored.restored = true;
        assert_eq!(
            deck_drawing(Some(&restored), true, 0).primary,
            Primary::Resume
        );

        let mut failed = state(Status::Failed);
        failed.error = Some(PlayerError {
            code: ErrorCode::OpenFailed,
            message: "Sign in to Apple Music to play this.".into(),
        });
        let f = deck_drawing(Some(&failed), true, 0);
        assert!(f.failed);
        assert_eq!(
            f.status.as_deref(),
            Some("Sign in to Apple Music to play this.")
        );
        assert_eq!(
            deck_drawing(Some(&failed), false, 0).status.as_deref(),
            Some("Sign in to Apple Music to play music."),
            "signed out, the sign-in is what is missing"
        );
        failed.error = None;
        assert!(
            deck_drawing(Some(&failed), true, 0)
                .status
                .unwrap()
                .starts_with("This track could not be played")
        );
    }

    #[test]
    fn the_length_comes_from_the_track_before_the_stream_reports_it() {
        let mut s = state(Status::Opening);
        s.duration_ms = 0;
        assert_eq!(deck_drawing(Some(&s), true, 0).duration_ms, 229_000);
    }

    #[test]
    fn the_pill_drops_controls_before_the_lcd_gets_too_narrow() {
        let theme = Theme::new(rustet_ui::theme::Appearance::Dark);
        let room = |w: f32| Room::for_width(gpui::px(w), &theme);
        assert_eq!(
            room(668.),
            Room::Full,
            "the pill at its widest has everything"
        );
        assert_eq!(room(480.), Room::Medium);
        assert_eq!(room(400.), Room::Narrow);
        assert_eq!(room(300.), Room::Tiny);
    }

    #[test]
    fn repeat_cycles_off_all_one() {
        assert_eq!(next_repeat(Repeat::Off), Repeat::All);
        assert_eq!(next_repeat(Repeat::All), Repeat::One);
        assert_eq!(next_repeat(Repeat::One), Repeat::Off);
    }
}
