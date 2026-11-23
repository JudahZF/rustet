//! Media key grabs on a virtual X server (Xvfb), pressed through XTEST. Never the user's display.
#![cfg(target_os = "linux")]

use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    time::Duration,
};

use rustet_integrations::{Transport, media_keys::MediaKeys};
use x11rb::{
    connection::Connection,
    protocol::{xproto::ConnectionExt as _, xtest::ConnectionExt as _},
};

struct Xvfb(std::process::Child, String);

impl Xvfb {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-nolisten",
                "tcp",
                "-screen",
                "0",
                "64x64x24",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("Xvfb on PATH (nix shell nixpkgs#xorg.xvfb)");
        let mut n = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut n)
            .unwrap();
        Self(child, format!(":{}", n.trim()))
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "needs Xvfb: nix shell nixpkgs#xorg.xvfb -c cargo test -p rustet-integrations --test media_keys -- --ignored"]
async fn media_keys_are_grabbed_and_reported() {
    let x = Xvfb::start();
    let (keys, mut rx) = MediaKeys::spawn(Some(&x.1)).unwrap();
    // A second grabber finds the keys taken and still starts.
    let (other, _other_rx) = MediaKeys::spawn(Some(&x.1)).unwrap();

    let (conn, _) = x11rb::connect(Some(&x.1)).unwrap();
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn
        .get_keyboard_mapping(min, max - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let per = usize::from(map.keysyms_per_keycode);
    let code_of = |sym: u32| {
        map.keysyms
            .chunks(per)
            .zip(min..=max)
            .find(|(s, _)| s.contains(&sym))
            .map(|(_, c)| c)
            .unwrap()
    };
    let press = |code: u8| {
        conn.xtest_fake_input(2, code, 0, x11rb::NONE, 0, 0, 0)
            .unwrap(); // KeyPress
        conn.xtest_fake_input(3, code, 0, x11rb::NONE, 0, 0, 0)
            .unwrap(); // KeyRelease
        conn.flush().unwrap();
    };
    for (sym, cmd) in [
        (0x1008_FF14, Transport::PlayPause),
        (0x1008_FF17, Transport::Next),
        (0x1008_FF16, Transport::Previous),
        (0x1008_FF15, Transport::Pause),
    ] {
        press(code_of(sym));
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert_eq!(got, Some(cmd));
    }
    drop(other);
    // Dropping the handle stops the listener thread and releases the grabs.
    drop(keys);
    assert_eq!(rx.recv().await, None);
}
