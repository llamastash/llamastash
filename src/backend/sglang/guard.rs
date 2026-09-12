//! The unified-memory guard for SGLang.
//!
//! SGLang has no byte-level KV cap. `--mem-fraction-static` is a fraction of
//! the **whole pool** — on a unified host, all of system RAM — and the only
//! deterministic bound is `--max-total-tokens`, denominated in tokens. So the
//! byte budget every fill-the-pool engine gets from
//! [`crate::launch::admission::unified_kv_cache_budget`] has to be divided by
//! the model's KV bytes per token, which means reading its attention geometry
//! out of `config.json`. Read from the SGLang 0.5.18 flag surface (`--help`).

use std::path::Path;

/// Floor on the token pool. Under this the cap is not a usable context, so
/// the launch is refused outright rather than admitted with a pool that
/// rejects every real request after a full weight load.
pub const MIN_POOL_TOKENS: u64 = 2048;

/// The `--max-total-tokens` cap for a unified-memory host, or `None` to
/// **refuse the launch** — the same contract as the byte budget it divides.
pub fn max_total_tokens_cap(
  free_bytes: u64,
  weights_bytes: u64,
  kv_bytes_per_token: u64,
) -> Option<u32> {
  let budget = crate::launch::admission::unified_kv_cache_budget(free_bytes, weights_bytes)?;
  tokens_for_budget(budget, kv_bytes_per_token)
}

/// A byte budget spent in tokens, or `None` to **refuse the launch**: a pool
/// under [`MIN_POOL_TOKENS`], or a per-token cost of zero. Zero is unreadable
/// geometry, not a free model — dividing by anything else would hand the
/// launcher an unbounded cap on the very host the guard exists for.
pub fn tokens_for_budget(budget_bytes: u64, kv_bytes_per_token: u64) -> Option<u32> {
  if kv_bytes_per_token == 0 {
    return None;
  }
  let tokens = budget_bytes / kv_bytes_per_token;
  (tokens >= MIN_POOL_TOKENS).then(|| u32::try_from(tokens).unwrap_or(u32::MAX))
}

/// Bytes of KV cache one token costs across every layer, from the model's
/// `config.json`. `None` when the geometry cannot be read — the caller refuses
/// rather than guessing, because a guess in the wrong direction is the freeze.
pub fn kv_bytes_per_token(model_dir: &Path) -> Option<u64> {
  let text = std::fs::read_to_string(model_dir.join("config.json")).ok()?;
  let json: serde_json::Value = serde_json::from_str(&text).ok()?;
  kv_bytes_per_token_from(&json)
}

/// [`kv_bytes_per_token`] over a parsed `config.json`.
///
/// Every layer is counted as full attention. Hybrid models (sliding-window or
/// Mamba layers) cost less than that, so the figure overestimates and the cap
/// lands smaller than it could — the safe direction. A `--kv-cache-dtype`
/// narrower than the weights, passed through extras, overestimates the same
/// way.
///
/// A zero anywhere in the product is `None` too: a `kv_lora_rank: 0` "not
/// MLA" marker, zero layers, or a head dim that rounds to nothing are
/// unreadable geometry, and a zero cost would turn the byte budget into an
/// unbounded token cap.
pub fn kv_bytes_per_token_from(config: &serde_json::Value) -> Option<u64> {
  // Multimodal repos nest the language model under `text_config`.
  let cfg = if config.get("num_hidden_layers").is_some() {
    config
  } else {
    config.get("text_config")?
  };
  let layers = field(cfg, "num_hidden_layers").filter(|l| *l > 0)?;
  let dtype = ["torch_dtype", "dtype"]
    .iter()
    .find_map(|k| cfg.get(k).or_else(|| config.get(k)))
    .and_then(|v| v.as_str());
  let per_layer = match field(cfg, "kv_lora_rank").filter(|r| *r > 0) {
    // Multi-head latent attention caches one compressed latent per token,
    // shared by K and V, plus the decoupled RoPE key alongside it.
    Some(rank) => rank + field(cfg, "qk_rope_head_dim").unwrap_or(0),
    None => {
      let heads = field(cfg, "num_attention_heads").filter(|h| *h > 0)?;
      let kv_heads = field(cfg, "num_key_value_heads").unwrap_or(heads);
      let head_dim = match field(cfg, "head_dim") {
        Some(d) => d,
        None => field(cfg, "hidden_size")? / heads,
      };
      2 * kv_heads * head_dim
    }
  };
  let bytes = layers * per_layer * dtype_bytes(dtype);
  (bytes > 0).then_some(bytes)
}

