//! Refuses connections from other users of the machine.
//!
//! The API is unauthenticated and loopback is shared by every user: without this check any
//! local account could drive playback, export files as the owner or clear the DRM session.
//! Browser origin checks ([`crate::guard`]) only stop other web pages.
//!
//! On Linux the kernel tells us who owns the client end of a loopback connection: the uid
//! column of the socket's entry in `/proc/net/tcp{,6}`. Connections from any uid other than the
//! engine's own (or root, who could do anything anyway) are closed before the TLS handshake. On
//! Windows the TCP table gives the owning process, whose token gives the user. Where the check
//! cannot work it is switched off with a warning rather than bricking the app;
//! `RUSTET_ALLOW_OTHER_USERS=1` switches it off deliberately.

use std::net::SocketAddr;

use tracing::warn;

#[cfg(windows)]
mod windows;

/// How accepted connections are checked.
#[derive(Clone, Debug)]
pub(crate) enum PeerGuard {
    /// Every connection is accepted.
    Off,
    /// Linux: the client's uid must be `euid` or root.
    #[cfg(unix)]
    ProcNet { euid: u32 },
    /// Windows: the client must run as the engine's user.
    #[cfg(windows)]
    SameUser(std::sync::Arc<windows::UserSid>),
}

#[cfg(unix)]
const PROC_NET_PATHS: [&str; 2] = ["/proc/net/tcp", "/proc/net/tcp6"];

impl PeerGuard {
    /// The check for this platform, or [`PeerGuard::Off`] when it cannot be enforced.
    pub fn from_env() -> Self {
        if std::env::var("RUSTET_ALLOW_OTHER_USERS").is_ok_and(|v| v == "1") {
            warn!("RUSTET_ALLOW_OTHER_USERS=1: any local user can use the engine API");
            return Self::Off;
        }
        Self::platform()
    }

    #[cfg(unix)]
    fn platform() -> Self {
        if let Err(e) = std::fs::metadata(PROC_NET_PATHS[0]) {
            warn!(
                "cannot identify local clients (no /proc/net/tcp); the engine API is open to every local user: {e}"
            );
            return Self::Off;
        }
        Self::ProcNet {
            euid: rustix::process::geteuid().as_raw(),
        }
    }

    #[cfg(windows)]
    fn platform() -> Self {
        match windows::current_user_sid() {
            Ok(sid) => Self::SameUser(std::sync::Arc::new(sid)),
            Err(e) => {
                warn!(
                    "cannot determine the engine's user; the engine API is open to every local user: {e}"
                );
                Self::Off
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn platform() -> Self {
        Self::Off
    }

    /// Whether the connection from `client` to `server` may be served. It reads the socket
    /// table, so it blocks: call it off the runtime workers.
    pub fn admits(&self, client: SocketAddr, server: SocketAddr) -> bool {
        match self {
            Self::Off => true,
            #[cfg(unix)]
            Self::ProcNet { euid } => match peer_uid(client, server) {
                Ok(uid) if uid == *euid || uid == 0 => true,
                Ok(uid) => {
                    warn!(uid, "refusing connection from another user");
                    false
                }
                Err(e) => {
                    warn!("refusing connection: could not identify the client user: {e}");
                    false
                }
            },
            #[cfg(windows)]
            Self::SameUser(sid) => match windows::peer_is_same_user(client, server, sid) {
                Ok(same) => same,
                Err(e) => {
                    warn!("refusing connection: could not identify the client user: {e}");
                    false
                }
            },
        }
    }
}

/// The uid of the process on the other end of a loopback TCP connection.
#[cfg(unix)]
fn peer_uid(client: SocketAddr, server: SocketAddr) -> std::io::Result<u32> {
    use std::sync::Mutex;
    // One scan at a time keeps a burst of connections from re-reading the table in parallel.
    static SCAN: Mutex<()> = Mutex::new(());
    let _one = SCAN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // The client socket's own entry has its local address on the left: our remote one.
    let (want_local, want_remote) = (proc_addr(client), proc_addr(server));
    let mut first_err = None;
    for path in PROC_NET_PATHS {
        match std::fs::read_to_string(path) {
            Ok(table) => {
                if let Some(uid) = find_uid(&table, &want_local, &want_remote)? {
                    return Ok(uid);
                }
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.unwrap_or_else(|| {
        std::io::Error::other(format!("no socket entry for {client} -> {server}"))
    }))
}

/// The uid column of the row for `local` → `remote` in a `/proc/net/tcp` table.
#[cfg(unix)]
fn find_uid(table: &str, local: &str, remote: &str) -> std::io::Result<Option<u32>> {
    // sl local rem st tx:rx tr:tm retrnsmt uid timeout inode …
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[1] != local || fields[2] != remote {
            continue;
        }
        return fields[7].parse().map(Some).map_err(std::io::Error::other);
    }
    Ok(None)
}

/// An address as `/proc/net/tcp` writes it: each 32-bit word of the address in hex, in host
/// byte order (little-endian on the machines that matter), then `:` and the port in hex.
#[cfg(unix)]
fn proc_addr(a: SocketAddr) -> String {
    use std::fmt::Write;
    let ip = match a.ip() {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(std::net::IpAddr::V6(v6), std::net::IpAddr::V4),
        ip => ip,
    };
    let octets: Vec<u8> = match ip {
        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
        std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    let mut s = String::new();
    for word in octets.chunks(4) {
        for b in word.iter().rev() {
            let _ = write!(s, "{b:02X}");
        }
    }
    let _ = write!(s, ":{:04X}", a.port());
    s
}

#[cfg(all(test, unix))]
mod tests {
    use std::net::{TcpListener, TcpStream};

    use super::*;

    #[test]
    fn proc_addr_format() {
        assert_eq!(
            proc_addr("127.0.0.1:20025".parse().unwrap()),
            "0100007F:4E39"
        );
        assert_eq!(
            proc_addr("[::1]:80".parse().unwrap()),
            "00000000000000000000000001000000:0050"
        );
        assert_eq!(
            proc_addr("[::ffff:127.0.0.1]:1".parse().unwrap()),
            "0100007F:0001"
        );
    }

    #[test]
    fn find_uid_reads_the_matching_row() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:4E39 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 1\n";
        assert_eq!(
            find_uid(table, "0100007F:4E39", "0100007F:D431").unwrap(),
            Some(1000)
        );
        assert_eq!(
            find_uid(table, "0100007F:D431", "0100007F:4E39").unwrap(),
            None
        );
    }

    // The kernel's socket table must name the uid of the client on a real loopback
    // connection: this exercises the address formatting end to end.
    #[test]
    fn peer_uid_finds_the_client_on_a_loopback_connection() {
        if std::fs::metadata(PROC_NET_PATHS[0]).is_err() {
            return;
        }
        let ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (server, peer) = ln.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        let uid = peer_uid(peer, server.local_addr().unwrap()).unwrap();
        assert_eq!(uid, rustix::process::getuid().as_raw());
    }

    #[test]
    fn guard_admits_own_user_and_refuses_others() {
        if std::fs::metadata(PROC_NET_PATHS[0]).is_err() {
            return;
        }
        let ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (server, peer) = ln.accept().unwrap();
        let local = server.local_addr().unwrap();
        let me = rustix::process::geteuid().as_raw();
        assert!(PeerGuard::ProcNet { euid: me }.admits(peer, local));
        if me != 0 {
            assert!(!PeerGuard::ProcNet { euid: me + 1 }.admits(peer, local));
        }
        assert!(PeerGuard::Off.admits(peer, local));
    }
}
