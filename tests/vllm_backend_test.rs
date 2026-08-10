//! vLLM backend integration coverage.
//!
//! The neutrality guard here is the counterpart to the one inside
//! `src/discovery/hf_repos.rs`: that one proves the substrate names no engine,
//! this one proves the engine stays inside its own module.

use std::path::{Path, PathBuf};

/// Files allowed to name the backend, per the "adding a backend" contract in
/// `AGENTS.md`: the backend's own module, the registry, and the config
/// re-export that keeps the typed struct's path stable.
const ALLOWED: &[&str] = &[
  "src/backend/vllm/mod.rs",
  "src/backend/vllm/discovery.rs",
  "src/backend/mod.rs",
  "src/config/mod.rs",
  "src/config/loader.rs",
];

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      rust_sources(&path, out);
    } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
      out.push(path);
    }
  }
}

/// The backend id must not appear anywhere in `src/` outside the module and
/// the registration points — **in code or in comments**. Removing the backend
/// has to be deleting one directory plus a handful of registry lines.
#[test]
fn backend_id_does_not_leak_outside_its_module() {
  let root = repo_root();
  let mut files = Vec::new();
  rust_sources(&root.join("src"), &mut files);
  assert!(!files.is_empty(), "found no sources to scan");

  // Split so this test's own source cannot match when it is scanned.
  let needle = concat!("v", "llm");
  let mut leaks = Vec::new();
  for file in files {
    let rel = file
      .strip_prefix(&root)
      .unwrap_or(&file)
      .to_string_lossy()
      .replace('\\', "/");
    if ALLOWED.contains(&rel.as_str()) {
      continue;
    }
    let Ok(text) = std::fs::read_to_string(&file) else {
      continue;
    };
    if text.to_ascii_lowercase().contains(needle) {
      leaks.push(rel);
    }
  }
  assert!(
    leaks.is_empty(),
    "backend id leaked outside its module and the registration points: {leaks:?}"
  );
}

/// Every registration point is present, so the backend actually reaches the
/// generic tree rather than sitting in a module nothing dispatches to.
#[test]
fn backend_is_registered_in_the_enum_and_the_registry() {
  let registry = std::fs::read_to_string(repo_root().join("src/backend/mod.rs")).unwrap();
  let id = concat!("V", "llm");
  assert!(
    registry.contains(&format!("Backends::{id}($b) => $body")),
    "missing the for_each_backend! arm"
  );
  assert!(
    registry.contains(&format!("Backends::{id}({id}Backend::new())")),
    "missing the Backends::all() line"
  );
}
