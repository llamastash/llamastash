//! SGLang's declared knobs.
//!
//! Transcribed from the live SGLang 0.5.18 server's flag surface (verified
//! 2026-09-07 against a running binary's argv and /get_server_info). The long
//! tail stays on `extras` — only the core set that maps onto a llamastash concept
//! appears here.
//!
//! Knob ids follow the shared convention: the flag name without the leading
//! `--` (e.g. `--context-length` → id "context-length").

use crate::launch::knobs::def::CTX_LADDER;
use crate::launch::knobs::{AutoKind, Concept, Emit, Group, KnobDef, KnobKind, Ring, Shape};

pub const KNOBS: &[KnobDef] = &[
  KnobDef {
    id: "context-length",
    flag: None,
    concept: Some(Concept::ContextLength),
    kind: KnobKind::U32 {
      max: Some(crate::config::MAX_CTX_TOKENS),
    },
    auto: Some(AutoKind::Delegate),
    group: Group::Context,
    label: "Context",
    help: "context length in tokens",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::UpToTrainedContext(CTX_LADDER),
    volatile: false,
  },
  KnobDef {
    id: "mem-fraction-static",
    flag: None,
    concept: None,
    kind: KnobKind::F32 {
      min: Some(0.0),
      max: Some(1.0),
    },
    auto: None,
    group: Group::Memory,
    label: "GPU memory frac",
    help: "fraction of GPU memory sglang may claim, 0.0-1.0",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::Fixed(&["0.5", "0.7", "0.8", "0.9", "0.95"]),
    volatile: true,
  },
  KnobDef {
    // The only deterministic bound on the KV pool: set, it overrides whatever
    // `mem-fraction-static` would have sized. Volatile like its sibling — a
    // persisted value would turn one preset experiment into a permanent
    // opt-out of the unified-memory guard.
    id: "max-total-tokens",
    flag: None,
    concept: None,
    kind: KnobKind::U32 { max: None },
    auto: None,
    group: Group::Memory,
    label: "KV pool tokens",
    help: "hard cap on tokens in the KV memory pool; overrides the GPU memory fraction",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::None,
    volatile: true,
  },
  KnobDef {
    id: "enable-unified-memory",
    flag: None,
    concept: None,
    kind: KnobKind::Bool,
    auto: None,
    group: Group::Memory,
    label: "Unified KV pool",
    help: "one dynamically split pool for hybrid (attention + SWA/Mamba) models instead of static partitions",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::BareFlagWhenTrue,
    ring: Ring::None,
    volatile: false,
  },
  KnobDef {
    id: "max-running-requests",
    flag: None,
    concept: Some(Concept::MaxConcurrency),
    kind: KnobKind::U32 { max: None },
    auto: None,
    group: Group::Throughput,
    label: "Max sequences",
    help: "ceiling on concurrently batched requests",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::Fixed(&["1", "8", "16", "32", "64", "128", "256"]),
    volatile: false,
  },
  KnobDef {
    id: "quantization",
    flag: None,
    concept: None,
    kind: KnobKind::OpenEnum {
      choices: &[],
      shape: Shape::Identifier,
    },
    auto: None,
    group: Group::Advanced,
    label: "Quantization",
    help: "quantization method; free-form string (e.g. modelopt_fp4)",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::None,
    volatile: false,
  },
  KnobDef {
    id: "served-model-name",
    flag: None,
    concept: None,
    kind: KnobKind::OpenEnum {
      choices: &[],
      shape: Shape::Identifier,
    },
    auto: None,
    group: Group::Advanced,
    label: "Served model name",
    help: "name this model is served under",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::None,
    volatile: false,
  },
  KnobDef {
    id: "trust-remote-code",
    flag: None,
    concept: None,
    kind: KnobKind::Bool,
    auto: None,
    group: Group::Advanced,
    label: "Trust remote code",
    help: "execute custom model code shipped in the repo (only for repos you trust)",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::BareFlagWhenTrue,
    ring: Ring::None,
    volatile: false,
  },
  KnobDef {
    id: "tool-call-parser",
    flag: None,
    concept: None,
    kind: KnobKind::OpenEnum {
      choices: &[],
      shape: Shape::Identifier,
    },
    auto: None,
    group: Group::Advanced,
    label: "Tool call parser",
    help: "string (e.g. qwen3_coder)",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::None,
    volatile: false,
  },
  KnobDef {
    id: "reasoning-parser",
    flag: None,
    concept: None,
    kind: KnobKind::OpenEnum {
      choices: &[],
      shape: Shape::Identifier,
    },
    auto: None,
    group: Group::Advanced,
    label: "Reasoning parser",
    help: "string (e.g. nemotron_3)",
    aliases: &[],
    fallback: crate::launch::params::LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
    ring: Ring::None,
    volatile: false,
  },
];

