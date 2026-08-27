//! Compose `llama-server` argv from the user's launch choices.
//!
//! This is llama.cpp's argv emitter — it lives with the backend that is
//! its only caller ([`super::LlamaCppBackend::process_spec`]) rather than in
//! the neutral `launch::params` IR. The loopback/credential denylist it
//! enforces on `extras` is the shared [`is_forbidden_head`] guard, which
//! stays in `launch::params` because the native-knob path reuses it.
//!
//! Order matters: `--host 127.0.0.1` and `--port` come first so the
//! command line reads well in logs; then `-m <path>`, then mode flags
//! (`--embeddings` / `--reranking`), then `--jinja` (config default or
//! forced by reasoning) and the reasoning `--reasoning-format deepseek`
//! pair, then `-c <ctx>`, then
//! the typed knobs in canonical order, then any user-supplied
//! `extras` argv tail. `extras` land *last* so they always trump
//! everything else — that's the contract documented on the TUI's
//! "Settings" tab.
//!
//! The extras strip enforces the loopback-only and same-UID contract: a
//! curated denylist (`--host`, `--listen`, `--bind`, `--api-key`,
//! `--ssl-*`) is refused. llama-server honours the last-occurrence of a
//! flag, so without this guard a trailing `--host 0.0.0.0` in `extras`
//! would expose the model to the LAN.

use std::ffi::OsString;

use crate::launch::mode::LaunchMode;
use crate::launch::params::{is_forbidden_head, LaunchParams};

