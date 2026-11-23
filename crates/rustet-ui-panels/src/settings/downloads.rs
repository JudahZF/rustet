//! Settings → Downloads: every export option, the folder and FFmpeg.

use gpui::{
    AnyElement, Context, IntoElement, ParentElement, PathPromptOptions, Styled, Window, div,
};
use rustet_app::settings::{Marker, Settings};
use rustet_player::model::StreamingQuality;
use rustet_ui::components::Button;
use rustet_ui::theme::ActiveTheme;

use super::{NOT_USED_YET, SettingsPage};
use crate::ui::{self, group, row, run};

/// The engine's folder when none is set.
pub const DEFAULT_FOLDER: &str = "~/Music/Rustet-Downloads";

/// The file-name markers (`[E]`, `[C]`, `[M]`).
#[derive(Clone, Copy)]
enum MarkerKind {
    Explicit,
    Clean,
    Master,
}

impl MarkerKind {
    /// The switch's key, the text field's key, the label and the line under it.
    const fn names(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Explicit => (
                "explicit",
                "explicit-text",
                "Mark explicit tracks",
                "Adds the marker to explicit tracks.",
            ),
            Self::Clean => (
                "clean",
                "clean-text",
                "Mark clean tracks",
                "Adds the marker to clean versions.",
            ),
            Self::Master => (
                "master",
                "master-text",
                "Mark Apple Digital Masters",
                "Adds the marker to mastered-for-Apple albums.",
            ),
        }
    }

    fn marker(self, d: &rustet_app::settings::Downloads) -> &Marker {
        match self {
            Self::Explicit => &d.explicit_marker,
            Self::Clean => &d.clean_marker,
            Self::Master => &d.master_marker,
        }
    }
}

