//! FFmpeg and VLC invocations: music-video mux, subtitle embedding, ALAC → FLAC conversion.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::tagger::TrackMeta;
use crate::tool::{self, Stdout, ToolError};

/// The FFmpeg binary for a request's `ffmpegPath` (empty: `ffmpeg` on PATH).
pub(crate) fn resolve_ffmpeg(custom: &str) -> Result<PathBuf, ToolError> {
    let name = if custom.is_empty() { "ffmpeg" } else { custom };
    tool::look_path(name).ok_or(ToolError::FfmpegNotFound)
}

/// The first VLC found: the explicit path, then `cvlc` (headless), then `vlc`.
pub(crate) fn find_vlc(explicit: &str) -> Option<PathBuf> {
    [explicit, "cvlc", "vlc"]
        .into_iter()
        .filter(|c| !c.is_empty())
        .find_map(tool::look_path)
}

/// Replaces line breaks and NULs in tag values: arguments are passed as argv (no shell), but
/// multi-line or NUL-truncated tags are malformed in most players.
fn sanitize_meta(v: &str) -> String {
    v.replace(['\n', '\r', '\0'], " ")
}

/// The `-metadata key=value` arguments for a FLAC file. Empty values are left out.
fn metadata_args(meta: &TrackMeta) -> Vec<OsString> {
    let numbered = |n: u32, total: u32| match (n, total) {
        (0, _) => String::new(),
        (n, 0) => n.to_string(),
        (n, t) => format!("{n}/{t}"),
    };
    let tags = [
        ("title", sanitize_meta(&meta.title)),
        ("artist", sanitize_meta(&meta.artist_name)),
        ("album_artist", sanitize_meta(&meta.album_artist)),
        ("album", sanitize_meta(&meta.album_name)),
        ("track", numbered(meta.track_number, meta.track_total)),
        ("disc", numbered(meta.disc_number, meta.disc_total)),
        ("date", sanitize_meta(&meta.release_date)),
        ("genre", sanitize_meta(&meta.genre)),
        ("composer", sanitize_meta(&meta.composer)),
        ("copyright", sanitize_meta(&meta.copyright)),
        ("publisher", sanitize_meta(&meta.record_label)),
        ("isrc", sanitize_meta(&meta.isrc)),
        ("barcode", sanitize_meta(&meta.upc)),
    ];
    tags.into_iter()
        .filter(|(_, v)| !v.is_empty())
        .flat_map(|(k, v)| ["-metadata".into(), format!("{k}={v}").into()])
        .collect()
}

async fn ffmpeg(
    cancel: &CancellationToken,
    ffmpeg: &Path,
    args: &[OsString],
) -> Result<(), ToolError> {
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    tool::run_tool(cancel, ffmpeg, &args, Stdout::Discard)
        .await
        .map(drop)
}

fn os(args: &[&str]) -> impl Iterator<Item = OsString> {
    args.iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
        .into_iter()
}

/// Encodes `src` (raw FLAC from VLC) to FLAC with metadata tags, and embeds the cover image at
/// `art` when given. Re-encoding (not `-c copy`) makes FFmpeg write a correct
/// `STREAMINFO.total_samples`, which VLC's streaming output leaves at 0 (Duration: N/A).
pub(crate) async fn tag_flac(
    cancel: &CancellationToken,
    ffmpeg_path: &str,
    src: &Path,
    art: Option<&Path>,
    dst: &Path,
    meta: &TrackMeta,
) -> Result<(), ToolError> {
    let bin = resolve_ffmpeg(ffmpeg_path)?;
    let mut args: Vec<OsString> = vec!["-i".into(), src.into()];
    match art {
        Some(art) => {
            args.extend([OsString::from("-i"), art.into()]);
            args.extend(os(&[
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "flac",
                "-compression_level",
                "8",
                "-c:v",
                "copy",
                "-disposition:v",
                "attached_pic",
            ]));
        }
        None => args.extend(os(&[
            "-map",
            "0:a",
            "-c:a",
            "flac",
            "-compression_level",
            "8",
        ])),
    }
    args.extend(os(&["-map_metadata", "-1"]));
    args.extend(metadata_args(meta));
    args.extend(os(&["-f", "flac", "-y"]));
    args.push(dst.into());
    ffmpeg(cancel, &bin, &args).await
}

