//! Knob *values* — the tri-state a slot holds, and the map a launch carries.
//!
//! Replaces the split between `TypedKnobs` (statically-typed llama.cpp IR) and
//! `backend_knobs` (stringly-typed per-backend map): one map, real types, one
//! layering engine.

use std::collections::BTreeMap;

use super::def::{KnobDef, KnobId, KnobKind};

/// A concrete knob value. The variant is fixed by the knob's
/// [`KnobKind`] — a parse that disagrees is rejected,
/// so a stored `Scalar` always matches its descriptor.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
  U32(u32),
  F32(f32),
  Bool(bool),
  Str(String),
}

impl Scalar {
  pub fn as_u32(&self) -> Option<u32> {
    match self {
      Scalar::U32(v) => Some(*v),
      _ => None,
    }
  }

  pub fn as_f32(&self) -> Option<f32> {
    match self {
      Scalar::F32(v) => Some(*v),
      _ => None,
    }
  }

  pub fn as_bool(&self) -> Option<bool> {
    match self {
      Scalar::Bool(v) => Some(*v),
      _ => None,
    }
  }

  pub fn as_str(&self) -> Option<&str> {
    match self {
      Scalar::Str(v) => Some(v.as_str()),
      _ => None,
    }
  }

  /// The value as the engine would receive it on a command line.
  pub fn to_arg(&self) -> String {
    match self {
      Scalar::U32(v) => v.to_string(),
      Scalar::F32(v) => v.to_string(),
      Scalar::Bool(v) => v.to_string(),
      Scalar::Str(v) => v.clone(),
    }
  }
}

/// One slot's state. The third state — *inherited* — is the absence of an
/// entry in the [`KnobSet`], which the layered resolver fills from the next
/// layer down or leaves to the engine's own default.
///
/// What `Auto` *means* is per-knob ([`AutoKind`](super::def::AutoKind)), not
/// globally "delegate to `--fit`".
#[derive(Clone, Debug, PartialEq)]
pub enum KnobValue {
  Set(Scalar),
  Auto,
}

impl KnobValue {
  pub fn is_auto(&self) -> bool {
    matches!(self, KnobValue::Auto)
  }

  pub fn set_value(&self) -> Option<&Scalar> {
    match self {
      KnobValue::Set(s) => Some(s),
      KnobValue::Auto => None,
    }
  }
}

/// The bare token denoting [`KnobValue::Auto`] in YAML and JSON.
pub const AUTO_TOKEN: &str = "auto";

/// A launch's knob values, keyed by descriptor id.
///
/// Only registry-known ids can be keys ([`KnobId`] borrows a `&'static str`
/// from a declaration), so an unrecognised config key is reported at parse
/// time instead of being stored as an orphan nobody reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KnobSet {
  values: BTreeMap<KnobId, KnobValue>,
}

impl KnobSet {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn is_empty(&self) -> bool {
    self.values.is_empty()
  }

  pub fn len(&self) -> usize {
    self.values.len()
  }

  pub fn get(&self, id: KnobId) -> Option<&KnobValue> {
    self.values.get(&id)
  }

  pub fn contains(&self, id: KnobId) -> bool {
    self.values.contains_key(&id)
  }

  pub fn set(&mut self, id: KnobId, value: KnobValue) {
    self.values.insert(id, value);
  }

  pub fn set_scalar(&mut self, id: KnobId, value: Scalar) {
    self.values.insert(id, KnobValue::Set(value));
  }

  pub fn set_auto(&mut self, id: KnobId) {
    self.values.insert(id, KnobValue::Auto);
  }

  /// Drop the slot back to inherited.
  pub fn clear(&mut self, id: KnobId) -> Option<KnobValue> {
    self.values.remove(&id)
  }

  pub fn iter(&self) -> impl Iterator<Item = (KnobId, &KnobValue)> {
    self.values.iter().map(|(k, v)| (*k, v))
  }

