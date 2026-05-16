//! Integration tests for the split-GGUF shard grouper. Exercises the
//! public `discovery::split_gguf` surface (`group`, `parse_shard_name`)
//! without going through the filesystem walker.

use std::path::PathBuf;

use llamatui::discovery::split_gguf::{group, parse_shard_name, DiscoveredEntry};

#[test]
fn end_to_end_grouping_for_a_typical_huggingface_layout() {
  // The shape an `hf-hub` cache surfaces for a sharded conversion: each
  // shard is a sibling under `snapshots/<rev>/`. The scanner sees them
  // flat and the grouper collapses them into one entry.
  let snapshots = PathBuf::from("/cache/huggingface/hub/models--Qwen--Qwen2.5/snapshots/abc");
  let paths = vec![
    snapshots.join("Qwen2.5-32B-Instruct-Q4_K_M-00001-of-00005.gguf"),
    snapshots.join("Qwen2.5-32B-Instruct-Q4_K_M-00002-of-00005.gguf"),
    snapshots.join("Qwen2.5-32B-Instruct-Q4_K_M-00003-of-00005.gguf"),
    snapshots.join("Qwen2.5-32B-Instruct-Q4_K_M-00004-of-00005.gguf"),
    snapshots.join("Qwen2.5-32B-Instruct-Q4_K_M-00005-of-00005.gguf"),
    snapshots.join("README.gguf-not-a-gguf.txt"),
  ];
  // The non-gguf file isn't part of the grouper input — discovery
  // filters by extension before calling group. But verify the grouper
  // doesn't choke on a non-shard name either:
  let entries = group(paths);
  assert_eq!(entries.len(), 2, "one Split + one Single passthrough");

  // Find the Split and the Single explicitly; order can vary.
  let split = entries
    .iter()
    .find(|e| matches!(e, DiscoveredEntry::Split(_)))
    .expect("a Split entry must be present");
  let single = entries
    .iter()
    .find(|e| matches!(e, DiscoveredEntry::Single(_)))
    .expect("the non-shard file must round-trip as Single");
  match split {
    DiscoveredEntry::Split(g) => {
      assert_eq!(g.base, "Qwen2.5-32B-Instruct-Q4_K_M");
      assert_eq!(g.total, 5);
      assert_eq!(g.shards.len(), 5);
      assert!(g.complete);
      assert!(g
        .launch_path
        .to_string_lossy()
        .ends_with("Qwen2.5-32B-Instruct-Q4_K_M-00001-of-00005.gguf"));
    }
    other => panic!("expected Split, got {other:?}"),
  }
  match single {
    DiscoveredEntry::Single(path) => {
      assert!(
        path.to_string_lossy().ends_with("README.gguf-not-a-gguf.txt"),
        "unexpected Single passthrough path: {}",
        path.display()
      );
    }
    other => panic!("expected Single, got {other:?}"),
  }
}

#[test]
fn shard_name_parser_round_trip() {
  let info =
    parse_shard_name("Mixtral-8x22B-Q5_K_M-00007-of-00012.gguf").expect("canonical shard parses");
  assert_eq!(info.base, "Mixtral-8x22B-Q5_K_M");
  assert_eq!(info.index, 7);
  assert_eq!(info.total, 12);
}
