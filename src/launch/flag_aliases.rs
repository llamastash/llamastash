//! Shared flag-alias recognition for the typed-knob surface.
//!
//! Three call sites need to recognise the same `--n-gpu-layers` /
//! `-ngl` / `--n-gpu-layers=N` alias families: the `compose` argv
//! emitter (so it emits canonical flag names), the CLI tail-args
//! parser (so it routes recognised flags into typed slots), and the
//! TUI extras-row inline edit. Centralising the table here keeps
//! those three in lock-step.

use crate::launch::params::LayerLabel;

/// One typed knob the editor surfaces. Keep in sync with
/// `crate::launch::knobs::KnobSet` in `crate::config`.
///
/// `Ctx` and `Reasoning` are surfaced as typed knobs so the resolver
/// chain and the editor render them through the same layer-source
/// machinery as everything else. Their argv emission is still
/// special-cased in `compose` (ctx → `-c <N>`, reasoning → the
/// `--jinja --reasoning-format deepseek` bundle); `argvify` skips
/// these two fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KnobField {
  Ctx,
  Reasoning,
  NGpuLayers,
  NCpuMoe,
  Threads,
  CacheTypeK,
  CacheTypeV,
  FlashAttn,
  Mlock,
  NoMmap,
  Parallel,
  BatchSize,
  UbatchSize,
  RopeFreqScale,
  Keep,
  Device,
  /// Proportional model split across GPUs (`--tensor-split 3,1`).
  TensorSplit,
  /// Primary GPU holding non-split tensors (`--main-gpu`).
  MainGpu,
  /// How llama-server splits across GPUs (`--split-mode none|layer|row`).
  SplitMode,
}

impl KnobField {
  /// Whether llama-server's `--fit` actually decides this knob when it's
  /// left unset — i.e. the placement / sizing parameters fit adjusts to
  /// make a model fit device memory. The `Auto` knob state (delegate to
  /// fit) is only meaningful for these; for every other knob `Auto`
  /// would emit nothing and behave identically to `Inherited`, so the
  /// seeding rule and the picker's cycle ring suppress the `Auto` stop
  /// off this list.
  ///
  /// Single source of truth: extend this match when a future
  /// llama-server build lets `--fit` govern another parameter.
  pub fn fit_governed(self) -> bool {
    matches!(
      self,
      KnobField::Ctx | KnobField::NGpuLayers | KnobField::NCpuMoe | KnobField::TensorSplit
    )
  }

  /// The snake_case serde key this knob serialises under in `crate::launch::knobs::KnobSet`
  /// and persisted state. Single source for any field-name display (the
  /// Settings label, error text) so a label can never drift from the
  /// persisted JSON key — a drift test pins it against the actual serde
  /// output.
  pub fn field_name(self) -> &'static str {
    match self {
      KnobField::Ctx => "ctx",
      KnobField::Reasoning => "reasoning",
      KnobField::NGpuLayers => "n_gpu_layers",
      KnobField::NCpuMoe => "n_cpu_moe",
      KnobField::Threads => "threads",
      KnobField::CacheTypeK => "cache_type_k",
      KnobField::CacheTypeV => "cache_type_v",
      KnobField::FlashAttn => "flash_attn",
      KnobField::Mlock => "mlock",
      KnobField::NoMmap => "no_mmap",
      KnobField::Parallel => "parallel",
      KnobField::BatchSize => "batch_size",
      KnobField::UbatchSize => "ubatch_size",
      KnobField::RopeFreqScale => "rope_freq_scale",
      KnobField::Keep => "keep",
      KnobField::Device => "device",
      KnobField::TensorSplit => "tensor_split",
      KnobField::MainGpu => "main_gpu",
      KnobField::SplitMode => "split_mode",
    }
  }
}

/// What the parser expects after the flag head. Bool consumes no
/// value; everything else takes one (either the next token or the
/// `=value` suffix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
  U32,
  F32,
  Bool,
  /// `cache_type_k` / `cache_type_v` allowed set.
  KvCacheType,
  /// `split_mode` allowed set (`none|layer|row`).
  SplitMode,
  /// Free-form string (e.g. device selector, tensor-split ratio).
  Str,
}

