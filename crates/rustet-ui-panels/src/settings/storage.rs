//! Settings → Storage: the song cache (limits in the settings), the music-video cache (its
//! limit is the engine's own), usage, and clearing them.

use gpui::{AnyElement, Context, IntoElement, Window};
use rustet_app::engine::types::{CacheScope, MvCacheUpdate};
use rustet_ui::components::{Button, Toast, Toggle};

use super::SettingsPage;
use super::controls::{MV_CACHE_MB, Range, SONG_CACHE_DEFAULT_MB};
use crate::confirm::confirm;
use crate::format;
use crate::ui::{self, fact, group, row, run, run_in};

const MB: f64 = 1024. * 1024.;

impl SettingsPage {
    pub(super) fn refresh_storage(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        run(
            self.app.runtime(),
            async move { app.engine().cache_stats().await },
            cx,
            |p, stats, _| {
                if !p.frozen {
                    p.status.cache = stats.ok();
                }
            },
        );
        let app = self.app.clone();
        run(
            self.app.runtime(),
            async move { app.engine().mv_cache().await },
            cx,
            |p, mv, cx| {
                if !p.frozen {
                    p.status.mv_cache = mv.ok();
                    p.show_mv_cache(cx);
                }
            },
        );
    }

    /// The music-video slider follows the engine's value; it is made when the first arrives.
    pub(crate) fn show_mv_cache(&mut self, cx: &mut Context<Self>) {
        let Some(mv) = &self.status.mv_cache else {
            return;
        };
        let mb = mv.max_bytes as f64 / MB;
        match &self.c.mv_cache {
            Some(range) => range.show(mb, cx),
            None => {
                let mut subs = Vec::new();
                let range = Range::new(
                    "Music-video cache limit",
                    MV_CACHE_MB,
                    mb,
                    |c| c.mv_cache.as_mut(),
                    |p, mb, cx| {
                        p.set_mv_cache(
                            MvCacheUpdate {
                                max_bytes: Some((mb * MB) as i64),
                                ..Default::default()
                            },
                            cx,
                        )
                    },
                    &mut subs,
                    cx,
                );
                self.c.mv_cache = Some(range);
                self._subscriptions.extend(subs);
            }
        }
    }

    fn set_mv_cache(&mut self, update: MvCacheUpdate, cx: &mut Context<Self>) {
        let app = self.app.clone();
        run(
            self.app.runtime(),
            async move { app.engine().set_mv_cache(update).await },
            cx,
            |p, result, cx| {
                match result {
                    Ok(mv) => {
                        p.refused.remove("mv-cache");
                        p.status.mv_cache = Some(mv);
                    }
                    Err(e) => p.refuse(
                        "mv-cache",
                        format!("Rustet's engine did not take the change: {e}"),
                        cx,
                    ),
                }
                p.show_mv_cache(cx);
            },
        );
    }

    fn confirm_clear(&mut self, songs: bool, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity();
        let (title, message, action) = if songs {
            (
                "Clear the song cache?",
                "Songs you played are downloaded again the next time you play them.",
                "Clear song cache",
            )
        } else {
            (
                "Clear the music-video cache?",
                "Music videos you watched are downloaded again the next time.",
                "Clear video cache",
            )
        };
        confirm(
            title,
            message,
            action,
            move |window, cx| {
                page.update(cx, |p, cx| {
                    let app = p.app.clone();
                    let job = async move {
                        if songs {
                            app.engine().clear_cache(CacheScope::Persistent).await
                        } else {
                            app.engine().clear_mv_cache().await
                        }
                    };
                    run_in(
                        p.app.runtime(),
                        job,
                        window,
                        cx,
                        move |p, result, window, cx| {
                            let toast = match result {
                                Ok(()) if songs => Toast::new("Song cache cleared"),
                                Ok(()) => Toast::new("Music-video cache cleared"),
                                Err(e) => {
                                    Toast::failure(format!("Rustet could not clear the cache: {e}"))
                                }
                            };
                            Self::toast(toast, window, cx);
                            p.refresh_storage(cx);
                        },
                    );
                })
            },
            window,
            cx,
        );
    }

    pub(super) fn storage(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let s = self.settings.storage;
        let limit = f64::from(s.song_cache_limit_mb.unwrap_or(SONG_CACHE_DEFAULT_MB));
        let mut songs = vec![row(
            "Song cache limit",
            Some("Songs you play are kept on this computer up to this size.".into()),
            ui::slider(
                &self.c.song_cache.slider,
                format::megabytes(self.c.song_cache.preview.unwrap_or(limit) as u64),
                cx,
            ),
            cx,
        )];
        songs.extend(self.refusal_of("song-cache", cx));
        songs.push(row(
            "Remove unused songs",
            Some("Songs not played for this long are removed.".into()),
            self.c.ttl.select.clone(),
            cx,
        ));
        songs.extend(self.refusal_of("ttl", cx));
        if let Some(c) = &self.status.cache {
            let p = &c.persistent;
            songs.push(fact(
                "In use",
                format!(
                    "{} of {}, {}",
                    format::bytes(p.size_bytes),
                    format::bytes(p.limit_bytes),
                    format::count(p.count, "song", "songs")
                ),
                cx,
            ));
        }
        songs.push(row(
            "Clear the song cache",
            None,
            Button::new("clear-songs", "Clear song cache")
                .on_click(self.on(|p, w, cx| p.confirm_clear(true, w, cx), cx)),
            cx,
        ));

        let mut videos = Vec::new();
        match (&self.status.mv_cache, &self.c.mv_cache) {
            (Some(mv), Some(range)) => {
                let page = cx.entity();
                videos.push(
                    Toggle::new("mv-cache-on", "Keep music videos", mv.enabled)
                        .description("Videos you watch are kept on this computer.")
                        .on_change(move |on, _, cx| {
                            page.update(cx, |p, cx| {
                                p.set_mv_cache(
                                    MvCacheUpdate {
                                        enabled: Some(on),
                                        ..Default::default()
                                    },
                                    cx,
                                )
                            })
                        })
                        .into_any_element(),
                );
                videos.push(row(
                    "Music-video cache limit",
                    None,
                    ui::slider(
                        &range.slider,
                        format::megabytes(range.preview.unwrap_or(mv.max_bytes as f64 / MB) as u64),
                        cx,
                    ),
                    cx,
                ));
                videos.push(fact(
                    "In use",
                    format::bytes(u64::try_from(mv.size_bytes).unwrap_or(0)),
                    cx,
                ));
            }
            _ => videos.push(fact(
                "Music-video cache",
                "Available when Rustet's engine is running",
                cx,
            )),
        }
        videos.extend(self.refusal_of("mv-cache", cx));
        videos.push(row(
            "Clear the music-video cache",
            None,
            Button::new("clear-videos", "Clear video cache")
                .on_click(self.on(|p, w, cx| p.confirm_clear(false, w, cx), cx)),
            cx,
        ));
        vec![group("Songs", songs, cx), group("Music videos", videos, cx)]
    }
}
