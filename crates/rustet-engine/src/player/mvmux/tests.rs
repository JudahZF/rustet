use std::sync::Mutex;

use rustet_mp4::{MediaType, Moov, RawBox, TrunSample};

use super::*;

const V_TS: u64 = 90_000;
const A_TS: u64 = 44_100;

/// An fMP4 stream of one track: `count` fragments of `frag_s` seconds, two samples each, whose
/// decode times start at `first_s` plus a raw offset of `offset_s`. Sample bytes are
/// `[tag, fragment index, sample index]`.
fn stream(
    media: MediaType,
    ts: u64,
    frag_s: f64,
    first_s: f64,
    count: u64,
    offset_s: f64,
    tag: u8,
) -> Vec<u8> {
    let mut init = InitSegment::single_track(ts as u32, media).unwrap();
    if media == MediaType::Video {
        // A clear avc1 entry: 1280×720 at bytes 24 and 26 of its body.
        let mut body = vec![0; 78];
        body[24..26].copy_from_slice(&1280u16.to_be_bytes());
        body[26..28].copy_from_slice(&720u16.to_be_bytes());
        let trak = init.moov_mut().unwrap().traks_mut().next().unwrap();
        trak.stsd_mut().unwrap().entries.push(Mp4Box::Raw(RawBox {
            kind: FourCC(*b"avc1"),
            payload: body,
        }));
    }
    let mut out = init.to_bytes().unwrap();
    let half = (frag_s * ts as f64 / 2.0) as u64;
    let first = (first_s / frag_s) as u8;
    for i in 0..count {
        let mut f = Fragment::new(i as u32 + 1, 1);
        let start = ((first_s + offset_s) * ts as f64) as u64 + i * 2 * half;
        for k in 0..2u8 {
            // The second video sample of each fragment is not a sync sample.
            let flags = if media == MediaType::Video && k == 1 {
                NON_SYNC
            } else {
                0
            };
            let sample = TrunSample {
                duration: half as u32,
                size: 3,
                flags,
                composition_time_offset: 0,
            };
            f.add_full_sample(
                sample,
                start + u64::from(k) * half,
                &[tag, first + i as u8, k],
            );
        }
        out.extend(f.to_bytes().unwrap());
    }
    out
}

/// One output fragment: per track, its start (s) and its samples' bytes.
type Parsed = Vec<(u32, f64, Vec<[u8; 3]>)>;

fn parse(out: &[u8]) -> (Moov, Vec<Parsed>) {
    let (mut at, mut moov, mut frags, mut frag) = (0, None, Vec::new(), Fragment::default());
    while at < out.len() {
        let (b, used) = Mp4Box::decode(&out[at..], at as u64).unwrap();
        at += used;
        match b {
            Mp4Box::Moov(m) => moov = Some(m),
            Mp4Box::Moof(_) => frag.push(b),
            Mp4Box::Mdat(_) => {
                frag.push(b);
                let mut tracks = Vec::new();
                for (id, ts) in [(VIDEO_ID, V_TS), (AUDIO_ID, A_TS)] {
                    let samples = frag.full_samples(Some(&Trex::new(id))).unwrap();
                    if let Some(first) = samples.first() {
                        let data = &frag.mdat().unwrap().data;
                        let bytes = samples
                            .iter()
                            .map(|s| data[s.data.clone()].try_into().unwrap())
                            .collect();
                        tracks.push((id, first.decode_time as f64 / ts as f64, bytes));
                    }
                }
                frags.push(tracks);
                frag = Fragment::default();
            }
            _ => {}
        }
    }
    (moov.unwrap(), frags)
}

async fn run(
    video: Vec<u8>,
    audio: Vec<u8>,
    plan: MuxPlan,
) -> (Vec<u8>, Option<(u32, u32)>, MuxIndex) {
    let (tx, rx) = watch::channel(None);
    let index = Mutex::new(MuxIndex::default());
    let mut out = Vec::new();
    mux(
        video.as_slice(),
        audio.as_slice(),
        plan,
        &mut out,
        &tx,
        &index,
    )
    .await
    .unwrap();
    let progress = *rx.borrow();
    (out, progress, index.into_inner().unwrap())
}

