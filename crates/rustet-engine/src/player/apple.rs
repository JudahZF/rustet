//! [`Catalog`] and [`ActivityReporter`] over `rustet_ampapi::Api`: station next tracks, the
//! autoplay station and play-activity reports, with the engine's current tokens.

use std::sync::{Arc, Weak};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use rustet_ampapi::api::{
    EndReason as AmpEnd, ItemId, MediaKind, PlayContainer, PlayEvent, PlayMode, PlayPhase,
    PlayedItem, Repeat, ResourceRef, ResourceType,
};
use rustet_ampapi::model::{Artwork, AudioTrait, Resource};
use rustet_ampapi::{Api, ApiConfig, DeveloperToken, ErrorKind, Locale, UserToken};
use rustet_player::activity::{EndReason, EventType, PlayActivityEvent};
use rustet_player::model::{CatalogId, ItemKind, QueueItemIn, TrackMeta};
use rustet_player::ports::{ActivityReporter, Catalog, CatalogError, ReportError};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::state::Engine;

pub(crate) struct AppleLink {
    api: Api,
    engine: Weak<Engine>,
}

impl AppleLink {
    /// The client over the engine's Apple client. An engine whose API base is not Apple's (a
    /// mock server) sends the edge and play-activity calls there too, never to Apple.
    pub fn new(engine: &Arc<Engine>) -> Self {
        let config = if engine.amp_base == rustet_ampapi::AMP_BASE {
            ApiConfig::default()
        } else {
            let base = engine.amp_base.trim_end_matches('/');
            ApiConfig {
                edge_base: base.to_owned(),
                play_activity_url: format!("{base}/play"),
                ..ApiConfig::default()
            }
        };
        Self {
            api: Api::with_config(engine.amp.clone(), Locale::default(), config),
            engine: Arc::downgrade(engine),
        }
    }

    /// The client with the engine's tokens and locale of this moment (they rotate).
    async fn api(&self) -> Result<&Api, String> {
        let engine = self.engine.upgrade().ok_or("the engine stopped")?;
        let user = engine.tokens.music_user_token();
        self.api
            .set_user_token((!user.is_empty()).then(|| UserToken::new(user)));
        let developer = engine.tokens.developer();
        if !developer.is_empty() {
            self.api
                .set_developer_token(DeveloperToken::new(developer))
                .await;
        }
        // A UI that signed in on the web pushes the storefront code. With only the FairPlay
        // sign-in the engine has the numeric ID (`143444`), which the catalog API refuses: ask
        // Apple once and keep the answer.
        if engine.tokens.pushed_storefront().is_none() && self.api.is_signed_in() {
            match self.api.account_storefront(&CancellationToken::new()).await {
                Ok(storefront) => engine.tokens.set_locale(&storefront.id, ""),
                Err(e) => debug!("player: account storefront: {e}"),
            }
        }
        self.api.set_locale(Locale::new(
            engine.tokens.storefront_code(),
            engine.tokens.language(),
        ));
        Ok(&self.api)
    }

    /// The storefront for opening a track, asked from Apple when the UI pushed none.
    pub async fn storefront(&self) -> String {
        let _ = self.api().await;
        self.engine
            .upgrade()
            .map(|e| e.tokens.playback_storefront())
            .unwrap_or_default()
    }
}

fn catalog_error(e: rustet_ampapi::Error) -> CatalogError {
    match e.kind() {
        ErrorKind::Unauthorized => CatalogError::Authentication,
        _ => CatalogError::Other(e.to_string()),
    }
}

fn trait_name(t: &AudioTrait) -> &str {
    match t {
        AudioTrait::Lossless => "lossless",
        AudioTrait::HiResLossless => "hi-res-lossless",
        AudioTrait::LossyStereo => "lossy-stereo",
        AudioTrait::Atmos => "atmos",
        AudioTrait::Spatial => "spatial",
        AudioTrait::Other(s) => s,
    }
}

fn artwork(a: Option<&Artwork>) -> String {
    a.map(|a| a.url.clone()).unwrap_or_default()
}

