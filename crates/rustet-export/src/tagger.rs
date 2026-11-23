//! iTunes-style MP4 tags (`moov.udta.meta.ilst`) and the cover-art download.
//!
//! The atoms and data types match what the Go engine wrote with go-mp4tag: UTF-8 text atoms,
//! `trkn` / `disk` number pairs, `rtng` as a signed byte (1 explicit, 2 clean), `covr` as JPEG or
//! PNG, and freeform `----:com.apple.iTunes:<NAME>` atoms. Values left empty keep what the file
//! already has, and new artwork is added after existing artwork. Unlike go-mp4tag, a file
//! without an `ilst` (or `udta`) gets one.

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use mp4ameta::{Data, Fourcc, FreeformIdent, ReadConfig, Tag, WriteConfig};
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use tokio_util::sync::CancellationToken;

/// The metadata used for tagging, from the catalog responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrackMeta {
    pub title: String,
    pub artist_name: String,
    pub album_artist: String,
    pub album_name: String,
    pub track_number: u32,
    pub track_total: u32,
    pub disc_number: u32,
    pub disc_total: u32,
    /// `YYYY-MM-DD`.
    pub release_date: String,
    pub genre: String,
    pub composer: String,
    pub copyright: String,
    pub record_label: String,
    pub isrc: String,
    pub upc: String,
    /// `explicit`, `clean` or empty.
    pub content_rating: String,
    /// Apple CDN template URL with `{w}` and `{h}`.
    pub artwork_url: String,
    pub has_lyrics: bool,
}

/// Why tagging failed. Tagging failures are logged, never fatal to an export.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Mp4(#[from] mp4ameta::Error),
    #[error("not an MP4 file: {0}")]
    Layout(&'static str),
}

/// One downloaded cover image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Artwork {
    pub data: Vec<u8>,
    /// The CDN served `image/png`; everything else is tagged as JPEG.
    pub png: bool,
}

/// Writes `meta`, `artwork` and `lyrics` (empty: none) into the MP4 at `path`.
pub(crate) fn tag_file(
    path: &Path,
    meta: &TrackMeta,
    artwork: Option<Artwork>,
    lyrics: &str,
) -> Result<(), TagError> {
    edit_tag(path, |tag| apply(tag, meta, artwork, lyrics))
}

const NAME: Fourcc = Fourcc(*b"\xa9nam");
const ARTIST: Fourcc = Fourcc(*b"\xa9ART");
const ALBUM: Fourcc = Fourcc(*b"\xa9alb");
const ALBUM_ARTIST: Fourcc = Fourcc(*b"aART");
const COMPOSER: Fourcc = Fourcc(*b"\xa9wrt");
const COPYRIGHT: Fourcc = Fourcc(*b"cprt");
const LYRICS: Fourcc = Fourcc(*b"\xa9lyr");
const GENRE: Fourcc = Fourcc(*b"\xa9gen");
const PUBLISHER: Fourcc = Fourcc(*b"\xa9pub");
const DATE: Fourcc = Fourcc(*b"\xa9day");
const RATING: Fourcc = Fourcc(*b"rtng");
const TRACK: Fourcc = Fourcc(*b"trkn");
const DISC: Fourcc = Fourcc(*b"disk");
const COVER: Fourcc = Fourcc(*b"covr");
const ITUNES: &str = "com.apple.iTunes";

