//! Compose `llama-server` argv from the user's launch choices.
//!
//! Order matters: `--host 127.0.0.1` and `--port` come first so the
//! command line reads well in logs; then `-m <path>`, then mode flags
//! (`--embeddings` / `--reranking`), then reasoning bundle
//! (`--jinja --reasoning-format deepseek`), then `-c <ctx>`, then any
//! user-supplied advanced flags. Advanced flags land *last* so they
//! always trump bundled ones — that's the contract documented on the
//! TUI's "Advanced" panel.
//!
//! `validate_advanced` enforces the loopback-only and same-UID contract:
//! a curated denylist (`--host`, `--listen`, `--bind`, `--api-key`,
//! `--ssl-*`) is refused. llama-server honours the last-occurrence of a
//! flag, so without this guard a trailing `--host 0.0.0.0` in `advanced`
//! would expose the model to the LAN.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::launch::mode::LaunchMode;

/// Flags refused in `LaunchParams.advanced` because they would break
/// the loopback-only / same-UID security contract documented in
/// `docs/architecture.md`. Match is case-insensitive on the flag
/// itself; `--ssl-*` matches any flag starting with that prefix.
pub const FORBIDDEN_ADVANCED_PREFIXES: &[&str] = &[
  "--host",
  "--listen",
  "--bind",
  "--api-key",
  "--ssl-",
];

/// Returns the subset of `advanced` flags that hit the denylist. Used
/// by IPC handlers to refuse a launch before spawn, and by `compose`
/// to defensively strip in case validation was skipped.
pub fn forbidden_in_advanced(advanced: &[OsString]) -> Vec<String> {
  advanced
    .iter()
    .filter_map(|s| {
      let lossy = s.to_string_lossy();
      let head = lossy.split('=').next().unwrap_or(&lossy);
      let lower = head.to_ascii_lowercase();
      if FORBIDDEN_ADVANCED_PREFIXES
        .iter()
        .any(|p| lower == *p || (p.ends_with('-') && lower.starts_with(p)))
      {
        Some(lossy.into_owned())
      } else {
        None
      }
    })
    .collect()
}

/// All launch knobs the supervisor reads. Persisted under
/// `last_params: HashMap<ModelId, LaunchParams>` in `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchParams {
  /// Absolute path to the GGUF the user picked (or shard 1 for split
  /// sets).
  pub model_path: PathBuf,
  /// Chosen launch mode (chat / embedding / rerank).
  pub mode: LaunchMode,
  /// Context length. `None` lets `llama-server` use the GGUF's
  /// native value (no `-c` flag).
  pub ctx: Option<u32>,
  /// Listening port. `None` leaves port allocation to the supervisor.
  pub port: Option<u16>,
  /// Reasoning bundle on/off. When `true`, supervisor appends
  /// `--jinja --reasoning-format deepseek` to the argv.
  pub reasoning: bool,
  /// Free-form pass-through flags. The TUI's advanced panel and the
  /// CLI's `-- ...` tail both flow into here.
  pub advanced: Vec<OsString>,
}

impl LaunchParams {
  pub fn new(model_path: PathBuf, mode: LaunchMode) -> Self {
    Self {
      model_path,
      mode,
      ctx: None,
      port: None,
      reasoning: false,
      advanced: Vec::new(),
    }
  }
}

