//! Distil a parsed [`GgufHeader`] into a domain-relevant
//! [`ModelMetadata`] view: architecture, parameter count, dominant
//! quantisation, native context length, chat-template, tokenizer hint,
//! reasoning hint, and mode hint.
//!
//! Lookup is **best-effort**: GGUFs in the wild are inconsistent about
//! whether keys are present, what arch prefix they use, and what the
//! intended mode is. We bias toward "return None / Unknown" rather than
//! "fail the file" so that discovery can still surface the row
//! with a partial/warning state.

use crate::gguf::header::{GgufHeader, GgufValue, TensorInfo};

/// High-level domain summary derived from a parsed [`GgufHeader`].
#[derive(Debug, Clone)]
pub struct ModelMetadata {
  pub arch: Option<String>,
  /// Approximate total parameter count, where derivable.
  pub total_parameters: Option<u64>,
  /// Compact human label for the parameter count (e.g., "7B", "235B",
  /// "1.2T"). `None` when `total_parameters` is unset or below 100M.
  pub parameter_label: Option<String>,
  /// Dominant tensor quantisation across the model's weight tensors.
  pub quant: Quant,
  /// Verbatim quant string for backends whose quantisation has no GGML
  /// tag (e.g. an affine "4-bit gs64" or an AWQ/GPTQ label). `quant`
  /// is GGML-only and `Copy`, and `Unknown(u32)` means "unknown *GGML*
  /// tag" — wrong to reuse for a non-GGML scheme. GGUF constructors leave
  /// this `None`; a safetensors backend overlays it in its projection.
  /// Rendered verbatim where present (a future safetensors backend wires
  /// the display); GGUF output is unchanged while it stays `None`.
  pub quant_label: Option<String>,
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
  ///
  /// For split-GGUF entries the scanner replaces the shard-1-only
  /// value with the on-disk total across every shard (see
  /// `discovery::scanner::apply_split_total_weights`). The header-
  /// computed per-shard value would otherwise mis-report a 2-shard
  /// 80B model as ~half its real footprint in `list`, `show`, and
  /// the recommender's VRAM-fit predicate.
  pub weights_bytes: Option<u64>,
  /// Embedded multi-token-prediction (MTP / NextN) draft-head layer count,
  /// read from `{arch}.nextn_predict_layers` (`LLM_KV_NEXTN_PREDICT_LAYERS`).
  /// `Some(n>0)` means the file carries its own speculative draft head and so
  /// self-speculates with no separate drafter (Qwen3.5/3.6, GLM-4.x, DeepSeek). `None` when the key is absent
  /// or zero — the model is not embedded-MTP-capable. Orthogonal to a
  /// *separate* `mtp-*.gguf` head, which is a sibling-file property
  /// (`DiscoveredModel::mtp_head`), not a header one.
  pub mtp: Option<u32>,
}

impl ModelMetadata {
  /// The quant label to render: the backend's verbatim overlay where one
  /// exists, else the GGML tag.
  ///
  /// The single accessor on purpose. The fallback used to be written out at
  /// each render site, and two of five had it — so an AWQ safetensors row read
  /// `AWQ` in `list` and the TUI but `Unknown` through the Ollama-compat
  /// surface. Call this; never reach for `quant` directly to display.
  pub fn quant_display(&self) -> String {
    self
      .quant_label
      .clone()
      .unwrap_or_else(|| self.quant.label().to_string())
  }
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

