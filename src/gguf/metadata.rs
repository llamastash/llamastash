//! Distil a parsed [`GgufHeader`] into a domain-relevant
//! [`ModelMetadata`] view: architecture, parameter count, dominant
//! quantisation, native context length, chat-template, tokenizer hint,
//! reasoning hint, and mode hint.
//!
//! Lookup is **best-effort**: GGUFs in the wild are inconsistent about
//! whether keys are present, what arch prefix they use, and what the
//! intended mode is. We bias toward "return None / Unknown" rather than
//! "fail the file" so that discovery (Unit 4) can still surface the row
//! with a partial/warning state.

use crate::gguf::header::{GgufHeader, GgufValue, TensorInfo};

/// High-level domain summary derived from a parsed [`GgufHeader`].
#[derive(Debug, Clone)]
pub struct ModelMetadata {
  pub arch: Option<String>,
  /// Approximate total parameter count, where derivable.
  pub total_parameters: Option<u64>,
  /// Common human label for the parameter count (e.g., "7B"). Optional;
  /// returned only when `total_parameters` is set and matches a familiar
  /// bucket.
  pub parameter_label: Option<String>,
  /// Dominant tensor quantisation across the model's weight tensors.
  pub quant: Quant,
  pub native_ctx: Option<u64>,
  pub chat_template: Option<String>,
  pub tokenizer_kind: Option<String>,
  /// Set when the GGUF advertises a `<think>` special token (DeepSeek-R1,
  /// Qwen3, Marco-O1, …). Implies `--reasoning-format deepseek --jinja`.
  /// Collapsed from a single-variant enum until a second reasoning style
  /// is in scope.
  pub reasoning_hint: bool,
  pub mode_hint: ModeHint,
  /// Sum of per-tensor storage bytes (the GGUF weights footprint).
  /// `None` when the header has no usable tensor info — typical for
  /// metadata-only GGUFs. Surfaced in `list_models` so the TUI can
  /// render a weights-only est-mem badge without re-reading the
  /// header on every refresh (origin: R8, est-mem render half).
  pub weights_bytes: Option<u64>,
}

/// GGML tensor quantisation tag the GGUF advertises. `Unknown(u32)` carries
/// the raw tag for upstream variants we haven't enumerated yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(non_camel_case_types)]
pub enum Quant {
  F32,
  F16,
  BF16,
  Q4_0,
  Q4_1,
  Q5_0,
  Q5_1,
  Q8_0,
  Q8_1,
  Q2_K,
  Q3_K,
  Q4_K,
  Q5_K,
  Q6_K,
  Q8_K,
  IQ2_XXS,
  IQ2_XS,
  IQ3_XXS,
  IQ1_S,
  IQ4_NL,
  IQ3_S,
  IQ2_S,
  IQ4_XS,
  IQ1_M,
  TQ1_0,
  TQ2_0,
  I8,
  I16,
  I32,
  I64,
  F64,
  Unknown(u32),
}

impl Quant {
  /// Map from the raw GGML type tag found in tensor info.
  pub fn from_ggml_tag(tag: u32) -> Self {
    match tag {
      0 => Quant::F32,
      1 => Quant::F16,
      2 => Quant::Q4_0,
      3 => Quant::Q4_1,
      6 => Quant::Q5_0,
      7 => Quant::Q5_1,
      8 => Quant::Q8_0,
      9 => Quant::Q8_1,
      10 => Quant::Q2_K,
      11 => Quant::Q3_K,
      12 => Quant::Q4_K,
      13 => Quant::Q5_K,
      14 => Quant::Q6_K,
      15 => Quant::Q8_K,
      16 => Quant::IQ2_XXS,
      17 => Quant::IQ2_XS,
      18 => Quant::IQ3_XXS,
      19 => Quant::IQ1_S,
      20 => Quant::IQ4_NL,
      21 => Quant::IQ3_S,
      22 => Quant::IQ2_S,
      23 => Quant::IQ4_XS,
      24 => Quant::I8,
      25 => Quant::I16,
      26 => Quant::I32,
      27 => Quant::I64,
      28 => Quant::F64,
      29 => Quant::IQ1_M,
      30 => Quant::BF16,
      34 => Quant::TQ1_0,
      35 => Quant::TQ2_0,
      other => Quant::Unknown(other),
    }
  }

