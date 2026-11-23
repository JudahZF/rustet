//! [`Deck`] over two `rustet_vlc` decks on one libvlc instance.
//!
//! Each load is fed from the session's disk-cache entry as `/api/v1/vlc/load` feeds it (the
//! committed file, a reader on the running download, or a download started now), or from a
//! [`spool`](super::spool) when that is impossible or the load starts mid-track. A music video
//! with video on plays its video and audio joined into one stream ([`super::mvmux`]) from a spool,
//! with its frames in a ring in the engine's `video_dir`; a seek inside what is spooled reopens
//! libvlc on a view of that spool (the player's load stays), a seek beyond it reloads. libvlc's
//! load IDs are mapped to the player's, and an event of a replaced load is dropped here.

use std::ffi::CStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
use rustet_player::model::VideoSurface;
use rustet_player::ports::{
    Deck, DeckError, DeckEvent, DeckEvents, DeckId, DeckSignal, DeckTiming, LoadId, LoadRequest,
};
use rustet_vlc::{Event as VlcEvent, Instance, Start, VideoOutput};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::spool::{self, MusicVideo};
use crate::handlers::playback::qualifier;
use crate::handlers::vlc::{CacheSource, needs_hold, open_source};
use crate::state::Engine;

/// Events of a load libvlc started before `load` returned its ID; kept until the ID is known.
const EARLY_CAP: usize = 64;
/// A music-video load waits this long for both init segments (the frame size comes from them).
const MV_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the current load reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    /// The committed cache file: libvlc seeks in it at once.
    File,
    /// A reader on the running cache download: seekable once the download committed.
    Download,
    /// A stream outside the cache, from its start position: a seek reloads.
    Spool,
    /// A music video's video and audio joined into one stream ([`super::mvmux`]).
    MusicVideo,
}

struct Current {
    player: LoadId,
    vlc: rustet_vlc::LoadId,
    session: String,
    asset: String,
    q: &'static str,
    feed: Feed,
    /// Opened paused and not resumed yet: libvlc's `Playing` of the pre-roll is not a start.
    cued: bool,
    /// A music video: its spool (seeks inside it open a view on it) and where its frames go.
    video: Option<(MusicVideo, VideoOutput)>,
}

#[derive(Default)]
struct Slot {
    current: Option<Current>,
    early: Vec<rustet_vlc::DeckEvent>,
    level: u8,
    muted: bool,
    /// libvlc is paused (or cued): a reopen for a seek stays paused.
    paused: bool,
}

/// One deck of the player.
pub(crate) struct VlcDeck {
    id: DeckId,
    vlc: Arc<rustet_vlc::Deck>,
    engine: Weak<Engine>,
    events: DeckEvents,
    slot: Mutex<Slot>,
}

fn failed(e: impl std::fmt::Display) -> DeckError {
    DeckError::Failed(e.to_string())
}

