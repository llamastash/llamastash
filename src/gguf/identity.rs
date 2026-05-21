//! Stable model identity = (canonical path, BLAKE3 of header bytes).
//!
//! Why a header hash instead of a whole-file hash:
//! - Whole-file hashing of a 7B GGUF is ~5 GB of disk I/O. The launcher
//!   touches identity on every scan, which would brick discovery.
//! - The header is small (<1 MiB typical) and is the part of the file that
//!   uniquely identifies the model (arch, tensors layout, quant tags).
//! - Identity must survive a `mv` of the file. Path-only identity does not;
//!   header-hash + canonical-path lets us detect a renamed file and fold
//!   its last-params (Unit 5) onto the new path.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Stable identifier for a single GGUF on disk.
///
/// Serialised in `state.json` (Unit 5) as `{ "path": "...",
/// "header_blake3": "<hex>" }` so manual inspection is friendly and
/// the diff against a renamed model is human-readable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelId {
  /// Canonical absolute path (`std::fs::canonicalize`).
  pub path: PathBuf,
  /// BLAKE3 hash of the structural header bytes (the `raw` field returned
  /// by [`crate::gguf::header::read_path`]).
  #[serde(with = "blake3_hex")]
  pub header_blake3: [u8; 32],
}

mod blake3_hex {
  use serde::{Deserialize, Deserializer, Serializer};

  pub fn serialize<S: Serializer>(bytes: &[u8; 32], ser: S) -> Result<S::Ok, S::Error> {
    let mut hex = String::with_capacity(64);
    for b in bytes {
      hex.push_str(&format!("{b:02x}"));
    }
    ser.serialize_str(&hex)
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<[u8; 32], D::Error> {
    let s = String::deserialize(de)?;
    if s.len() != 64 {
      return Err(serde::de::Error::custom(format!(
        "expected 64-char hex BLAKE3 digest, got {} chars",
        s.len()
      )));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
      let pair = std::str::from_utf8(chunk).map_err(serde::de::Error::custom)?;
      out[i] = u8::from_str_radix(pair, 16).map_err(serde::de::Error::custom)?;
    }
    Ok(out)
  }
}

impl ModelId {
  /// Lower-case hex view of the BLAKE3 digest, suitable for filenames and
  /// log lines.
  pub fn header_hex(&self) -> String {
    let mut out = String::with_capacity(64);
    for byte in &self.header_blake3 {
      out.push_str(&format!("{byte:02x}"));
    }
    out
  }

  /// Short fingerprint (first 8 hex chars). Used in CLI output and TUI
  /// status rows where the full 64-char digest is too noisy.
  pub fn short_fingerprint(&self) -> String {
    let mut out = String::with_capacity(8);
    for byte in self.header_blake3.iter().take(4) {
      out.push_str(&format!("{byte:02x}"));
    }
    out
  }
}

/// Compute a [`ModelId`] from the supplied path and the raw header bytes
/// returned by [`crate::gguf::header::read_path`].
///
/// `path` is canonicalised via [`std::fs::canonicalize`] when possible;
/// when the file does not exist (in tests that build only an in-memory
/// header), we fall back to the path as supplied.
///
/// HuggingFace cache exception: when canonicalisation strips the
/// `.gguf` extension (the HF hub layout, where canonical targets are
/// sha256-named blobs surfaced via `snapshots/<rev>/<name>.gguf`
/// symlinks), keep the supplied symlink path. The blob path looks
/// like an id to users, breaks llama.cpp's split-aware filename
/// parser, and produces useless log filenames downstream. Identity
/// equality across renames is still anchored on `header_blake3`, so
/// keeping a non-canonical path here does not weaken the rename
/// detection contract.
pub fn compute<P: AsRef<Path>>(path: P, header_bytes: &[u8]) -> ModelId {
  let supplied = path.as_ref().to_path_buf();
  let canonical = std::fs::canonicalize(&supplied).unwrap_or_else(|_| supplied.clone());
  let resolved = if canonical.extension().and_then(|s| s.to_str()) == Some("gguf") {
    canonical
  } else {
    supplied
  };
  let digest = blake3::hash(header_bytes);
  ModelId {
    path: resolved,
    header_blake3: *digest.as_bytes(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  #[test]
  fn same_bytes_yield_same_hash() {
    let bytes = b"GGUF\x03\x00\x00\x00";
    let a = compute("/tmp/llamastash-fake-a.gguf", bytes);
    let b = compute("/tmp/llamastash-fake-b.gguf", bytes);
    assert_eq!(a.header_blake3, b.header_blake3);
    assert_ne!(a.path, b.path);
  }

  #[test]
  fn hash_is_stable_across_rename() {
    let dir = tempdir_for_test();
    let a = dir.join("alpha.gguf");
    let b = dir.join("beta.gguf");
    let bytes = b"GGUF\x03\x00\x00\x00 some header payload".to_vec();
    fs::write(&a, &bytes).unwrap();
    let id_a = compute(&a, &bytes);
    fs::rename(&a, &b).unwrap();
    let id_b = compute(&b, &bytes);
    assert_eq!(id_a.header_blake3, id_b.header_blake3);
    assert_ne!(id_a.path, id_b.path);
  }

  #[cfg(unix)]
  #[test]
  fn hf_blob_symlink_keeps_named_path() {
    // Regression: when the supplied path is the HuggingFace
    // snapshot symlink (named `*.gguf`) and the canonical target is
    // a sha256-named blob (no extension), `compute` must keep the
    // symlink path. Otherwise it leaks the blob path into
    // state.json, status output, log filenames, and llama-server's
    // `-m` flag — the last of which trips the split-aware loader
    // with `invalid split file name` and surfaces the model in the
    // TUI as a sha256 id instead of its real name.
    let dir = tempdir_for_test();
    let blob = dir.join("403434e5c8454520");
    let bytes = b"GGUF\x03\x00\x00\x00 hf-cache header".to_vec();
    fs::write(&blob, &bytes).unwrap();
    let named = dir.join("qwen2.5-32b-q4_k_m-00001-of-00005.gguf");
    std::os::unix::fs::symlink(&blob, &named).unwrap();

    let id = compute(&named, &bytes);
    assert_eq!(
      id.path.file_name().and_then(|s| s.to_str()),
      Some("qwen2.5-32b-q4_k_m-00001-of-00005.gguf"),
      "blob target should not replace the symlink path: got {:?}",
      id.path,
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn user_alias_symlink_still_canonicalises() {
    // The HF-blob carve-out must not regress the user-alias case:
    // when both source and target end in `.gguf`, the canonical
    // (target) path is the right identity so multiple aliases of
    // the same file collapse to a single row.
    let dir = tempdir_for_test();
    let real = dir.join("model.gguf");
    let bytes = b"GGUF\x03\x00\x00\x00 user header".to_vec();
    fs::write(&real, &bytes).unwrap();
    let alias = dir.join("alias.gguf");
    std::os::unix::fs::symlink(&real, &alias).unwrap();

    let id = compute(&alias, &bytes);
    assert_eq!(
      id.path,
      fs::canonicalize(&real).unwrap(),
      "user-managed gguf alias must resolve to the canonical target"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn short_fingerprint_is_eight_hex_chars() {
    let id = compute("/tmp/x.gguf", b"abc");
    assert_eq!(id.short_fingerprint().len(), 8);
    assert!(id
      .short_fingerprint()
      .chars()
      .all(|c| c.is_ascii_hexdigit()));
  }

  fn tempdir_for_test() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
      "llamastash-id-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    fs::create_dir_all(&base).unwrap();
    base
  }
}