/// Transcodes `src` (ALAC `.m4a`) to FLAC with metadata tags. With `art`, that image is the
/// cover (a separate input is more reliable than copying the `covr` atom across formats);
/// without, any artwork the tagger already embedded in `src` is copied. The caller removes `dst`
/// on failure.
pub(crate) async fn convert_to_flac(
    cancel: &CancellationToken,
    ffmpeg_path: &str,
    src: &Path,
    art: Option<&Path>,
    dst: &Path,
    meta: &TrackMeta,
) -> Result<(), ToolError> {
    let bin = resolve_ffmpeg(ffmpeg_path)?;
    let mut args: Vec<OsString> = vec!["-i".into(), src.into()];
    match art {
        Some(art) => {
            args.extend([OsString::from("-i"), art.into()]);
            args.extend(os(&["-map", "0:a", "-map", "1:v"]));
        }
        // The covr atom reads as a video stream; '?' keeps a source without artwork working.
        None => args.extend(os(&["-map", "0:a", "-map", "0:v?"])),
    }
    args.extend(os(&[
        "-c:a",
        "flac",
        "-c:v",
        "copy",
        "-disposition:v",
        "attached_pic",
        "-compression_level",
        "8",
        // Start clean; the -metadata flags set what is needed.
        "-map_metadata",
        "-1",
    ]));
    args.extend(metadata_args(meta));
    args.push("-y".into());
    args.push(dst.into());
    ffmpeg(cancel, &bin, &args).await
}

/// Combines a video-only and an audio-only fMP4 into one MP4 with `-c copy`. Music-video
/// sessions deliver the two separately.
pub(crate) async fn mux_video_audio(
    cancel: &CancellationToken,
    ffmpeg_path: &str,
    video: &Path,
    audio: &Path,
    out: &Path,
) -> Result<(), ToolError> {
    let bin = resolve_ffmpeg(ffmpeg_path)?;
    let mut args: Vec<OsString> = vec!["-i".into(), video.into(), "-i".into(), audio.into()];
    args.extend(os(&[
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:v",
        "copy",
        "-c:a",
        "copy",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
        "-y",
    ]));
    args.push(out.into());
    ffmpeg(cancel, &bin, &args).await
}

/// Adds an SRT subtitle track to an MP4 in place: a second FFmpeg pass whose output replaces
/// `mp4` on success.
pub(crate) async fn add_subtitle_track(
    cancel: &CancellationToken,
    ffmpeg_path: &str,
    mp4: &Path,
    srt: &Path,
) -> Result<(), ToolError> {
    let bin = resolve_ffmpeg(ffmpeg_path)?;
    let tmp = with_suffix(mp4, ".sub.mp4");
    let mut args: Vec<OsString> = vec!["-i".into(), mp4.into(), "-i".into(), srt.into()];
    args.extend(os(&[
        "-map",
        "0:v",
        "-map",
        "0:a",
        "-map",
        "1:s",
        "-c:v",
        "copy",
        "-c:a",
        "copy",
        "-c:s",
        "mov_text",
        "-metadata:s:s:0",
        "language=eng",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
        "-y",
    ]));
    args.push(tmp.clone().into());
    let res = match ffmpeg(cancel, &bin, &args).await {
        Ok(()) => tokio::fs::rename(&tmp, mp4).await.map_err(ToolError::Io),
        Err(e) => Err(e),
    };
    if res.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    res
}

/// Decodes a lossless ALAC `.m4a` to raw FLAC with VLC. VLC's MP4 demuxer handles Apple's
/// identical-tfdt fMP4 without dropping fragments, unlike FFmpeg's mov demuxer. The output has
/// correct audio but `STREAMINFO.total_samples = 0`; [`tag_flac`] re-encodes it to fix that.
pub(crate) async fn vlc_to_flac(
    cancel: &CancellationToken,
    vlc: &Path,
    src: &Path,
    dst: &Path,
) -> Result<(), ToolError> {
    let sout = format!(
        "#transcode{{acodec=flac,channels=2}}:std{{access=file,mux=raw,dst={}}}",
        escape_sout(&dst.to_string_lossy())
    );
    let args: [&OsStr; 8] = [
        "--intf".as_ref(),
        "dummy".as_ref(),
        "--no-video".as_ref(),
        "--sout".as_ref(),
        sout.as_ref(),
        "--play-and-exit".as_ref(),
        "-q".as_ref(),
        src.as_os_str(),
    ];
    tool::run_tool(cancel, vlc, &args, Stdout::Discard)
        .await
        .map(drop)
}

/// Escapes VLC sout separators in a path: `}` closes the option block, `:` and `#` separate
/// modules.
fn escape_sout(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        if matches!(c, '}' | ':' | '#') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `p` with `suffix` appended to its file name.
pub(crate) fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_args_skip_empty_and_sanitize() {
        let meta = TrackMeta {
            title: "Line\nbreak".into(),
            track_number: 3,
            track_total: 12,
            disc_number: 1,
            record_label: "Label".into(),
            ..TrackMeta::default()
        };
        let args: Vec<String> = metadata_args(&meta)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-metadata",
                "title=Line break",
                "-metadata",
                "track=3/12",
                "-metadata",
                "disc=1",
                "-metadata",
                "publisher=Label",
            ]
        );
    }

    #[test]
    fn sout_escaping() {
        assert_eq!(escape_sout("/a:b}c#d"), r"/a\:b\}c\#d");
    }
}
