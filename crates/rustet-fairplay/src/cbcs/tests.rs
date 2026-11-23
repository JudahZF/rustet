use std::sync::atomic::{AtomicUsize, Ordering};

use rustet_hls::CbcsSegment;
use rustet_mp4::{Children, MediaType, Tfhd, Trun, TrunSample};
use tokio::io::AsyncReadExt;

use super::*;

const ADAM_ID: &str = "1440833098";

fn keys(list: &[&str]) -> Vec<Option<String>> {
    list.iter()
        .map(|k| (!k.is_empty()).then(|| k.to_string()))
        .collect()
}

fn media(key_list: &[&str], durations: &[f64]) -> CbcsMedia {
    let n = key_list.len().max(durations.len());
    CbcsMedia {
        file_url: "http://127.0.0.1:1/file.mp4".into(),
        kid_base64: None,
        segments: (0..n)
            .map(|i| CbcsSegment {
                key_uri: key_list
                    .get(i)
                    .filter(|k| !k.is_empty())
                    .map(|k| k.to_string()),
                duration: durations.get(i).copied().unwrap_or(10.0),
            })
            .collect(),
    }
}

/// Accepts every dial with a pipe whose peer discards everything (closing writes a goodbye
/// frame), and counts the dials.
#[derive(Default)]
struct CountingDialer {
    dials: AtomicUsize,
    in_process: bool,
}

fn drained_pipe() -> DrmStream {
    let (c, mut peer) = tokio::io::duplex(1024);
    tokio::spawn(async move {
        let mut buf = [0; 256];
        while matches!(peer.read(&mut buf).await, Ok(n) if n > 0) {}
    });
    Box::new(c)
}

#[async_trait]
impl CbcsDialer for CountingDialer {
    async fn dial_cbcs(&self, _: &CancellationToken) -> io::Result<DrmStream> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        Ok(drained_pipe())
    }

    fn in_process(&self) -> bool {
        self.in_process
    }
}

fn source(key_list: &[&str], durations: &[f64]) -> CbcsSource {
    CbcsSource::new(
        ADAM_ID,
        Arc::new(CountingDialer::default()),
        reqwest::Client::new(),
        &media(key_list, durations),
        0,
    )
}

// ── Keys ─────────────────────────────────────────────────────────────────────

#[test]
fn effective_key_is_the_last_declared_one() {
    let k = keys(&[PREFETCH_KEY, "skd://real", "", "", "skd://next", ""]);
    let want = [
        PREFETCH_KEY,
        "skd://real",
        "skd://real",
        "skd://real",
        "skd://next",
        "skd://next",
    ];
    for (i, w) in want.iter().enumerate() {
        assert_eq!(effective_key(&k, i), Some(*w), "fragment {i}");
    }
    assert_eq!(effective_key(&k, 99), Some("skd://next"), "past the end");
    assert_eq!(effective_key(&[], 3), None);
}

/// A handshake on a fresh connection starts with the adamID length byte, never with the
/// four zero bytes of a key switch, which the server would misread.
#[tokio::test]
async fn key_handshake_bytes() {
    let mut buf = Vec::new();
    send_key_handshake(&mut buf, ADAM_ID, Some("skd://real"))
        .await
        .unwrap();
    let mut want = vec![10];
    want.extend_from_slice(ADAM_ID.as_bytes());
    want.push(10);
    want.extend_from_slice(b"skd://real");
    assert_eq!(buf, want);

    buf.clear();
    send_key_handshake(&mut buf, ADAM_ID, Some(PREFETCH_KEY))
        .await
        .unwrap();
    assert_eq!(
        &buf[..2],
        b"\x010",
        "the prefetch key goes under adamID \"0\""
    );

    buf.clear();
    send_key_handshake(&mut buf, ADAM_ID, None).await.unwrap();
    assert!(buf.is_empty());
}

