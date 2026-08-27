//! vLLM's declared knobs.
//!
//! Transcribed from the pre-registry `VLLM_NATIVE_KNOBS` descriptors and
//! `VLLM_KNOB_FLAGS`, plus the context slot vLLM honoured through the old
//! `capabilities()` gate. Every flag was checked against a live vLLM install
//! (0.27.1); the long tail stays on `extras`.
//!
//! Three of these now carry a [`Concept`], which the old two-channel model had
//! no way to express — they were modelled twice, once as a llama.cpp IR slot
//! and once as an unrelated native knob:
//!
//! - `max-model-len` ↔ llama.cpp `--ctx-size`
//! - `max-num-seqs` ↔ llama.cpp `--parallel`
//! - `kv-cache-dtype` ↔ llama.cpp `--cache-type-k` / `-v`
//!
//! vLLM has one KV-cache dtype where llama.cpp has two. It claims the K
//! concept; the V concept has no vLLM counterpart, so a `cache-type-v` value
//! does not carry into a vLLM launch and is reported as dropped.

use crate::launch::knobs::{AutoKind, Concept, Emit, Group, KnobDef, KnobKind, Shape};
use crate::launch::params::LayerLabel;

pub const KNOBS: &[KnobDef] = &[
  KnobDef {
    id: "max-model-len",
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
    fallback: LayerLabel::ModelDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "kv-cache-memory-bytes",
    flag: None,
    concept: None,
    // Accepts suffixed sizes (`8G`), so free-form rather than numeric.
    kind: KnobKind::Str,
    auto: None,
    group: Group::Memory,
    label: "KV cache size",
    help: "hard cap on KV cache bytes (e.g. 8G); overrides the GPU memory fraction",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "gpu-memory-utilization",
    flag: None,
    concept: None,
    kind: KnobKind::F32 {
      min: Some(0.0),
      max: Some(1.0),
    },
    auto: None,
    group: Group::Memory,
    label: "GPU memory frac",
    help: "fraction of GPU memory vLLM may claim, 0.0-1.0 (vLLM default 0.92)",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "max-num-seqs",
    flag: None,
    concept: Some(Concept::MaxConcurrency),
    kind: KnobKind::U32 { max: None },
    auto: None,
    group: Group::Throughput,
    label: "Max sequences",
    help: "ceiling on concurrently batched sequences",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "tensor-parallel-size",
    flag: None,
    concept: None,
    kind: KnobKind::U32 { max: None },
    auto: None,
    group: Group::MultiGpu,
    label: "Tensor parallel",
    help: "GPUs to shard the model across on this host",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "dtype",
    flag: None,
    concept: None,
    kind: KnobKind::Enum {
      choices: &["auto", "half", "bfloat16", "float16", "float32"],
    },
    auto: None,
    group: Group::Advanced,
    label: "Weight dtype",
    help: "weight/activation dtype",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "kv-cache-dtype",
    flag: None,
    concept: Some(Concept::KvCacheKType),
    kind: KnobKind::OpenEnum {
      choices: &["auto", "fp8", "fp8_e5m2", "fp8_e4m3"],
      shape: Shape::Identifier,
    },
    auto: None,
    group: Group::Attention,
    label: "KV cache dtype",
    help: "KV cache dtype; the fp8 stops trade accuracy for cache headroom",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "quantization",
    flag: None,
    concept: None,
    kind: KnobKind::OpenEnum {
      choices: &["awq", "gptq", "fp8", "bitsandbytes"],
      shape: Shape::Identifier,
    },
    auto: None,
    group: Group::Advanced,
    label: "Quantization",
    help: "quantization method; leave unset to read it from the repo config",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::FlagValue,
  },
  KnobDef {
    id: "enforce-eager",
    flag: None,
    concept: None,
    kind: KnobKind::Bool,
    auto: None,
    group: Group::Advanced,
    label: "Eager mode",
    help: "skip graph capture — faster startup, lower steady-state throughput",
    aliases: &[],
    fallback: LayerLabel::ServerDefault,
    emit: Emit::BareFlagWhenTrue,
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
    fallback: LayerLabel::ServerDefault,
    emit: Emit::BareFlagWhenTrue,
  },
];