fn apply(tag: &mut Tag, meta: &TrackMeta, artwork: Option<Artwork>, lyrics: &str) {
    for (ident, value) in [
        (NAME, meta.title.as_str()),
        (ALBUM, &meta.album_name),
        (ALBUM_ARTIST, &meta.album_artist),
        (ARTIST, &meta.artist_name),
        (COMPOSER, &meta.composer),
        (COPYRIGHT, &meta.copyright),
        (LYRICS, lyrics),
        (GENRE, &meta.genre),
        (PUBLISHER, &meta.record_label),
        (DATE, &meta.release_date),
    ] {
        if !value.is_empty() {
            tag.set_data(ident, Data::Utf8(value.to_owned()));
        }
    }

    let rating = match meta.content_rating.as_str() {
        "explicit" => Some(1),
        "clean" => Some(2),
        _ => None, // keep whatever the file has
    };
    if let Some(r) = rating {
        tag.set_data(RATING, Data::BeSigned(vec![r]));
    }

    // trkn: 0, number, total, 0 (u16 each); disk: 0, number, total.
    let (n, t) = merged_pair(tag, TRACK, meta.track_number, meta.track_total);
    if n > 0 || t > 0 {
        let mut b = pair_bytes(n, t);
        b.extend([0, 0]);
        tag.set_data(TRACK, Data::Reserved(b));
    }
    let (n, t) = merged_pair(tag, DISC, meta.disc_number, meta.disc_total);
    if n > 0 || t > 0 {
        tag.set_data(DISC, Data::Reserved(pair_bytes(n, t)));
    }

    for (name, value) in [
        ("ISRC", &meta.isrc),
        ("UPC", &meta.upc),
        ("LABEL", &meta.record_label),
        ("RELEASETIME", &meta.release_date),
        ("PERFORMER", &meta.artist_name),
    ] {
        if !value.is_empty() {
            tag.set_data(
                FreeformIdent::new_static(ITUNES, name),
                Data::Utf8(value.clone()),
            );
        }
    }

    if let Some(art) = artwork.filter(|a| !a.data.is_empty()) {
        let data = if art.png {
            Data::Png(art.data)
        } else {
            Data::Jpeg(art.data)
        };
        tag.add_data(COVER, data);
    }
}

/// A new number or total wins when non-zero; otherwise the file's value stays.
fn merged_pair(tag: &Tag, ident: Fourcc, number: u32, total: u32) -> (u16, u16) {
    let (old_n, old_t) = tag
        .bytes_of(&ident)
        .next()
        .filter(|b| b.len() >= 6)
        .map_or((0, 0), |b| {
            (
                u16::from_be_bytes([b[2], b[3]]),
                u16::from_be_bytes([b[4], b[5]]),
            )
        });
    let pick = |new: u32, old: u16| match u16::try_from(new) {
        Ok(0) | Err(_) => old,
        Ok(v) => v,
    };
    (pick(number, old_n), pick(total, old_t))
}

fn pair_bytes(n: u16, t: u16) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend(n.to_be_bytes());
    b.extend(t.to_be_bytes());
    b
}

/// One top-level box: its type and its byte range in the file.
#[derive(Debug, Clone, Copy)]
struct TopBox {
    kind: [u8; 4],
    start: u64,
    end: u64,
}

/// Lists the top-level boxes (headers only).
fn top_level_boxes(r: &mut (impl Read + Seek)) -> Result<Vec<TopBox>, TagError> {
    let len = r.seek(SeekFrom::End(0))?;
    let mut boxes = Vec::new();
    let mut pos = 0;
    while pos + 8 <= len {
        r.seek(SeekFrom::Start(pos))?;
        let mut head = [0u8; 8];
        r.read_exact(&mut head)?;
        let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let kind = [head[4], head[5], head[6], head[7]];
        let size = match size {
            0 => len - pos, // extends to the end of the file
            1 => {
                let mut large = [0u8; 8];
                r.read_exact(&mut large)?;
                u64::from_be_bytes(large)
            }
            s => s,
        };
        if size < 8 || pos + size > len {
            return Err(TagError::Layout("box size out of range"));
        }
        boxes.push(TopBox {
            kind,
            start: pos,
            end: pos + size,
        });
        pos += size;
    }
    Ok(boxes)
}

fn read_range(f: &mut (impl Read + Seek), b: TopBox, out: &mut Vec<u8>) -> io::Result<()> {
    f.seek(SeekFrom::Start(b.start))?;
    let len = usize::try_from(b.end - b.start).map_err(io::Error::other)?;
    let at = out.len();
    out.resize(at + len, 0);
    f.read_exact(&mut out[at..])
}