/// A station or autoplay track as the queue takes it. Other resource types are skipped.
fn queue_item(r: Resource) -> Option<QueueItemIn> {
    let (id, kind, meta) = match r {
        Resource::Song(s) => {
            let a = s.attributes?;
            let meta = TrackMeta {
                name: a.name,
                artist_name: a.artist_name,
                album_name: a.album_name.unwrap_or_default(),
                artwork_url: artwork(a.artwork.as_ref()),
                duration_ms: a.duration_in_millis.unwrap_or(0),
                audio_traits: a
                    .audio_traits
                    .iter()
                    .map(|t| trait_name(t).to_owned())
                    .collect(),
                content_rating: a.content_rating,
            };
            (s.id, ItemKind::Song, meta)
        }
        Resource::MusicVideo(v) => {
            let a = v.attributes?;
            let meta = TrackMeta {
                name: a.name,
                artist_name: a.artist_name,
                album_name: a.album_name.unwrap_or_default(),
                artwork_url: artwork(a.artwork.as_ref()),
                duration_ms: a.duration_in_millis.unwrap_or(0),
                audio_traits: Vec::new(),
                content_rating: a.content_rating,
            };
            (v.id, ItemKind::MusicVideo, meta)
        }
        _ => return None,
    };
    Some(QueueItemIn {
        id: CatalogId(id),
        library_id: None,
        kind,
        meta,
        container_id: None,
    })
}

#[async_trait]
impl Catalog for AppleLink {
    async fn station_next_tracks(
        &self,
        station_id: &str,
        limit: usize,
    ) -> Result<Vec<QueueItemIn>, CatalogError> {
        let api = self.api().await.map_err(CatalogError::Other)?;
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        let tracks = api
            .station_next_tracks(&CancellationToken::new(), station_id, limit)
            .await
            .map_err(catalog_error)?;
        Ok(tracks.into_iter().filter_map(queue_item).collect())
    }

    async fn autoplay_tracks(
        &self,
        seeds: &[CatalogId],
        limit: usize,
    ) -> Result<Vec<QueueItemIn>, CatalogError> {
        let api = self.api().await.map_err(CatalogError::Other)?;
        let seeds: Vec<_> = seeds
            .iter()
            .map(|s| ResourceRef::new(ResourceType::Songs, s.as_str()))
            .collect();
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        let station = api
            .autoplay(&CancellationToken::new(), &seeds, limit)
            .await
            .map_err(catalog_error)?;
        Ok(station.tracks.into_iter().filter_map(queue_item).collect())
    }
}

/// The player's event in the sender's model. The sender adds the client fields and the tokens.
fn play_event(e: &PlayActivityEvent) -> PlayEvent {
    let ids = &e.ids;
    let id = match &ids.cloud_id {
        Some(library_id) => ItemId::Library {
            library_id: library_id.clone(),
            catalog_id: Some(ids.subscription_adam_id.clone()),
        },
        None => ItemId::Catalog(ids.subscription_adam_id.clone()),
    };
    let c = e.container_ids.as_ref();
    let field = |f: fn(&rustet_player::activity::ContainerIds) -> &Option<String>| {
        c.and_then(|c| f(c).clone())
    };
    let container = match e.container_type {
        3 => field(|c| &c.album_adam_id)
            .map(ItemId::Catalog)
            .or_else(|| {
                field(|c| &c.cloud_album_id).map(|l| ItemId::Library {
                    library_id: l,
                    catalog_id: None,
                })
            })
            .map(PlayContainer::Album),
        2 => {
            let global = field(|c| &c.global_playlist_id);
            let id = match field(|c| &c.universal_library_id) {
                Some(library_id) => Some(ItemId::Library {
                    library_id,
                    catalog_id: global,
                }),
                None => global.map(ItemId::Catalog),
            };
            id.map(|id| PlayContainer::Playlist {
                id,
                version_hash: None,
            })
        }
        1 => field(|c| &c.station_id).map(|id| PlayContainer::Station { id, hash: None }),
        4 => Some(PlayContainer::Artist(String::new())),
        _ => None,
    };
    let ms = Duration::from_millis;
    let phase = match (e.event_type, e.end_reason_type) {
        (EventType::PlayEnd, reason) => PlayPhase::End {
            start: ms(e.start_position_in_milliseconds),
            end: ms(e
                .end_position_in_milliseconds
                .unwrap_or(e.start_position_in_milliseconds)),
            reason: match reason.unwrap_or(EndReason::Other) {
                EndReason::Other => AmpEnd::Other,
                EndReason::SkippedForwards => AmpEnd::SkippedForwards,
                EndReason::ManuallyPaused => AmpEnd::Paused,
                EndReason::OtherItemChosen => AmpEnd::OtherItemChosen,
                EndReason::NaturalEnd => AmpEnd::NaturalEnd,
                EndReason::FailedToLoad => AmpEnd::FailedToLoad,
                EndReason::ScrubBegin => AmpEnd::ScrubBegin,
                EndReason::SkippedBackwards => AmpEnd::SkippedBackwards,
                EndReason::ExitedApplication => AmpEnd::ExitedApplication,
            },
        },
        (EventType::PlayStart, _) => PlayPhase::Start {
            position: ms(e.start_position_in_milliseconds),
            container_changed: false,
        },
    };
    let mode = &e.play_mode;
    PlayEvent {
        item: PlayedItem {
            id,
            media: if e.media_type == 1 {
                MediaKind::Video
            } else {
                MediaKind::Audio
            },
        },
        container,
        phase,
        duration: ms(e.media_duration_in_milliseconds),
        mode: PlayMode {
            shuffle: mode.shuffle_play_mode != 0,
            repeat: match mode.repeat_play_mode {
                1 => Repeat::One,
                2 => Repeat::All,
                _ => Repeat::Off,
            },
            autoplay: mode.auto_play_mode != 0,
        },
        at: UNIX_EPOCH + ms(e.timestamp),
        utc_offset_seconds: None,
        reco_data: None,
    }
}

