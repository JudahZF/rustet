//! Now Playing (spec §6.1): the large artwork over its blurred backdrop, the lyrics following
//! the position (a tap on a line seeks), and the details of the stream that plays. The same
//! lyrics also fill the lyrics panel ([`NowPlaying::lyrics_panel`]).
//!
//! A music video shows in place of the artwork, with the lyrics beside it, a switch to audio
//! only and full screen (F, a double click; Escape leaves). The video's frame ring is read only
//! while Now Playing is on screen: elsewhere the video goes on as sound (the engine keeps
//! decoding, so coming back shows it at once and the sound never reloads).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Entity, EventEmitter, Role, SharedString, Subscription, Task, Window, div,
    prelude::*,
};
use rustet_app::App as RustetApp;
use rustet_app::pages::LoadError;
use rustet_app::pages::lyrics::{self, Lyrics};
use rustet_app::player::{position_now, unix_ms};
use rustet_player::model::{Command, ItemKind, PlayerState, Quality, QualityTier, Seek, Status};
use rustet_ui::artwork::DiskCache;
use rustet_ui::backdrop::Backdrop;
use rustet_ui::components::{Button, EmptyStateView};
use rustet_ui::content::artwork::Shape;
use rustet_ui::content::{ArtworkView, LyricsEvent, LyricsView, quality_text};
use rustet_ui::theme::{ActiveTheme, BOLD, SEMIBOLD};
use rustet_ui::video::{VideoSource, VideoView};
use tracing::debug;

use crate::deck::deck_drawing;

/// The lyrics of the track shown.
enum LyricsState {
    Loading,
    Ready {
        view: Entity<LyricsView>,
        _seek: Subscription,
    },
    /// Apple has none for this song.
    Missing,
    SignIn,
    Failed,
}

/// The facts of a stream, as label and value, and a note when the player fell back.
pub fn quality_details(q: &Quality) -> (Vec<(&'static str, String)>, Option<String>) {
    let mut rows = vec![(
        "Format",
        match q.tier {
            QualityTier::Aac => "AAC",
            QualityTier::Lossless => "Lossless",
            QualityTier::HiResLossless => "Hi-Res Lossless",
            QualityTier::Spatial => "Spatial audio",
            QualityTier::DolbyAtmos => "Dolby Atmos",
            QualityTier::Video => "Video",
        }
        .to_owned(),
    )];
    if let Some(bits) = q.bit_depth {
        rows.push(("Bit depth", format!("{bits}-bit")));
    }
    if let Some(rate) = q.sample_rate {
        rows.push((
            "Sample rate",
            format!("{:.1} kHz", f64::from(rate) / 1000.).replace(".0 ", " "),
        ));
    }
    if let Some(bps) = q.bitrate {
        rows.push(("Bitrate", rustet_ui::format::kbps(bps)));
    }
    if let Some(ch) = q.channels {
        rows.push((
            "Channels",
            match ch {
                1 => "Mono".to_owned(),
                2 => "Stereo".to_owned(),
                n => format!("{n} channels"),
            },
        ));
    }
    let note = q.fell_back.then(|| {
        format!(
            "Lossless isn’t available for this track. Playing {}.",
            quality_text(q)
        )
    });
    (rows, note)
}

/// The video view's phase, counters and frame size.
#[cfg(feature = "scenario")]
pub(crate) type VideoProbe = (
    rustet_ui::video::Phase,
    rustet_ui::video::VideoStats,
    Option<(u32, u32)>,
);

/// What Now Playing asks of the window for a music video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRequest {
    ToggleFullscreen,
    /// Play music videos as audio only (`true`) or with their video.
    AudioOnly(bool),
}

pub struct NowPlaying {
    app: RustetApp,
    state: Option<PlayerState>,
    /// Made with the window at the first music video.
    video: Option<Entity<VideoView>>,
    /// Now Playing is on screen (the video ring is read only then).
    open: bool,
    fullscreen: bool,
    /// The song the lyrics and backdrop are for.
    shown: Option<String>,
    lyrics: LyricsState,
    backdrop: Entity<Backdrop>,
    loading: Option<Task<()>>,
}