/// Reads the file's tag, applies `edit` and writes it back, atomically and with bounded memory.
///
/// mp4ameta keeps all data after the first change in memory. When the moov precedes the media
/// (fMP4 and fast-start files, which exports always are), only `ftyp` and `moov` are tagged, in
/// memory, and the new moov is spliced into a copy of the file that then replaces it. Chunk
/// offsets come out right because every one of them points past the moov in both layouts.
/// Otherwise (moov at the end) the data that moves is small and the file is tagged in place.
fn edit_tag(path: &Path, edit: impl FnOnce(&mut Tag)) -> Result<(), TagError> {
    let read_cfg = ReadConfig {
        read_meta_items: true,
        read_image_data: true,
        ..ReadConfig::NONE
    };
    let write_cfg = WriteConfig {
        write_meta_items: true,
        ..WriteConfig::NONE
    };

    let mut file = File::open(path)?;
    let boxes = top_level_boxes(&mut file)?;
    let find = |kind: &[u8; 4]| boxes.iter().copied().find(|b| &b.kind == kind);
    let ftyp = boxes
        .first()
        .copied()
        .filter(|b| &b.kind == b"ftyp")
        .ok_or(TagError::Layout("no ftyp box first"))?;
    let moov = find(b"moov").ok_or(TagError::Layout("no moov box"))?;
    let moov_first = find(b"mdat").is_some_and(|mdat| moov.start < mdat.start);
    if !moov_first {
        drop(file);
        let mut tag = Tag::read_with_path(path, &read_cfg)?;
        edit(&mut tag);
        tag.write_with_path(path, &write_cfg)?;
        return Ok(());
    }

    let mut head = Vec::new();
    read_range(&mut file, ftyp, &mut head)?;
    read_range(&mut file, moov, &mut head)?;
    head.extend_from_slice(&[0, 0, 0, 8, b'm', b'd', b'a', b't']);
    let mut mem = Cursor::new(head);
    let mut tag = Tag::read_with(&mut mem, &read_cfg)?;
    edit(&mut tag);
    tag.write_with(&mut mem, &write_cfg)?;
    let new_moov = top_level_boxes(&mut mem)?
        .into_iter()
        .find(|b| &b.kind == b"moov")
        .ok_or(TagError::Layout("tag writer dropped the moov box"))?;
    let mem = mem.into_inner();
    let new_moov = &mem[usize::try_from(new_moov.start).map_err(io::Error::other)?
        ..usize::try_from(new_moov.end).map_err(io::Error::other)?];

    let dir = path.parent().unwrap_or(Path::new("."));
    let mut out = tempfile::Builder::new()
        .prefix(".am-tag-")
        .tempfile_in(dir)?;
    file.seek(SeekFrom::Start(0))?;
    io::copy(&mut (&mut file).take(moov.start), &mut out)?;
    io::Write::write_all(&mut out, new_moov)?;
    file.seek(SeekFrom::Start(moov.end))?;
    io::copy(&mut file, &mut out)?;
    out.as_file().sync_all()?;
    drop(file);
    out.persist(path).map_err(|e| e.error)?;
    Ok(())
}

// ── Artwork ──────────────────────────────────────────────────────────────────

/// The square size requested from the Apple CDN when the request has none.
pub(crate) const DEFAULT_ARTWORK_SIZE: u32 = 3000;

/// Bounds one artwork download; Apple's 3000 px JPEGs are well under it.
pub(crate) const MAX_ARTWORK_BYTES: usize = 16 << 20;

const ARTWORK_TIMEOUT: Duration = Duration::from_secs(30);
const ARTWORK_ATTEMPTS: u32 = 3;

/// Why the artwork could not be downloaded. The export continues without it.
#[derive(Debug, thiserror::Error)]
pub enum ArtworkError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("artwork HTTP {}", .0.as_u16())]
    Status(StatusCode),
    #[error("artwork larger than {MAX_ARTWORK_BYTES} bytes")]
    TooLarge,
    #[error("cancelled")]
    Cancelled,
}

