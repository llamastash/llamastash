//! Hand-rolled GGUF header reader.
//!
//! GGUF (v2/v3) layout that matters here:
//!
//! ```text
//! magic        : 4 bytes "GGUF"
//! version      : u32 LE
//! tensor_count : u64 LE
//! kv_count     : u64 LE
//! kv[kv_count] : { key: gguf_string, value_type: u32 LE, value: per-type }
//! ti[tensor_count] : {
//!   name: gguf_string,
//!   n_dims: u32 LE,
//!   dims: [n_dims] u64 LE,
//!   type: u32 LE (ggml type),
//!   offset: u64 LE,
//! }
//! (tensor data begins at next `general.alignment` boundary)
//! ```
//!
//! We only ever consume the structural prefix (everything up to but not
//! including tensor data). The exact byte slice that we successfully parsed
//! is returned to the caller so [`crate::gguf::identity`] can hash it for a
//! stable [`ModelId`](crate::gguf::identity::ModelId).
//!
//! Allocation is bounded: the reader caps the buffer at `options.cap_bytes`
//! and refuses headers whose parsed length would exceed the cap.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::gguf::errors::{GgufError, GgufResult};

/// GGUF magic. Stored little-endian when emitted as a u32.
pub const GGUF_MAGIC: &[u8; 4] = b"GGUF";

/// Default soft cap on the structural-header window in bytes (16 MiB).
///
/// Modern tokenizers (Gemma 3+, Qwen 2.5+, Llama 3.x) embed the full
/// `tokens.list` array — 128k–256k strings — directly in the metadata
/// KV section, which routinely pushes the structural header to 2–8
/// MiB on real models. The original 1 MiB default clipped these
/// headers mid-string and produced misleading `BadStringLen` errors
/// on every modern chat / embed model; 16 MiB covers everything
/// observed in the wild today with headroom for the next vocab-size
/// jump. Tests that need a smaller window can pass an explicit
/// `HeaderReadOptions { cap_bytes: … }`.
pub const DEFAULT_HEADER_CAP_BYTES: u64 = 16 << 20;
/// Hard cap, used if a caller asks for a larger window (64 MiB).
/// Sized so a hostile file with an enormous metadata payload still
/// fails fast rather than OOMing the daemon's blocking-pool worker.
pub const MAX_HEADER_CAP_BYTES: u64 = 64 << 20;

/// Defensive ceilings on attacker-controllable counts.
const MAX_KV_COUNT: u64 = 10_000;
const MAX_TENSOR_COUNT: u64 = 1_000_000;
const MAX_TENSOR_DIMS: u32 = 8;
const MAX_STRING_LEN: u64 = 4 * 1024 * 1024;
const MAX_ARRAY_LEN: u64 = 1_000_000;
/// Maximum levels of `Array(Array(...))` nesting accepted by
/// [`read_value`]. The header window cap (`MAX_HEADER_CAP_BYTES = 64
/// MiB`) lets a hostile file describe hundreds of thousands of levels
/// at ~12 bytes per level, which is enough to blow the parser's
/// (blocking-pool) stack on a synchronous recursion. We don't see
/// nested arrays in real GGUFs.
const MAX_ARRAY_NEST_DEPTH: usize = 4;

/// Tunable knobs for [`read_path`] and [`read_reader`].
#[derive(Debug, Clone, Copy)]
pub struct HeaderReadOptions {
  /// Maximum number of bytes to read from the file when locating the header.
  /// Clamped to [`MAX_HEADER_CAP_BYTES`].
  pub cap_bytes: u64,
}

impl Default for HeaderReadOptions {
  fn default() -> Self {
    HeaderReadOptions {
      cap_bytes: DEFAULT_HEADER_CAP_BYTES,
    }
  }
}

/// One parsed value from the metadata KV list.
#[derive(Debug, Clone, PartialEq)]
pub enum GgufValue {
  U8(u8),
  I8(i8),
  U16(u16),
  I16(i16),
  U32(u32),
  I32(i32),
  F32(f32),
  Bool(bool),
  String(String),
  Array(Vec<GgufValue>),
  U64(u64),
  I64(i64),
  F64(f64),
}

impl GgufValue {
  /// Convenience: extract a string value, returning None for other kinds.
  pub fn as_str(&self) -> Option<&str> {
    match self {
      GgufValue::String(s) => Some(s.as_str()),
      _ => None,
    }
  }

