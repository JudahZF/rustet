//! `GET /api/v1/tools`: whether the FFmpeg that exports would run is really usable.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::tool::{self, Stdout};

/// What the engine can truthfully say about the FFmpeg exports would run: that it was found and
/// that it runs, with its version and the encoders the app uses. A file called ffmpeg that is
/// missing a library, is the wrong architecture or hangs is reported unavailable, with the
/// reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FfmpegInfo {
    pub available: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// `aac`, `flac`, `libx264`: whether this build has them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoders: Option<BTreeMap<String, bool>>,
}

impl FfmpegInfo {
    fn error(msg: impl Into<String>) -> Self {
        Self {
            error: msg.into(),
            ..Self::default()
        }
    }
}

/// The encoders the app can ask FFmpeg for: re-encoding downloads to AAC or FLAC, and HEVC music
/// videos to H.264 (which only libx264 can do).
const PROBED_ENCODERS: [&str; 3] = ["aac", "flac", "libx264"];

const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Resolves FFmpeg the way exports do (the custom path if given and acceptable, else the one on
/// PATH) and runs it. `custom` is the user's "FFmpeg path" setting, validated like
/// `options.ffmpegPath`.
pub async fn probe_ffmpeg(custom: &str) -> FfmpegInfo {
    if !tool::valid_tool_path(custom, &["ffmpeg"]) {
        return FfmpegInfo::error("the path must name an ffmpeg binary");
    }
    if let Err(e) = tool::check_tool_executable(custom, &["ffmpeg"]) {
        return FfmpegInfo::error(if e.is_not_found() {
            "not found".to_owned()
        } else {
            e.to_string()
        });
    }
    let name = if custom.is_empty() { "ffmpeg" } else { custom };
    let Some(path) = tool::look_path(name) else {
        return FfmpegInfo::error("not found");
    };
    let mut info = FfmpegInfo {
        path: path.to_string_lossy().into_owned(),
        ..FfmpegInfo::default()
    };

    // Both runs share one deadline.
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let run = |flag| run_until(deadline, &path, flag);
    let out = match run("-version").await {
        Ok(out) => out,
        Err(e) => {
            info.error = format!("found, but it would not run: {e}");
            return info;
        }
    };
    info.version = ffmpeg_version(&String::from_utf8_lossy(&out));
    if info.version.is_empty() {
        info.error = "found, but it did not identify itself as ffmpeg".to_owned();
        return info;
    }
    info.available = true;
    if let Ok(out) = run("-encoders").await {
        info.encoders = Some(ffmpeg_encoders(&String::from_utf8_lossy(&out)));
    }
    info
}

/// Runs `ffmpeg -hide_banner <flag>` and returns its stdout. A timeout drops the run, which
/// kills the child.
async fn run_until(deadline: Instant, path: &Path, flag: &str) -> Result<Vec<u8>, String> {
    let args: [&OsStr; 2] = ["-hide_banner".as_ref(), flag.as_ref()];
    let never = CancellationToken::new();
    let run = tool::run_tool(&never, path, &args, Stdout::Capture);
    match timeout_at(deadline, run).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("timed out".to_owned()),
    }
}

/// Reads `ffmpeg version 9.0.2 Copyright ...` (also `n7.1-4-g...` style builds).
fn ffmpeg_version(out: &str) -> String {
    let line = out.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("ffmpeg"), Some("version"), Some(v)) => v.to_owned(),
        _ => String::new(),
    }
}