impl SettingsPage {
    /// Asks the engine whether the FFmpeg in the field (or on `PATH`) works.
    pub(crate) fn check_ffmpeg(&mut self, cx: &mut Context<Self>) {
        if self.frozen {
            return;
        }
        let app = self.app.clone();
        let path = self.settings.downloads.ffmpeg_path.clone();
        let asked = path.clone();
        run(
            self.app.runtime(),
            async move { app.engine().tools(&asked).await },
            cx,
            move |p, result, _| {
                // A later edit's answer wins.
                if p.settings.downloads.ffmpeg_path == path || path.is_empty() {
                    p.status.ffmpeg = Some(result.map(|t| t.ffmpeg).map_err(|e| e.to_string()));
                }
            },
        );
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this folder".into()),
        });
        cx.spawn_in(window, async move |page, cx| {
            let result = picked.await;
            page.update(cx, |p, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(dir) = paths.first() {
                        let dir = dir.to_string_lossy().into_owned();
                        p.write("folder", |s| s.downloads.folder = dir, cx);
                    }
                }
                Ok(Ok(None)) => {}
                // No portal (a desktop without xdg-desktop-portal): the field still works.
                Ok(Err(_)) | Err(_) => p.refuse(
                    "folder",
                    "Rustet could not open a folder picker here. Type the folder's path instead.",
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn marker(&self, kind: MarkerKind, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (key, text_key, label, what) = kind.names();
        let enabled = kind.marker(&self.settings.downloads).enabled;
        let set = match kind {
            MarkerKind::Explicit => |s: &mut Settings, on| s.downloads.explicit_marker.enabled = on,
            MarkerKind::Clean => |s: &mut Settings, on| s.downloads.clean_marker.enabled = on,
            MarkerKind::Master => |s: &mut Settings, on| s.downloads.master_marker.enabled = on,
        };
        let mut rows = vec![
            self.toggle(key, label, enabled, set, cx)
                .description(what)
                .into_any_element(),
        ];
        if enabled {
            let input = match kind {
                MarkerKind::Explicit => &self.c.explicit_text,
                MarkerKind::Clean => &self.c.clean_text,
                MarkerKind::Master => &self.c.master_text,
            };
            rows.push(row(
                "Marker text",
                Some("Put {tag} in a file name pattern to use it.".into()),
                ui::field(input, cx),
                cx,
            ));
        }
        rows.extend(self.refusal_of(key, cx));
        rows.extend(self.refusal_of(text_key, cx));
        rows
    }

    pub(super) fn downloads(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let d = self.settings.downloads.clone();
        let lossless_note = match (self.lossless_lock(), d.quality) {
            (_, StreamingQuality::HighQuality) => "Songs download as AAC 256 kbps.",
            (Some(_), _) => {
                "Lossless needs a lossless sign-in; until then songs download as AAC 256 kbps."
            }
            (None, _) if !self.settings.playback.lossless_enabled => {
                "Lossless is off in Playback, so songs download as AAC 256 kbps."
            }
            (None, _) => "Songs download as ALAC when the track has it.",
        };
        let mut quality = vec![row(
            "Quality",
            Some(lossless_note.into()),
            self.c.dl_quality.select.clone(),
            cx,
        )];
        quality.extend(self.refusal_of("dl-quality", cx));
        quality.push(row(
            "Music-video resolution",
            None,
            self.c.dl_mv.select.clone(),
            cx,
        ));
        quality.extend(self.refusal_of("dl-mv", cx));

        let mut artwork = vec![
            self.toggle(
                "embed-art",
                "Embed artwork",
                d.embed_artwork,
                |s, on| s.downloads.embed_artwork = on,
                cx,
            )
            .into_any_element(),
            row("Artwork size", None, self.c.dl_artwork.select.clone(), cx),
            self.toggle(
                "motion-art",
                "Animated artwork",
                d.motion_artwork,
                |s, on| s.downloads.motion_artwork = on,
                cx,
            )
            .description(NOT_USED_YET)
            .into_any_element(),
            self.toggle(
                "lyrics",
                "Embed lyrics",
                d.embed_lyrics,
                |s, on| s.downloads.embed_lyrics = on,
                cx,
            )
            .into_any_element(),
            self.toggle(
                "timed-lyrics",
                "Timed lyrics",
                d.timed_lyrics,
                |s, on| s.downloads.timed_lyrics = on,
                cx,
            )
            .description(NOT_USED_YET)
            .into_any_element(),
        ];
        for key in [
            "embed-art",
            "dl-artwork",
            "motion-art",
            "lyrics",
            "timed-lyrics",
        ] {
            artwork.extend(self.refusal_of(key, cx));
        }

        let mut files = vec![row(
            "Folder",
            Some(format!("Empty: {DEFAULT_FOLDER}").into()),
            div()
                .flex()
                .gap(cx.theme().space.s)
                .child(ui::field(&self.c.folder, cx))
                .child(
                    Button::new("choose-folder", "Choose")
                        .on_click(self.on(Self::choose_folder, cx)),
                ),
            cx,
        )];
        files.extend(self.refusal_of("folder", cx));
        files.push(row(
            "Album folder",
            None,
            ui::field(&self.c.album_template, cx),
            cx,
        ));
        files.extend(self.refusal_of("album-template", cx));
        files.push(row(
            "Track file",
            None,
            ui::field(&self.c.track_template, cx),
            cx,
        ));
        files.extend(self.refusal_of("track-template", cx));
        for kind in [MarkerKind::Explicit, MarkerKind::Clean, MarkerKind::Master] {
            files.extend(self.marker(kind, cx));
        }

        let ffmpeg_status = match &self.status.ffmpeg {
            None => "Checking FFmpeg".to_owned(),
            Some(Err(e)) => format!("Rustet's engine did not answer: {e}"),
            Some(Ok(f)) if f.can_reencode() => {
                let version = f
                    .version
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    "Found {} ({version})",
                    if f.path.is_empty() { "ffmpeg" } else { &f.path }
                )
            }
            Some(Ok(f)) if f.available => "FFmpeg runs, but has no AAC or FLAC encoder.".to_owned(),
            Some(Ok(f)) if !f.error.is_empty() => format!("Not found: {}", f.error),
            Some(Ok(_)) => "Not found. Install FFmpeg or enter its path.".to_owned(),
        };
        let mut ffmpeg = vec![
            self.toggle(
                "ffmpeg",
                "Convert with FFmpeg",
                d.ffmpeg_enabled,
                |s, on| s.downloads.ffmpeg_enabled = on,
                cx,
            )
            .description("Re-encode lossless downloads to FLAC.")
            .into_any_element(),
            row(
                "FFmpeg path",
                Some("Empty: ffmpeg on your PATH.".into()),
                ui::field(&self.c.ffmpeg_path, cx),
                cx,
            ),
            ui::note(ffmpeg_status, cx),
        ];
        ffmpeg.extend(self.refusal_of("ffmpeg", cx));
        ffmpeg.extend(self.refusal_of("ffmpeg-path", cx));

        let mut failures = vec![
            self.toggle(
                "retry",
                "Retry failed downloads",
                d.retry_on_fail,
                |s, on| s.downloads.retry_on_fail = on,
                cx,
            )
            .description("Each failed download is tried again once.")
            .into_any_element(),
        ];
        if d.retry_on_fail {
            failures.push(row(
                "Retry after",
                None,
                self.c.retry_delay.select.clone(),
                cx,
            ));
        }
        failures.extend(self.refusal_of("retry", cx));
        failures.extend(self.refusal_of("retry-delay", cx));

        vec![
            group("Quality", quality, cx),
            group("Artwork and lyrics", artwork, cx),
            group("Files", files, cx),
            group("FFmpeg", ffmpeg, cx),
            group("Failures", failures, cx),
        ]
    }
}