  /// Convenience: coerce any unsigned/signed integer value to u64.
  pub fn as_u64(&self) -> Option<u64> {
    Some(match self {
      GgufValue::U8(v) => *v as u64,
      GgufValue::I8(v) if *v >= 0 => *v as u64,
      GgufValue::U16(v) => *v as u64,
      GgufValue::I16(v) if *v >= 0 => *v as u64,
      GgufValue::U32(v) => *v as u64,
      GgufValue::I32(v) if *v >= 0 => *v as u64,
      GgufValue::U64(v) => *v,
      GgufValue::I64(v) if *v >= 0 => *v as u64,
      _ => return None,
    })
  }

  /// Convenience: coerce a boolean.
  pub fn as_bool(&self) -> Option<bool> {
    match self {
      GgufValue::Bool(b) => Some(*b),
      _ => None,
    }
  }
}

/// Per-tensor info entry (name + dimensions + ggml type tag). The byte
/// offset into the tensor-data segment is parsed but intentionally not
/// retained — Unit 3 only needs the structural shape for memory estimation.
#[derive(Debug, Clone)]
pub struct TensorInfo {
  pub name: String,
  pub dims: Vec<u64>,
  pub ggml_type: u32,
}

impl TensorInfo {
  /// Total number of elements (product of dims). Returns 0 if `dims` is empty.
  pub fn n_elements(&self) -> u64 {
    if self.dims.is_empty() {
      0
    } else {
      self.dims.iter().copied().fold(1u64, u64::saturating_mul)
    }
  }
}

/// Result of [`read_path`] / [`read_reader`]: the structural prefix bytes
/// (for identity hashing), plus the parsed shape.
#[derive(Debug, Clone)]
pub struct ReadHeader {
  /// Exact bytes consumed by parsing — suitable for BLAKE3.
  pub raw: Vec<u8>,
  /// Parsed structural view of those bytes.
  pub header: GgufHeader,
}

/// Parsed GGUF header.
#[derive(Debug, Clone)]
pub struct GgufHeader {
  pub version: u32,
  pub tensor_count: u64,
  pub metadata: HashMap<String, GgufValue>,
  pub tensors: Vec<TensorInfo>,
}

impl GgufHeader {
  /// Lookup helper for arch-prefixed keys (e.g. `llama.context_length`).
  pub fn get(&self, key: &str) -> Option<&GgufValue> {
    self.metadata.get(key)
  }

  /// First metadata string under any of the given keys.
  pub fn get_string<'a, K: AsRef<str>>(&'a self, keys: &[K]) -> Option<&'a str> {
    for k in keys {
      if let Some(v) = self.metadata.get(k.as_ref()).and_then(|v| v.as_str()) {
        return Some(v);
      }
    }
    None
  }

  /// First metadata integer under any of the given keys, coerced to u64.
  pub fn get_u64<K: AsRef<str>>(&self, keys: &[K]) -> Option<u64> {
    for k in keys {
      if let Some(v) = self.metadata.get(k.as_ref()).and_then(|v| v.as_u64()) {
        return Some(v);
      }
    }
    None
  }
}

/// Read and parse the header of a GGUF file located at `path`.
pub fn read_path<P: AsRef<Path>>(path: P, opts: HeaderReadOptions) -> GgufResult<ReadHeader> {
  let path_ref = path.as_ref();
  let file = File::open(path_ref).map_err(|e| GgufError::IoAt {
    path: path_ref.to_path_buf(),
    source: e,
  })?;
  read_reader(file, opts)
}

