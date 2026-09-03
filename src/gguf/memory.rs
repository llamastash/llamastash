//! Memory estimator for a launched model.
//!
//! Splits cost into **weights** (the GGUF tensors themselves) and **KV
//! cache** (which scales with context length and cache dtype). Each split
//! is further attributed to RAM vs VRAM based on `n_gpu_layers` (the
//! `-ngl` flag in `llama-server`).
//!
//! Estimates are intentionally approximate. The plan calls out ~10% as
//! acceptable; the dominant errors are (a) per-block padding inside ggml
//! (small), (b) inference-time scratch buffers we don't model, and (c)
//! advanced KV quantisation modes where the byte-per-element factor
//! changes. Consumers should display these as "estimate" not "exact".

use crate::backend::Backend;
use crate::gguf::header::{GgufHeader, GgufValue};
use crate::gguf::metadata::Quant;

/// Generate the [`CacheType`] enum, its name parser, its byte-cost
/// table, and the [`KV_CACHE_TYPES`] name list from one row-per-type
/// table so the launch-time validation surface and the KV memory
/// estimator can never disagree on which types are standard.
macro_rules! cache_types {
  ( default = $default:ident, $( $variant:ident => $name:literal : $bytes:expr ),+ $(,)? ) => {
    /// What KV cache dtype `llama-server` is launched with. Default f16.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[allow(non_camel_case_types)]
    pub enum CacheType { $( $variant, )+ }

    impl Default for CacheType {
      fn default() -> Self { CacheType::$default }
    }

    impl CacheType {
      /// Bytes per stored KV element under this cache dtype.
      pub fn bytes_per_elem(&self) -> f64 {
        match self { $( CacheType::$variant => $bytes, )+ }
      }

      /// Parse a `--cache-type-k/v` value (case-insensitive) into a
      /// known [`CacheType`]; `None` for anything outside the standard
      /// set, including build-specific custom types.
      pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
          $( $name => Some(CacheType::$variant), )+
          _ => None,
        }
      }
    }

    /// Standard llama-server cache types accepted by `--cache-type-k/v`
    /// and cycled in the TUI launch picker. Values outside this list still
    /// pass validation when they look like a custom quant identifier (see
    /// `crate::launch::knobs::Shape::Identifier`), so modified llama-server
    /// builds are not blocked at this layer.
    pub const KV_CACHE_TYPES: &[&str] = &[ $( $name, )+ ];
  };
}

// One row per type as `Variant => "flag-spelling" : bytes_per_element`,
// in llama-server's own `--cache-type-k` listing order (verified against
// b9245 `--help`). IQ4_NL shares Q4_0's 18-bytes-per-32-elements block
// layout. Adding a type here updates the enum, the parser, the cost
// table, and KV_CACHE_TYPES together.
cache_types! {
  default = F16,
  F32    => "f32"    : 4.0,
  F16    => "f16"    : 2.0,
  BF16   => "bf16"   : 2.0,
  Q8_0   => "q8_0"   : 34.0 / 32.0, // 1.0625
  Q4_0   => "q4_0"   : 18.0 / 32.0, // 0.5625
  Q4_1   => "q4_1"   : 20.0 / 32.0, // 0.625
  IQ4_NL => "iq4_nl" : 18.0 / 32.0, // 0.5625
  Q5_0   => "q5_0"   : 22.0 / 32.0, // 0.6875
  Q5_1   => "q5_1"   : 24.0 / 32.0, // 0.75
}

/// Parse a `--cache-type-{k,v}` tag (`q8_0`, `f16`, …) into a
/// [`CacheType`], defaulting to `f16` when absent or unrecognised.
/// Shared by the admission KV projection so it uses the same dtype
/// mapping the launch argv will.
pub fn parse_cache_type(raw: Option<&str>) -> CacheType {
  raw.and_then(CacheType::parse).unwrap_or_default()
}

