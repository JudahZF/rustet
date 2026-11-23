//! Binaural rendering through an FFmpeg child process. This is the Linux equivalent of the
//! DTS Sound Unbound HRTF renderer and the Dolby DAA headphone virtualizer:
//!
//! ```text
//! Windows DTS Sound Unbound           │ This implementation
//! ────────────────────────────────────┼─────────────────────────────
//! DTSXDecoder.dll (DTS:X JOC decode)  │ FFmpeg EC-3 / DTS decoder
//! ISpatialAudioMetadataClient         │ FFmpeg filter graph
//! DtsHrtfEnc::ProcessSingleBlock      │ FFmpeg headphone HRTF filter
//! DtsHrtfEnc::MixChannelBed           │ headphone accumulation
//! SPAC.crypt FIR filter bank          │ MIT KEMAR SOFA HRTF dataset
//! ```
//!
//! Surround input (EC-3 7.1) is rendered with HRTF convolution when an [`HrtfSet`] is
//! available, otherwise with an ITU-R BS.775-3 7.1 → stereo pan followed by Bauer
//! stereophonic-to-binaural (bs2b) crossfeed. Stereo input (ALAC) only gets bs2b crossfeed:
//! the headphone filter needs multichannel input.
//!
//! The saved AutoEQ settings are appended to every filter graph. Output is fragmented MP4 with
//! AAC LC at 256 kbps, suitable for MSE.

use std::ffi::OsString;
use std::io;
use std::process::{ExitStatus, Stdio};

use async_trait::async_trait;
use rustet_core::pipeline::{BoxError, PipelineError, Sink, Stage, StreamContext};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::autoeq;
use crate::hrtf::{HrtfSet, Speaker};

/// The fallback filter for multichannel input: ITU-R BS.775 downmix, bs2b crossfeed, 48 kHz.
const CROSSFEED_FILTER: &str = "pan=stereo|\
    FL<FL+0.707*FC+0.707*BL+0.707*SL+0.1*LFE|\
    FR<FR+0.707*FC+0.707*BR+0.707*SR+0.1*LFE,\
    bs2b=profile=default,\
    aresample=48000";

/// The filter for stereo input: bs2b crossfeed only.
const STEREO_FILTER: &str = "bs2b=profile=default,aresample=48000";

/// The output arguments shared by every render mode.
const OUTPUT_ARGS: [&str; 9] = [
    "-acodec",
    "aac",
    "-b:a",
    "256k",
    "-movflags",
    "frag_keyframe+empty_moov+default_base_moof",
    "-f",
    "mp4",
    "pipe:1",
];

/// An error of the binaural stage.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("binaural: ffmpeg not found in PATH")]
    FfmpegNotFound,
    #[error("binaural: start ffmpeg: {0}")]
    Start(io::Error),
    #[error("binaural: ffmpeg: {status}{}", stderr_suffix(.stderr))]
    Ffmpeg { status: ExitStatus, stderr: String },
    #[error("binaural: ffmpeg pipe: {0}")]
    Pipe(io::Error),
    #[error("binaural: inner source: {0}")]
    Input(io::Error),
}

fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(" ({stderr})")
    }
}

impl From<Error> for PipelineError {
    fn from(e: Error) -> Self {
        Self::Other(BoxError::from(e))
    }
}

/// The channel layout of the stage's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Multichannel (EC-3 7.1): HRTF convolution, or pan + crossfeed without HRTF data.
    Surround,
    /// Stereo (ALAC, PCM): bs2b crossfeed only.
    Stereo,
}

/// Transcodes its input to binaural stereo AAC in fragmented MP4 through FFmpeg.
///
/// The FFmpeg child is killed when the stream is cancelled or the stage returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinauralStage {
    pub input: Input,
}

#[async_trait]
impl Stage for BinauralStage {
    async fn process(
        &self,
        ctx: &StreamContext,
        r: &mut (dyn AsyncRead + Send + Unpin),
        w: &mut dyn Sink,
    ) -> Result<(), PipelineError> {
        let args = self.ffmpeg_args().await;
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Err(PipelineError::Cancelled),
            res = run_ffmpeg("ffmpeg".as_ref(), &args, r, w) => res.map_err(Into::into),
        }
    }
}

impl BinauralStage {
    async fn ffmpeg_args(&self) -> Vec<OsString> {
        // A missing or broken eq.json means no EQ, not a failed stream.
        let eq = autoeq::Settings::load()
            .await
            .map(|s| s.ffmpeg_chain())
            .unwrap_or_default();
        let hrtf = match self.input {
            Input::Stereo => None,
            Input::Surround => crate::hrtf::hrtf_set().await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "binaural: HRTF extraction failed, using crossfeed");
                None
            }),
        };
        let filter = match self.input {
            Input::Stereo => STEREO_FILTER,
            Input::Surround => CROSSFEED_FILTER,
        };
        build_args(hrtf, filter, &eq)
    }
}