impl EventEmitter<Command> for NowPlaying {}
impl EventEmitter<VideoRequest> for NowPlaying {}

impl NowPlaying {
    pub fn new(
        app: RustetApp,
        http: reqwest::Client,
        cache_dir: &Path,
        cx: &mut Context<Self>,
    ) -> Self {
        let disk = DiskCache::open(cache_dir.join("backdrops"), 64 << 20)
            .ok()
            .map(Arc::new);
        Self {
            app,
            state: None,
            video: None,
            open: false,
            fullscreen: false,
            shown: None,
            lyrics: LyricsState::Loading,
            backdrop: cx.new(|_| Backdrop::new(http, disk)),
            loading: None,
        }
    }

    pub fn backdrop(&self) -> Entity<Backdrop> {
        self.backdrop.clone()
    }

    /// A new player state; `follow` says Now Playing or the lyrics panel is on screen (only
    /// then are the lyrics and the backdrop loaded).
    pub fn set_state(&mut self, state: Option<PlayerState>, follow: bool, cx: &mut Context<Self>) {
        self.state = state;
        if follow {
            self.follow_track(cx);
        }
        self.sync_lyrics(cx);
        self.sync_video(cx);
        cx.notify();
    }

    /// Whether Now Playing is on screen, and in full screen.
    pub fn set_shown(&mut self, open: bool, fullscreen: bool, cx: &mut Context<Self>) {
        if (open, fullscreen) != (self.open, self.fullscreen) {
            (self.open, self.fullscreen) = (open, fullscreen);
            self.sync_video(cx);
            cx.notify();
        }
    }

    /// The track is a music video played with its picture (not audio only).
    pub fn shows_video(&self) -> bool {
        self.state.as_ref().is_some_and(|s| {
            !s.mv_audio_only
                && s.track
                    .as_ref()
                    .is_some_and(|t| t.kind == ItemKind::MusicVideo)
        })
    }

    /// The video view's phase, counters and frame size (scenario measurements).
    #[cfg(feature = "scenario")]
    pub(crate) fn video_stats(&self, cx: &gpui::App) -> Option<VideoProbe> {
        let v = self.video.as_ref()?.read(cx);
        Some((v.phase(), v.stats(), v.frame_size()))
    }

    /// The track is a music video played as audio only.
    fn video_as_audio(&self) -> bool {
        self.state.as_ref().is_some_and(|s| {
            s.mv_audio_only
                && s.track
                    .as_ref()
                    .is_some_and(|t| t.kind == ItemKind::MusicVideo)
        })
    }

    /// The ring to read: the track's video while Now Playing shows it.
    fn video_source(&self) -> Option<VideoSource> {
        let state = self
            .state
            .as_ref()
            .filter(|_| self.open && self.shows_video())?;
        Some(VideoSource {
            surface: state.video.clone()?,
            video: state.track.as_ref()?.uid,
        })
    }

    fn sync_video(&mut self, cx: &mut Context<Self>) {
        let source = self.video_source();
        if let Some(video) = &self.video {
            video.update(cx, |v, cx| v.set_source(source, cx));
        }
    }

    /// The video view, made with the window the first time it is needed.
    fn video_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<VideoView> {
        if self.video.is_none() {
            self.video = Some(cx.new(|cx| VideoView::new(window, cx)));
            self.sync_video(cx);
        }
        self.video.clone().expect("made above")
    }

    /// The video, full size; a double click toggles full screen.
    fn video_area(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("video-area")
            .size_full()
            .on_click(cx.listener(|_, e: &gpui::ClickEvent, _, cx| {
                if e.click_count() == 2 {
                    cx.emit(VideoRequest::ToggleFullscreen);
                }
            }))
            .child(self.video_view(window, cx))
    }