/// Inputs to the estimator other than the parsed header itself.
#[derive(Debug, Clone, Copy)]
pub struct EstimateOptions {
  pub ctx_len: u64,
  pub cache_type_k: CacheType,
  pub cache_type_v: CacheType,
  /// Number of transformer blocks offloaded to GPU (`-ngl`). `None` means
  /// CPU only; `Some(usize::MAX)` means "all on GPU".
  pub n_gpu_layers: Option<u32>,
}

impl Default for EstimateOptions {
  fn default() -> Self {
    EstimateOptions {
      ctx_len: 4096,
      cache_type_k: CacheType::F16,
      cache_type_v: CacheType::F16,
      n_gpu_layers: None,
    }
  }
}

/// Output of the estimator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryEstimate {
  pub weights_ram: u64,
  pub weights_vram: u64,
  pub kv_cache_ram: u64,
  pub kv_cache_vram: u64,
}

impl MemoryEstimate {
  pub fn total_ram(&self) -> u64 {
    self.weights_ram.saturating_add(self.kv_cache_ram)
  }

  pub fn total_vram(&self) -> u64 {
    self.weights_vram.saturating_add(self.kv_cache_vram)
  }
}

/// Estimate the launch-time memory footprint for the supplied GGUF header
/// and launch parameters.
pub fn estimate(header: &GgufHeader, opts: EstimateOptions) -> MemoryEstimate {
  let arch = header.string(&["general.architecture"]).map(str::to_string);
  let arch_key = arch.as_deref();

  let weights_total = weights_bytes(header);

  let n_layers = arch_key
    .and_then(|a| header.u64(&[format!("{a}.block_count")]))
    .unwrap_or(0);
  let gpu_fraction = gpu_fraction(opts.n_gpu_layers, n_layers);
  let weights_vram = (weights_total as f64 * gpu_fraction) as u64;
  let weights_ram = weights_total.saturating_sub(weights_vram);

  let kv_total = kv_bytes(header, arch_key, opts);
  let kv_vram = (kv_total as f64 * gpu_fraction) as u64;
  let kv_ram = kv_total.saturating_sub(kv_vram);

  MemoryEstimate {
    weights_ram,
    weights_vram,
    kv_cache_ram: kv_ram,
    kv_cache_vram: kv_vram,
  }
}

fn gpu_fraction(n_gpu_layers: Option<u32>, n_layers: u64) -> f64 {
  let n = match n_gpu_layers {
    None | Some(0) => return 0.0,
    Some(n) => n,
  };
  // Unknown layer count — if user asked for any GPU layers, assume "all".
  if n_layers == 0 {
    return 1.0;
  }
  (n as f64 / n_layers as f64).clamp(0.0, 1.0)
}

/// Sum of all tensor weight bytes, using each tensor's own ggml-type for
/// per-block geometry. Falls back to 0 for tensors with no elements.
pub fn weights_bytes(header: &GgufHeader) -> u64 {
  header
    .tensors
    .iter()
    .map(|t| Quant::from_ggml_tag(t.ggml_type).tensor_storage_bytes(&t.dims))
    .fold(0u64, u64::saturating_add)
}

/// Per-layer embedding tables that llama.cpp marks `TENSOR_READ_LAZY` and
/// reads row-by-row from the mapping instead of keeping resident.
///
/// Kept as a name list rather than an arch list because the flag travels with
/// the tensor: `qwen4exp` and `gemma4` both use this one, and a new arch
/// reusing the same table gets the same treatment for free.
const LAZY_TENSOR_NAMES: &[&str] = &["per_layer_token_embd.weight"];