  /// Map `general.file_type` (the `LLAMA_FTYPE` enum picked at quantization
  /// time) to the coarse quant family it names. This is the value
  /// llama.cpp's filename convention is built from — a `*-Q4_K_M.gguf` file
  /// carries `file_type = 15` — so it is the authoritative quant label.
  /// Prefer it over the `dominant_quant` tensor scan, whose by-bytes tally a
  /// large Q6_K token-embedding skews on big-vocab models (a Q4_K_M gemma
  /// would otherwise read as `Q6_K`). Returns `None` for the IQ*/TQ*/`GUESSED`
  /// ftypes whose enum values are less stable across llama.cpp versions, so
  /// the caller falls back to the tensor scan for those.
  ///
  /// Note this is a *different* enum from [`Self::from_ggml_tag`], which maps
  /// per-tensor GGML type tags (e.g. tag 14 is `Q6_K`, ftype 14 is `Q4_K_S`).
  pub fn from_file_type(ftype: u32) -> Option<Self> {
    use Quant::*;
    Some(match ftype {
      0 => F32,
      1 => F16,
      2 => Q4_0,
      3 => Q4_1,
      7 => Q8_0,
      8 => Q5_0,
      9 => Q5_1,
      10 | 21 => Q2_K, // Q2_K, Q2_K_S
      11..=13 => Q3_K, // Q3_K_S / Q3_K_M / Q3_K_L
      14..=15 => Q4_K, // Q4_K_S / Q4_K_M
      16..=17 => Q5_K, // Q5_K_S / Q5_K_M
      18 => Q6_K,
      32 => BF16,
      _ => return None,
    })
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
/// safe fallback — the launcher asks the user to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeHint {
  Chat,
  Embedding,
  Rerank,
  Unknown,
}

impl ModeHint {
  /// Stable lowercase label matching the CLI / IPC wire
  /// (`--mode chat|embedding|rerank`). `Unknown` returns `None` so
  /// callers can branch on "no signal" without comparing against a
  /// magic string.
  pub fn as_label(&self) -> Option<&'static str> {
    match self {
      ModeHint::Chat => Some("chat"),
      ModeHint::Embedding => Some("embedding"),
      ModeHint::Rerank => Some("rerank"),
      ModeHint::Unknown => None,
    }
  }
}

/// Distil a parsed header into [`ModelMetadata`].
pub fn summarise(header: &GgufHeader) -> ModelMetadata {
  let arch_raw = header.string(&["general.architecture"]).map(str::to_string);
  let arch_key = arch_raw.as_deref();

  let native_ctx = arch_key.and_then(|a| header.u64(&[format!("{a}.context_length")]));

  let chat_template = header
    .string(&["tokenizer.chat_template"])
    .map(str::to_string);
  let tokenizer_kind = header.string(&["tokenizer.ggml.model"]).map(str::to_string);

  let total_parameters = parameter_count(header);
  let parameter_label = total_parameters.and_then(label_for_param_count);

  // Prefer the file's declared quant family (`general.file_type`, the value
  // llama.cpp's filename convention is built from) over a dominant-by-bytes
  // tensor scan; the scan mislabels big-vocab models whose Q6_K token
  // embedding outweighs a Q4_K body (a Q4_K_M gemma would read as Q6_K).
  let quant = header
    .u64(&["general.file_type"])
    .and_then(|ft| Quant::from_file_type(ft as u32))
    .unwrap_or_else(|| dominant_quant(&header.tensors));
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
  // Embedded MTP draft-head layer count. `> 0` ⇒ the model carries its own
  // draft head and self-speculates; absent/zero ⇒ not embedded-MTP-capable.
  let mtp = arch_key
    .and_then(|a| header.u64(&[format!("{a}.nextn_predict_layers")]))
    .filter(|n| *n > 0)
    .map(|n| n as u32);

  ModelMetadata {
    arch: arch_raw,
    total_parameters,
    parameter_label,
    quant,
    // GGUF carries a GGML quant tag, not a verbatim non-GGML label.
    quant_label: None,
    native_ctx,
    chat_template,
    tokenizer_kind,
    reasoning_hint,
    mode_hint,
    weights_bytes,
    mtp,
  }
}

/// Parameter count: prefer an explicit model-level count, else sum the
/// element counts of the "weight"/"bias" tensors present in this header.
/// The tensor sum is per-file, so for a split GGUF it covers only the
/// parsed shard — the scanner aggregates the rest across siblings.
fn parameter_count(header: &GgufHeader) -> Option<u64> {
  if let Some(p) = explicit_parameter_count(header) {
    return Some(p);
  }
  let summed = tensor_param_sum(header);
  (summed > 0).then_some(summed)
}

