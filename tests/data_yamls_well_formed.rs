//! CI gate: the YAML side-files the snapshot regen depends on
//! (`data/task-hints.yaml`, `data/gguf-publisher-allowlist.yaml`) must
//! parse cleanly with the expected top-level shape. Catches the
//! "maintainer broke YAML during a refresh" failure mode before a
//! daily-regen run picks up the change and fails opaquely.
//!
//! Unit 4 of docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md.
//! The plan called for a build.rs check; a `#[test]` parse is
//! equivalent for the CI gate (cargo test runs in the same workflow)
//! without adding build-deps for a file the binary doesn't bundle.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct TaskHints {
  prefixes: BTreeMap<String, Vec<String>>,
  defaults: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PublisherAllowlist {
  allowlist: Vec<String>,
}

const TASK_HINTS_YAML: &str = include_str!("../data/task-hints.yaml");
const PUBLISHER_ALLOWLIST_YAML: &str = include_str!("../data/gguf-publisher-allowlist.yaml");

#[test]
fn task_hints_yaml_parses_with_expected_shape() {
  let hints: TaskHints =
    serde_yaml::from_str(TASK_HINTS_YAML).expect("data/task-hints.yaml must be well-formed YAML");
  assert!(
    !hints.prefixes.is_empty(),
    "data/task-hints.yaml has no prefixes — at least one curated entry is required"
  );
  assert!(
    !hints.defaults.is_empty(),
    "data/task-hints.yaml `defaults` list must not be empty"
  );
  // Every tag in every list must be a recognised task hint — the
  // recommender's task filter only knows three keys today.
  let allowed = ["code", "reasoning", "general"];
  for (prefix, tags) in &hints.prefixes {
    assert!(
      !tags.is_empty(),
      "prefix {prefix:?} maps to an empty tag list"
    );
    for tag in tags {
      assert!(
        allowed.contains(&tag.as_str()),
        "prefix {prefix:?} carries unknown tag {tag:?} — extend `allowed` here and the recommender's task filter together"
      );
    }
  }
  for tag in &hints.defaults {
    assert!(
      allowed.contains(&tag.as_str()),
      "defaults carries unknown tag {tag:?}"
    );
  }
}

#[test]
fn gguf_publisher_allowlist_yaml_parses() {
  let list: PublisherAllowlist = serde_yaml::from_str(PUBLISHER_ALLOWLIST_YAML)
    .expect("data/gguf-publisher-allowlist.yaml must be well-formed YAML");
  assert!(
    !list.allowlist.is_empty(),
    "data/gguf-publisher-allowlist.yaml `allowlist` must list at least one trusted publisher"
  );
  // Publishers are HF orgs — no slashes, no whitespace, non-empty.
  for org in &list.allowlist {
    assert!(!org.is_empty(), "empty publisher entry");
    assert!(
      !org.contains('/'),
      "publisher {org:?} should be a bare HF org (no slash) — repo paths belong in the snapshot, not the allowlist"
    );
    assert!(
      !org.chars().any(char::is_whitespace),
      "publisher {org:?} contains whitespace"
    );
  }
}