/// llama.cpp's `--lazy-mode auto` threshold: only lazy-flagged tensors larger
/// than this stream, smaller ones stay resident (`llama-model-loader.cpp`).
const AUTO_LAZY_MIN_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Per-tensor storage bytes of every lazy-flagged tensor in this header.
///
/// Kept as a list rather than a single sum because the `--lazy-mode auto`
/// threshold is a per-tensor test, and the launch does not know which mode it
/// will run in until it reads its extras. Carried on
/// [`ModelMetadata::lazy_tensor_bytes`](crate::gguf::metadata::ModelMetadata)
/// so the admission path applies the threshold without re-reading headers.
/// Names are not kept: nothing displays them, and the flag is decided by
/// `LAZY_TENSOR_NAMES` at parse time.
pub fn lazy_tensor_sizes(header: &GgufHeader) -> Vec<u64> {
  header
    .tensors
    .iter()
    .filter(|t| LAZY_TENSOR_NAMES.contains(&t.name.as_str()))
    .map(|t| Quant::from_ggml_tag(t.ggml_type).tensor_storage_bytes(&t.dims))
    .collect()
}

/// Bytes these lazy-flagged tensors stream from disk rather than occupy in
/// memory, under llama.cpp's default `--lazy-mode auto`.
///
/// Subtract from a weight total before projecting a launch's demand: counting
/// them refuses launches that comfortably fit. Qwen3.8-Flash-Next carries a
/// 26.8 GiB `per_layer_token_embd`, so a 103.7 GiB file needs ~77 GiB resident.
///
/// `all_sizes` mirrors `--lazy-mode on`, which drops the size threshold that
/// `auto` applies.
///
/// Independent of mmap. Verified at llama.cpp `e750b887a`: `init_mappings`
/// maps when `use_mmap || lazy.any()`, per-tensor reads take
/// `use_mmap || lazy.has(cur)`, and lazy tensors are skipped by `mlock`. With
/// mmap off the *non-lazy* tensors are read into RAM instead of mapped, so
/// residency is still total minus these bytes. Only `--lazy-mode off` changes
/// that.
pub fn streamed_bytes(sizes: &[u64], all_sizes: bool) -> u64 {
  sizes
    .iter()
    .copied()
    .filter(|b| all_sizes || *b > AUTO_LAZY_MIN_BYTES)
    .fold(0u64, u64::saturating_add)
}

/// Closed-form KV cache bytes:
/// `2 (K+V) * n_layers * n_kv_heads * head_dim * ctx_len * bpe(cache_type)`,
/// but with separate K and V terms because llama-server lets the two be set
/// independently via `--cache-type-k` / `--cache-type-v`.
pub fn kv_bytes(header: &GgufHeader, arch: Option<&str>, opts: EstimateOptions) -> u64 {
  let Some(a) = arch else { return 0 };
  // A backend may model a header's KV cache better than the generic GQA/MLA
  // math (some compressed caches over-count heavily on the naive path). The
  // backend registry owns the header→model mapping and keys on the header
  // itself, so the right figure applies even when the model falls back to a
  // different backend — and this path names no backend.
  if let Some(bytes) = crate::backend::Backends::all()
    .iter()
    .find_map(|b| b.kv_bytes(header, Some(a), opts.ctx_len))
  {
    return bytes;
  }
  // MLA (deepseek2, kimi-k2): caches one compressed latent per token per
  // layer, not per-head K/V — the standard formula over-estimates ~10x.
  if let Some(mla) = mla_kv_elements(header, a) {
    let elements = mla.saturating_mul(opts.ctx_len as u128);
    // MLA stores a single latent (not separate K and V); size it with
    // the K cache dtype.
    return scale_bytes(elements, opts.cache_type_k.bytes_per_elem());
  }
  let Some(geom) = attention_geometry(header, a) else {
    return 0;
  };
  // Sum KV elements per layer. Each layer can differ in KV-head count
  // (per-layer `head_count_kv` array), head dim (sliding layers use the
  // `*_swa` length), and effective context (sliding layers cap at the
  // window). This is what makes the estimate match reality on gemma /
  // gpt-oss instead of over-counting every layer as full attention.
  let mut elements: u128 = 0;
  for layer in 0..geom.n_layers {
    let is_swa = geom.swa_window.is_some() && geom.swa_layer(layer);
    let kv_heads = geom.kv_heads(layer);
    let head_dim = if is_swa {
      geom.head_dim_swa
    } else {
      geom.head_dim
    };
    let ctx = if is_swa {
      opts.ctx_len.min(geom.swa_window.unwrap())
    } else {
      opts.ctx_len
    };
    elements = elements.saturating_add(
      (kv_heads as u128)
        .saturating_mul(head_dim as u128)
        .saturating_mul(ctx as u128),
    );
  }
  let bpe = opts.cache_type_k.bytes_per_elem() + opts.cache_type_v.bytes_per_elem();
  scale_bytes(elements, bpe)
}