  /// (`elements_per_block`, `bytes_per_block`) for this quant. Returns
  /// (1, 2) as a conservative default for `Unknown` so estimators don't
  /// divide by zero.
  pub fn block_geometry(&self) -> (u64, u64) {
    match self {
      Quant::F32 => (1, 4),
      Quant::F16 | Quant::BF16 => (1, 2),
      Quant::F64 | Quant::I64 => (1, 8),
      Quant::I32 => (1, 4),
      Quant::I16 => (1, 2),
      Quant::I8 => (1, 1),
      Quant::Q4_0 => (32, 18),
      Quant::Q4_1 => (32, 20),
      Quant::Q5_0 => (32, 22),
      Quant::Q5_1 => (32, 24),
      Quant::Q8_0 => (32, 34),
      Quant::Q8_1 => (32, 36),
      Quant::Q2_K => (256, 84),
      Quant::Q3_K => (256, 110),
      Quant::Q4_K => (256, 144),
      Quant::Q5_K => (256, 176),
      Quant::Q6_K => (256, 210),
      Quant::Q8_K => (256, 292),
      Quant::IQ2_XXS => (256, 66),
      Quant::IQ2_XS => (256, 74),
      Quant::IQ2_S => (256, 82),
      Quant::IQ3_XXS => (256, 98),
      Quant::IQ3_S => (256, 110),
      Quant::IQ1_S => (256, 50),
      Quant::IQ1_M => (256, 56),
      Quant::IQ4_NL => (32, 18),
      Quant::IQ4_XS => (256, 136),
      Quant::TQ1_0 => (256, 54),
      Quant::TQ2_0 => (256, 66),
      Quant::Unknown(_) => (1, 2),
    }
  }

  /// Estimate on-disk tensor bytes for a GGML tensor with these dimensions.
  ///
  /// Quantized GGML blocks are row-oriented: the first dimension is rounded
  /// up to whole blocks for every row formed by the remaining dimensions.
  /// Flattening the full element count before rounding would undercount
  /// tensors whose rows are not block-aligned.
  pub fn tensor_storage_bytes(&self, dims: &[u64]) -> u64 {
    let Some((&row_width, rest)) = dims.split_first() else {
      return 0;
    };
    let (elems_per_block, bytes_per_block) = self.block_geometry();
    if elems_per_block == 0 {
      return 0;
    }
    let rows = rest.iter().copied().fold(1u64, u64::saturating_mul);
    row_width
      .div_ceil(elems_per_block)
      .saturating_mul(rows)
      .saturating_mul(bytes_per_block)
  }

  /// Reverse of [`Self::label`] — parses the canonical wire label
  /// (`"Q4_K"`, `"IQ3_S"`, …) back into a `Quant`. Returns `None`
  /// for unrecognised labels so the caller decides between
  /// `Unknown(0)` and a hard error. Stays in sync with `label()`
  /// because it walks the same enum.
  pub fn from_label(label: &str) -> Option<Self> {
    use Quant::*;
    let all = [
      F32, F16, BF16, Q4_0, Q4_1, Q5_0, Q5_1, Q8_0, Q8_1, Q2_K, Q3_K, Q4_K, Q5_K, Q6_K, Q8_K,
      IQ2_XXS, IQ2_XS, IQ2_S, IQ3_XXS, IQ3_S, IQ1_S, IQ1_M, IQ4_NL, IQ4_XS, TQ1_0, TQ2_0, I8, I16,
      I32, I64, F64,
    ];
    all.into_iter().find(|q| q.label() == label)
  }