/// Materialise the argv `Command::args(...)` will hand to
/// `llama-server`. Caller passes the resolved listening port
/// separately because allocation happens in the supervisor, not in
/// `LaunchParams`.
pub fn compose(params: &LaunchParams, allocated_port: u16) -> Vec<OsString> {
  let mut argv: Vec<OsString> = Vec::with_capacity(16 + params.advanced.len());
  argv.push("--host".into());
  argv.push("127.0.0.1".into());
  argv.push("--port".into());
  argv.push(allocated_port.to_string().into());
  argv.push("-m".into());
  argv.push(params.model_path.clone().into());
  match params.mode {
    LaunchMode::Chat => {}
    LaunchMode::Embedding => argv.push("--embeddings".into()),
    LaunchMode::Rerank => argv.push("--reranking".into()),
  }
  if params.reasoning {
    argv.push("--jinja".into());
    argv.push("--reasoning-format".into());
    argv.push("deepseek".into());
  }
  if let Some(ctx) = params.ctx {
    argv.push("-c".into());
    argv.push(ctx.to_string().into());
  }
  // Defensive strip: refuse to pass loopback-breaking flags even if
  // an upstream validator was skipped. Last-occurrence semantics in
  // llama-server mean a single `--host 0.0.0.0` here would override
  // the bundled `--host 127.0.0.1` above.
  let mut iter = params.advanced.iter().peekable();
  while let Some(adv) = iter.next() {
    let lossy = adv.to_string_lossy();
    let head = lossy.split('=').next().unwrap_or(&lossy).to_ascii_lowercase();
    let banned = FORBIDDEN_ADVANCED_PREFIXES
      .iter()
      .any(|p| head == *p || (p.ends_with('-') && head.starts_with(p)));
    if banned {
      log::warn!("compose: stripping forbidden advanced flag {lossy:?}");
      // A token like `--host 0.0.0.0` is two args. Drop the value too
      // if it's the next non-flag token. `--host=0.0.0.0` is one arg
      // and already consumed.
      if !lossy.contains('=') {
        if let Some(next) = iter.peek() {
          let next_lossy = next.to_string_lossy();
          if !next_lossy.starts_with('-') {
            iter.next();
          }
        }
      }
      continue;
    }
    argv.push(adv.clone());
  }
  argv
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strs(args: &[OsString]) -> Vec<String> {
    args
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  fn base_params() -> LaunchParams {
    LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat)
  }

  #[test]
  fn chat_mode_emits_canonical_argv_prefix() {
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    let head: Vec<&str> = argv.iter().map(String::as_str).take(6).collect();
    assert_eq!(
      head,
      vec![
        "--host",
        "127.0.0.1",
        "--port",
        "41100",
        "-m",
        "/m/model.gguf"
      ]
    );
    // Chat mode adds no embedding/rerank flag.
    assert!(!argv
      .iter()
      .any(|a| a == "--embeddings" || a == "--reranking"));
  }

  #[test]
  fn embedding_mode_adds_embeddings_flag() {
    let mut p = base_params();
    p.mode = LaunchMode::Embedding;
    let argv = strs(&compose(&p, 41100));
    assert!(argv.iter().any(|a| a == "--embeddings"));
    assert!(!argv.iter().any(|a| a == "--reranking"));
  }

  #[test]
  fn rerank_mode_adds_reranking_flag() {
    let mut p = base_params();
    p.mode = LaunchMode::Rerank;
    let argv = strs(&compose(&p, 41100));
    assert!(argv.iter().any(|a| a == "--reranking"));
  }

  #[test]
  fn reasoning_bundles_jinja_and_deepseek() {
    let mut p = base_params();
    p.reasoning = true;
    let argv = strs(&compose(&p, 41100));
    assert!(argv.iter().any(|a| a == "--jinja"));
    let i = argv.iter().position(|a| a == "--reasoning-format").unwrap();
    assert_eq!(argv[i + 1], "deepseek");
  }

  #[test]
  fn ctx_override_emits_dash_c() {
    let mut p = base_params();
    p.ctx = Some(32768);
    let argv = strs(&compose(&p, 41100));
    let i = argv.iter().position(|a| a == "-c").unwrap();
    assert_eq!(argv[i + 1], "32768");
  }

  #[test]
  fn ctx_unset_omits_dash_c() {
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "-c"));
  }

  #[test]
  fn advanced_flags_land_at_the_end_to_override_bundled() {
    let mut p = base_params();
    p.reasoning = true;
    p.advanced = vec![
      // User wants raw reasoning format despite the reasoning bundle.
      OsString::from("--reasoning-format"),
      OsString::from("none"),
      OsString::from("--threads"),
      OsString::from("8"),
    ];
    let argv = strs(&compose(&p, 41100));
    // Last occurrence of `--reasoning-format` wins because
    // `llama-server` honours the right-most flag — that's the basis
    // of the "advanced flags trump bundled" contract.
    let positions: Vec<usize> = argv
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--reasoning-format")
      .map(|(i, _)| i)
      .collect();
    assert_eq!(positions.len(), 2, "bundled + override both present");
    let last = *positions.last().unwrap();
    assert_eq!(argv[last + 1], "none", "advanced override is last");
  }

  #[test]
  fn allocated_port_appears_after_port_flag() {
    let p = base_params();
    let argv = strs(&compose(&p, 41200));
    let i = argv.iter().position(|a| a == "--port").unwrap();
    assert_eq!(argv[i + 1], "41200");
  }

  #[test]
  fn forbidden_in_advanced_flags_loopback_bypass_attempts() {
    let advanced = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--LISTEN=0.0.0.0:8080"),
      OsString::from("--threads"),
      OsString::from("8"),
      OsString::from("--api-key"),
      OsString::from("secret"),
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/key.pem"),
    ];
    let banned = forbidden_in_advanced(&advanced);
    assert!(banned.iter().any(|s| s == "--host"));
    assert!(banned.iter().any(|s| s == "--LISTEN=0.0.0.0:8080"));
    assert!(banned.iter().any(|s| s == "--api-key"));
    assert!(banned.iter().any(|s| s == "--ssl-key-file"));
    assert!(!banned.iter().any(|s| s == "--threads"));
  }

  #[test]
  fn compose_strips_forbidden_advanced_flags_and_their_values() {
    let mut p = base_params();
    p.advanced = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--threads"),
      OsString::from("8"),
      OsString::from("--api-key=secret"),
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/key.pem"),
    ];
    let argv = strs(&compose(&p, 41100));
    // Bundled `--host 127.0.0.1` survives; the trailing `--host 0.0.0.0`
    // and its value have been stripped.
    let host_count = argv.iter().filter(|a| *a == "--host").count();
    assert_eq!(host_count, 1, "only the bundled --host should remain");
    assert!(!argv.iter().any(|a| a == "0.0.0.0"));
    // --api-key=foo single-token form is dropped.
    assert!(!argv.iter().any(|a| a.starts_with("--api-key")));
    assert!(!argv.iter().any(|a| a == "secret"));
    // --ssl-* prefix match.
    assert!(!argv.iter().any(|a| a == "--ssl-key-file"));
    assert!(!argv.iter().any(|a| a == "/etc/key.pem"));
    // Innocent flags survive in order.
    let t = argv.iter().position(|a| a == "--threads").unwrap();
    assert_eq!(argv[t + 1], "8");
  }
}
