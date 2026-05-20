//! Length-prefixed frame codec for the Unix-socket transport.
//!
//! Each frame is a 4-byte big-endian length followed by `len` body bytes.
//! `MAX_FRAME_SIZE` is enforced on read *before* the body buffer is
//! allocated, so a malicious peer cannot ask llamastash to allocate a
//! gigabyte by sending an inflated prefix.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap on a single JSON-RPC frame. Matches the YAML config cap in
/// `crate::config::loader::MAX_CONFIG_BYTES` so the two adversarial
/// surfaces are bounded the same way.
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Structured errors so callers (and tests) can distinguish a protocol
/// violation from a transport-level failure.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
  /// Peer sent a length prefix larger than `MAX_FRAME_SIZE`.
  #[error("peer announced a {advertised}-byte frame; max is {max}")]
  TooLarge { advertised: usize, max: usize },
  /// Peer closed the connection at or before the prefix completed.
  #[error("peer closed the connection mid-frame")]
  PeerClosed,
  /// Any other I/O error from the underlying socket.
  #[error("frame i/o error: {0}")]
  Io(#[source] io::Error),
}

impl From<io::Error> for FrameError {
  fn from(e: io::Error) -> Self {
    if e.kind() == io::ErrorKind::UnexpectedEof {
      Self::PeerClosed
    } else {
      Self::Io(e)
    }
  }
}

/// Read one length-prefixed frame from `reader`. Caps the body at
/// `MAX_FRAME_SIZE` and refuses to allocate beyond that even if the peer
/// lies about the size.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, FrameError> {
  let mut prefix = [0u8; 4];
  reader.read_exact(&mut prefix).await?;
  let len = u32::from_be_bytes(prefix) as usize;
  if len > MAX_FRAME_SIZE {
    return Err(FrameError::TooLarge {
      advertised: len,
      max: MAX_FRAME_SIZE,
    });
  }
  let mut body = vec![0u8; len];
  reader.read_exact(&mut body).await?;
  Ok(body)
}

/// Write one length-prefixed frame to `writer`. The caller is responsible
/// for keeping the body under `MAX_FRAME_SIZE`; oversized writes are
/// rejected here so a buggy server can't crash a careful client.
///
/// The prefix and body are concatenated into a single buffer and written
/// with one `write_all`, so a future cancelled mid-call either emits
/// the whole frame or leaves the byte stream byte-aligned-for-the-next-call.
/// Without this, a cancellation between the prefix `write_all` and the
/// body `write_all` would leave the peer parsing 4 bytes of prefix and
/// waiting forever for a body that never comes.
pub async fn write_frame<W: AsyncWrite + Unpin>(
  writer: &mut W,
  body: &[u8],
) -> Result<(), FrameError> {
  if body.len() > MAX_FRAME_SIZE {
    return Err(FrameError::TooLarge {
      advertised: body.len(),
      max: MAX_FRAME_SIZE,
    });
  }
  // Cast is safe: bounded by MAX_FRAME_SIZE above, which fits in u32.
  let prefix = (body.len() as u32).to_be_bytes();
  let mut buf = Vec::with_capacity(prefix.len() + body.len());
  buf.extend_from_slice(&prefix);
  buf.extend_from_slice(body);
  writer.write_all(&buf).await?;
  writer.flush().await?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use tokio::io::duplex;

  use super::*;

  #[tokio::test]
  async fn roundtrip_preserves_body() {
    let (mut client, mut server) = duplex(8192);
    let body = b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":1}";

    let write = tokio::spawn(async move { write_frame(&mut client, body).await });
    let read_body = read_frame(&mut server).await.expect("read should succeed");

    write.await.unwrap().expect("write should succeed");
    assert_eq!(&read_body, body);
  }

  #[tokio::test]
  async fn zero_length_frame_is_legal() {
    let (mut client, mut server) = duplex(64);
    let write = tokio::spawn(async move { write_frame(&mut client, &[]).await });
    let body = read_frame(&mut server).await.expect("read should succeed");
    write.await.unwrap().expect("write should succeed");
    assert!(body.is_empty());
  }

  #[tokio::test]
  async fn oversized_prefix_is_rejected_before_alloc() {
    // Hand-craft a frame whose prefix says 1 GiB but send no body — a
    // naïve implementation would allocate 1 GiB and then block on read.
    let (mut client, mut server) = duplex(8);
    let bogus: u32 = 1024 * 1024 * 1024;
    let prefix = bogus.to_be_bytes();
    tokio::spawn(async move {
      let _ = client.write_all(&prefix).await;
    });

    let err = read_frame(&mut server)
      .await
      .expect_err("oversized prefix must be rejected");
    match err {
      FrameError::TooLarge { advertised, max } => {
        assert_eq!(advertised, bogus as usize);
        assert_eq!(max, MAX_FRAME_SIZE);
      }
      other => panic!("expected TooLarge, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn truncated_prefix_reports_peer_closed() {
    let (mut client, mut server) = duplex(8);
    tokio::spawn(async move {
      // Write only 2 of the 4 prefix bytes, then drop the socket.
      let _ = client.write_all(&[0, 0]).await;
    });

    let err = read_frame(&mut server)
      .await
      .expect_err("short prefix must error");
    assert!(matches!(err, FrameError::PeerClosed));
  }

  #[tokio::test]
  async fn truncated_body_reports_peer_closed() {
    let (mut client, mut server) = duplex(64);
    tokio::spawn(async move {
      // Advertise 8 bytes, send 3.
      let _ = client.write_all(&8u32.to_be_bytes()).await;
      let _ = client.write_all(b"abc").await;
    });

    let err = read_frame(&mut server)
      .await
      .expect_err("short body must error");
    assert!(matches!(err, FrameError::PeerClosed));
  }

  #[tokio::test]
  async fn write_rejects_oversized_body() {
    let (mut client, _server) = duplex(64);
    let huge = vec![0u8; MAX_FRAME_SIZE + 1];
    let err = write_frame(&mut client, &huge)
      .await
      .expect_err("oversized write must error");
    assert!(matches!(err, FrameError::TooLarge { .. }));
  }
}
