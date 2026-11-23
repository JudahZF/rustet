//! The tray item on a private bus. The tray connects to "the session bus", so the check runs in
//! a child process whose `DBUS_SESSION_BUS_ADDRESS` is the private bus. The bus has no
//! StatusNotifierWatcher, so no icon is shown anywhere.
#![cfg(target_os = "linux")]

mod common;

use std::{collections::HashMap, process::Command, time::Duration};

use common::{PrivateBus, wait_for_name};
use rustet_integrations::{
    Transport,
    tray::{Tray, TrayEvent, icon_from_png},
};
use tokio::time::timeout;
use zbus::zvariant::{OwnedValue, Structure, Value};

const ICON: &[u8] = include_bytes!("data/tray-icon.png");
/// Set by the parent test to the private bus address; the child refuses to run without it.
const GUARD: &str = "RUSTET_TEST_PRIVATE_BUS";

#[test]
fn icon_is_argb_and_scaled_down() {
    let icon = icon_from_png(ICON).unwrap();
    assert_eq!(
        (icon.width, icon.height),
        (64, 64),
        "512 px asset scaled by 8"
    );
    assert_eq!(icon.data.len(), 64 * 64 * 4);
    // The asset's corner is transparent and its glyph opaque: alpha comes first (ARGB).
    assert_eq!(icon.data[0], 0);
    assert!(icon.data.as_chunks::<4>().0.iter().any(|p| p[0] == 255));
}

#[test]
fn tray_on_private_bus() {
    let bus = PrivateBus::start();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["tray_child", "--exact", "--ignored", "--nocapture"])
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env(GUARD, &bus.address)
        .status()
        .unwrap();
    assert!(status.success());
}

/// `(width, height, ARGB32 bytes)`.
type Pixmap = (i32, i32, Vec<u8>);

/// `(id, properties, children)` of a dbusmenu layout node.
type Node = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn children(node: &Node) -> Vec<Node> {
    node.2
        .iter()
        .map(|c| {
            let s: Structure = c.try_clone().unwrap().try_into().unwrap();
            let Value::Structure(s) = Value::from(s) else {
                unreachable!()
            };
            s.try_into().unwrap()
        })
        .collect()
}

fn label(node: &Node) -> Option<String> {
    node.1
        .get("label")
        .map(|l| String::try_from(l.try_clone().unwrap()).unwrap())
}

async fn layout(menu: &zbus::Proxy<'_>) -> Vec<(i32, Option<String>)> {
    let (_rev, root): (u32, Node) = menu
        .call("GetLayout", &(0i32, -1i32, Vec::<String>::new()))
        .await
        .unwrap();
    children(&root).iter().map(|c| (c.0, label(c))).collect()
}

#[tokio::test]
#[ignore = "child of tray_on_private_bus"]
async fn tray_child() {
    let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
    assert!(
        std::env::var(GUARD).is_ok_and(|g| g == addr),
        "only runs on the private bus of tray_on_private_bus"
    );

    let (tray, mut events) = Tray::spawn(ICON).await.unwrap();
    let conn = zbus::connection::Builder::address(addr.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    wait_for_name(&conn, &name, Duration::from_secs(10)).await;

    let item = zbus::Proxy::new(
        &conn,
        name.clone(),
        "/StatusNotifierItem",
        "org.kde.StatusNotifierItem",
    )
    .await
    .unwrap();
    assert_eq!(item.get_property::<String>("Id").await.unwrap(), "rustet");
    assert_eq!(
        item.get_property::<String>("Title").await.unwrap(),
        "Rustet"
    );
    let pixmaps: Vec<Pixmap> = item.get_property("IconPixmap").await.unwrap();
    assert_eq!((pixmaps[0].0, pixmaps[0].1), (64, 64));
    let (_, _, tip_title, _): (String, Vec<Pixmap>, String, String) =
        item.get_property("ToolTip").await.unwrap();
    assert_eq!(tip_title, "Rustet");

    // Left click toggles the window.
    let () = item.call("Activate", &(0i32, 0i32)).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap(),
        Some(TrayEvent::ToggleWindow)
    );

    let menu = zbus::Proxy::new(&conn, name, "/MenuBar", "com.canonical.dbusmenu")
        .await
        .unwrap();
    let items = layout(&menu).await;
    let labels: Vec<&str> = items.iter().filter_map(|(_, l)| l.as_deref()).collect();
    assert_eq!(
        labels,
        [
            "Show/Hide Window",
            "Maximise/Restore",
            "Play/Pause",
            "Next",
            "Previous",
            "Mini Player",
            "Restart App",
            "Exit"
        ]
    );
    assert_eq!(items.len(), labels.len() + 3, "three separators");

    let mut click = async |wanted: &str| {
        let id = items
            .iter()
            .find(|(_, l)| l.as_deref() == Some(wanted))
            .unwrap()
            .0;
        let () = menu
            .call("Event", &(id, "clicked", Value::from(0i32), 0u32))
            .await
            .unwrap();
        timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(click("Next").await, TrayEvent::Playback(Transport::Next));
    assert_eq!(
        click("Play/Pause").await,
        TrayEvent::Playback(Transport::PlayPause)
    );
    assert_eq!(click("Maximise/Restore").await, TrayEvent::ToggleMaximize);
    assert_eq!(click("Mini Player").await, TrayEvent::ToggleMiniPlayer);
    assert_eq!(click("Restart App").await, TrayEvent::Restart);
    assert_eq!(click("Exit").await, TrayEvent::Quit);

    tray.set_mini_player_open(true).await;
    let labels = layout(&menu).await;
    assert!(
        labels
            .iter()
            .any(|(_, l)| l.as_deref() == Some("Hide Mini Player"))
    );

    tray.shutdown().await;
}