impl ValueKind {
  /// Placeholder shown after the flag in `--help` (clap value name).
  /// For the closed-set kinds it spells out the choices so the help is
  /// self-documenting; the free-form `Str` kinds get a generic name a
  /// caller can refine per-knob.
  pub fn cli_value_name(self) -> &'static str {
    match self {
      ValueKind::U32 => "N",
      ValueKind::F32 => "X",
      ValueKind::Bool => "BOOL",
      ValueKind::KvCacheType => "CACHE_TYPE",
      ValueKind::SplitMode => "none|layer|row",
      ValueKind::Str => "VALUE",
    }
  }
}

/// One row in the alias table.
pub struct KnobSpec {
  pub field: KnobField,
  pub canonical: &'static str,
  pub aliases: &'static [&'static str],
  pub kind: ValueKind,
  /// One-line description, shown as the flag's `--help` text on `start`
  /// (see `crate::cli::knob_flags`) and available to the TUI editor.
  /// Single source of truth — keep terse and imperative.
  pub help: &'static str,
  /// Layer reported by the resolver when no chain layer supplies a
  /// value. `ServerDefault` for flags whose omission falls back to
  /// llama-server's hardcoded default. `ModelDefault` for flags
  /// llama-server reads from the model file (GGUF header / chat
  /// template) when the flag is omitted.
  pub fallback_label: LayerLabel,
}

/// Known k/v cache quant types from a standard `llama-server` build,
/// used as the cycle ring in the TUI picker and the "known types" hint
/// in CLI / TUI validation errors. Re-exported from the single source of
/// truth next to the KV memory estimator in [`crate::gguf::memory`], so
/// the validation set and the byte-cost table stay in lock-step. Values
/// outside this list are still accepted when they look like a valid quant
/// identifier (see [`crate::cli::tail_args::is_custom_kv_cache_type`]), so
/// custom builds that add extra types (e.g. `fp4`, `turbo_quant`) are not
/// blocked at the llamastash layer.
pub use crate::gguf::memory::KV_CACHE_TYPES;

/// Allowed values for `split_mode` (matches llama-server's
/// `--split-mode` choices). `layer` is llama-server's own default;
/// `tensor` splits individual tensors across GPUs (upstream `--split-mode
/// tensor`).
pub const SPLIT_MODES: &[&str] = &["none", "layer", "row", "tensor"];