/// Per fragment, per track: (track, start s, sample count).
fn layout(frags: &[Parsed]) -> Vec<Vec<(u32, f64, usize)>> {
    frags
        .iter()
        .map(|f| f.iter().map(|(id, t, s)| (*id, *t, s.len())).collect())
        .collect()
}

// The two tracks of one music video become one fMP4 that libvlc demuxes: both traks in one
// moov with their own IDs, the length set; each video fragment carries the audio that starts
// before it ends, in its own moof and mdat; the audio after the video comes last.
#[tokio::test]
async fn joins_both_tracks_fragment_by_fragment() {
    // Video: 2 s fragments from 0 (raw times 10 s ahead). Audio: 3 s fragments of two 1.5 s samples.
    let video = stream(MediaType::Video, V_TS, 2.0, 0.0, 5, 10.0, b'v');
    let audio = stream(MediaType::Audio, A_TS, 3.0, 0.0, 4, 0.0, b'a');
    let plan = MuxPlan {
        video_start_s: 0.0,
        audio_start_s: 0.0,
        duration_ms: 10_000,
    };
    let (out, progress, index) = run(video, audio, plan).await;
    let (moov, frags) = parse(&out);

    let ids: Vec<u32> = moov.traks().filter_map(Trak::track_id).collect();
    assert_eq!(ids, [VIDEO_ID, AUDIO_ID]);
    assert_eq!(
        moov.trexs().map(|t| t.track_id).collect::<Vec<_>>(),
        [VIDEO_ID, AUDIO_ID]
    );
    let mvhd = moov.mvhd().unwrap();
    assert_eq!(
        mvhd.duration(),
        10_000 * u64::from(mvhd.timescale()) / 1000,
        "libvlc needs the length"
    );
    let mvhd_bytes = Mp4Box::Mvhd(mvhd.clone()).to_bytes().unwrap();
    assert_eq!(
        &mvhd_bytes[mvhd_bytes.len() - 4..],
        &3u32.to_be_bytes(),
        "next track ID"
    );
    let audio_trak = moov.traks().nth(1).unwrap();
    assert_eq!(audio_trak.handler_type(), Some(FourCC::SOUN));
    assert_eq!(audio_trak.mdhd().unwrap().duration(), 10 * A_TS);

    // The video's raw timeline (10 s ahead) is moved to 0. Audio samples start at 0, 1.5, 3, …
    let (v, a) = (VIDEO_ID, AUDIO_ID);
    assert_eq!(
        layout(&frags),
        [
            vec![(v, 0.0, 2), (a, 0.0, 2)], // audio 0 and 1.5 start before 2
            vec![(v, 2.0, 2), (a, 3.0, 1)],
            vec![(v, 4.0, 2), (a, 4.5, 1)],
            vec![(v, 6.0, 2), (a, 6.0, 2)],
            vec![(v, 8.0, 2), (a, 9.0, 1)],
            vec![(a, 10.5, 1)], // after the video
        ]
    );
    // Every sample's bytes, in order, per track.
    let bytes = |id| {
        frags
            .iter()
            .flatten()
            .filter(|t| t.0 == id)
            .flat_map(|t| t.2.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        bytes(v),
        (0..5)
            .flat_map(|i| (0..2).map(move |k| [b'v', i, k]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        bytes(a),
        (0..4)
            .flat_map(|i| (0..2).map(move |k| [b'a', i, k]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        progress,
        Some((1280, 720)),
        "the visible size from the sample entry"
    );
    assert_eq!(
        index
            .frags
            .iter()
            .map(|f| (f.start_ms, f.end_ms))
            .take(2)
            .collect::<Vec<_>>(),
        [(0, 2000), (2000, 4000)]
    );
}

// A seek stream starts at a segment boundary: each track lands at its playlist time whatever its
// raw decode time, so libvlc reports positions on the playlist timeline.
#[tokio::test]
async fn a_seek_stream_starts_at_its_playlist_time() {
    let video = stream(MediaType::Video, V_TS, 2.0, 6.0, 2, 10.0, b'v');
    let audio = stream(MediaType::Audio, A_TS, 3.0, 6.0, 2, 0.0, b'a');
    let plan = MuxPlan {
        video_start_s: 6.0,
        audio_start_s: 6.0,
        duration_ms: 12_000,
    };
    let (out, _, index) = run(video, audio, plan).await;
    let (_, frags) = parse(&out);
    let (v, a) = (VIDEO_ID, AUDIO_ID);
    assert_eq!(
        layout(&frags),
        [
            vec![(v, 6.0, 2), (a, 6.0, 2)],
            vec![(v, 8.0, 2), (a, 9.0, 1)],
            vec![(a, 10.5, 1)]
        ]
    );
    assert_eq!(
        frags[0][0].2[0],
        [b'v', 3, 0],
        "the first video fragment is the one of 6 s"
    );
    assert_eq!(index.frags[0].start_ms, 6000);
}

// A stream that stops inside a fragment (its producer was cancelled) ends its track there; the
// other track still goes to its end.
#[tokio::test]
async fn a_cut_input_ends_its_track() {
    let video = stream(MediaType::Video, V_TS, 2.0, 0.0, 3, 0.0, b'v');
    let mut audio = stream(MediaType::Audio, A_TS, 3.0, 0.0, 2, 0.0, b'a');
    audio.truncate(audio.len() - 1);
    let plan = MuxPlan {
        video_start_s: 0.0,
        audio_start_s: 0.0,
        duration_ms: 6000,
    };
    let (out, _, _) = run(video, audio, plan).await;
    let (_, frags) = parse(&out);
    let count = |id| {
        frags
            .iter()
            .flatten()
            .filter(|t| t.0 == id)
            .map(|t| t.2.len())
            .sum::<usize>()
    };
    assert_eq!((count(VIDEO_ID), count(AUDIO_ID)), (6, 2));
}

// libvlc seeks without reading the whole file only with an index: the head ends with a sidx whose
// one reference spans any time and starts at the first moof. A view that starts mid-stream
// starts at the fragment holding the target (or the keyframe before it), so its head plus the
// bytes from there is a valid stream of its own.
#[tokio::test]
async fn the_head_has_an_index_and_views_start_where_the_target_is() {
    let video = stream(MediaType::Video, V_TS, 2.0, 0.0, 5, 10.0, b'v');
    let audio = stream(MediaType::Audio, A_TS, 3.0, 0.0, 4, 0.0, b'a');
    let plan = MuxPlan {
        video_start_s: 0.0,
        audio_start_s: 0.0,
        duration_ms: 10_000,
    };
    let (out, _, index) = run(video, audio, plan).await;
    let (b, used) = Mp4Box::decode(&index.head[index.head.len() - 44..], 0).unwrap();
    assert_eq!((b.kind(), used), (FourCC::SIDX, 44));
    assert_eq!(&out[..index.head.len()], index.head.as_slice());
    assert_eq!(
        index.frags[0].offset,
        index.head.len() as u64,
        "the first moof follows the sidx"
    );
    assert!(
        index
            .frags
            .iter()
            .all(|f| &out[f.offset as usize + 4..][..4] == b"moof")
    );

    let at = index.offset_for(6500).unwrap();
    assert_eq!(at, index.frags[3].offset, "the 6–8 s fragment");
    let mut view = index.head.clone();
    view.extend_from_slice(&out[at as usize..]);
    let (_, frags) = parse(&view);
    assert_eq!(
        frags.iter().map(|f| f[0].1).collect::<Vec<_>>(),
        [6.0, 8.0, 10.5]
    );
    assert_eq!(index.offset_for(60_000), None, "not written");
}
