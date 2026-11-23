use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Instant;

use super::*;

/// Frame `n` of the test pattern: every pixel holds the frame number in B, G, R (alpha is the
/// ring's 255). A frame mixing two numbers is torn.
fn pattern(format: Format, n: u64) -> Vec<u8> {
    let px = [n as u8, (n >> 8) as u8, (n >> 16) as u8, 0];
    let mut v = Vec::with_capacity(format.frame_bytes());
    for _ in 0..format.height {
        for _ in 0..format.stride / 4 {
            v.extend_from_slice(&px);
        }
    }
    v
}

/// The frame number every pixel of `buf` holds, or `None` for a torn (or wrong-alpha) frame.
fn number_of(buf: &[u8]) -> Option<u64> {
    let first = buf.get(..4)?;
    let n = u64::from(first[0]) | u64::from(first[1]) << 8 | u64::from(first[2]) << 16;
    (first[3] == 255 && buf.as_chunks::<4>().0.iter().all(|p| p == first)).then_some(n)
}

#[test]
fn a_reader_racing_the_writer_never_sees_a_torn_frame() {
    let dir = tempfile::tempdir().unwrap();
    for format in [
        Format::bgra(64, 16),
        Format::bgra(640, 360),
        Format {
            stride: 1288 * 4,
            ..Format::bgra(1280, 720)
        },
    ] {
        let mut writer = Writer::create(dir.path(), 1, format).unwrap();
        let mut reader = Reader::open(writer.path()).unwrap();
        let frames = (16 << 20) / format.frame_bytes() as u64 + 200;
        let done = Arc::new(AtomicBool::new(false));
        let write = {
            let done = done.clone();
            thread::spawn(move || {
                for n in 1..=frames {
                    writer.write_frame(&pattern(format, n), n).unwrap();
                }
                done.store(true, Ordering::SeqCst);
                writer
            })
        };
        let (mut buf, mut seen, mut last) = (Vec::new(), 0u64, 0u64);
        while !done.load(Ordering::SeqCst) || seen == 0 {
            if let Read::Frame(f) = reader.read(&mut buf) {
                assert_eq!(
                    number_of(&buf),
                    Some(f.number % (1 << 24)),
                    "torn frame {} at {format:?}",
                    f.number
                );
                assert_eq!(f.presented_at_us, f.number, "the time belongs to the frame");
                assert!(f.number > last, "frames only move forward");
                (last, seen) = (f.number, seen + 1);
            }
        }
        let writer = write.join().unwrap();
        // The newest frame is still there after the writer stopped writing.
        assert!(
            matches!(reader.read(&mut buf), Read::Frame(f) if f.number == frames) || last == frames
        );
        assert!(seen > 1, "the reader saw {seen} frames");
        drop(writer);
    }
}

#[test]
fn a_slot_being_written_is_not_returned() {
    let dir = tempfile::tempdir().unwrap();
    let format = Format::bgra(8, 2);
    let mut writer = Writer::create(dir.path(), 1, format).unwrap();
    for n in 1..=3 {
        writer.write_frame(&pattern(format, n), 0).unwrap();
    }
    let mut reader = Reader::open(writer.path()).unwrap();
    // Frame 3's slot goes odd, as if the writer had started frame 6 in it during the copy.
    let seq = &writer.shared.map.words()[w::slot_seq(0)];
    seq.store(2 * 6 + 1, Ordering::Relaxed);
    let mut buf = Vec::new();
    assert_eq!(
        reader.read(&mut buf),
        Read::Unchanged,
        "a slot that does not hold `latest` complete is skipped"
    );
    seq.store(2 * 3, Ordering::Relaxed);
    assert!(matches!(reader.read(&mut buf), Read::Frame(f) if f.number == 3));
    assert_eq!(number_of(&buf), Some(3));
}

#[test]
fn readers_before_the_first_frame_and_late_ones() {
    let dir = tempfile::tempdir().unwrap();
    let format = Format::bgra(16, 4);
    let mut writer = Writer::create(dir.path(), 7, format).unwrap();
    let mut early = Reader::open(writer.path()).unwrap();
    let mut buf = Vec::new();
    assert_eq!(early.read(&mut buf), Read::Unchanged, "no frame yet");
    assert!(buf.is_empty(), "the buffer is untouched");
    assert_eq!(
        (early.generation(), early.format(), early.writer_pid()),
        (7, format, std::process::id())
    );

    writer.write_frame(&pattern(format, 1), 0).unwrap();
    assert!(matches!(early.read(&mut buf), Read::Frame(f) if f.number == 1));
    for n in 2..=10 {
        writer.write_frame(&pattern(format, n), 0).unwrap();
    }
    let mut late = Reader::open(writer.path()).unwrap();
    assert!(
        matches!(late.read(&mut buf), Read::Frame(f) if f.number == 10),
        "a late reader gets the newest frame at once"
    );
    assert_eq!(number_of(&buf), Some(10));
    assert_eq!(
        late.read(&mut buf),
        Read::Unchanged,
        "and nothing again until the next one"
    );
    assert!(
        matches!(early.read(&mut buf), Read::Frame(f) if f.number == 10),
        "a slow reader skips to the newest"
    );
}