/// Formats an Apple CDN artwork template (`https://…/{w}x{h}bb.jpg`) for a square `size`.
pub(crate) fn artwork_url(template: &str, size: u32) -> String {
    let size = if size == 0 {
        DEFAULT_ARTWORK_SIZE
    } else {
        size
    }
    .to_string();
    template.replace("{w}", &size).replace("{h}", &size)
}

/// Downloads the artwork, retrying up to 3 times with growing back-off for transient CDN
/// failures. `cancel` stops it at once.
pub(crate) async fn download_artwork(
    http: &reqwest::Client,
    cancel: &CancellationToken,
    template: &str,
    size: u32,
) -> Result<Artwork, ArtworkError> {
    let url = artwork_url(template, size);
    let mut last = ArtworkError::Cancelled;
    for attempt in 0..ARTWORK_ATTEMPTS {
        if attempt > 0 {
            let backoff = Duration::from_secs(u64::from(attempt) * 2);
            if cancel
                .run_until_cancelled(tokio::time::sleep(backoff))
                .await
                .is_none()
            {
                return Err(ArtworkError::Cancelled);
            }
        }
        match fetch_artwork(http, cancel, &url).await {
            Ok(art) => return Ok(art),
            Err(ArtworkError::Cancelled) => return Err(ArtworkError::Cancelled),
            Err(e) => last = e,
        }
    }
    Err(last)
}

async fn fetch_artwork(
    http: &reqwest::Client,
    cancel: &CancellationToken,
    url: &str,
) -> Result<Artwork, ArtworkError> {
    let fetch = async {
        let mut resp = http.get(url).timeout(ARTWORK_TIMEOUT).send().await?;
        if resp.status() != StatusCode::OK {
            return Err(ArtworkError::Status(resp.status()));
        }
        let png = resp
            .headers()
            .get(CONTENT_TYPE)
            .is_some_and(|v| v == "image/png");
        let mut data = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if data.len() + chunk.len() > MAX_ARTWORK_BYTES {
                return Err(ArtworkError::TooLarge);
            }
            data.extend_from_slice(&chunk);
        }
        Ok(Artwork { data, png })
    };
    cancel
        .run_until_cancelled(fetch)
        .await
        .unwrap_or(Err(ArtworkError::Cancelled))
}

/// Writes the artwork for `meta` next to `beside` (for FFmpeg to embed) and returns the path,
/// or `None` when artwork is off, unknown or failed to download.
pub(crate) async fn artwork_to_temp(
    http: &reqwest::Client,
    cancel: &CancellationToken,
    meta: &TrackMeta,
    size: u32,
    beside: &Path,
) -> Option<PathBuf> {
    if meta.artwork_url.is_empty() {
        return None;
    }
    let art = download_artwork(http, cancel, &meta.artwork_url, size)
        .await
        .ok()?;
    let p = crate::ffmpeg::with_suffix(beside, ".art.jpg");
    tokio::fs::write(&p, &art.data).await.ok()?;
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn artwork_url_substitutes_size() {
        let t = "https://is1-ssl.mzstatic.com/image/thumb/Music1/abc/{w}x{h}bb.jpg";
        assert_eq!(
            artwork_url(t, 3000),
            "https://is1-ssl.mzstatic.com/image/thumb/Music1/abc/3000x3000bb.jpg"
        );
        assert!(artwork_url("https://cdn.example.com/{w}x{h}.jpg", 0).contains("3000x3000"));
    }

    /// Serves one canned response per connection, after `delay`.
    async fn serve(body_len: usize, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {body_len}\r\n\r\n"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&vec![7u8; body_len]).await;
                });
            }
        });
        format!("http://{addr}/{{w}}x{{h}}.png")
    }

    #[tokio::test]
    async fn artwork_download_is_bounded() {
        let http = reqwest::Client::new();
        let cancel = CancellationToken::new();
        let url = serve(MAX_ARTWORK_BYTES + 1024, Duration::ZERO).await;
        let err = fetch_artwork(&http, &cancel, &artwork_url(&url, 100)).await;
        assert!(matches!(err, Err(ArtworkError::TooLarge)), "{err:?}");

        let url = serve(10, Duration::ZERO).await;
        let art = download_artwork(&http, &cancel, &url, 100).await.unwrap();
        assert!(art.png && art.data == [7; 10]);
    }

    #[tokio::test]
    async fn artwork_download_is_cancellable() {
        let http = reqwest::Client::new();
        let url = serve(10, Duration::from_secs(60)).await;
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stopper.cancel();
        });
        let res = tokio::time::timeout(
            Duration::from_secs(3),
            download_artwork(&http, &cancel, &url, 100),
        )
        .await
        .expect("cancelling the job did not stop the artwork download");
        assert!(matches!(res, Err(ArtworkError::Cancelled)));
    }
}