/// Materialise the argv `Command::args(...)` will hand to
/// `llama-server`. Caller passes the resolved listening port
/// separately because allocation happens in the supervisor, not in
/// `LaunchParams`.
///
/// `params.knobs.str(crate::launch::knobs::kid("device"))`, when set, is a real `llama-server` device
/// selector (`Vulkan0`, `CUDA0`, `ROCm0`) sourced from that binary's
/// own `--list-devices` output (see [`crate::backend::llama_cpp::list_devices`]).
/// It is emitted verbatim as a single `--device <selector>` — no index
/// math, no backend guessing. The caller is responsible for spawning
/// the matching binary so the selector is valid.
pub(crate) fn compose(params: &LaunchParams, allocated_port: u16) -> Vec<OsString> {
  let mut knob_argv =
    crate::launch::knobs::emit_argv(crate::backend::DEFAULT_BACKEND_ID, &params.knobs, &[]);
  let mut argv: Vec<OsString> = Vec::with_capacity(16 + knob_argv.len() + params.extras.len());
  argv.push("--host".into());
  argv.push("127.0.0.1".into());
  argv.push("--port".into());
  argv.push(allocated_port.to_string().into());
  argv.push("-m".into());
  argv.push(params.model_path.clone().into());
  if let Some(ref mmproj) = params.mmproj_path {
    argv.push("--mmproj".into());
    argv.push(mmproj.clone().into());
  }
  match params.mode {
    LaunchMode::Chat => {}
    LaunchMode::Embedding => argv.push("--embeddings".into()),
    LaunchMode::Rerank => argv.push("--reranking".into()),
  }
  // `--jinja` rides on the config-derived `jinja` launch knob (carried in
  // `backend_knobs`, seeded by the backend) *or* the reasoning toggle —
  // reasoning needs the Jinja chat template, so it forces the flag on even
  // when the config default is `false`. Emitted once; reasoning then adds its
  // `--reasoning-format deepseek` pair.
  let jinja = params
    .launch_config
    .get("jinja")
    .is_some_and(|s| s == "true");
  if jinja || params.reasoning {
    argv.push("--jinja".into());
  }
  if params.reasoning {
    argv.push("--reasoning-format".into());
    argv.push("deepseek".into());
  }
  // MTP speculative decoding — emitted BEFORE the ctx / `--fit-ctx` block so
  // llama.cpp's `--fit` reserves the MTP draft context (KD6). The directive was
  // resolved server-side (`compose_and_spawn` → `resolve_mtp_directive`) against
  // the model's real capability and any user `--spec-type` in extras (KD1/KD3);
  // `None` means "not MTP this launch" and emits nothing.
  if let Some(mtp) = &params.mtp_directive {
    argv.push("--spec-type".into());
    argv.push("draft-mtp".into());
    if let Some(ref draft) = mtp.draft_model {
      argv.push("--model-draft".into());
      argv.push(draft.clone().into());
    }
    if let Some(n) = params.mtp_draft_n {
      argv.push("--spec-draft-n-max".into());
      argv.push(n.to_string().into());
    }
  }
  // Context window: a pinned `ctx` emits `-c <N>` and suppresses
  // `--fit-ctx` (fit honors the pin). An unset `ctx` (Auto / Inherited)
  // emits `--fit-ctx <floor>` (floor from the config-derived `fit_ctx_floor`
  // launch knob) so `--fit` sizes the window for the available memory but
  // never collapses below the floor.
  if let Some(ctx) = params.ctx {
    argv.push("-c".into());
    argv.push(ctx.to_string().into());
  } else if let Some(floor) = params
    .launch_config
    .get("fit_ctx_floor")
    .and_then(|s| s.parse::<u32>().ok())
  {
    argv.push("--fit-ctx".into());
    argv.push(floor.to_string().into());
  }
  // Emit the device selector verbatim — exactly once. Empty / unset
  // means "let llama-server auto-select" (no flag).
  if let Some(sel) = params
    .knobs
    .text_by_name("device")
    .filter(|s| !s.is_empty())
  {
    knob_argv.push("--device".into());
    knob_argv.push(sel.into());
  }
  argv.extend(knob_argv);
  // Defensive strip: refuse to pass loopback-breaking flags even if
  // an upstream validator was skipped. Last-occurrence semantics in
  // llama-server mean a single `--host 0.0.0.0` here would override
  // the bundled `--host 127.0.0.1` above.
  let mut iter = params.extras.iter().peekable();
  while let Some(adv) = iter.next() {
    let lossy = adv.to_string_lossy();
    let head = lossy
      .split('=')
      .next()
      .unwrap_or(&lossy)
      .to_ascii_lowercase();
    if is_forbidden_head(&head) {
      log::warn!("compose: stripping forbidden extras flag {lossy:?}");
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
  use crate::launch::params::MtpDirective;
  use std::path::PathBuf;

  fn strs(args: &[OsString]) -> Vec<String> {
    args
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  fn base_params() -> LaunchParams {
    LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat)
  }

  /// **Golden argv (plan D9).** Pins the exact command line for a fully-set
  /// launch, so any change to a knob's flag, value formatting or emission
  /// order shows up as a diff here rather than as a behaviour change nobody
  /// noticed. `scripts/bench/` depends on this argv being stable.
  ///
  /// Replaces the migration-era test that compared the registry emitter
  /// against the hand-written `argvify`; with that function gone, the golden
  /// is what carries the guarantee forward.
  #[test]
  fn composed_argv_matches_golden() {
    let mut p = base_params();
    p.ctx = Some(32768);
    p.knobs = crate::knobset! {
      n_gpu_layers: 99,
      n_cpu_moe: 12,
      threads: 8,
      cache_type_k: "q8_0",
      cache_type_v: "q8_0",
      flash_attn: true,
      mlock: true,
      no_mmap: true,
      parallel: 4,
      batch_size: 2048,
      ubatch_size: 512,
      rope_freq_scale: 1.0,
      keep: 128,
      tensor_split: "3,1",
      main_gpu: 1,
      split_mode: "row",
    };
    assert_eq!(
      strs(&compose(&p, 41100)),
      vec![
        "--host",
        "127.0.0.1",
        "--port",
        "41100",
        "-m",
        "/m/model.gguf",
        "-c",
        "32768",
        "--n-gpu-layers",
        "99",
        "--n-cpu-moe",
        "12",
        "--tensor-split",
        "3,1",
        "--main-gpu",
        "1",
        "--split-mode",
        "row",
        "--threads",
        "8",
        "--cache-type-k",
        "q8_0",
        "--cache-type-v",
        "q8_0",
        "--parallel",
        "4",
        "--flash-attn",
        "on",
        "--mlock",
        "--no-mmap",
        "--batch-size",
        "2048",
        "--ubatch-size",
        "512",
        "--rope-freq-scale",
        "1.0",
        "--keep",
        "128",
      ]
    );
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
    assert!(!argv
      .iter()
      .any(|a| a == "--embeddings" || a == "--reranking"));
  }

  #[test]
  fn unset_ctx_without_floor_emits_neither() {
    // No floor configured (e.g. a bare LaunchParams) → no ctx flags.
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "-c" || a == "--fit-ctx"));
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

  // ---- MTP speculative decoding ----

  #[test]
  fn compose_emits_no_spec_type_when_directive_absent() {
    // The common case: no MTP directive → no `--spec-type` on argv.
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "--spec-type"));
    assert!(!argv.iter().any(|a| a == "--model-draft"));
  }

  #[test]
  fn compose_emits_separate_head_and_draft_n_max() {
    // Separate head: `--spec-type draft-mtp --model-draft <path>`, plus a
    // configured `--spec-draft-n-max`.
    let mut p = base_params();
    p.mtp_directive = Some(MtpDirective {
      draft_model: Some(PathBuf::from("/m/mtp-model.gguf")),
    });
    p.mtp_draft_n = Some(5);
    let argv = strs(&compose(&p, 41100));
    let md = argv
      .iter()
      .position(|a| a == "--model-draft")
      .expect("--model-draft");
    assert_eq!(argv[md + 1], "/m/mtp-model.gguf");
    let n = argv
      .iter()
      .position(|a| a == "--spec-draft-n-max")
      .expect("--spec-draft-n-max");
    assert_eq!(argv[n + 1], "5");
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
  fn compose_emits_knobs_then_extras_at_tail() {
    let mut p = base_params();
    p.knobs.set_scalar(
      crate::launch::knobs::kid("n-gpu-layers"),
      crate::launch::knobs::Scalar::U32(99),
    );
    p.extras = vec!["--rope-freq-base".into(), "10000".into()];
    let argv = strs(&compose(&p, 41100));
    let ngl = argv.iter().position(|a| a == "--n-gpu-layers").unwrap();
    let rfb = argv.iter().position(|a| a == "--rope-freq-base").unwrap();
    assert!(ngl < rfb, "knobs must precede extras");
    assert_eq!(argv[rfb + 1], "10000");
  }

  #[test]
  fn compose_strips_forbidden_extras_flags_and_their_values() {
    let mut p = base_params();
    p.extras = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--threads"),
      OsString::from("8"),
      OsString::from("--api-key=secret"),
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/key.pem"),
    ];
    let argv = strs(&compose(&p, 41100));
    let host_count = argv.iter().filter(|a| *a == "--host").count();
    assert_eq!(host_count, 1, "only the bundled --host should remain");
    assert!(!argv.iter().any(|a| a == "0.0.0.0"));
    assert!(!argv.iter().any(|a| a.starts_with("--api-key")));
    assert!(!argv.iter().any(|a| a == "secret"));
    assert!(!argv.iter().any(|a| a == "--ssl-key-file"));
    assert!(!argv.iter().any(|a| a == "/etc/key.pem"));
    let t = argv.iter().position(|a| a == "--threads").unwrap();
    assert_eq!(argv[t + 1], "8");
  }

  #[test]
  fn compose_emits_extras_overlap_after_knob_so_last_wins() {
    let mut p = base_params();
    p.knobs.set_scalar(
      crate::launch::knobs::kid("n-gpu-layers"),
      crate::launch::knobs::Scalar::U32(99),
    );
    p.extras = vec!["--n-gpu-layers".into(), "7".into()];
    let argv = strs(&compose(&p, 41100));
    let positions: Vec<usize> = argv
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--n-gpu-layers")
      .map(|(i, _)| i)
      .collect();
    assert_eq!(positions.len(), 2, "both knob and extras occurrence kept");
    let last = *positions.last().unwrap();
    assert_eq!(argv[last + 1], "7", "extras occurrence is later in argv");
  }

  #[test]
  fn allocated_port_appears_after_port_flag() {
    let p = base_params();
    let argv = strs(&compose(&p, 41200));
    let i = argv.iter().position(|a| a == "--port").unwrap();
    assert_eq!(argv[i + 1], "41200");
  }

  #[test]
  fn compose_emits_mmproj_flag_when_path_set() {
    let mut p = base_params();
    p.mmproj_path = Some(PathBuf::from("/m/mmproj-model.gguf"));
    let argv = strs(&compose(&p, 41100));
    let i = argv.iter().position(|a| a == "--mmproj").unwrap();
    assert_eq!(argv[i + 1], "/m/mmproj-model.gguf");
  }

  #[test]
  fn compose_omits_mmproj_flag_when_path_not_set() {
    let p = base_params();
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "--mmproj"));
  }

  // ---- Device selector tests ----

  /// Collect every `--device` value present in the argv.

  #[test]
  fn compose_skips_device_when_none() {
    let p = base_params();
    assert!(p.knobs.str(crate::launch::knobs::kid("device")).is_none());
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| *a == "--device"));
  }
}