fn scale_bytes(elements: u128, bytes_per_elem: f64) -> u64 {
  let total = (elements as f64) * bytes_per_elem;
  if total.is_finite() && total >= 0.0 {
    total as u64
  } else {
    0
  }
}

/// MLA KV elements per token across all layers, or `None` when the model
/// is not MLA. DeepSeek-V2/V3 (`deepseek2`) and Kimi K2 cache a single
/// compressed latent of `kv_lora_rank` plus the rope key of
/// `rope.dimension_count` per token per layer, so the cache is
/// `n_layers * (kv_lora_rank + rope_dim)` elements — an order of
/// magnitude under the per-head MHA/GQA figure.
fn mla_kv_elements(header: &GgufHeader, arch: &str) -> Option<u128> {
  let kv_lora_rank = header.u64(&[format!("{arch}.attention.kv_lora_rank")])?;
  let n_layers = header.u64(&[format!("{arch}.block_count")])?;
  let rope_dim = header
    .u64(&[format!("{arch}.rope.dimension_count")])
    .unwrap_or(0);
  Some((n_layers as u128).saturating_mul((kv_lora_rank + rope_dim) as u128))
}

/// Per-layer attention geometry for the KV estimate.
struct AttnGeometry {
  n_layers: u64,
  /// Full-attention head dim (`key_length`, else `embedding_length /
  /// head_count`).
  head_dim: u64,
  /// Sliding-window head dim (`key_length_swa`); equals `head_dim` when
  /// the model doesn't distinguish.
  head_dim_swa: u64,
  /// Sliding-window size in tokens, when the arch has one.
  swa_window: Option<u64>,
  /// Per-layer KV-head counts, length `n_layers`.
  kv_heads: Vec<u64>,
  /// Per-layer sliding flag (`1` = sliding-window, `0` = full); empty
  /// when the arch has no per-layer pattern.
  swa_pattern: Vec<u64>,
}

impl AttnGeometry {
  fn kv_heads(&self, layer: u64) -> u64 {
    self.kv_heads.get(layer as usize).copied().unwrap_or(0)
  }
  /// Whether `layer` is a sliding-window layer. With no per-layer pattern
  /// but a window present, treat every layer as sliding.
  fn swa_layer(&self, layer: u64) -> bool {
    match self.swa_pattern.get(layer as usize) {
      Some(flag) => *flag == 1,
      None => self.swa_pattern.is_empty(),
    }
  }
}