/// Canonical emission order. Pinned by the plan's Risks & Dependencies
/// table to keep argv diffs readable across releases. `Ctx` and
/// `Reasoning` sit at the top so the editor renders them first.
const SPECS: &[KnobSpec] = &[
  KnobSpec {
    field: KnobField::Ctx,
    canonical: "--ctx-size",
    aliases: &["-c"],
    kind: ValueKind::U32,
    help: "context length in tokens (0 = model's trained maximum)",
    fallback_label: LayerLabel::ModelDefault,
  },
  KnobSpec {
    field: KnobField::Reasoning,
    canonical: "--reasoning",
    aliases: &[],
    kind: ValueKind::Bool,
    help: "enable reasoning (jinja + deepseek reasoning-format bundle)",
    fallback_label: LayerLabel::ModelDefault,
  },
  KnobSpec {
    field: KnobField::NGpuLayers,
    canonical: "--n-gpu-layers",
    aliases: &["-ngl"],
    kind: ValueKind::U32,
    help: "layers offloaded to the GPU (0 = CPU-only)",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::NCpuMoe,
    canonical: "--n-cpu-moe",
    aliases: &["-ncmoe"],
    kind: ValueKind::U32,
    help: "MoE expert layers kept on the CPU (frees VRAM)",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::Device,
    canonical: "--device",
    aliases: &["-d"],
    kind: ValueKind::Str,
    help: "device selector(s), comma-separated (e.g. Vulkan0,Vulkan1)",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::TensorSplit,
    canonical: "--tensor-split",
    aliases: &["-ts"],
    kind: ValueKind::Str,
    help: "proportional split across GPUs (e.g. 3,1)",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::MainGpu,
    canonical: "--main-gpu",
    aliases: &["-mg"],
    kind: ValueKind::U32,
    help: "index of the primary GPU holding non-split tensors",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::SplitMode,
    canonical: "--split-mode",
    aliases: &["-sm"],
    kind: ValueKind::SplitMode,
    help: "how to split the model across GPUs",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::Threads,
    canonical: "--threads",
    aliases: &["-t"],
    kind: ValueKind::U32,
    help: "CPU threads used during generation",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::CacheTypeK,
    canonical: "--cache-type-k",
    aliases: &["-ctk"],
    kind: ValueKind::KvCacheType,
    help: "K cache quantization type",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::CacheTypeV,
    canonical: "--cache-type-v",
    aliases: &["-ctv"],
    kind: ValueKind::KvCacheType,
    help: "V cache quantization type",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::Parallel,
    canonical: "--parallel",
    aliases: &["-np"],
    kind: ValueKind::U32,
    help: "parallel sequences served concurrently",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::FlashAttn,
    canonical: "--flash-attn",
    aliases: &["-fa"],
    kind: ValueKind::Bool,
    help: "enable flash attention",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::Mlock,
    canonical: "--mlock",
    aliases: &[],
    kind: ValueKind::Bool,
    help: "lock the model in RAM (prevents swap-out)",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::NoMmap,
    canonical: "--no-mmap",
    aliases: &[],
    kind: ValueKind::Bool,
    help: "load the whole model into RAM instead of mmap",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::BatchSize,
    canonical: "--batch-size",
    aliases: &["-b"],
    kind: ValueKind::U32,
    help: "logical batch size for prompt processing",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::UbatchSize,
    canonical: "--ubatch-size",
    aliases: &["-ub"],
    kind: ValueKind::U32,
    help: "physical (micro) batch size",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::RopeFreqScale,
    canonical: "--rope-freq-scale",
    aliases: &[],
    kind: ValueKind::F32,
    help: "RoPE frequency scaling factor (context extension)",
    fallback_label: LayerLabel::ServerDefault,
  },
  KnobSpec {
    field: KnobField::Keep,
    canonical: "--keep",
    aliases: &[],
    kind: ValueKind::U32,
    help: "tokens kept from the initial prompt on context shift",
    fallback_label: LayerLabel::ServerDefault,
  },
];

/// All knob specs in canonical emission order. Used by `argvify` and
/// the typed editor to render rows top-to-bottom.
pub fn knob_specs() -> &'static [KnobSpec] {
  SPECS
}

/// Whether `field` is surfaced as a generated first-class `start` flag
/// (see `crate::cli::knob_flags`).
///
/// `Ctx` and `Reasoning` are excluded: they keep their dedicated
/// `--ctx` / `--reasoning` flags on `StartArgs` (with bundled
/// reasoning-format semantics), and a derived `--reasoning` would
/// collide with the existing one. Every other knob is CLI-derived, so
/// adding a spec row automatically yields a `start` flag.
pub fn is_cli_derived(field: KnobField) -> bool {
  !matches!(field, KnobField::Ctx | KnobField::Reasoning)
}

/// Lookup a spec by field.
pub fn spec_for(field: KnobField) -> &'static KnobSpec {
  SPECS
    .iter()
    .find(|s| s.field == field)
    .expect("knob_specs is exhaustive over KnobField")
}

/// Result of `recognise` — what the token matched, and whether the
/// value was already inlined via `=`.
#[derive(Debug, PartialEq, Eq)]
pub struct Recognised<'a> {
  pub field: KnobField,
  pub kind: ValueKind,
  /// `Some(v)` when the caller passed `--flag=value`. `None` means
  /// "consume the next argv token as the value" (or no value for
  /// `ValueKind::Bool`).
  pub inline_value: Option<&'a str>,
}

/// Match a single argv token against the alias table. Returns `None`
/// for unrecognised flags so the caller can route them to `extras`.
///
/// Match is case-insensitive on the flag head (canonical / aliases
/// are lowercase already; user-typed flags get lowercased before
/// comparison).
pub fn recognise(token: &str) -> Option<Recognised<'_>> {
  let (head, inline) = match token.split_once('=') {
    Some((h, v)) => (h, Some(v)),
    None => (token, None),
  };
  let lower = head.to_ascii_lowercase();
  for spec in SPECS {
    if spec.canonical == lower || spec.aliases.iter().any(|a| *a == lower) {
      return Some(Recognised {
        field: spec.field,
        kind: spec.kind,
        inline_value: inline,
      });
    }
  }
  None
}

