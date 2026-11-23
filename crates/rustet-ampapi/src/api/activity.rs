//! Play-activity reports: what MusicKit sends so Recently Played, Replay and
//! recommendations learn from plays (native-UI spec §2.4). This is the request builder and
//! sender only; when to send is the player's job (`rustet-player`).
//!
//! **Unverified.** Written from MusicKit JS v3 (`PlayActivitySender`, the field functions
//! of its play-activity encoder) and the web player's configuration; never sent while
//! writing this crate. The field rules below follow MusicKit for catalog and library songs
//! and music videos; live radio streams are not modelled.

use std::time::{Duration, SystemTime};

use reqwest::Method;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{Api, Call};
use crate::Error;
use crate::client::BROWSER_USER_AGENT;

/// MusicKit's `clientId` and `eventType`.
const CLIENT_ID: &str = "JSCLIENT";
const EVENT_TYPE: &str = "JSPLAY";
/// The web player passes `sourceType: 16` (WEB_APP); MusicKit's own default is 10.
const SOURCE_TYPE: u8 = 16;
/// MusicKit's `build-version` for the web player (`{name: "Apple Music", version: "1.0",
/// build: "2646.4.0-external"}`) on [`BROWSER_USER_AGENT`]: app, OS from the user agent,
/// `model/` + `navigator.platform`, build.
const BUILD_VERSION: &str =
    "AppleMusic/1.0 Windows/nt 10.0; win64; x64 model/Win32 build/2646.4.0-external";
/// `feature-name` without a container.
const FEATURE_DEFAULT: &str = "music_kit-integration";

/// A catalog or library ID of a played item or a container.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ItemId {
    Catalog(String),
    Library {
        library_id: String,
        /// The catalog ID, when the library item has one.
        catalog_id: Option<String>,
    },
}

/// Audio or video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Audio,
    Video,
}

/// The song or music video that played.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlayedItem {
    pub id: ItemId,
    pub media: MediaKind,
}

/// What the item was played from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlayContainer {
    Album(ItemId),
    Playlist {
        id: ItemId,
        /// `playParams.versionHash` of a catalog playlist.
        version_hash: Option<String>,
    },
    Station {
        id: String,
        /// `playParams.stationHash`.
        hash: Option<String>,
    },
    Artist(String),
}

/// Why playback of an item ended (MusicKit `PlayActivityEndReasonType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndReason {
    Other = 1,
    SkippedForwards = 2,
    Paused = 3,
    Suspended = 4,
    /// The user chose another item.
    OtherItemChosen = 5,
    PausedForInactivity = 6,
    NaturalEnd = 7,
    SessionTimeout = 8,
    Banned = 9,
    FailedToLoad = 10,
    PausedOnTimeout = 11,
    ScrubBegin = 12,
    ScrubEnd = 13,
    SkippedBackwards = 14,
    NotSupported = 15,
    QuickPlay = 16,
    ExitedApplication = 17,
}

/// Start or end of a play. An end reason exists only on an end.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlayPhase {
    Start {
        /// Where playback started (0 unless resumed or seeked).
        position: Duration,
        /// The item's container differs from the previous item's.
        container_changed: bool,
    },
    End {
        /// Where this stretch of playback started.
        start: Duration,
        end: Duration,
        reason: EndReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Repeat {
    Off,
    One,
    All,
}

/// The player's modes when the event happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlayMode {
    pub shuffle: bool,
    pub repeat: Repeat,
    pub autoplay: bool,
}

/// One play-activity event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayEvent {
    pub item: PlayedItem,
    pub container: Option<PlayContainer>,
    pub phase: PlayPhase,
    /// The item's duration.
    pub duration: Duration,
    pub mode: PlayMode,
    /// When it happened; sent as `milliseconds-since-play`, measured at send time.
    pub at: SystemTime,
    /// The local UTC offset then (east positive); `None` sends MusicKit's -1.
    pub utc_offset_seconds: Option<i32>,
    /// `meta.reco_data`-style recommendation data of the item, when it came from Home.
    pub reco_data: Option<String>,
}

