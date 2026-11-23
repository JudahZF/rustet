//! `GET /api/v1/audioanalysis/{id}`: cross-fade timing from Apple Music's `audio-analysis`
//! relationship. All times are milliseconds from the start of the track.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::handlers::catalog::{BROWSER_USER_AGENT, PROXY_TIMEOUT, catalog_params};
use crate::http::{ApiError, ApiResult, query_param};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Fade {
    start_ms: i64,
    end_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
struct Loudness {
    #[serde(skip_serializing_if = "is_zero")]
    beginning: f64,
    #[serde(skip_serializing_if = "is_zero")]
    main: f64,
    #[serde(skip_serializing_if = "is_zero")]
    ending: f64,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// The response. Fields Apple did not return are left out.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Analysis {
    asset_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fade_in: Option<Fade>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fade_out: Option<Fade>,
    #[serde(skip_serializing_if = "is_zero")]
    bpm: f64,
    #[serde(skip_serializing_if = "String::is_empty")]
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    loudness: Option<Loudness>,
}

// Apple's songs response with the audio-analysis relationship; only what Rustet uses.
#[derive(Deserialize)]
struct SongsResp {
    #[serde(default)]
    data: Vec<SongData>,
}

#[derive(Deserialize)]
struct SongData {
    #[serde(default)]
    relationships: Relationships,
}

#[derive(Deserialize, Default)]
struct Relationships {
    #[serde(rename = "audio-analysis")]
    audio_analysis: Option<AnalysisList>,
}

#[derive(Deserialize)]
struct AnalysisList {
    #[serde(default)]
    data: Vec<AnalysisData>,
}

#[derive(Deserialize)]
struct AnalysisData {
    #[serde(default)]
    attributes: AnalysisAttributes,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct AnalysisAttributes {
    fade_in: Option<AppleFade>,
    fade_out: Option<AppleFade>,
    bpm: f64,
    key: String,
    loudness: Option<Loudness>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
struct AppleFade {
    #[serde(default)]
    start_in_milliseconds: i64,
    #[serde(default)]
    end_in_milliseconds: i64,
}

impl From<AppleFade> for Fade {
    fn from(f: AppleFade) -> Self {
        Self {
            start_ms: f.start_in_milliseconds,
            end_ms: f.end_in_milliseconds,
        }
    }
}

/// The analysis in Apple's songs response; `None` when Apple has none for the track.
fn extract(id: &str, resp: SongsResp) -> Option<Analysis> {
    let a = resp
        .data
        .into_iter()
        .next()?
        .relationships
        .audio_analysis?
        .data
        .into_iter()
        .next()?
        .attributes;
    Some(Analysis {
        asset_id: id.to_owned(),
        fade_in: a.fade_in.map(Fade::from),
        fade_out: a.fade_out.map(Fade::from),
        bpm: a.bpm,
        key: a.key,
        loudness: a.loudness,
    })
}

/// Query: `sf`, and `token` (an optional bearer JWT; the engine's token otherwise). A 204 when
/// Apple has no analysis for the track: not an error.
pub(crate) async fn audio_analysis(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
) -> ApiResult {
    let p = catalog_params(&s, &uri, id)?;
    let q = query_param(&uri, "token");
    let mut token = q.strip_prefix("Bearer ").unwrap_or(&q).to_owned();
    if token.is_empty() {
        token = s.tokens.developer();
    }
    if token.is_empty() {
        token =
            s.amp.token().await.map_err(|e| {
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("token: {e}"))
            })?;
    }

    let resp = s
        .http
        .get(format!(
            "{}/v1/catalog/{}/songs/{}",
            s.amp_base, p.storefront, p.id
        ))
        .timeout(PROXY_TIMEOUT)
        .query(&[
            ("extend", "extendedAssetUrls"),
            ("include", "audio-analysis"),
        ])
        .bearer_auth(&token)
        .header(header::ORIGIN, "https://music.apple.com")
        .header(header::USER_AGENT, BROWSER_USER_AGENT)
        .send()
        .await;
    let bad_gateway = |msg: String| ApiError::new(StatusCode::BAD_GATEWAY, msg);
    let resp = resp.map_err(|e| bad_gateway(format!("audio-analysis fetch: {e}")))?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(bad_gateway(format!(
            "audio-analysis: HTTP {}",
            resp.status().as_u16()
        )));
    }
    let songs: SongsResp = resp
        .json()
        .await
        .map_err(|e| bad_gateway(format!("audio-analysis decode: {e}")))?;
    let Some(analysis) = extract(&p.id, songs) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let mut body = serde_json::to_vec(&analysis).unwrap_or_default();
    body.push(b'\n');
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // The analysis of a track does not change.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    Ok((headers, body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_omits_missing_fields() {
        let resp: SongsResp = serde_json::from_str(
            r#"{"data":[{"relationships":{"audio-analysis":{"data":[{"attributes":{
                "fadeIn":{"startInMilliseconds":0,"endInMilliseconds":1200},
                "bpm":120.5,"loudness":{"main":-8.5}}}]}}}]}"#,
        )
        .unwrap();
        let a = extract("42", resp).unwrap();
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::json!({"assetId":"42","fadeIn":{"startMs":0,"endMs":1200},"bpm":120.5,"loudness":{"main":-8.5}})
        );
        let none: SongsResp = serde_json::from_str(r#"{"data":[{"relationships":{}}]}"#).unwrap();
        assert!(extract("42", none).is_none());
        assert!(extract("42", serde_json::from_str(r#"{"data":[]}"#).unwrap()).is_none());
    }
}
