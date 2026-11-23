//! MPRIS2 (`org.mpris.MediaPlayer2.rustet` on the session bus): now-playing for taskbars, lock
//! screens, KDE Connect and the desktop's media keys, and control requests back to the page.
//!
//! Lifecycle, as in the Electron shell:
//! - The first connection is tried 1.5 s after start, or at the first update if that is sooner.
//! - With no session bus (checked on every attempt, see [`has_session_bus`]) nothing is retried
//!   until the next update.
//! - A failed or lost connection is retried after [`retry_delay`] (2 s doubling to 60 s); updates
//!   do not reconnect sooner. A connection that lives 10 s resets the back-off.
//! - The last state is kept across reconnects and is what a new connection starts with.
//! - `Position` is answered from the last update and never announced; the page asks for
//!   `Seeked` (`seeked: true`) after a seek and on resume.

mod iface;
mod metadata;

use std::{borrow::Cow, collections::HashMap, path::Path, time::Duration};

use futures::StreamExt;
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, sleep_until, timeout},
};
use zbus::{
    MessageStream, fdo,
    fdo::{RequestNameFlags, RequestNameReply},
    names::InterfaceName,
    object_server::InterfaceRef,
    zvariant::Value as DValue,
};

use self::iface::{Metadata, Player, PlayerState, Root};
use crate::{command::PageCommand, now_playing::MprisUpdate};

