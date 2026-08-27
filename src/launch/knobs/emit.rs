//! Argv emission from declared knobs.
//!
//! One generic emitter replaces the per-backend hand-written translators (the
//! llama.cpp `argvify` match and `native_knobs::translate`), and applies the
//! loopback/credential denylist to *every* channel rather than each one
//! carrying its own guard.
//!
//! Emission order is the backend's declaration order, which is why the
//! declarations are written in the engine's canonical flag order: argv diffs
//! stay readable across releases.

use std::ffi::OsString;

use super::def::{Emit, KnobDef};
use super::registry;
use super::value::{KnobSet, KnobValue};
use crate::launch::params::is_forbidden_head_ext;

/// Emit `knobs` as argv for `backend_id`.
///
/// - [`Emit::Custom`] knobs emit nothing here: the backend consumes them
///   itself (a value that expands into a flag bundle, or a non-argv transport
///   such as an HTTP load request).
/// - `Auto` and unset emit nothing — an `Auto` knob is precisely the request
///   to let the engine decide, and an unset one falls through to its default.
/// - An empty string value emits nothing, matching the "empty selector means
///   auto-select" rule the device knob has always had.
///
/// `extra_forbidden` carries the backend's own network-affecting heads on top
/// of the base denylist, so a value can never rebind the listener off loopback
/// or weaken the network posture.
pub fn emit_argv(backend_id: &str, knobs: &KnobSet, extra_forbidden: &[&str]) -> Vec<OsString> {
  let mut out = Vec::new();
  for def in registry::for_backend(backend_id) {
    let Some(KnobValue::Set(scalar)) = knobs.get(def.knob_id()) else {
      continue;
    };
    let flag = def.emit_flag();
    match def.emit {
      Emit::Custom => continue,
      Emit::BareFlagWhenTrue => {
        if scalar.as_bool() == Some(true) {
          push(&mut out, &flag, None, extra_forbidden, def);
        }
      }
      Emit::FlagOnOff => match scalar.as_bool() {
        Some(true) => push(&mut out, &flag, Some("on"), extra_forbidden, def),
        Some(false) => push(&mut out, &flag, Some("off"), extra_forbidden, def),
        None => {}
      },
      Emit::FlagValue => {
        let value = scalar.to_arg();
        if !value.is_empty() {
          push(&mut out, &flag, Some(&value), extra_forbidden, def);
        }
      }
    }
  }
  out
}

/// Push `flag` (+ optional value) unless either head hits the denylist.
///
/// A knob's *flag* is registry-validated at build time, so only a free-text
/// *value* can realistically trip this — a `--device` selector or a path knob
/// carrying something like `--api-key`. Dropped and logged rather than
/// emitted, matching what the pre-registry native-knob translator did.
fn push(
  out: &mut Vec<OsString>,
  flag: &str,
  value: Option<&str>,
  extra_forbidden: &[&str],
  def: &KnobDef,
) {
  if is_forbidden_head_ext(flag, extra_forbidden) {
    log::warn!("knob `{}`: refusing to emit denylisted flag {flag:?}", def.id);
    return;
  }
  if let Some(v) = value {
    // Scan every whitespace-separated token, not just the leading one: a
    // free-text value like `/x --cors` would otherwise smuggle a denylisted
    // flag through as one argv element the engine still splits on.
    let smuggles = v.split_whitespace().any(|tok| {
      let head = tok.split('=').next().unwrap_or(tok);
      head.starts_with('-') && is_forbidden_head_ext(head, extra_forbidden)
    });
    if smuggles {
      log::warn!("knob `{}`: refusing denylisted value", def.id);
      return;
    }
    out.push(OsString::from(flag));
    out.push(OsString::from(v));
  } else {
    out.push(OsString::from(flag));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::knobs::value::Scalar;
  use crate::launch::knobs::KnobId;

  fn backend() -> &'static str {
    crate::backend::DEFAULT_BACKEND_ID
  }

  fn id(name: &'static str) -> KnobId {
    registry::resolve_id(name).unwrap_or_else(|| panic!("no knob `{name}`"))
  }

  fn argv(knobs: &KnobSet) -> Vec<String> {
    emit_argv(backend(), knobs, &[])
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  #[test]
  fn auto_and_unset_emit_nothing() {
    let mut k = KnobSet::new();
    k.set_auto(id("n-gpu-layers"));
    assert!(argv(&k).is_empty(), "Auto delegates, so no flag is emitted");
    assert!(argv(&KnobSet::new()).is_empty());
  }

  #[test]
  fn a_valued_knob_emits_flag_then_value() {
    let mut k = KnobSet::new();
    k.set_scalar(id("threads"), Scalar::U32(8));
    assert_eq!(argv(&k), vec!["--threads", "8"]);
  }

  #[test]
  fn bare_bool_emits_only_when_true() {
    let mut on = KnobSet::new();
    on.set_scalar(id("mlock"), Scalar::Bool(true));
    assert_eq!(argv(&on), vec!["--mlock"]);

    let mut off = KnobSet::new();
    off.set_scalar(id("mlock"), Scalar::Bool(false));
    assert!(argv(&off).is_empty(), "false emits no --no-flag form");
  }

  /// Regression: modern llama-server rejects a bare `--flash-attn` and parses
  /// the next argv entry as its value, so this knob must always carry one.
  #[test]
  fn flash_attn_always_carries_an_explicit_value() {
    let mut on = KnobSet::new();
    on.set_scalar(id("flash-attn"), Scalar::Bool(true));
    assert_eq!(argv(&on), vec!["--flash-attn", "on"]);

    let mut off = KnobSet::new();
    off.set_scalar(id("flash-attn"), Scalar::Bool(false));
    assert_eq!(argv(&off), vec!["--flash-attn", "off"]);
  }

  #[test]
  fn a_whole_float_keeps_one_decimal_place() {
    let mut k = KnobSet::new();
    k.set_scalar(id("rope-freq-scale"), Scalar::F32(1.0));
    assert_eq!(argv(&k), vec!["--rope-freq-scale", "1.0"]);
  }

  #[test]
  fn an_empty_string_value_emits_nothing() {
    // An empty device selector has always meant "auto-select".
    let mut k = KnobSet::new();
    k.set_scalar(id("device"), Scalar::Str(String::new()));
    assert!(argv(&k).is_empty());
  }

  #[test]
  fn custom_emitters_are_left_to_the_backend() {
    // The context knob expands into the engine's own flag plus fit handling,
    // so the generic emitter must not emit it.
    let mut k = KnobSet::new();
    k.set_scalar(id("ctx-size"), Scalar::U32(4096));
    assert!(
      argv(&k).is_empty(),
      "Emit::Custom knobs are the backend's business"
    );
  }

  #[test]
  fn emission_follows_declaration_order() {
    let mut k = KnobSet::new();
    k.set_scalar(id("keep"), Scalar::U32(64));
    k.set_scalar(id("n-gpu-layers"), Scalar::U32(99));
    k.set_scalar(id("threads"), Scalar::U32(8));
    // Declaration order is offload → throughput → advanced, regardless of the
    // order the caller happened to set them in.
    assert_eq!(
      argv(&k),
      vec!["--n-gpu-layers", "99", "--threads", "8", "--keep", "64"]
    );
  }

  #[test]
  fn a_denylisted_value_is_dropped_not_emitted() {
    let mut k = KnobSet::new();
    k.set_scalar(id("device"), Scalar::Str("--api-key=leak".into()));
    assert!(
      argv(&k).is_empty(),
      "a value that looks like a denylisted flag never reaches the engine"
    );
  }
}
