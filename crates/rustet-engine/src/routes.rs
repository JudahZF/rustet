//! The route table: every route of the Go engine except `/debug/pprof`.

use std::path::PathBuf;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::{delete, get, post, put};
use rustet_ampapi::RecommendationKind;
use tower_http::services::ServeDir;

use axum::body::Body;
use rustet_player::model::Command;

use crate::handlers::{
    audioanalysis, auth, cache, catalog, drm, export, library, metadata, mvcache, playback, player,
    recommendations, system, vlc, vseg,
};
use crate::http::text_error;
use crate::state::AppState;

/// The API router, guard included.
pub(crate) fn router(state: AppState) -> Router {
    let feed = |kind: RecommendationKind| {
        get(move |s: State<AppState>, uri: Uri, headers: HeaderMap| {
            recommendations::feed(kind, s, uri, headers)
        })
    };
    // Player commands: one `Command` per route, with or without a JSON body.
    let bare = |c: Command| post(move |State(s): State<AppState>| player::bare(s, c.clone()));
    macro_rules! with_body {
        ($make:path) => {
            post(|State(s): State<AppState>, b: Body| player::with_body(s, b, $make))
        };
    }
    let mut api = Router::new()
        .route("/api/v1/status", get(system::status))
        .route("/api/v1/tools", get(system::tools))
        .route("/api/v1/capabilities", get(system::capabilities))
        .route("/api/v1/events", get(system::events))
        .route("/api/v1/metrics", get(system::metrics))
        .route("/api/v1/debug/runtime", get(system::runtime_stats))
        // Playback sessions and their streams.
        .route("/api/v1/playback", post(playback::create))
        .route("/api/v1/playback/{id}/audio", get(playback::audio))
        .route("/api/v1/playback/{id}/vseg/init", get(vseg::init))
        .route("/api/v1/playback/{id}/vseg/seg/{n}", get(vseg::seg))
        .route("/api/v1/playback/{id}/vseg/manifest", get(vseg::manifest))
        .route("/api/v1/playback/{id}/vseg/seek", get(vseg::seek))
        .route("/api/v1/playback/{id}/vseg", delete(vseg::stop))
        .route("/api/v1/playback/{id}/precache", post(playback::precache))
        .route("/api/v1/playback/{id}/touch", post(playback::touch))
        .route("/api/v1/playback/{id}", delete(playback::delete))
        // The renderer signals what the user looks at; the scheduler decides what to warm.
        .route("/api/v1/playback/context", put(playback::context))
        // Cache limits, usage and clearing; cache-warming jobs.
        .route("/api/v1/cache/config", put(cache::config))
        .route("/api/v1/cache/stats", get(cache::stats))
        .route("/api/v1/cache/playback", delete(cache::clear_playback))
        .route(
            "/api/v1/cache/mv",
            get(mvcache::get).put(mvcache::put).delete(mvcache::clear),
        )
        .route(
            "/api/v1/jobs/{id}",
            get(cache::job_status).delete(cache::job_cancel),
        )
        // Metadata.
        .route("/api/v1/metadata/{id}", get(metadata::metadata))
        .route("/api/v1/artwork/{id}", get(metadata::artwork))
        .route("/api/v1/lyrics/{id}", get(metadata::lyrics))
        .route(
            "/api/v1/audioanalysis/{id}",
            get(audioanalysis::audio_analysis),
        )
        // Exports.
        .route("/api/v1/export", post(export::create).get(export::list))
        .route(
            "/api/v1/export/{id}",
            get(export::get).delete(export::cancel),
        )
        .route("/api/v1/export/{id}/retry", post(export::retry))
        .route("/api/v1/export/{id}/priority", post(export::priority))
        // DRM: the renderer expresses intent (sign in, submit a code); the engine orchestrates.
        .route("/api/v1/drm/status", get(drm::status))
        .route("/api/v1/drm/authenticate", post(drm::authenticate))
        .route("/api/v1/drm/challenge", post(drm::challenge))
        .route("/api/v1/drm/logout", post(drm::logout))
        .route("/api/v1/drm/session", delete(drm::logout))
        .route("/api/v1/drm/forget-device", post(drm::forget_device))
        // The local library metadata cache.
        .route("/api/v1/library/token", post(library::token))
        .route("/api/v1/library/sync", post(library::sync))
        .route("/api/v1/library/ingest", post(library::ingest))
        .route("/api/v1/library/status", get(library::status))
        .route("/api/v1/library/playlists", get(library::playlists))
        .route(
            "/api/v1/library/playlists/{id}/tracks",
            get(library::playlist_tracks),
        )
        .route(
            "/api/v1/library/albums/{id}/tracks",
            get(library::album_tracks),
        )
        // Catalog entities.
        .route("/api/v1/catalog/albums/{id}", get(catalog::album))
        .route("/api/v1/catalog/playlists/{id}", get(catalog::playlist))
        .route("/api/v1/catalog/artists/{id}", get(catalog::artist))
        // Personalised feeds (dormant: the bundled renderer does not use them).
        .route(
            "/api/v1/recommendations",
            feed(RecommendationKind::Recommendations),
        )
        .route(
            "/api/v1/recommendations/heavy-rotation",
            feed(RecommendationKind::HeavyRotation),
        )
        .route(
            "/api/v1/recommendations/recently-played",
            feed(RecommendationKind::RecentlyPlayed),
        )
        // The player (spec §4.8): queue, transport, modes, settings, history.
        .route("/api/v1/player", get(player::state))
        .route("/api/v1/player/queue", with_body!(Command::SetQueue))
        .route("/api/v1/player/queue/insert", with_body!(Command::Insert))
        .route("/api/v1/player/queue/remove", with_body!(Command::Remove))
        .route("/api/v1/player/queue/move", with_body!(Command::Move))
        .route("/api/v1/player/queue/jump", with_body!(Command::Jump))
        .route("/api/v1/player/queue/clear", bare(Command::ClearUpcoming))
        .route("/api/v1/player/play", bare(Command::Play))
        .route("/api/v1/player/pause", bare(Command::Pause))
        .route("/api/v1/player/toggle", bare(Command::Toggle))
        .route("/api/v1/player/next", bare(Command::Next))
        .route("/api/v1/player/previous", bare(Command::Previous))
        .route("/api/v1/player/stop", bare(Command::Stop))
        .route("/api/v1/player/seek", with_body!(Command::Seek))
        .route("/api/v1/player/volume", with_body!(Command::Volume))
        .route("/api/v1/player/mode", with_body!(Command::Mode))
        .route("/api/v1/player/settings", with_body!(Command::Settings))
        .route("/api/v1/player/hover", with_body!(Command::Hover))
        .route("/api/v1/player/video", post(player::video))
        .route(
            "/api/v1/player/history",
            get(player::history).delete(player::clear_history),
        )
        // Which tokens the engine holds; web sign-out.
        .route("/api/v1/auth", get(auth::get))
        .route("/api/v1/auth/token", delete(auth::delete_token))
        .route("/api/v1/auth/session", get(auth::session))
        // The libvlc player of the web shell. While the native UI's player plays, these stay
        // usable: whoever starts playback last owns the sound (`Engine::player_takes_libvlc`).
        .route("/api/v1/vlc/load", post(vlc::load))
        .route("/api/v1/vlc/pause", post(vlc::pause))
        .route("/api/v1/vlc/resume", post(vlc::resume))
        .route("/api/v1/vlc/stop", post(vlc::stop))
        .route("/api/v1/vlc/time", get(vlc::time))
        .route("/api/v1/vlc/seek", post(vlc::seek))
        .route("/api/v1/vlc/volume", post(vlc::volume))
        .fallback(|| async { text_error(StatusCode::NOT_FOUND, "404 page not found") });

    // Bundled fonts next to the executable, so the page can load SF Pro through @font-face
    // without depending on system fonts.
    if let Some(dir) = fonts_dir() {
        api = api.nest_service("/fonts", ServeDir::new(dir));
    }

    let guard = state.guard.clone();
    api.with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            guard,
            crate::guard::guard,
        ))
}

fn fonts_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("fonts"))
}