/// The well-known name. A second instance gets `….instance<pid>`.
pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.rustet";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Delay of the first connection: the D-Bus session is not always reachable the instant the
/// app starts.
const START_DELAY: Duration = Duration::from_millis(1500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection up this long is healthy: earlier failures are forgotten.
const HEALTHY_AFTER: Duration = Duration::from_secs(10);

/// Which bus to register on.
#[derive(Debug, Clone)]
pub enum Bus {
    /// The user's session bus (`DBUS_SESSION_BUS_ADDRESS`).
    Session,
    /// A bus at this D-Bus address (tests use a private bus).
    Address(String),
}

/// What desktop clients ask for.
#[derive(Debug, Clone, PartialEq)]
pub enum MprisEvent {
    /// Send to the page on `mpris:cmd`.
    Command(PageCommand),
    /// `Raise`: show and focus the main window.
    Raise,
    /// `Quit`: quit the app.
    Quit,
    /// `OpenUri` (at most 2 KiB). The shell opens it only if it is a link Rustet handles.
    OpenUri(String),
}

/// Is there a D-Bus session bus? The environment variables alone do not prove it (sandboxes and
/// minimal sessions set them with no bus behind them). Unix socket paths are checked for
/// existence; abstract and non-Unix transports cannot be, so they count as available.
pub fn has_session_bus(
    env: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> bool {
    if let Some(addr) = env("DBUS_SESSION_BUS_ADDRESS").filter(|a| !a.is_empty()) {
        for entry in addr.split(';') {
            if let Some(params) = entry.strip_prefix("unix:") {
                let path = params
                    .split(',')
                    .find_map(|kv| kv.strip_prefix("path="))
                    .filter(|p| !p.is_empty());
                match path {
                    Some(p) => {
                        if exists(Path::new(
                            &*percent_encoding::percent_decode_str(p).decode_utf8_lossy(),
                        )) {
                            return true;
                        }
                    }
                    None => return true, // abstract=… or runtime=…
                }
            } else if !entry.trim().is_empty() {
                return true; // tcp:, nonce-tcp:, …
            }
        }
        return false;
    }
    env("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .is_some_and(|d| exists(&Path::new(&d).join("bus")))
}

/// Delay before reconnect attempt `failures` (1-based): 2 s, doubling to a 60 s ceiling.
pub fn retry_delay(failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(5);
    Duration::from_millis(2000 << exp).min(Duration::from_secs(60))
}

enum Msg {
    Update(MprisUpdate),
    Stop(oneshot::Sender<()>),
}

/// Handle to the MPRIS service. Dropping it unregisters the player.
pub struct Mpris {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Mpris {
    /// Starts the service. Requests from desktop clients arrive on the returned receiver.
    pub fn spawn(bus: Bus) -> (Self, mpsc::UnboundedReceiver<MprisEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(Service::new(bus, events_tx).run(rx));
        (Self { tx }, events_rx)
    }

    /// `mpris:update`.
    pub fn update(&self, update: MprisUpdate) {
        let _ = self.tx.send(Msg::Update(update));
    }

    /// Unregisters and waits until the bus name is released.
    pub async fn stop(self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Stop(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

/// A registered player.
struct Live {
    _conn: zbus::Connection,
    /// Ends when the bus connection closes.
    messages: MessageStream,
    player: InterfaceRef<Player>,
}

struct Service {
    bus: Bus,
    events: mpsc::UnboundedSender<MprisEvent>,
    /// The last state from the page, kept across reconnects.
    state: PlayerState,
    live: Option<Live>,
    failures: u32,
    /// Next timed attempt: the delayed start, then the back-off.
    attempt_at: Option<Instant>,
    healthy_at: Option<Instant>,
}

enum Wake {
    Msg(Option<Msg>),
    Attempt,
    Closed,
    Healthy,
}

async fn at(t: Option<Instant>) {
    match t {
        Some(t) => sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Waits until the connection closes: its message stream reports the socket error, or ends.
async fn closed(live: &mut Option<Live>) {
    match live {
        Some(l) => while let Some(Ok(_)) = l.messages.next().await {},
        None => std::future::pending().await,
    }
}

impl Service {
    fn new(bus: Bus, events: mpsc::UnboundedSender<MprisEvent>) -> Self {
        Self {
            bus,
            events,
            state: PlayerState::default(),
            live: None,
            failures: 0,
            attempt_at: Some(Instant::now() + START_DELAY),
            healthy_at: None,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        loop {
            let wake = tokio::select! {
                msg = rx.recv() => Wake::Msg(msg),
                () = at(self.attempt_at) => Wake::Attempt,
                () = closed(&mut self.live) => Wake::Closed,
                () = at(self.healthy_at) => Wake::Healthy,
            };
            match wake {
                Wake::Msg(None) => break,
                Wake::Msg(Some(Msg::Stop(done))) => {
                    self.live = None;
                    let _ = done.send(());
                    break;
                }
                Wake::Msg(Some(Msg::Update(u))) => self.update(u).await,
                Wake::Attempt => {
                    self.attempt_at = None;
                    self.start().await;
                }
                Wake::Closed => {
                    tracing::warn!("MPRIS: D-Bus connection closed, will reconnect");
                    self.lost();
                }
                Wake::Healthy => {
                    self.healthy_at = None;
                    self.failures = 0;
                }
            }
        }
    }

    async fn update(&mut self, u: MprisUpdate) {
        let metadata = u.metadata.as_ref().map(metadata::convert);
        let Some(live) = &self.live else {
            self.remember(&u, metadata);
            // A pending back-off wins; otherwise try now (the delayed start included).
            if self.failures == 0 || self.attempt_at.is_none() {
                self.attempt_at = None;
                self.start().await;
            }
            return;
        };
        let player = live.player.clone();
        self.remember(&u, metadata.clone());
        if let Err(e) = push(&player, &u, metadata, self.state.position).await {
            tracing::warn!("MPRIS update failed, will reconnect: {e}");
            self.lost();
        }
    }

    /// Keeps each field of the update for the next connection.
    fn remember(&mut self, u: &MprisUpdate, metadata: Option<Metadata>) {
        let s = &mut self.state;
        if let Some(v) = u.status {
            s.status = v;
        }
        if let Some(m) = metadata {
            s.metadata = m;
        }
        if let Some(p) = u.position_us {
            s.position = p;
        }
        if let Some(v) = u.shuffle {
            s.shuffle = v;
        }
        if let Some(v) = u.loop_status {
            s.loop_status = v;
        }
        if let Some(v) = u.volume {
            s.volume = v;
        }
    }

    async fn start(&mut self) {
        if self.live.is_some() {
            return;
        }
        if matches!(self.bus, Bus::Session)
            && !has_session_bus(|k| std::env::var(k).ok(), Path::exists)
        {
            return;
        }
        // Metadata never holds file descriptors, so cloning it cannot fail.
        match timeout(
            CONNECT_TIMEOUT,
            connect(&self.bus, self.state.clone(), self.events.clone()),
        )
        .await
        {
            Ok(Ok(live)) => {
                tracing::info!("MPRIS2 service registered");
                self.live = Some(live);
                self.healthy_at = Some(Instant::now() + HEALTHY_AFTER);
            }
            Ok(Err(e)) => {
                tracing::warn!("MPRIS2 init failed: {e}");
                self.lost();
            }
            Err(_) => {
                tracing::warn!("MPRIS2 init timed out");
                self.lost();
            }
        }
    }

    /// Drops the connection (releasing the name) and schedules the next attempt.
    fn lost(&mut self) {
        self.live = None;
        self.healthy_at = None;
        if self.attempt_at.is_none() {
            self.failures += 1;
            self.attempt_at = Some(Instant::now() + retry_delay(self.failures));
        }
    }
}

async fn connect(
    bus: &Bus,
    state: PlayerState,
    events: mpsc::UnboundedSender<MprisEvent>,
) -> zbus::Result<Live> {
    let builder = match bus {
        Bus::Session => zbus::connection::Builder::session()?,
        Bus::Address(a) => zbus::connection::Builder::address(a.as_str())?,
    };
    let conn = builder
        .serve_at(
            OBJECT_PATH,
            Root {
                events: events.clone(),
            },
        )?
        .serve_at(OBJECT_PATH, Player { state, events })?
        .build()
        .await?;
    let messages = MessageStream::from(&conn);
    if !own_name(&conn, BUS_NAME).await? {
        // Another instance owns the name (MPRIS: append `.instance<pid>`).
        let name = format!("{BUS_NAME}.instance{}", std::process::id());
        if !own_name(&conn, &name).await? {
            return Err(zbus::Error::NameTaken);
        }
    }
    let player = conn
        .object_server()
        .interface::<_, Player>(OBJECT_PATH)
        .await?;
    Ok(Live {
        _conn: conn,
        messages,
        player,
    })
}

/// Requests `name` without queueing; false when someone else has it.
async fn own_name(conn: &zbus::Connection, name: &str) -> zbus::Result<bool> {
    match conn
        .request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(reply) => Ok(matches!(
            reply,
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner
        )),
        Err(zbus::Error::NameTaken) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Applies the fields of an update to the live player and announces what changed, in one
/// `PropertiesChanged`.
async fn push(
    player: &InterfaceRef<Player>,
    u: &MprisUpdate,
    metadata: Option<Metadata>,
    position: i64,
) -> zbus::Result<()> {
    let mut changed: HashMap<&str, DValue<'static>> = HashMap::new();
    {
        let mut p = player.get_mut().await;
        let s = &mut p.state;
        if u.position_us.is_some() {
            s.position = position;
            s.position_at = std::time::Instant::now();
        }
        if let Some(v) = u.status.filter(|v| *v != s.status) {
            s.status = v;
            changed.insert("PlaybackStatus", v.as_str().into());
        }
        if let Some(m) = metadata.filter(|m| *m != s.metadata) {
            changed.insert("Metadata", DValue::from(m.clone()));
            s.metadata = m;
        }
        if let Some(v) = u.shuffle.filter(|v| *v != s.shuffle) {
            s.shuffle = v;
            changed.insert("Shuffle", v.into());
        }
        if let Some(v) = u.loop_status.filter(|v| *v != s.loop_status) {
            s.loop_status = v;
            changed.insert("LoopStatus", v.as_str().into());
        }
        if let Some(v) = u.volume.filter(|v| *v != s.volume) {
            s.volume = v;
            changed.insert("Volume", v.into());
        }
    }
    let emitter = player.signal_emitter();
    if !changed.is_empty() {
        fdo::Properties::properties_changed(
            emitter,
            InterfaceName::from_static_str_unchecked(PLAYER_IFACE),
            changed,
            Cow::Borrowed(&[]),
        )
        .await?;
    }
    if u.seeked && position != 0 {
        Player::seeked(emitter, position).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_bus_needs_an_actual_bus() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let all = |_: &Path| true;
        let none = |_: &Path| false;
        assert!(!has_session_bus(env(&[]), all), "no variables at all");
        assert!(
            !has_session_bus(env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]), none),
            "runtime dir without a bus"
        );
        assert!(has_session_bus(
            env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]),
            |p| p == Path::new("/run/user/1000/bus")
        ));
        let addr = |a: &'static str| env(Box::leak(Box::new([("DBUS_SESSION_BUS_ADDRESS", a)])));
        assert!(
            !has_session_bus(addr("unix:path=/run/user/1000/bus"), none),
            "missing socket"
        );
        assert!(has_session_bus(addr("unix:path=/run/user/1000/bus"), all));
        assert!(
            has_session_bus(addr("unix:abstract=/tmp/dbus-x,guid=1"), none),
            "abstract cannot be checked"
        );
        assert!(
            has_session_bus(addr("unix:path=/a/b;unix:path=/c/d"), |p| p
                == Path::new("/c/d")),
            "any transport"
        );
        assert!(
            has_session_bus(addr("unix:path=/run/user/1000/bus,guid=abc"), |p| p
                == Path::new("/run/user/1000/bus")),
            "trailing parameters"
        );
        assert!(
            has_session_bus(addr("unix:guid=abc,path=/x%20y"), |p| p
                == Path::new("/x y")),
            "escaped path"
        );
        assert!(has_session_bus(addr("tcp:host=localhost,port=1"), none));
    }

    #[test]
    fn retry_delay_doubles_to_a_minute() {
        let ms: Vec<u128> = [1, 2, 3, 4, 5, 6, 7, 20]
            .map(|f| retry_delay(f).as_millis())
            .to_vec();
        assert_eq!(ms, [2000, 4000, 8000, 16000, 32000, 60000, 60000, 60000]);
        assert_eq!(retry_delay(0).as_millis(), 2000);
    }
}
