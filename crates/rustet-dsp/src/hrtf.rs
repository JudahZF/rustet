//! HRTF impulse responses for the binaural renderer.
//!
//! This is the Linux equivalent of the SPAC-based HRTF engine in DTS Sound Unbound
//! (`DtsHrtfEnc.dll`):
//!
//! ```text
//! DtsHrtfEnc::InitializeConvInp/OutL/OutR → extraction + cache (this module)
//! DtsHrtfEnc::ProcessSingleBlock          → FFmpeg headphone filter
//! DtsHrtfEnc::MixChannelBed               → FFmpeg headphone accumulation
//! DTS SPAC.crypt FIR filter bank          → MIT KEMAR SOFA HRTF dataset
//! ```
//!
//! The MIT KEMAR normal-pinna SOFA file ships with libmysofa on most Linux distributions.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

/// The canonical MIT KEMAR SOFA location installed by libmysofa.
const SOFA_PATH: &str = "/usr/share/libmysofa/MIT_KEMAR_normal_pinna.sofa";

/// Bounds on the SOFA dimensions. The file may be user-supplied, and a crafted header must not
/// size a multi-gigabyte allocation.
const MAX_MEASUREMENTS: usize = 1 << 16;
const MAX_IR_LEN: usize = 1 << 16;

/// Reads a SOFA (HDF5) file with h5py and writes a compact blob to stdout:
///
/// ```text
/// [4]byte "SOFA"
/// i32 LE  M  (measurements)
/// i32 LE  N  (IR taps)
/// i32 LE  SR (sample rate, Hz)
/// [M*2*N]f64 LE  IR: measurement-major, ear-minor (L then R), tap-innermost
/// [M*2]f64 LE    SourcePosition: azimuth, elevation per measurement
/// ```
const SOFA_READER_PY: &str = r"
import sys, struct, h5py, numpy as np
path = sys.argv[1]
with h5py.File(path, 'r') as f:
    ir  = np.array(f['Data.IR'], dtype=np.float64)        # [M,2,N]
    pos = np.array(f['SourcePosition'], dtype=np.float64)  # [M,3]
    sr  = int(np.array(f['Data.SamplingRate']).flatten()[0])
M, _, N = ir.shape
sys.stdout.buffer.write(b'SOFA')
sys.stdout.buffer.write(struct.pack('<iii', M, N, sr))
sys.stdout.buffer.write(ir.tobytes())                 # M*2*N float64, C-order
sys.stdout.buffer.write(pos[:, :2].astype(np.float64).tobytes())  # M*2 float64
";

/// An error while preparing the HRTF set.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("hrtf: {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("hrtf: python3 sofa reader: {status} ({stderr})")]
    Reader {
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("hrtf: start python3 sofa reader: {0}")]
    StartReader(io::Error),
    #[error("hrtf: invalid sofa blob (len={0})")]
    InvalidBlob(usize),
    #[error("hrtf: implausible sofa dimensions (M={m} N={n} sr={sample_rate})")]
    ImplausibleDimensions { m: i32, n: i32, sample_rate: i32 },
    #[error("hrtf: blob too short (have {have}, need {need})")]
    BlobTooShort { have: usize, need: usize },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// A 7.1 bed speaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    FrontCentre,
    BackLeft,
    BackRight,
    SideLeft,
    SideRight,
    Lfe,
}

impl Speaker {
    /// All speakers, in the order of the FFmpeg headphone map.
    pub const ALL: [Self; 8] = [
        Self::FrontLeft,
        Self::FrontRight,
        Self::FrontCentre,
        Self::BackLeft,
        Self::BackRight,
        Self::SideLeft,
        Self::SideRight,
        Self::Lfe,
    ];

    /// The FFmpeg channel name, also the cached WAV's file stem.
    pub const fn name(self) -> &'static str {
        match self {
            Self::FrontLeft => "FL",
            Self::FrontRight => "FR",
            Self::FrontCentre => "FC",
            Self::BackLeft => "BL",
            Self::BackRight => "BR",
            Self::SideLeft => "SL",
            Self::SideRight => "SR",
            Self::Lfe => "LFE",
        }
    }

    /// ITU-R BS.775-3 position as SOFA (azimuth, elevation) in degrees: 0° is front, 90° is
    /// left, counter-clockwise.
    const fn position(self) -> (f64, f64) {
        match self {
            Self::FrontLeft => (30.0, 0.0),
            Self::FrontRight => (330.0, 0.0),
            Self::FrontCentre => (0.0, 0.0),
            Self::BackLeft => (110.0, 0.0),
            Self::BackRight => (250.0, 0.0),
            Self::SideLeft => (90.0, 0.0),
            Self::SideRight => (270.0, 0.0),
            // The sub is low-passed anyway; use the front HRTF.
            Self::Lfe => (0.0, 0.0),
        }
    }
}