/// Inline tests for the SGLang knob table.
///
/// These assertions must FAIL if the table is wrong (e.g. a knob id is
/// duplicated, a concept is missing, a bound is wrong, or a flag spelling
/// leaked into an id). They are intentionally strict — a passing test means
/// the table satisfies every invariant listed in the spec.
#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::knobs::Concept;

  /// Every knob id must be unique — duplicates would render the picker
  /// ambiguous and break persistence.
  #[test]
  fn knob_ids_are_unique() {
    let ids: Vec<&str> = KNOBS.iter().map(|d| d.id).collect();
    let mut seen = std::collections::HashSet::new();
    for id in &ids {
      assert!(seen.insert(*id), "duplicate knob id: {id}");
    }
  }

  /// The context-length knob must exist and carry Concept::ContextLength.
  #[test]
  fn context_length_knob_has_concept() {
    let knob = KNOBS
      .iter()
      .find(|d| d.id == "context-length")
      .expect("context-length knob must exist in KNOBS");
    assert!(
      knob.concept == Some(Concept::ContextLength),
      "context-length must carry Concept::ContextLength, got {:?}",
      knob.concept
    );
  }

  /// The mem-fraction-static knob must be F32 bounded min 0.0 / max 1.0.
  #[test]
  fn mem_fraction_static_is_f32_bounded() {
    let knob = KNOBS
      .iter()
      .find(|d| d.id == "mem-fraction-static")
      .expect("mem-fraction-static knob must exist in KNOBS");
    match &knob.kind {
      KnobKind::F32 { min, max } => {
        assert_eq!(*min, Some(0.0), "mem-fraction-static min must be 0.0");
        assert_eq!(*max, Some(1.0), "mem-fraction-static max must be 1.0");
      }
      other => panic!("mem-fraction-static kind must be F32, got {:?}", other),
    }
  }

  /// The token cap is what the unified-memory guard writes, so it has to be
  /// an unbounded integer and must not persist past the launch that set it.
  #[test]
  fn max_total_tokens_is_an_unbounded_volatile_u32() {
    let knob = KNOBS
      .iter()
      .find(|d| d.id == "max-total-tokens")
      .expect("max-total-tokens knob must exist in KNOBS");
    assert!(
      matches!(knob.kind, KnobKind::U32 { max: None }),
      "max-total-tokens must be an unbounded U32, got {:?}",
      knob.kind
    );
    assert!(knob.volatile, "max-total-tokens must not persist");
    assert!(matches!(knob.emit, Emit::FlagValue));
  }

  #[test]
  fn enable_unified_memory_is_a_bare_flag() {
    let knob = KNOBS
      .iter()
      .find(|d| d.id == "enable-unified-memory")
      .expect("enable-unified-memory knob must exist in KNOBS");
    assert!(matches!(knob.kind, KnobKind::Bool));
    assert!(matches!(knob.emit, Emit::BareFlagWhenTrue));
  }

  /// No knob id must start with "--" — ids are the flag name without dashes.
  #[test]
  fn no_knob_id_starts_with_dashes() {
    for def in KNOBS {
      assert!(
        !def.id.starts_with('-'),
        "knob id must not start with '-': '{}'",
        def.id
      );
      assert!(
        !def.id.starts_with("--"),
        "knob id must not start with '--': '{}'",
        def.id
      );
    }
  }
}