/// Runs a libvlc call off the runtime workers (they block on libvlc's input thread).
async fn blocking<R: Send + 'static>(
    vlc: &Arc<rustet_vlc::Deck>,
    f: impl FnOnce(&rustet_vlc::Deck) -> R + Send + 'static,
) -> R {
    let vlc = vlc.clone();
    match tokio::task::spawn_blocking(move || f(&vlc)).await {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// Creates the two decks on a new libvlc instance with `args`, and the task that forwards their
/// events. `None` when libvlc is not available.
pub(crate) async fn libvlc_decks(
    engine: &Arc<Engine>,
    args: &'static [&'static CStr],
    events: DeckEvents,
) -> Option<[Arc<VlcDeck>; 2]> {
    let (tx, rx) = mpsc::unbounded_channel();
    let made = tokio::task::spawn_blocking(move || {
        let inst = Instance::with_args(args)?;
        Ok::<_, rustet_vlc::Error>((inst.deck(tx.clone())?, inst.deck(tx)?))
    })
    .await;
    let (a, b) = match made {
        Ok(Ok(decks)) => decks,
        Ok(Err(e)) => {
            info!("player: {e}; no libvlc decks");
            return None;
        }
        Err(e) => {
            warn!("player: libvlc init: {e}");
            return None;
        }
    };
    let deck = |id, vlc| {
        Arc::new(VlcDeck {
            id,
            vlc: Arc::new(vlc),
            engine: Arc::downgrade(engine),
            events: events.clone(),
            slot: Mutex::new(Slot {
                level: 100,
                ..Slot::default()
            }),
        })
    };
    let decks = [deck(DeckId::A, a), deck(DeckId::B, b)];
    tokio::spawn(pump(decks.clone(), rx));
    Some(decks)
}

/// Routes libvlc's events to their deck.
async fn pump(decks: [Arc<VlcDeck>; 2], mut rx: mpsc::UnboundedReceiver<rustet_vlc::DeckEvent>) {
    while let Some(ev) = rx.recv().await {
        if let Some(deck) = decks.iter().find(|d| d.vlc.id() == ev.deck) {
            deck.on_vlc(ev).await;
        }
    }
}

impl VlcDeck {
    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The player's event for a libvlc event of the current load, if it has one.
    fn translate(cur: &Current, event: VlcEvent) -> Option<(LoadId, VlcEvent)> {
        match event {
            VlcEvent::Playing if cur.cued => None,
            VlcEvent::Playing
            | VlcEvent::EndReached
            | VlcEvent::Error
            | VlcEvent::VideoStarted { .. }
            | VlcEvent::VideoStopped => Some((cur.player, event)),
            _ => None,
        }
    }

    async fn on_vlc(&self, ev: rustet_vlc::DeckEvent) {
        let action = {
            let mut slot = self.lock();
            match &slot.current {
                Some(cur) if cur.vlc == ev.load => Self::translate(cur, ev.event),
                // A replaced load: dropped.
                Some(cur) if ev.load < cur.vlc => None,
                // A load whose ID `load` has not stored yet (or after a stop).
                _ => {
                    if slot.early.len() < EARLY_CAP {
                        slot.early.push(ev);
                    }
                    None
                }
            }
        };
        if let Some((load, event)) = action {
            self.send(load, event).await;
        }
    }

    async fn send(&self, load: LoadId, event: VlcEvent) {
        let event = match event {
            VlcEvent::Playing => DeckEvent::Playing,
            VlcEvent::Error => DeckEvent::Error,
            VlcEvent::VideoStarted {
                generation,
                width,
                height,
            } => {
                let Some(dir) = self.engine.upgrade().and_then(|e| e.video_dir.clone()) else {
                    return;
                };
                let ring = dir.join(rustet_framering::file_name(std::process::id(), generation));
                DeckEvent::Video(Some(VideoSurface {
                    ring: ring.to_string_lossy().into_owned(),
                    generation,
                    width,
                    height,
                }))
            }
            VlcEvent::VideoStopped => DeckEvent::Video(None),
            _ => DeckEvent::Ended {
                source_eof: blocking(&self.vlc, rustet_vlc::Deck::source_ended).await,
            },
        };
        let _ = self.events.send(DeckSignal {
            deck: self.id,
            load,
            event,
        });
    }

    async fn apply_volume(&self, level: u8, muted: bool) {
        {
            let mut slot = self.lock();
            (slot.level, slot.muted) = (level, muted);
        }
        // libvlc has no mute call here (its set_volume unmutes): mute is level 0.
        let level = if muted { 0 } else { i64::from(level) };
        blocking(&self.vlc, move |d| d.set_volume(level)).await;
    }

    /// Makes `cur` the current load and passes on the events libvlc sent for it before.
    async fn adopt(&self, cur: Current) {
        let early = {
            let mut slot = self.lock();
            let vlc_load = cur.vlc;
            let early = std::mem::take(&mut slot.early);
            let events = early
                .into_iter()
                .filter(|e| e.load == vlc_load)
                .filter_map(|e| Self::translate(&cur, e.event))
                .collect::<Vec<_>>();
            slot.current = Some(cur);
            events
        };
        for (load, event) in early {
            self.send(load, event).await;
        }
    }

    /// A seek in a music video. Inside what is spooled, libvlc reopens a view of the spool that
    /// starts at the fragments holding the target and lands on it with its start-time: no new
    /// download, and the player keeps its load (the ring is new). Beyond it, the player reloads,
    /// which streams from the segment holding the target.
    async fn seek_video(
        &self,
        mv: MusicVideo,
        output: VideoOutput,
        position_ms: u64,
    ) -> Result<(), DeckError> {
        let view = tokio::task::spawn_blocking(move || mv.view(position_ms))
            .await
            .map_err(failed)?;
        let Some(src) = view else {
            return Err(DeckError::NeedsReload);
        };
        let src = src.map_err(failed)?;
        let start = if self.lock().paused {
            Start::Paused
        } else {
            Start::Playing
        };
        let vlc_load = blocking(&self.vlc, move |d| {
            d.open_video(src, start, position_ms, output)
        })
        .await
        .map_err(failed)?;
        let cur = {
            let mut slot = self.lock();
            let Some(mut cur) = slot.current.take() else {
                return Err(failed("nothing loaded"));
            };
            cur.vlc = vlc_load;
            cur.cued = start == Start::Paused;
            cur
        };
        debug!(
            session = cur.session,
            position_ms, "player: music video seeks in its spool"
        );
        self.adopt(cur).await;
        Ok(())
    }

    /// The libvlc deck, for the live tests that compare a direct seek.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &Arc<rustet_vlc::Deck> {
        &self.vlc
    }

    fn engine(&self) -> Result<Arc<Engine>, DeckError> {
        self.engine
            .upgrade()
            .ok_or_else(|| failed("the engine stopped"))
    }
}

