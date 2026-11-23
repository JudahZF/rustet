//! `/api/v1/player/*`: the player's commands, state and history (spec §4.8). Each command body
//! is one [`Command`]; an accepted command answers 202 once the player executed it.

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use rustet_player::PlayerHandle;
use rustet_player::model::{Command, CommandError, VideoMode};
use serde::de::DeserializeOwned;

use crate::http::{ApiError, ApiResult, json, read_json};
use crate::state::AppState;

/// A queue of a few thousand tracks with their metadata.
const BODY_LIMIT: usize = 4 << 20;

fn player(s: &AppState) -> Result<&PlayerHandle, ApiError> {
    s.player
        .get()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "player not available"))
}

impl From<CommandError> for ApiError {
    fn from(e: CommandError) -> Self {
        let status = match e {
            CommandError::StaleGeneration { .. } => StatusCode::CONFLICT,
            CommandError::UnknownUid(_) => StatusCode::NOT_FOUND,
            CommandError::BadIndex { .. }
            | CommandError::BadVolume(_)
            | CommandError::BadHeight(_)
            | CommandError::Unsupported(_) => StatusCode::BAD_REQUEST,
            CommandError::Closed => StatusCode::SERVICE_UNAVAILABLE,
        };
        ApiError::new(status, e)
    }
}

/// Runs a command without a body (`play`, `pause`, `next`, …).
pub(crate) async fn bare(s: AppState, command: Command) -> ApiResult {
    player(&s)?.send(command).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// Runs the command `make` builds from the JSON body.
pub(crate) async fn with_body<T: DeserializeOwned>(
    s: AppState,
    body: Body,
    make: fn(T) -> Command,
) -> ApiResult {
    let player = player(&s)?;
    let body = read_json::<T>(body, BODY_LIMIT).await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {e}"),
        )
    })?;
    player.send(make(body)).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// `POST /api/v1/player/video` with `{"audioOnly": bool}`: music videos as audio only, or with
/// their video again. 202 once the player switched (a playing video reloads at its position).
pub(crate) async fn video(State(s): State<AppState>, body: Body) -> ApiResult {
    let player = player(&s)?;
    let mode: VideoMode = read_json(body, BODY_LIMIT).await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {e}"),
        )
    })?;
    player.set_mv_audio_only(mode.audio_only).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// `GET /api/v1/player`: the player state.
pub(crate) async fn state(State(s): State<AppState>) -> ApiResult {
    Ok(json(StatusCode::OK, &player(&s)?.state()))
}

/// `GET /api/v1/player/history`: played tracks, newest first.
pub(crate) async fn history(State(s): State<AppState>) -> ApiResult {
    Ok(json(StatusCode::OK, &player(&s)?.history()))
}

/// `DELETE /api/v1/player/history`.
pub(crate) async fn clear_history(State(s): State<AppState>) -> ApiResult {
    player(&s)?.send(Command::ClearHistory).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