/// Reads `ffmpeg -encoders`: after a `------` line, each row is `<flags> <name> <description>`.
fn ffmpeg_encoders(out: &str) -> BTreeMap<String, bool> {
    let listed: Vec<&str> = out
        .lines()
        .skip_while(|l| !l.trim().starts_with("------"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    PROBED_ENCODERS
        .iter()
        .map(|e| ((*e).to_owned(), listed.contains(e)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ENCODERS: &str = "Encoders:
 V..... = Video
 A..... = Audio
 S..... = Subtitle
 .F.... = Frame-level multithreading
 ------
 A....D aac                  AAC (Advanced Audio Coding)
 A....D flac                 FLAC (Free Lossless Audio Codec)
 A....D pcm_s16le            PCM signed 16-bit little-endian
";

    #[test]
    fn version_line() {
        for (input, want) in [
            (
                "ffmpeg version 9.0.2 Copyright (c) 2000-2026 the FFmpeg developers\nbuilt with gcc",
                "9.0.2",
            ),
            (
                "ffmpeg version n7.1-4-gabc123 Copyright (c) 2000-2025",
                "n7.1-4-gabc123",
            ),
            ("not ffmpeg at all", ""),
            ("", ""),
        ] {
            assert_eq!(ffmpeg_version(input), want, "{input:?}");
        }
    }

    #[test]
    fn encoder_listing() {
        let got = ffmpeg_encoders(SAMPLE_ENCODERS);
        assert!(got["aac"] && got["flac"] && !got["libx264"], "{got:?}");
        let got = ffmpeg_encoders(&format!(
            "{SAMPLE_ENCODERS} V....D libx264              libx264 H.264 / AVC\n"
        ));
        assert!(got["libx264"]);
        // The legend above the separator is not a listing.
        let got = ffmpeg_encoders(" A..... = Audio\n V..... = Video\n");
        assert!(got.values().all(|have| !have), "{got:?}");
    }

    #[test]
    fn json_shape() {
        let info = FfmpegInfo {
            available: true,
            path: "/usr/bin/ffmpeg".into(),
            version: "9.0.2".into(),
            error: String::new(),
            encoders: Some(ffmpeg_encoders(SAMPLE_ENCODERS)),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            serde_json::json!({
                "available": true,
                "path": "/usr/bin/ffmpeg",
                "version": "9.0.2",
                "encoders": {"aac": true, "flac": true, "libx264": false}
            })
        );
        assert_eq!(
            serde_json::to_value(FfmpegInfo::error("not found")).unwrap(),
            serde_json::json!({"available": false, "error": "not found"})
        );
    }

    /// A script called ffmpeg in a private temp dir.
    #[cfg(unix)]
    fn fake_ffmpeg(body: &str) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ffmpeg");
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, p.to_string_lossy().into_owned())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_runs_the_binary() {
        let (_dir, p) = fake_ffmpeg(&format!(
            "case \"$2\" in\n-version) echo \"ffmpeg version 9.0.2 Copyright (c) 2000-2026 the FFmpeg developers\" ;;\n-encoders) cat <<'EOF'\n{SAMPLE_ENCODERS}EOF\n;;\nesac\n"
        ));
        let info = probe_ffmpeg(&p).await;
        assert!(
            info.available && info.version == "9.0.2" && info.error.is_empty(),
            "{info:?}"
        );
        let enc = info.encoders.unwrap();
        assert!(enc["aac"] && enc["flac"] && !enc["libx264"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_reports_a_broken_binary() {
        let (_dir, p) = fake_ffmpeg("echo 'error while loading shared libraries' >&2\nexit 127\n");
        let info = probe_ffmpeg(&p).await;
        assert!(
            !info.available && info.error.contains("would not run"),
            "{info:?}"
        );
        let (_dir, p) = fake_ffmpeg("echo 'some other tool'\n");
        let info = probe_ffmpeg(&p).await;
        assert!(
            !info.available && info.error.contains("did not identify itself"),
            "{info:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_refuses_what_exports_refuse() {
        assert_eq!(
            probe_ffmpeg("/nonexistent/dir/ffmpeg").await.error,
            "not found"
        );
        for p in [
            "/bin/ls",
            "/usr/bin/../bin/ffmpeg",
            "bin/ffmpeg",
            "/nonexistent/dir/ffmpeg",
        ] {
            let info = probe_ffmpeg(p).await;
            assert!(!info.available && !info.error.is_empty(), "{p}: {info:?}");
        }
    }
}