/// One titled cluster of knobs in the Settings editor's **display**
/// order. This is deliberately distinct from [`knob_specs`] (which is
/// the pinned *argv* emission order): the editor groups knobs by what
/// they do and surfaces the most-changed clusters first, while argv
/// order stays stable so recorded launch argv / goldens don't churn
/// when the UI is reorganised.
pub struct KnobGroup {
  /// Header rendered above the group's rows.
  pub title: &'static str,
  /// Knobs in this group, top-to-bottom.
  pub fields: &'static [KnobField],
  /// When true, the whole group (header + rows) is hidden unless the
  /// host exposes more than one selectable device — these knobs are
  /// meaningless on single-GPU / CPU-only hosts.
  pub multi_device_only: bool,
}

/// Editor display groups, ordered by how often a typical user touches
/// them. Every [`KnobField`] appears exactly once across the groups
/// (drift-guarded by a test). The flat concatenation of `fields` is
/// also the vertical navigation order (see
/// `crate::tui::launch_picker::PickerField::all`).
const DISPLAY_GROUPS: &[KnobGroup] = &[
  KnobGroup {
    title: "Context",
    fields: &[KnobField::Ctx, KnobField::Reasoning],
    multi_device_only: false,
  },
  KnobGroup {
    title: "GPU / CPU offload",
    fields: &[KnobField::NGpuLayers, KnobField::NCpuMoe],
    multi_device_only: false,
  },
  KnobGroup {
    title: "Multi-GPU placement",
    fields: &[
      KnobField::Device,
      KnobField::TensorSplit,
      KnobField::MainGpu,
      KnobField::SplitMode,
    ],
    multi_device_only: true,
  },
  KnobGroup {
    title: "Attention & KV cache",
    fields: &[
      KnobField::FlashAttn,
      KnobField::CacheTypeK,
      KnobField::CacheTypeV,
    ],
    multi_device_only: false,
  },
  KnobGroup {
    title: "Throughput",
    fields: &[
      KnobField::Threads,
      KnobField::Parallel,
      KnobField::BatchSize,
      KnobField::UbatchSize,
    ],
    multi_device_only: false,
  },
  KnobGroup {
    title: "Memory loading",
    fields: &[KnobField::Mlock, KnobField::NoMmap],
    multi_device_only: false,
  },
  KnobGroup {
    title: "Advanced",
    fields: &[KnobField::RopeFreqScale, KnobField::Keep],
    multi_device_only: false,
  },
];

/// Editor display groups in render / navigation order.
pub fn knob_display_groups() -> &'static [KnobGroup] {
  DISPLAY_GROUPS
}