#[async_trait]
impl Deck for VlcDeck {
    async fn load(&self, req: LoadRequest) -> Result<(), DeckError> {
        let engine = self.engine()?;
        let playback = engine
            .playback
            .as_ref()
            .ok_or_else(|| failed("playback not available"))?;
        let sess = playback
            .manager
            .session(&req.session.0)
            .ok_or(DeckError::SessionGone)?;
        let (id, q) = (req.session.0.clone(), qualifier(&sess));
        engine.player_takes_libvlc().await;
        self.apply_volume(req.volume, req.muted).await;

        let committed = match &engine.disk_cache {
            Some(c) => c.path(&sess.asset_id, q).await.is_some(),
            None => false,
        };
        let music_video = sess.kind == rustet_core::media::SessionKind::MusicVideo;
        let video_dir = engine
            .video_dir
            .clone()
            .filter(|_| music_video && req.video);
        // libvlc blocks on a seek in a growing file: a mid-track start of an uncached track
        // streams from the target instead. A music video's audio alone streams from a spool too.
        let cached =
            if music_video || (req.start_ms > 0 && !committed && sess.capabilities.seekable) {
                None
            } else {
                open_source(&engine, &id, &sess.asset_id, q)
                    .await
                    .map_err(failed)?
            };
        if !req.paused {
            let hold = cached.is_some() && needs_hold(&engine, &sess.asset_id, q).await;
            engine.alac.foreground(&id, hold);
        }
        let start = if req.paused {
            Start::Paused
        } else {
            Start::Playing
        };
        let mut video = None;
        let (feed, vlc_load) = match (cached, video_dir) {
            (_, Some(dir)) => {
                // From the segment holding the start; libvlc's start-time lands on it exactly.
                let (src, mv) = spool::start_music_video(&engine, &id, req.start_ms)
                    .await
                    .map_err(failed)?;
                // The frame size is in the init segments; libvlc only knows the decoder's padded one.
                let mut size = mv.video_size.clone();
                let init = size.wait_for(Option::is_some);
                let source_size = match tokio::time::timeout(MV_INIT_TIMEOUT, init).await {
                    Ok(Ok(s)) => *s,
                    _ => None,
                };
                let output = VideoOutput {
                    dir,
                    max_height: sess.mv_max_height,
                    source_size,
                };
                debug!(
                    session = id,
                    start_ms = req.start_ms,
                    ?source_size,
                    "player: deck plays a music video"
                );
                video = Some((mv, output.clone()));
                let at = req.start_ms;
                (
                    Feed::MusicVideo,
                    blocking(&self.vlc, move |d| d.open_video(src, start, at, output)).await,
                )
            }
            (Some(src), None) => {
                let feed = if matches!(src, CacheSource::File { .. }) {
                    Feed::File
                } else {
                    Feed::Download
                };
                let start_ms = i64::try_from(req.start_ms).unwrap_or(i64::MAX);
                let load = blocking(&self.vlc, move |d| {
                    let load = d.open_source(src, start)?;
                    // Applied once libvlc plays; only a complete file gets here with a start.
                    if start_ms > 0 {
                        d.set_time(start_ms);
                    }
                    Ok::<_, rustet_vlc::Error>(load)
                })
                .await;
                (feed, load)
            }
            (None, None) => {
                let src = spool::start(&engine, &id, req.start_ms)
                    .await
                    .map_err(failed)?;
                debug!(
                    session = id,
                    start_ms = req.start_ms,
                    "player: deck reads a spool"
                );
                (
                    Feed::Spool,
                    blocking(&self.vlc, move |d| d.open_source(src, start)).await,
                )
            }
        };
        let vlc_load = vlc_load.map_err(|e| {
            engine.alac.done(&id);
            failed(e)
        })?;
        self.adopt(Current {
            player: req.load,
            vlc: vlc_load,
            session: id,
            asset: sess.asset_id.clone(),
            q,
            feed,
            cued: req.paused,
            video,
        })
        .await;
        self.lock().paused = req.paused;
        Ok(())
    }

