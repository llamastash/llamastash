//! Error variants surfaced by the GGUF parser, identity, and estimator.
//!
//! Kept as a small explicit enum rather than `anyhow::Error` because callers
//! in the daemon (Unit 5 supervisor, Unit 4 scanner) need to distinguish
//! "this file is not a GGUF" from "the file is truncated" from "I/O failure"
//! when deciding whether to drop a file from the list or surface a warning.

use std::io;
use std::path::PathBuf;

/// Errors produced while reading or interpreting a GGUF file's header.
#[derive(Debug)]
pub enum GgufError {
  /// Underlying I/O failure (open / read / seek).
  Io(io::Error),
  /// File path that triggered the error, when relevant. Optional context.
  IoAt { path: PathBuf, source: io::Error },
  /// First four bytes did not match `GGUF`.
  BadMagic,
  /// File begins with `GGUF` but advertises a version this build does not
  /// understand. We support v2 and v3.
  UnsupportedVersion(u32),
  /// Reader hit EOF before finishing the structural read it was attempting
  /// (magic + version + counts + KV list + tensor info).
  Truncated { needed: usize, got: usize },
  /// Header advertises a structure larger than the configured cap. Bounded
  /// to avoid OOM on hostile or corrupt files.
  HeaderTooLarge { advertised: u64, cap: u64 },
  /// Encountered a metadata-value type tag this parser does not understand.
  /// (GGUF reserves a small enum; anything outside it is a sign of corruption
  /// or a newer spec.)
  BadValueType(u32),
  /// A GGUF string length is implausibly large (would not fit in the
  /// remaining header window).
  BadStringLen(u64),
  /// A non-UTF-8 byte sequence appeared where the GGUF spec requires UTF-8.
  BadUtf8,
  /// `Array(Array(...))` value-types nested past the configured cap.
  /// The parser short-circuits well before stack overflow rather than
  /// crashing the worker. Bounded because a malicious file inside the
  /// 1 MiB header cap can still describe ~87 000 levels of nesting at
  /// 12 bytes per level.
  ArrayNestingTooDeep { depth: usize, cap: usize },
}

impl std::fmt::Display for GgufError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      GgufError::Io(e) => write!(f, "gguf I/O error: {e}"),
      GgufError::IoAt { path, source } => {
        write!(f, "gguf I/O error at {}: {source}", path.display())
      }
      GgufError::BadMagic => write!(f, "not a GGUF file (magic mismatch)"),
      GgufError::UnsupportedVersion(v) => {
        write!(f, "unsupported GGUF version: {v} (supported: 2, 3)")
      }
      GgufError::Truncated { needed, got } => {
        write!(f, "gguf header truncated: needed {needed} bytes, got {got}")
      }
      GgufError::HeaderTooLarge { advertised, cap } => write!(
        f,
        "gguf header advertises {advertised} bytes which exceeds cap {cap}"
      ),
      GgufError::BadValueType(t) => write!(f, "unknown gguf value-type tag: {t}"),
      GgufError::BadStringLen(n) => write!(f, "gguf string length out of range: {n}"),
      GgufError::BadUtf8 => write!(f, "gguf string contained invalid UTF-8"),
      GgufError::ArrayNestingTooDeep { depth, cap } => {
        write!(f, "gguf array nested {depth} levels, exceeds cap {cap}")
      }
    }
  }
}

impl std::error::Error for GgufError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      GgufError::Io(e) => Some(e),
      GgufError::IoAt { source, .. } => Some(source),
      _ => None,
    }
  }
}

impl From<io::Error> for GgufError {
  fn from(e: io::Error) -> Self {
    GgufError::Io(e)
  }
}

/// Convenience alias used across the `gguf` module.
pub type GgufResult<T> = Result<T, GgufError>;
