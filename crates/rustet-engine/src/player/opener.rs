//! [`Opener`] over the session manager and the pre-warm scheduler.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use rustet_core::media::{OpenRequest as SessionRequest, SessionKind, SpatialAudio};
use rustet_core::pipeline::Codec as SessionCodec;
use rustet_playback::Session;
use rustet_player::model::{Codec, ItemKind, Spatial};
use rustet_player::ports::{
    OpenClass, OpenError, OpenMode, OpenRequest, OpenedSession, Opener, SessionId,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::handlers::playback::{
    self as routes, open_session, qualifier, release_session, start_precache,
};
use crate::state::Engine;

pub(crate) struct EngineOpener(pub Weak<Engine>);

fn fail(class: OpenClass, message: impl Into<String>) -> OpenError {
    OpenError {
        class,
        message: message.into(),
    }
}

impl EngineOpener {
    fn engine(&self) -> Result<Arc<Engine>, OpenError> {
        self.0
            .upgrade()
            .ok_or_else(|| fail(OpenClass::Other, "the engine stopped"))
    }
}

impl From<routes::OpenClass> for OpenClass {
    fn from(c: routes::OpenClass) -> Self {
        use routes::OpenClass as C;
        match c {
            C::Cancelled => Self::Cancelled,
            C::NotFound => Self::NotFound,
            C::Unavailable => Self::Unavailable,
            C::LicenceRefused => Self::LicenceRefused,
            C::Authentication => Self::Authentication,
            C::Transport => Self::Transport,
            C::Other => Self::Other,
        }
    }
}

/// The parts of the session descriptor the player needs.
fn describe(sess: &Session) -> OpenedSession {
    let video = sess.kind == SessionKind::MusicVideo;
    let codec = match sess.codec {
        _ if video => Codec::H264,
        Some(SessionCodec::Alac) => Codec::Alac,
        Some(SessionCodec::Atmos) => Codec::Atmos,
        Some(SessionCodec::H264) => Codec::H264,
        Some(SessionCodec::Aac) | None => Codec::Aac,
    };
    let video_height = video.then(|| match sess.mv_max_height {
        0 => sess.video_heights.iter().copied().max().unwrap_or(0),
        max => sess
            .video_heights
            .iter()
            .copied()
            .filter(|&h| h <= max)
            .max()
            .unwrap_or(max),
    });
    OpenedSession {
        id: SessionId(sess.id.clone()),
        codec,
        sample_rate: sess.sample_rate,
        bit_depth: sess.bit_depth,
        bit_rate: sess.bit_rate,
        channels: sess.channel_count,
        spatial: sess.spatial_audio.map(|s| match s {
            SpatialAudio::Binaural => Spatial::Binaural,
            SpatialAudio::BinauralLossless => Spatial::BinauralLossless,
        }),
        duration_ms: u64::from(sess.duration_ms),
        video_height,
    }
}

#[async_trait]
impl Opener for EngineOpener {
    async fn open(
        &self,
        req: OpenRequest,
        cancel: CancellationToken,
    ) -> Result<OpenedSession, OpenError> {
        let engine = self.engine()?;
        let p = engine
            .playback
            .as_ref()
            .ok_or_else(|| fail(OpenClass::Other, "playback not available"))?;
        // The route answers 401 here; the player shows "Sign in".
        let mut_token = engine.tokens.music_user_token();
        if mut_token.is_empty() {
            return Err(fail(
                OpenClass::Authentication,
                "not signed in: no music user token",
            ));
        }
        let token = tokio::select! {
            () = cancel.cancelled() => return Err(fail(OpenClass::Cancelled, "cancelled")),
            r = engine.resolve_token() => r.map_err(|e| fail(OpenClass::Transport, e.to_string()))?,
        };
        let (lossless, hi_res, mv_max_height) = match req.mode {
            OpenMode::Aac => (false, false, None),
            OpenMode::Lossless { hi_res } => (true, hi_res, None),
            OpenMode::Video { max_height } => (false, false, Some(max_height)),
        };
        let video = req.kind == ItemKind::MusicVideo || mv_max_height.is_some();
        // A session the prefetcher opened at the matching quality skips the webplayback round trip.
        // The prefetcher opens lossless sessions without hi-res, so a hi-res request opens its own.
        let pre_warmed = (!video && !hi_res)
            .then(|| p.scheduler.take_pre_warmed(req.id.as_str(), lossless))
            .flatten()
            .and_then(|id| p.manager.session(&id));
        let sess = match pre_warmed {
            Some(sess) => sess,
            None => {
                let open = SessionRequest {
                    asset_id: req.id.0,
                    storefront: engine.account_storefront().await,
                    token,
                    mut_token,
                    language: engine.tokens.language(),
                    lossless,
                    hi_res,
                    atmos: false,
                    video,
                    mv_max_height: mv_max_height.unwrap_or(0),
                    mv_audio_priorities: Vec::new(),
                };
                open_session(&engine, &cancel, open)
                    .await
                    .map_err(|f| fail(f.class.into(), f.message))?
            }
        };
        engine.emit(
            "playback.created",
            &json!({"sessionId": sess.id, "assetId": sess.asset_id, "codec": qualifier(&sess)}),
        );
        Ok(describe(&sess))
    }

    async fn precache(&self, session: &SessionId) {
        let Ok(engine) = self.engine() else { return };
        let Some(sess) = engine
            .playback
            .as_ref()
            .and_then(|p| p.manager.session(&session.0))
        else {
            return;
        };
        start_precache(&engine, session.0.clone(), &sess).await;
    }

    async fn touch(&self, session: &SessionId) -> Result<(), OpenError> {
        let engine = self.engine()?;
        engine
            .playback
            .as_ref()
            .and_then(|p| p.manager.session(&session.0))
            .map(drop)
            .ok_or_else(|| fail(OpenClass::NotFound, "session not found or expired"))
    }

    async fn release(&self, session: &SessionId) {
        let Ok(engine) = self.engine() else { return };
        if let Some(p) = &engine.playback {
            release_session(&engine, p, &session.0);
        }
    }
}