/// The explicit model-level parameter count a GGUF may declare
/// (`general.parameter_count`, or the arch-prefixed variant). `None` when
/// the header carries no such key — the count then comes from
/// [`tensor_param_sum`], which is per-shard for a split GGUF and so must
/// be aggregated across shards, whereas an explicit count is already the
/// whole-model figure.
pub(crate) fn explicit_parameter_count(header: &GgufHeader) -> Option<u64> {
  if let Some(p) = header.u64(&["general.parameter_count"]) {
    return Some(p);
  }
  let arch = header.string(&["general.architecture"]);
  arch.and_then(|a| header.u64(&[format!("{a}.parameter_count")]))
}

/// Sum the element counts of the "weight"/"bias" tensors in one header.
/// The fallback parameter estimate when no explicit count is declared,
/// and the per-shard contribution the scanner sums across a split
/// model's shards.
pub(crate) fn tensor_param_sum(header: &GgufHeader) -> u64 {
  header
    .tensors
    .iter()
    .filter(|t| t.name.ends_with(".weight") || t.name.ends_with(".bias"))
    .map(|t| t.n_elements())
    .fold(0u64, u64::saturating_add)
}

/// Format a raw parameter count as a compact, accurate label:
/// `22_700_000` → `22.7M`, `124_000_000` → `124M`, `6_900_000_000` → `6.9B`,
/// `8_030_000_000` → `8B`, `284_334_567_511` → `284B`,
/// `1_200_000_000_000` → `1.2T`. A value at/above 100 in its unit drops the
/// decimal, and a trailing `.0` is trimmed so `7.0B` reads as `7B`. `None`
/// only for a zero count (the caller already suppresses a zero tensor sum).
/// Shared with the backend-neutral safetensors substrate
/// ([`crate::discovery::hf_repos::config_to_metadata`]) so a config-dim
/// param estimate renders with the same labels GGUF rows use.
pub(crate) fn label_for_param_count(count: u64) -> Option<String> {
  if count == 0 {
    return None;
  }
  let n = count as f64;
  let (value, unit) = if n >= 1e12 {
    (n / 1e12, "T")
  } else if n >= 1e9 {
    (n / 1e9, "B")
  } else {
    (n / 1e6, "M")
  };
  let label = if value >= 100.0 {
    format!("{value:.0}{unit}")
  } else {
    let one_dp = format!("{value:.1}");
    let trimmed = one_dp.strip_suffix(".0").unwrap_or(&one_dp);
    format!("{trimmed}{unit}")
  };
  Some(label)
}

