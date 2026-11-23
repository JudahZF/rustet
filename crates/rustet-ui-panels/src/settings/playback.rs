//! Settings → Playback, and History.

use gpui::{AnyElement, Context, IntoElement, Window};
use rustet_app::account::Backend;
use rustet_player::model::{Command, Mode, StreamingQuality};
use rustet_ui::components::{Button, Toast, Toggle};

use super::SettingsPage;
use super::controls::QUALITIES;
use crate::confirm::confirm;
use crate::ui::{group, row, run_in};

impl SettingsPage {
    /// Why lossless cannot be turned on now; `None` when it can.
    pub(crate) fn lossless_lock(&self) -> Option<&'static str> {
        match (&self.status.drm, self.status.backend) {
            (Some(d), _) if !d.available => {
                Some("This computer has no FairPlay libraries, so Rustet plays AAC 256 kbps.")
            }
            (_, Some(Backend::Web)) => {
                Some("This computer has no FairPlay libraries, so Rustet plays AAC 256 kbps.")
            }
            (Some(d), _) if d.signed_in() => None,
            _ => Some("Sign in with your Apple ID to play lossless."),
        }
    }

    pub(super) fn playback(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = self.settings.playback;
        let lock = self.lossless_lock();
        let effective = p.effective_quality();
        let plays = match (lock, effective) {
            (Some(_), _) | (None, StreamingQuality::HighQuality) => "AAC 256 kbps",
            (None, q) => QUALITIES
                .iter()
                .find(|(v, _)| *v == q)
                .map_or("", |(_, l)| l),
        };
        let quality_note = if p.lossless_enabled || p.quality == StreamingQuality::HighQuality {
            format!("Rustet plays {plays}.")
        } else {
            "Lossless is off, so Rustet plays AAC 256 kbps.".to_owned()
        };
        let lossless = self
            .toggle(
                "lossless",
                "Lossless",
                p.lossless_enabled,
                |s, on| s.playback.lossless_enabled = on,
                cx,
            )
            .description(lock.unwrap_or("Play ALAC when the track has it. Off: AAC 256 kbps."))
            .disabled(lock.is_some());

        let mut quality = vec![row(
            "Streaming quality",
            Some(quality_note.into()),
            self.c.quality.select.clone(),
            cx,
        )];
        quality.extend(self.refusal_of("quality", cx));
        quality.push(lossless.into_any_element());
        quality.extend(self.refusal_of("lossless", cx));
        quality.push(row(
            "Music-video resolution",
            Some("The highest resolution music videos play at.".into()),
            self.c.mv_height.select.clone(),
            cx,
        ));
        quality.extend(self.refusal_of("mv-height", cx));
        let audio_only = match self.status.mv_audio_only {
            Some(on) => {
                let app = self.app.clone();
                Toggle::new("mv-audio-only", "Music videos as audio only", on)
                    .description("Play the sound of music videos without the picture.")
                    .on_change(move |on, _, _| {
                        let app = app.clone();
                        app.runtime()
                            .clone()
                            .spawn(async move { app.player().set_mv_audio_only(on).await });
                    })
            }
            None => Toggle::new("mv-audio-only", "Music videos as audio only", false)
                .description("Available when Rustet's engine is running.")
                .disabled(true),
        };
        quality.push(audio_only.into_any_element());

        let autoplay = match self.status.autoplay {
            Some(on) => {
                let app = self.app.clone();
                Toggle::new("autoplay", "Autoplay", on)
                    .description("When the queue ends, keep playing similar music.")
                    .on_change(move |on, _, _| {
                        let app = app.clone();
                        let mode = Command::Mode(Mode {
                            autoplay: Some(on),
                            ..Mode::default()
                        });
                        app.runtime()
                            .clone()
                            .spawn(async move { app.player().send(&mode).await });
                    })
            }
            None => Toggle::new("autoplay", "Autoplay", false)
                .description("Available when Rustet's engine is running.")
                .disabled(true),
        };
        let mut behaviour = vec![
            autoplay.into_any_element(),
            self.toggle(
                "resume",
                "Resume on launch",
                p.resume_on_launch,
                |s, on| s.playback.resume_on_launch = on,
                cx,
            )
            .description("Open with the queue and the track you were playing.")
            .into_any_element(),
            self.toggle(
                "report-plays",
                "Share listening history with Apple Music",
                p.report_plays,
                |s, on| s.playback.report_plays = on,
                cx,
            )
            .description("Plays count towards Recently Played, Replay and your recommendations.")
            .into_any_element(),
        ];
        for key in ["resume", "report-plays"] {
            behaviour.extend(self.refusal_of(key, cx));
        }

        // Stored and sent to the player, which does not apply them yet (rustet_player: "later").
        let later = vec![
            Toggle::new(
                "crossfade",
                "Crossfade",
                p.crossfade.mode != rustet_player::model::CrossfadeMode::Off,
            )
            .description("Coming in a later version.")
            .disabled(true)
            .into_any_element(),
            Toggle::new("sound-check", "Sound Check", p.sound_check)
                .description("Coming in a later version.")
                .disabled(true)
                .into_any_element(),
        ];
        vec![
            group("Quality", quality, cx),
            group("Playing", behaviour, cx),
            group("Later", later, cx),
        ]
    }

    pub(super) fn history(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = vec![
            self.toggle(
                "history",
                "Show history in Up Next",
                self.settings.history.show_in_up_next,
                |s, on| s.history.show_in_up_next = on,
                cx,
            )
            .description(
                "Played tracks stay above the current one. Rustet keeps the history either way.",
            )
            .into_any_element(),
        ];
        rows.extend(self.refusal_of("history", cx));
        rows.push(row(
            "Clear play history",
            Some("Removes the tracks you played from Up Next.".into()),
            Button::new("clear-history", "Clear history")
                .on_click(self.on(Self::confirm_clear_history, cx)),
            cx,
        ));
        vec![group("Play history", rows, cx)]
    }

    fn confirm_clear_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity();
        confirm(
            "Clear play history?",
            "The tracks you played leave Up Next. Your Apple Music history is not changed.",
            "Clear history",
            move |window, cx| {
                page.update(cx, |p, cx| {
                    let app = p.app.clone();
                    run_in(
                        p.app.runtime(),
                        async move { app.engine().clear_history().await },
                        window,
                        cx,
                        |_, result, window, cx| {
                            let toast = match result {
                                Ok(()) => Toast::new("Play history cleared"),
                                Err(e) => Toast::failure(format!(
                                    "Rustet could not clear the history: {e}"
                                )),
                            };
                            Self::toast(toast, window, cx);
                        },
                    );
                })
            },
            window,
            cx,
        );
    }
}
