//! Per-shard + total on-disk-size helpers for GGUF models.
//!
//! Single source of truth for "how big is this model on disk" so the
//! scanner, the `show` command, and any future consumer agree on the
//! number byte-for-byte. The summed value feeds:
//!
//! - The catalog row's `weights_bytes` (an upper bound on tensor
//!   bytes — the header + per-tensor padding is <1% on quant models).
//! - The SIZE column in `llamastash list` and in the right pane.
//! - The recommender's VRAM-fit predicate.
//! - The `size.on_disk_total_bytes` field of `llamastash show`.
//!
//! Synchronous file-metadata reads. Callers that hold an async runtime
//! handle wrap the call in `tokio::task::spawn_blocking` for multi-
//! shard sets so the blocking-pool stat'ing doesn't pin a worker
//! thread; see `scanner::apply_split_total_weights`.

use std::path::{Path, PathBuf};

/// One shard's path plus its on-disk byte count. `bytes` is `0` when
/// the file is missing or unreadable so a temporarily-broken sibling
/// shrinks the total rather than panicking the caller; the path is
/// always preserved so the consumer can still surface it in a UI.
#[derive(Debug, Clone)]
pub struct ShardSize {
  pub path: PathBuf,
  pub bytes: u64,
}

/// Sum on-disk bytes across `primary` + every sibling. Missing files
/// count as `0`.
pub fn on_disk_total(primary: &Path, siblings: &[PathBuf]) -> u64 {
  per_shard(primary, siblings)
    .into_iter()
    .map(|s| s.bytes)
    .fold(0u64, u64::saturating_add)
}

/// Per-shard breakdown: `[shard 1, shard 2, …, shard N]` in the order
/// `(primary, siblings…)`. Missing files surface as `bytes = 0` rather
/// than dropping out of the list so the caller can render every
/// path it knows about.
pub fn per_shard(primary: &Path, siblings: &[PathBuf]) -> Vec<ShardSize> {
  let mut out = Vec::with_capacity(1 + siblings.len());
  out.push(stat_one(primary));
  for sib in siblings {
    out.push(stat_one(sib));
  }
  out
}

fn stat_one(path: &Path) -> ShardSize {
  // A whole-directory row (a safetensors snapshot) has no meaningful `len()` —
  // the inode size is ~4 KiB, which would otherwise beat the real weights
  // total the caller falls back to. Report 0 so the fallback wins.
  let bytes = std::fs::metadata(path)
    .map(|m| if m.is_dir() { 0 } else { m.len() })
    .unwrap_or(0);
  ShardSize {
    path: path.to_path_buf(),
    bytes,
  }
}

#[cfg(test)]
mod tests {
  /// A directory row must not report its inode size. The SIZE column falls
  /// back to the summed weights, and ~4 KiB would silently beat it.
  #[test]
  fn a_directory_primary_contributes_zero_not_its_inode_size() {
    let dir = crate::util::test_temp::unique_temp_dir("shard-dir");
    std::fs::write(dir.join("model.safetensors"), vec![0u8; 1024]).unwrap();
    assert_eq!(on_disk_total(&dir, &[]), 0);
    let _ = std::fs::remove_dir_all(&dir);
  }

  use super::*;

  #[test]
  fn on_disk_total_sums_every_shard_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("m.gguf");
    std::fs::write(&p, b"0123456789").unwrap(); // 10 bytes
    let s2 = dir.path().join("m-2.gguf");
    std::fs::write(&s2, b"abcdef").unwrap(); // 6 bytes
    assert_eq!(on_disk_total(&p, &[s2]), 16);
  }

  #[test]
  fn on_disk_total_treats_missing_siblings_as_zero() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("only.gguf");
    std::fs::write(&p, b"abcd").unwrap();
    let missing = dir.path().join("not-there.gguf");
    assert_eq!(on_disk_total(&p, &[missing]), 4);
  }

  #[test]
  fn per_shard_preserves_order_and_missing_paths() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.gguf");
    std::fs::write(&p, b"AA").unwrap();
    let s2 = dir.path().join("missing.gguf");
    let s3 = dir.path().join("c.gguf");
    std::fs::write(&s3, b"CCCC").unwrap();
    let breakdown = per_shard(&p, &[s2.clone(), s3.clone()]);
    assert_eq!(breakdown.len(), 3);
    assert_eq!(breakdown[0].bytes, 2);
    assert_eq!(breakdown[0].path, p);
    assert_eq!(breakdown[1].bytes, 0);
    assert_eq!(breakdown[1].path, s2);
    assert_eq!(breakdown[2].bytes, 4);
    assert_eq!(breakdown[2].path, s3);
  }
}