/// The most byte-significant quant across weight tensors. Used as a
/// **fallback** when `general.file_type` is absent or maps to an
/// unstable IQ*/TQ* enum value (see [`Quant::from_file_type`], which is
/// preferred because it matches llama.cpp's filename labels). On
/// big-vocab models a large Q6_K token-embedding can outweigh a Q4_K
/// body here, so this is a best-effort label, not authoritative.
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

  // Only shard 1 of a split set is parsed, and the output projection may sit
  // in any shard, so "no output projection here" says nothing about the model.
  // The encoder fallback below would read that silence as an embedding model.
  // Covers both shapes seen in the wild: a metadata-only first shard
  // (Qwen3.8-Flash-Next, 0 of 1224 tensors) and llama-gguf-split's, which puts
  // tensors in shard 1 but the projection later.
  let is_split = header.u64(&["split.count"]).is_some_and(|n| n > 1);
  if is_split || header.tensors.is_empty() {
    // Chat-template presence, in the three shapes `gguf_writer.py` actually
    // emits. `tokenizer.chat_template` is always written with `add_string`, so
    // it is never an array; the multi-template case writes the *names* to
    // `tokenizer.chat_templates` and each body to `tokenizer.chat_template.<name>`.
    // And when the source list carries no `default` entry, `add_chat_template`
    // returns before writing the singular key at all — that model has templates
    // but no default, and reading only the singular key calls it template-less.
    let has_template = header
      .string(&["tokenizer.chat_template"])
      .is_some_and(|t| !t.is_empty())
      || header
        .metadata
        .get("tokenizer.chat_templates")
        .is_some_and(|v| matches!(v, GgufValue::Array(items) if !items.is_empty()))
      || header
        .metadata
        .keys()
        .any(|k| k.starts_with("tokenizer.chat_template."));
    // Judgement, twice-reviewed, recorded so it is not re-litigated blind: a
    // chat template is weak evidence, but `Unknown` is worse. The CLI refuses
    // an unknown hint outright (`cli::start`), so `Unknown` would make every
    // split chat model without a preset fail `start` — and split is the norm
    // for large GGUFs. The daemon and proxy resolve `Unknown` to Chat anyway,
    // so it would not even stop a decoder-arch embedder being served as chat
    // there; it only moves the failure to the CLI. An *unsplit* model of that
    // kind already reads Chat here via `output_norm.weight`, so this keeps
    // split and unsplit consistent rather than making split stricter.
    // The residual miss is a split decoder finetuned for embeddings that ships
    // a chat template and carries no embed/pooling/rerank signal; log it so it
    // is at least visible in daemon logs.
    return if has_template {
      log::debug!(
        "{}: split shard has no output projection; inferring chat from the \
         chat template",
        header
          .string(&["general.name"])
          .or(arch)
          .unwrap_or("<unnamed>")
      );
      ModeHint::Chat
    } else {
      ModeHint::Unknown
    };
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

/// Reasoning markers — special tokens (GGUF) / chat-template substrings
/// (safetensors) that signal a model emits explicit reasoning. Shared so the
/// GGUF token scan here and the safetensors chat-template scan in
/// [`crate::discovery::hf_repos`] grow from one list instead of drifting.
pub(crate) const REASONING_MARKERS: &[&str] = &["<think>"];

fn infer_reasoning_hint(header: &GgufHeader) -> bool {
  // Scan the tokenizer.ggml.tokens array (when present) for a reasoning
  // marker — a strong, model-agnostic signal the model emits explicit reasoning.
  if let Some(GgufValue::Array(items)) = header.metadata.get("tokenizer.ggml.tokens") {
    for v in items {
      if let GgufValue::String(s) = v {
        if REASONING_MARKERS.contains(&s.as_str()) {
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
  fn a_split_shard_without_the_output_projection_is_not_an_embedding_model() {
    // Only shard 1 is parsed and the projection may live in a later shard, so
    // its absence here proves nothing. Both wild shapes: llama-gguf-split
    // leaves tensors in shard 1, Qwen3.8-Flash-Next leaves none.
    for tensors in [true, false] {
      let mut b = FixtureBuilder::new()
        .with_arch("llama")
        .with_embedding_length(4096)
        .with_chat_template("{{ messages }}")
        .with_kv("split.count", GgufValue::U16(3));
      if tensors {
        b = b.with_tensor("blk.0.attn_q.weight", &[64, 64], 1);
      }
      let m = parse(b.build());
      assert_eq!(
        m.mode_hint,
        ModeHint::Chat,
        "split shard (tensors={tensors}) must not read as an embedding model"
      );
    }
  }

  #[test]
  fn a_split_shard_with_named_templates_and_no_default_reads_as_chat() {
    // gguf-py writes `tokenizer.chat_template` only when the source list has a
    // `default` entry; otherwise the bodies land under
    // `tokenizer.chat_template.<name>` with the names in
    // `tokenizer.chat_templates`, and the singular key is absent entirely.
    // Reading only the singular key called such a model template-less.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_embedding_length(4096)
      .with_kv("split.count", GgufValue::U16(3))
      .with_kv(
        "tokenizer.chat_templates",
        GgufValue::Array(vec![GgufValue::String("tool_use".to_string())]),
      )
      .with_kv(
        "tokenizer.chat_template.tool_use",
        GgufValue::String("{{ messages }}".to_string()),
      )
      .build();
    assert_eq!(parse(bytes).mode_hint, ModeHint::Chat);
  }

  #[test]
  fn mtp_reads_nextn_predict_layers_when_positive() {
    // Embedded MTP head: `{arch}.nextn_predict_layers > 0` ⇒ Some(n).
    let bytes = FixtureBuilder::new()
      .with_arch("qwen35")
      .with_kv("qwen35.nextn_predict_layers", GgufValue::U64(1))
      .build();
    let m = parse(bytes);
    assert_eq!(
      m.mtp,
      Some(1),
      "positive nextn layer count surfaces as Some"
    );
  }

  #[test]
  fn mtp_none_when_key_absent_or_zero() {
    // Absent key ⇒ None.
    let none = parse(FixtureBuilder::new().with_arch("llama").build());
    assert_eq!(none.mtp, None, "no nextn key ⇒ not embedded-MTP-capable");
    // Explicit zero ⇒ None (a non-MTP checkpoint that still writes the key).
    let zero = parse(
      FixtureBuilder::new()
        .with_arch("qwen35")
        .with_kv("qwen35.nextn_predict_layers", GgufValue::U64(0))
        .build(),
    );
    assert_eq!(zero.mtp, None, "zero layers ⇒ not capable");
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
  fn from_file_type_maps_quant_families() {
    use Quant::*;
    // K-quant mixes collapse to their family — this is what fixes a
    // Q4_K_M gemma reading as Q6_K (file_type 15 → Q4_K).
    assert_eq!(Quant::from_file_type(15), Some(Q4_K)); // Q4_K_M
    assert_eq!(Quant::from_file_type(14), Some(Q4_K)); // Q4_K_S
    assert_eq!(Quant::from_file_type(11), Some(Q3_K)); // Q3_K_S
    assert_eq!(Quant::from_file_type(13), Some(Q3_K)); // Q3_K_L
    assert_eq!(Quant::from_file_type(17), Some(Q5_K)); // Q5_K_M
    assert_eq!(Quant::from_file_type(10), Some(Q2_K));
    assert_eq!(Quant::from_file_type(18), Some(Q6_K));
    assert_eq!(Quant::from_file_type(7), Some(Q8_0));
    assert_eq!(Quant::from_file_type(0), Some(F32));
    assert_eq!(Quant::from_file_type(32), Some(BF16));
    // IQ*/TQ*/GUESSED fall back to the tensor scan.
    assert_eq!(Quant::from_file_type(30), None); // IQ4_XS
    assert_eq!(Quant::from_file_type(1024), None); // GUESSED
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
  fn parameter_label_formats_compact_and_accurate() {
    assert_eq!(
      label_for_param_count(6_900_000_000).as_deref(),
      Some("6.9B")
    );
    // Trailing `.0` trims: 8.03B rounds to one decimal then drops it.
    assert_eq!(label_for_param_count(8_030_000_000).as_deref(), Some("8B"));
    assert_eq!(label_for_param_count(7_000_000_000).as_deref(), Some("7B"));
    // Large models keep their real magnitude instead of collapsing.
    assert_eq!(
      label_for_param_count(235_000_000_000).as_deref(),
      Some("235B")
    );
    assert_eq!(
      label_for_param_count(671_000_000_000).as_deref(),
      Some("671B")
    );
    assert_eq!(
      label_for_param_count(1_200_000_000_000).as_deref(),
      Some("1.2T")
    );
    // Small (embedding) models label in millions instead of `?`.
    assert_eq!(label_for_param_count(22_700_000).as_deref(), Some("22.7M"));
    assert_eq!(label_for_param_count(124_000_000).as_deref(), Some("124M"));
    // Only a zero count is unlabelable.
    assert_eq!(label_for_param_count(0), None);
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

  #[test]
  fn from_ggml_tag_maps_known_and_unknown_tags() {
    use Quant::*;
    // Spot-check representative tags across the table, including the
    // boundary between K-quants, IQ-quants, and the integer/float types.
    let known: &[(u32, Quant)] = &[
      (0, F32),
      (1, F16),
      (2, Q4_0),
      (8, Q8_0),
      (12, Q4_K),
      (14, Q6_K),
      (16, IQ2_XXS),
      (23, IQ4_XS),
      (24, I8),
      (28, F64),
      (30, BF16),
      (34, TQ1_0),
      (35, TQ2_0),
    ];
    for (tag, expect) in known {
      assert_eq!(Quant::from_ggml_tag(*tag), *expect, "tag {tag}");
    }
    // A tag outside the table falls through to Unknown carrying the raw tag.
    assert_eq!(Quant::from_ggml_tag(999), Quant::Unknown(999));
  }

  #[test]
  fn block_geometry_is_nonzero_for_every_variant() {
    use Quant::*;
    // Every quant must report a non-zero elements-per-block so the
    // storage estimator never divides by zero. `Unknown` falls back to
    // the conservative (1, 2) default the doc-comment promises.
    let all = [
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
      IQ2_S,
      IQ3_XXS,
      IQ3_S,
      IQ1_S,
      IQ1_M,
      IQ4_NL,
      IQ4_XS,
      TQ1_0,
      TQ2_0,
      I8,
      I16,
      I32,
      I64,
      F64,
      Unknown(7),
    ];
    for q in all {
      let (epb, bpb) = q.block_geometry();
      assert!(epb > 0, "elements-per-block must be > 0 for {}", q.label());
      assert!(bpb > 0, "bytes-per-block must be > 0 for {}", q.label());
    }
    assert_eq!(Quant::Unknown(0).block_geometry(), (1, 2));
  }

  #[test]
  fn tensor_storage_bytes_rounds_rows_up_to_whole_blocks() {
    // Q4_K is 256 elements / 144 bytes per block. A [300, 4] tensor has
    // 4 rows of width 300; each row needs ceil(300/256)=2 blocks, so
    // 4 * 2 * 144 = 1152 bytes (NOT ceil(1200/256)*144, which would
    // undercount the per-row padding).
    let bytes = Quant::Q4_K.tensor_storage_bytes(&[300, 4]);
    assert_eq!(bytes, 1152);
    // Empty dims contribute zero (the `split_first` early-return).
    assert_eq!(Quant::Q4_K.tensor_storage_bytes(&[]), 0);
    // A single-dimension F32 tensor is just width * 4.
    assert_eq!(Quant::F32.tensor_storage_bytes(&[10]), 40);
  }

  #[test]
  fn parameter_count_reads_arch_prefixed_key() {
    // No `general.parameter_count`, but `<arch>.parameter_count` is set —
    // the arch-prefixed fallback must pick it up before the tensor sum.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_kv("llama.parameter_count", GgufValue::U64(7_000_000_000))
      .with_tensor("output.weight", &[10, 10], 1)
      .build();
    let m = parse(bytes);
    assert_eq!(m.total_parameters, Some(7_000_000_000));
  }

  #[test]
  fn embedding_mode_from_arch_embedding_length_without_output() {
    // No output/lm_head/output_norm and no pooling/name signal, but the
    // arch advertises `<arch>.embedding_length` — the encoder fallback
    // classifies it as embedding.
    let bytes = FixtureBuilder::new()
      .with_arch("nomic")
      .with_embedding_length(768)
      .with_tensor("blk.0.attn_q.weight", &[768, 768], 1)
      .build();
    let m = parse(bytes);
    assert_eq!(m.mode_hint, ModeHint::Embedding);
  }

  #[test]
  fn reasoning_hint_ignores_non_string_token_entries() {
    // A tokens array carrying a non-string entry must not panic or
    // false-trigger — the scan skips it and reports no reasoning hint.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_kv(
        "tokenizer.ggml.tokens",
        GgufValue::Array(vec![
          GgufValue::U64(42),
          GgufValue::String("<bos>".to_string()),
        ]),
      )
      .build();
    let m = parse(bytes);
    assert!(!m.reasoning_hint);
  }
}