  pub fn ids(&self) -> impl Iterator<Item = KnobId> + '_ {
    self.values.keys().copied()
  }

  /// Concrete value for `id` when it is `Set` and the kind matches.
  pub fn u32(&self, id: KnobId) -> Option<u32> {
    self.get(id)?.set_value()?.as_u32()
  }

  pub fn f32(&self, id: KnobId) -> Option<f32> {
    self.get(id)?.set_value()?.as_f32()
  }

  pub fn bool(&self, id: KnobId) -> Option<bool> {
    self.get(id)?.set_value()?.as_bool()
  }

  pub fn str(&self, id: KnobId) -> Option<&str> {
    self.get(id)?.set_value()?.as_str()
  }

  /// Whether `id` is explicitly delegated to launch-time resolution.
  pub fn is_auto(&self, id: KnobId) -> bool {
    self.get(id).is_some_and(KnobValue::is_auto)
  }

  /// Write every slot from `other` that this set does not already hold.
  /// The per-field merge the layered resolver runs, and the "overlay CLI
  /// overrides onto a preset baseline" the `start` handler needs — a set knob
  /// is never clobbered by a lower layer.
  pub fn fill_from(&mut self, other: &KnobSet) {
    for (id, value) in other.iter() {
      self.values.entry(id).or_insert_with(|| value.clone());
    }
  }

  /// Write every slot from `other`, replacing what is already here.
  /// The "these overrides win" direction.
  pub fn overlay(&mut self, other: &KnobSet) {
    for (id, value) in other.iter() {
      self.values.insert(id, value.clone());
    }
  }

  /// Keep only the slots `keep` accepts. Used to drop knobs the resolved
  /// backend does not declare, which R6 requires be dropped and surfaced
  /// rather than silently emitted or hard-failed.
  pub fn retain_ids(&mut self, keep: impl Fn(KnobId) -> bool) -> Vec<KnobId> {
    let dropped: Vec<KnobId> = self.ids().filter(|id| !keep(*id)).collect();
    for id in &dropped {
      self.values.remove(id);
    }
    dropped
  }
}

impl FromIterator<(KnobId, KnobValue)> for KnobSet {
  fn from_iter<T: IntoIterator<Item = (KnobId, KnobValue)>>(iter: T) -> Self {
    Self {
      values: iter.into_iter().collect(),
    }
  }
}

/// Why a value could not be accepted for a knob. Carries the knob id so every
/// surface can render the same message with its own framing (a clap `USAGE`
/// error, a TUI inline row error, a config-load warning).
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
  NotAnInteger {
    id: KnobId,
    value: String,
  },
  NotAFloat {
    id: KnobId,
    value: String,
  },
  NotABool {
    id: KnobId,
    value: String,
  },
  OutOfRange {
    id: KnobId,
    value: String,
    bound: String,
  },
  NotAChoice {
    id: KnobId,
    value: String,
    choices: &'static [&'static str],
  },
}

impl std::fmt::Display for ParseError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ParseError::NotAnInteger { id, value } => {
        write!(f, "{id}: `{value}` is not a whole number")
      }
      ParseError::NotAFloat { id, value } => write!(f, "{id}: `{value}` is not a number"),
      ParseError::NotABool { id, value } => {
        write!(
          f,
          "{id}: `{value}` is not a boolean (try on/off, true/false)"
        )
      }
      ParseError::OutOfRange { id, value, bound } => {
        write!(f, "{id}: `{value}` is out of range ({bound})")
      }
      ParseError::NotAChoice { id, value, choices } => {
        write!(f, "{id}: `{value}` is not one of {}", choices.join(", "))
      }
    }
  }
}

impl std::error::Error for ParseError {}

/// Accepted spellings for a boolean knob value. Matches what the CLI tail
/// parser has always taken, so `--flash-attn off` keeps working.
fn parse_bool(s: &str) -> Option<bool> {
  match s.to_ascii_lowercase().as_str() {
    "true" | "on" | "yes" | "1" => Some(true),
    "false" | "off" | "no" | "0" => Some(false),
    _ => None,
  }
}

