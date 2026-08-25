//! `KnobSet` serialisation, shared by `config.yaml`, `state.json` and the wire.
//!
//! A knob set is a flat map of `id: value`, where the value is the bare scalar
//! the engine would take. The reserved token `auto` denotes
//! [`KnobValue::Auto`]; a knob whose literal value really is the string
//! `"auto"` uses the `{ value: auto }` escape, exactly as before.
//!
//! **Keys are resolved through the registry**, which is what turns a typo into
//! a warning naming the key. That is a strict improvement on the shape this
//! replaces: `PresetBody.knobs` was `#[serde(flatten)]`, and flatten cannot
//! carry `deny_unknown_fields`, so a misspelled knob was dropped with no error
//! at all.
//!
//! Keys normalise `_` to `-`, so a config written either way loads.

use std::collections::BTreeMap;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::def::KnobDef;
use super::registry;
use super::value::{parse_value, KnobSet, KnobValue, Scalar, AUTO_TOKEN};

/// One value as it arrives from YAML or JSON, before the registry says what
/// kind it should be. Untagged, so the variant order is the coercion
/// preference: a bare `true` is a bool, `8` an integer, `0.5` a float.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawValue {
  Bool(bool),
  U64(u64),
  I64(i64),
  F64(f64),
  Str(String),
  /// The `{ value: … }` escape, the only way to pin the literal string
  /// `"auto"` on a knob that has an Auto state — and the legacy
  /// `{ auto: true }` sentinel, whose value is a bool rather than a string.
  Escape(BTreeMap<String, EscapeVal>),
}

/// A value inside the `{ … }` escape form. Kept separate from [`RawValue`] so
/// the escape stays one level deep (no recursion, no boxing) while still
/// admitting the legacy sentinel's `true`.
#[derive(Deserialize, Clone, PartialEq)]
#[serde(untagged)]
enum EscapeVal {
  Bool(bool),
  U64(u64),
  Str(String),
}

impl EscapeVal {
  fn as_text(&self) -> String {
    match self {
      EscapeVal::Bool(b) => b.to_string(),
      EscapeVal::U64(n) => n.to_string(),
      EscapeVal::Str(s) => s.clone(),
    }
  }
}

impl RawValue {
  /// The value as a string, which is what `parse_value` validates against the
  /// knob's declared kind. Going through one text form keeps YAML and JSON
  /// agreeing on every edge (`8` vs `"8"`, `on` vs `true`).
  fn as_text(&self) -> Option<String> {
    match self {
      RawValue::Bool(b) => Some(b.to_string()),
      RawValue::U64(n) => Some(n.to_string()),
      RawValue::I64(n) => Some(n.to_string()),
      RawValue::F64(f) => Some(f.to_string()),
      RawValue::Str(s) => Some(s.clone()),
      RawValue::Escape(_) => None,
    }
  }

  /// The escaped literal, when this is the `{ value: … }` form.
  fn escaped(&self) -> Option<String> {
    match self {
      RawValue::Escape(m) => m.get("value").map(EscapeVal::as_text),
      _ => None,
    }
  }

  /// The pre-bare-token `{ auto: true }` sentinel, which means the Auto state
  /// rather than a literal value.
  fn is_legacy_auto_sentinel(&self) -> bool {
    matches!(self, RawValue::Escape(m) if m.get("auto") == Some(&EscapeVal::Bool(true)))
  }
}

/// Convert one raw entry into a typed value for `def`, or `None` with a
/// warning when it cannot be represented.
fn coerce(def: &KnobDef, raw: &RawValue) -> Option<KnobValue> {
  if raw.is_legacy_auto_sentinel() {
    return def.has_auto().then_some(KnobValue::Auto);
  }
  // `{ value: auto }` always means the literal, never the Auto state.
  if let Some(literal) = raw.escaped() {
    return Some(KnobValue::Set(Scalar::Str(literal)));
  }
  let text = raw.as_text()?;
  match parse_value(def, &text) {
    Ok(v) => Some(v),
    Err(e) => {
      log::warn!("knob `{}`: {e}; ignoring", def.id);
      None
    }
  }
}

