//! Listening-port allocator for spawned `llama-server` instances.
//!
//! The plan fixed `41100..=41300` as the default range — high,
//! unprivileged, and not commonly claimed by dev servers. The
//! allocator linearly probes the range, asking the OS to bind each
//! candidate via `TcpListener::bind` (then immediately drops the
//! socket); the first successful bind wins and is recorded into the
//! daemon's running-snapshot so orphan re-adoption can find the
//! child later.

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::ops::RangeInclusive;

use crate::config::loader::PortRange;

/// Errors from [`allocate`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AllocateError {
  /// No port in the configured range was bindable.
  #[error("no free port in {}-{} ({} reserved by caller)", range.0, range.1, in_use.len())]
  NoFreePort { range: (u16, u16), in_use: Vec<u16> },
  /// Inverted or empty range — caller bug, surfaces as a structured
  /// error so the supervisor can refuse the launch instead of
  /// looping forever.
  #[error("port range is empty or inverted")]
  EmptyRange,
}

/// Allocate a free port from `range`, skipping any already in
/// `reserved` (the supervisor's set of ports it has handed out but
/// whose children haven't yet bound). `reserved` matters because
/// the OS-bind probe drops the socket immediately, so two
/// near-simultaneous allocations could otherwise both pick the same
/// port.
pub fn allocate(range: &PortRange, reserved: &[u16]) -> Result<u16, AllocateError> {
  let span = configured_range(range)?;
  let mut conflicts: Vec<u16> = Vec::new();
  for port in span {
    if reserved.contains(&port) {
      conflicts.push(port);
      continue;
    }
    if try_bind(port) {
      return Ok(port);
    }
    conflicts.push(port);
  }
  Err(AllocateError::NoFreePort {
    range: (range.start, range.end),
    in_use: conflicts,
  })
}

fn configured_range(range: &PortRange) -> Result<RangeInclusive<u16>, AllocateError> {
  if range.start == 0 || range.end < range.start {
    return Err(AllocateError::EmptyRange);
  }
  Ok(range.start..=range.end)
}

/// Try a `TcpListener::bind` against 127.0.0.1:port. We immediately
/// drop the listener — this is a probe, not a long-lived socket.
fn try_bind(port: u16) -> bool {
  let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
  TcpListener::bind(addr).is_ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn allocate_picks_first_free_port_in_range() {
    // Use a high port range so we're unlikely to collide with
    // anything bound on the test box.
    let range = PortRange {
      start: 45200,
      end: 45250,
    };
    let p = allocate(&range, &[]).expect("free port");
    assert!(range.start <= p && p <= range.end);
  }

  #[test]
  fn reserved_ports_skipped_even_when_bindable() {
    let range = PortRange {
      start: 45301,
      end: 45310,
    };
    let reserved = vec![45301, 45302];
    let p = allocate(&range, &reserved).expect("free port");
    assert!(!reserved.contains(&p));
    assert!(p >= 45303);
  }

  #[test]
  fn empty_range_returns_structured_error() {
    let range = PortRange {
      start: 100,
      end: 50,
    };
    assert_eq!(
      allocate(&range, &[]).unwrap_err(),
      AllocateError::EmptyRange
    );
  }

  #[test]
  fn zero_start_rejected_as_empty_range() {
    let range = PortRange { start: 0, end: 10 };
    assert_eq!(
      allocate(&range, &[]).unwrap_err(),
      AllocateError::EmptyRange
    );
  }

  #[test]
  fn fully_reserved_range_returns_no_free_port() {
    let range = PortRange {
      start: 45401,
      end: 45403,
    };
    let reserved = vec![45401, 45402, 45403];
    let err = allocate(&range, &reserved).unwrap_err();
    match err {
      AllocateError::NoFreePort {
        range: r,
        in_use: ports,
      } => {
        assert_eq!(r, (45401, 45403));
        assert_eq!(ports, vec![45401, 45402, 45403]);
      }
      other => panic!("expected NoFreePort, got {other:?}"),
    }
  }
}
