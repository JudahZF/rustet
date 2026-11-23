//! Play-activity events (spec §2.4): what Apple's `universal-activity-service` learns from a play.
//!
//! The player decides **when** and **what**: a `PLAY_START` when audio starts or resumes, a
//! `PLAY_END` with its reason when it stops (pause, seek, skip, natural end, quit). Field names
//! and values are MusicKit v3's (`musickit.js`, from code, unverified against Apple).
//!
//! The sender (`rustet_ampapi`) adds the client fields and the envelope: `store-front`,
//! `utc-offset-in-seconds`, `user-agent`, `build-version`, `persistent-id`, `guid`,
//! `milliseconds-since-play` (now − `timestamp` at send time) and
//! `{"client_id":"JSCLIENT","event_type":"JSPLAY","data":[…]}`.

use serde::{Serialize, Serializer};

use crate::model::{Container, ContainerKind, ItemKind, QueueItem, Repeat};

/// `event-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    PlayEnd = 0,
    PlayStart = 1,
}

/// `end-reason-type` (MusicKit `PlayActivityEndReasonType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Other = 1,
    SkippedForwards = 2,
    ManuallyPaused = 3,
    OtherItemChosen = 5,
    NaturalEnd = 7,
    FailedToLoad = 10,
    ScrubBegin = 12,
    SkippedBackwards = 14,
    ExitedApplication = 17,
}

macro_rules! serialize_as_u8 {
    ($($t:ty),*) => {$(
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_u8(*self as u8)
            }
        }
    )*};
}
serialize_as_u8!(EventType, EndReason);

/// `ids`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Ids {
    /// The catalog ID of a subscription (streamed) item.
    pub subscription_adam_id: String,
    /// The library ID, for library items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_id: Option<String>,
}

/// `container-ids`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ContainerIds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_adam_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_album_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_playlist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub universal_library_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub station_id: Option<String>,
}

/// `play-mode`: MusicKit's numbers (repeat 0 none, 1 one, 2 all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct PlayMode {
    pub auto_play_mode: u8,
    pub repeat_play_mode: u8,
    pub shuffle_play_mode: u8,
}

impl PlayMode {
    pub fn new(autoplay: bool, repeat: Repeat, shuffle: bool) -> Self {
        let repeat_play_mode = match repeat {
            Repeat::Off => 0,
            Repeat::One => 1,
            Repeat::All => 2,
        };
        Self {
            auto_play_mode: autoplay.into(),
            repeat_play_mode,
            shuffle_play_mode: shuffle.into(),
        }
    }
}

/// One play-activity event, serialised with MusicKit's field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct PlayActivityEvent {
    pub event_type: EventType,
    /// 1 = ITUNES_STORE_CONTENT (4 = STREAM, for live stations later).
    #[serde(rename = "type")]
    pub item_type: u8,
    pub ids: Ids,
    /// 0 unknown, 1 radio, 2 playlist, 3 album, 4 artist.
    pub container_type: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_ids: Option<ContainerIds>,
    pub start_position_in_milliseconds: u64,
    /// `PLAY_END` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_position_in_milliseconds: Option<u64>,
    pub media_duration_in_milliseconds: u64,
    /// `PLAY_END` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason_type: Option<EndReason>,
    pub play_mode: PlayMode,
    /// 0 audio, 1 video.
    pub media_type: u8,
    /// Unix ms of the event.
    pub timestamp: u64,
}

/// What a report needs to know about the item and the queue.
pub(crate) struct ReportContext<'a> {
    pub item: &'a QueueItem,
    pub container: Option<&'a Container>,
    pub duration_ms: u64,
    pub play_mode: PlayMode,
}

impl ReportContext<'_> {
    fn event(&self, event_type: EventType, start_ms: u64, timestamp: u64) -> PlayActivityEvent {
        let (container_type, container_ids) = container_fields(self.container, self.item);
        PlayActivityEvent {
            event_type,
            item_type: 1,
            ids: Ids {
                subscription_adam_id: self.item.id.0.clone(),
                cloud_id: self.item.library_id.clone(),
            },
            container_type,
            container_ids,
            start_position_in_milliseconds: start_ms,
            end_position_in_milliseconds: None,
            media_duration_in_milliseconds: self.duration_ms,
            end_reason_type: None,
            play_mode: self.play_mode,
            media_type: match self.item.kind {
                ItemKind::Song => 0,
                ItemKind::MusicVideo => 1,
            },
            timestamp,
        }
    }

    pub fn start(&self, position_ms: u64, timestamp: u64) -> PlayActivityEvent {
        self.event(EventType::PlayStart, position_ms, timestamp)
    }

    pub fn end(
        &self,
        start_ms: u64,
        end_ms: u64,
        reason: EndReason,
        timestamp: u64,
    ) -> PlayActivityEvent {
        PlayActivityEvent {
            end_position_in_milliseconds: Some(end_ms),
            end_reason_type: Some(reason),
            ..self.event(EventType::PlayEnd, start_ms, timestamp)
        }
    }
}

