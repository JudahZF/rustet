//! MPRIS against a private D-Bus daemon, read and driven by a client the way a desktop widget
//! does it.
#![cfg(target_os = "linux")]

mod common;

use std::{collections::HashMap, time::Duration};

use common::{PrivateBus, client, wait_for_name};
use futures::StreamExt;
use rustet_integrations::{
    Action, LoopStatus, Transport,
    mpris::{BUS_NAME, Bus, Mpris, MprisEvent},
    now_playing::MprisUpdate,
};
use serde_json::json;
use tokio::{sync::mpsc, time::timeout};
use zbus::{
    fdo::PropertiesProxy,
    names::InterfaceName,
    zvariant::{ObjectPath, OwnedValue, Value},
};

const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const WAIT: Duration = Duration::from_secs(10);

async fn props(conn: &zbus::Connection) -> PropertiesProxy<'static> {
    PropertiesProxy::builder(conn)
        .destination(BUS_NAME)
        .unwrap()
        .path(PATH)
        .unwrap()
        .build()
        .await
        .unwrap()
}

async fn get(p: &PropertiesProxy<'_>, iface: &'static str, name: &str) -> OwnedValue {
    p.get(InterfaceName::from_static_str(iface).unwrap(), name)
        .await
        .unwrap()
}

async fn player(conn: &zbus::Connection) -> zbus::Proxy<'static> {
    zbus::Proxy::new(conn, BUS_NAME, PATH, PLAYER)
        .await
        .unwrap()
}

async fn next_event(rx: &mut mpsc::UnboundedReceiver<MprisEvent>) -> MprisEvent {
    timeout(WAIT, rx.recv())
        .await
        .expect("event in time")
        .expect("service running")
}

fn update(v: serde_json::Value) -> MprisUpdate {
    serde_json::from_value(v).unwrap()
}

const TRACK: &str = "/com/apple/music/track/1440857781";

fn track_update() -> MprisUpdate {
    update(json!({
        "status": "Playing",
        "position": 5_000_000,
        "metadata": {
            "mpris:trackid": TRACK,
            "mpris:length": 215_000_000,
            "mpris:artUrl": "https://is1-ssl.mzstatic.com/image/1000x1000bb.jpg",
            "xesam:title": "Song",
            "xesam:artist": ["Artist"],
            "xesam:album": "Album",
            "xesam:trackNumber": 4,
        },
    }))
}

/// `Position` is `at` or up to two seconds later: a playing track runs on the clock from the
/// last update, because clients read the position at any time.
async fn position_near(p: &zbus::fdo::PropertiesProxy<'_>, at: i64) {
    let position = i64::try_from(get(p, PLAYER, "Position").await).unwrap();
    assert!((at..at + 2_000_000).contains(&position), "{position}");
}