/// HRTF impulse responses for the binaural renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HrtfSet {
    /// One stereo IR WAV per speaker, extracted from the MIT KEMAR SOFA dataset. Used with
    /// FFmpeg `hrir=stereo`. Indexed like [`Speaker::ALL`].
    Stereo {
        ir_paths: [PathBuf; 8],
        sample_rate: u32,
    },
    /// A user-provided 14-channel WAV in HeSuVi layout (FL FR FC BL BR SL SR, L+R each). Used
    /// with FFmpeg `hrir=multich`.
    Multich { wav: PathBuf, sample_rate: u32 },
}

impl HrtfSet {
    /// `"kemar"` for the SOFA set, `"custom"` for the user's WAV.
    pub const fn preset_name(&self) -> &'static str {
        match self {
            Self::Stereo { .. } => "kemar",
            Self::Multich { .. } => "custom",
        }
    }

    pub const fn sample_rate(&self) -> u32 {
        match self {
            Self::Stereo { sample_rate, .. } | Self::Multich { sample_rate, .. } => *sample_rate,
        }
    }
}

/// The HRTF set, detected once per process. In priority order:
///
/// 1. `hrtf.wav` in the config directory: a user-provided 14-channel HeSuVi WAV.
/// 2. The libmysofa MIT KEMAR SOFA file, extracted per speaker into `<cache dir>/hrtf`.
/// 3. `Ok(None)`: no HRTF data; the caller falls back to crossfeed.
pub async fn hrtf_set() -> Result<Option<&'static HrtfSet>, &'static Error> {
    static SET: OnceCell<Result<Option<HrtfSet>, Error>> = OnceCell::const_new();
    SET.get_or_init(detect).await.as_ref().map(Option::as_ref)
}

async fn detect() -> Result<Option<HrtfSet>, Error> {
    let custom = rustet_core::appdir::config_dir().join("hrtf.wav");
    if tokio::fs::metadata(&custom).await.is_ok() {
        match wav_sample_rate(&custom).await {
            Ok(sample_rate) => {
                return Ok(Some(HrtfSet::Multich {
                    wav: custom,
                    sample_rate,
                }));
            }
            Err(e) => tracing::warn!(path = %custom.display(), error = %e,
                "hrtf: custom WAV unreadable, falling back to SOFA"),
        }
    }

    let sofa = Path::new(SOFA_PATH);
    if let Err(e) = tokio::fs::metadata(sofa).await
        && e.kind() == io::ErrorKind::NotFound
    {
        return Ok(None); // No SOFA: silent crossfeed fallback.
    }
    let dir = rustet_core::appdir::cache_dir().join("hrtf");
    extract(sofa, &dir).await.map(Some)
}

/// The sample rate from bytes 24–27 of a WAV header.
async fn wav_sample_rate(path: &Path) -> io::Result<u32> {
    let mut header = [0u8; 28];
    tokio::fs::File::open(path)
        .await?
        .read_exact(&mut header)
        .await?;
    match u32::from_le_bytes(header[24..28].try_into().expect("4 bytes")) {
        0 => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sample rate 0 in WAV header",
        )),
        sr => Ok(sr),
    }
}

/// `stamp.json`: which SOFA file version the cached WAVs came from.
#[derive(Debug, Serialize, Deserialize)]
struct Stamp {
    sofa_path: String,
    mod_ns: i64,
    sample_rate: u32,
}

fn wav_paths(dir: &Path) -> [PathBuf; 8] {
    Speaker::ALL.map(|s| dir.join(format!("{}.wav", s.name())))
}