/// The tokens and storefront every event repeats.
struct Context<'a> {
    developer_token: &'a str,
    user_token: &'a str,
    storefront: &'a str,
    now: SystemTime,
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl PlayContainer {
    /// MusicKit's `container-type`: 1 radio, 2 playlist, 3 album, 4 artist.
    fn kind(&self) -> u8 {
        match self {
            Self::Station { .. } => 1,
            Self::Playlist { .. } => 2,
            Self::Album(_) => 3,
            Self::Artist(_) => 4,
        }
    }

    /// The web player's container names.
    fn feature_name(&self) -> &'static str {
        match self {
            Self::Station { .. } => "station",
            Self::Playlist { .. } => "playlist",
            Self::Album(_) => "album",
            Self::Artist(_) => "artist",
        }
    }

    fn ids(&self) -> Map<String, Value> {
        let mut ids = Map::new();
        let mut put = |key: &str, value: &str| {
            ids.insert(key.to_owned(), value.into());
        };
        match self {
            Self::Album(ItemId::Catalog(id)) => put("album-adam-id", id),
            Self::Album(ItemId::Library { library_id, .. }) => put("cloud-album-id", library_id),
            Self::Playlist { id, version_hash } => {
                match id {
                    ItemId::Catalog(id) => put("global-playlist-id", id),
                    ItemId::Library {
                        library_id,
                        catalog_id,
                    } => {
                        if let Some(catalog_id) = catalog_id {
                            put("global-playlist-id", catalog_id);
                        }
                        put("universal-library-id", library_id);
                    }
                }
                if let Some(hash) = version_hash.as_deref().filter(|h| !h.is_empty()) {
                    put("playlist-version-hash", hash);
                }
            }
            Self::Station { id, hash } => {
                put("station-id", id);
                if let Some(hash) = hash.as_deref().filter(|h| !h.is_empty()) {
                    put("station-hash", hash);
                }
            }
            Self::Artist(_) => {}
        }
        ids
    }
}

impl PlayEvent {
    fn to_json(&self, context: &Context<'_>) -> Value {
        let mut ids = Map::new();
        match &self.item.id {
            ItemId::Catalog(id) => {
                ids.insert("subscription-adam-id".into(), id.as_str().into());
            }
            ItemId::Library {
                library_id,
                catalog_id,
            } => {
                if let Some(catalog_id) = catalog_id {
                    ids.insert("subscription-adam-id".into(), catalog_id.as_str().into());
                }
                ids.insert("cloud-id".into(), library_id.as_str().into());
            }
        }
        let duration = millis(self.duration);
        let since = millis(context.now.duration_since(self.at).unwrap_or_default());
        let mode = &self.mode;
        let mut event = json!({
            "ids": ids,
            "type": 1, // ITUNES_STORE_CONTENT
            "media-type": match self.item.media { MediaKind::Audio => 0, MediaKind::Video => 1 },
            "media-duration-in-milliseconds": duration,
            "milliseconds-since-play": since,
            "play-mode": {
                "auto-play-mode": if mode.autoplay { 2 } else { 1 },
                "repeat-play-mode": match mode.repeat { Repeat::Off => 1, Repeat::One => 2, Repeat::All => 3 },
                "shuffle-play-mode": if mode.shuffle { 2 } else { 1 },
            },
            "feature-name": self.container.as_ref().map_or(FEATURE_DEFAULT, PlayContainer::feature_name),
            "store-front": context.storefront,
            "utc-offset-in-seconds": self.utc_offset_seconds.unwrap_or(-1),
            "source-type": SOURCE_TYPE,
            "user-agent": BROWSER_USER_AGENT,
            "build-version": BUILD_VERSION,
            "developer-token": context.developer_token,
            "user-token": context.user_token,
            "persistent-id": uuid::Uuid::new_v4().to_string(),
            "offline": false,
            "private-enabled": false,
            "sb-enabled": true,
            "siri-initiated": false,
            "internal-build": false,
        });
        let map = event.as_object_mut().expect("an object literal");
        match &self.phase {
            PlayPhase::Start {
                position,
                container_changed,
            } => {
                map.insert("event-type".into(), 1.into());
                map.insert(
                    "start-position-in-milliseconds".into(),
                    millis(*position).into(),
                );
                map.insert(
                    "event-reason-hint-type".into(),
                    u8::from(*container_changed).into(),
                );
            }
            PlayPhase::End { start, end, reason } => {
                map.insert("event-type".into(), 0.into());
                map.insert(
                    "start-position-in-milliseconds".into(),
                    millis(*start).into(),
                );
                map.insert("end-position-in-milliseconds".into(), millis(*end).into());
                map.insert("end-reason-type".into(), (*reason as u8).into());
                // MusicKit reports a position past the end as the duration.
                map.insert(
                    "media-duration-in-milliseconds".into(),
                    duration.max(millis(*start)).into(),
                );
            }
        }
        if let Some(container) = &self.container {
            map.insert("container-type".into(), container.kind().into());
            let ids = container.ids();
            if !ids.is_empty() {
                map.insert("container-ids".into(), ids.into());
            }
        }
        if let Some(reco) = &self.reco_data {
            map.insert("reco-data".into(), reco.as_str().into());
        }
        event
    }
}

