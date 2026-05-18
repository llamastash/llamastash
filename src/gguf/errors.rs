//! Error variants surfaced by the GGUF parser, identity, and estimator.
//!
//! Kept as a small explicit enum rather than `anyhow::Error` because callers
//! in the daemon (Unit 5 supervisor, Unit 4 scanner) need to distinguish
//! "this file is not a GGUF" from "the file is truncated" from "I/O failure"
//! when deciding whether to drop a file from the list or surface a warning.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Errors produced while reading or interpreting a GGUF file's header.
#[derive(Debug, Error)]
pub enum GgufError {
  /// Underlying I/O failure (open / read / seek).
  #[error("gguf I/O error: {0}")]
  Io(#[from] io::Error),
  /// File path that triggered the error, when relevant. Optional context.
  #[error("gguf I/O error at {}: {source}", path.display())]
  IoAt {
    path: PathBuf,
    #[source]
    source: io::Error,
  },
  /// First four bytes did not match `GGUF`.
  #[error("not a GGUF file (magic mismatch)")]
  BadMagic,
  /// File begins with `GGUF` but advertises a version this build does not
  /// understand. We support v2 and v3.
  #[error("unsupported GGUF version: {0} (supported: 2, 3)")]
  UnsupportedVersion(u32),
  /// Reader hit EOF before finishing the structural read it was attempting
  /// (magic + version + counts + KV list + tensor info).
  #[error("gguf header truncated: needed {needed} bytes, got {got}")]
  Truncated { needed: usize, got: usize },
  /// Header advertises a structure larger than the configured cap. Bounded
  /// to avoid OOM on hostile or corrupt files.
  #[error("gguf header advertises {advertised} bytes which exceeds cap {cap}")]
  HeaderTooLarge { advertised: u64, cap: u64 },
  /// Encountered a metadata-value type tag this parser does not understand.
  /// (GGUF reserves a small enum; anything outside it is a sign of corruption
  /// or a newer spec.)
  #[error("unknown gguf value-type tag: {0}")]
  BadValueType(u32),
  /// A GGUF string length is implausibly large (would not fit in the
  /// remaining header window).
  #[error("gguf string length out of range: {0}")]
  BadStringLen(u64),
  /// A non-UTF-8 byte sequence appeared where the GGUF spec requires UTF-8.
  #[error("gguf string contained invalid UTF-8")]
  BadUtf8,
  /// `Array(Array(...))` value-types nested past the configured cap.
  /// The parser short-circuits well before stack overflow rather than
  /// crashing the worker. Bounded because a malicious file inside the
  /// 1 MiB header cap can still describe ~87 000 levels of nesting at
  /// 12 bytes per level.
  #[error("gguf array nested {depth} levels, exceeds cap {cap}")]
  ArrayNestingTooDeep { depth: usize, cap: usize },
}

// Display + Error + From<io::Error> are derived above via `#[derive(Error)]`
// + `#[from]` so the previous hand-rolled impls have moved into the derive.

/// Convenience alias used across the `gguf` module.
pub type GgufResult<T> = Result<T, GgufError>;
