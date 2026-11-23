//! A private D-Bus daemon for tests. Never the user's session bus: the daemon listens on a
//! socket in a temporary directory and has no service activation, so no real service can be
//! started on it.

#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

pub struct PrivateBus {
    pub address: String,
    socket: PathBuf,
    dir: tempfile::TempDir,
    child: Child,
}

impl PrivateBus {
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bus");
        std::fs::write(
            dir.path().join("bus.conf"),
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                socket.display()
            ),
        )
        .unwrap();
        let (child, address) = Self::daemon(&dir);
        Self {
            address,
            socket,
            dir,
            child,
        }
    }

    fn daemon(dir: &tempfile::TempDir) -> (Child, String) {
        let mut child = Command::new("dbus-daemon")
            .arg(format!(
                "--config-file={}",
                dir.path().join("bus.conf").display()
            ))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon on PATH (nix shell nixpkgs#dbus)");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line.trim().to_owned();
        assert!(
            address.contains(&*dir.path().to_string_lossy()),
            "private bus address: {address}"
        );
        (child, address)
    }

    /// The address without the daemon's GUID, as session bus addresses are: it stays valid
    /// across [`PrivateBus::restart`].
    pub fn path_address(&self) -> String {
        format!("unix:path={}", self.socket.display())
    }

    /// Kills the daemon and starts a new one at the same address.
    pub fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        let _ = std::fs::remove_file(&self.socket);
        std::thread::sleep(Duration::from_millis(100));
        (self.child, self.address) = Self::daemon(&self.dir);
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub async fn client(bus: &PrivateBus) -> zbus::Connection {
    zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// Waits until `name` has an owner on the bus.
pub async fn wait_for_name(conn: &zbus::Connection, name: &str, within: Duration) {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let owned = dbus.name_has_owner(name.try_into().unwrap()).await.unwrap();
        if owned {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{name} not registered within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
