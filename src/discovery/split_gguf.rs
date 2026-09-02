//! Detect and group split-GGUF shards.
//!
//! When a model is too large to fit in a single file, `llama.cpp`'s
//! conversion tools emit it as a sequence of shards named
//! `<base>-NNNNN-of-MMMMM.gguf`. To `llama-server`, the shard set is
//! launched by pointing it at shard 1; the loader follows the
//! `-of-MMMMM` suffix to find siblings. Discovery collapses
//! the shard set into a single user-visible row so the TUI shows one
//! model entry rather than five lines of `*-00001-of-00005.gguf` …
//! `*-00005-of-00005.gguf`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

/// Canonical split-shard filename pattern. Capture groups:
/// 1. `base`   — everything before `-NNNNN-of-MMMMM`
/// 2. `index`  — current shard index (1-based, zero-padded to 5)
/// 3. `total`  — total shards in the set (zero-padded to 5)
fn shard_regex() -> &'static Regex {
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| {
    Regex::new(r"^(.+)-(\d{5})-of-(\d{5})\.gguf$")
      .expect("split-gguf regex is a compile-time constant")
  })
}

/// The filename a split set is known by: shard 1's name with the
/// `-NNNNN-of-NNNNN` marker removed, extension kept.
///
/// A split model is one model, so every user-visible surface names it this way
/// rather than after whichever shard happens to be the entry point. `None` for
/// a name that is not a shard, so callers fall back to the filename as-is.
pub fn collapsed_file_name(file_name: &str) -> Option<String> {
  parse_shard_name(file_name).map(|s| format!("{}.gguf", s.base))
}

/// One parsed shard filename. `index` is 1-based, matching the
/// on-disk numbering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardInfo {
  pub base: String,
  pub index: u32,
  pub total: u32,
}

/// Parse a filename (no directory component) into a [`ShardInfo`] if it
/// matches the canonical split-shard naming convention. `None` means
/// "this is a regular GGUF, not a shard".
pub fn parse_shard_name(name: &str) -> Option<ShardInfo> {
  let caps = shard_regex().captures(name)?;
  let base = caps.get(1)?.as_str().to_string();
  let index = caps.get(2)?.as_str().parse::<u32>().ok()?;
  let total = caps.get(3)?.as_str().parse::<u32>().ok()?;
  // index 0 and total 0 are nonsensical; reject them so callers don't
  // have to handle the edge case.
  if index == 0 || total == 0 {
    return None;
  }
  Some(ShardInfo { base, index, total })
}

/// Output of grouping a list of GGUF paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredEntry {
  /// A standalone GGUF file (or a shard whose siblings weren't seen).
  Single(PathBuf),
  /// A split-shard set sharing the same base prefix and total count.
  Split(SplitGroup),
}

/// A group of shards that together form a single model. `launch_path`
/// is the file the supervisor should pass to `llama-server` (`-m`); on
/// a healthy set this is shard 1 (`*-00001-of-NNNNN.gguf`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitGroup {
  /// Common prefix (e.g. `Qwen2.5-Coder-32B-Instruct-Q4_K_M` for files
  /// named `Qwen2.5-Coder-32B-Instruct-Q4_K_M-00001-of-00005.gguf` …).
  pub base: String,
  /// Total shard count advertised by the filenames (e.g. 5).
  pub total: u32,
  /// All shards in the set, sorted by index ascending. May be sparse
  /// if some shards are missing on disk.
  pub shards: Vec<PathBuf>,
  /// File to hand to `llama-server`'s `-m` flag. Shard 1 when present;
  /// the lowest-index shard otherwise. The supervisor can warn if this
  /// isn't shard 1 (`complete == false`).
  pub launch_path: PathBuf,
  /// `true` when every shard from 1..=total is present on disk.
  pub complete: bool,
}