/// Extracts the 7.1 impulse responses from `sofa` into per-speaker WAVs in `dir`. A stamp file
/// skips the extraction while the SOFA file is unchanged.
async fn extract(sofa: &Path, dir: &Path) -> Result<HrtfSet, Error> {
    tokio::fs::create_dir_all(dir).await.map_err(io_err(dir))?;
    let modified = tokio::fs::metadata(sofa)
        .await
        .and_then(|m| m.modified())
        .map_err(io_err(sofa))?;
    // Unix nanoseconds, as Go's ModTime().UnixNano() wrote them.
    let mod_ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    };
    let sofa_path = sofa.to_string_lossy().into_owned();
    let ir_paths = wav_paths(dir);

    let stamp_path = dir.join("stamp.json");
    if let Ok(raw) = tokio::fs::read(&stamp_path).await
        && let Ok(stamp) = serde_json::from_slice::<Stamp>(&raw)
        && stamp.sofa_path == sofa_path
        && stamp.mod_ns == mod_ns
        && all_exist(&ir_paths).await
    {
        return Ok(HrtfSet::Stereo {
            ir_paths,
            sample_rate: stamp.sample_rate,
        });
    }

    let data = parse_sofa_blob(&read_sofa(sofa).await?)?;
    for (speaker, path) in Speaker::ALL.into_iter().zip(&ir_paths) {
        let (az, el) = speaker.position();
        let ir = &data.measurements[nearest_measurement(&data.measurements, az, el)];
        tokio::fs::write(path, stereo_wav(&ir.left, &ir.right, data.sample_rate))
            .await
            .map_err(io_err(path))?;
    }

    let stamp = Stamp {
        sofa_path,
        mod_ns,
        sample_rate: data.sample_rate,
    };
    let raw = serde_json::to_vec(&stamp).expect("stamp serializes");
    if let Err(e) = tokio::fs::write(&stamp_path, raw).await {
        tracing::debug!(error = %e, "hrtf: stamp write failed");
    }
    Ok(HrtfSet::Stereo {
        ir_paths,
        sample_rate: data.sample_rate,
    })
}

async fn all_exist(paths: &[PathBuf]) -> bool {
    for p in paths {
        if tokio::fs::metadata(p).await.is_err() {
            return false;
        }
    }
    true
}