/// Parse one raw string into this knob's value, honouring the reserved
/// `auto` token when the knob has an `Auto` state.
///
/// A knob with no `Auto` state treats `auto` as an ordinary value, so a string
/// knob whose real value is `"auto"` needs no escape — the ambiguity only
/// exists where an `Auto` state exists, and there the `{ value: auto }` config
/// escape covers it.
pub fn parse_value(def: &KnobDef, raw: &str) -> Result<KnobValue, ParseError> {
  if def.has_auto() && raw.eq_ignore_ascii_case(AUTO_TOKEN) {
    return Ok(KnobValue::Auto);
  }
  let id = def.knob_id();
  let scalar = match def.kind {
    KnobKind::U32 { max } => {
      let v: u32 = raw.trim().parse().map_err(|_| ParseError::NotAnInteger {
        id,
        value: raw.to_string(),
      })?;
      if let Some(m) = max {
        if v > m {
          return Err(ParseError::OutOfRange {
            id,
            value: raw.to_string(),
            bound: format!("max {m}"),
          });
        }
      }
      Scalar::U32(v)
    }
    KnobKind::F32 { min, max } => {
      let v: f32 = raw.trim().parse().map_err(|_| ParseError::NotAFloat {
        id,
        value: raw.to_string(),
      })?;
      if let Some(lo) = min {
        if v < lo {
          return Err(ParseError::OutOfRange {
            id,
            value: raw.to_string(),
            bound: format!("min {lo}"),
          });
        }
      }
      if let Some(hi) = max {
        if v > hi {
          return Err(ParseError::OutOfRange {
            id,
            value: raw.to_string(),
            bound: format!("max {hi}"),
          });
        }
      }
      Scalar::F32(v)
    }
    KnobKind::Bool => Scalar::Bool(parse_bool(raw).ok_or_else(|| ParseError::NotABool {
      id,
      value: raw.to_string(),
    })?),
    KnobKind::Enum { choices } => {
      let hit =
        choices
          .iter()
          .find(|c| c.eq_ignore_ascii_case(raw))
          .ok_or(ParseError::NotAChoice {
            id,
            value: raw.to_string(),
            choices,
          })?;
      Scalar::Str((*hit).to_string())
    }
    // Deliberately permissive: a custom engine build may accept values we
    // cannot enumerate, so the ring is a hint, never a gate.
    KnobKind::OpenEnum { .. } | KnobKind::Str => Scalar::Str(raw.to_string()),
  };
  Ok(KnobValue::Set(scalar))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::knobs::def::{AutoKind, Emit, Group};

  fn def(kind: KnobKind, auto: Option<AutoKind>) -> KnobDef {
    KnobDef {
      id: "test-knob",
      flag: None,
      concept: None,
      kind,
      auto,
      group: Group::Advanced,
      label: "Test",
      help: "test knob",
      aliases: &[],
      fallback: crate::launch::params::LayerLabel::ServerDefault,
      emit: Emit::FlagValue,
    }
  }

  #[test]
  fn auto_token_only_wins_when_the_knob_has_an_auto_state() {
    let with = def(KnobKind::Str, Some(AutoKind::Delegate));
    assert_eq!(parse_value(&with, "auto").unwrap(), KnobValue::Auto);
    // No Auto state → `auto` is just a string, no escape needed.
    let without = def(KnobKind::Str, None);
    assert_eq!(
      parse_value(&without, "auto").unwrap(),
      KnobValue::Set(Scalar::Str("auto".into()))
    );
  }

  #[test]
  fn u32_max_is_enforced() {
    let d = def(KnobKind::U32 { max: Some(10) }, None);
    assert!(parse_value(&d, "10").is_ok());
    assert!(matches!(
      parse_value(&d, "11"),
      Err(ParseError::OutOfRange { .. })
    ));
  }

  #[test]
  fn bool_accepts_the_cli_spellings() {
    let d = def(KnobKind::Bool, None);
    for (raw, want) in [("on", true), ("off", false), ("TRUE", true), ("0", false)] {
      assert_eq!(
        parse_value(&d, raw).unwrap(),
        KnobValue::Set(Scalar::Bool(want)),
        "{raw}"
      );
    }
    assert!(matches!(
      parse_value(&d, "maybe"),
      Err(ParseError::NotABool { .. })
    ));
  }

  #[test]
  fn closed_enum_rejects_and_open_enum_accepts_an_unlisted_value() {
    let closed = def(
      KnobKind::Enum {
        choices: &["a", "b"],
      },
      None,
    );
    assert!(matches!(
      parse_value(&closed, "z"),
      Err(ParseError::NotAChoice { .. })
    ));
    let open = def(
      KnobKind::OpenEnum {
        choices: &["a", "b"],
      },
      None,
    );
    assert_eq!(
      parse_value(&open, "z").unwrap(),
      KnobValue::Set(Scalar::Str("z".into()))
    );
  }

  #[test]
  fn fill_from_never_clobbers_and_overlay_always_does() {
    let a = KnobId("a");
    let b = KnobId("b");
    let mut base = KnobSet::new();
    base.set_scalar(a, Scalar::U32(1));

    let mut lower = KnobSet::new();
    lower.set_scalar(a, Scalar::U32(9));
    lower.set_scalar(b, Scalar::U32(2));

    let mut filled = base.clone();
    filled.fill_from(&lower);
    assert_eq!(filled.u32(a), Some(1), "set slot survives a lower layer");
    assert_eq!(filled.u32(b), Some(2), "unset slot is filled");

    let mut overlaid = base.clone();
    overlaid.overlay(&lower);
    assert_eq!(overlaid.u32(a), Some(9), "overlay wins");
  }

  #[test]
  fn retain_ids_reports_what_it_dropped() {
    let keep = KnobId("keep");
    let drop = KnobId("drop");
    let mut set = KnobSet::new();
    set.set_scalar(keep, Scalar::U32(1));
    set.set_scalar(drop, Scalar::U32(2));
    let dropped = set.retain_ids(|id| id == keep);
    assert_eq!(dropped, vec![drop]);
    assert!(set.contains(keep) && !set.contains(drop));
  }
}