impl Api {
    /// Reports plays to Apple in one request. Nothing is sent for an empty list.
    ///
    /// The body is `{"client_id":"JSCLIENT","event_type":"JSPLAY","data":[…]}`, posted to
    /// [`PLAY_ACTIVITY_URL`](super::PLAY_ACTIVITY_URL) with the developer token and
    /// `media-user-token`. Each event carries both tokens, as MusicKit's do, so the body
    /// must never be logged.
    pub async fn report_plays(
        &self,
        cancel: &CancellationToken,
        events: &[PlayEvent],
    ) -> Result<(), Error> {
        if events.is_empty() {
            return Ok(());
        }
        let user_token = self.user_token().ok_or(Error::MissingUserToken)?;
        let developer_token = self.developer_token(cancel).await?;
        let locale = self.locale();
        let context = Context {
            developer_token: developer_token.secret(),
            user_token: user_token.secret(),
            storefront: &locale.storefront,
            now: SystemTime::now(),
        };
        let body = json!({
            "client_id": CLIENT_ID,
            "event_type": EVENT_TYPE,
            "data": events.iter().map(|e| e.to_json(&context)).collect::<Vec<_>>(),
        });
        let call = Call {
            method: Method::POST,
            url: Url::parse(&self.inner.play_activity_url)?,
            user: true,
            body: Some(body),
        };
        self.execute(cancel, call).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn context(now: SystemTime) -> Context<'static> {
        Context {
            developer_token: "dev",
            user_token: "mut",
            storefront: "us",
            now,
        }
    }

    fn event(phase: PlayPhase, container: Option<PlayContainer>) -> PlayEvent {
        PlayEvent {
            item: PlayedItem {
                id: ItemId::Catalog("1440857781".into()),
                media: MediaKind::Audio,
            },
            container,
            phase,
            duration: Duration::from_millis(200_500),
            mode: PlayMode {
                shuffle: true,
                repeat: Repeat::All,
                autoplay: false,
            },
            at: UNIX_EPOCH + Duration::from_secs(1_000),
            utc_offset_seconds: Some(3600),
            reco_data: None,
        }
    }

    #[test]
    fn start_event_fields() {
        let start = event(
            PlayPhase::Start {
                position: Duration::ZERO,
                container_changed: true,
            },
            Some(PlayContainer::Album(ItemId::Catalog("1440857700".into()))),
        );
        let now = start.at + Duration::from_millis(2500);
        let json = start.to_json(&context(now));
        assert_eq!(json["event-type"], 1);
        assert_eq!(json["ids"], json!({"subscription-adam-id": "1440857781"}));
        assert_eq!(json["container-type"], 3);
        assert_eq!(
            json["container-ids"],
            json!({"album-adam-id": "1440857700"})
        );
        assert_eq!(json["event-reason-hint-type"], 1);
        assert_eq!(json["milliseconds-since-play"], 2500);
        assert_eq!(json["start-position-in-milliseconds"], 0);
        assert_eq!(
            json["play-mode"],
            json!({"auto-play-mode": 1, "repeat-play-mode": 3, "shuffle-play-mode": 2})
        );
        assert_eq!(json["feature-name"], "album");
        assert_eq!(json["utc-offset-in-seconds"], 3600);
        assert!(json.get("end-reason-type").is_none() && json.get("timestamp").is_none());
    }

    #[test]
    fn end_event_fields_for_a_library_playlist() {
        let end = event(
            PlayPhase::End {
                start: Duration::from_millis(1000),
                end: Duration::from_millis(200_500),
                reason: EndReason::NaturalEnd,
            },
            Some(PlayContainer::Playlist {
                id: ItemId::Library {
                    library_id: "p.abc".into(),
                    catalog_id: Some("pl.xyz".into()),
                },
                version_hash: None,
            }),
        );
        let json = end.to_json(&context(end.at));
        assert_eq!(json["event-type"], 0);
        assert_eq!(json["end-reason-type"], 7);
        assert_eq!(json["end-position-in-milliseconds"], 200_500);
        assert_eq!(json["container-type"], 2);
        assert_eq!(
            json["container-ids"],
            json!({"global-playlist-id": "pl.xyz", "universal-library-id": "p.abc"})
        );
        assert!(json.get("event-reason-hint-type").is_none());
    }
}
