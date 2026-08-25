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

use crate::config::{KnobValueOpt, TypedKnobs};
use crate::launch::flag_aliases::{knob_specs, KnobField, ValueKind};
use crate::launch::mode::LaunchMode;
use crate::launch::params::{is_forbidden_head, LaunchParams};

/// Argv-ify the typed knob set in canonical flag order. Skips
/// `None` fields; for booleans, only emits the flag when
/// `Some(true)` (`Some(false)` is an explicit opt-out — no
/// `--no-flash-attn` form because llama-server doesn't have one).
///
/// `Ctx` and `Reasoning` are deliberately skipped here — they live
/// in `TypedKnobs` for the resolver chain and the editor's source
/// chips, but `compose` emits them inline (ctx → `-c <N>`, reasoning
/// → `--reasoning-format deepseek`, plus the `--jinja` it shares with
/// the config default) so their argv order stays distinct from the
/// other knobs.
///
/// `Device` is also skipped here: `knobs.device` holds a real
/// `llama-server` device selector (`Vulkan0`, `CUDA0`, `ROCm0`) and
/// `compose` emits it exactly once as `--device <selector>`. Emitting
/// it here too would put a *second* `--device` on the argv;
/// llama-server validates each `--device` token as it parses, so a
/// stray/duplicate value makes it bail with `invalid device: …` before
/// last-occurrence-wins ever applies.
fn argvify(knobs: &TypedKnobs) -> Vec<OsString> {
  let mut out: Vec<OsString> = Vec::new();
  for spec in knob_specs() {
    match spec.field {
      // Skipped here (emitted inline by `compose`) or governed by fit:
      // an `Auto` knob falls through `set_value()` to `None`, so no
      // flag is emitted and `--fit` is left to place it.
      KnobField::Ctx | KnobField::Reasoning | KnobField::Device => continue,
      KnobField::NGpuLayers => push_u32(
        &mut out,
        spec.canonical,
        knobs.n_gpu_layers.set_value().copied(),
      ),
      KnobField::NCpuMoe => push_u32(
        &mut out,
        spec.canonical,
        knobs.n_cpu_moe.set_value().copied(),
      ),
      KnobField::Threads => push_u32(&mut out, spec.canonical, knobs.threads.set_value().copied()),
      KnobField::CacheTypeK => push_str(
        &mut out,
        spec.canonical,
        knobs.cache_type_k.set_value().map(String::as_str),
      ),
      KnobField::CacheTypeV => push_str(
        &mut out,
        spec.canonical,
        knobs.cache_type_v.set_value().map(String::as_str),
      ),
      KnobField::Parallel => push_u32(
        &mut out,
        spec.canonical,
        knobs.parallel.set_value().copied(),
      ),
      KnobField::FlashAttn => push_flash_attn(
        &mut out,
        spec.canonical,
        knobs.flash_attn.set_value().copied(),
      ),
      KnobField::Mlock => push_bool(&mut out, spec.canonical, knobs.mlock.set_value().copied()),
      KnobField::NoMmap => push_bool(&mut out, spec.canonical, knobs.no_mmap.set_value().copied()),
      KnobField::BatchSize => push_u32(
        &mut out,
        spec.canonical,
        knobs.batch_size.set_value().copied(),
      ),
      KnobField::UbatchSize => push_u32(
        &mut out,
        spec.canonical,
        knobs.ubatch_size.set_value().copied(),
      ),
      KnobField::RopeFreqScale => push_f32(
        &mut out,
        spec.canonical,
        knobs.rope_freq_scale.set_value().copied(),
      ),
      KnobField::Keep => push_u32(&mut out, spec.canonical, knobs.keep.set_value().copied()),
      KnobField::TensorSplit => push_str(
        &mut out,
        spec.canonical,
        knobs.tensor_split.set_value().map(String::as_str),
      ),
      KnobField::MainGpu => push_u32(
        &mut out,
        spec.canonical,
        knobs.main_gpu.set_value().copied(),
      ),
      KnobField::SplitMode => push_str(
        &mut out,
        spec.canonical,
        knobs.split_mode.set_value().map(String::as_str),
      ),
    }
    // `ValueKind` is the source-of-truth for emission shape; sanity
    // check that our match handled the right kind.
    debug_assert!(
      matches!(
        spec.kind,
        ValueKind::U32
          | ValueKind::F32
          | ValueKind::Bool
          | ValueKind::KvCacheType
          | ValueKind::SplitMode
          | ValueKind::Str
      ),
      "ValueKind exhaustiveness drift"
    );
  }
  out
}

