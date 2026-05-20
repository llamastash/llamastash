//! Verify the UID of the peer on the other end of a Unix-domain socket.
//!
//! Linux exposes this through `SO_PEERCRED`, which fills a `ucred` struct
//! with the peer's PID, UID, and GID. macOS uses `getpeereid(2)` instead.
//! Both are kernel-attested — the peer cannot lie about its UID. This is
//! the only auth boundary in Unit 2: any peer whose UID matches the
//! daemon's UID is trusted; everything else is hung up on before its
//! request is parsed.

use std::io;

use tokio::net::UnixStream;

/// The peer-credential information we need. We don't surface PID/GID here
/// because nothing in Unit 2 consumes them; add fields as later units
/// require them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
  pub uid: u32,
}

/// Read peer credentials from an *accepted* `UnixStream`.
///
/// Returns `Err(io::Error)` if the platform call fails. Callers should
/// log the error and close the connection — failing closed is the only
/// safe response to a credential read that didn't complete.
pub fn read_peer_credentials(stream: &UnixStream) -> io::Result<PeerCred> {
  read_peer_credentials_impl(stream)
}

/// Returns true iff `peer` matches the current process's real UID. The
/// daemon process is owned by exactly one user; any peer whose UID
/// doesn't match cannot have legitimate llamastash credentials.
pub fn is_authorized_peer(peer: PeerCred) -> bool {
  // SAFETY: `getuid(2)` is documented as "always successful" and reads no
  // memory.
  let me = unsafe { libc::getuid() };
  peer.uid == me
}

#[cfg(target_os = "linux")]
fn read_peer_credentials_impl(stream: &UnixStream) -> io::Result<PeerCred> {
  use std::{mem::size_of, os::fd::AsRawFd};

  let fd = stream.as_raw_fd();
  let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
  let mut len = size_of::<libc::ucred>() as libc::socklen_t;
  // SAFETY: `getsockopt(SO_PEERCRED)` writes into `cred`. `len` is the
  // full struct size; the kernel will populate as much as it knows.
  let ret = unsafe {
    libc::getsockopt(
      fd,
      libc::SOL_SOCKET,
      libc::SO_PEERCRED,
      (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
      &mut len,
    )
  };
  if ret != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(PeerCred { uid: cred.uid })
}

#[cfg(target_os = "macos")]
fn read_peer_credentials_impl(stream: &UnixStream) -> io::Result<PeerCred> {
  use std::os::fd::AsRawFd;

  let fd = stream.as_raw_fd();
  let mut uid: libc::uid_t = 0;
  let mut gid: libc::gid_t = 0;
  // SAFETY: `getpeereid` writes into two `uid_t` slots. Both are
  // stack-allocated and live for the call.
  let ret = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
  if ret != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(PeerCred { uid })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_peer_credentials_impl(_stream: &UnixStream) -> io::Result<PeerCred> {
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "peercred is only implemented for Linux and macOS",
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn read_peer_credentials_returns_own_uid_on_socket_pair() {
    let (a, b) = UnixStream::pair().expect("UnixStream::pair should succeed");
    let cred_a = read_peer_credentials(&a).expect("peercred a");
    let cred_b = read_peer_credentials(&b).expect("peercred b");
    // SAFETY: see is_authorized_peer.
    let me = unsafe { libc::getuid() };
    assert_eq!(cred_a.uid, me);
    assert_eq!(cred_b.uid, me);
  }

  #[tokio::test]
  async fn is_authorized_peer_accepts_own_uid() {
    let (a, _b) = UnixStream::pair().expect("pair");
    let cred = read_peer_credentials(&a).expect("peercred");
    assert!(is_authorized_peer(cred));
  }

  #[test]
  fn is_authorized_peer_rejects_other_uid() {
    // SAFETY: see is_authorized_peer.
    let me = unsafe { libc::getuid() };
    let other = me.wrapping_add(1);
    assert!(!is_authorized_peer(PeerCred { uid: other }));
  }
}