/// Runs the h5py reader over `sofa` and returns its blob.
async fn read_sofa(sofa: &Path) -> Result<Vec<u8>, Error> {
    let out = tokio::process::Command::new("python3")
        .arg("-c")
        .arg(SOFA_READER_PY)
        .arg(sofa)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(Error::StartReader)?;
    if !out.status.success() {
        return Err(Error::Reader {
            status: out.status,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(out.stdout)
}

/// One measured direction: the impulse response of each ear and the source position.
#[derive(Debug, Clone, PartialEq)]
struct Measurement {
    left: Vec<f64>,
    right: Vec<f64>,
    azimuth: f64,
    elevation: f64,
}

#[derive(Debug)]
struct SofaData {
    measurements: Vec<Measurement>,
    sample_rate: u32,
}

/// Decodes the blob written by [`SOFA_READER_PY`].
fn parse_sofa_blob(raw: &[u8]) -> Result<SofaData, Error> {
    if raw.len() < 16 || &raw[..4] != b"SOFA" {
        return Err(Error::InvalidBlob(raw.len()));
    }
    let i32_at = |off: usize| i32::from_le_bytes(raw[off..off + 4].try_into().expect("4 bytes"));
    let (m, n, sr) = (i32_at(4), i32_at(8), i32_at(12));
    let dims = (
        usize::try_from(m)
            .ok()
            .filter(|&m| (1..=MAX_MEASUREMENTS).contains(&m)),
        usize::try_from(n)
            .ok()
            .filter(|&n| (1..=MAX_IR_LEN).contains(&n)),
        u32::try_from(sr).ok().filter(|&sr| sr > 0),
    );
    let (Some(count), Some(taps), Some(sample_rate)) = dims else {
        return Err(Error::ImplausibleDimensions {
            m,
            n,
            sample_rate: sr,
        });
    };

    let body = &raw[16..];
    let need = count * 2 * taps * 8 + count * 2 * 8;
    if body.len() < need {
        return Err(Error::BlobTooShort {
            have: raw.len(),
            need: 16 + need,
        });
    }
    let f64s = |bytes: &[u8]| -> Vec<f64> {
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|&c| f64::from_le_bytes(c))
            .collect()
    };
    let (irs, positions) = body[..need].split_at(count * 2 * taps * 8);
    let measurements = irs
        .chunks_exact(2 * taps * 8)
        .zip(positions.as_chunks::<16>().0)
        .map(|(ir, pos)| {
            let (left, right) = ir.split_at(taps * 8);
            let [azimuth, elevation] = f64s(pos)[..] else {
                unreachable!("16 bytes are two f64")
            };
            Measurement {
                left: f64s(left),
                right: f64s(right),
                azimuth,
                elevation,
            }
        })
        .collect();
    Ok(SofaData {
        measurements,
        sample_rate,
    })
}

/// The index of the measurement nearest to (`az`, `el`) by Euclidean distance in degrees. The
/// first wins a tie.
fn nearest_measurement(measurements: &[Measurement], az: f64, el: f64) -> usize {
    measurements
        .iter()
        .map(|m| (m.azimuth - az).hypot(m.elevation - el))
        .enumerate()
        .fold(
            (0, f64::MAX),
            |best, (i, d)| if d < best.1 { (i, d) } else { best },
        )
        .0
}

/// A two-channel IEEE float32 WAV for FFmpeg's headphone filter (`hrir=stereo`): the left and
/// right ear impulse responses, interleaved.
pub(crate) fn stereo_wav(left: &[f64], right: &[f64], sample_rate: u32) -> Vec<u8> {
    let data_len = u32::try_from(left.len() * 2 * 4).expect("IR length is bounded");
    let mut out = Vec::with_capacity(46 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    // RIFF size: everything after this field. The fmt chunk is 18 bytes, so the header past
    // here is 38 bytes.
    out.extend_from_slice(&(38 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&18u32.to_le_bytes()); // fmt chunk size, with the 2-byte extension
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2 * 4).to_le_bytes()); // byte rate
    out.extend_from_slice(&8u16.to_le_bytes()); // block align
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(&0u16.to_le_bytes()); // extension size
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for (&l, &r) in left.iter().zip(right) {
        out.extend_from_slice(&(l as f32).to_le_bytes());
        out.extend_from_slice(&(r as f32).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob_header(m: i32, n: i32, sr: i32) -> Vec<u8> {
        [
            b"SOFA".as_slice(),
            &m.to_le_bytes(),
            &n.to_le_bytes(),
            &sr.to_le_bytes(),
        ]
        .concat()
    }

    #[test]
    fn parse_sofa_blob_rejects_hostile_dimensions() {
        for (name, header) in [
            ("negative M", blob_header(-1, 4, 48000)),
            ("negative N", blob_header(4, -1, 48000)),
            ("zero M", blob_header(0, 4, 48000)),
            ("huge M and N", blob_header(1 << 30, 1 << 30, 48000)),
            ("overflowing M*N", blob_header(i32::MAX, i32::MAX, 48000)),
            ("bad rate", blob_header(2, 2, 0)),
            ("short body", blob_header(2, 2, 48000)),
            ("bad magic", b"SOFX\0\0\0\0\0\0\0\0\0\0\0\0".to_vec()),
        ] {
            assert!(
                parse_sofa_blob(&header).is_err(),
                "{name}: expected an error"
            );
        }
    }

    #[test]
    fn parse_sofa_blob_reads_layout() {
        let (m, n) = (2usize, 3usize);
        let mut blob = blob_header(m as i32, n as i32, 48000);
        // IRs: measurement-major, then ear, then tap. Value = 100*m + 10*ear + tap.
        for mi in 0..m {
            for ear in 0..2 {
                for tap in 0..n {
                    blob.extend_from_slice(&((100 * mi + 10 * ear + tap) as f64).to_le_bytes());
                }
            }
        }
        for (az, el) in [(30.0f64, 0.0f64), (330.0, 10.0)] {
            blob.extend_from_slice(&az.to_le_bytes());
            blob.extend_from_slice(&el.to_le_bytes());
        }
        let d = parse_sofa_blob(&blob).unwrap();
        assert_eq!(d.sample_rate, 48000);
        assert_eq!(d.measurements[1].left, [100.0, 101.0, 102.0]);
        assert_eq!(d.measurements[1].right, [110.0, 111.0, 112.0]);
        assert_eq!(
            (d.measurements[1].azimuth, d.measurements[1].elevation),
            (330.0, 10.0)
        );
        assert_eq!(nearest_measurement(&d.measurements, 330.0, 0.0), 1);
        assert_eq!(nearest_measurement(&d.measurements, 0.0, 0.0), 0);
    }

    #[test]
    fn stereo_wav_header() {
        let wav = stereo_wav(&[0.5, -0.25], &[1.0, 0.0], 44100);
        let u32_at = |o: usize| u32::from_le_bytes(wav[o..o + 4].try_into().unwrap());
        assert_eq!(wav.len(), 46 + 16);
        assert_eq!(u32_at(4) as usize, wav.len() - 8);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(24), 44100);
        assert_eq!(&wav[38..42], b"data");
        assert_eq!(u32_at(42), 16);
        assert_eq!(f32::from_le_bytes(wav[46..50].try_into().unwrap()), 0.5);
        assert_eq!(f32::from_le_bytes(wav[50..54].try_into().unwrap()), 1.0);
    }

    #[tokio::test]
    async fn wav_sample_rate_reads_header_and_rejects_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        std::fs::write(&path, stereo_wav(&[0.0], &[0.0], 48000)).unwrap();
        assert_eq!(wav_sample_rate(&path).await.unwrap(), 48000);
        std::fs::write(&path, stereo_wav(&[0.0], &[0.0], 0)).unwrap();
        assert!(wav_sample_rate(&path).await.is_err());
        std::fs::write(&path, b"RIFF").unwrap();
        assert!(wav_sample_rate(&path).await.is_err());
    }
}