  pub fn label(&self) -> &'static str {
    match self {
      Quant::F32 => "F32",
      Quant::F16 => "F16",
      Quant::BF16 => "BF16",
      Quant::Q4_0 => "Q4_0",
      Quant::Q4_1 => "Q4_1",
      Quant::Q5_0 => "Q5_0",
      Quant::Q5_1 => "Q5_1",
      Quant::Q8_0 => "Q8_0",
      Quant::Q8_1 => "Q8_1",
      Quant::Q2_K => "Q2_K",
      Quant::Q3_K => "Q3_K",
      Quant::Q4_K => "Q4_K",
      Quant::Q5_K => "Q5_K",
      Quant::Q6_K => "Q6_K",
      Quant::Q8_K => "Q8_K",
      Quant::IQ2_XXS => "IQ2_XXS",
      Quant::IQ2_XS => "IQ2_XS",
      Quant::IQ2_S => "IQ2_S",
      Quant::IQ3_XXS => "IQ3_XXS",
      Quant::IQ3_S => "IQ3_S",
      Quant::IQ1_S => "IQ1_S",
      Quant::IQ1_M => "IQ1_M",
      Quant::IQ4_NL => "IQ4_NL",
      Quant::IQ4_XS => "IQ4_XS",
      Quant::TQ1_0 => "TQ1_0",
      Quant::TQ2_0 => "TQ2_0",
      Quant::I8 => "I8",
      Quant::I16 => "I16",
      Quant::I32 => "I32",
      Quant::I64 => "I64",
      Quant::F64 => "F64",
      Quant::Unknown(_) => "Unknown",
    }
  }
}

/// What kind of inference surface this GGUF best matches. `Unknown` is the
/// safe fallback — the launcher (Unit 6 / Unit 8) asks the user to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeHint {
  Chat,
  Embedding,
  Rerank,
  Unknown,
}

/// Distil a parsed header into [`ModelMetadata`].
pub fn summarise(header: &GgufHeader) -> ModelMetadata {
  let arch_raw = header
    .string(&["general.architecture"])
    .map(str::to_string);
  let arch_key = arch_raw.as_deref();

  let native_ctx = arch_key.and_then(|a| header.u64(&[format!("{a}.context_length")]));

  let chat_template = header
    .string(&["tokenizer.chat_template"])
    .map(str::to_string);
  let tokenizer_kind = header
    .string(&["tokenizer.ggml.model"])
    .map(str::to_string);

  let total_parameters = parameter_count(header, arch_key);
  let parameter_label = total_parameters.and_then(label_for_param_count);

  let quant = dominant_quant(&header.tensors);
  let mode_hint = infer_mode_hint(header, arch_key);
  let reasoning_hint = infer_reasoning_hint(header);
  let weights_bytes = {
    let bytes = crate::gguf::memory::weights_bytes(header);
    if bytes == 0 {
      None
    } else {
      Some(bytes)
    }
  };

  ModelMetadata {
    arch: arch_raw,
    total_parameters,
    parameter_label,
    quant,
    native_ctx,
    chat_template,
    tokenizer_kind,
    reasoning_hint,
    mode_hint,
    weights_bytes,
  }
}

/// Parameter count: prefer `general.parameter_count` (explicit), then sum
/// of element counts across "weight" tensors as a fallback.
fn parameter_count(header: &GgufHeader, arch: Option<&str>) -> Option<u64> {
  if let Some(p) = header.u64(&["general.parameter_count"]) {
    return Some(p);
  }
  // Architecture-prefixed variants seen in some GGUFs.
  if let Some(a) = arch {
    if let Some(p) = header.u64(&[format!("{a}.parameter_count")]) {
      return Some(p);
    }
  }
  let summed: u64 = header
    .tensors
    .iter()
    .filter(|t| t.name.ends_with(".weight") || t.name.ends_with(".bias"))
    .map(|t| t.n_elements())
    .fold(0u64, u64::saturating_add);
  if summed == 0 {
    None
  } else {
    Some(summed)
  }
}

/// Round a raw parameter count to a familiar "0.5B / 1B / 3B / 7B / 13B"
/// bucket. Returns `None` if the count is too small to label confidently.
fn label_for_param_count(count: u64) -> Option<String> {
  const G: u64 = 1_000_000_000;
  const M: u64 = 1_000_000;
  let buckets_b: &[(u64, &str)] = &[
    (500 * M, "0.5B"),
    (G, "1B"),
    (3 * G, "3B"),
    (7 * G, "7B"),
    (8 * G, "8B"),
    (13 * G, "13B"),
    (20 * G, "20B"),
    (30 * G, "30B"),
    (34 * G, "34B"),
    (70 * G, "70B"),
    (180 * G, "180B"),
  ];
  if count < 100 * M {
    return None;
  }
  let (_, label) = buckets_b
    .iter()
    .min_by_key(|(target, _)| (count as i128 - *target as i128).unsigned_abs())?;
  Some((*label).to_string())
}

