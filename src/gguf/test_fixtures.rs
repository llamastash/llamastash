//! Synthetic GGUF byte-builder used by both inline unit tests in this crate
//! and the integration tests under `tests/`. Marked `#[doc(hidden)]` because
//! the surface is testing-only — it isn't part of llamastash's public API.
//!
//! The builder emits a structurally valid GGUF v3 prefix (magic, version,
//! counts, KV list, tensor info). It does **not** emit tensor data — Unit 3
//! only consumes the header, and any consumer that wants real tensor bytes
//! lives in a later unit (the supervisor in Unit 5 doesn't read tensor data
//! either, it just launches `llama-server` against the file path).

use crate::gguf::header::GgufValue;

/// Fluent builder for synthetic GGUF headers. See module docs.
#[derive(Debug, Default)]
pub struct FixtureBuilder {
  metadata: Vec<(String, GgufValue)>,
  tensors: Vec<TensorSpec>,
  version: u32,
}

#[derive(Debug, Clone)]
struct TensorSpec {
  name: String,
  dims: Vec<u64>,
  ggml_type: u32,
}

impl FixtureBuilder {
  /// New empty builder. Defaults to GGUF v3.
  pub fn new() -> Self {
    FixtureBuilder {
      version: 3,
      ..FixtureBuilder::default()
    }
  }

  /// Override the GGUF version (used to exercise the unsupported-version error path).
  pub fn with_version(mut self, version: u32) -> Self {
    self.version = version;
    self
  }

  /// Set the `general.architecture` key. This also implies the prefix used
  /// by per-arch metadata keys (e.g. `llama.context_length`).
  pub fn with_arch(mut self, arch: &str) -> Self {
    self.metadata.push((
      "general.architecture".to_string(),
      GgufValue::String(arch.to_string()),
    ));
    self
  }

  /// Set `<arch>.context_length`. Architecture must have been set via
  /// [`Self::with_arch`] before calling this.
  pub fn with_context_length(mut self, ctx: u64) -> Self {
    let key = format!("{}.context_length", self.arch_or("model"));
    self.metadata.push((key, GgufValue::U64(ctx)));
    self
  }

  pub fn with_block_count(mut self, n: u64) -> Self {
    let key = format!("{}.block_count", self.arch_or("model"));
    self.metadata.push((key, GgufValue::U64(n)));
    self
  }

  pub fn with_head_count(mut self, n: u64) -> Self {
    let key = format!("{}.attention.head_count", self.arch_or("model"));
    self.metadata.push((key, GgufValue::U64(n)));
    self
  }

  pub fn with_head_count_kv(mut self, n: u64) -> Self {
    let key = format!("{}.attention.head_count_kv", self.arch_or("model"));
    self.metadata.push((key, GgufValue::U64(n)));
    self
  }

  pub fn with_embedding_length(mut self, n: u64) -> Self {
    let key = format!("{}.embedding_length", self.arch_or("model"));
    self.metadata.push((key, GgufValue::U64(n)));
    self
  }

  pub fn with_feed_forward_length(mut self, n: u64) -> Self {
    let key = format!("{}.feed_forward_length", self.arch_or("model"));
    self.metadata.push((key, GgufValue::U64(n)));
    self
  }

  pub fn with_chat_template(mut self, template: &str) -> Self {
    self.metadata.push((
      "tokenizer.chat_template".to_string(),
      GgufValue::String(template.to_string()),
    ));
    self
  }

  pub fn with_tokenizer_model(mut self, kind: &str) -> Self {
    self.metadata.push((
      "tokenizer.ggml.model".to_string(),
      GgufValue::String(kind.to_string()),
    ));
    self
  }

  /// Adds an arbitrary KV pair (used to test mode-hint heuristics and the
  /// rare general.* keys that don't fit the helpers above).
  pub fn with_kv(mut self, key: &str, value: GgufValue) -> Self {
    self.metadata.push((key.to_string(), value));
    self
  }

  /// Adds a fake KV pair of arbitrary string length — used to exercise the
  /// `cap_bytes` truncation path.
  pub fn with_padding_kv(self, key: &str, len: usize) -> Self {
    let value = "x".repeat(len);
    self.with_kv(key, GgufValue::String(value))
  }

  /// Append a tensor info entry. `ggml_type` is the GGML quantisation tag
  /// (F32=0, F16=1, Q4_0=2, Q8_0=8, Q4_K=12, etc.).
  pub fn with_tensor(mut self, name: &str, dims: &[u64], ggml_type: u32) -> Self {
    self.tensors.push(TensorSpec {
      name: name.to_string(),
      dims: dims.to_vec(),
      ggml_type,
    });
    self
  }

