//! Discord Rich Presence over Discord's local IPC: framed JSON on the `discord-ipc-N` Unix
//! socket (a named pipe on Windows). Shows the current track as the user's Discord status.
//!
//! Frames are `[opcode: i32 LE][length: i32 LE][JSON]`. The client sends a handshake (op 0),
//! waits for the `READY` dispatch, then sets the activity with `SET_ACTIVITY` (op 1).
//!
//! Lifecycle (as in the Electron shell): the first `discord:update` enables presence and
//! connects. A connection that closes while presence is enabled is retried after 5 s; a failed
//! connect is retried by the next update (the page sends one every 10 s). `discord:disable`
//! clears the activity and closes the socket.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    time::Instant,
};

/// The Discord application (client) ID of Rustet. The `play` and `pause` small-image assets are
/// uploaded under this application (see `electron/discord-assets/`).
pub const CLIENT_ID: &str = "1545304879441121320";

const OP_HANDSHAKE: i32 = 0;
const OP_FRAME: i32 = 1;
/// A frame longer than this is treated as a desynchronised stream.
const MAX_FRAME: usize = 1 << 24;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Discord's limit for the text fields of an activity.
const TEXT_MAX: usize = 128;

/// Discord IPC endpoints, in the order they are tried: named pipes on Windows; elsewhere
/// `discord-ipc-0..9` in the runtime and temp directories, including the Snap and Flatpak
/// sub-directories.
pub fn ipc_paths(windows: bool, env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    if windows {
        return (0..10)
            .map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}")))
            .collect();
    }
    let bases = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .filter_map(|k| env(k).filter(|v| !v.is_empty()))
        .chain(std::iter::once("/tmp".to_owned()));
    let mut paths = Vec::new();
    for base in bases {
        for sub in [
            "",
            "snap.discord",
            "app/com.discordapp.Discord",
            "app/com.discordapp.DiscordCanary",
        ] {
            for i in 0..10 {
                paths.push(Path::new(&base).join(sub).join(format!("discord-ipc-{i}")));
            }
        }
    }
    paths
}

/// `[op LE][len LE][json]`.
pub fn encode_frame(op: i32, payload: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(payload).expect("JSON values serialise");
    let mut frame = Vec::with_capacity(8 + json.len());
    frame.extend_from_slice(&op.to_le_bytes());
    frame.extend_from_slice(&(json.len() as i32).to_le_bytes());
    frame.extend_from_slice(&json);
    frame
}

/// Splits complete frames off the front of `buf` and returns their payloads. A negative or
/// oversized length drops the buffered bytes, as the Electron parser did.
fn take_frames(buf: &mut Vec<u8>) -> Vec<Value> {
    let mut out = Vec::new();
    while buf.len() >= 8 {
        let len = i32::from_le_bytes(buf[4..8].try_into().expect("4 bytes"));
        let Ok(len) = usize::try_from(len)
            .map_err(drop)
            .and_then(|l| if l > MAX_FRAME { Err(()) } else { Ok(l) })
        else {
            buf.clear();
            break;
        };
        if buf.len() < 8 + len {
            break;
        }
        if let Ok(v) = serde_json::from_slice(&buf[8..8 + len]) {
            out.push(v);
        }
        buf.drain(..8 + len);
    }
    out
}

/// The `discord:update` payload from the page.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PresenceUpdate {
    pub name: String,
    pub artist: String,
    pub album: String,
    pub artwork_url: String,
    pub playing: bool,
    pub started_at_ms: f64,
    pub ends_at_ms: f64,
    pub apple_url: String,
    pub is_video: bool,
}

#[derive(Serialize)]
struct Assets {
    large_image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    large_text: Option<String>,
    small_image: &'static str,
    small_text: &'static str,
}

#[derive(Serialize)]
struct Timestamps {
    start: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<i64>,
}

#[derive(Serialize)]
struct Button {
    label: &'static str,
    url: String,
}

/// A "Listening to …" activity.
#[derive(Serialize)]
struct Activity {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assets: Option<Assets>,
    /// Always present: `null` makes Discord drop a running timer; an absent key keeps it.
    timestamps: Option<Timestamps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    buttons: Option<[Button; 1]>,
}

/// At most [`TEXT_MAX`] characters, `None` when empty.
fn text(s: &str) -> Option<String> {
    Some(s.chars().take(TEXT_MAX).collect::<String>()).filter(|s| !s.is_empty())
}

/// JavaScript `Math.round(ms / 1000)`.
fn unix_secs(ms: f64) -> i64 {
    (ms / 1000.0 + 0.5).floor() as i64
}

