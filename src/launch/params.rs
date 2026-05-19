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

use crate::config::ArchDefaults;
use crate::launch::mode::LaunchMode;

/// Flags refused in `LaunchParams.advanced` because they would break
/// the loopback-only / same-UID security contract documented in
/// `docs/architecture.md`. Match is case-insensitive on the flag
/// itself; `--ssl-*` matches any flag starting with that prefix.
pub const FORBIDDEN_ADVANCED_PREFIXES: &[&str] =
  &["--host", "--listen", "--bind", "--api-key", "--ssl-"];

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

fn advanced_contains_flag(advanced: &[OsString], flag_aliases: &[&str]) -> bool {
  advanced.iter().any(|s| {
    let lossy = s.to_string_lossy();
    let head = lossy.split('=').next().unwrap_or(&lossy);
    flag_aliases.contains(&head)
  })
}

/// Merge `defaults` into `params.advanced`, but only for flags the
/// caller has not already supplied. Pure function — no I/O. Respects
/// R69 precedence: caller-provided flags (originating from preset /
/// last-params / explicit CLI) outrank arch defaults.
///
/// Boolean flags are appended without a value (e.g. `--flash-attn`)
/// only when the default is `Some(true)`; `Some(false)` is treated as
/// "explicitly opt out, do not emit". Skipping the flag entirely when
/// the default is `None` keeps argv compact.
pub fn apply_arch_defaults(params: &mut LaunchParams, defaults: &ArchDefaults) {
  let mut push_kv = |aliases: &[&str], canonical: &str, value: String| {
    if advanced_contains_flag(&params.advanced, aliases) {
      return;
    }
    params.advanced.push(canonical.into());
    params.advanced.push(value.into());
  };
  if let Some(v) = defaults.n_gpu_layers {
    push_kv(&["--n-gpu-layers", "-ngl"], "--n-gpu-layers", v.to_string());
  }
  if let Some(v) = defaults.threads {
    push_kv(&["--threads", "-t"], "--threads", v.to_string());
  }
  if let Some(ref v) = defaults.cache_type_k {
    push_kv(&["--cache-type-k", "-ctk"], "--cache-type-k", v.clone());
  }
  if let Some(ref v) = defaults.cache_type_v {
    push_kv(&["--cache-type-v", "-ctv"], "--cache-type-v", v.clone());
  }
  if let Some(v) = defaults.parallel {
    push_kv(&["--parallel", "-np"], "--parallel", v.to_string());
  }
  // Boolean flags: emit only when `Some(true)` and not already present.
  let mut push_bool = |alias: &str| {
    if advanced_contains_flag(&params.advanced, &[alias]) {
      return;
    }
    params.advanced.push(alias.into());
  };
  if defaults.flash_attn == Some(true) {
    push_bool("--flash-attn");
  }
  if defaults.mlock == Some(true) {
    push_bool("--mlock");
  }
  if defaults.no_mmap == Some(true) {
    push_bool("--no-mmap");
  }
}

/// Same as [`apply_arch_defaults`] but looks the architecture up in
/// `Config.arch_defaults`. A no-op when the architecture has no
/// entry. Exposed as a convenience for the IPC handler, which has
/// the daemon's `Config` clone handy.
pub fn apply_arch_defaults_for(
  params: &mut LaunchParams,
  arch_defaults: &std::collections::BTreeMap<String, ArchDefaults>,
  architecture: &str,
) {
  if let Some(d) = arch_defaults.get(architecture) {
    apply_arch_defaults(params, d);
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
    let head = lossy
      .split('=')
      .next()
      .unwrap_or(&lossy)
      .to_ascii_lowercase();
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
  fn apply_arch_defaults_fills_missing_kv_and_bool_flags() {
    let mut p = base_params();
    let d = ArchDefaults {
      n_gpu_layers: Some(99),
      threads: Some(8),
      cache_type_k: Some("q8_0".into()),
      cache_type_v: Some("q8_0".into()),
      flash_attn: Some(true),
      mlock: Some(false),
      no_mmap: Some(true),
      parallel: Some(4),
    };
    apply_arch_defaults(&mut p, &d);
    let adv = strs(&p.advanced);
    let ngl = adv.iter().position(|a| a == "--n-gpu-layers").unwrap();
    assert_eq!(adv[ngl + 1], "99");
    let t = adv.iter().position(|a| a == "--threads").unwrap();
    assert_eq!(adv[t + 1], "8");
    let ctk = adv.iter().position(|a| a == "--cache-type-k").unwrap();
    assert_eq!(adv[ctk + 1], "q8_0");
    let ctv = adv.iter().position(|a| a == "--cache-type-v").unwrap();
    assert_eq!(adv[ctv + 1], "q8_0");
    let par = adv.iter().position(|a| a == "--parallel").unwrap();
    assert_eq!(adv[par + 1], "4");
    assert!(adv.iter().any(|a| a == "--flash-attn"));
    assert!(adv.iter().any(|a| a == "--no-mmap"));
    assert!(
      !adv.iter().any(|a| a == "--mlock"),
      "Some(false) must NOT emit the flag"
    );
  }

  #[test]
  fn apply_arch_defaults_respects_caller_supplied_flags() {
    // Caller already specified --n-gpu-layers (e.g. via preset). The
    // arch default must not override.
    let mut p = base_params();
    p.advanced = vec!["--n-gpu-layers".into(), "40".into()];
    let d = ArchDefaults {
      n_gpu_layers: Some(99),
      ..ArchDefaults::default()
    };
    apply_arch_defaults(&mut p, &d);
    let adv = strs(&p.advanced);
    let positions: Vec<usize> = adv
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--n-gpu-layers")
      .map(|(i, _)| i)
      .collect();
    assert_eq!(positions.len(), 1, "caller's flag must not be duplicated");
    assert_eq!(adv[positions[0] + 1], "40", "caller's value survives");
  }

  #[test]
  fn apply_arch_defaults_recognises_short_aliases() {
    // Caller passed `-ngl 40`; arch default's `--n-gpu-layers` must
    // not fire because the short alias is already present.
    let mut p = base_params();
    p.advanced = vec!["-ngl".into(), "40".into()];
    let d = ArchDefaults {
      n_gpu_layers: Some(99),
      ..ArchDefaults::default()
    };
    apply_arch_defaults(&mut p, &d);
    assert!(
      !p.advanced.iter().any(|s| s == "--n-gpu-layers"),
      "short alias should block the canonical flag"
    );
  }

  #[test]
  fn apply_arch_defaults_for_missing_arch_is_noop() {
    use std::collections::BTreeMap;
    let mut p = base_params();
    let original = p.advanced.clone();
    let map: BTreeMap<String, ArchDefaults> = BTreeMap::new();
    apply_arch_defaults_for(&mut p, &map, "qwen2");
    assert_eq!(p.advanced, original, "missing arch must be a no-op");
  }

  #[test]
  fn apply_arch_defaults_recognises_equals_form() {
    // Caller passed `--threads=8`; default's `--threads 16` must not fire.
    let mut p = base_params();
    p.advanced = vec!["--threads=8".into()];
    let d = ArchDefaults {
      threads: Some(16),
      ..ArchDefaults::default()
    };
    apply_arch_defaults(&mut p, &d);
    let t_count = p
      .advanced
      .iter()
      .filter(|s| {
        let lossy = s.to_string_lossy();
        let head = lossy.split('=').next().unwrap_or(&lossy);
        head == "--threads"
      })
      .count();
    assert_eq!(t_count, 1, "equals-form should block the default");
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
