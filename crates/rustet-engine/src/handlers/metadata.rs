//! Track metadata, artwork and lyrics.

use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::IntoResponse;
use regex::Regex;
use rustet_ampapi::lyrics::{LyricsFormat, LyricsRequest};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::handlers::catalog::{CATALOG_ID, PROXY_TIMEOUT, catalog_params};
use crate::http::{ApiError, ApiResult, json, lang, query_param, scan_int};
use crate::state::AppState;

/// One available stream quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamInfo {
    pub codec: &'static str,
    #[serde(skip_serializing_if = "is_zero")]
    pub sample_rate: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub bit_depth: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub bitrate: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl StreamInfo {
    const fn codec(codec: &'static str) -> Self {
        Self {
            codec,
            sample_rate: 0,
            bit_depth: 0,
            bitrate: 0,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Meta<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    title: &'a str,
    artist_name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    album_name: &'a str,
    duration_ms: u64,
    artwork_url: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    has_lyrics: bool,
    #[serde(rename = "has4k", skip_serializing_if = "std::ops::Not::not")]
    has_4k: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    has_hdr: bool,
    available_streams: Option<Vec<StreamInfo>>,
}

/// Track info and the available qualities. The ID is tried as a song, a music video, then an
/// album.
pub(crate) async fn metadata(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let p = catalog_params(&s, &uri, id)?;
    let (cancel, lang, tok) = (
        CancellationToken::new(),
        lang(&headers),
        s.tokens.developer(),
    );
    let id = p.id.as_str();

    if let Ok(song) = s.amp.song(&cancel, &p.storefront, id, &lang, &tok).await
        && let Some(song) = song.data.first()
    {
        let a = &song.attributes;
        return Ok(json(
            StatusCode::OK,
            &Meta {
                id,
                kind: "song",
                title: &a.name,
                artist_name: &a.artist_name,
                album_name: &a.album_name,
                duration_ms: a.duration_in_millis,
                artwork_url: fmt_artwork_url(&a.artwork.url, 500),
                has_lyrics: a.has_lyrics,
                has_4k: false,
                has_hdr: false,
                available_streams: Some(streams_from_traits(&a.audio_traits)),
            },
        ));
    }
    if let Ok(mv) = s
        .amp
        .music_video(&cancel, &p.storefront, id, &lang, &tok)
        .await
        && let Some(mv) = mv.data.first()
    {
        let a = &mv.attributes;
        return Ok(json(
            StatusCode::OK,
            &Meta {
                id,
                kind: "mv",
                title: &a.name,
                artist_name: &a.artist_name,
                album_name: &a.album_name,
                duration_ms: a.duration_in_millis,
                artwork_url: fmt_artwork_url(&a.artwork.url, 500),
                has_lyrics: false,
                has_4k: a.has_4k,
                has_hdr: a.has_hdr,
                available_streams: Some(vec![StreamInfo::codec("H.264"), StreamInfo::codec("AAC")]),
            },
        ));
    }
    if let Ok(album) = s.amp.album(&cancel, &p.storefront, id, &lang, &tok).await
        && let Some(album) = album.data.first()
    {
        let a = &album.attributes;
        return Ok(json(
            StatusCode::OK,
            &Meta {
                id,
                kind: "album",
                title: &a.name,
                artist_name: &a.artist_name,
                album_name: "",
                duration_ms: 0,
                artwork_url: fmt_artwork_url(&a.artwork.url, 500),
                has_lyrics: false,
                has_4k: false,
                has_hdr: false,
                available_streams: None,
            },
        ));
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "asset not found"))
}

/// The artwork image of a song or music video at `size` pixels (50–3000, default 500).
pub(crate) async fn artwork(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let p = catalog_params(&s, &uri, id)?;
    let size = scan_int(&query_param(&uri, "size"))
        .unwrap_or(500)
        .clamp(50, 3000);
    let (cancel, lang, tok) = (
        CancellationToken::new(),
        lang(&headers),
        s.tokens.developer(),
    );

    let template = match s.amp.song(&cancel, &p.storefront, &p.id, &lang, &tok).await {
        Ok(song) if !song.data.is_empty() => song.data[0].attributes.artwork.url.clone(),
        _ => match s
            .amp
            .music_video(&cancel, &p.storefront, &p.id, &lang, &tok)
            .await
        {
            Ok(mv) if !mv.data.is_empty() => mv.data[0].attributes.artwork.url.clone(),
            _ => return Err(ApiError::new(StatusCode::NOT_FOUND, "not found")),
        },
    };
    let img = s
        .http
        .get(fmt_artwork_url(&template, size))
        .timeout(PROXY_TIMEOUT)
        .send()
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "artwork fetch failed"))?;
    // The status is always 200, as in the Go engine; the content type is the CDN's.
    let content_type = img.headers().get(header::CONTENT_TYPE).cloned();
    let mut resp = Body::from_stream(img.bytes_stream()).into_response();
    let h = resp.headers_mut();
    if let Some(ct) = content_type {
        h.insert(header::CONTENT_TYPE, ct);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    Ok(resp)
}