impl PresenceUpdate {
    /// The activity JSON sent with `SET_ACTIVITY`.
    pub fn activity(&self) -> Value {
        let assets = (!self.artwork_url.is_empty()).then(|| Assets {
            large_image: self.artwork_url.clone(),
            large_text: text(&self.album),
            small_image: if self.playing { "play" } else { "pause" },
            small_text: if self.playing { "Playing" } else { "Paused" },
        });
        let ts = |ms: f64| (ms.is_finite() && ms != 0.0).then(|| unix_secs(ms));
        let timestamps = if self.playing {
            ts(self.started_at_ms).map(|start| Timestamps {
                start,
                end: ts(self.ends_at_ms),
            })
        } else {
            None
        };
        let buttons = (!self.apple_url.is_empty()).then(|| {
            [Button {
                label: if self.is_video {
                    "Open Music Video"
                } else {
                    "Open in Apple Music"
                },
                url: self.apple_url.clone(),
            }]
        });
        let activity = Activity {
            kind: 2,
            details: text(&self.name),
            state: text(&self.artist),
            assets,
            timestamps,
            buttons,
        };
        serde_json::to_value(activity).expect("activity serialises")
    }
}

enum Msg {
    Update(Box<PresenceUpdate>),
    Clear,
    Disable,
}

/// Handle to the presence service. Dropping it stops the service and closes the socket.
pub struct Discord {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Discord {
    /// Starts the service. It stays idle until the first [`Discord::update`].
    /// `paths` are the IPC endpoints to try ([`ipc_paths`] for the real ones).
    pub fn spawn(paths: Vec<PathBuf>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Service::new(paths).run(rx));
        Self { tx }
    }

    /// `discord:update`: enable presence and show this track.
    pub fn update(&self, update: PresenceUpdate) {
        let _ = self.tx.send(Msg::Update(Box::new(update)));
    }

    /// `discord:clear`: remove the activity, keep the connection.
    pub fn clear(&self) {
        let _ = self.tx.send(Msg::Clear);
    }

    /// `discord:disable`: remove the activity and disconnect until the next update.
    pub fn disable(&self) {
        let _ = self.tx.send(Msg::Disable);
    }
}

#[cfg(unix)]
type Stream = tokio::net::UnixStream;

#[cfg(unix)]
async fn connect(path: &Path) -> std::io::Result<Stream> {
    Stream::connect(path).await
}

// Windows: Discord listens on named pipes (`ipc_paths(true, …)`). Connecting to them
// (`tokio::net::windows::named_pipe::ClientOptions`) is not written yet: it cannot be built
// or tested on the Linux machine this port was made on.
#[cfg(not(unix))]
type Stream = tokio::io::DuplexStream;

#[cfg(not(unix))]
async fn connect(_path: &Path) -> std::io::Result<Stream> {
    Err(std::io::ErrorKind::Unsupported.into())
}

enum Wake {
    Msg(Option<Msg>),
    Read(std::io::Result<usize>),
    Reconnect,
}

/// Reads from the connection, or waits forever when there is none.
async fn read_some(conn: &mut Option<Connection>, buf: &mut [u8]) -> std::io::Result<usize> {
    match conn {
        Some(c) => c.stream.read(buf).await,
        None => std::future::pending().await,
    }
}

struct Connection {
    stream: Stream,
    buf: Vec<u8>,
    ready: bool,
}

struct Service {
    paths: Vec<PathBuf>,
    enabled: bool,
    pending: Option<Value>,
    conn: Option<Connection>,
    reconnect_at: Option<Instant>,
    nonce: u64,
}