#[tokio::test]
async fn frag_key_starts_with_the_fragment_zero_key_then_switches() {
    let src = source(&[PREFETCH_KEY, "skd://real"], &[]);
    let mut armed = None;
    let mut buf = Vec::new();

    src.send_frag_key(0, &mut armed, &mut buf).await;
    let mut want = vec![1, b'0', PREFETCH_KEY.len() as u8];
    want.extend_from_slice(PREFETCH_KEY.as_bytes());
    assert_eq!(buf, want);

    buf.clear();
    src.send_frag_key(1, &mut armed, &mut buf).await;
    // The playlist URI goes out bare, as upstream sends it: no ",<kid>" suffix.
    let mut want = vec![0, 0, 0, 0, ADAM_ID.len() as u8];
    want.extend_from_slice(ADAM_ID.as_bytes());
    want.push(10);
    want.extend_from_slice(b"skd://real");
    assert_eq!(
        buf, want,
        "fragment 1 must switch away from the prefetch key first"
    );

    buf.clear();
    src.send_frag_key(1, &mut armed, &mut buf).await;
    src.send_frag_key(2, &mut armed, &mut buf).await;
    assert!(buf.is_empty(), "the armed key is not sent again");
}

#[tokio::test]
async fn seek_frag_key_switches_only_after_the_start() {
    let src = source(&[PREFETCH_KEY, "skd://real"], &[]);
    let mut buf = Vec::new();
    src.send_seek_frag_key(0, 0, &mut buf).await.unwrap();
    assert!(buf.is_empty());
    src.send_seek_frag_key(1, 0, &mut buf).await.unwrap();
    assert_eq!(&buf[..5], &[0, 0, 0, 0, 10]);
}

/// Like upstream, only the keys of segments 0 and 1 are used; later tags are ignored.
#[test]
fn upstream_keys_keep_segments_zero_and_one() {
    let m = media(&[PREFETCH_KEY, "skd://real", "", "skd://later"], &[]);
    assert_eq!(upstream_keys(&m), keys(&[PREFETCH_KEY, "skd://real"]));
    assert_eq!(first_audio_key_uri(&m), Some("skd://real"));
    assert_eq!(
        first_audio_key_uri(&media(&[PREFETCH_KEY, "", "skd://later"], &[])),
        None
    );
    assert!(is_sentinel_key(&PREFETCH_KEY.to_uppercase()));
}

#[test]
fn source_from_past_the_end_starts_at_the_last_segment() {
    let src = source(&[], &[10.0, 10.0, 10.0]);
    let (_, actual) = src.source_from(999.0);
    assert_eq!(actual, 20.0);
    assert_eq!(start_fragment(&[10.0, 10.0, 10.0], 999.0), (2, 20.0));
    assert_eq!(start_fragment(&[10.0, 10.0, 10.0], 15.0), (1, 10.0));
    assert_eq!(start_fragment(&[10.0, 10.0, 10.0], 10.0), (1, 10.0));
}

#[test]
fn seek_source_carries_the_key_in_force() {
    let src = source(&[PREFETCH_KEY, "skd://real", ""], &[10.0, 10.0, 10.0]);
    assert!(src.seekable().is_some());
    let (from_zero, actual) = src.seek_to(0.0);
    assert_eq!((from_zero.start, actual), (Start::Beginning, 0.0));
    let (skip, actual) = src.seek_to(25.0);
    assert_eq!(actual, 20.0);
    let want = Start::Fragment {
        index: 2,
        key: Some("skd://real".into()),
    };
    assert_eq!(skip.start, want);
    assert!(
        skip.seekable().is_none(),
        "a seek source is not seekable again"
    );
}

// ── Output patching ──────────────────────────────────────────────────────────

/// A moof shaped like Apple's ALAC fragments: a v0 tfdt and `truns` truns of
/// `samples_per_trun` samples, every trun with a data offset.
fn alac_like_fragment(truns: usize, samples_per_trun: usize, dur: u32) -> Fragment {
    let mut frag = Fragment::new(7, 3);
    let traf = frag.moof_mut().unwrap().traf_mut().unwrap();
    traf.tfdt_mut().unwrap().version = 0;
    let tfhd = traf.tfhd_mut().unwrap();
    tfhd.sample_description_index = 2;
    tfhd.flags |= Tfhd::SAMPLE_DESCRIPTION_INDEX_PRESENT;
    for i in 0..truns {
        if i > 0 {
            traf.push(Trun::new());
        }
        let trun = traf.truns_mut().nth(i).unwrap();
        trun.data_offset = 1000 + i as i32;
        for _ in 0..samples_per_trun {
            trun.samples.push(TrunSample {
                duration: dur,
                size: 10,
                ..TrunSample::default()
            });
        }
    }
    frag
}