fn field(cfg: &serde_json::Value, key: &str) -> Option<u64> {
  cfg.get(key)?.as_u64()
}

/// Bytes per KV element. bf16/fp16 is the common precision and also the
/// fallback for an absent or unknown dtype.
fn dtype_bytes(dtype: Option<&str>) -> u64 {
  match dtype {
    Some("float32" | "float") => 4,
    Some(s) if s.starts_with("float8") => 1,
    _ => 2,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::admission::{DEFAULT_KV_CACHE_BYTES, MIN_KV_CACHE_BYTES};

  const GB: u64 = 1024 * 1024 * 1024;

  /// Qwen2.5-0.5B: 24 layers, 2 KV heads, head_dim 64, bf16.
  fn qwen05b() -> serde_json::Value {
    serde_json::json!({
      "num_hidden_layers": 24,
      "num_attention_heads": 14,
      "num_key_value_heads": 2,
      "hidden_size": 896,
      "torch_dtype": "bfloat16"
    })
  }

  #[test]
  fn gqa_geometry_is_two_kv_heads_times_head_dim_per_layer() {
    // 2 (K+V) * 2 heads * 64 dim * 2 bytes * 24 layers.
    assert_eq!(
      kv_bytes_per_token_from(&qwen05b()),
      Some(2 * 2 * 64 * 2 * 24)
    );
  }

  #[test]
  fn explicit_head_dim_wins_over_the_hidden_size_quotient() {
    let mut c = qwen05b();
    c["head_dim"] = serde_json::json!(128);
    assert_eq!(kv_bytes_per_token_from(&c), Some(2 * 2 * 128 * 2 * 24));
  }

  #[test]
  fn missing_kv_heads_falls_back_to_full_multi_head() {
    let mut c = qwen05b();
    c.as_object_mut().unwrap().remove("num_key_value_heads");
    assert_eq!(kv_bytes_per_token_from(&c), Some(2 * 14 * 64 * 2 * 24));
  }

  #[test]
  fn dtype_scales_the_figure_and_defaults_to_two_bytes() {
    let mut c = qwen05b();
    c["torch_dtype"] = serde_json::json!("float32");
    assert_eq!(kv_bytes_per_token_from(&c), Some(2 * 2 * 64 * 4 * 24));
    c["torch_dtype"] = serde_json::json!("float8_e4m3fn");
    assert_eq!(kv_bytes_per_token_from(&c), Some(2 * 2 * 64 * 24));
    c.as_object_mut().unwrap().remove("torch_dtype");
    assert_eq!(kv_bytes_per_token_from(&c), Some(2 * 2 * 64 * 2 * 24));
  }

  #[test]
  fn multimodal_configs_nest_the_geometry_under_text_config() {
    let c = serde_json::json!({ "model_type": "x-vl", "text_config": qwen05b() });
    assert_eq!(kv_bytes_per_token_from(&c), Some(2 * 2 * 64 * 2 * 24));
  }

  #[test]
  fn latent_attention_caches_the_latent_plus_rope_key_once_per_layer() {
    let c = serde_json::json!({
      "num_hidden_layers": 61,
      "num_attention_heads": 128,
      "kv_lora_rank": 512,
      "qk_rope_head_dim": 64,
      "torch_dtype": "bfloat16"
    });
    assert_eq!(kv_bytes_per_token_from(&c), Some(61 * (512 + 64) * 2));
  }

  #[test]
  fn unreadable_geometry_is_none_not_a_guess() {
    assert_eq!(kv_bytes_per_token_from(&serde_json::json!({})), None);
    assert_eq!(
      kv_bytes_per_token_from(&serde_json::json!({ "num_hidden_layers": 24 })),
      None,
      "layers without heads or dims"
    );
    assert_eq!(
      kv_bytes_per_token_from(&serde_json::json!({
        "num_hidden_layers": 24, "num_attention_heads": 0, "hidden_size": 896
      })),
      None,
      "zero heads must not divide"
    );
    let mut zero_layers = qwen05b();
    zero_layers["num_hidden_layers"] = serde_json::json!(0);
    assert_eq!(kv_bytes_per_token_from(&zero_layers), None, "zero layers");
    let mut zero_kv_heads = qwen05b();
    zero_kv_heads["num_key_value_heads"] = serde_json::json!(0);
    assert_eq!(
      kv_bytes_per_token_from(&zero_kv_heads),
      None,
      "zero KV heads"
    );
  }

  /// `"kv_lora_rank": 0` is a "not MLA" marker some configs carry; it must
  /// fall through to the head geometry, never price the model at zero.
  #[test]
  fn a_zero_latent_rank_falls_through_to_the_head_geometry() {
    let mut c = qwen05b();
    c["kv_lora_rank"] = serde_json::json!(0);
    c["qk_rope_head_dim"] = serde_json::json!(0);
    assert_eq!(kv_bytes_per_token_from(&c), Some(2 * 2 * 64 * 2 * 24));
    let only_rank = serde_json::json!({
      "num_hidden_layers": 61, "kv_lora_rank": 0, "torch_dtype": "bfloat16"
    });
    assert_eq!(
      kv_bytes_per_token_from(&only_rank),
      None,
      "no heads to fall through to"
    );
  }

  #[test]
  fn reads_the_config_json_beside_the_weights() {
    let dir = crate::util::test_temp::unique_temp_dir("sglang-geometry");
    assert_eq!(kv_bytes_per_token(&dir), None, "no config.json yet");
    std::fs::write(dir.join("config.json"), qwen05b().to_string()).unwrap();
    assert_eq!(kv_bytes_per_token(&dir), Some(2 * 2 * 64 * 2 * 24));
    std::fs::write(dir.join("config.json"), "not json").unwrap();
    assert_eq!(kv_bytes_per_token(&dir), None);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// The token cap is the shared byte budget divided by the per-token cost,
  /// and refuses exactly when the budget does.
  #[test]
  fn token_cap_divides_the_shared_budget_and_refuses_with_it() {
    let per_token = 2 * 2 * 64 * 2 * 24;
    // Plenty free: the default budget applies.
    assert_eq!(
      max_total_tokens_cap(113 * GB, GB, per_token),
      Some((DEFAULT_KV_CACHE_BYTES / per_token) as u32)
    );
    // Tight: what is left after weights + reserve.
    assert_eq!(
      max_total_tokens_cap(20 * GB, 8 * GB, per_token),
      Some((4 * GB / per_token) as u32)
    );
    // Under the byte floor: refused, same as the byte budget.
    assert_eq!(max_total_tokens_cap(10 * GB, 8 * GB, per_token), None);
  }

  /// A model whose per-token cost is huge can pass the byte floor and still
  /// yield a pool no request fits in; that is a refusal, not a launch.
  #[test]
  fn token_cap_refuses_a_pool_under_the_token_floor() {
    let too_costly = MIN_KV_CACHE_BYTES / (MIN_POOL_TOKENS - 1);
    assert_eq!(
      max_total_tokens_cap(8 * GB + MIN_KV_CACHE_BYTES, 0, too_costly),
      None
    );
    let just_fits = MIN_KV_CACHE_BYTES / MIN_POOL_TOKENS;
    assert_eq!(
      max_total_tokens_cap(8 * GB + MIN_KV_CACHE_BYTES, 0, just_fits),
      Some(MIN_POOL_TOKENS as u32)
    );
  }

  /// A zero per-token cost is unreadable geometry. Dividing by one instead
  /// would return the whole budget as tokens, which the launcher takes as an
  /// unbounded pool on the very host the guard protects.
  #[test]
  fn a_zero_per_token_cost_refuses_rather_than_uncapping() {
    assert_eq!(max_total_tokens_cap(113 * GB, GB, 0), None);
    assert_eq!(tokens_for_budget(DEFAULT_KV_CACHE_BYTES, 0), None);
  }

  /// The unsampled path spends the default budget through the same floor.
  #[test]
  fn the_default_budget_spent_in_tokens_keeps_the_floor() {
    let too_costly = DEFAULT_KV_CACHE_BYTES / (MIN_POOL_TOKENS - 1);
    assert_eq!(tokens_for_budget(DEFAULT_KV_CACHE_BYTES, too_costly), None);
    let just_fits = DEFAULT_KV_CACHE_BYTES / MIN_POOL_TOKENS;
    assert_eq!(
      tokens_for_budget(DEFAULT_KV_CACHE_BYTES, just_fits),
      Some(MIN_POOL_TOKENS as u32)
    );
    assert_eq!(
      tokens_for_budget(DEFAULT_KV_CACHE_BYTES, 32),
      Some((DEFAULT_KV_CACHE_BYTES / 32) as u32)
    );
  }
}