/// Group a flat list of GGUF paths into singles and shard groups.
///
/// Input order is preserved within `Single` entries; `Split` entries
/// appear in the position of their lowest-index shard. Files whose
/// name doesn't match the shard regex pass through as `Single`.
///
/// Sibling detection keys on `(base, total)`: two files sharing the
/// same prefix but disagreeing on total are treated as unrelated sets.
pub fn group(paths: impl IntoIterator<Item = PathBuf>) -> Vec<DiscoveredEntry> {
  // Bucket by (parent_dir, base, total). Sibling shards must live in
  // the same directory — a coincidental name collision under two
  // unrelated trees should not group.
  use std::collections::BTreeMap;
  type Key = (PathBuf, String, u32);

  let mut singles: Vec<(usize, PathBuf)> = Vec::new();
  let mut buckets: BTreeMap<Key, (usize, Vec<(u32, PathBuf)>)> = BTreeMap::new();

  for (insertion_order, path) in paths.into_iter().enumerate() {
    let name = match path.file_name().and_then(|n| n.to_str()) {
      Some(n) => n,
      None => {
        singles.push((insertion_order, path));
        continue;
      }
    };
    match parse_shard_name(name) {
      Some(info) => {
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let entry = buckets
          .entry((parent, info.base, info.total))
          .or_insert_with(|| (insertion_order, Vec::new()));
        entry.0 = entry.0.min(insertion_order);
        entry.1.push((info.index, path));
      }
      None => singles.push((insertion_order, path)),
    }
  }

  // Flatten buckets into Split entries (or back into Singles for the
  // degenerate "only one shard observed" case — there's nothing to
  // group there, the launch story is identical to a regular file).
  let mut combined: Vec<(usize, DiscoveredEntry)> = Vec::new();
  for ((_parent, base, total), (first_seen, mut shards)) in buckets {
    if shards.len() == 1 {
      let (_, p) = shards.pop().expect("len-1 vec");
      combined.push((first_seen, DiscoveredEntry::Single(p)));
      continue;
    }
    shards.sort_by_key(|(i, _)| *i);
    let launch_path = shards
      .iter()
      .find(|(i, _)| *i == 1)
      .map(|(_, p)| p.clone())
      .unwrap_or_else(|| shards[0].1.clone());
    let complete = shards.len() as u32 == total
      && shards.first().map(|(i, _)| *i) == Some(1)
      && shards.last().map(|(i, _)| *i) == Some(total);
    let shard_paths = shards.into_iter().map(|(_, p)| p).collect();
    combined.push((
      first_seen,
      DiscoveredEntry::Split(SplitGroup {
        base,
        total,
        shards: shard_paths,
        launch_path,
        complete,
      }),
    ));
  }
  for (order, p) in singles {
    combined.push((order, DiscoveredEntry::Single(p)));
  }
  combined.sort_by_key(|(order, _)| *order);
  combined.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_canonical_shard_name() {
    let info = parse_shard_name("Qwen2.5-Coder-32B-Q4_K_M-00001-of-00005.gguf")
      .expect("canonical shard parses");
    assert_eq!(info.base, "Qwen2.5-Coder-32B-Q4_K_M");
    assert_eq!(info.index, 1);
    assert_eq!(info.total, 5);
  }

  #[test]
  fn rejects_non_shard_filename() {
    assert!(parse_shard_name("tinyllama-Q4.gguf").is_none());
    assert!(
      parse_shard_name("model-1-of-5.gguf").is_none(),
      "needs 5-digit zero-padding"
    );
    assert!(
      parse_shard_name("model-00001-of-00000.gguf").is_none(),
      "rejects total=0"
    );
    assert!(
      parse_shard_name("model-00000-of-00005.gguf").is_none(),
      "rejects index=0"
    );
    assert!(
      parse_shard_name("model-00001-of-00005.txt").is_none(),
      "wrong extension"
    );
  }

  #[test]
  fn groups_full_shard_set_under_one_entry() {
    let dir = PathBuf::from("/models");
    let paths = vec![
      dir.join("alpha-00001-of-00003.gguf"),
      dir.join("alpha-00002-of-00003.gguf"),
      dir.join("alpha-00003-of-00003.gguf"),
    ];
    let entries = group(paths.clone());
    assert_eq!(entries.len(), 1);
    match &entries[0] {
      DiscoveredEntry::Split(g) => {
        assert_eq!(g.base, "alpha");
        assert_eq!(g.total, 3);
        assert_eq!(g.shards.len(), 3);
        assert_eq!(g.launch_path, paths[0]);
        assert!(g.complete);
      }
      other => panic!("expected Split, got {other:?}"),
    }
  }

  #[test]
  fn shards_with_missing_shard_1_fall_back_to_lowest_index() {
    let dir = PathBuf::from("/models");
    let paths = vec![
      dir.join("alpha-00002-of-00003.gguf"),
      dir.join("alpha-00003-of-00003.gguf"),
    ];
    let entries = group(paths.clone());
    match &entries[0] {
      DiscoveredEntry::Split(g) => {
        assert_eq!(g.launch_path, paths[0]);
        assert!(!g.complete, "missing shard 1 → not complete");
      }
      other => panic!("expected Split, got {other:?}"),
    }
  }

  #[test]
  fn lone_shard_falls_back_to_single() {
    // A single shard file with no siblings has nothing to group; it
    // surfaces as a normal Single entry so the user still sees it.
    let p = PathBuf::from("/models/alpha-00001-of-00005.gguf");
    let entries = group(vec![p.clone()]);
    assert_eq!(entries, vec![DiscoveredEntry::Single(p)]);
  }

  #[test]
  fn shards_in_different_dirs_dont_cross_group() {
    let paths = vec![
      PathBuf::from("/a/model-00001-of-00002.gguf"),
      PathBuf::from("/b/model-00001-of-00002.gguf"),
      PathBuf::from("/a/model-00002-of-00002.gguf"),
      PathBuf::from("/b/model-00002-of-00002.gguf"),
    ];
    let entries = group(paths);
    assert_eq!(entries.len(), 2, "two independent shard sets, two groups");
    for e in &entries {
      match e {
        DiscoveredEntry::Split(g) => assert!(g.complete),
        other => panic!("expected Split, got {other:?}"),
      }
    }
  }

  #[test]
  fn mismatched_total_treated_as_separate_sets() {
    // Same base prefix but disagreeing on `total`: these are two
    // different conversions of the same model. Group them separately.
    let dir = PathBuf::from("/models");
    let paths = vec![
      dir.join("alpha-00001-of-00003.gguf"),
      dir.join("alpha-00001-of-00005.gguf"),
      dir.join("alpha-00002-of-00003.gguf"),
    ];
    let entries = group(paths);
    let mut split_count = 0;
    let mut single_count = 0;
    for e in &entries {
      match e {
        DiscoveredEntry::Split(_) => split_count += 1,
        DiscoveredEntry::Single(_) => single_count += 1,
      }
    }
    assert_eq!(split_count, 1, "alpha-of-3 with two shards groups");
    assert_eq!(single_count, 1, "alpha-of-5 with one shard stays single");
  }

  #[test]
  fn singles_and_splits_preserve_relative_order() {
    let dir = PathBuf::from("/models");
    let paths = vec![
      dir.join("first.gguf"),
      dir.join("alpha-00001-of-00002.gguf"),
      dir.join("between.gguf"),
      dir.join("alpha-00002-of-00002.gguf"),
      dir.join("last.gguf"),
    ];
    let entries = group(paths);
    assert_eq!(entries.len(), 4);
    match &entries[0] {
      DiscoveredEntry::Single(p) => assert_eq!(p.file_name().unwrap(), "first.gguf"),
      other => panic!("expected Single first, got {other:?}"),
    }
    match &entries[1] {
      DiscoveredEntry::Split(g) => assert_eq!(g.base, "alpha"),
      other => panic!("expected Split alpha, got {other:?}"),
    }
    match &entries[2] {
      DiscoveredEntry::Single(p) => assert_eq!(p.file_name().unwrap(), "between.gguf"),
      other => panic!("expected Single between, got {other:?}"),
    }
    match &entries[3] {
      DiscoveredEntry::Single(p) => assert_eq!(p.file_name().unwrap(), "last.gguf"),
      other => panic!("expected Single last, got {other:?}"),
    }
  }
}