impl Serialize for KnobSet {
  fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
    let mut map = ser.serialize_map(Some(self.len()))?;
    for (id, value) in self.iter() {
      match value {
        KnobValue::Auto => map.serialize_entry(id.as_str(), AUTO_TOKEN)?,
        KnobValue::Set(scalar) => match scalar {
          // A literal "auto" would read back as the Auto state, so it takes
          // the escape on the way out too.
          Scalar::Str(s) if s == AUTO_TOKEN => {
            let escape = BTreeMap::from([("value", AUTO_TOKEN)]);
            map.serialize_entry(id.as_str(), &escape)?;
          }
          Scalar::U32(v) => map.serialize_entry(id.as_str(), v)?,
          Scalar::F32(v) => map.serialize_entry(id.as_str(), v)?,
          Scalar::Bool(v) => map.serialize_entry(id.as_str(), v)?,
          Scalar::Str(v) => map.serialize_entry(id.as_str(), v)?,
        },
      }
    }
    map.end()
  }
}

struct KnobSetVisitor;

impl<'de> Visitor<'de> for KnobSetVisitor {
  type Value = KnobSet;

  fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("a map of knob id to value")
  }

  fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<KnobSet, M::Error> {
    let mut set = KnobSet::new();
    while let Some((key, raw)) = access.next_entry::<String, RawValue>()? {
      let Some(id) = registry::resolve_id(&key) else {
        log::warn!("unknown knob `{key}`; ignoring (no backend declares it)");
        continue;
      };
      let Some(def) = registry::def_for(id) else {
        continue;
      };
      if let Some(value) = coerce(def, &raw) {
        set.set(id, value);
      }
    }
    Ok(set)
  }
}

impl<'de> Deserialize<'de> for KnobSet {
  fn deserialize<D: Deserializer<'de>>(de: D) -> Result<KnobSet, D::Error> {
    de.deserialize_map(KnobSetVisitor)
  }
}

