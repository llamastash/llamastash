//! `llamastash start <model> -- <flags>` tail-args parser.
//!
//! Walks tokens left-to-right. A flag any backend declares — by its canonical
//! id, a declared alias (`-ngl`, `-c`), or a concept's neutral spelling — lands
//! on that knob; everything else routes to `extras`. Type / range errors return
//! `USAGE` (64); unrecognised flags route silently.
//!
//! Recognition reads the registry, not one backend's table. It used to read
//! the default backend's alias list alone, so a knob any *other* backend
//! declared had a generated `start` flag that parsed, appeared in `--help`, and
//! then fell through to `extras` — reaching the engine as raw argv while
//! skipping layering, persistence and presets entirely.

use std::ffi::OsString;

use crate::cli::exit_codes::{CliExit, USAGE};
use crate::launch::knobs::{self, KnobDef, KnobKind};

/// Walk `tokens` and split into (knobs, extras). Last-occurrence
/// wins for repeated knob flags.
pub fn parse_tail_args(
  tokens: &[OsString],
) -> Result<(crate::launch::knobs::KnobSet, Vec<OsString>), CliExit> {
  let mut knobs = crate::launch::knobs::KnobSet::new();
  let mut extras: Vec<OsString> = Vec::new();
  let mut iter = tokens.iter().peekable();
  while let Some(tok) = iter.next() {
    let lossy = tok.to_string_lossy().into_owned();
    let (head, inline) = match lossy.split_once('=') {
      Some((h, v)) => (h.to_string(), Some(v.to_string())),
      None => (lossy.clone(), None),
    };
    // Only a flag can name a knob. Without this a bare positional that happens
    // to match a knob id (`threads`) would be swallowed instead of forwarded.
    let Some(def) = head
      .starts_with('-')
      .then(|| knobs::resolve_id(&head).and_then(knobs::def_for))
      .flatten()
    else {
      extras.push(tok.clone());
      continue;
    };
    let value = match def.kind {
      // Booleans default to `true` for a bare flag (`--flash-attn`). The
      // equals-form (`--flash-attn=false`) is honoured so a user override
      // actually disables a knob an inherited layer set to `true`. Space-form
      // is consumed only when the next token is a recognised on/off spelling
      // or the `auto` token (handled here, not by `parse_bool`, since it sets
      // the knob's `KnobValue::Auto` state rather than a boolean) — modern
      // llama-server's `--flash-attn` requires `on|off|auto`, so we mirror
      // that. Anything else stays unconsumed and routes through extras. This
      // fixes the prior `--flash-attn auto` bug where `auto` was left as a
      // dangling positional in extras, producing broken argv.
      KnobKind::Bool => match inline {
        Some(v) => Some(v),
        None => match iter
          .peek()
          .map(|t| t.to_string_lossy().to_ascii_lowercase())
        {
          Some(p) if knobs::parse_bool(&p).is_some() || p == knobs::AUTO_TOKEN => {
            iter.next();
            Some(p)
          }
          _ => None,
        },
      },
      _ => Some(consume_value(&lossy, inline.as_deref(), &mut iter)?),
    };
    apply_knob(&mut knobs, def, value.as_deref(), &lossy)?;
  }
  Ok((knobs, extras))
}

fn consume_value<'a, I>(
  flag: &str,
  inline: Option<&str>,
  iter: &mut std::iter::Peekable<I>,
) -> Result<String, CliExit>
where
  I: Iterator<Item = &'a OsString>,
{
  if let Some(v) = inline {
    return Ok(v.to_string());
  }
  let next = iter.next().ok_or_else(|| {
    CliExit::new(
      USAGE,
      format!("{flag}: missing value (expected an argument)"),
    )
  })?;
  Ok(next.to_string_lossy().into_owned())
}

