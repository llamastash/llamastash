//! End-to-end integration tests for the GGUF layer (Unit 3).
//!
//! Each test synthesises a GGUF byte sequence via the in-crate fixture
//! builder, writes it to a temp file, and drives `read_path` / `summarise`
//! / `compute_identity` / `estimate_memory` as the daemon's discovery and
//! supervisor consumers will.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use llamastash::gguf::test_fixtures::{build_minimal_gguf, FixtureBuilder};
use llamastash::gguf::{
  compute_identity, estimate_memory, read_path, summarise_metadata, EstimateOptions, GgufError,
  GgufValue, HeaderReadOptions, ModeHint, Quant,
};

#[test]
fn happy_path_parses_arch_ctx_and_chat_template() {
  let tmp = TmpDir::new("happy");
  let path = tmp.write(
    "model.gguf",
    &FixtureBuilder::new()
      .with_arch("llama")
      .with_context_length(8192)
      .with_chat_template("{% for m in messages %}{{ m.content }}{% endfor %}")
      .with_tokenizer_model("llama")
      .with_block_count(32)
      .with_head_count(32)
      .with_head_count_kv(8)
      .with_embedding_length(4096)
      .with_tensor("output.weight", &[4096, 32000], 12) // Q4_K
      .with_tensor("blk.0.attn_q.weight", &[4096, 4096], 12)
      .build(),
  );

  let read = read_path(&path, HeaderReadOptions::default()).unwrap();
  let meta = summarise_metadata(&read.header);

  assert_eq!(meta.arch.as_deref(), Some("llama"));
  assert_eq!(meta.native_ctx, Some(8192));
  assert!(meta.chat_template.is_some());
  assert_eq!(meta.tokenizer_kind.as_deref(), Some("llama"));
  assert_eq!(meta.mode_hint, ModeHint::Chat);
  assert_eq!(meta.quant, Quant::Q4_K);
}

#[test]
fn identity_stable_across_rename_with_real_files() {
  let tmp = TmpDir::new("rename");
  let bytes = build_minimal_gguf("llama");
  let original = tmp.write("alpha.gguf", &bytes);
  let read_a = read_path(&original, HeaderReadOptions::default()).unwrap();
  let id_a = compute_identity(&original, &read_a.raw);

  let renamed = tmp.path.join("beta.gguf");
  fs::rename(&original, &renamed).unwrap();
  let read_b = read_path(&renamed, HeaderReadOptions::default()).unwrap();
  let id_b = compute_identity(&renamed, &read_b.raw);

  assert_eq!(id_a.header_blake3, id_b.header_blake3);
  assert_ne!(id_a.path, id_b.path);
}

#[test]
fn identity_stable_across_symlink() {
  let tmp = TmpDir::new("symlink");
  let bytes = build_minimal_gguf("llama");
  let original = tmp.write("real.gguf", &bytes);
  let link = tmp.path.join("alias.gguf");
  #[cfg(unix)]
  std::os::unix::fs::symlink(&original, &link).unwrap();
  #[cfg(not(unix))]
  fs::copy(&original, &link).unwrap();

  let read_real = read_path(&original, HeaderReadOptions::default()).unwrap();
  let read_link = read_path(&link, HeaderReadOptions::default()).unwrap();

  let id_real = compute_identity(&original, &read_real.raw);
  let id_link = compute_identity(&link, &read_link.raw);
  assert_eq!(id_real.header_blake3, id_link.header_blake3);
  // Canonicalised path collapses the symlink to the real file.
  #[cfg(unix)]
  assert_eq!(id_real.path, id_link.path);
}

#[test]
fn zero_tensor_file_returns_metadata_without_panic() {
  let tmp = TmpDir::new("zero-tensor");
  let path = tmp.write("empty.gguf", &build_minimal_gguf("llama"));
  let read = read_path(&path, HeaderReadOptions::default()).unwrap();
  let meta = summarise_metadata(&read.header);
  assert_eq!(meta.mode_hint, ModeHint::Unknown);
  assert!(matches!(meta.quant, Quant::Unknown(_)));
  let est = estimate_memory(&read.header, EstimateOptions::default());
  assert_eq!(est.total_ram(), 0);
}

#[test]
fn missing_chat_template_is_none_not_error() {
  let tmp = TmpDir::new("no-chat-template");
  let bytes = FixtureBuilder::new()
    .with_arch("llama")
    .with_context_length(2048)
    .with_tensor("output.weight", &[10, 10], 1)
    .build();
  let path = tmp.write("model.gguf", &bytes);
  let read = read_path(&path, HeaderReadOptions::default()).unwrap();
  let meta = summarise_metadata(&read.header);
  assert!(meta.chat_template.is_none());
  assert_eq!(meta.native_ctx, Some(2048));
}

#[test]
fn unsupported_version_returns_typed_error() {
  let tmp = TmpDir::new("bad-version");
  let bytes = FixtureBuilder::new()
    .with_version(99)
    .with_arch("x")
    .build();
  let path = tmp.write("v99.gguf", &bytes);
  let err = read_path(&path, HeaderReadOptions::default()).unwrap_err();
  assert!(
    matches!(err, GgufError::UnsupportedVersion(99)),
    "got {err:?}"
  );
}