fn push_u32(out: &mut Vec<OsString>, canonical: &str, value: Option<u32>) {
  if let Some(v) = value {
    out.push(canonical.into());
    out.push(v.to_string().into());
  }
}

fn push_f32(out: &mut Vec<OsString>, canonical: &str, value: Option<f32>) {
  if let Some(v) = value {
    out.push(canonical.into());
    out.push(format_f32(v).into());
  }
}

fn push_str(out: &mut Vec<OsString>, canonical: &str, value: Option<&str>) {
  if let Some(v) = value {
    out.push(canonical.into());
    out.push(v.to_string().into());
  }
}

fn push_bool(out: &mut Vec<OsString>, canonical: &str, value: Option<bool>) {
  if value == Some(true) {
    out.push(canonical.into());
  }
}

/// Modern llama-server (b9000+) requires `--flash-attn on|off|auto`
/// and rejects the bare flag — passing `--flash-attn` alone causes
/// the next argv entry to be parsed as the flash-attn value.
fn push_flash_attn(out: &mut Vec<OsString>, canonical: &str, value: Option<bool>) {
  match value {
    Some(true) => {
      out.push(canonical.into());
      out.push("on".into());
    }
    Some(false) => {
      out.push(canonical.into());
      out.push("off".into());
    }
    None => {}
  }
}

/// Format an f32 without trailing zeros beyond the canonical
/// representation. Integer-valued floats render with a `.0` suffix
/// so the value still reads as a float (e.g. `2` → `"2.0"`).
fn format_f32(v: f32) -> String {
  if v.fract() == 0.0 && v.is_finite() {
    format!("{v:.1}")
  } else {
    format!("{v}")
  }
}