/// The FFmpeg arguments for one render: HRTF convolution when `hrtf` is set, otherwise the
/// crossfeed `filter`. `eq` is an AutoEQ chain or `""`.
fn build_args(hrtf: Option<&HrtfSet>, filter: &str, eq: &str) -> Vec<OsString> {
    let with_eq = |base: &str| {
        if eq.is_empty() {
            base.to_owned()
        } else {
            format!("{base},{eq}")
        }
    };
    let mut args: Vec<OsString> = ["-v", "error", "-i", "pipe:0"].map(Into::into).to_vec();
    match hrtf {
        // One 14-channel HeSuVi IR.
        Some(HrtfSet::Multich { wav, .. }) => {
            args.extend(["-i".into(), wav.into()]);
            args.extend([
                "-filter_complex".into(),
                format!(
                    "[0:a][1:a]headphone=map='FL|FC|FR|BL|BR|SL|SR':hrir=multich,loudnorm,{}",
                    with_eq("aresample=48000")
                )
                .into(),
            ]);
        }
        // Inputs 1–8 are the per-speaker IRs. The headphone filter convolves each input
        // channel with its IR and sums to binaural stereo; loudnorm (EBU R128) matches the
        // DTS output level.
        Some(HrtfSet::Stereo { ir_paths, .. }) => {
            for path in ir_paths {
                args.extend(["-i".into(), path.into()]);
            }
            let inputs: String = (0..=ir_paths.len()).map(|i| format!("[{i}:a]")).collect();
            let map = Speaker::ALL.map(Speaker::name).join("|");
            args.extend([
                "-filter_complex".into(),
                format!(
                    "{inputs}headphone=map='{map}':hrir=stereo,loudnorm,{}",
                    with_eq("aresample=48000")
                )
                .into(),
            ]);
        }
        None => args.extend(["-af".into(), with_eq(filter).into()]),
    }
    args.extend(OUTPUT_ARGS.map(Into::into));
    args
}

