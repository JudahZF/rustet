//! Known answers recorded from the Go engine (`engine/utils/aacstream`, mp4ff
//! `itouakirai/mp4ff v0.0.0-20250930132656-98812935a1c7`) for the same inputs. The renderer feeds these bytes to a
//! MediaSource, so the output must match byte for byte.
//!
//! Fixtures (`tests/data`):
//! - `cenc_video.in.mp4`, `cbcs_audio.in.mp4`: cut from the mp4ff test data (see the rustet-mp4 fixtures README).
//! - `aac_clear.mp4`: built with mp4ff in the shape of Apple's clear AAC: ftyp + moov + pssh, six fragments of 43
//!   samples without a tfdt, track ID 2, durations from the tfhd default (1024), a styp before fragment 4.

use rustet_core::pipeline::StreamContext;
use rustet_streams::{MvLiveIndex, decrypt_mp4_streaming, passthrough_streaming};
use sha2::{Digest, Sha256};

const CENC_VIDEO: &[u8] = include_bytes!("data/cenc_video.in.mp4");
const CBCS_AUDIO: &[u8] = include_bytes!("data/cbcs_audio.in.mp4");
const AAC_CLEAR: &[u8] = include_bytes!("data/aac_clear.mp4");

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

async fn decrypt(input: &[u8], key: &str) -> Vec<u8> {
    let mut out = Vec::new();
    decrypt_mp4_streaming(
        &StreamContext::default(),
        &mut &input[..],
        &hex(key),
        &mut out,
    )
    .await
    .unwrap();
    out
}

async fn passthrough(input: &[u8], ctx: &StreamContext) -> Vec<u8> {
    let mut out = Vec::new();
    passthrough_streaming(ctx, &mut &input[..], &mut out)
        .await
        .unwrap();
    out
}

#[tokio::test]
async fn cenc_video_decrypts_like_go() {
    let out = decrypt(CENC_VIDEO, "63cb5f7184dd4b689a5c5ff11ee6a328").await;
    assert_eq!(
        sha(&out),
        "49b2fc5c4e45035097d11ad51fc2fad988325146a47d27f4bec177a872f2740b"
    );
}

/// Audio path: the fragment IDs are normalised and the tfdt forced to version 1 before decryption.
#[tokio::test]
async fn cbcs_audio_decrypts_like_go() {
    let out = decrypt(CBCS_AUDIO, "5ffd93861fa776e96cccd934898fc1c8").await;
    assert_eq!(
        sha(&out),
        "82afe7d8e48ccaaa5b0034185476e55b39f9a372dde63fdb64b88d8a3a8d9332"
    );
}

/// A clear stream through the decryptor: the pssh is dropped and fragments without senc pass through.
#[tokio::test]
async fn clear_stream_through_the_decryptor_matches_go() {
    let out = decrypt(AAC_CLEAR, "00000000000000000000000000000000").await;
    assert_eq!(
        sha(&out),
        "4aec49c6e5c904f010d2219c482b672990cac53f29377e476f08e24a371dcfc0"
    );
}

/// The tfdt rewrite: missing tfdts are inserted (version 1) on a continuous timeline from 0, track IDs become 1.
#[tokio::test]
async fn passthrough_rewrites_tfdts_like_go() {
    let out = passthrough(AAC_CLEAR, &StreamContext::default()).await;
    assert_eq!(
        sha(&out),
        "891ec36e214ce39a60a427db4e8db84c31694a2c4a79e000c7ced7284642fc36"
    );
    // Existing tfdts and several truns per traf.
    let out = passthrough(CBCS_AUDIO, &StreamContext::default()).await;
    assert_eq!(
        sha(&out),
        "0d58db98f6670957c307890656901c7ce47c14f497cd184dc9b1db2026145a46"
    );
}

/// The seek path: the timeline starts at the actual start, and fragments that end before the target are dropped.
#[tokio::test]
async fn passthrough_seek_matches_go() {
    let ctx = StreamContext {
        seek_actual_start: Some(10.0),
        seek_target: Some(12.5),
        ..StreamContext::default()
    };
    let out = passthrough(AAC_CLEAR, &ctx).await;
    assert_eq!(out.len(), 3148);
    assert_eq!(
        sha(&out),
        "32936781702c1453f5030778b5da1764bafacd568772a5edac6c8b2834cf9847"
    );
}

#[tokio::test]
async fn truncated_input_is_an_error_not_a_clean_end() {
    let mut out = Vec::new();
    let res =
        passthrough_streaming(&StreamContext::default(), &mut &AAC_CLEAR[..100], &mut out).await;
    assert!(res.is_err(), "an init segment cut short must fail");

    let ctx = StreamContext::default();
    ctx.cancel.cancel();
    let res = passthrough_streaming(&ctx, &mut &AAC_CLEAR[..], &mut Vec::new()).await;
    assert!(matches!(res, Err(rustet_streams::Error::Cancelled)));
}

struct Indexed {
    init_size: i64,
    timescale: u64,
    t: Vec<f64>,
    off: Vec<i64>,
    end: Vec<i64>,
    limit: Vec<i64>,
    parsed: i64,
}

fn index(input: &[u8], chunk: usize) -> Indexed {
    let ix = MvLiveIndex::new();
    input.chunks(chunk).for_each(|c| ix.feed(c));
    let n = ix.frag_count();
    Indexed {
        init_size: ix.init_size().unwrap_or(0),
        timescale: ix.timescale(),
        t: ix.all_frag_timings().iter().map(|f| f.t).collect(),
        off: (0..n).map(|i| ix.frag_off_by_index(i).unwrap()).collect(),
        end: (0..n)
            .map(|i| ix.frag_end_by_index(i).unwrap_or(0))
            .collect(),
        limit: (0..n)
            .map(|i| ix.frag_limit_by_index(i).unwrap_or(0))
            .collect(),
        parsed: ix.parsed_bytes(),
    }
}

/// A multi-track (video + audio) input: times come from the video track.
#[test]
fn index_of_multi_track_input_matches_go() {
    let ix = index(CENC_VIDEO, 7);
    assert_eq!(
        (ix.init_size, ix.timescale, ix.parsed),
        (1416, 90000, 196_002)
    );
    assert_eq!(ix.t, [0.0, 5.0]);
    assert_eq!(ix.off, [1416, 117_451]);
    assert_eq!(ix.end, [117_451, 0]);
    assert_eq!(ix.limit, [117_451, 196_002]);

    let ix = index(CBCS_AUDIO, 1000);
    assert_eq!(
        (ix.init_size, ix.timescale, ix.parsed),
        (1338, 48000, 175_963)
    );
    assert_eq!(ix.t, [0.0]);
    assert_eq!(ix.off, [1338]);
    assert_eq!(ix.end, [0]);
    assert_eq!(ix.limit, [175_963]);
}