/// The most byte-significant quant across weight tensors. Mirrors the
/// convention of llama.cpp's filename labels (which name the dominant K-quant
/// even though norm tensors stay F32).
fn dominant_quant(tensors: &[TensorInfo]) -> Quant {
  if tensors.is_empty() {
    return Quant::Unknown(0);
  }
  // Sum bytes per ggml-type across "weight" tensors only — biases / norms
  // are usually F32 and would otherwise drown out the headline quant.
  let mut by_type: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
  for t in tensors {
    if !t.name.ends_with(".weight") {
      continue;
    }
    let bytes = Quant::from_ggml_tag(t.ggml_type).tensor_storage_bytes(&t.dims);
    let entry = by_type.entry(t.ggml_type).or_default();
    *entry = entry.saturating_add(bytes);
  }
  // Fall back to "first tensor" if no `.weight` tensors exist.
  let (tag, _) = if by_type.is_empty() {
    (tensors[0].ggml_type, 0)
  } else {
    by_type
      .into_iter()
      .max_by_key(|(_, bytes)| *bytes)
      .expect("non-empty checked")
  };
  Quant::from_ggml_tag(tag)
}

fn infer_mode_hint(header: &GgufHeader, arch: Option<&str>) -> ModeHint {
  let tensor_names: Vec<&str> = header.tensors.iter().map(|t| t.name.as_str()).collect();
  let has = |needle: &str| tensor_names.iter().any(|n| n == &needle);
  let any_contains = |needle: &str| tensor_names.iter().any(|n| n.contains(needle));

  let arch_hint = arch.unwrap_or("").to_ascii_lowercase();
  let tags_lc = header
    .string(&["general.tags"])
    .map(|s| s.to_ascii_lowercase())
    .unwrap_or_default();
  let name_lc = header
    .string(&["general.name"])
    .map(|s| s.to_ascii_lowercase())
    .unwrap_or_default();

  // Reranker first: very specific tensor signatures or
  // metadata-level marker.
  if arch_hint.contains("rerank")
    || tags_lc.contains("rerank")
    || name_lc.contains("rerank")
    || any_contains("cls.predictions")
    || has("cls.score.weight")
  {
    return ModeHint::Rerank;
  }

  // Embedding (checked BEFORE the tied-embedding chat heuristic):
  // some embedding models are finetunes of decoder-only LLMs (e.g.
  // `nomic-embed-code` from Qwen2) that still ship
  // `output_norm.weight`, but they advertise an explicit
  // `<arch>.pooling_type` key — the GGUF format's own embedding
  // signal — and/or `general.name` / `general.tags` carry "embed".
  // BERT-family arch is the strongest signal of all.
  let has_pooling_type = arch
    .map(|a| header.metadata.contains_key(&format!("{a}.pooling_type")))
    .unwrap_or(false);
  if arch_hint == "bert"
    || arch_hint.contains("embed")
    || tags_lc.contains("embed")
    || name_lc.contains("embed")
    || has_pooling_type
  {
    return ModeHint::Embedding;
  }

  // Chat: explicit output projection, OR a final `output_norm`
  // before the (often tied-to-input-embeddings) lm_head. Newer
  // decoder-only LLMs — Gemma 3/4, Qwen 3, several Mistral variants
  // — share input embeddings with the output projection and so omit
  // `output.weight` / `lm_head.weight` from the GGUF. Checking
  // `output_norm.weight` catches those without misclassifying
  // BERT-family encoders, which don't have it (they end with a
  // `pooler` or `cls.*` head instead).
  if has("output.weight") || has("lm_head.weight") || has("output_norm.weight") {
    return ModeHint::Chat;
  }

  // Older fallback: arch advertises embedding_length without any
  // output projection — almost certainly an encoder.
  if let Some(a) = arch {
    if header.u64(&[format!("{a}.embedding_length")]).is_some() {
      return ModeHint::Embedding;
    }
  }

  ModeHint::Unknown
}

