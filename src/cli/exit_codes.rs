//! Exit-code table shared by every non-interactive subcommand (R35).
//!
//! Codes are part of the public CLI contract: agent scripts pin
//! against them to branch on failure class without having to parse
//! human-readable error strings. Keep them stable across versions.
//! The companion table in `README.md`'s troubleshooting section must
//! match this file — use `cargo test cli::exit_codes` to spot drift.

use crate::ipc::ClientError;

/// Successful execution.
pub const SUCCESS: i32 = 0;
/// Bad CLI usage (missing required arg, invalid combination). Clap
/// emits this kind of failure on its own; reserved here so handlers
/// don't accidentally collide.
pub const USAGE: i32 = 64;
/// Daemon is not reachable (socket missing, peer hung up, timeout).
pub const DAEMON_UNREACHABLE: i32 = 65;
/// Caller-supplied model reference matched zero or multiple models.
/// The handler emits a disambiguation hint to stderr.
pub const MODEL_NOT_FOUND: i32 = 66;
/// Daemon accepted `start_model` but the supervisor failed (probe
/// timeout, port allocation failure, etc.).
pub const LAUNCH_FAILED: i32 = 67;
/// `stop_model` / `stop_all` returned an error. Distinct from
/// MODEL_NOT_FOUND so scripts can branch on "tried to stop, daemon
/// declined" vs. "didn't find that model in the first place".
pub const STOP_FAILED: i32 = 68;
/// Reserved for `pull` (R46). Lands in v2 alongside the in-app HF
/// pull worker. Documented here so the table stays gap-free.
#[allow(dead_code)]
pub const PULL_FAILED: i32 = 69;
/// `llama-server` binary not on `$PATH`, no `--llama-server` flag,
/// and `LLAMASTASH_LLAMA_SERVER` unset.
pub const BINARY_NOT_FOUND: i32 = 70;
/// Catch-all for unexpected errors that don't map to a documented
/// failure class. anyhow's bubble-up lands here.
pub const UNKNOWN: i32 = 71;
/// `llamastash init` aborted before reaching a smoke-launch — install
/// integrity check failed, daemon stop/restart could not be coerced,
/// archive-bomb defenses tripped, or the user declined a confirm
/// prompt. Distinct from `INIT_DOWNLOAD_FAILED` and
/// `INIT_SMOKE_FAILED` so agents can branch on the failure phase.
pub const INIT_ABORTED: i32 = 72;
/// `llamastash init`'s download step failed (disk-full precheck,
/// HF API error, shard checksum mismatch). Distinct from
/// `PULL_FAILED` (69) so a wizard-internal failure is separable
/// from a standalone `llamastash pull <repo>` error.
pub const INIT_DOWNLOAD_FAILED: i32 = 73;
/// `llamastash init` reached the smoke-launch phase but the launch
/// didn't reach a healthy `/v1/chat/completions` response (probe
/// timeout, OOM at load, binary --version probe failed).
pub const INIT_SMOKE_FAILED: i32 = 74;

/// Result type for CLI handlers. `Ok(())` exits zero; `Err(CliExit)`
/// prints the message to stderr and exits with the bound code.
pub type CliResult = Result<(), CliExit>;

/// Structured exit signal. Wraps a numeric code (one of the constants
/// above) plus an optional stderr message. `dispatch` consumes these
/// at the top of `main` and returns the code to the OS.
#[derive(Debug)]
pub struct CliExit {
  pub code: i32,
  pub message: Option<String>,
}

impl CliExit {
  pub fn new(code: i32, message: impl Into<String>) -> Self {
    Self {
      code,
      message: Some(message.into()),
    }
  }

  /// Bare exit code — used for codes whose meaning is conveyed by
  /// the code alone (e.g. SIGPIPE during `logs --follow`).
  pub fn code_only(code: i32) -> Self {
    Self {
      code,
      message: None,
    }
  }

  /// Shortcut for the very common shape
  /// `CliExit::new(CODE, format!("PREFIX: {err}"))`. Walks `err`'s
  /// `std::error::Error::source()` chain and appends every layer's
  /// `Display` so users see the underlying cause (e.g. `hf-hub:
  /// request error: ...: io error: connection reset`) instead of
  /// just the top-level wrapper. Saves a `format!` at the call site
  /// and keeps the prefix-then-cause formatting consistent across
  /// handlers.
  pub fn prefix<E: std::error::Error>(code: i32, prefix: impl AsRef<str>, err: E) -> Self {
    use std::fmt::Write;
    let mut buf = format!("{}: {err}", prefix.as_ref());
    let mut cur = err.source();
    while let Some(s) = cur {
      let _ = write!(buf, ": {s}");
      cur = s.source();
    }
    Self::new(code, buf)
  }