/// Read the per-layer attention geometry. `head_count_kv` is read
/// per-layer: gemma3 / gemma4 store it as an **array** (full vs
/// sliding-window layers carry different KV-head counts); a scalar
/// broadcasts to every layer; absent falls back to MHA (`head_count`).
/// Sliding-window archs additionally expose `sliding_window` +
/// `sliding_window_pattern` (1 = sliding) and a smaller `*_swa` head dim,
/// which cap those layers' KV at the window.
fn attention_geometry(header: &GgufHeader, arch: &str) -> Option<AttnGeometry> {
  let a = arch;
  let n_layers = header.u64(&[format!("{a}.block_count")])?;
  let n_heads = header.u64(&[format!("{a}.attention.head_count")])?;
  let head_dim = match header.u64(&[format!("{a}.attention.key_length")]) {
    Some(k) => k,
    None => {
      let embed = header.u64(&[format!("{a}.embedding_length")])?;
      if n_heads == 0 {
        return None;
      }
      embed / n_heads
    }
  };
  let head_dim_swa = header
    .u64(&[format!("{a}.attention.key_length_swa")])
    .unwrap_or(head_dim);
  let swa_window = header.u64(&[format!("{a}.attention.sliding_window")]);
  let kv_heads = per_layer_u64(
    header,
    &format!("{a}.attention.head_count_kv"),
    n_layers,
    n_heads,
  );
  let swa_pattern =
    per_layer_u64_no_default(header, &format!("{a}.attention.sliding_window_pattern"));
  Some(AttnGeometry {
    n_layers,
    head_dim,
    head_dim_swa,
    swa_window,
    kv_heads,
    swa_pattern,
  })
}

/// A metadata value that may be a per-layer array or a single scalar,
/// broadcast to `n_layers` entries (`default` fills gaps / absence).
fn per_layer_u64(header: &GgufHeader, key: &str, n_layers: u64, default: u64) -> Vec<u64> {
  match header.get(key) {
    Some(GgufValue::Array(items)) => (0..n_layers as usize)
      .map(|i| items.get(i).and_then(GgufValue::as_u64).unwrap_or(default))
      .collect(),
    Some(v) => vec![v.as_u64().unwrap_or(default); n_layers as usize],
    None => vec![default; n_layers as usize],
  }
}