/// Read and parse the header from any [`Read`] source. The reader is fully
/// consumed (or hits the cap) before parsing; this keeps the BLAKE3 input
/// byte-stable irrespective of reader-side chunk sizes.
pub fn read_reader<R: Read>(reader: R, opts: HeaderReadOptions) -> GgufResult<ReadHeader> {
  let cap = opts.cap_bytes.min(MAX_HEADER_CAP_BYTES);
  let mut buf = Vec::new();
  // +1 so we can detect that the file is longer than the cap (the read of
  // the cap-plus-one byte either succeeds or returns 0).
  let mut take = reader.take(cap + 1);
  take.read_to_end(&mut buf)?;
  let truncated_by_cap = buf.len() as u64 > cap;
  if truncated_by_cap {
    buf.truncate(cap as usize);
  }

  let mut cur = Cursor::new(&buf);
  let magic = cur.read_bytes(4)?;
  if magic != GGUF_MAGIC {
    return Err(GgufError::BadMagic);
  }
  let version = cur.read_u32_le()?;
  if version != 2 && version != 3 {
    return Err(GgufError::UnsupportedVersion(version));
  }
  let tensor_count = cur.read_u64_le()?;
  if tensor_count > MAX_TENSOR_COUNT {
    return Err(GgufError::HeaderTooLarge {
      advertised: tensor_count,
      cap: MAX_TENSOR_COUNT,
    });
  }
  let kv_count = cur.read_u64_le()?;
  if kv_count > MAX_KV_COUNT {
    return Err(GgufError::HeaderTooLarge {
      advertised: kv_count,
      cap: MAX_KV_COUNT,
    });
  }

  // Symmetrise the pre-allocation with the tensor list below: cap
  // attacker-controlled hints at a small bound; `HashMap` resizes
  // organically as inserts arrive. Without this an attacker could
  // force a 10 000-slot allocation for one element of payload.
  let mut metadata = HashMap::with_capacity((kv_count.min(1024)) as usize);
  for _ in 0..kv_count {
    let key = cur.read_gguf_string()?;
    let value_type = cur.read_u32_le()?;
    let value = read_value(&mut cur, value_type, 0)?;
    metadata.insert(key, value);
  }

  let mut tensors = Vec::with_capacity(tensor_count.min(4096) as usize);
  for _ in 0..tensor_count {
    let name = cur.read_gguf_string()?;
    let n_dims = cur.read_u32_le()?;
    if n_dims > MAX_TENSOR_DIMS {
      return Err(GgufError::HeaderTooLarge {
        advertised: n_dims as u64,
        cap: MAX_TENSOR_DIMS as u64,
      });
    }
    let mut dims = Vec::with_capacity(n_dims as usize);
    for _ in 0..n_dims {
      dims.push(cur.read_u64_le()?);
    }
    let ggml_type = cur.read_u32_le()?;
    let _offset = cur.read_u64_le()?;
    tensors.push(TensorInfo {
      name,
      dims,
      ggml_type,
    });
  }

  let consumed = cur.position();
  let raw = buf[..consumed].to_vec();

  Ok(ReadHeader {
    raw,
    header: GgufHeader {
      version,
      tensor_count,
      metadata,
      tensors,
    },
  })
}

fn read_value(cur: &mut Cursor<'_>, value_type: u32, depth: usize) -> GgufResult<GgufValue> {
  Ok(match value_type {
    0 => GgufValue::U8(cur.read_u8()?),
    1 => GgufValue::I8(cur.read_u8()? as i8),
    2 => GgufValue::U16(u16::from_le_bytes(cur.read_array::<2>()?)),
    3 => GgufValue::I16(i16::from_le_bytes(cur.read_array::<2>()?)),
    4 => GgufValue::U32(cur.read_u32_le()?),
    5 => GgufValue::I32(i32::from_le_bytes(cur.read_array::<4>()?)),
    6 => GgufValue::F32(f32::from_le_bytes(cur.read_array::<4>()?)),
    7 => GgufValue::Bool(cur.read_u8()? != 0),
    8 => GgufValue::String(cur.read_gguf_string()?),
    9 => {
      if depth >= MAX_ARRAY_NEST_DEPTH {
        return Err(GgufError::ArrayNestingTooDeep {
          depth: depth + 1,
          cap: MAX_ARRAY_NEST_DEPTH,
        });
      }
      let elem_ty = cur.read_u32_le()?;
      let len = cur.read_u64_le()?;
      if len > MAX_ARRAY_LEN {
        return Err(GgufError::HeaderTooLarge {
          advertised: len,
          cap: MAX_ARRAY_LEN,
        });
      }
      // Vec pre-allocation must be bounded by what can actually be read
      // from the remaining header bytes. A malicious header can declare
      // `len = MAX_ARRAY_LEN` and then truncate — without this cap we
      // would have already committed up to 40 KiB per nesting level
      // before `Truncated` fires. `read_value` consumes at least one
      // byte per element (Bool/U8); divide remaining-byte budget by 1
      // for the worst case.
      let remaining = cur.remaining() as u64;
      let safe_cap = len.min(1024).min(remaining) as usize;
      let mut items = Vec::with_capacity(safe_cap);
      for _ in 0..len {
        items.push(read_value(cur, elem_ty, depth + 1)?);
      }
      GgufValue::Array(items)
    }
    10 => GgufValue::U64(cur.read_u64_le()?),
    11 => GgufValue::I64(i64::from_le_bytes(cur.read_array::<8>()?)),
    12 => GgufValue::F64(f64::from_le_bytes(cur.read_array::<8>()?)),
    other => return Err(GgufError::BadValueType(other)),
  })
}