/// The lyric relationships the renderer asks for (`lyrics`, `syllable-lyrics`): plain single
/// path segments only.
static LYRIC_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z-]{1,32}$").expect("valid regex"));

/// Lyrics in `format` (`lrc` by default, `ttml`, `vtt`, `srt`).
pub(crate) async fn lyrics(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let mut storefront = query_param(&uri, "sf");
    if storefront.is_empty() {
        storefront = s.tokens.playback_storefront();
    }
    let format = query_param(&uri, "format");
    let mut kind = query_param(&uri, "type");
    if kind.is_empty() {
        kind = "lyrics".to_owned();
    }
    // These become path segments of a request that carries the user's media-user-token.
    if !CATALOG_ID.is_match(&id) || !CATALOG_ID.is_match(&storefront) || !LYRIC_TYPE.is_match(&kind)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid id, storefront or type",
        ));
    }
    let (lang, token, mut_token) = (
        lang(&headers),
        s.tokens.developer(),
        s.tokens.media_user_token(),
    );
    let format = LyricsFormat::from(format.as_str());
    let req = LyricsRequest {
        storefront: &storefront,
        song_id: &id,
        kind: &kind,
        language: &lang,
        format,
        token: &token,
        media_user_token: &mut_token,
    };
    let text = s
        .amp
        .lyrics(&CancellationToken::new(), &req)
        .await
        .map_err(|e| ApiError::new(StatusCode::NOT_FOUND, e))?;
    let content_type = match format {
        LyricsFormat::Ttml => "application/xml; charset=utf-8",
        LyricsFormat::Vtt => "text/vtt; charset=utf-8",
        LyricsFormat::Srt | LyricsFormat::Lrc => "text/plain; charset=utf-8",
    };
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        text,
    )
        .into_response())
}

/// Fills an Apple artwork URL template (`{w}x{h}`) with a square size.
pub(crate) fn fmt_artwork_url(template: &str, size: i64) -> String {
    let size = size.to_string();
    template.replace("{w}", &size).replace("{h}", &size)
}

/// The stream qualities an `audioTraits` list advertises, AAC always last.
pub(crate) fn streams_from_traits(traits: &[String]) -> Vec<StreamInfo> {
    let has = |t: &str| traits.iter().any(|x| x == t);
    let mut out = Vec::new();
    if has("hi-res-lossless") {
        // The Go engine looked for a "<…>-<rate>-<depth>" trait; a part that is not a number
        // keeps the previous value, so Apple's real traits leave the defaults.
        let (mut rate, mut depth) = (96_000, 24);
        for t in traits {
            let parts: Vec<&str> = t.split('-').collect();
            if parts.len() >= 2 {
                rate = scan_int(parts[parts.len() - 2]).unwrap_or(rate);
                depth = scan_int(parts[parts.len() - 1]).unwrap_or(depth);
                if rate > 0 && depth > 0 {
                    break;
                }
            }
        }
        out.push(StreamInfo {
            sample_rate: rate,
            bit_depth: depth,
            ..StreamInfo::codec("ALAC")
        });
    } else if has("lossless") {
        out.push(StreamInfo {
            sample_rate: 44_100,
            bit_depth: 16,
            ..StreamInfo::codec("ALAC")
        });
    }
    if has("atmos") {
        out.push(StreamInfo::codec("E-AC-3"));
    }
    out.push(StreamInfo {
        bitrate: 256_000,
        ..StreamInfo::codec("AAC")
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traits(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn artwork_url_substitutes_the_size() {
        assert_eq!(
            fmt_artwork_url("https://cdn.example.com/{w}x{h}bb.jpg", 300),
            "https://cdn.example.com/300x300bb.jpg"
        );
        assert_eq!(fmt_artwork_url("{w}/{h}/img.jpg", 0), "0/0/img.jpg");
    }

    #[test]
    fn streams_from_traits_cases() {
        assert_eq!(
            streams_from_traits(&[]),
            [StreamInfo {
                bitrate: 256_000,
                ..StreamInfo::codec("AAC")
            }]
        );
        let codecs = |t: &[&str]| {
            streams_from_traits(&traits(t))
                .iter()
                .map(|s| s.codec)
                .collect::<Vec<_>>()
        };
        assert_eq!(codecs(&["lossless"]), ["ALAC", "AAC"]);
        assert_eq!(codecs(&["atmos"]), ["E-AC-3", "AAC"]);
        let hi = streams_from_traits(&traits(&[
            "atmos",
            "hi-res-lossless",
            "lossless",
            "lossy-stereo",
        ]));
        assert_eq!(
            hi[0],
            StreamInfo {
                sample_rate: 96_000,
                bit_depth: 24,
                ..StreamInfo::codec("ALAC")
            }
        );
        assert_eq!(hi.len(), 3);
        let lossless = &streams_from_traits(&traits(&["lossless"]))[0];
        assert_eq!((lossless.sample_rate, lossless.bit_depth), (44_100, 16));
    }

    #[test]
    fn stream_info_omits_zero_fields() {
        assert_eq!(
            serde_json::to_string(&StreamInfo::codec("E-AC-3")).unwrap(),
            r#"{"codec":"E-AC-3"}"#
        );
    }
}
