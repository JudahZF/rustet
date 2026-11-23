//! The Windows peer check: the TCP table (`GetExtendedTcpTable`) maps a connection to its
//! owning process, and the process token gives its user SID.
#![allow(unsafe_code)] // Win32 calls; each block states its contract.

use std::io;
use std::net::{IpAddr, SocketAddr};

use tracing::warn;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
    TCP_TABLE_OWNER_PID_CONNECTIONS,
};
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::Security::{
    CopySid, EqualSid, GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

/// An owned copy of a user SID.
#[derive(Debug)]
pub(crate) struct UserSid(Vec<u8>);

impl UserSid {
    fn as_psid(&self) -> *mut core::ffi::c_void {
        self.0.as_ptr().cast_mut().cast()
    }
}

/// Closes a handle when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned open by a Win32 call and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The user of a process token.
fn token_user(process: HANDLE) -> Option<UserSid> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is a valid process handle; `token` receives a handle we own.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let token = Handle(token);
    let mut len = 0u32;
    // SAFETY: a size query with a null buffer; only `len` is written.
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return None;
    }
    // u64 elements keep the buffer aligned for TOKEN_USER.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` writable bytes.
    if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) }
        == 0
    {
        return None;
    }
    // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points into the buffer.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: `sid` is a valid SID inside `buf`, which is alive.
    let sid_len = unsafe { GetLengthSid(sid) };
    let mut out = vec![0u8; sid_len as usize];
    // SAFETY: `out` has `sid_len` bytes; `sid` is valid.
    if unsafe { CopySid(sid_len, out.as_mut_ptr().cast(), sid) } == 0 {
        return None;
    }
    Some(UserSid(out))
}

/// The engine's own user.
pub(crate) fn current_user_sid() -> io::Result<UserSid> {
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    token_user(unsafe { GetCurrentProcess() }).ok_or_else(io::Error::last_os_error)
}

fn session_of(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: `session` is a valid out pointer.
    (unsafe { ProcessIdToSessionId(pid, &mut session) } != 0).then_some(session)
}

/// Whether the process on the other end of the loopback connection runs as `me`.
///
/// A token that can be read gives the exact user. Chromium's sandboxed helpers (the network
/// service, renderers) run with restricted tokens that deny `TOKEN_QUERY` even to the same user,
/// so when the token is unreadable the logon session decides: another user's processes live in
/// a different session.
pub(crate) fn peer_is_same_user(
    client: SocketAddr,
    server: SocketAddr,
    me: &UserSid,
) -> io::Result<bool> {
    let pid = owner_pid(client, server)?;
    // SAFETY: plain call; a null return is handled.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if !process.is_null() {
        let process = Handle(process);
        if let Some(sid) = token_user(process.0) {
            // SAFETY: both SIDs are valid and alive.
            if unsafe { EqualSid(sid.as_psid(), me.as_psid()) } != 0 {
                return Ok(true);
            }
            warn!(pid, "peer runs as a different user");
            return Ok(false);
        }
    }
    let Some(peer_session) = session_of(pid) else {
        warn!(pid, "peer token unreadable and its session unknown");
        return Ok(false);
    };
    // SAFETY: plain call.
    let ours = session_of(unsafe { GetCurrentProcessId() }).ok_or_else(io::Error::last_os_error)?;
    if peer_session == ours {
        return Ok(true);
    }
    warn!(
        pid,
        peer_session, ours, "peer token unreadable and it runs in another session"
    );
    Ok(false)
}

fn ipv4_word(a: SocketAddr) -> io::Result<u32> {
    match a.ip() {
        // The table holds the address bytes in network order, read as a little-endian word.
        IpAddr::V4(v4) => Ok(u32::from_le_bytes(v4.octets())),
        IpAddr::V6(_) => Err(io::Error::other("not an IPv4 TCP connection")),
    }
}

/// The pid owning the client end (local = client, remote = server) of a connection.
fn owner_pid(client: SocketAddr, server: SocketAddr) -> io::Result<u32> {
    let (want_local, want_remote) = (ipv4_word(client)?, ipv4_word(server)?);
    // Ports hold a network-order value in the low 16 bits.
    let port = |p: u32| u16::from_be(p as u16);
    let mut size = 0u32;
    for _ in 0..3 {
        let mut buf = vec![0u32; (size as usize).div_ceil(4)];
        let ptr = if buf.is_empty() {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr().cast()
        };
        // SAFETY: `ptr` is null (size query) or points to `size` writable, aligned bytes.
        let r = unsafe {
            GetExtendedTcpTable(
                ptr,
                &mut size,
                0,
                u32::from(AF_INET),
                TCP_TABLE_OWNER_PID_CONNECTIONS,
                0,
            )
        };
        if r == ERROR_INSUFFICIENT_BUFFER {
            size += 4096; // the table can grow between the sizing call and the real one
            continue;
        }
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let table = buf.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
        // SAFETY: on success the buffer holds a MIB_TCPTABLE_OWNER_PID with `dwNumEntries` rows.
        let rows: &[MIB_TCPROW_OWNER_PID] = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!((*table).table).cast(),
                (*table).dwNumEntries as usize,
            )
        };
        return rows
            .iter()
            .find(|row| {
                row.dwLocalAddr == want_local
                    && row.dwRemoteAddr == want_remote
                    && port(row.dwLocalPort) == client.port()
                    && port(row.dwRemotePort) == server.port()
            })
            .map(|row| row.dwOwningPid)
            .ok_or_else(|| {
                io::Error::other(format!("no TCP table entry for {client} -> {server}"))
            });
    }
    Err(io::Error::other("TCP table kept growing"))
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};

    use super::*;

    #[test]
    fn a_client_running_as_the_same_user_is_admitted() {
        let me = current_user_sid().unwrap();
        let ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (server, peer) = ln.accept().unwrap();
        assert!(peer_is_same_user(peer, server.local_addr().unwrap(), &me).unwrap());
    }
}
