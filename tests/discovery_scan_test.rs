//! End-to-end discovery tests: walk a real temp filesystem tree and
//! verify the streaming `mpsc` channel surfaces every model the way
//! Unit 4's plan specifies.

use std::fs;
use std::path::PathBuf;

use llamadash::discovery::scanner::{scan, ScanOptions, ScanRoot};
use llamadash::discovery::ModelSource;
use llamadash::gguf::test_fixtures::build_minimal_gguf;

fn unique_temp_dir(label: &str) -> PathBuf {
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let dir = std::env::temp_dir().join(format!(
    "llamadash-disc-{label}-{}-{nanos}",
    std::process::id()
  ));
  fs::create_dir_all(&dir).expect("temp dir");
  dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_emits_three_models_grouped_under_parent_dirs() {
  // Layout:
  //   root/
  //     models/a.gguf
  //     models/b.gguf
  //     library/c.gguf
  let root = unique_temp_dir("three");
  fs::create_dir_all(root.join("models")).unwrap();
  fs::create_dir_all(root.join("library")).unwrap();
  fs::write(root.join("models/a.gguf"), build_minimal_gguf("llama")).unwrap();
  fs::write(root.join("models/b.gguf"), build_minimal_gguf("qwen3")).unwrap();
  fs::write(root.join("library/c.gguf"), build_minimal_gguf("bert")).unwrap();

  let mut rx = scan(
    vec![ScanRoot {
      path: root.clone(),
      source: ModelSource::UserPath,
    }],
    ScanOptions::default(),
  );

  let mut seen = Vec::new();
  while let Some(m) = rx.recv().await {
    seen.push(m);
  }
  assert_eq!(seen.len(), 3, "three GGUFs across two dirs");

  // Every row should have metadata since the fixture is structurally valid.
  for m in &seen {
    assert!(m.metadata.is_some(), "minimal gguf should parse");
    assert!(m.parse_error.is_none());
  }

  // The `parent` field powers the TUI's "Models / <dir>" grouping.
  let parents: std::collections::HashSet<_> = seen.iter().map(|m| m.parent.clone()).collect();
  assert_eq!(parents.len(), 2, "rows grouped under two parent dirs");

  fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_ignores_part_files_and_non_gguf() {
  let root = unique_temp_dir("ignore");
  fs::write(root.join("ok.gguf"), build_minimal_gguf("llama")).unwrap();
  fs::write(root.join("pending.gguf.part"), b"mid-download").unwrap();
  fs::write(root.join("notes.txt"), b"unrelated").unwrap();

  let mut rx = scan(
    vec![ScanRoot {
      path: root.clone(),
      source: ModelSource::UserPath,
    }],
    ScanOptions::default(),
  );

  let mut seen = Vec::new();
  while let Some(m) = rx.recv().await {
    seen.push(m);
  }
  assert_eq!(seen.len(), 1, ".part and .txt must be skipped");
  fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_continues_when_one_root_is_missing() {
  let alive = unique_temp_dir("alive");
  fs::write(alive.join("ok.gguf"), build_minimal_gguf("llama")).unwrap();
  let dead = PathBuf::from("/nonexistent/llamadash/scan/root");

  let mut rx = scan(
    vec![
      ScanRoot {
        path: dead,
        source: ModelSource::UserPath,
      },
      ScanRoot {
        path: alive.clone(),
        source: ModelSource::HuggingFace,
      },
    ],
    ScanOptions::default(),
  );

  let mut seen = Vec::new();
  while let Some(m) = rx.recv().await {
    seen.push(m);
  }
  assert_eq!(
    seen.len(),
    1,
    "missing root logs warning; other root still scanned"
  );
  assert_eq!(seen[0].source, ModelSource::HuggingFace);
  fs::remove_dir_all(&alive).ok();
}
