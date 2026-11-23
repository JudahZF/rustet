//! Discord Rich Presence against a fake Discord IPC server on a Unix socket in a temp dir.
#![cfg(unix)]

use std::time::Duration;

use rustet_integrations::discord::{CLIENT_ID, Discord, PresenceUpdate, encode_frame};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    time::timeout,
};

const WAIT: Duration = Duration::from_secs(10);

async fn read_frame(s: &mut UnixStream) -> (i32, Value) {
    let mut head = [0u8; 8];
    timeout(WAIT, s.read_exact(&mut head))
        .await
        .expect("frame in time")
        .unwrap();
    let op = i32::from_le_bytes(head[..4].try_into().unwrap());
    let len = i32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
    let mut body = vec![0; len];
    s.read_exact(&mut body).await.unwrap();
    (op, serde_json::from_slice(&body).unwrap())
}

async fn accept(l: &UnixListener) -> UnixStream {
    timeout(WAIT, l.accept())
        .await
        .expect("connection in time")
        .unwrap()
        .0
}

/// Accepts, checks the handshake and answers READY.
async fn handshake(l: &UnixListener) -> UnixStream {
    let mut s = accept(l).await;
    let (op, hello) = read_frame(&mut s).await;
    assert_eq!(op, 0);
    assert_eq!(hello, json!({ "v": 1, "client_id": CLIENT_ID }));
    // READY split across two writes, to exercise the frame buffer.
    let ready = encode_frame(1, &json!({ "cmd": "DISPATCH", "evt": "READY", "data": {} }));
    s.write_all(&ready[..5]).await.unwrap();
    s.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    s.write_all(&ready[5..]).await.unwrap();
    s
}

fn track(name: &str, playing: bool) -> PresenceUpdate {
    serde_json::from_value(json!({
        "name": name, "artist": "Artist", "album": "Album",
        "artworkUrl": "https://img/a.jpg", "playing": playing,
        "startedAtMs": 1_700_000_000_000u64, "endsAtMs": 1_700_000_200_000u64,
        "appleUrl": "https://music.apple.com/us/song/s/1", "isVideo": false,
    }))
    .unwrap()
}

#[tokio::test]
async fn presence_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("discord-ipc-1");
    let listener = UnixListener::bind(&sock).unwrap();
    // The first path has no server: the next one is tried.
    let discord = Discord::spawn(vec![dir.path().join("discord-ipc-0"), sock.clone()]);

    // Idle until the first update; the activity waits for READY.
    discord.update(track("First", true));
    let mut s = handshake(&listener).await;
    let (op, set) = read_frame(&mut s).await;
    assert_eq!(op, 1);
    assert_eq!(set["cmd"], "SET_ACTIVITY");
    assert_eq!(set["args"]["pid"], std::process::id());
    assert!(set["nonce"].is_string());
    let a = &set["args"]["activity"];
    assert_eq!(a["details"], "First");
    assert_eq!(a["type"], 2);
    assert_eq!(
        a["timestamps"],
        json!({ "start": 1_700_000_000, "end": 1_700_000_200 })
    );
    assert_eq!(a["assets"]["small_image"], "play");

    // Updates go straight out once ready.
    discord.update(track("Second", false));
    let (_, set) = read_frame(&mut s).await;
    assert_eq!(set["args"]["activity"]["details"], "Second");
    assert!(set["args"]["activity"]["timestamps"].is_null());

    discord.clear();
    let (_, set) = read_frame(&mut s).await;
    assert!(set["args"]["activity"].is_null());

    // Discord goes away: reconnect after 5 s, and the pending activity is sent again on READY.
    discord.update(track("Third", true));
    let _ = read_frame(&mut s).await;
    drop(s);
    let started = tokio::time::Instant::now();
    let mut s = handshake(&listener).await;
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "reconnect waits ~5 s"
    );
    let (_, set) = read_frame(&mut s).await;
    assert_eq!(set["args"]["activity"]["details"], "Third");

    // Disable: clear, then disconnect and stay disconnected.
    discord.disable();
    let (_, set) = read_frame(&mut s).await;
    assert!(set["args"]["activity"].is_null());
    let mut rest = Vec::new();
    timeout(WAIT, s.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert!(rest.is_empty(), "socket closed after the clear");
    assert!(
        timeout(Duration::from_secs(6), listener.accept())
            .await
            .is_err(),
        "no reconnect when disabled"
    );
}

#[tokio::test]
async fn no_discord_running_is_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let discord = Discord::spawn(vec![dir.path().join("discord-ipc-0")]);
    discord.update(track("x", true));
    discord.clear();
    discord.disable();
    tokio::time::sleep(Duration::from_millis(200)).await; // let the service try and give up
    // A server that appears later is used by the next update.
    let listener = UnixListener::bind(dir.path().join("discord-ipc-0")).unwrap();
    discord.update(track("Later", true));
    let mut s = handshake(&listener).await;
    let (_, set) = read_frame(&mut s).await;
    assert_eq!(set["args"]["activity"]["details"], "Later");
}