/// Pipes `r` through `program` into `w`. Stderr is collected for the error message, so the
/// child cannot block on it. The child is killed if this future is dropped.
async fn run_ffmpeg(
    program: &std::ffi::OsStr,
    args: &[OsString],
    r: &mut (dyn AsyncRead + Send + Unpin),
    w: &mut dyn Sink,
) -> Result<(), Error> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Error::FfmpegNotFound,
            _ => Error::Start(e),
        })?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let feed = async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = r.read(&mut buf).await.map_err(Error::Input)?;
            if n == 0 {
                break;
            }
            // FFmpeg closed its stdin: it exited or stopped reading. Its status tells why.
            if stdin.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        // Dropping stdin is FFmpeg's end of input.
        Ok(())
    };
    let drain = async move {
        // Dropping stdout on a write error makes FFmpeg fail with a broken pipe, not block.
        tokio::io::copy(&mut stdout, w).await?;
        w.flush().await
    };
    let collect = async move {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text).await;
        String::from_utf8_lossy(&text).trim().to_owned()
    };
    let (fed, drained, stderr) = tokio::join!(feed, drain, collect);
    let status = child.wait().await.map_err(Error::Pipe)?;

    drained.map_err(Error::Pipe)?;
    if !status.success() {
        return Err(Error::Ffmpeg { status, stderr });
    }
    fed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn strs(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|a| a.to_str().unwrap()).collect()
    }

    #[test]
    fn crossfeed_args_append_eq() {
        let args = build_args(None, STEREO_FILTER, "volume=-1.00dB");
        assert_eq!(
            strs(&args)[..6],
            [
                "-v",
                "error",
                "-i",
                "pipe:0",
                "-af",
                "bs2b=profile=default,aresample=48000,volume=-1.00dB"
            ]
        );
        assert_eq!(strs(&args)[6..], OUTPUT_ARGS);
        assert_eq!(
            strs(&build_args(None, CROSSFEED_FILTER, ""))[5],
            "pan=stereo|FL<FL+0.707*FC+0.707*BL+0.707*SL+0.1*LFE|\
             FR<FR+0.707*FC+0.707*BR+0.707*SR+0.1*LFE,bs2b=profile=default,aresample=48000"
        );
    }

    #[test]
    fn hrtf_stereo_args() {
        let set = HrtfSet::Stereo {
            ir_paths: Speaker::ALL.map(|s| PathBuf::from(format!("/c/{}.wav", s.name()))),
            sample_rate: 44100,
        };
        let args = build_args(Some(&set), CROSSFEED_FILTER, "");
        let args = strs(&args);
        assert_eq!(args[4..8], ["-i", "/c/FL.wav", "-i", "/c/FR.wav"]);
        assert_eq!(args[18..20], ["-i", "/c/LFE.wav"]);
        assert_eq!(
            args[21],
            "[0:a][1:a][2:a][3:a][4:a][5:a][6:a][7:a][8:a]\
             headphone=map='FL|FR|FC|BL|BR|SL|SR|LFE':hrir=stereo,loudnorm,aresample=48000"
        );
    }

    #[test]
    fn hrtf_multich_args() {
        let set = HrtfSet::Multich {
            wav: "/c/hrtf.wav".into(),
            sample_rate: 48000,
        };
        let args = build_args(Some(&set), CROSSFEED_FILTER, "equalizer=f=1");
        assert_eq!(
            strs(&args)[4..8],
            [
                "-i",
                "/c/hrtf.wav",
                "-filter_complex",
                "[0:a][1:a]headphone=map='FL|FC|FR|BL|BR|SL|SR':hrir=multich,loudnorm,\
              aresample=48000,equalizer=f=1"
            ]
        );
    }

    /// The child process plumbing, with `cat` standing in for FFmpeg.
    #[tokio::test]
    async fn run_pipes_through_the_child() {
        let payload: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        let mut out = Vec::new();
        run_ffmpeg("cat".as_ref(), &[], &mut payload.as_slice(), &mut out)
            .await
            .unwrap();
        assert_eq!(out, payload);
    }

    #[tokio::test]
    async fn run_reports_exit_status_and_stderr() {
        let args: Vec<OsString> = ["-c", "echo bad filter >&2; exit 3"]
            .map(Into::into)
            .to_vec();
        let err = run_ffmpeg("sh".as_ref(), &args, &mut b"x".as_slice(), &mut Vec::new())
            .await
            .unwrap_err();
        match err {
            Error::Ffmpeg { status, stderr } => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr, "bad filter");
            }
            other => panic!("got {other:?}"),
        }
        let err = run_ffmpeg(
            "/nonexistent/ffmpeg".as_ref(),
            &[],
            &mut b"".as_slice(),
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::FfmpegNotFound), "{err:?}");
    }

    /// Real FFmpeg with the filters this module needs (bs2b is not in the bundled minimal
    /// build or in nixpkgs#ffmpeg). Run with `nix shell nixpkgs#ffmpeg-full -c cargo test
    /// -p rustet-dsp -- --ignored`.
    #[tokio::test]
    #[ignore = "needs an ffmpeg with bs2b, pan and the aac encoder on PATH"]
    async fn ffmpeg_renders_stereo_crossfeed_to_fmp4() {
        // One second of 48 kHz stereo silence as a WAV.
        let mut wav = crate::cavern::streaming_wav_header().to_vec();
        wav.extend(std::iter::repeat_n(0u8, 48000 * 8));
        let args = build_args(
            None,
            STEREO_FILTER,
            "volume=-1.00dB,equalizer=f=1000:t=q:w=1.0000:g=2.00",
        );
        let mut out = Vec::new();
        run_ffmpeg("ffmpeg".as_ref(), &args, &mut wav.as_slice(), &mut out)
            .await
            .unwrap();
        assert_eq!(&out[4..8], b"ftyp", "fragmented MP4 output");
    }

    /// The HRTF graph (eight per-speaker IR inputs, headphone filter, loudnorm) against real
    /// FFmpeg. Same requirements as above.
    #[tokio::test]
    #[ignore = "needs an ffmpeg with the headphone and loudnorm filters on PATH"]
    async fn ffmpeg_renders_hrtf_stereo_to_fmp4() {
        let dir = tempfile::tempdir().unwrap();
        let ir_paths = Speaker::ALL.map(|s| dir.path().join(format!("{}.wav", s.name())));
        let impulse: Vec<f64> = std::iter::once(1.0).chain([0.0; 63]).collect();
        for p in &ir_paths {
            std::fs::write(p, crate::hrtf::stereo_wav(&impulse, &impulse, 48000)).unwrap();
        }
        let set = HrtfSet::Stereo {
            ir_paths,
            sample_rate: 48000,
        };
        // One second of a 440 Hz 7.1 float tone (loudnorm turns pure silence into NaN): the
        // stereo header with 8 channels.
        let mut wav = crate::cavern::streaming_wav_header().to_vec();
        wav[22..24].copy_from_slice(&8u16.to_le_bytes());
        wav[28..32].copy_from_slice(&(48000u32 * 32).to_le_bytes());
        wav[32..34].copy_from_slice(&32u16.to_le_bytes());
        for i in 0..48000 {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.1;
            wav.extend(std::iter::repeat_n(v.to_le_bytes(), 8).flatten());
        }
        let args = build_args(Some(&set), CROSSFEED_FILTER, "");
        let mut out = Vec::new();
        run_ffmpeg("ffmpeg".as_ref(), &args, &mut wav.as_slice(), &mut out)
            .await
            .unwrap();
        assert_eq!(&out[4..8], b"ftyp", "fragmented MP4 output");
    }
}