/// MusicKit's `container-type` and `container-ids` for a queue context.
fn container_fields(container: Option<&Container>, item: &QueueItem) -> (u8, Option<ContainerIds>) {
    let Some(c) = container else { return (0, None) };
    let id = Some(c.id.clone());
    let (kind, ids) = match c.kind {
        ContainerKind::Album => (
            3,
            ContainerIds {
                album_adam_id: id,
                ..ContainerIds::default()
            },
        ),
        ContainerKind::LibraryAlbum => (
            3,
            ContainerIds {
                cloud_album_id: id,
                ..ContainerIds::default()
            },
        ),
        ContainerKind::Playlist => (
            2,
            ContainerIds {
                global_playlist_id: id,
                ..ContainerIds::default()
            },
        ),
        ContainerKind::LibraryPlaylist => (
            2,
            ContainerIds {
                universal_library_id: id,
                ..ContainerIds::default()
            },
        ),
        ContainerKind::Station | ContainerKind::LiveStation => (
            1,
            ContainerIds {
                station_id: id,
                ..ContainerIds::default()
            },
        ),
        ContainerKind::Artist => (4, ContainerIds::default()),
        ContainerKind::LibrarySongs => (0, ContainerIds::default()),
    };
    // An autoplay item does not belong to the context that ran out.
    if item.source == crate::model::ItemSource::Autoplay {
        return (0, None);
    }
    let empty = ids == ContainerIds::default();
    (kind, (!empty).then_some(ids))
}

/// Pending events, flushed on `PLAY_END`, every 30 s and on quit. Failed sends come back and are
/// kept, oldest dropped first.
#[derive(Debug, Default)]
pub(crate) struct Outbox {
    pending: Vec<PlayActivityEvent>,
}

/// Events kept while Apple cannot be reached.
const OUTBOX_CAP: usize = 200;

impl Outbox {
    pub fn push(&mut self, e: PlayActivityEvent) {
        self.pending.push(e);
        self.trim();
    }

    /// A failed batch goes back in front of what was queued since.
    pub fn requeue(&mut self, mut batch: Vec<PlayActivityEvent>) {
        batch.append(&mut self.pending);
        self.pending = batch;
        self.trim();
    }

    fn trim(&mut self) {
        let excess = self.pending.len().saturating_sub(OUTBOX_CAP);
        self.pending.drain(..excess);
    }

    pub fn take(&mut self) -> Vec<PlayActivityEvent> {
        std::mem::take(&mut self.pending)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CatalogId, ItemSource, TrackMeta};
    use serde_json::json;

    fn item(source: ItemSource) -> QueueItem {
        QueueItem {
            uid: 1,
            id: CatalogId("1441164359".into()),
            library_id: Some("i.abc".into()),
            kind: ItemKind::Song,
            source,
            meta: TrackMeta::default(),
            container_id: None,
        }
    }

    #[test]
    fn a_play_end_uses_musickit_names_and_numbers() {
        let album = Container {
            kind: ContainerKind::Album,
            id: "1441164357".into(),
            name: None,
        };
        let it = item(ItemSource::Context);
        let ctx = ReportContext {
            item: &it,
            container: Some(&album),
            duration_ms: 200_000,
            play_mode: PlayMode::new(false, Repeat::All, true),
        };
        let v =
            serde_json::to_value(ctx.end(1000, 61_000, EndReason::SkippedForwards, 99)).unwrap();
        assert_eq!(
            v,
            json!({
                "event-type": 0, "type": 1,
                "ids": {"subscription-adam-id": "1441164359", "cloud-id": "i.abc"},
                "container-type": 3, "container-ids": {"album-adam-id": "1441164357"},
                "start-position-in-milliseconds": 1000, "end-position-in-milliseconds": 61000,
                "media-duration-in-milliseconds": 200000, "end-reason-type": 2,
                "play-mode": {"auto-play-mode": 0, "repeat-play-mode": 2, "shuffle-play-mode": 1},
                "media-type": 0, "timestamp": 99
            })
        );
        let start = serde_json::to_value(ctx.start(0, 5)).unwrap();
        assert_eq!(start["event-type"], 1);
        assert!(
            start.get("end-reason-type").is_none()
                && start.get("end-position-in-milliseconds").is_none()
        );
    }

    #[test]
    fn autoplay_items_report_no_container() {
        let album = Container {
            kind: ContainerKind::Album,
            id: "1".into(),
            name: None,
        };
        let it = item(ItemSource::Autoplay);
        let ctx = ReportContext {
            item: &it,
            container: Some(&album),
            duration_ms: 0,
            play_mode: PlayMode::new(true, Repeat::Off, false),
        };
        let v = serde_json::to_value(ctx.start(0, 0)).unwrap();
        assert_eq!(v["container-type"], 0);
        assert!(v.get("container-ids").is_none());
        assert_eq!(v["play-mode"]["auto-play-mode"], 1);
    }

    #[test]
    fn the_outbox_keeps_failed_batches_in_order_and_is_bounded() {
        let it = item(ItemSource::User);
        let ctx = ReportContext {
            item: &it,
            container: None,
            duration_ms: 0,
            play_mode: PlayMode::new(false, Repeat::Off, false),
        };
        let mut o = Outbox::default();
        o.push(ctx.start(0, 1));
        let batch = o.take();
        o.push(ctx.start(0, 2));
        o.requeue(batch);
        assert_eq!(
            o.take().iter().map(|e| e.timestamp).collect::<Vec<_>>(),
            [1, 2]
        );
        for t in 0..(OUTBOX_CAP as u64 + 5) {
            o.push(ctx.start(0, t));
        }
        let kept = o.take();
        assert_eq!(kept.len(), OUTBOX_CAP);
        assert_eq!(kept[0].timestamp, 5, "oldest dropped first");
    }
}