  /// Map an `ipc::ClientError` into the canonical exit code. Connect
  /// failures land on `DAEMON_UNREACHABLE`; remote / protocol errors
  /// land on `UNKNOWN` unless the caller has more specific context
  /// and overrides the mapping itself.
  pub fn from_client_error(e: ClientError) -> Self {
    match &e {
      ClientError::Connect(_) => CliExit::new(DAEMON_UNREACHABLE, format!("{e}")),
      ClientError::Timeout(_) => CliExit::new(DAEMON_UNREACHABLE, format!("{e}")),
      _ => CliExit::new(UNKNOWN, format!("{e}")),
    }
  }
}

impl std::fmt::Display for CliExit {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.message {
      Some(m) => write!(f, "{m}"),
      None => write!(f, "exit {}", self.code),
    }
  }
}

impl std::error::Error for CliExit {}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  #[test]
  fn prefix_walks_source_chain_into_message() {
    #[derive(Debug, thiserror::Error)]
    #[error("outer wrapper")]
    struct Outer(#[source] Middle);
    #[derive(Debug, thiserror::Error)]
    #[error("middle layer")]
    struct Middle(#[source] std::io::Error);

    let err = Outer(Middle(std::io::Error::other("inner io")));
    let exit = CliExit::prefix(INIT_DOWNLOAD_FAILED, "init download", err);
    let msg = exit.message.expect("prefix always sets a message");
    assert!(msg.contains("init download:"), "got: {msg}");
    assert!(msg.contains("outer wrapper"), "got: {msg}");
    assert!(msg.contains("middle layer"), "got: {msg}");
    assert!(msg.contains("inner io"), "got: {msg}");
  }

  #[test]
  fn distinct_codes_per_failure_class() {
    let codes = [
      SUCCESS,
      USAGE,
      DAEMON_UNREACHABLE,
      MODEL_NOT_FOUND,
      LAUNCH_FAILED,
      STOP_FAILED,
      PULL_FAILED,
      BINARY_NOT_FOUND,
      UNKNOWN,
      INIT_ABORTED,
      INIT_DOWNLOAD_FAILED,
      INIT_SMOKE_FAILED,
    ];
    let mut sorted = codes.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
      sorted.len(),
      codes.len(),
      "exit-code constants must be distinct"
    );
  }

  #[test]
  fn from_client_error_maps_connect_to_daemon_unreachable() {
    let io = std::io::Error::from(std::io::ErrorKind::NotFound);
    let exit = CliExit::from_client_error(ClientError::Connect(io));
    assert_eq!(exit.code, DAEMON_UNREACHABLE);
  }

  #[test]
  fn from_client_error_maps_timeout_to_daemon_unreachable() {
    let exit = CliExit::from_client_error(ClientError::Timeout(Duration::from_secs(1)));
    assert_eq!(exit.code, DAEMON_UNREACHABLE);
  }

  #[test]
  fn from_client_error_wildcard_maps_to_unknown() {
    // Catch-all arm: Frame / Encode / Decode / Remote all collapse to
    // UNKNOWN so callers that don't have method-specific context get
    // a consistent exit. Pin the contract.
    use crate::ipc::protocol::{ErrorCode, ErrorObject};
    use std::io::{Error, ErrorKind};

    let frame_err = ClientError::Frame(crate::ipc::framing::FrameError::Io(Error::from(
      ErrorKind::UnexpectedEof,
    )));
    assert_eq!(CliExit::from_client_error(frame_err).code, UNKNOWN);

    let decode_err = ClientError::Decode(serde_json::from_str::<()>("{").unwrap_err());
    assert_eq!(CliExit::from_client_error(decode_err).code, UNKNOWN);

    let remote_err = ClientError::Remote(ErrorObject::new(
      ErrorCode::InternalError,
      "synthetic remote".to_string(),
    ));
    assert_eq!(CliExit::from_client_error(remote_err).code, UNKNOWN);
  }
}