/// Like [`per_layer_u64`] but returns an empty vec when the key is
/// absent (so callers can distinguish "no pattern" from "all zero").
fn per_layer_u64_no_default(header: &GgufHeader, key: &str) -> Vec<u64> {
  match header.get(key) {
    Some(GgufValue::Array(items)) => items.iter().filter_map(GgufValue::as_u64).collect(),
    Some(v) => v.as_u64().map(|n| vec![n]).unwrap_or_default(),
    None => Vec::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gguf::header::{read_reader, HeaderReadOptions};
  use crate::gguf::test_fixtures::FixtureBuilder;
  use std::io::Cursor as IoCursor;

  fn parse(bytes: Vec<u8>) -> GgufHeader {
    read_reader(IoCursor::new(bytes), HeaderReadOptions::default())
      .unwrap()
      .header
  }

  #[test]
  fn kv_bytes_matches_closed_form() {
    // n_layers=32, n_kv_heads=8, head_dim=128, ctx=8192, f16 K and f16 V.
    // 2 (K+V) * 32 * 8 * 128 * 8192 * 2 = 2 * 32 * 8 * 128 * 8192 * 2
    //   = (32*8*128) * 8192 * 4 = 32768 * 8192 * 4 = 1_073_741_824 bytes (~1 GiB)
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_block_count(32)
      .with_head_count(32)
      .with_head_count_kv(8)
      .with_embedding_length(32 * 128) // → head_dim = 128
      .build();
    let h = parse(bytes);
    let opts = EstimateOptions {
      ctx_len: 8192,
      ..EstimateOptions::default()
    };
    let kv = kv_bytes(&h, Some("llama"), opts);
    let expected: u64 = 2 * 32 * 8 * 128 * 8192 * 2;
    assert_eq!(kv, expected);
  }

  #[test]
  fn kv_bytes_sums_per_layer_kv_head_array() {
    // gemma3 / gemma4 store `head_count_kv` as a per-layer array (full
    // vs sliding-window layers differ). KV must sum the array, not fall
    // back to full MHA (head_count) for every layer.
    let kv_per_layer = [16i32, 16, 16, 16, 16, 4]; // sum = 84
    let bytes = FixtureBuilder::new()
      .with_arch("gemma4")
      .with_block_count(6)
      .with_head_count(32)
      .with_kv(
        "gemma4.attention.head_count_kv",
        GgufValue::Array(kv_per_layer.iter().map(|n| GgufValue::I32(*n)).collect()),
      )
      .with_embedding_length(32 * 128) // → head_dim = 128
      .build();
    let h = parse(bytes);
    let opts = EstimateOptions {
      ctx_len: 1000,
      ..EstimateOptions::default()
    };
    let kv = kv_bytes(&h, Some("gemma4"), opts);
    // total_kv_heads = sum(array) = 84, NOT n_layers*n_heads = 6*32 = 192.
    let expected: u64 = 2 * 84 * 128 * 1000 * 2;
    assert_eq!(kv, expected);
    let naive_mha: u64 = 2 * (6 * 32) * 128 * 1000 * 2;
    assert!(kv < naive_mha, "array sum must undercut the MHA fallback");
  }

  #[test]
  fn kv_bytes_models_deepseek4_compressed_cache() {
    // ds4 keeps, per layer, a raw window (4096 rows) plus a compressed cache
    // of ctx/ratio rows (ratio 0 = raw only); each row is key_length=512 F32
    // latents. Toy 4-layer model, ratios [0, 4, 128, 0], ctx=8192.
    let ratios = [0i32, 4, 128, 0];
    let bytes = FixtureBuilder::new()
      .with_arch("deepseek4")
      .with_block_count(4)
      .with_head_count(64)
      .with_kv("deepseek4.attention.key_length", GgufValue::U32(512))
      .with_kv(
        "deepseek4.attention.compress_ratios",
        GgufValue::Array(ratios.iter().map(|n| GgufValue::I32(*n)).collect()),
      )
      .build();
    let h = parse(bytes);
    let kv = kv_bytes(
      &h,
      Some("deepseek4"),
      EstimateOptions {
        ctx_len: 8192,
        ..EstimateOptions::default()
      },
    );
    // rows = 4×raw(4096) + 8192/4 + 8192/128 = 16384 + 2048 + 64 = 18496.
    let expected = (4 * 4096 + 8192 / 4 + 8192 / 128) * 512 * 4;
    assert_eq!(kv, expected);
    // Well under the naive GQA figure the generic path would produce
    // (head_count_kv=1 × key_length=512 × full ctx), which ignores the
    // sequence compression — the whole reason for the dedicated branch.
    let naive: u64 = 4 * 512 * 8192 * 4;
    assert!(
      kv < naive,
      "compression must undercut naive GQA: {kv} vs {naive}"
    );
  }

  #[test]
  fn kv_bytes_caps_sliding_window_layers() {
    // gemma-style: sliding layers (pattern=1) use the smaller `*_swa`
    // head dim and cap their context at the window; full layers
    // (pattern=0) use the full head dim and full context.
    let bytes = FixtureBuilder::new()
      .with_arch("gemma4")
      .with_block_count(3)
      .with_head_count(32)
      .with_kv(
        "gemma4.attention.head_count_kv",
        GgufValue::Array(vec![
          GgufValue::I32(16),
          GgufValue::I32(16),
          GgufValue::I32(4),
        ]),
      )
      .with_kv("gemma4.attention.key_length", GgufValue::U32(512))
      .with_kv("gemma4.attention.key_length_swa", GgufValue::U32(256))
      .with_kv("gemma4.attention.sliding_window", GgufValue::U32(1024))
      .with_kv(
        "gemma4.attention.sliding_window_pattern",
        GgufValue::Array(vec![
          GgufValue::I32(1),
          GgufValue::I32(1),
          GgufValue::I32(0),
        ]),
      )
      .build();
    let h = parse(bytes);
    let opts = EstimateOptions {
      ctx_len: 8192,
      ..EstimateOptions::default()
    };
    let kv = kv_bytes(&h, Some("gemma4"), opts);
    // swa layers: 16*256*min(8192,1024)=1024 each; full layer: 4*512*8192.
    let elems: u64 = 16 * 256 * 1024 + 16 * 256 * 1024 + 4 * 512 * 8192;
    assert_eq!(kv, elems * 4); // f16 K + f16 V = 4 bytes/elem
                               // Far under the no-sliding estimate (every layer full ctx + 512 dim).
    let naive: u64 = (16 + 16 + 4) * 512 * 8192 * 4;
    assert!(
      kv < naive / 4,
      "sliding cap must slash KV: kv={kv} naive={naive}"
    );
  }

  #[test]
  fn kv_bytes_uses_mla_latent_for_deepseek() {
    // deepseek2 / kimi: KV is one compressed latent per token per layer
    // (`kv_lora_rank + rope_dim`), not per-head K/V.
    let bytes = FixtureBuilder::new()
      .with_arch("deepseek2")
      .with_block_count(4)
      .with_head_count(128)
      .with_kv("deepseek2.attention.kv_lora_rank", GgufValue::U32(512))
      .with_kv("deepseek2.rope.dimension_count", GgufValue::U32(64))
      .with_kv("deepseek2.attention.key_length", GgufValue::U32(192))
      .build();
    let h = parse(bytes);
    let opts = EstimateOptions {
      ctx_len: 8192,
      ..EstimateOptions::default()
    };
    let kv = kv_bytes(&h, Some("deepseek2"), opts);
    // 4 layers * (512 + 64) * 8192 * 2 bytes (single latent, f16).
    let expected: u64 = 4 * (512 + 64) * 8192 * 2;
    assert_eq!(kv, expected);
    // Far under what a per-head GQA reading would give for 128 heads.
    let as_gqa: u64 = 4 * 128 * 192 * 8192 * 4;
    assert!(kv * 10 < as_gqa, "MLA must be ~10x under per-head: kv={kv}");
  }

  #[test]
  fn kv_bytes_falls_back_to_mha_when_kv_heads_absent() {
    // No head_count_kv at all → assume full MHA (head_count per layer).
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_block_count(4)
      .with_head_count(16)
      .with_embedding_length(16 * 128)
      .build();
    let h = parse(bytes);
    let opts = EstimateOptions {
      ctx_len: 512,
      ..EstimateOptions::default()
    };
    let kv = kv_bytes(&h, Some("llama"), opts);
    let expected: u64 = 2 * (4 * 16) * 128 * 512 * 2;
    assert_eq!(kv, expected);
  }

  #[test]
  fn weights_bytes_sums_tensors() {
    // Two tensors at F16 (2 bytes/elem): 1000 + 2000 elems → 6000 bytes.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_tensor("output.weight", &[1000], 1)
      .with_tensor("blk.0.attn_q.weight", &[2000], 1)
      .build();
    let h = parse(bytes);
    assert_eq!(weights_bytes(&h), 1000 * 2 + 2000 * 2);
  }

  #[test]
  fn lazy_tensor_sizes_picks_only_the_flagged_tensor() {
    // F16, 2 bytes/elem. Only `per_layer_token_embd.weight` is lazy-flagged;
    // the token embedding beside it is an ordinary resident tensor with a
    // confusingly similar name.
    let bytes = FixtureBuilder::new()
      .with_arch("gemma4")
      .with_tensor("token_embd.weight", &[1000], 1)
      .with_tensor("per_layer_token_embd.weight", &[3000], 1)
      .build();
    let h = parse(bytes);
    assert_eq!(lazy_tensor_sizes(&h), vec![3000 * 2]);
    assert_eq!(
      weights_bytes(&h),
      1000 * 2 + 3000 * 2,
      "the total keeps both"
    );
  }

  #[test]
  fn lazy_tensor_sizes_is_empty_for_an_ordinary_model() {
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_tensor("token_embd.weight", &[1000], 1)
      .build();
    assert!(lazy_tensor_sizes(&parse(bytes)).is_empty());
  }

  #[test]
  fn streamed_bytes_applies_the_auto_threshold_per_tensor() {
    let small = AUTO_LAZY_MIN_BYTES - 1;
    let big = AUTO_LAZY_MIN_BYTES + 1;
    // `auto` (the engine default) streams only what is over the threshold...
    assert_eq!(streamed_bytes(&[small, big], false), big);
    // ...`on` drops the threshold and streams every flagged tensor.
    assert_eq!(streamed_bytes(&[small, big], true), small + big);
    // Exactly at the threshold stays resident: llama.cpp's test is `>`.
    assert_eq!(streamed_bytes(&[AUTO_LAZY_MIN_BYTES], false), 0);
    assert_eq!(streamed_bytes(&[], false), 0);
  }

  #[test]
  fn weights_bytes_applies_q4k_geometry() {
    // Q4_K: 256 elements per 144 bytes. Tensor of 256*4 = 1024 elems → 576 bytes.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_tensor("blk.0.ffn_up.weight", &[1024], 12)
      .build();
    let h = parse(bytes);
    assert_eq!(weights_bytes(&h), 4 * 144);
  }

  #[test]
  fn weights_bytes_rounds_each_quantized_row() {
    // Q4_K rows of width 1 still occupy one 256-element block per row.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_tensor("tiny.rows.weight", &[1, 3], 12)
      .build();
    let h = parse(bytes);
    assert_eq!(weights_bytes(&h), 3 * 144);
  }

  #[test]
  fn estimate_splits_ram_and_vram_by_ngl() {
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_block_count(32)
      .with_head_count(8)
      .with_embedding_length(8 * 64)
      .with_tensor("output.weight", &[1024], 1) // 2 KiB at F16
      .build();
    let h = parse(bytes);
    let opts = EstimateOptions {
      ctx_len: 1024,
      n_gpu_layers: Some(16), // half of 32 layers
      ..EstimateOptions::default()
    };
    let est = estimate(&h, opts);
    // gpu_fraction = 0.5 → ram == vram
    assert_eq!(est.weights_ram, est.weights_vram);
    assert!(est.kv_cache_vram > 0);
    assert_eq!(est.kv_cache_ram, est.kv_cache_vram);
  }

  #[test]
  fn estimate_returns_zero_kv_when_arch_geometry_missing() {
    let bytes = FixtureBuilder::new()
      .with_arch("mystery")
      .with_tensor("some.weight", &[64], 1)
      .build();
    let h = parse(bytes);
    let est = estimate(&h, EstimateOptions::default());
    assert_eq!(est.kv_cache_ram, 0);
    assert_eq!(est.kv_cache_vram, 0);
    assert!(est.weights_ram > 0);
  }

  #[test]
  fn cache_type_parse_accepts_lowercase_and_uppercase() {
    assert_eq!(CacheType::parse("Q8_0"), Some(CacheType::Q8_0));
    assert_eq!(CacheType::parse("f16"), Some(CacheType::F16));
    // iq4_nl is part of the standard cycle set; it must estimate at
    // Q4_0's cost, not silently fall back to f16.
    assert_eq!(CacheType::parse("iq4_nl"), Some(CacheType::IQ4_NL));
    assert_eq!(
      CacheType::IQ4_NL.bytes_per_elem(),
      CacheType::Q4_0.bytes_per_elem()
    );
    // Custom build types still fall through to the f16 default.
    assert_eq!(CacheType::parse("nonsense"), None);
  }

  #[test]
  fn every_advertised_cache_type_has_a_cost() {
    // KV_CACHE_TYPES and the cost table are generated from one macro
    // table, so this just documents the contract: every advertised name
    // round-trips through parse and has a real per-element cost.
    assert!(!KV_CACHE_TYPES.is_empty());
    for name in KV_CACHE_TYPES {
      let ct =
        CacheType::parse(name).unwrap_or_else(|| panic!("no CacheType for advertised {name}"));
      assert!(ct.bytes_per_elem() > 0.0, "{name} has a non-positive cost");
    }
  }
}
