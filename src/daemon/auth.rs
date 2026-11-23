//! Bearer-token authentication for the control-plane HTTP listener.
//!
//! The token is generated fresh on every daemon start (32 bytes from `SysRng`,
//! base64url without padding) and written to `runtime.json` with the listener
//! URL. Clients read that file, or honor `LLAMASTASH_IPC_TOKEN`, and send
//! `Authorization: Bearer <token>` on every request except `/health`.
//! Comparisons are constant-time.
//!
//! The token plus the file permissions on `runtime.json` (0o600 on Unix,
//! owner-only DACL on Windows) are the whole control-plane auth story: the
//! secret reaches only the same-UID processes that can read the file.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::TryRng;

use crate::util::http_auth::constant_time_eq;

/// Raw token length before base64url encoding. 32 bytes of OS randomness is
/// about 256 bits of entropy, past the bar for a same-machine secret rotated
/// on every daemon start.
pub const TOKEN_BYTES: usize = 32;

/// Per-daemon bearer token. Wraps the base64url string so equality checks are
/// constant-time and a stray `Debug` log cannot leak the secret.
#[derive(Clone)]
pub struct IpcToken(String);

impl IpcToken {
  /// Generate a fresh token from `SysRng`. The only panic path is an OS
  /// randomness failure, which on Linux/macOS means `getrandom(2)` errored, a
  /// non-recoverable state where a panic is the honest response.
  pub fn generate() -> Self {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::SysRng
      .try_fill_bytes(&mut bytes)
      .expect("SysRng must succeed for daemon startup");
    Self(URL_SAFE_NO_PAD.encode(bytes))
  }

  /// Wrap an existing token string (env override path / tests).
  pub fn from_string(raw: String) -> Self {
    Self(raw)
  }

  /// Borrow the encoded string for transport or serialization. The slice holds
  /// the full secret, so callers must not log it.
  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Consume the token to recover the owned string. Same secrecy caveat as
  /// `as_str`.
  pub fn into_string(self) -> String {
    self.0
  }

  /// Constant-time comparison against a candidate string. An early length
  /// mismatch is acceptable: the token length is not a useful signal.
  pub fn verify(&self, candidate: &str) -> bool {
    constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
  }
}

impl std::fmt::Debug for IpcToken {
  // Keep the secret out of Debug output so wrapping `log::debug!` and `format!`
  // calls never emit it.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IpcToken")
      .field("len", &self.0.len())
      .finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generate_produces_unique_tokens() {
    let a = IpcToken::generate();
    let b = IpcToken::generate();
    assert_ne!(a.as_str(), b.as_str(), "two fresh tokens collided");
    // 32 bytes base64url-encoded without padding is 43 chars
    // (ceil(32 * 4 / 3), from the general ceil(N * 8 / 6)).
    assert_eq!(a.as_str().len(), 43);
  }

  #[test]
  fn verify_accepts_self() {
    let t = IpcToken::generate();
    let candidate = t.as_str().to_owned();
    assert!(t.verify(&candidate));
  }

  #[test]
  fn verify_rejects_wrong_token() {
    let t = IpcToken::generate();
    assert!(!t.verify("not-the-token"));
    assert!(!t.verify(""));
  }

  #[test]
  fn verify_rejects_length_mismatch() {
    let t = IpcToken::from_string("short".into());
    assert!(!t.verify("shorter"));
    assert!(!t.verify("much-longer-than-the-token"));
  }

  #[test]
  fn debug_does_not_leak_secret() {
    let t = IpcToken::from_string("super-secret-token-value".into());
    let dbg = format!("{t:?}");
    assert!(!dbg.contains("super-secret-token-value"));
    assert!(dbg.contains("IpcToken"));
  }
}
