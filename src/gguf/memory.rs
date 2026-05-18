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

use crate::gguf::header::GgufHeader;
use crate::gguf::metadata::Quant;

/// What KV cache dtype `llama-server` is launched with. Default f16.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(non_camel_case_types)]
pub enum CacheType {
  F32,
  #[default]
  F16,
  BF16,
  Q8_0,
  Q5_1,
  Q5_0,
  Q4_1,
  Q4_0,
}

impl CacheType {
  /// Bytes per stored KV element under this cache dtype.
  pub fn bytes_per_elem(&self) -> f64 {
    match self {
      CacheType::F32 => 4.0,
      CacheType::F16 | CacheType::BF16 => 2.0,
      CacheType::Q8_0 => 34.0 / 32.0, // 1.0625
      CacheType::Q5_1 => 24.0 / 32.0, // 0.75
      CacheType::Q5_0 => 22.0 / 32.0, // 0.6875
      CacheType::Q4_1 => 20.0 / 32.0, // 0.625
      CacheType::Q4_0 => 18.0 / 32.0, // 0.5625
    }
  }

  /// Parse the `--cache-type-k/v` flag value as llama-server accepts it.
  pub fn parse(s: &str) -> Option<Self> {
    Some(match s.to_ascii_lowercase().as_str() {
      "f32" => CacheType::F32,
      "f16" => CacheType::F16,
      "bf16" => CacheType::BF16,
      "q8_0" => CacheType::Q8_0,
      "q5_1" => CacheType::Q5_1,
      "q5_0" => CacheType::Q5_0,
      "q4_1" => CacheType::Q4_1,
      "q4_0" => CacheType::Q4_0,
      _ => return None,
    })
  }
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
  let arch = header
    .string(&["general.architecture"])
    .map(str::to_string);
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

/// Closed-form KV cache bytes:
/// `2 (K+V) * n_layers * n_kv_heads * head_dim * ctx_len * bpe(cache_type)`,
/// but with separate K and V terms because llama-server lets the two be set
/// independently via `--cache-type-k` / `--cache-type-v`.
pub fn kv_bytes(header: &GgufHeader, arch: Option<&str>, opts: EstimateOptions) -> u64 {
  let (n_layers, n_kv_heads, head_dim) = match attention_geometry(header, arch) {
    Some(v) => v,
    None => return 0,
  };
  let elements_per_dtype = (n_layers as u128)
    .saturating_mul(n_kv_heads as u128)
    .saturating_mul(head_dim as u128)
    .saturating_mul(opts.ctx_len as u128);
  let k_bytes = (elements_per_dtype as f64) * opts.cache_type_k.bytes_per_elem();
  let v_bytes = (elements_per_dtype as f64) * opts.cache_type_v.bytes_per_elem();
  let total = k_bytes + v_bytes;
  if total.is_finite() && total >= 0.0 {
    total as u64
  } else {
    0
  }
}

/// (n_layers, n_kv_heads, head_dim) derived from the GGUF metadata, or
/// `None` if any required field is missing.
fn attention_geometry(header: &GgufHeader, arch: Option<&str>) -> Option<(u64, u64, u64)> {
  let a = arch?;
  let n_layers = header.u64(&[format!("{a}.block_count")])?;
  let n_heads = header.u64(&[format!("{a}.attention.head_count")])?;
  let n_kv_heads = header
    .u64(&[format!("{a}.attention.head_count_kv")])
    .unwrap_or(n_heads);
  // head_dim: explicit `attention.key_length` if present, else
  // `embedding_length / head_count`.
  let head_dim = if let Some(k) = header.u64(&[format!("{a}.attention.key_length")]) {
    k
  } else {
    let embed = header.u64(&[format!("{a}.embedding_length")])?;
    if n_heads == 0 {
      return None;
    }
    embed / n_heads
  };
  Some((n_layers, n_kv_heads, head_dim))
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
    assert_eq!(CacheType::parse("nonsense"), None);
  }
}