/// Materialise the argv `Command::args(...)` will hand to
/// `llama-server`. Caller passes the resolved listening port
/// separately because allocation happens in the supervisor, not in
/// `LaunchParams`.
///
/// `params.knobs.device`, when set, is a real `llama-server` device
/// selector (`Vulkan0`, `CUDA0`, `ROCm0`) sourced from that binary's
/// own `--list-devices` output (see [`crate::backend::llama_cpp::list_devices`]).
/// It is emitted verbatim as a single `--device <selector>` — no index
/// math, no backend guessing. The caller is responsible for spawning
/// the matching binary so the selector is valid.
pub(crate) fn compose(params: &LaunchParams, allocated_port: u16) -> Vec<OsString> {
  let mut knob_argv = argvify(&params.knobs);
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
    .backend_knobs
    .get("jinja")
    .and_then(|kv| kv.as_set())
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
    .backend_knobs
    .get("fit_ctx_floor")
    .and_then(|kv| kv.as_set())
    .and_then(|s| s.parse::<u32>().ok())
  {
    argv.push("--fit-ctx".into());
    argv.push(floor.to_string().into());
  }
  // Emit the device selector verbatim — exactly once. Empty / unset
  // means "let llama-server auto-select" (no flag).
  if let Some(sel) = params
    .knobs
    .device
    .set_value()
    .map(String::as_str)
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
  use crate::config::KnobValue;
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
  fn unset_ctx_emits_fit_ctx_floor_and_no_ngl() {
    // Auto / Inherited ctx (None) + a configured floor (the `fit_ctx_floor`
    // launch knob) → `--fit-ctx`, no `-c`, and (after de-pin) no `-ngl`.
    let mut p = base_params();
    p.ctx = None;
    p.backend_knobs
      .insert("fit_ctx_floor".into(), KnobValue::Set("16384".into()));
    let argv = strs(&compose(&p, 41100));
    let pos = argv
      .iter()
      .position(|a| a == "--fit-ctx")
      .expect("--fit-ctx");
    assert_eq!(argv[pos + 1], "16384");
    assert!(!argv.iter().any(|a| a == "-c"), "no -c when ctx unset");
    assert!(!argv.iter().any(|a| a == "-ngl"), "ngl is de-pinned");
  }

  #[test]
  fn pinned_ctx_emits_dash_c_and_suppresses_fit_ctx() {
    // A user-pinned ctx wins: `-c <N>`, no `--fit-ctx` (fit honors it).
    let mut p = base_params();
    p.ctx = Some(32768);
    p.backend_knobs
      .insert("fit_ctx_floor".into(), KnobValue::Set("16384".into()));
    let argv = strs(&compose(&p, 41100));
    let pos = argv.iter().position(|a| a == "-c").expect("-c");
    assert_eq!(argv[pos + 1], "32768");
    assert!(
      !argv.iter().any(|a| a == "--fit-ctx"),
      "a pinned ctx suppresses the fit floor"
    );
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

  #[test]
  fn jinja_knob_emits_jinja_without_reasoning() {
    // The `jinja` launch knob (seeded from config) emits `--jinja` even with
    // reasoning off, and no `--reasoning-format` rides along.
    let mut p = base_params();
    p.backend_knobs
      .insert("jinja".into(), KnobValue::Set("true".into()));
    let argv = strs(&compose(&p, 41100));
    assert_eq!(argv.iter().filter(|a| *a == "--jinja").count(), 1);
    assert!(!argv.iter().any(|a| a == "--reasoning-format"));
  }

  #[test]
  fn jinja_disabled_omits_the_flag() {
    // Jinja off (config `jinja: false` seeds `Set("false")`, or no key at all)
    // → no `--jinja`.
    let mut p = base_params();
    p.backend_knobs
      .insert("jinja".into(), KnobValue::Set("false".into()));
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| a == "--jinja"));
    // A bare params with no jinja knob likewise emits nothing.
    let bare = base_params();
    assert!(!strs(&compose(&bare, 41100)).iter().any(|a| a == "--jinja"));
  }

  #[test]
  fn reasoning_forces_jinja_even_when_config_disables_it() {
    // The reasoning toggle needs the Jinja engine, so it wins over a
    // `jinja: false` config — and `--jinja` is still emitted exactly once.
    let mut p = base_params();
    p.backend_knobs
      .insert("jinja".into(), KnobValue::Set("false".into()));
    p.reasoning = true;
    let argv = strs(&compose(&p, 41100));
    assert_eq!(argv.iter().filter(|a| *a == "--jinja").count(), 1);
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
  fn compose_emits_embedded_mtp_before_fit_ctx() {
    // Embedded head (draft_model = None): `--spec-type draft-mtp`, no
    // `--model-draft`, positioned before `--fit-ctx` (KD6).
    let mut p = base_params();
    p.backend_knobs
      .insert("fit_ctx_floor".into(), KnobValue::Set("16384".into()));
    p.mtp_directive = Some(MtpDirective { draft_model: None });
    let argv = strs(&compose(&p, 41100));
    let st = argv
      .iter()
      .position(|a| a == "--spec-type")
      .expect("--spec-type");
    assert_eq!(argv[st + 1], "draft-mtp");
    assert!(
      !argv.iter().any(|a| a == "--model-draft"),
      "embedded head: no drafter"
    );
    let fit = argv
      .iter()
      .position(|a| a == "--fit-ctx")
      .expect("--fit-ctx");
    assert!(
      st < fit,
      "--spec-type must precede --fit-ctx (MTP-aware fit)"
    );
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

  /// **The migration invariant (plan D9).** The registry emitter must produce
  /// byte-identical argv to the pre-registry `argvify` for the same knob
  /// values. Anything else is a silent behaviour change on every launch.
  ///
  /// This test and its converter exist only to prove the migration; both die
  /// with `TypedKnobs` when the persistence flip lands.
  #[test]
  fn registry_emitter_matches_argvify_byte_for_byte() {
    use crate::launch::knobs::{self, KnobSet, Scalar};

    fn id(name: &str) -> knobs::KnobId {
      knobs::resolve_id(name).unwrap_or_else(|| panic!("no knob `{name}`"))
    }

    /// Throwaway `TypedKnobs` -> `KnobSet` converter, covering exactly the
    /// fields `argvify` emits.
    fn to_set(t: &TypedKnobs) -> KnobSet {
      let mut k = KnobSet::new();
      macro_rules! num {
        ($field:ident, $name:literal) => {
          if let Some(v) = t.$field.set_value().copied() {
            k.set_scalar(id($name), Scalar::U32(v));
          }
        };
      }
      macro_rules! flag {
        ($field:ident, $name:literal) => {
          if let Some(v) = t.$field.set_value().copied() {
            k.set_scalar(id($name), Scalar::Bool(v));
          }
        };
      }
      macro_rules! text {
        ($field:ident, $name:literal) => {
          if let Some(v) = t.$field.set_value() {
            k.set_scalar(id($name), Scalar::Str(v.clone()));
          }
        };
      }
      num!(n_gpu_layers, "n-gpu-layers");
      num!(n_cpu_moe, "n-cpu-moe");
      num!(threads, "threads");
      num!(parallel, "parallel");
      num!(batch_size, "batch-size");
      num!(ubatch_size, "ubatch-size");
      num!(keep, "keep");
      num!(main_gpu, "main-gpu");
      flag!(flash_attn, "flash-attn");
      flag!(mlock, "mlock");
      flag!(no_mmap, "no-mmap");
      text!(cache_type_k, "cache-type-k");
      text!(cache_type_v, "cache-type-v");
      text!(tensor_split, "tensor-split");
      text!(split_mode, "split-mode");
      if let Some(v) = t.rope_freq_scale.set_value().copied() {
        k.set_scalar(id("rope-freq-scale"), Scalar::F32(v));
      }
      k
    }

    let cases: Vec<(&str, TypedKnobs)> = vec![
      ("empty", TypedKnobs::default()),
      (
        "full set",
        TypedKnobs {
          ctx: Some(KnobValue::Set(32768)),
          reasoning: Some(KnobValue::Set(true)),
          n_gpu_layers: Some(KnobValue::Set(99)),
          n_cpu_moe: Some(KnobValue::Set(12)),
          threads: Some(KnobValue::Set(8)),
          cache_type_k: Some(KnobValue::Set("q8_0".into())),
          cache_type_v: Some(KnobValue::Set("q8_0".into())),
          flash_attn: Some(KnobValue::Set(true)),
          mlock: Some(KnobValue::Set(true)),
          no_mmap: Some(KnobValue::Set(true)),
          parallel: Some(KnobValue::Set(4)),
          batch_size: Some(KnobValue::Set(2048)),
          ubatch_size: Some(KnobValue::Set(512)),
          rope_freq_scale: Some(KnobValue::Set(1.0)),
          keep: Some(KnobValue::Set(64)),
          device: Some(KnobValue::Set("ROCm0".into())),
          tensor_split: Some(KnobValue::Set("3,1".into())),
          main_gpu: Some(KnobValue::Set(1)),
          split_mode: Some(KnobValue::Set("row".into())),
        },
      ),
      (
        "bools off",
        TypedKnobs {
          flash_attn: Some(KnobValue::Set(false)),
          mlock: Some(KnobValue::Set(false)),
          no_mmap: Some(KnobValue::Set(false)),
          ..TypedKnobs::default()
        },
      ),
      (
        "auto knobs emit nothing",
        TypedKnobs {
          ctx: Some(KnobValue::Auto),
          n_gpu_layers: Some(KnobValue::Auto),
          n_cpu_moe: Some(KnobValue::Auto),
          tensor_split: Some(KnobValue::Auto),
          ..TypedKnobs::default()
        },
      ),
      (
        "fractional float",
        TypedKnobs {
          rope_freq_scale: Some(KnobValue::Set(0.5)),
          ..TypedKnobs::default()
        },
      ),
    ];

    for (name, typed) in cases {
      let old = strs(&argvify(&typed));
      let new = strs(&knobs::emit_argv(
        crate::backend::DEFAULT_BACKEND_ID,
        &to_set(&typed),
        &[],
      ));
      assert_eq!(old, new, "argv diverged for case `{name}`");
    }
  }

  #[test]
  fn argvify_emits_full_set_in_canonical_order() {
    let knobs = TypedKnobs {
      ctx: Some(KnobValue::Set(32768)),
      reasoning: Some(KnobValue::Set(true)),
      n_gpu_layers: Some(KnobValue::Set(99)),
      n_cpu_moe: Some(KnobValue::Set(12)),
      threads: Some(KnobValue::Set(8)),
      cache_type_k: Some(KnobValue::Set("q8_0".into())),
      cache_type_v: Some(KnobValue::Set("q8_0".into())),
      flash_attn: Some(KnobValue::Set(true)),
      mlock: Some(KnobValue::Set(true)),
      no_mmap: Some(KnobValue::Set(true)),
      parallel: Some(KnobValue::Set(4)),
      batch_size: Some(KnobValue::Set(2048)),
      ubatch_size: Some(KnobValue::Set(512)),
      rope_freq_scale: Some(KnobValue::Set(1.0)),
      keep: Some(KnobValue::Set(128)),
      device: None,
      tensor_split: Some(KnobValue::Set("3,1".into())),
      main_gpu: Some(KnobValue::Set(0)),
      split_mode: Some(KnobValue::Set("layer".into())),
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(
      argv,
      vec![
        "--n-gpu-layers",
        "99",
        "--n-cpu-moe",
        "12",
        "--tensor-split",
        "3,1",
        "--main-gpu",
        "0",
        "--split-mode",
        "layer",
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
  fn argvify_skips_none_fields() {
    let knobs = TypedKnobs {
      n_gpu_layers: Some(KnobValue::Set(99)),
      flash_attn: Some(KnobValue::Set(true)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--n-gpu-layers", "99", "--flash-attn", "on"]);
  }

  #[test]
  fn argvify_some_false_omits_bare_bool_flags() {
    // True bare flags (`--mlock`, `--no-mmap`) are absent when set to
    // false — there's no `--no-mlock` form in llama-server.
    let knobs = TypedKnobs {
      mlock: Some(KnobValue::Set(false)),
      no_mmap: Some(KnobValue::Set(false)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert!(
      argv.is_empty(),
      "Some(false) bare bools must not emit the flag"
    );
  }

  #[test]
  fn argvify_flash_attn_false_emits_off() {
    // `--flash-attn` takes a value (`on|off|auto`); Some(false) MUST
    // emit `--flash-attn off` so a user override actually disables it
    // when an inherited layer set Some(true).
    let knobs = TypedKnobs {
      flash_attn: Some(KnobValue::Set(false)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--flash-attn", "off"]);
  }

  #[test]
  fn argvify_empty_yields_empty() {
    let argv = strs(&argvify(&TypedKnobs::default()));
    assert!(argv.is_empty());
  }

  #[test]
  fn argvify_rope_freq_scale_formats_one_point_oh() {
    let knobs = TypedKnobs {
      rope_freq_scale: Some(KnobValue::Set(1.0)),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert_eq!(argv, vec!["--rope-freq-scale", "1.0"]);
  }

  #[test]
  fn compose_emits_knobs_then_extras_at_tail() {
    let mut p = base_params();
    p.knobs.n_gpu_layers = Some(KnobValue::Set(99));
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
    p.knobs.n_gpu_layers = Some(KnobValue::Set(99));
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
  fn device_values(argv: &[String]) -> Vec<&str> {
    argv
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--device")
      .flat_map(|(i, _)| argv.get(i + 1).map(|v| v.as_str()))
      .collect()
  }

  #[test]
  fn compose_emits_selector_verbatim_exactly_once() {
    // `knobs.device` holds a real llama-server selector now. It must be
    // passed through unchanged and appear exactly once — a duplicate or
    // a mangled value (`0:0`) makes llama-server bail with
    // `invalid device`.
    for sel in ["Vulkan0", "Vulkan1", "CUDA0", "ROCm0"] {
      let mut p = base_params();
      p.knobs.device = Some(KnobValue::Set(sel.into()));
      let argv = strs(&compose(&p, 41100));
      let vals = device_values(&argv);
      assert_eq!(
        vals,
        vec![sel],
        "selector {sel} must be the only --device value"
      );
    }
  }

  #[test]
  fn compose_skips_device_when_none() {
    let p = base_params();
    assert!(p.knobs.device.is_none());
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| *a == "--device"));
  }

  #[test]
  fn compose_skips_device_when_empty_string() {
    // Empty selector means "auto-select" — no flag emitted.
    let mut p = base_params();
    p.knobs.device = Some(KnobValue::Set(String::new()));
    let argv = strs(&compose(&p, 41100));
    assert!(!argv.iter().any(|a| *a == "--device"));
  }

  #[test]
  fn argvify_never_emits_device() {
    // The selector belongs to compose, not argvify — otherwise it would
    // be emitted twice.
    let knobs = TypedKnobs {
      device: Some(KnobValue::Set("Vulkan1".into())),
      ..TypedKnobs::default()
    };
    let argv = strs(&argvify(&knobs));
    assert!(!argv.iter().any(|a| a == "--device"));
  }
}
