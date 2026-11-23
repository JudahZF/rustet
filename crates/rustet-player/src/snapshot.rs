//! On-disk formats: the resume snapshot (`player-state.json`) and the preferences
//! (`player-prefs.json`). Both are JSON with camelCase names.

use serde::{Deserialize, Serialize};

use crate::model::{Container, ItemSource, PlayerSettings, QueueItem, QueueItemIn, Repeat};

/// The snapshot version this crate writes. Others are ignored (v2 was the renderer's
/// `rustet_resume_state`, which lived in the UI's store).
pub const SNAPSHOT_VERSION: u32 = 3;
/// Snapshots older than this are ignored (renderer `_RESUME_MAX_AGE_MS`).
pub const SNAPSHOT_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// One queue entry in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotItem {
    #[serde(flatten)]
    pub item: QueueItemIn,
    pub source: ItemSource,
}

impl From<&QueueItem> for SnapshotItem {
    fn from(i: &QueueItem) -> Self {
        Self {
            item: QueueItemIn {
                id: i.id.clone(),
                library_id: i.library_id.clone(),
                kind: i.kind,
                meta: i.meta.clone(),
                container_id: i.container_id.clone(),
            },
            source: i.source,
        }
    }
}

/// `{v:3, items, current, repeat, shuffle, autoplay, context, positionMs, savedAt}` (spec §4.2),
/// plus `original`: the pre-shuffle order as indexes into `items`, so shuffle off still works
/// after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub v: u32,
    pub items: Vec<SnapshotItem>,
    pub current: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<Vec<usize>>,
    pub repeat: Repeat,
    pub shuffle: bool,
    pub autoplay: bool,
    pub context: Option<Container>,
    pub position_ms: u64,
    /// Unix ms.
    pub saved_at: u64,
}

impl Snapshot {
    /// Parses a stored snapshot. `None` for another version, an empty queue, a snapshot older
    /// than seven days (or from the future), or bytes that are not a snapshot.
    pub fn parse(bytes: &[u8], now_ms: u64) -> Option<Self> {
        #[derive(Deserialize)]
        struct Version {
            v: Option<u32>,
        }
        if serde_json::from_slice::<Version>(bytes).ok()?.v != Some(SNAPSHOT_VERSION) {
            return None;
        }
        let snap: Self = serde_json::from_slice(bytes).ok()?;
        let fresh = snap.saved_at <= now_ms && now_ms - snap.saved_at <= SNAPSHOT_MAX_AGE_MS;
        (fresh && !snap.items.is_empty()).then_some(snap)
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a snapshot always serialises")
    }
}

/// Volume and settings, kept across restarts. A mute never carries over, and the volume is the
/// last audible one (renderer `rustet.vlcVolume`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    pub volume: u8,
    pub settings: PlayerSettings,
    /// Music videos play as audio only.
    pub mv_audio_only: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            volume: 100,
            settings: PlayerSettings::default(),
            mv_audio_only: false,
        }
    }
}

impl Prefs {
    /// Unreadable bytes give the defaults; a stored volume of 0 or above 100 gives 100.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut p: Self = serde_json::from_slice(bytes).unwrap_or_default();
        if p.volume == 0 || p.volume > 100 {
            p.volume = 100;
        }
        p
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("prefs always serialise")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CatalogId, ItemKind, TrackMeta};
    use serde_json::json;

    fn snap(saved_at: u64) -> Snapshot {
        Snapshot {
            v: SNAPSHOT_VERSION,
            items: vec![SnapshotItem {
                item: QueueItemIn {
                    id: CatalogId("1".into()),
                    library_id: None,
                    kind: ItemKind::Song,
                    meta: TrackMeta {
                        name: "N".into(),
                        ..TrackMeta::default()
                    },
                    container_id: None,
                },
                source: ItemSource::Context,
            }],
            current: Some(0),
            original: None,
            repeat: Repeat::One,
            shuffle: false,
            autoplay: true,
            context: None,
            position_ms: 61_000,
            saved_at,
        }
    }

    #[test]
    fn the_wire_shape_is_flat_camel_case() {
        let v: serde_json::Value = serde_json::from_slice(&snap(5).to_json()).unwrap();
        assert_eq!(v["v"], 3);
        assert_eq!(v["positionMs"], 61_000);
        assert_eq!(v["savedAt"], 5);
        assert_eq!(v["repeat"], "one");
        assert_eq!(
            v["items"][0],
            json!({"id": "1", "kind": "song", "meta": {
            "name": "N", "artistName": "", "albumName": "", "artworkUrl": "", "durationMs": 0, "audioTraits": []
        }, "source": "context"})
        );
    }

    #[test]
    fn a_fresh_snapshot_round_trips() {
        let now = 1_000_000_000;
        assert_eq!(
            Snapshot::parse(&snap(now - 1000).to_json(), now),
            Some(snap(now - 1000))
        );
    }

    #[test]
    fn old_other_version_and_broken_snapshots_are_ignored() {
        let now = SNAPSHOT_MAX_AGE_MS * 3;
        assert_eq!(
            Snapshot::parse(&snap(now - SNAPSHOT_MAX_AGE_MS - 1).to_json(), now),
            None,
            "older than 7 days"
        );
        assert!(Snapshot::parse(&snap(now - SNAPSHOT_MAX_AGE_MS).to_json(), now).is_some());
        assert_eq!(
            Snapshot::parse(&snap(now + 60_000).to_json(), now),
            None,
            "from the future"
        );
        let v2 = json!({"v": 2, "containers": [{"items": ["1"]}], "adamId": "1", "savedAt": now});
        assert_eq!(
            Snapshot::parse(v2.to_string().as_bytes(), now),
            None,
            "the renderer's v2"
        );
        assert_eq!(Snapshot::parse(b"not json", now), None);
        let mut empty = snap(now);
        empty.items.clear();
        assert_eq!(Snapshot::parse(&empty.to_json(), now), None);
    }

    #[test]
    fn prefs_defaults_and_bad_volumes() {
        assert_eq!(Prefs::parse(b"garbage"), Prefs::default());
        assert_eq!(
            Prefs::parse(br#"{"volume": 0}"#).volume,
            100,
            "a mute never carries over"
        );
        assert_eq!(Prefs::parse(br#"{"volume": 40}"#).volume, 40);
        let p = Prefs::parse(br#"{"volume": 40, "settings": {"quality": "highQuality"}}"#);
        assert_eq!(
            p.settings.quality,
            crate::model::StreamingQuality::HighQuality
        );
        assert!(p.settings.report_plays, "absent fields keep their defaults");
    }
}