#[test]
fn a_size_change_is_a_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let small = Format::bgra(32, 16);
    let mut old = Writer::create(dir.path(), 1, small).unwrap();
    old.write_frame(&pattern(small, 1), 0).unwrap();
    let mut reader = Reader::open(old.path()).unwrap();
    let old_path = old.path().to_owned();

    let big = Format::bgra(64, 32);
    let mut new = Writer::create(dir.path(), 2, big).unwrap();
    old.supersede();
    assert!(!old_path.exists(), "the old file is gone");
    let mut buf = Vec::new();
    assert!(
        matches!(reader.read(&mut buf), Read::Frame(f) if f.number == 1),
        "the last frame is still shown"
    );
    assert_eq!(reader.read(&mut buf), Read::Ended(End::Superseded));

    new.write_frame(&pattern(big, 1), 0).unwrap();
    let mut next = Reader::open(new.path()).unwrap();
    assert_eq!((next.generation(), next.format()), (2, big));
    assert!(matches!(next.read(&mut buf), Read::Frame(_)));
    assert_eq!(buf.len(), big.packed_bytes());
}

#[test]
fn closing_deletes_the_ring_and_tells_readers() {
    let dir = tempfile::tempdir().unwrap();
    let format = Format {
        stride: 24 * 4,
        ..Format::bgra(16, 4)
    };
    let mut writer = Writer::create(dir.path(), 1, format).unwrap();
    writer.write_frame(&pattern(format, 5), 0).unwrap();
    let path = writer.path().to_owned();
    let mut reader = Reader::open(&path).unwrap();
    drop(writer);
    assert!(!path.exists());
    let mut buf = Vec::new();
    assert!(matches!(reader.read(&mut buf), Read::Frame(_)));
    assert_eq!(
        buf.len(),
        16 * 4 * 4,
        "rows come back packed, without the stride padding"
    );
    assert_eq!(number_of(&buf), Some(5));
    assert_eq!(reader.read(&mut buf), Read::Ended(End::Closed));
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no temporary file left"
    );
}

#[test]
fn files_that_are_not_rings_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("rustet-video-1-1.ring");
    fs::write(&junk, vec![7u8; 8192]).unwrap();
    assert!(matches!(Reader::open(&junk), Err(Error::NotARing)));
    fs::write(&junk, b"short").unwrap();
    assert!(matches!(Reader::open(&junk), Err(Error::NotARing)));

    let format = Format::bgra(8, 2);
    let writer = Writer::create(dir.path(), 9, format).unwrap();
    let mut bytes = fs::read(writer.path()).unwrap();
    bytes[8..16].copy_from_slice(&2u64.to_ne_bytes());
    let other = dir.path().join("rustet-video-2-2.ring");
    fs::write(&other, &bytes).unwrap();
    assert!(matches!(Reader::open(&other), Err(Error::Version(2))));
    assert!(
        matches!(
            Writer::create(dir.path(), 3, Format::bgra(3, 2)),
            Err(Error::BadFormat(_))
        ),
        "odd width"
    );
    let mut w = Writer::create(dir.path(), 4, format).unwrap();
    assert!(matches!(
        w.write_frame(&[0; 8], 0),
        Err(Error::ShortFrame { want: 64, .. })
    ));
}

/// The child half of [`a_dead_writer_leaves_a_stale_ring`]: with `RUSTET_FRAMERING_CHILD` set it
/// writes one frame, prints the ring's path and waits to be killed.
#[test]
fn dead_writer_child() {
    let Some(dir) = std::env::var_os("RUSTET_FRAMERING_CHILD") else {
        return;
    };
    let format = Format::bgra(8, 2);
    let mut w = Writer::create(Path::new(&dir), 1, format).unwrap();
    w.write_frame(&pattern(format, 42), 0).unwrap();
    println!("RING {}", w.path().display());
    thread::sleep(Duration::from_secs(60));
}

#[test]
fn a_dead_writer_leaves_a_stale_ring() {
    let dir = tempfile::tempdir().unwrap();
    /// Kills the child when the test fails early.
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::dead_writer_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("RUSTET_FRAMERING_CHILD", dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .map(Kill)
        .unwrap();
    let out = std::io::BufReader::new(child.0.stdout.take().unwrap());
    let path = out
        .lines()
        .map_while(Result::ok)
        .find_map(|l| l.split_once("RING ").map(|(_, p)| PathBuf::from(p)))
        .expect("the child made a ring");
    let mut reader = Reader::open(&path).unwrap();
    assert_eq!(reader.ended(), None, "live while the writer runs");
    assert_eq!(sweep(dir.path()), 0, "a live ring is kept");
    drop(child); // SIGKILL: no drop, no close
    let killed = Instant::now();
    while reader.ended().is_none() {
        assert!(
            killed.elapsed() < STALE_AFTER + Duration::from_secs(2),
            "never went stale"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        killed.elapsed() > STALE_AFTER - HEARTBEAT_EVERY * 2,
        "stale too early: {:?}",
        killed.elapsed()
    );
    let mut buf = Vec::new();
    assert!(
        matches!(reader.read(&mut buf), Read::Frame(f) if f.number == 1),
        "its last frame is still readable"
    );
    assert_eq!(reader.read(&mut buf), Read::Ended(End::Stale));
    assert_eq!(
        sweep(dir.path()),
        1,
        "the sweep deletes the dead writer's ring"
    );
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn the_ring_directory_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let rings = dir.path().join("rustet");
    ensure_private_dir(&rings).unwrap();
    assert_eq!(
        fs::metadata(&rings).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::set_permissions(&rings, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        ensure_private_dir(&rings),
        Err(Error::NotPrivate(_))
    ));
    let format = Format::bgra(8, 2);
    fs::set_permissions(&rings, fs::Permissions::from_mode(0o700)).unwrap();
    let w = Writer::create(&rings, 1, format).unwrap();
    assert_eq!(
        fs::metadata(w.path()).unwrap().permissions().mode() & 0o777,
        0o600,
        "the ring is the user's only"
    );
}