/// ALAC moofs carry 11 truns (10 × 16 + 1 frames). The decode time must advance by every
/// trun's duration, or VLC reports a "Fragment sequence discontinuity" about 28 s in.
#[test]
fn patch_alac_fragment_sums_every_trun() {
    let mut frag = alac_like_fragment(11, 16, 4096);
    let traf = frag.moof_mut().unwrap().traf_mut().unwrap();
    traf.truns_mut().nth(10).unwrap().samples.truncate(1);

    let next = patch_alac_fragment(&mut frag, 1_000_000);
    assert_eq!(next, 1_000_000 + 161 * 4096, "all 161 frames");
    let tfdt = frag.moof().unwrap().traf().unwrap().tfdt().unwrap();
    assert_eq!(tfdt.base_media_decode_time(), 1_000_000);
    assert_eq!(
        skip_frag_tfdt_delta(&alac_like_fragment(11, 16, 4096)),
        176 * 4096
    );
}

/// Upgrading tfdt to 64 bits grows the moof by 4 bytes; every trun's data offset must move
/// with it or the samples are read from the wrong place.
#[test]
fn patch_alac_fragment_shifts_data_offsets_when_tfdt_grows() {
    let mut frag = alac_like_fragment(3, 2, 1024);
    let before = frag.moof().unwrap().size();
    patch_alac_fragment(&mut frag, 0);
    let moof = frag.moof().unwrap();
    assert_eq!(moof.size() - before, 4, "tfdt v0 → v1");
    let traf = moof.traf().unwrap();
    assert_eq!(traf.tfdt().unwrap().version, 1);
    for (i, trun) in traf.truns().enumerate() {
        assert_eq!(trun.data_offset, 1000 + i as i32 + 4, "trun {i}");
    }
}

/// Apple's CDN varies the track ID and sample description index between fragments, which
/// makes ffmpeg skip the trun; both are normalised to 1.
#[test]
fn patch_alac_fragment_normalises_track_and_sequence() {
    let mut frag = alac_like_fragment(1, 1, 1024);
    patch_alac_fragment_seq(&mut frag, 0, 0);
    let moof = frag.moof().unwrap();
    let tfhd = moof.traf().unwrap().tfhd().unwrap();
    assert_eq!((tfhd.track_id, tfhd.sample_description_index), (1, 1));
    assert_eq!(moof.mfhd().unwrap().sequence_number, 0, "VLC counts from 0");
}

#[test]
fn patch_alac_fragment_adds_a_missing_tfdt_before_the_trun() {
    let mut frag = alac_like_fragment(1, 2, 1024);
    let traf = frag.moof_mut().unwrap().traf_mut().unwrap();
    traf.remove_kind(rustet_mp4::FourCC::TFDT);
    let next = patch_alac_fragment(&mut frag, 5);
    assert_eq!(next, 5 + 2 * 1024);
    let traf = frag.moof().unwrap().traf().unwrap();
    let kinds: Vec<_> = traf.children.iter().map(Mp4Box::kind).collect();
    assert_eq!(
        kinds,
        [
            rustet_mp4::FourCC::TFHD,
            rustet_mp4::FourCC::TFDT,
            rustet_mp4::FourCC::TRUN
        ]
    );
    assert_eq!(traf.tfdt().unwrap().version, 1);
    // tfdt v1 is 20 bytes.
    assert_eq!(traf.trun().unwrap().data_offset, 1000 + 20);
}

#[test]
fn patch_alac_fragment_without_moof_is_a_no_op() {
    assert_eq!(patch_alac_fragment(&mut Fragment::default(), 42), 42);
    assert_eq!(skip_frag_tfdt_delta(&Fragment::default()), 0);
}