/// Tiny byte-slice cursor with EOF-aware reads. Kept private so the parser
/// surface stays small; we deliberately do not use `std::io::Cursor` because
/// we want EOF to become [`GgufError::Truncated`] without intermediate
/// conversions.
struct Cursor<'a> {
  buf: &'a [u8],
  pos: usize,
}

impl<'a> Cursor<'a> {
  fn new(buf: &'a [u8]) -> Self {
    Cursor { buf, pos: 0 }
  }

  fn position(&self) -> usize {
    self.pos
  }

  fn remaining(&self) -> usize {
    self.buf.len().saturating_sub(self.pos)
  }

  fn read_bytes(&mut self, n: usize) -> GgufResult<&'a [u8]> {
    if self.remaining() < n {
      return Err(GgufError::Truncated {
        needed: self.pos + n,
        got: self.buf.len(),
      });
    }
    let out = &self.buf[self.pos..self.pos + n];
    self.pos += n;
    Ok(out)
  }

  fn read_array<const N: usize>(&mut self) -> GgufResult<[u8; N]> {
    let slice = self.read_bytes(N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
  }

  fn read_u8(&mut self) -> GgufResult<u8> {
    Ok(self.read_array::<1>()?[0])
  }

  fn read_u32_le(&mut self) -> GgufResult<u32> {
    Ok(u32::from_le_bytes(self.read_array::<4>()?))
  }

  fn read_u64_le(&mut self) -> GgufResult<u64> {
    Ok(u64::from_le_bytes(self.read_array::<8>()?))
  }

  fn read_gguf_string(&mut self) -> GgufResult<String> {
    let len = self.read_u64_le()?;
    if len > MAX_STRING_LEN || len as usize > self.remaining() {
      return Err(GgufError::BadStringLen(len));
    }
    let bytes = self.read_bytes(len as usize)?;
    std::str::from_utf8(bytes)
      .map(|s| s.to_owned())
      .map_err(|_| GgufError::BadUtf8)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gguf::test_fixtures::{build_minimal_gguf, FixtureBuilder};
  use std::io::Cursor as IoCursor;

  #[test]
  fn rejects_non_gguf_bytes() {
    let bytes = b"NOT-A-GGUF-FILE";
    let err = read_reader(IoCursor::new(&bytes[..]), HeaderReadOptions::default()).unwrap_err();
    assert!(matches!(err, GgufError::BadMagic));
  }

  #[test]
  fn rejects_truncated_after_magic() {
    let bytes = &b"GGUF"[..];
    let err = read_reader(IoCursor::new(bytes), HeaderReadOptions::default()).unwrap_err();
    assert!(matches!(err, GgufError::Truncated { .. }));
  }

  #[test]
  fn rejects_unsupported_version() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"GGUF");
    bytes.extend_from_slice(&99u32.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
    bytes.extend_from_slice(&0u64.to_le_bytes()); // kv_count
    let err = read_reader(IoCursor::new(&bytes[..]), HeaderReadOptions::default()).unwrap_err();
    assert!(matches!(err, GgufError::UnsupportedVersion(99)));
  }

  #[test]
  fn reads_minimal_valid_header() {
    let bytes = build_minimal_gguf("llama");
    let read = read_reader(
      IoCursor::new(bytes.as_slice()),
      HeaderReadOptions::default(),
    )
    .unwrap();
    assert_eq!(read.header.version, 3);
    assert_eq!(read.header.tensor_count, 0);
    assert_eq!(
      read.header.get_string(&["general.architecture"]).unwrap(),
      "llama"
    );
    // The raw bytes must exactly equal what we consumed — the contract relied
    // on by `identity::compute`.
    assert_eq!(read.raw.len(), bytes.len());
  }

  #[test]
  fn rejects_excessive_kv_count() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"GGUF");
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&(MAX_KV_COUNT + 1).to_le_bytes());
    let err = read_reader(IoCursor::new(&bytes[..]), HeaderReadOptions::default()).unwrap_err();
    assert!(matches!(err, GgufError::HeaderTooLarge { .. }));
  }

  #[test]
  fn parses_tensor_info_and_metadata_together() {
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_block_count(2)
      .with_head_count(4)
      .with_embedding_length(8)
      .with_context_length(2048)
      .with_tensor("output.weight", &[8, 32000], 1)
      .with_tensor("blk.0.attn_q.weight", &[8, 8], 8)
      .build();
    let read = read_reader(
      IoCursor::new(bytes.as_slice()),
      HeaderReadOptions::default(),
    )
    .unwrap();
    assert_eq!(read.header.tensor_count, 2);
    assert_eq!(read.header.tensors.len(), 2);
    assert_eq!(read.header.tensors[0].name, "output.weight");
    assert_eq!(read.header.tensors[1].ggml_type, 8); // Q8_0
  }

  #[test]
  fn default_cap_handles_realistic_tokenizer_payload() {
    // Modern tokenizers (Gemma 3+, Qwen 2.5+, Llama 3.x) embed
    // `tokens.list` arrays of 128k–256k strings inside the metadata
    // KV section; the resulting structural header routinely lands
    // in the 2–8 MiB range. The original 1 MiB default cap clipped
    // these mid-string and produced misleading `BadStringLen` /
    // `Truncated` errors on every modern chat / embed model. This
    // test pins the default cap large enough to parse a synthetic
    // 4 MiB metadata payload — a plausible real-world floor.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_padding_kv("tokens.list_synthetic_payload", 4 * 1024 * 1024)
      .build();
    let read = read_reader(
      IoCursor::new(bytes.as_slice()),
      HeaderReadOptions::default(),
    )
    .expect("default cap must accommodate a 4 MiB metadata payload");
    assert_eq!(
      read
        .header
        .metadata
        .get("general.architecture")
        .and_then(|v| v.as_str()),
      Some("llama")
    );
  }

  #[test]
  fn header_too_large_when_cap_too_small_for_strings() {
    // A 64 KiB key would normally parse fine, but with a 1 KiB cap the
    // reader will see truncation before completing the string.
    let bytes = FixtureBuilder::new()
      .with_arch("llama")
      .with_padding_kv("filler", 64 * 1024)
      .build();
    let opts = HeaderReadOptions { cap_bytes: 1024 };
    let err = read_reader(IoCursor::new(bytes.as_slice()), opts).unwrap_err();
    assert!(
      matches!(
        err,
        GgufError::Truncated { .. } | GgufError::BadStringLen(_)
      ),
      "got {err:?}"
    );
  }

  /// Build a GGUF file whose only KV is `Array(Array(Array(...)))` nested
  /// `depth` levels deep, each level a 1-element array of the next. The
  /// innermost element is a `U8`.
  fn build_nested_array_gguf(depth: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"GGUF");
    bytes.extend_from_slice(&3u32.to_le_bytes()); // version
    bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
    bytes.extend_from_slice(&1u64.to_le_bytes()); // kv_count
                                                  // KV[0].key = "nest"
    let key = b"nest";
    bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
    bytes.extend_from_slice(key);
    // KV[0].value_type = 9 (Array)
    bytes.extend_from_slice(&9u32.to_le_bytes());
    // Each outer Array advertises elem_ty=9 (Array) with len=1, except
    // the innermost which advertises elem_ty=0 (U8) with len=1 + a byte.
    for _ in 0..(depth - 1) {
      bytes.extend_from_slice(&9u32.to_le_bytes()); // elem_ty = Array
      bytes.extend_from_slice(&1u64.to_le_bytes()); // len = 1
    }
    bytes.extend_from_slice(&0u32.to_le_bytes()); // innermost elem_ty = U8
    bytes.extend_from_slice(&1u64.to_le_bytes()); // len = 1
    bytes.push(0x42);
    bytes
  }

  #[test]
  fn rejects_array_nesting_past_cap() {
    let bytes = build_nested_array_gguf(MAX_ARRAY_NEST_DEPTH + 2);
    let err = read_reader(
      IoCursor::new(bytes.as_slice()),
      HeaderReadOptions::default(),
    )
    .unwrap_err();
    assert!(
      matches!(err, GgufError::ArrayNestingTooDeep { .. }),
      "got {err:?}"
    );
  }

  #[test]
  fn accepts_array_nesting_at_cap() {
    let bytes = build_nested_array_gguf(MAX_ARRAY_NEST_DEPTH);
    let read = read_reader(
      IoCursor::new(bytes.as_slice()),
      HeaderReadOptions::default(),
    )
    .expect("at-cap nesting must parse");
    // The KV exists and is a nested Array.
    assert_eq!(read.header.metadata.len(), 1);
    assert!(matches!(
      read.header.metadata.get("nest"),
      Some(GgufValue::Array(_))
    ));
  }
}