/// Whether a knob row is shown / navigable for the given host. Only
/// the `multi_device_only` groups are conditional — gated on the host
/// exposing more than one selectable device. Unknown fields (should
/// never happen — the drift test covers every field) default to
/// visible.
pub fn knob_row_visible(field: KnobField, multi_device: bool) -> bool {
  for group in DISPLAY_GROUPS {
    if group.fields.contains(&field) {
      return !group.multi_device_only || multi_device;
    }
  }
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn recognise_canonical_long_flag() {
    let r = recognise("--threads").unwrap();
    assert_eq!(r.field, KnobField::Threads);
    assert_eq!(r.kind, ValueKind::U32);
    assert_eq!(r.inline_value, None);
  }

  #[test]
  fn recognise_short_alias() {
    let r = recognise("-ngl").unwrap();
    assert_eq!(r.field, KnobField::NGpuLayers);
  }

  #[test]
  fn recognise_n_cpu_moe_canonical_and_alias() {
    let canonical = recognise("--n-cpu-moe").unwrap();
    assert_eq!(canonical.field, KnobField::NCpuMoe);
    assert_eq!(canonical.kind, ValueKind::U32);
    let alias = recognise("-ncmoe").unwrap();
    assert_eq!(alias.field, KnobField::NCpuMoe);
  }

  #[test]
  fn recognise_equals_form_splits_value() {
    let r = recognise("--threads=8").unwrap();
    assert_eq!(r.field, KnobField::Threads);
    assert_eq!(r.inline_value, Some("8"));
  }

  #[test]
  fn recognise_unknown_flag_returns_none() {
    assert!(recognise("--rope-freq-base").is_none());
    assert!(recognise("--ssl-key-file").is_none());
  }

  #[test]
  fn recognise_is_case_insensitive_on_flag_head() {
    let r = recognise("--THREADS=8").unwrap();
    assert_eq!(r.field, KnobField::Threads);
    assert_eq!(r.inline_value, Some("8"));
  }

  #[test]
  fn knob_specs_pinned_order_covers_every_field() {
    let canon: Vec<&str> = knob_specs().iter().map(|s| s.canonical).collect();
    assert_eq!(
      canon,
      vec![
        "--ctx-size",
        "--reasoning",
        "--n-gpu-layers",
        "--n-cpu-moe",
        "--device",
        "--tensor-split",
        "--main-gpu",
        "--split-mode",
        "--threads",
        "--cache-type-k",
        "--cache-type-v",
        "--parallel",
        "--flash-attn",
        "--mlock",
        "--no-mmap",
        "--batch-size",
        "--ubatch-size",
        "--rope-freq-scale",
        "--keep",
      ]
    );
  }

  #[test]
  fn every_spec_has_help_text() {
    // The `start` command derives a `--help` line per knob from
    // `spec.help` (see `crate::cli::knob_flags`); a blank one would
    // ship an undocumented flag.
    for spec in knob_specs() {
      assert!(
        !spec.help.trim().is_empty(),
        "{:?} ({}) has empty help text",
        spec.field,
        spec.canonical
      );
    }
  }

  #[test]
  fn ctx_and_reasoning_are_not_cli_derived() {
    // They keep their dedicated `--ctx` / `--reasoning` flags on
    // `StartArgs`; everything else becomes a generated `start` flag.
    assert!(!is_cli_derived(KnobField::Ctx));
    assert!(!is_cli_derived(KnobField::Reasoning));
    for spec in knob_specs() {
      if !matches!(spec.field, KnobField::Ctx | KnobField::Reasoning) {
        assert!(
          is_cli_derived(spec.field),
          "{:?} should be CLI-derived",
          spec.field
        );
      }
    }
  }

  #[test]
  fn recognise_new_placement_knobs_canonical_and_alias() {
    assert_eq!(
      recognise("--tensor-split").unwrap().field,
      KnobField::TensorSplit
    );
    assert_eq!(recognise("-ts").unwrap().field, KnobField::TensorSplit);
    assert_eq!(recognise("--main-gpu").unwrap().field, KnobField::MainGpu);
    assert_eq!(recognise("-mg").unwrap().field, KnobField::MainGpu);
    assert_eq!(
      recognise("--split-mode").unwrap().field,
      KnobField::SplitMode
    );
    assert_eq!(recognise("-sm").unwrap().field, KnobField::SplitMode);
  }

  #[test]
  fn display_groups_cover_every_knob_field_exactly_once() {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut count = 0usize;
    for group in knob_display_groups() {
      for f in group.fields {
        assert!(
          seen.insert(spec_for(*f).canonical),
          "{:?} appears in more than one display group",
          f
        );
        count += 1;
      }
    }
    assert_eq!(
      count,
      knob_specs().len(),
      "display groups must cover every knob in knob_specs() exactly once"
    );
  }

  #[test]
  fn multi_gpu_placement_knobs_hidden_on_single_device() {
    for field in [
      KnobField::Device,
      KnobField::TensorSplit,
      KnobField::MainGpu,
      KnobField::SplitMode,
    ] {
      assert!(!knob_row_visible(field, false), "{field:?} on single GPU");
      assert!(knob_row_visible(field, true), "{field:?} on multi GPU");
    }
    // A non-placement knob is always visible.
    assert!(knob_row_visible(KnobField::Ctx, false));
  }

  #[test]
  fn knob_specs_carry_fallback_labels() {
    for spec in knob_specs() {
      match spec.field {
        KnobField::Ctx | KnobField::Reasoning => {
          assert_eq!(
            spec.fallback_label,
            LayerLabel::ModelDefault,
            "{:?}",
            spec.field
          );
        }
        _ => {
          assert_eq!(
            spec.fallback_label,
            LayerLabel::ServerDefault,
            "{:?}",
            spec.field
          );
        }
      }
    }
  }
}