fn infer_reasoning_hint(header: &GgufHeader) -> bool {
  // Scan the tokenizer.ggml.tokens array (when present) for `<think>` —
  // a strong, model-agnostic signal that the model emits explicit reasoning.
  if let Some(GgufValue::Array(items)) = header.metadata.get("tokenizer.ggml.tokens") {
    for v in items {
      if let GgufValue::String(s) = v {
        if s == "<think>" {
          return true;
        }
      }
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gguf::header::{read_reader, HeaderReadOptions};
  use crate::gguf::test_fixtures::FixtureBuilder;
  use std::io::Cursor as IoCursor;

  fn parse(bytes: Vec<u8>) -> ModelMetadata {
    let read = read_reader(IoCursor::new(bytes), HeaderReadOptions::default()).unwrap();
    summarise(&read.header)
  }

  #[test]
  fn chat_mode_when_output_weight_present() {
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_context_length(4096)
      .with_tensor("output.weight", &[128, 32000], 12)
      .with_tensor("blk.0.attn_q.weight", &[128, 128], 12)
      .build();
    let m = parse(bytes);
    assert_eq!(m.arch.as_deref(), Some("llama"));
    assert_eq!(m.native_ctx, Some(4096));
    assert_eq!(m.mode_hint, ModeHint::Chat);
    assert_eq!(m.quant, Quant::Q4_K);
  }

  #[test]
  fn chat_mode_when_output_norm_present_with_tied_embeddings() {
    // Regression: Gemma 3/4, Qwen 3, etc. tie input embeddings to the
    // output projection, so the GGUF lacks `output.weight` /
    // `lm_head.weight`. They still ship `output_norm.weight` — the
    // final RMSNorm before the LM head — which is enough to identify
    // them as decoder-only chat models.
    let bytes = FixtureBuilder::new()
      .with_arch("gemma3")
      .with_embedding_length(2048)
      .with_context_length(131_072)
      .with_tensor("token_embd.weight", &[2048, 256_000], 12)
      .with_tensor("blk.0.attn_q.weight", &[2048, 2048], 12)
      .with_tensor("output_norm.weight", &[2048], 12)
      .build();
    let m = parse(bytes);
    assert_eq!(m.arch.as_deref(), Some("gemma3"));
    assert_eq!(
      m.mode_hint,
      ModeHint::Chat,
      "gemma 3/4 must classify as chat despite tied output embeddings"
    );
  }

  #[test]
  fn embedding_mode_when_arch_pooling_type_is_set() {
    // `qwen2.pooling_type` (or any `<arch>.pooling_type`) is the
    // GGUF format's explicit signal for an embedding/pooling head.
    // Even when the model carries `output_norm.weight` from its
    // base decoder, the pooling_type key wins — that's how
    // nomic-embed-code (Qwen2 base, embedding head) is identified
    // in the wild.
    let bytes = FixtureBuilder::new()
      .with_arch("qwen2")
      .with_embedding_length(2048)
      .with_kv("qwen2.pooling_type", GgufValue::U64(0))
      .with_tensor("token_embd.weight", &[2048, 152_000], 12)
      .with_tensor("output_norm.weight", &[2048], 12)
      .build();
    let m = parse(bytes);
    assert_eq!(
      m.mode_hint,
      ModeHint::Embedding,
      "pooling_type must override the output_norm chat heuristic"
    );
  }

  #[test]
  fn embedding_mode_when_general_name_carries_embed_even_with_output_norm() {
    // Regression: nomic-embed-code is a Qwen2 finetune that ships
    // `output_norm.weight` (a decoder-only chat signal) but is
    // published purely as an embedding model. The `general.name` key
    // carries "embed" — when present, that overrides the chat
    // heuristic.
    let bytes = FixtureBuilder::new()
      .with_arch("qwen2")
      .with_embedding_length(2048)
      .with_kv("general.name", GgufValue::String("nomic-embed-code".into()))
      .with_tensor("token_embd.weight", &[2048, 152_000], 12)
      .with_tensor("blk.0.attn_q.weight", &[2048, 2048], 12)
      .with_tensor("output_norm.weight", &[2048], 12)
      .build();
    let m = parse(bytes);
    assert_eq!(m.arch.as_deref(), Some("qwen2"));
    assert_eq!(
      m.mode_hint,
      ModeHint::Embedding,
      "general.name=`*embed*` must classify as embedding even when output_norm is present"
    );
  }

  #[test]
  fn embedding_mode_for_bert_arch() {
    let bytes = FixtureBuilder::new()
      .with_arch("bert")
      .with_embedding_length(768)
      .with_tensor("blk.0.attn_q.weight", &[768, 768], 1)
      .build();
    let m = parse(bytes);
    assert_eq!(m.mode_hint, ModeHint::Embedding);
  }

  #[test]
  fn rerank_mode_for_cls_score() {
    let bytes = FixtureBuilder::new()
      .with_arch("bert")
      .with_tensor("cls.score.weight", &[768, 1], 1)
      .build();
    let m = parse(bytes);
    assert_eq!(m.mode_hint, ModeHint::Rerank);
  }

  #[test]
  fn unknown_mode_when_signals_missing() {
    let bytes = FixtureBuilder::new()
      .with_arch("mystery")
      .with_tensor("some.thing.weight", &[16, 16], 1)
      .build();
    let m = parse(bytes);
    assert_eq!(m.mode_hint, ModeHint::Unknown);
  }

  #[test]
  fn reasoning_hint_from_think_token() {
    let bytes = FixtureBuilder::new()
      .with_arch("qwen3")
      .with_kv(
        "tokenizer.ggml.tokens",
        GgufValue::Array(vec![
          GgufValue::String("<bos>".to_string()),
          GgufValue::String("<think>".to_string()),
          GgufValue::String("</think>".to_string()),
        ]),
      )
      .build();
    let m = parse(bytes);
    assert!(m.reasoning_hint);
  }

  #[test]
  fn quant_from_label_round_trips_every_variant() {
    // Tier-B sweep replaced an open-coded 15-arm `parse_quant`
    // with the canonical `Quant::from_label`. The contract is
    // strict round-tripping: every `label()` output must parse
    // back to the same variant. `Unknown(_)` is the only special
    // case — its label is the literal "Unknown" sentinel rather
    // than a round-trippable tag.
    use Quant::*;
    let canonical = [
      F32, F16, BF16, Q4_0, Q4_1, Q5_0, Q5_1, Q8_0, Q8_1, Q2_K, Q3_K, Q4_K, Q5_K, Q6_K, Q8_K,
      IQ2_XXS, IQ2_XS, IQ2_S, IQ3_XXS, IQ3_S, IQ1_S, IQ1_M, IQ4_NL, IQ4_XS, TQ1_0, TQ2_0, I8, I16,
      I32, I64, F64,
    ];
    for q in canonical {
      let label = q.label();
      assert_eq!(
        Quant::from_label(label),
        Some(q),
        "round-trip failed for {label}",
      );
    }
    assert_eq!(Quant::from_label("nope"), None);
  }

  #[test]
  fn reasoning_hint_absent_without_think_token() {
    // Tier-A YAGNI sweep collapsed `Option<ReasoningHint>` to `bool`.
    // The absence path used to be `None`; it's now `false`, but the
    // observable behaviour (no `<think>` ⇒ no reasoning hint) must
    // be unchanged.
    let bytes = FixtureBuilder::new().with_arch("llama").build();
    let m = parse(bytes);
    assert!(!m.reasoning_hint);
  }

  #[test]
  fn parameter_label_buckets_to_7b() {
    let label = label_for_param_count(6_900_000_000);
    assert_eq!(label.as_deref(), Some("7B"));
  }

  #[test]
  fn parameter_count_falls_back_to_tensor_sum() {
    // No general.parameter_count → sum of .weight tensors' element counts.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_tensor("output.weight", &[10, 10], 1)
      .with_tensor("blk.0.attn_q.weight", &[10, 10], 1)
      .build();
    let m = parse(bytes);
    assert_eq!(m.total_parameters, Some(200));
  }

  #[test]
  fn dominant_quant_counts_quantized_rows_with_padding() {
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_tensor("small.q4.weight", &[1, 3], 12)
      .with_tensor("larger.f16.weight", &[200], 1)
      .build();
    let m = parse(bytes);
    assert_eq!(m.quant, Quant::Q4_K);
  }
}