    async fn play(&self) -> Result<(), DeckError> {
        let cue = {
            let mut slot = self.lock();
            slot.current.as_mut().filter(|c| c.cued).map(|c| {
                c.cued = false;
                (c.session.clone(), c.asset.clone(), c.q, c.feed)
            })
        };
        let engine = self.engine()?;
        // A resume after the web shell's routes took libvlc starts playback too.
        engine.player_takes_libvlc().await;
        // A pre-rolled track becomes the one the player waits on.
        if let Some((id, asset, q, feed)) = cue {
            let hold = feed == Feed::Download && needs_hold(&engine, &asset, q).await;
            engine.alac.foreground(&id, hold);
        }
        self.lock().paused = false;
        blocking(&self.vlc, rustet_vlc::Deck::resume).await;
        Ok(())
    }

    async fn pause(&self) -> Result<(), DeckError> {
        self.lock().paused = true;
        blocking(&self.vlc, rustet_vlc::Deck::pause).await;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), DeckError> {
        let (feed, asset, q, video) = {
            let slot = self.lock();
            let cur = slot
                .current
                .as_ref()
                .ok_or_else(|| failed("nothing loaded"))?;
            (cur.feed, cur.asset.clone(), cur.q, cur.video.clone())
        };
        if let Some((mv, output)) = video {
            return self.seek_video(mv, output, position_ms).await;
        }
        let complete = match feed {
            Feed::File => true,
            // The reader follows the same file: once committed every byte is there.
            Feed::Download => match &self.engine()?.disk_cache {
                Some(c) => c.path(&asset, q).await.is_some(),
                None => false,
            },
            Feed::Spool | Feed::MusicVideo => false,
        };
        if !complete {
            return Err(DeckError::NeedsReload);
        }
        let ms = i64::try_from(position_ms).unwrap_or(i64::MAX);
        blocking(&self.vlc, move |d| d.set_time(ms)).await;
        Ok(())
    }

    async fn stop(&self) -> Result<(), DeckError> {
        {
            let mut slot = self.lock();
            slot.current = None;
            slot.early.clear();
        }
        blocking(&self.vlc, rustet_vlc::Deck::stop).await;
        Ok(())
    }

    async fn set_volume(&self, level: u8, muted: bool) -> Result<(), DeckError> {
        self.apply_volume(level, muted).await;
        Ok(())
    }

    async fn timing(&self) -> DeckTiming {
        let (t, v, source_ended) =
            blocking(&self.vlc, |d| (d.timing(), d.volume(), d.source_ended())).await;
        let (level, muted) = {
            let slot = self.lock();
            (slot.level, slot.muted)
        };
        // Muted here is level 0 in libvlc: report the level the user had.
        let (volume, muted) = if muted && v.level == 0 {
            (level, true)
        } else {
            (v.level.min(100) as u8, v.muted)
        };
        DeckTiming {
            position_ms: u64::try_from(t.position_ms).ok().filter(|&p| p > 0),
            length_ms: u64::try_from(t.length_ms).ok().filter(|&l| l > 0),
            source_ended,
            volume,
            muted,
        }
    }
}

/// The deck when libvlc is not available: every load fails, so the player reports the error.
pub(crate) struct NoDeck;

#[async_trait]
impl Deck for NoDeck {
    async fn load(&self, _: LoadRequest) -> Result<(), DeckError> {
        Err(failed("libvlc not available"))
    }
    async fn play(&self) -> Result<(), DeckError> {
        Ok(())
    }
    async fn pause(&self) -> Result<(), DeckError> {
        Ok(())
    }
    async fn seek(&self, _: u64) -> Result<(), DeckError> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), DeckError> {
        Ok(())
    }
    async fn set_volume(&self, _: u8, _: bool) -> Result<(), DeckError> {
        Ok(())
    }
    async fn timing(&self) -> DeckTiming {
        DeckTiming::default()
    }
}