    fn render_video(
        &mut self,
        track: &crate::deck::TrackLine,
        wide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        if self.fullscreen {
            return div()
                .size_full()
                .child(self.video_area(window, cx))
                .into_any_element();
        }
        let controls = div()
            .flex()
            .items_center()
            .gap(theme.space.s)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(theme.text.title2)
                            .font_weight(BOLD)
                            .truncate()
                            .child(SharedString::from(track.title.clone())),
                    )
                    .child(
                        div()
                            .text_color(colors.text_secondary)
                            .truncate()
                            .child(track.artist.clone()),
                    ),
            )
            .child(
                Button::new("video-audio-only", "Audio only")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(VideoRequest::AudioOnly(true)))),
            )
            .child(
                Button::new("video-fullscreen", "Full screen")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(VideoRequest::ToggleFullscreen))),
            );
        let picture = div()
            .flex_1()
            .min_h_0()
            .rounded(theme.radius.menu)
            .overflow_hidden()
            .child(self.video_area(window, cx));
        div()
            .id("now-playing")
            .role(Role::Region)
            .aria_label("Now Playing")
            .size_full()
            .flex()
            .when(!wide, |d| d.flex_col())
            .gap(theme.space.xl)
            .px(theme.space.xxl)
            .pb(theme.space.xl)
            .text_color(colors.text)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.m)
                    .flex_grow(2.)
                    .flex_basis(gpui::relative(0.66))
                    .min_w_0()
                    .min_h_0()
                    .child(picture)
                    .child(controls),
            )
            .child(
                div()
                    .flex_basis(gpui::relative(0.34))
                    .flex_shrink_0()
                    .min_w_0()
                    .min_h_0()
                    .child(self.lyrics_pane(cx)),
            )
            .into_any_element()
    }

    /// Loads lyrics and the backdrop when the track changed.
    fn follow_track(&mut self, cx: &mut Context<Self>) {
        let track = self.state.as_ref().and_then(|s| s.track.clone());
        let id = track.as_ref().map(|t| t.id.0.clone());
        if id == self.shown {
            return;
        }
        self.shown = id.clone();
        let Some((id, track)) = id.zip(track) else {
            self.lyrics = LyricsState::Missing;
            return;
        };
        if !track.meta.artwork_url.is_empty() {
            let url = rustet_app::pages::model::Artwork {
                template: track.meta.artwork_url.clone(),
                width: None,
                height: None,
                palette: None,
            }
            .url(600, 600);
            self.backdrop.update(cx, |b, cx| b.show(url, cx));
        }
        self.load_lyrics(id, cx);
    }

    fn load_lyrics(&mut self, id: String, cx: &mut Context<Self>) {
        self.lyrics = LyricsState::Loading;
        let api = self.app.api().clone();
        let job = rustet_ui::runtime::spawn(cx, async move { lyrics::load(&api, &id).await });
        self.loading = Some(cx.spawn(async move |this, cx| {
            let Ok(result) = job.await else { return };
            this.update(cx, |this, cx| this.lyrics_loaded(result, cx))
                .ok();
        }));
    }

    fn lyrics_loaded(&mut self, result: Result<Lyrics, LoadError>, cx: &mut Context<Self>) {
        self.lyrics = match result {
            Ok(lyrics) => {
                let view = cx.new(|cx| LyricsView::new(lyrics, cx));
                let sub = cx.subscribe(&view, |this, _, e: &LyricsEvent, cx| {
                    let LyricsEvent::Seek(at) = *e;
                    let generation = this.state.as_ref().map(|s| s.generation);
                    cx.emit(Command::Seek(Seek {
                        position_ms: at.as_millis() as u64,
                        expect_generation: generation,
                    }));
                });
                LyricsState::Ready { view, _seek: sub }
            }
            Err(LoadError::NotFound) => LyricsState::Missing,
            Err(LoadError::SignInRequired) => LyricsState::SignIn,
            Err(e) => {
                debug!("lyrics: {e}");
                LyricsState::Failed
            }
        };
        self.sync_lyrics(cx);
        cx.notify();
    }

    fn sync_lyrics(&mut self, cx: &mut Context<Self>) {
        let LyricsState::Ready { view, .. } = &self.lyrics else {
            return;
        };
        let Some(state) = &self.state else { return };
        let position = Duration::from_millis(position_now(state, unix_ms()));
        let playing = state.status == Status::Playing;
        view.update(cx, |l, cx| {
            l.set_position(position, Instant::now(), playing, cx)
        });
    }

    /// The lyrics for the lyrics panel (loaded for the playing track on first use).
    pub(crate) fn lyrics_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.follow_track(cx);
        if self.state.as_ref().is_none_or(|s| s.track.is_none()) {
            return EmptyStateView::new(&rustet_app::pages::model::EmptyState {
                title: "Nothing playing".into(),
                message: "Play a song, and its lyrics show here.".into(),
            })
            .into_any_element();
        }
        self.lyrics_pane(cx)
    }

    fn lyrics_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let empty = |title: &str, message: &str| {
            EmptyStateView::new(&rustet_app::pages::model::EmptyState {
                title: title.into(),
                message: message.into(),
            })
        };
        match &self.lyrics {
            LyricsState::Ready { view, .. } => view.clone().into_any_element(),
            LyricsState::Loading => div()
                .text_color(theme.colors.text_secondary)
                .child("Loading lyrics…")
                .into_any_element(),
            LyricsState::Missing => empty(
                "No lyrics for this song",
                "Apple Music has no lyrics for it.",
            )
            .into_any_element(),
            LyricsState::SignIn => empty(
                "Sign in to see lyrics",
                "Lyrics come with your Apple Music account.",
            )
            .action("Sign In", |_, cx| {
                if let Some(shell) = cx
                    .try_global::<crate::shell::ShellGlobal>()
                    .map(|g| g.0.clone())
                {
                    shell.update(cx, |s, cx| s.sign_in(cx));
                }
            })
            .into_any_element(),
            LyricsState::Failed => div()
                .flex()
                .flex_col()
                .gap(theme.space.s)
                .child(empty(
                    "Lyrics did not load",
                    "Check the connection, then try again.",
                ))
                .child(
                    Button::new("lyrics-retry", "Try again").on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(id) = this.shown.clone() {
                                this.load_lyrics(id, cx);
                            }
                        },
                    )),
                )
                .into_any_element(),
        }
    }
}