#[test]
fn patch_moov_duration_sets_every_header() {
    let mut init = InitSegment::single_track(44100, MediaType::Audio).unwrap();
    patch_moov_duration(&mut init, 0);
    assert_eq!(
        init.moov().unwrap().mvhd().unwrap().duration(),
        0,
        "0 leaves the moov unchanged"
    );

    patch_moov_duration(&mut init, 298_144);
    let moov = init.moov().unwrap();
    let trak = moov.traks().next().unwrap();
    assert_eq!(moov.mvhd().unwrap().duration(), 298_144);
    assert_eq!(trak.tkhd().unwrap().duration(), 298_144);
    assert_eq!(
        trak.mdhd().unwrap().duration(),
        298_144 * 44100 / 1000,
        "mdhd is in the 44.1 kHz media timescale"
    );
}

// ── DRM connection ───────────────────────────────────────────────────────────

fn idle_conn<'a>(dialer: &'a CountingDialer, cancel: &'a CancellationToken) -> DrmConn<'a> {
    let mut dc = DrmConn::new(dialer, cancel, drained_pipe());
    dc.last_used = std::time::Instant::now()
        .checked_sub(2 * DRM_IDLE_TIMEOUT)
        .unwrap();
    dc
}

#[tokio::test]
async fn idle_socket_connection_is_replaced() {
    let dialer = CountingDialer::default();
    let cancel = CancellationToken::new();
    assert!(idle_conn(&dialer, &cancel).refresh_if_needed().await);
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 1);

    let mut fresh = DrmConn::new(&dialer, &cancel, drained_pipe());
    assert!(
        !fresh.refresh_if_needed().await,
        "a recently used connection stays"
    );
}

#[tokio::test]
async fn idle_in_process_pipe_is_kept() {
    let dialer = CountingDialer {
        in_process: true,
        ..CountingDialer::default()
    };
    let cancel = CancellationToken::new();
    assert!(!idle_conn(&dialer, &cancel).refresh_if_needed().await);
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn redial_after_cancel_does_not_dial() {
    let dialer = CountingDialer::default();
    let cancel = CancellationToken::new();
    let mut dc = DrmConn::new(&dialer, &cancel, drained_pipe());
    cancel.cancel();
    assert!(matches!(dc.redial(3).await, Err(Error::Cancelled)));
    assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
}

// ── Stall detection ──────────────────────────────────────────────────────────

const STALL: Duration = Duration::from_millis(100);

/// Reads of at least 256 bytes keep the download alive.
#[tokio::test(start_paused = true)]
async fn stall_guard_progress_resets_the_clock() {
    let (mut tx, rx) = tokio::io::duplex(4096);
    let mut guard = StallGuard::new(rx, STALL);
    let mut buf = [0; 512];
    for _ in 0..5 {
        tokio::time::sleep(STALL / 2).await;
        tx.write_all(&[7; 512]).await.unwrap();
        guard.read_exact(&mut buf).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn stall_guard_fires_without_progress() {
    let (_tx, rx) = tokio::io::duplex(64);
    let mut guard = StallGuard::new(rx, STALL);
    let err = guard.read(&mut [0; 16]).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

/// Small reads do not count as progress, so a trickling connection cannot evade the guard.
#[tokio::test(start_paused = true)]
async fn stall_guard_small_reads_do_not_reset() {
    let (mut tx, rx) = tokio::io::duplex(64);
    let mut guard = StallGuard::new(rx, STALL);
    let mut failed = false;
    for _ in 0..10 {
        tokio::time::sleep(STALL / 4).await;
        tx.write_all(&[1]).await.unwrap();
        if guard.read(&mut [0; 16]).await.is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "a 1-byte trickle must time out");
}

/// The clock also runs while nobody reads: data waiting after a long pause is not read.
#[tokio::test(start_paused = true)]
async fn stall_guard_counts_time_between_reads() {
    let (mut tx, rx) = tokio::io::duplex(64);
    let mut guard = StallGuard::new(rx, STALL);
    tx.write_all(b"ready").await.unwrap();
    tokio::time::sleep(STALL * 2).await;
    assert!(guard.read(&mut [0; 16]).await.is_err());
}
