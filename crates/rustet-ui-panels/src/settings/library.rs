//! Settings → Library: the local library cache and its sync.

use gpui::{AnyElement, Context, IntoElement};
use rustet_app::account::AccountState;
use rustet_app::library::{SyncEvent, SyncPhase, SyncStatus};
use rustet_ui::components::Button;

use super::SettingsPage;
use crate::format;
use crate::ui::{fact, group, row, run};

/// What a running sync is doing, as a line.
pub fn sync_progress(event: Option<&SyncEvent>) -> String {
    match event {
        None => "Starting".to_owned(),
        Some(SyncEvent::Fetched { phase, fetched }) => {
            let what = match phase {
                SyncPhase::Songs => "songs",
                SyncPhase::Albums => "albums",
                SyncPhase::Playlists => "playlists",
                SyncPhase::PlaylistTracks => "playlist tracks",
            };
            format!("Fetched {fetched} {what}")
        }
        Some(SyncEvent::Playlist {
            index, total, name, ..
        }) => {
            format!("Playlist {} of {total}: {name}", index + 1)
        }
        Some(SyncEvent::Saved(_)) => "Saving".to_owned(),
    }
}

impl SettingsPage {
    pub(super) fn refresh_library(&mut self, cx: &mut Context<Self>) {
        let syncer = self.app.account().syncer().clone();
        run(
            self.app.runtime(),
            async move { syncer.library().status().await },
            cx,
            |p, status, _| {
                if !p.frozen {
                    p.status.library = status.ok();
                }
            },
        );
    }

    pub(super) fn library(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let signed_in = matches!(self.status.account, AccountState::SignedIn { .. });
        let running = matches!(self.status.sync, SyncStatus::Running(_));
        let (state, detail) = match &self.status.sync {
            SyncStatus::Running(event) => {
                ("Syncing".to_owned(), Some(sync_progress(event.as_ref())))
            }
            SyncStatus::Failed(why) => (
                "The last sync stopped".to_owned(),
                Some(format!("{why}. Sync now continues where it stopped.")),
            ),
            SyncStatus::Idle | SyncStatus::Done(_) => {
                let last = self
                    .status
                    .library
                    .and_then(|l| l.stats.synced_at)
                    .map(|at| {
                        let ago = (chrono::Utc::now() - at).to_std().unwrap_or_default();
                        format!("Last synced {}", format::ago(ago))
                    });
                (last.unwrap_or_else(|| "Never synced".to_owned()), None)
            }
        };
        let syncer = self.app.account().syncer().clone();
        let runtime = self.app.runtime().clone();
        let sync = Button::new("sync-now", if running { "Syncing" } else { "Sync now" })
            .disabled(!signed_in || running)
            .on_click(move |_, _, _| {
                let syncer = syncer.clone();
                // `Syncer::start` spawns on the current runtime.
                runtime.spawn(async move { syncer.start() });
            });
        let description = if signed_in {
            detail
        } else {
            Some("Sign in to sync your library.".to_owned())
        };
        let mut rows = vec![row(state, description.map(Into::into), sync, cx)];
        if let Some(stats) = self.status.library.map(|l| l.stats) {
            rows.push(fact("Songs", stats.songs.to_string(), cx));
            rows.push(fact("Albums", stats.albums.to_string(), cx));
            rows.push(fact("Playlists", stats.playlists.to_string(), cx));
        }
        let mut downloads = vec![
            self.toggle(
                "songinfo",
                "Use song info for playlist downloads",
                self.settings.library.use_song_info_for_playlist,
                |s, on| s.library.use_song_info_for_playlist = on,
                cx,
            )
            .description(super::NOT_USED_YET)
            .into_any_element(),
        ];
        downloads.extend(self.refusal_of("songinfo", cx));
        vec![
            group("Library on this computer", rows, cx),
            group("Playlists", downloads, cx),
        ]
    }
}