/// Write one recognised flag's value into the knob set.
///
/// One generic path replaces the per-field match this used to be: the value is
/// parsed against the knob's own declaration, so range checks, closed choice
/// sets and the bool spellings all come from the registry rather than from a
/// hand-maintained arm per knob.
fn apply_knob(
  knobs: &mut crate::launch::knobs::KnobSet,
  def: &'static KnobDef,
  value: Option<&str>,
  flag: &str,
) -> Result<(), CliExit> {
  let id = def.knob_id();
  // The `auto` literal sets the knob's Auto state, on any knob that has one.
  // For the string knobs where `auto` is also a legal upstream value, the knob
  // state wins; to pass a literal `auto` through, use the `--` extras tail.
  if def.has_auto() && value.is_some_and(|v| v.eq_ignore_ascii_case(knobs::AUTO_TOKEN)) {
    knobs.set_auto(id);
    return Ok(());
  }
  let raw = match (def.kind, value) {
    // A bare boolean flag means "on".
    (KnobKind::Bool, None) => "true",
    (_, Some(v)) => v,
    (_, None) => return Err(CliExit::new(USAGE, format!("{flag} needs a value"))),
  };
  match knobs::parse_value(def, raw) {
    Ok(v) => {
      knobs.set(id, v);
      Ok(())
    }
    Err(e) => Err(CliExit::new(USAGE, format!("{flag}: {e}"))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn osvec(args: &[&str]) -> Vec<OsString> {
    args.iter().map(|s| OsString::from(*s)).collect()
  }

  #[test]
  fn happy_path_threads_and_flash_attn() {
    let (knobs, extras) = parse_tail_args(&osvec(&["--threads", "8", "--flash-attn"])).unwrap();
    assert_eq!(knobs.u32(crate::launch::knobs::kid("threads")), Some(8));
    assert_eq!(
      knobs.bool(crate::launch::knobs::kid("flash-attn")),
      Some(true)
    );
    assert!(extras.is_empty());
  }

  #[test]
  fn short_alias_ngl() {
    let (knobs, extras) = parse_tail_args(&osvec(&["-ngl", "99"])).unwrap();
    assert_eq!(
      knobs.u32(crate::launch::knobs::kid("n-gpu-layers")),
      Some(99)
    );
    assert!(extras.is_empty());
  }

  #[test]
  fn n_cpu_moe_parses_canonical_and_alias() {
    let (knobs, extras) = parse_tail_args(&osvec(&["--n-cpu-moe", "12"])).unwrap();
    assert_eq!(knobs.u32(crate::launch::knobs::kid("n-cpu-moe")), Some(12));
    assert!(extras.is_empty());
    let (alias, _) = parse_tail_args(&osvec(&["-ncmoe", "8"])).unwrap();
    assert_eq!(alias.u32(crate::launch::knobs::kid("n-cpu-moe")), Some(8));
  }

  #[test]
  fn placement_knobs_parse_canonical_and_alias() {
    let (k, extras) = parse_tail_args(&osvec(&[
      "--tensor-split",
      "3,1",
      "--main-gpu",
      "0",
      "--split-mode",
      "row",
    ]))
    .unwrap();
    assert_eq!(
      k.str(crate::launch::knobs::kid("tensor-split")),
      Some("3,1")
    );
    assert_eq!(k.u32(crate::launch::knobs::kid("main-gpu")), Some(0));
    assert_eq!(k.str(crate::launch::knobs::kid("split-mode")), Some("row"));
    assert!(extras.is_empty());
    let (alias, _) =
      parse_tail_args(&osvec(&["-ts", "2,1,1", "-mg", "1", "-sm", "layer"])).unwrap();
    assert_eq!(
      alias.str(crate::launch::knobs::kid("tensor-split")),
      Some("2,1,1")
    );
    assert_eq!(alias.u32(crate::launch::knobs::kid("main-gpu")), Some(1));
    assert_eq!(
      alias.str(crate::launch::knobs::kid("split-mode")),
      Some("layer")
    );
  }

  #[test]
  fn split_mode_validates_set() {
    let err = parse_tail_args(&osvec(&["--split-mode", "diagonal"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("none, layer, row"), "{msg}");
  }

  #[test]
  fn tensor_split_rejects_non_numeric() {
    let err = parse_tail_args(&osvec(&["--tensor-split", "3,x"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--tensor-split"), "{msg}");
    // A valid ratio round-trips verbatim.
    let (k, _) = parse_tail_args(&osvec(&["--tensor-split", "0.6,0.4"])).unwrap();
    assert_eq!(
      k.str(crate::launch::knobs::kid("tensor-split")),
      Some("0.6,0.4")
    );
  }

  #[test]
  fn equals_form_parses_identically() {
    let (knobs, _) = parse_tail_args(&osvec(&["--threads=8"])).unwrap();
    assert_eq!(knobs.u32(crate::launch::knobs::kid("threads")), Some(8));
  }

  #[test]
  fn unknown_token_routes_to_extras() {
    let (knobs, extras) = parse_tail_args(&osvec(&["--rope-freq-base", "10000"])).unwrap();
    assert_eq!(knobs, crate::launch::knobs::KnobSet::new());
    assert_eq!(
      extras,
      vec![OsString::from("--rope-freq-base"), OsString::from("10000")]
    );
  }

  #[test]
  fn typed_knob_type_error_returns_usage() {
    let err = parse_tail_args(&osvec(&["--threads", "xyz"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--threads"), "msg should name the flag: {msg}");
    assert!(msg.contains("xyz"), "msg should quote the bad token: {msg}");
  }

  #[test]
  fn missing_value_returns_usage() {
    let err = parse_tail_args(&osvec(&["--n-gpu-layers"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--n-gpu-layers"));
  }

  #[test]
  fn last_occurrence_wins() {
    let (knobs, _) = parse_tail_args(&osvec(&["--threads", "4", "--threads", "16"])).unwrap();
    assert_eq!(knobs.u32(crate::launch::knobs::kid("threads")), Some(16));
  }

  #[test]
  fn boolean_does_not_consume_next_flag() {
    let (knobs, _) = parse_tail_args(&osvec(&["--flash-attn", "--threads", "8"])).unwrap();
    assert_eq!(
      knobs.bool(crate::launch::knobs::kid("flash-attn")),
      Some(true)
    );
    assert_eq!(knobs.u32(crate::launch::knobs::kid("threads")), Some(8));
  }

  #[test]
  fn boolean_space_form_consumes_on_off_value() {
    // Modern llama-server requires `--flash-attn on|off|auto`; the
    // bench harness emits the space form, so the parser must absorb
    // the value rather than leaving it as an orphan positional.
    let (knobs_on, extras_on) = parse_tail_args(&osvec(&["--flash-attn", "on"])).unwrap();
    assert_eq!(
      knobs_on.bool(crate::launch::knobs::kid("flash-attn")),
      Some(true)
    );
    assert!(
      extras_on.is_empty(),
      "`on` must be consumed, not routed to extras: {extras_on:?}"
    );

    let (knobs_off, extras_off) = parse_tail_args(&osvec(&["--flash-attn", "off"])).unwrap();
    assert_eq!(
      knobs_off.bool(crate::launch::knobs::kid("flash-attn")),
      Some(false)
    );
    assert!(extras_off.is_empty());
  }

  #[test]
  fn bool_equals_false_sets_explicit_off() {
    // Lets users override a built-in `Some(true)` from the CLI
    // without having to round-trip through YAML or the TUI.
    let (knobs, extras) = parse_tail_args(&osvec(&["--flash-attn=false"])).unwrap();
    assert_eq!(
      knobs.bool(crate::launch::knobs::kid("flash-attn")),
      Some(false)
    );
    assert!(extras.is_empty());
  }

  #[test]
  fn bool_equals_true_sets_explicit_on() {
    let (knobs, _) = parse_tail_args(&osvec(&["--flash-attn=true"])).unwrap();
    assert_eq!(
      knobs.bool(crate::launch::knobs::kid("flash-attn")),
      Some(true)
    );
  }

  #[test]
  fn bool_accepts_alternate_truthy_falsy_spellings() {
    for spelling in ["1", "on", "yes", "TRUE", "True"] {
      let (knobs, _) = parse_tail_args(&osvec(&[&format!("--mlock={spelling}")])).unwrap();
      assert_eq!(
        knobs.bool(crate::launch::knobs::kid("mlock")),
        Some(true),
        "{spelling:?} should parse to Some(true)"
      );
    }
    for spelling in ["0", "off", "no", "FALSE", "False"] {
      let (knobs, _) = parse_tail_args(&osvec(&[&format!("--mlock={spelling}")])).unwrap();
      assert_eq!(
        knobs.bool(crate::launch::knobs::kid("mlock")),
        Some(false),
        "{spelling:?} should parse to Some(false)"
      );
    }
  }

  #[test]
  fn bool_rejects_garbage_value_with_usage_and_named_flag() {
    let err = parse_tail_args(&osvec(&["--flash-attn=maybe"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    let msg = err.to_string();
    assert!(msg.contains("--flash-attn"), "msg must name flag: {msg}");
    assert!(msg.contains("maybe"), "msg must quote value: {msg}");
  }

  #[test]
  fn cache_type_k_validates_set() {
    // Every standard llama-server type, plus custom identifiers from
    // modified builds, parse through to the typed slot unchanged.
    for t in [
      "f32",
      "f16",
      "bf16",
      "q8_0",
      "q4_0",
      "q4_1",
      "iq4_nl",
      "q5_0",
      "q5_1",
      "fp4",
      "turbo_quant",
      "myfmt0",
    ] {
      let (parsed, _) = parse_tail_args(&osvec(&["--cache-type-k", t])).expect(t);
      assert_eq!(
        parsed.str(crate::launch::knobs::kid("cache-type-k")),
        Some(t)
      );
    }
    // Identifiers that can't name a type (leading digit, embedded space)
    // are still rejected with a USAGE error that lists the known set.
    let err = parse_tail_args(&osvec(&["--cache-type-k", "4bad"])).unwrap_err();
    assert_eq!(err.code, USAGE);
    assert!(err.to_string().contains("f16, bf16, q8_0"), "{err}");
    assert_eq!(
      parse_tail_args(&osvec(&["--cache-type-k", "bad type"]))
        .unwrap_err()
        .code,
      USAGE
    );
  }

  #[test]
  fn rope_freq_scale_accepts_float() {
    let (knobs, _) = parse_tail_args(&osvec(&["--rope-freq-scale", "0.5"])).unwrap();
    assert_eq!(
      knobs.f32(crate::launch::knobs::kid("rope-freq-scale")),
      Some(0.5)
    );
  }

  #[test]
  fn mixed_knobs_and_extras() {
    let (knobs, extras) = parse_tail_args(&osvec(&[
      "--threads",
      "8",
      "--rope-freq-base",
      "10000",
      "-ngl",
      "99",
    ]))
    .unwrap();
    assert_eq!(knobs.u32(crate::launch::knobs::kid("threads")), Some(8));
    assert_eq!(
      knobs.u32(crate::launch::knobs::kid("n-gpu-layers")),
      Some(99)
    );
    assert_eq!(
      extras,
      vec![OsString::from("--rope-freq-base"), OsString::from("10000")]
    );
  }
  /// Recognition reads the registry, so a knob any backend declares lands on
  /// the knob rather than in `extras`.
  ///
  /// It read the default backend's alias table alone before, so every knob
  /// another backend declared had a generated `start` flag that parsed,
  /// appeared in `--help`, and then fell through to the raw argv tail — no
  /// layering, no persistence into `last_params`, and nothing for
  /// `presets save` to record.
  #[test]
  fn a_knob_from_any_backend_lands_on_the_knob_not_in_extras() {
    let home = crate::backend::DEFAULT_BACKEND_ID;
    for (backend, def) in crate::launch::knobs::registry::iter() {
      if backend == home {
        continue;
      }
      let raw = match def.kind {
        KnobKind::Bool => "true".to_string(),
        KnobKind::U32 { .. } => "4".to_string(),
        KnobKind::F32 { min, max } => {
          let lo = min.unwrap_or(0.0);
          format!("{}", max.map_or(lo + 1.0, |hi| (lo + hi) / 2.0))
        }
        KnobKind::Ratio => "3,1".to_string(),
        KnobKind::Enum { choices } | KnobKind::OpenEnum { choices, .. } => choices[0].to_string(),
        KnobKind::Str => "x".to_string(),
      };
      // The CLI spells a knob by its **id**, not by the flag the engine takes:
      // two knobs may share an engine flag (a sidecar path and its on/off
      // enable), and only the ids are unique.
      let token = OsString::from(format!("--{}={raw}", def.id));
      let (knobs, extras) = parse_tail_args(&[token]).expect("parse");
      assert!(
        extras.is_empty(),
        "{backend}'s `{}` fell through to extras: {extras:?}",
        def.id
      );
      // The id it lands on is its own, or a sibling carrying the same concept.
      // At parse time the serving backend isn't known yet, so `--ctx` means
      // "the context window" and stores under whichever knob declares that
      // concept; `resolve_layered` re-keys it into the serving backend's own
      // spelling at launch.
      let stored: Vec<_> = knobs.iter().map(|(id, _)| id).collect();
      let ok = stored.iter().any(|id| {
        *id == def.knob_id()
          || (def.concept.is_some()
            && crate::launch::knobs::def_for(*id).and_then(|d| d.concept) == def.concept)
      });
      assert!(
        ok,
        "{backend}'s `{}` parsed but stored nothing that reaches it: {stored:?}",
        def.id
      );
    }
  }
}