#[test]
fn truncated_file_returns_typed_error() {
  let tmp = TmpDir::new("truncated");
  // Just the magic, nothing after.
  let path = tmp.write("trunc.gguf", b"GGUF");
  let err = read_path(&path, HeaderReadOptions::default()).unwrap_err();
  assert!(matches!(err, GgufError::Truncated { .. }), "got {err:?}");
}

#[test]
fn non_gguf_file_returns_bad_magic() {
  let tmp = TmpDir::new("bad-magic");
  let path = tmp.write("not-a-gguf.bin", b"some random bytes that aren't a model");
  let err = read_path(&path, HeaderReadOptions::default()).unwrap_err();
  assert!(matches!(err, GgufError::BadMagic), "got {err:?}");
}

#[test]
fn nonexistent_path_returns_io_error_with_path() {
  let err = read_path("/nonexistent/path/to.gguf", HeaderReadOptions::default()).unwrap_err();
  match err {
    GgufError::IoAt { path, .. } => {
      assert_eq!(path, PathBuf::from("/nonexistent/path/to.gguf"));
    }
    other => panic!("expected IoAt, got {other:?}"),
  }
}

#[test]
fn header_too_large_when_kv_count_exceeds_cap() {
  let tmp = TmpDir::new("huge-kv");
  // Hand-craft a header that advertises a KV count well above our defensive ceiling.
  let mut bytes = Vec::new();
  bytes.extend_from_slice(b"GGUF");
  bytes.extend_from_slice(&3u32.to_le_bytes());
  bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
  bytes.extend_from_slice(&(10_000_000u64).to_le_bytes()); // kv_count well above cap
  let path = tmp.write("huge.gguf", &bytes);
  let err = read_path(&path, HeaderReadOptions::default()).unwrap_err();
  assert!(
    matches!(err, GgufError::HeaderTooLarge { .. }),
    "got {err:?}"
  );
}

#[test]
fn estimator_for_7b_q4km_in_realistic_range() {
  // Synthetic-but-realistic 7B-class header: 32 layers, 32 heads (32 KV
  // heads), 4096 embedding → head_dim 128. Single Q4_K tensor stands in
  // for the model body (sized to ~4.3 GB at Q4_K_M).
  // Q4_K bytes_per_elem = 144 / 256 = 0.5625. We need bytes ≈ 4.3 GB.
  // → elements ≈ 4.3 GB / 0.5625 ≈ 7.64 G elems, choose 7.5G to stay safe.
  let elem_count: u64 = 7_500_000_000;
  let bytes = FixtureBuilder::new()
    .with_arch("llama")
    .with_block_count(32)
    .with_head_count(32)
    .with_head_count_kv(32)
    .with_embedding_length(4096)
    .with_context_length(8192)
    .with_tensor("dummy.body.weight", &[elem_count], 12) // Q4_K
    .build();
  let tmp = TmpDir::new("7b-est");
  let path = tmp.write("7b.gguf", &bytes);
  let read = read_path(&path, HeaderReadOptions::default()).unwrap();
  let est = estimate_memory(
    &read.header,
    EstimateOptions {
      ctx_len: 8192,
      ..EstimateOptions::default()
    },
  );
  // Weights: ~4.3 GiB. KV at 8192 ctx, f16 K+V: 2*32*32*128*8192*2 = 4 GiB.
  let weights_gb = est.weights_ram as f64 / (1u64 << 30) as f64;
  let kv_gb = est.kv_cache_ram as f64 / (1u64 << 30) as f64;
  assert!(
    (3.5..=5.0).contains(&weights_gb),
    "weights {weights_gb} GB outside expected band"
  );
  assert!(
    (3.5..=4.5).contains(&kv_gb),
    "kv {kv_gb} GB outside expected band"
  );
}