  /// Serialise to a structurally valid GGUF byte vector (header only — no
  /// tensor data follows).
  pub fn build(self) -> Vec<u8> {
    let FixtureBuilder {
      metadata,
      tensors,
      version,
    } = self;
    let mut out = Vec::new();
    out.extend_from_slice(b"GGUF");
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
    out.extend_from_slice(&(metadata.len() as u64).to_le_bytes());
    for (key, value) in metadata {
      write_string(&mut out, &key);
      write_value(&mut out, &value);
    }
    for t in tensors {
      write_string(&mut out, &t.name);
      out.extend_from_slice(&(t.dims.len() as u32).to_le_bytes());
      for d in &t.dims {
        out.extend_from_slice(&d.to_le_bytes());
      }
      out.extend_from_slice(&t.ggml_type.to_le_bytes());
      out.extend_from_slice(&0u64.to_le_bytes()); // offset (zero is fine; we don't read tensor data)
    }
    out
  }

  fn arch_or<'a>(&'a self, fallback: &'a str) -> &'a str {
    for (k, v) in &self.metadata {
      if k == "general.architecture" {
        if let GgufValue::String(s) = v {
          return s.as_str();
        }
      }
    }
    fallback
  }
}

/// Convenience for the most common minimal-but-valid fixture: a header with
/// just `general.architecture` and zero tensors.
pub fn build_minimal_gguf(arch: &str) -> Vec<u8> {
  FixtureBuilder::new().with_arch(arch).build()
}

fn write_string(out: &mut Vec<u8>, s: &str) {
  out.extend_from_slice(&(s.len() as u64).to_le_bytes());
  out.extend_from_slice(s.as_bytes());
}

fn write_value(out: &mut Vec<u8>, value: &GgufValue) {
  match value {
    GgufValue::U8(v) => {
      out.extend_from_slice(&0u32.to_le_bytes());
      out.push(*v);
    }
    GgufValue::I8(v) => {
      out.extend_from_slice(&1u32.to_le_bytes());
      out.push(*v as u8);
    }
    GgufValue::U16(v) => {
      out.extend_from_slice(&2u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
    GgufValue::I16(v) => {
      out.extend_from_slice(&3u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
    GgufValue::U32(v) => {
      out.extend_from_slice(&4u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
    GgufValue::I32(v) => {
      out.extend_from_slice(&5u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
    GgufValue::F32(v) => {
      out.extend_from_slice(&6u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
    GgufValue::Bool(b) => {
      out.extend_from_slice(&7u32.to_le_bytes());
      out.push(if *b { 1 } else { 0 });
    }
    GgufValue::String(s) => {
      out.extend_from_slice(&8u32.to_le_bytes());
      write_string(out, s);
    }
    GgufValue::Array(items) => {
      out.extend_from_slice(&9u32.to_le_bytes());
      let elem_ty = items.first().map(value_type_tag).unwrap_or(8 /* string */);
      out.extend_from_slice(&elem_ty.to_le_bytes());
      out.extend_from_slice(&(items.len() as u64).to_le_bytes());
      for item in items {
        write_value_payload(out, item);
      }
    }
    GgufValue::U64(v) => {
      out.extend_from_slice(&10u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
    GgufValue::I64(v) => {
      out.extend_from_slice(&11u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
    GgufValue::F64(v) => {
      out.extend_from_slice(&12u32.to_le_bytes());
      out.extend_from_slice(&v.to_le_bytes());
    }
  }
}

/// Inside an array, each element is just the payload — no per-element type tag.
fn write_value_payload(out: &mut Vec<u8>, value: &GgufValue) {
  match value {
    GgufValue::U8(v) => out.push(*v),
    GgufValue::I8(v) => out.push(*v as u8),
    GgufValue::U16(v) => out.extend_from_slice(&v.to_le_bytes()),
    GgufValue::I16(v) => out.extend_from_slice(&v.to_le_bytes()),
    GgufValue::U32(v) => out.extend_from_slice(&v.to_le_bytes()),
    GgufValue::I32(v) => out.extend_from_slice(&v.to_le_bytes()),
    GgufValue::F32(v) => out.extend_from_slice(&v.to_le_bytes()),
    GgufValue::Bool(b) => out.push(if *b { 1 } else { 0 }),
    GgufValue::String(s) => write_string(out, s),
    GgufValue::Array(_) => unreachable!("nested arrays are not used in fixtures"),
    GgufValue::U64(v) => out.extend_from_slice(&v.to_le_bytes()),
    GgufValue::I64(v) => out.extend_from_slice(&v.to_le_bytes()),
    GgufValue::F64(v) => out.extend_from_slice(&v.to_le_bytes()),
  }
}

fn value_type_tag(v: &GgufValue) -> u32 {
  match v {
    GgufValue::U8(_) => 0,
    GgufValue::I8(_) => 1,
    GgufValue::U16(_) => 2,
    GgufValue::I16(_) => 3,
    GgufValue::U32(_) => 4,
    GgufValue::I32(_) => 5,
    GgufValue::F32(_) => 6,
    GgufValue::Bool(_) => 7,
    GgufValue::String(_) => 8,
    GgufValue::Array(_) => 9,
    GgufValue::U64(_) => 10,
    GgufValue::I64(_) => 11,
    GgufValue::F64(_) => 12,
  }
}