impl Render for NowPlaying {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_track(cx);
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let d = deck_drawing(self.state.as_ref(), true, unix_ms());
        let wide = rustet_ui::frame::Layout::for_width(window.viewport_size().width / theme.zoom)
            == rustet_ui::frame::Layout::Full;
        let art_size = theme.size.art_header;
        let Some(track) = d.track.clone() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyStateView::new(&rustet_app::pages::model::EmptyState {
                    title: "Nothing playing".into(),
                    message: "Play something, and it shows here with its lyrics.".into(),
                }))
                .into_any_element();
        };
        if self.shows_video() {
            return self.render_video(&track, wide, window, cx);
        }
        let show_video = self.video_as_audio().then(|| {
            Button::new("video-show", "Show the video")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(VideoRequest::AudioOnly(false))))
        });
        let quality = self.state.as_ref().and_then(|s| s.quality);
        // The stream: its format, then a card of label and value rows split by hairlines, as
        // Settings draws its groups.
        let details = quality.map(|q| {
            let (rows, note) = quality_details(&q);
            let count = rows.len();
            div()
                .id("quality-details")
                .role(Role::Group)
                .aria_label("Stream quality")
                .flex()
                .flex_col()
                .gap(theme.space.s)
                .child(
                    div()
                        .text_size(theme.text.callout)
                        .font_weight(SEMIBOLD)
                        .font_features(theme.tabular())
                        .text_color(colors.text_secondary)
                        .child(quality_text(&q)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .px(theme.space.m)
                        .rounded(theme.radius.menu)
                        .bg(colors.fill)
                        .text_size(theme.text.callout)
                        .children(rows.into_iter().enumerate().map(|(i, (label, value))| {
                            div()
                                .flex()
                                .justify_between()
                                .py(theme.space.s)
                                .when(i + 1 < count, |d| {
                                    d.border_b(theme.size.hairline).border_color(colors.divider)
                                })
                                .child(div().text_color(colors.text_secondary).child(label))
                                .child(div().font_features(theme.tabular()).child(value))
                        })),
                )
                .children(note.map(|n| {
                    div()
                        .text_size(theme.text.callout)
                        .text_color(colors.text_secondary)
                        .child(n)
                }))
        });
        let info = div()
            .flex()
            .flex_col()
            .gap(theme.space.l)
            .flex_none()
            .w(art_size)
            .child(
                div()
                    .rounded(theme.radius.card)
                    .shadow(theme.artwork_shadow())
                    .child(ArtworkView::new(track.artwork.clone(), art_size).shape(Shape::Card)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.xs)
                    .child(
                        div()
                            .text_size(theme.text.large_title)
                            .font_weight(BOLD)
                            .line_height(theme.text.large_title_line)
                            .child(SharedString::from(track.title.clone())),
                    )
                    .child(
                        div()
                            .text_size(theme.text.title2)
                            .text_color(colors.text_secondary)
                            .child(track.artist.clone()),
                    )
                    .when(!track.album.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(theme.text.body)
                                .text_color(colors.text_secondary)
                                .child(track.album.clone()),
                        )
                    }),
            )
            .children(show_video)
            .children(details);
        div()
            .id("now-playing")
            .role(Role::Region)
            .aria_label("Now Playing")
            .size_full()
            .flex()
            .when(!wide, |d| d.flex_col())
            .gap(theme.space.xxl)
            .px(theme.space.xxl)
            .pb(theme.space.xl)
            .text_color(colors.text)
            .child(info)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(self.lyrics_pane(cx)),
            )
            .into_any_element()
    }
}