#[async_trait]
impl ActivityReporter for AppleLink {
    async fn report(&self, events: &[PlayActivityEvent]) -> Result<(), ReportError> {
        let api = self.api().await.map_err(ReportError)?;
        let events: Vec<_> = events.iter().map(play_event).collect();
        api.report_plays(&CancellationToken::new(), &events)
            .await
            .map_err(|e| ReportError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use rustet_player::activity::{ContainerIds, Ids, PlayMode as PlayerMode};

    use super::*;

    fn event(
        event_type: EventType,
        container_type: u8,
        container_ids: Option<ContainerIds>,
    ) -> PlayActivityEvent {
        PlayActivityEvent {
            event_type,
            item_type: 1,
            ids: Ids {
                subscription_adam_id: "1441164359".into(),
                cloud_id: None,
            },
            container_type,
            container_ids,
            start_position_in_milliseconds: 1_000,
            end_position_in_milliseconds: (event_type == EventType::PlayEnd).then_some(30_000),
            media_duration_in_milliseconds: 200_000,
            end_reason_type: (event_type == EventType::PlayEnd)
                .then_some(EndReason::ManuallyPaused),
            play_mode: PlayerMode {
                auto_play_mode: 1,
                repeat_play_mode: 2,
                shuffle_play_mode: 0,
            },
            media_type: 0,
            timestamp: 1_700_000_000_000,
        }
    }

    // The two crates model a play separately; a field lost here never reaches Apple.
    #[test]
    fn player_events_map_to_the_sender_model() {
        let album = ContainerIds {
            album_adam_id: Some("1441164357".into()),
            ..ContainerIds::default()
        };
        let e = play_event(&event(EventType::PlayEnd, 3, Some(album)));
        assert_eq!(
            e.item,
            PlayedItem {
                id: ItemId::Catalog("1441164359".into()),
                media: MediaKind::Audio
            }
        );
        assert_eq!(
            e.container,
            Some(PlayContainer::Album(ItemId::Catalog("1441164357".into())))
        );
        assert_eq!(
            e.phase,
            PlayPhase::End {
                start: Duration::from_secs(1),
                end: Duration::from_secs(30),
                reason: AmpEnd::Paused
            }
        );
        assert_eq!(
            e.mode,
            PlayMode {
                shuffle: false,
                repeat: Repeat::All,
                autoplay: true
            }
        );
        assert_eq!(
            (e.duration, e.at),
            (
                Duration::from_secs(200),
                UNIX_EPOCH + Duration::from_millis(1_700_000_000_000)
            )
        );

        let library = ContainerIds {
            global_playlist_id: Some("pl.1".into()),
            universal_library_id: Some("p.2".into()),
            ..ContainerIds::default()
        };
        let mut start = event(EventType::PlayStart, 2, Some(library));
        start.ids.cloud_id = Some("i.3".into());
        let e = play_event(&start);
        assert_eq!(
            e.item.id,
            ItemId::Library {
                library_id: "i.3".into(),
                catalog_id: Some("1441164359".into())
            }
        );
        assert_eq!(
            e.container,
            Some(PlayContainer::Playlist {
                id: ItemId::Library {
                    library_id: "p.2".into(),
                    catalog_id: Some("pl.1".into())
                },
                version_hash: None
            })
        );
        assert_eq!(
            e.phase,
            PlayPhase::Start {
                position: Duration::from_secs(1),
                container_changed: false
            }
        );
        let station = ContainerIds {
            station_id: Some("ra.9".into()),
            ..ContainerIds::default()
        };
        let e = play_event(&event(EventType::PlayStart, 1, Some(station)));
        assert_eq!(
            e.container,
            Some(PlayContainer::Station {
                id: "ra.9".into(),
                hash: None
            })
        );
        assert_eq!(
            play_event(&event(EventType::PlayStart, 0, None)).container,
            None
        );
    }
}