impl Service {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            enabled: false,
            pending: None,
            conn: None,
            reconnect_at: None,
            nonce: 0,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut chunk = [0u8; 4096];
        loop {
            let reconnect_at = self.reconnect_at;
            let wake = tokio::select! {
                msg = rx.recv() => Wake::Msg(msg),
                read = read_some(&mut self.conn, &mut chunk) => Wake::Read(read),
                () = tokio::time::sleep_until(reconnect_at.unwrap_or_else(Instant::now)),
                    if reconnect_at.is_some() => Wake::Reconnect,
            };
            match wake {
                Wake::Msg(None) => break,
                Wake::Msg(Some(Msg::Update(u))) => {
                    self.enabled = true;
                    let activity = u.activity();
                    self.pending = Some(activity.clone());
                    match &self.conn {
                        None => self.connect().await,
                        Some(c) if c.ready => self.set_activity(Some(activity)).await,
                        Some(_) => {} // sent on READY
                    }
                }
                Wake::Msg(Some(Msg::Clear)) => {
                    self.pending = None;
                    self.set_activity(None).await;
                }
                Wake::Msg(Some(Msg::Disable)) => {
                    self.enabled = false;
                    self.pending = None;
                    self.set_activity(None).await;
                    self.reconnect_at = None;
                    self.conn = None;
                }
                Wake::Read(Ok(n)) if n > 0 => self.on_data(&chunk[..n]).await,
                Wake::Read(_) => self.on_close(),
                Wake::Reconnect => {
                    self.reconnect_at = None;
                    self.connect().await;
                }
            }
        }
        // Discord drops the activity itself when the socket closes.
    }

    /// Tries each endpoint in turn and sends the handshake on the first that accepts.
    async fn connect(&mut self) {
        if self.conn.is_some() {
            return;
        }
        for path in &self.paths {
            let Ok(mut stream) = connect(path).await else {
                continue;
            };
            let hello = encode_frame(OP_HANDSHAKE, &json!({ "v": 1, "client_id": CLIENT_ID }));
            if stream.write_all(&hello).await.is_err() {
                continue;
            }
            tracing::debug!("Discord IPC connected at {}", path.display());
            self.conn = Some(Connection {
                stream,
                buf: Vec::new(),
                ready: false,
            });
            return;
        }
    }

    async fn on_data(&mut self, data: &[u8]) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        conn.buf.extend_from_slice(data);
        let frames = take_frames(&mut conn.buf);
        if frames
            .iter()
            .any(|f| f.get("evt").and_then(Value::as_str) == Some("READY"))
        {
            conn.ready = true;
            if let Some(activity) = self.pending.clone() {
                self.set_activity(Some(activity)).await;
            }
        }
    }

    fn on_close(&mut self) {
        self.conn = None;
        if self.enabled && self.reconnect_at.is_none() {
            self.reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
        }
    }

    /// `SET_ACTIVITY` with an activity, or `null` to clear it. Only once `READY`.
    async fn set_activity(&mut self, activity: Option<Value>) {
        let Some(conn) = self.conn.as_mut().filter(|c| c.ready) else {
            return;
        };
        self.nonce += 1;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let frame = encode_frame(
            OP_FRAME,
            &json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": std::process::id(), "activity": activity },
                "nonce": format!("{millis}-{}", self.nonce),
            }),
        );
        if conn.stream.write_all(&frame).await.is_err() {
            self.on_close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_named_pipes_on_windows_sockets_elsewhere() {
        let w = ipc_paths(true, |_| None);
        assert_eq!(w.len(), 10);
        assert_eq!(w[0], PathBuf::from(r"\\.\pipe\discord-ipc-0"));
        let env = |k: &str| (k == "XDG_RUNTIME_DIR").then(|| "/run/user/1000".to_owned());
        let l = ipc_paths(false, env);
        assert_eq!(l[0], PathBuf::from("/run/user/1000/discord-ipc-0"));
        assert!(l.contains(&PathBuf::from("/run/user/1000/snap.discord/discord-ipc-3")));
        assert!(l.contains(&PathBuf::from(
            "/run/user/1000/app/com.discordapp.Discord/discord-ipc-9"
        )));
        assert!(l.contains(&PathBuf::from(
            "/tmp/app/com.discordapp.DiscordCanary/discord-ipc-0"
        )));
        assert_eq!(l.len(), 2 * 4 * 10, "runtime dir and /tmp");
    }

    #[test]
    fn frame_is_op_len_json() {
        let payload = json!({ "v": 1, "client_id": "123" });
        let frame = encode_frame(0, &payload);
        assert_eq!(i32::from_le_bytes(frame[0..4].try_into().unwrap()), 0);
        let len = i32::from_le_bytes(frame[4..8].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 8);
        assert_eq!(
            serde_json::from_slice::<Value>(&frame[8..]).unwrap(),
            payload
        );
    }

    #[test]
    fn frames_are_split_and_bad_lengths_drop_the_buffer() {
        let a = encode_frame(1, &json!({ "evt": "READY" }));
        let b = encode_frame(1, &json!({ "n": 2 }));
        let mut buf = [a.clone(), b[..5].to_vec()].concat();
        assert_eq!(take_frames(&mut buf), vec![json!({ "evt": "READY" })]);
        buf.extend_from_slice(&b[5..]);
        assert_eq!(take_frames(&mut buf), vec![json!({ "n": 2 })]);
        assert!(buf.is_empty());

        let mut bad = [1i32.to_le_bytes(), (-1i32).to_le_bytes()].concat();
        bad.extend_from_slice(&a);
        assert!(take_frames(&mut bad).is_empty());
        assert!(bad.is_empty());
    }

    #[test]
    fn activity_fields() {
        let u = PresenceUpdate {
            name: "x".repeat(200),
            artist: "Artist".into(),
            album: String::new(),
            artwork_url: "https://img/a.jpg".into(),
            playing: true,
            started_at_ms: 1_700_000_000_400.0,
            ends_at_ms: 1_700_000_180_600.0,
            apple_url: "https://music.apple.com/us/song/x/1".into(),
            is_video: false,
        };
        let a = u.activity();
        assert_eq!(a["type"], 2);
        assert_eq!(a["details"].as_str().unwrap().chars().count(), 128);
        assert_eq!(a["state"], "Artist");
        assert_eq!(
            a["assets"],
            json!({ "large_image": "https://img/a.jpg", "small_image": "play", "small_text": "Playing" })
        );
        assert_eq!(
            a["timestamps"],
            json!({ "start": 1_700_000_000, "end": 1_700_000_181 })
        );
        assert_eq!(
            a["buttons"],
            json!([{ "label": "Open in Apple Music", "url": "https://music.apple.com/us/song/x/1" }])
        );

        let paused = PresenceUpdate {
            playing: false,
            is_video: true,
            artwork_url: String::new(),
            name: String::new(),
            ..u
        };
        let a = paused.activity();
        assert!(a.get("details").is_none() && a.get("assets").is_none());
        assert!(a["timestamps"].is_null(), "explicit null clears the timer");
        assert_eq!(a["buttons"][0]["label"], "Open Music Video");
    }
}