#[test]
fn estimator_for_phi_3_5_mini_q5_matches_closed_form() {
  // Phi 3.5 mini Q5_K_M reference geometry: arch "phi3", 32 layers, 32 heads
  // (32 KV heads), 3072 embedding → head_dim = 96, native ctx 131072. A single
  // Q5_K body tensor stands in for the model weights, sized so storage cost
  // approximates a real Phi 3.5 mini Q5_K_M file (~2.6 GiB for ~3.8B params).
  // Q5_K bytes_per_elem = 176 / 256 = 0.6875.
  let elem_count: u64 = 4_000_000_000;
  let bytes = FixtureBuilder::new()
    .with_arch("phi3")
    .with_block_count(32)
    .with_head_count(32)
    .with_head_count_kv(32)
    .with_embedding_length(3072)
    .with_context_length(131_072)
    .with_tensor("dummy.body.weight", &[elem_count], 13) // Q5_K
    .build();
  let tmp = TmpDir::new("phi3-est");
  let path = tmp.write("phi3.gguf", &bytes);
  let read = read_path(&path, HeaderReadOptions::default()).unwrap();
  let est = estimate_memory(
    &read.header,
    EstimateOptions {
      ctx_len: 4096,
      ..EstimateOptions::default()
    },
  );

  // Weights: row-aligned Q5_K geometry (256 elems/block, 176 bytes/block).
  let expected_weights = elem_count.div_ceil(256) * 176;
  assert_eq!(est.weights_ram, expected_weights);
  // KV at 4096 ctx, f16 K + f16 V:
  //   2 (K+V) * 32 layers * 32 kv_heads * 96 head_dim * 4096 ctx * 2 (f16) = 1.5 GiB.
  let expected_kv: u64 = 2 * 32 * 32 * 96 * 4096 * 2;
  assert_eq!(est.kv_cache_ram, expected_kv);
  // CPU-only default: nothing offloaded to VRAM.
  assert_eq!(est.weights_vram, 0);
  assert_eq!(est.kv_cache_vram, 0);

  // Sanity-check the human-facing band: ~2.6 GiB weights, ~1.5 GiB KV at 4k ctx.
  let weights_gib = est.weights_ram as f64 / (1u64 << 30) as f64;
  let kv_gib = est.kv_cache_ram as f64 / (1u64 << 30) as f64;
  assert!(
    (2.0..=3.2).contains(&weights_gib),
    "weights {weights_gib} GiB outside expected Phi 3.5 mini Q5_K_M band"
  );
  assert!(
    (1.0..=2.0).contains(&kv_gib),
    "kv {kv_gib} GiB outside expected Phi 3.5 mini @ 4k ctx band"
  );
}

#[test]
fn estimator_for_synthetic_embedding_model_matches_closed_form() {
  // BGE-small-style synthetic embedding model: BERT arch, 6 layers, 12 heads
  // (12 KV heads), 384 embedding → head_dim = 32, 512 ctx, F16 weights. We use
  // an attention tensor (no `output.weight`) so the mode heuristic stays
  // Embedding rather than flipping to Chat.
  let body_elems: u64 = 33_000_000; // ~66 MiB at F16 — typical BGE-small footprint.
  let bytes = FixtureBuilder::new()
    .with_arch("bert")
    .with_block_count(6)
    .with_head_count(12)
    .with_head_count_kv(12)
    .with_embedding_length(384)
    .with_context_length(512)
    .with_tensor("blk.0.attn_q.weight", &[body_elems], 1) // F16
    .build();
  let tmp = TmpDir::new("embed-est");
  let path = tmp.write("embed.gguf", &bytes);
  let read = read_path(&path, HeaderReadOptions::default()).unwrap();

  let meta = summarise_metadata(&read.header);
  assert_eq!(meta.mode_hint, ModeHint::Embedding);
  assert_eq!(meta.quant, Quant::F16);

  let est = estimate_memory(
    &read.header,
    EstimateOptions {
      ctx_len: 512,
      ..EstimateOptions::default()
    },
  );
  // Weights: F16 = 2 bytes/elem.
  assert_eq!(est.weights_ram, body_elems * 2);
  // KV: 2 (K+V) * 6 layers * 12 kv_heads * 32 head_dim * 512 ctx * 2 (f16) = 4.5 MiB.
  let expected_kv: u64 = 2 * 6 * 12 * 32 * 512 * 2;
  assert_eq!(est.kv_cache_ram, expected_kv);
  assert_eq!(est.weights_vram, 0);
  assert_eq!(est.kv_cache_vram, 0);
}

#[test]
fn small_header_cap_truncates_into_typed_error() {
  // A header with a single small KV pair would normally fit, but we set the
  // cap to 1 byte to force a truncation error.
  let tmp = TmpDir::new("cap-too-small");
  let path = tmp.write("model.gguf", &build_minimal_gguf("llama"));
  let opts = HeaderReadOptions { cap_bytes: 1 };
  let err = read_path(&path, opts).unwrap_err();
  assert!(matches!(err, GgufError::Truncated { .. }), "got {err:?}");
}

#[test]
fn reasoning_hint_surfaces_in_summary() {
  let tmp = TmpDir::new("reasoning");
  let bytes = FixtureBuilder::new()
    .with_arch("qwen3")
    .with_kv(
      "tokenizer.ggml.tokens",
      GgufValue::Array(vec![
        GgufValue::String("<bos>".to_string()),
        GgufValue::String("<think>".to_string()),
      ]),
    )
    .build();
  let path = tmp.write("qwen3.gguf", &bytes);
  let read = read_path(&path, HeaderReadOptions::default()).unwrap();
  let meta = summarise_metadata(&read.header);
  assert!(meta.reasoning_hint);
}

// --- tiny tmpdir helper (avoids adding a `tempfile` dev-dep just for this) ---

struct TmpDir {
  path: PathBuf,
}

impl TmpDir {
  fn new(label: &str) -> Self {
    let nanos = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let path = std::env::temp_dir().join(format!(
      "llamastash-gguf-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    TmpDir { path }
  }

  fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
    let p = self.path.join(name);
    let mut f = fs::File::create(&p).unwrap();
    f.write_all(bytes).unwrap();
    p
  }
}

impl Drop for TmpDir {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.path);
  }
}