impl crate::shell::Shell {
    /// The strip's left over Now Playing: the way back to the page, a 28 px glass circle.
    pub(crate) fn now_playing_leading(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                rustet_ui::components::IconButton::new(
                    "now-playing-close",
                    rustet_ui::icons::Icon::ChevronDown,
                    "Close Now Playing",
                )
                .glass(rustet_ui::components::Circle::Small)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.now_playing_open = false;
                    cx.notify();
                })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use rustet_player::model::Codec;

    use super::*;

    #[test]
    fn quality_details_name_the_stream_and_a_fallback() {
        let hires = Quality {
            codec: Codec::Alac,
            tier: QualityTier::HiResLossless,
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            bitrate: None,
            channels: Some(2),
            spatial: None,
            video_height: None,
            fell_back: false,
        };
        let (rows, note) = quality_details(&hires);
        assert_eq!(
            rows,
            [
                ("Format", "Hi-Res Lossless".to_owned()),
                ("Bit depth", "24-bit".to_owned()),
                ("Sample rate", "96 kHz".to_owned()),
                ("Channels", "Stereo".to_owned()),
            ]
        );
        assert_eq!(note, None);
        let cd = Quality {
            sample_rate: Some(44_100),
            ..hires
        };
        assert_eq!(quality_details(&cd).0[2].1, "44.1 kHz");
        let aac = Quality {
            codec: Codec::Aac,
            tier: QualityTier::Aac,
            bit_depth: None,
            sample_rate: None,
            bitrate: Some(256_000),
            fell_back: true,
            ..hires
        };
        assert_eq!(
            quality_details(&aac).1.as_deref(),
            Some("Lossless isn’t available for this track. Playing AAC 256 kbps.")
        );
    }
}