/// Fold a pre-registry preset entry into a knob map (plan D10).
///
/// The old shape kept typed knobs flattened at the entry level, native knobs
/// in a `backend_knobs:` sub-map, and `mode` / `mtp` / `mtp_draft_n` as
/// siblings. All four are knobs now, so they belong in one map.
///
/// `reserved` names the keys that are *not* knobs and must be left alone.
/// Deleted in stage 7 once configs have moved; it never writes, so comments in
/// a hand-authored file are safe by construction.
pub fn fold_legacy_entry(
  entry: &BTreeMap<String, yaml_serde::Value>,
  reserved: &[&str],
) -> KnobSet {
  let mut set = KnobSet::new();
  let mut ingest = |key: &str, raw: &yaml_serde::Value| {
    if reserved.contains(&key) {
      return;
    }
    let Some(id) = registry::resolve_id(key) else {
      log::warn!("unknown knob `{key}` in preset; ignoring");
      return;
    };
    let Some(def) = registry::def_for(id) else {
      return;
    };
    if let Ok(raw) = yaml_serde::from_value::<RawValue>(raw.clone()) {
      if let Some(v) = coerce(def, &raw) {
        set.set(id, v);
      }
    }
  };
  for (key, raw) in entry {
    if key == "backend_knobs" {
      if let Some(nested) = raw.as_mapping() {
        for (nk, nv) in nested {
          if let Some(nk) = nk.as_str() {
            ingest(nk, nv);
          }
        }
      }
      continue;
    }
    ingest(key, raw);
  }
  set
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::knobs::KnobId;

  fn id(name: &str) -> KnobId {
    registry::resolve_id(name).unwrap_or_else(|| panic!("no knob `{name}`"))
  }

  fn yaml(s: &str) -> KnobSet {
    yaml_serde::from_str(s).expect("parse")
  }

  #[test]
  fn scalars_round_trip_through_yaml() {
    let set = yaml("ctx-size: 65536\nflash-attn: true\nrope-freq-scale: 0.5\n");
    assert_eq!(set.u32(id("ctx-size")), Some(65536));
    assert_eq!(set.bool(id("flash-attn")), Some(true));
    assert_eq!(set.f32(id("rope-freq-scale")), Some(0.5));

    let text = yaml_serde::to_string(&set).unwrap();
    assert_eq!(yaml(&text), set, "a round trip is lossless");
  }

  #[test]
  fn underscore_and_dash_spellings_both_load() {
    let a = yaml("n_gpu_layers: 99\n");
    let b = yaml("n-gpu-layers: 99\n");
    assert_eq!(a, b);
    assert_eq!(a.u32(id("n-gpu-layers")), Some(99));
  }

  #[test]
  fn the_bare_auto_token_is_the_auto_state() {
    let set = yaml("n-gpu-layers: auto\n");
    assert!(set.is_auto(id("n-gpu-layers")));
    // …and survives a round trip as the bare token, not as a string value.
    let text = yaml_serde::to_string(&set).unwrap();
    assert!(text.contains("auto"), "{text}");
    assert!(yaml(&text).is_auto(id("n-gpu-layers")));
  }

  #[test]
  fn the_escape_pins_a_literal_auto_string() {
    let set = yaml("cache-type-k:\n  value: auto\n");
    assert_eq!(set.str(id("cache-type-k")), Some("auto"));
    assert!(!set.is_auto(id("cache-type-k")));
    assert_eq!(
      yaml(&yaml_serde::to_string(&set).unwrap()),
      set,
      "the escape survives a round trip"
    );
  }

  #[test]
  fn the_legacy_auto_sentinel_still_reads() {
    // Pre-bare-token configs wrote `{ auto: true }`; they must keep loading.
    let set = yaml("n-gpu-layers:\n  auto: true\n");
    assert!(set.is_auto(id("n-gpu-layers")));
  }

  #[test]
  fn an_unknown_key_is_dropped_rather_than_failing_the_load() {
    let set = yaml("definitely-not-a-knob: 5\nthreads: 4\n");
    assert_eq!(set.len(), 1, "the good knob survives");
    assert_eq!(set.u32(id("threads")), Some(4));
  }

  #[test]
  fn a_bad_value_is_dropped_rather_than_failing_the_load() {
    let set = yaml("threads: not-a-number\nkeep: 8\n");
    assert_eq!(set.u32(id("threads")), None);
    assert_eq!(set.u32(id("keep")), Some(8));
  }

  #[test]
  fn a_legacy_entry_folds_flat_native_and_sibling_keys_into_one_map() {
    let entry: BTreeMap<String, yaml_serde::Value> = yaml_serde::from_str(
      "ctx: 4096\nflash_attn: true\nmode: embedding\nmtp: off\n\
       extras:\n  - --rope-freq-base\nbackend_knobs:\n  ssd_streaming: \"false\"\n",
    )
    .unwrap();
    let set = fold_legacy_entry(&entry, &["extras", "backend", "server", "default"]);
    assert_eq!(set.u32(id("ctx-size")), Some(4096), "ctx alias resolves");
    assert_eq!(set.bool(id("flash-attn")), Some(true));
    assert_eq!(set.str(id("mode")), Some("embedding"), "mode is a knob now");
    assert_eq!(set.bool(id("mtp")), Some(false), "off parses as a bool");
    assert_eq!(
      set.bool(id("ssd-streaming")),
      Some(false),
      "native knobs come up out of backend_knobs"
    );
    assert!(
      !set.contains(id("ctx-size")) || !set.iter().any(|(k, _)| k.as_str() == "extras"),
      "reserved keys are never treated as knobs"
    );
  }
}