/// Tagging real files made by FFmpeg: every layout the exporter meets, read back by mp4ameta and
/// by FFmpeg, with the audio checked bit for bit. Needs FFmpeg (`FFMPEG=/path/to/ffmpeg`, with
/// `ffprobe` next to it): `cargo test -p rustet-export -- --ignored`.
#[cfg(test)]
mod ffmpeg_tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg() -> PathBuf {
        std::env::var_os("FFMPEG").map_or_else(|| PathBuf::from("ffmpeg"), PathBuf::from)
    }

    fn run(bin: &Path, args: &[&str]) -> String {
        let out = Command::new(bin).args(args).output().expect("run ffmpeg");
        assert!(
            out.status.success(),
            "{bin:?} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn make(dir: &Path, name: &str, codec: &str, layout: &[&str]) -> PathBuf {
        let p = dir.join(name);
        let mut args = vec![
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-c:a",
            codec,
        ];
        args.extend_from_slice(layout);
        args.extend(["-f", "mp4", "-y", p.to_str().unwrap()]);
        run(&ffmpeg(), &args);
        p
    }

    /// The decoded audio's MD5, and that decoding reports no error.
    fn audio_md5(p: &Path) -> String {
        run(
            &ffmpeg(),
            &[
                "-v",
                "error",
                "-xerror",
                "-i",
                p.to_str().unwrap(),
                "-map",
                "0:a",
                "-f",
                "md5",
                "-",
            ],
        )
    }

    fn meta() -> TrackMeta {
        TrackMeta {
            title: "Mr. Brightside".into(),
            artist_name: "The Killers".into(),
            album_artist: "The Killers".into(),
            album_name: "Hot Fuss".into(),
            track_number: 3,
            track_total: 12,
            disc_number: 1,
            disc_total: 2,
            release_date: "2004-06-07".into(),
            genre: "Alternative".into(),
            composer: "Brandon Flowers".into(),
            copyright: "℗ 2004 Island".into(),
            record_label: "Island".into(),
            isrc: "USIR20400274".into(),
            upc: "602498618110".into(),
            content_rating: "explicit".into(),
            ..TrackMeta::default()
        }
    }

    /// A tiny valid PNG (1×1).
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xff, 0xff, 0x3f, 0, 0x05, 0xfe, 0x02, 0xfe, 0xa7,
        0x35, 0x81, 0x84, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn check(p: &Path) {
        let tag = Tag::read_from_path(p).unwrap();
        let text = |ident: Fourcc| tag.strings_of(&ident).next().map(str::to_owned);
        assert_eq!(text(NAME).as_deref(), Some("Mr. Brightside"));
        assert_eq!(text(ARTIST).as_deref(), Some("The Killers"));
        assert_eq!(text(ALBUM_ARTIST).as_deref(), Some("The Killers"));
        assert_eq!(text(ALBUM).as_deref(), Some("Hot Fuss"));
        assert_eq!(text(COMPOSER).as_deref(), Some("Brandon Flowers"));
        assert_eq!(text(COPYRIGHT).as_deref(), Some("℗ 2004 Island"));
        assert_eq!(text(LYRICS).as_deref(), Some("[00:01.00]line"));
        assert_eq!(text(GENRE).as_deref(), Some("Alternative"));
        assert_eq!(text(PUBLISHER).as_deref(), Some("Island"));
        assert_eq!(text(DATE).as_deref(), Some("2004-06-07"));
        assert_eq!(tag.data_of(&RATING).next(), Some(&Data::BeSigned(vec![1])));
        assert_eq!(
            tag.data_of(&TRACK).next(),
            Some(&Data::Reserved(vec![0, 0, 0, 3, 0, 12, 0, 0]))
        );
        assert_eq!(
            tag.data_of(&DISC).next(),
            Some(&Data::Reserved(vec![0, 0, 0, 1, 0, 2]))
        );
        for (name, want) in [
            ("ISRC", "USIR20400274"),
            ("UPC", "602498618110"),
            ("LABEL", "Island"),
            ("RELEASETIME", "2004-06-07"),
            ("PERFORMER", "The Killers"),
        ] {
            let ident = FreeformIdent::new_static(ITUNES, name);
            assert_eq!(tag.strings_of(&ident).next(), Some(want), "{name}");
        }
        assert_eq!(tag.data_of(&COVER).next(), Some(&Data::Png(PNG.to_vec())));

        // FFmpeg reads the tags and the cover too.
        let probe = ffmpeg().with_file_name("ffprobe");
        let json = run(
            &probe,
            &[
                "-v",
                "error",
                "-show_entries",
                "format_tags:stream=codec_type,codec_name",
                "-of",
                "json",
                p.to_str().unwrap(),
            ],
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let tags = &v["format"]["tags"];
        assert_eq!(tags["title"], "Mr. Brightside", "{json}");
        assert_eq!(tags["album_artist"], "The Killers");
        assert_eq!(tags["track"], "3/12");
        assert_eq!(tags["disc"], "1/2");
        assert_eq!(tags["date"], "2004-06-07");
        assert_eq!(tags["ISRC"], "USIR20400274");
        let cover = v["streams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["codec_name"] == "png");
        assert!(cover, "no cover stream: {json}");
    }

    #[test]
    #[ignore = "needs FFmpeg: set FFMPEG"]
    fn tags_every_layout_without_touching_the_audio() {
        let dir = tempfile::tempdir().unwrap();
        let fragmented = ["-movflags", "frag_keyframe+empty_moov+default_base_moof"];
        let no_udta = [
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-map_metadata",
            "-1",
            "-bitexact",
        ];
        let faststart = ["-movflags", "+faststart"];
        for (name, codec, layout) in [
            ("fragmented-alac.m4a", "alac", &fragmented[..]),
            ("fragmented-no-udta.m4a", "alac", &no_udta[..]),
            ("faststart-aac.m4a", "aac", &faststart[..]),
            ("moov-last-alac.m4a", "alac", &[][..]),
        ] {
            let p = make(dir.path(), name, codec, layout);
            let before = audio_md5(&p);
            let art = Artwork {
                data: PNG.to_vec(),
                png: true,
            };
            tag_file(&p, &meta(), Some(art), "[00:01.00]line")
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            check(&p);
            assert_eq!(audio_md5(&p), before, "{name}: audio changed");

            // Tagging again keeps what is not set and adds the new artwork after the old.
            let again = TrackMeta {
                title: "New".into(),
                ..TrackMeta::default()
            };
            let art = Artwork {
                data: vec![0xff, 0xd8, 0xff, 0xe0],
                png: false,
            };
            tag_file(&p, &again, Some(art), "").unwrap();
            let tag = Tag::read_from_path(&p).unwrap();
            assert_eq!(tag.title(), Some("New"));
            assert_eq!(tag.album(), Some("Hot Fuss"), "{name}: merge lost a tag");
            assert_eq!(
                tag.data_of(&TRACK).next(),
                Some(&Data::Reserved(vec![0, 0, 0, 3, 0, 12, 0, 0]))
            );
            assert_eq!(tag.data_of(&COVER).count(), 2, "{name}");
            assert_eq!(audio_md5(&p), before, "{name}: audio changed on retag");
            let leftovers: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.starts_with(".am-tag-"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }
}