#[tokio::test]
async fn registers_reports_and_controls() {
    let bus = PrivateBus::start();
    let conn = client(&bus).await;
    let (mpris, mut events) = Mpris::spawn(Bus::Address(bus.address.clone()));

    // Registers on its own after the start delay, before any update.
    wait_for_name(&conn, BUS_NAME, WAIT).await;
    let p = props(&conn).await;

    // org.mpris.MediaPlayer2
    let root = p
        .get_all(InterfaceName::from_static_str(ROOT).unwrap())
        .await
        .unwrap();
    let s = |k: &str| String::try_from(root[k].clone()).unwrap();
    let b = |k: &str| bool::try_from(&root[k]).unwrap();
    assert_eq!(s("Identity"), "Rustet");
    assert_eq!(s("DesktopEntry"), "rustet");
    assert!(b("CanQuit") && b("CanRaise"));
    assert!(!b("CanSetFullscreen") && !b("HasTrackList") && !b("Fullscreen"));
    assert_eq!(
        Vec::<String>::try_from(root["SupportedMimeTypes"].clone()).unwrap(),
        ["audio/mpeg", "audio/flac"]
    );
    assert!(
        Vec::<String>::try_from(root["SupportedUriSchemes"].clone())
            .unwrap()
            .is_empty()
    );

    // org.mpris.MediaPlayer2.Player defaults.
    let all = p
        .get_all(InterfaceName::from_static_str(PLAYER).unwrap())
        .await
        .unwrap();
    assert_eq!(
        String::try_from(all["PlaybackStatus"].clone()).unwrap(),
        "Stopped"
    );
    assert_eq!(String::try_from(all["LoopStatus"].clone()).unwrap(), "None");
    for cap in [
        "CanControl",
        "CanPlay",
        "CanPause",
        "CanSeek",
        "CanGoNext",
        "CanGoPrevious",
    ] {
        assert!(bool::try_from(&all[cap]).unwrap(), "{cap}");
    }
    for rate in ["Rate", "MinimumRate", "MaximumRate"] {
        assert_eq!(f64::try_from(&all[rate]).unwrap(), 1.0, "{rate}");
    }

    // An update is announced in one PropertiesChanged; Position is not announced.
    let mut changes = p.receive_properties_changed().await.unwrap();
    mpris.update(track_update());
    let sig = timeout(WAIT, changes.next()).await.unwrap().unwrap();
    let args = sig.args().unwrap();
    assert_eq!(args.interface_name.as_str(), PLAYER);
    let mut keys: Vec<&str> = args.changed_properties.keys().copied().collect();
    keys.sort_unstable();
    assert_eq!(keys, ["Metadata", "PlaybackStatus"]);

    assert_eq!(
        String::try_from(get(&p, PLAYER, "PlaybackStatus").await).unwrap(),
        "Playing"
    );
    position_near(&p, 5_000_000).await;
    let meta: HashMap<String, OwnedValue> = get(&p, PLAYER, "Metadata").await.try_into().unwrap();
    assert_eq!(
        ObjectPath::try_from(meta["mpris:trackid"].clone())
            .unwrap()
            .as_str(),
        TRACK
    );
    assert_eq!(i64::try_from(&meta["mpris:length"]).unwrap(), 215_000_000);
    assert_eq!(i32::try_from(&meta["xesam:trackNumber"]).unwrap(), 4);
    assert_eq!(
        Vec::<String>::try_from(meta["xesam:artist"].clone()).unwrap(),
        ["Artist"]
    );
    assert_eq!(meta["mpris:artUrl"].value_signature().to_string(), "s");

    // Unchanged fields are not announced again; changed ones are.
    mpris.update(update(
        json!({ "status": "Playing", "shuffle": true, "repeat": 1, "volume": 40 }),
    ));
    let sig = timeout(WAIT, changes.next()).await.unwrap().unwrap();
    let args = sig.args().unwrap();
    let mut keys: Vec<&str> = args.changed_properties.keys().copied().collect();
    keys.sort_unstable();
    assert_eq!(keys, ["LoopStatus", "Shuffle", "Volume"]);
    assert_eq!(args.changed_properties["LoopStatus"], Value::from("Track"));
    assert_eq!(args.changed_properties["Volume"], Value::from(0.4));

    // A position tick changes Position silently; `seeked` emits Seeked with it.
    let pl = player(&conn).await;
    let mut seeked = pl.receive_signal("Seeked").await.unwrap();
    mpris.update(update(json!({ "position": 7_000_000 })));
    mpris.update(update(json!({ "position": 9_000_000, "seeked": true })));
    let msg = timeout(WAIT, seeked.next()).await.unwrap().unwrap();
    assert_eq!(msg.body().deserialize::<i64>().unwrap(), 9_000_000);
    position_near(&p, 9_000_000).await;

    // Methods become page commands.
    for (method, cmd) in [
        ("Play", Transport::Play),
        ("Pause", Transport::Pause),
        ("PlayPause", Transport::PlayPause),
        ("Next", Transport::Next),
        ("Previous", Transport::Previous),
        ("Stop", Transport::Pause),
    ] {
        let () = pl.call(method, &()).await.unwrap();
        assert_eq!(
            next_event(&mut events).await,
            MprisEvent::Command(cmd.into()),
            "{method}"
        );
    }
    let () = pl.call("Seek", &(-5_000_000i64)).await.unwrap();
    assert_eq!(
        next_event(&mut events).await,
        MprisEvent::Command(Action::Seek { delta_ms: -5000.0 }.into())
    );

    // SetPosition: only for the current track and within its length (spec).
    let track = ObjectPath::try_from(TRACK).unwrap();
    let other = ObjectPath::try_from("/com/apple/music/track/2").unwrap();
    let () = pl
        .call("SetPosition", &(&other, 10_000_000i64))
        .await
        .unwrap();
    let () = pl
        .call("SetPosition", &(&track, 300_000_000i64))
        .await
        .unwrap();
    let () = pl.call("SetPosition", &(&track, -1i64)).await.unwrap();
    let () = pl
        .call("SetPosition", &(&track, 10_000_000i64))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        MprisEvent::Command(Action::SetPosition { ms: 10_000.0 }.into())
    );

    // Writable properties.
    let iface = InterfaceName::from_static_str(PLAYER).unwrap();
    p.set(iface.clone(), "Volume", Value::from(0.42))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        MprisEvent::Command(Action::SetVolume { value: 42.0 }.into())
    );
    p.set(iface.clone(), "Volume", Value::from(-3.0))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        MprisEvent::Command(Action::SetVolume { value: 0.0 }.into())
    );
    p.set(iface.clone(), "Shuffle", Value::from(false))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        MprisEvent::Command(Action::Shuffle { value: false }.into())
    );
    p.set(iface.clone(), "LoopStatus", Value::from("Playlist"))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        MprisEvent::Command(
            Action::SetLoopStatus {
                value: LoopStatus::Playlist
            }
            .into()
        )
    );
    assert!(
        p.set(iface.clone(), "LoopStatus", Value::from("Forever"))
            .await
            .is_err()
    );
    p.set(iface.clone(), "Rate", Value::from(2.0))
        .await
        .unwrap();
    assert_eq!(f64::try_from(get(&p, PLAYER, "Rate").await).unwrap(), 1.0);

    let () = pl.call("OpenUri", &("rustet://song/1",)).await.unwrap();
    assert_eq!(
        next_event(&mut events).await,
        MprisEvent::OpenUri("rustet://song/1".into())
    );
    let rootp = zbus::Proxy::new(&conn, BUS_NAME, PATH, ROOT).await.unwrap();
    let () = rootp.call("Raise", &()).await.unwrap();
    assert_eq!(next_event(&mut events).await, MprisEvent::Raise);
    let () = rootp.call("Quit", &()).await.unwrap();
    assert_eq!(next_event(&mut events).await, MprisEvent::Quit);

    // No command was produced by the rejected calls.
    assert!(events.try_recv().is_err());

    // Stop releases the name.
    mpris.stop().await;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
    assert!(
        !dbus
            .name_has_owner(BUS_NAME.try_into().unwrap())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn reconnects_with_the_last_state_after_the_bus_restarts() {
    let mut bus = PrivateBus::start();
    let (mpris, _events) = Mpris::spawn(Bus::Address(bus.path_address()));
    // The first update registers at once, without waiting for the start delay.
    mpris.update(track_update());
    mpris.update(update(json!({ "shuffle": true })));
    {
        let conn = client(&bus).await;
        wait_for_name(&conn, BUS_NAME, Duration::from_millis(1000)).await;
    }

    // The bus goes away while paused; the update in between is kept.
    bus.restart();
    mpris.update(update(json!({ "status": "Paused" })));
    let conn = client(&bus).await;
    // First retry after 2 s.
    wait_for_name(&conn, BUS_NAME, WAIT).await;
    let p = props(&conn).await;
    assert_eq!(
        String::try_from(get(&p, PLAYER, "PlaybackStatus").await).unwrap(),
        "Paused"
    );
    assert!(bool::try_from(get(&p, PLAYER, "Shuffle").await).unwrap());
    position_near(&p, 5_000_000).await;
    let meta: HashMap<String, OwnedValue> = get(&p, PLAYER, "Metadata").await.try_into().unwrap();
    assert_eq!(
        String::try_from(meta["xesam:title"].clone()).unwrap(),
        "Song"
    );
}

#[tokio::test]
async fn a_second_instance_takes_the_instance_name() {
    let bus = PrivateBus::start();
    let conn = client(&bus).await;
    let (first, _e1) = Mpris::spawn(Bus::Address(bus.address.clone()));
    first.update(MprisUpdate::default());
    wait_for_name(&conn, BUS_NAME, WAIT).await;
    let (second, _e2) = Mpris::spawn(Bus::Address(bus.address.clone()));
    second.update(MprisUpdate::default());
    wait_for_name(
        &conn,
        &format!("{BUS_NAME}.instance{}", std::process::id()),
        WAIT,
    )
    .await;
}

#[tokio::test]
async fn no_bus_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mpris, mut events) = Mpris::spawn(Bus::Address(format!(
        "unix:path={}",
        dir.path().join("none").display()
    )));
    mpris.update(track_update());
    // The service keeps running and answers stop.
    assert!(events.try_recv().is_err());
    timeout(WAIT, mpris.stop()).await.unwrap();
}
