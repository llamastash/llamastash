//! First-class `start` flags, generated from the knob registry.
//!
//! Every knob any backend declares becomes a real `start --<flag>`, grouped in
//! `--help` under the backend that declares it. Adding a knob to a backend adds
//! its flag and its help line with no edit here — which is the whole reason the
//! CLI can no longer drift from the editor.
//!
//! The flag set is the **union across every compiled-in backend**, not just the
//! ones installed on this host: a script written on a machine with one backend
//! has to parse on a machine without it.
//!
//! This type is *flattened* into the arg structs. clap is responsible only for
//! **discovery** (the flags show in `--help`), **acceptance** (no `--`
//! separator needed) and **raw capture**. All value typing, range checks and
//! the `USAGE` error messages stay in the single
//! [`crate::cli::tail_args::parse_tail_args`] parser: `from_arg_matches`
//! reconstructs a canonical `--flag value` token stream which the handler
//! feeds, together with the trailing `-- <raw>` args, through that one parser.

use std::ffi::{OsStr, OsString};

use clap::{Arg, ArgMatches, Command};

use crate::launch::knobs::{self, KnobDef, KnobKind};

/// Captured knob flags as a canonical token stream (`["--threads", "8",
/// "--flash-attn=true", …]`), ready to hand to `parse_tail_args`.
/// Empty when no derived flag was passed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnobFlags {
  pub tokens: Vec<OsString>,
}

/// Knobs that keep a hand-written flag on the arg struct instead of a derived
/// one, because they carry CLI semantics a plain value flag cannot.
///
/// `ctx` takes `TOKENS|auto` through its own parser, `reasoning` is a
/// `value_enum`, `mode` drives launch-mode resolution before any knob is read,
/// and `mtp` spells its tri-state `auto|on|off` rather than as a bare bool.
/// Deriving them too would register the same long twice, which panics clap at
/// startup.
///
/// These still reach every surface — the hand-written flag folds into the same
/// knob map — so the parity contract holds. The list is the exception, and a
/// test asserts nothing else joins it silently.
const HAND_WRITTEN: &[&str] = &[
  "ctx-size",
  "ctx",
  "max-model-len",
  "reasoning",
  "mode",
  "mtp",
  "mtp-draft-n",
];

fn is_derived(def: &KnobDef) -> bool {
  !HAND_WRITTEN.contains(&def.id)
}

/// Placeholder shown after the flag in `--help`. Refines the free-form knobs
/// past the generic name so the value shape is obvious without the help text.
fn value_name(def: &KnobDef) -> &'static str {
  match def.id {
    "device" => "SPEC",
    "main-gpu" => "INDEX",
    "kv-disk-dir" | "mtp-model" => "PATH",
    _ => def.kind.cli_value_name(),
  }
}

/// `--help` heading for one backend's knobs — keeps each backend's flags
/// visually together instead of one flat wall of forty-odd options.
///
/// clap wants a `'static` heading, so each is interned once. The set is
/// bounded by the number of compiled-in backends, and lives as long as the
/// process would anyway.
fn heading_for(backend_id: &str) -> &'static str {
  use std::collections::BTreeMap;
  use std::sync::{Mutex, OnceLock};
  static HEADINGS: OnceLock<Mutex<BTreeMap<String, &'static str>>> = OnceLock::new();
  let map = HEADINGS.get_or_init(|| Mutex::new(BTreeMap::new()));
  let mut map = map.lock().expect("heading cache");
  if let Some(h) = map.get(backend_id) {
    return h;
  }
  let leaked: &'static str = Box::leak(format!("Launch params ({backend_id})").into_boxed_str());
  map.insert(backend_id.to_string(), leaked);
  leaked
}

/// One clap arg per distinct knob id, under its declaring backend's heading.
///
/// A knob two backends declare (`threads`) is registered once, under the first
/// that declares it — one flag, one meaning, whichever backend ends up serving.
fn augment(mut cmd: Command) -> Command {
  let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
  for (backend_id, def) in knobs::registry::iter() {
    if !is_derived(def) || !seen.insert(def.id) {
      continue;
    }
    // Capture raw OsStrings so non-UTF8 paths / selectors survive; we never
    // parse them here — `parse_tail_args` does the real work.
    let mut arg = Arg::new(def.id)
      .long(def.id)
      .help(def.help)
      .help_heading(heading_for(backend_id))
      .value_name(value_name(def))
      .value_parser(clap::value_parser!(OsString));
    arg = match def.kind {
      // `--flash-attn` (bare → true), `--flash-attn=false`, or
      // `--flash-attn off` all work; bare uses the missing value.
      KnobKind::Bool => arg.num_args(0..=1).default_missing_value("true"),
      _ => arg.num_args(1),
    };
    // Single-dash aliases from the declaration (`-ngl`, `-t`) are
    // intentionally NOT registered as clap aliases: single-dash multi-char
    // forms aren't expressible, and single-char shorts risk colliding with
    // global flags. They all still work through the `--` passthrough, which
    // routes through the same `parse_tail_args`.
    cmd = cmd.arg(arg);
  }
  cmd
}

fn build(matches: &ArgMatches) -> KnobFlags {
  let mut tokens = Vec::new();
  let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
  for (_, def) in knobs::registry::iter() {
    if !is_derived(def) || !seen.insert(def.id) {
      continue;
    }
    // `get_raw` yields `Some` only when the flag was present (including a
    // bool's `default_missing_value`); `None` means absent.
    let Some(raw) = matches.get_raw(def.id) else {
      continue;
    };
    let value = raw.into_iter().next();
    let flag = format!("--{}", def.id);
    match def.kind {
      KnobKind::Bool => {
        // Emit `--flag=<value>` so the tail parser's `split_once('=')`
        // interprets it (a bare flag carries "true").
        let v = value.unwrap_or_else(|| OsStr::new("true"));
        let mut tok = OsString::from(flag);
        tok.push("=");
        tok.push(v);
        tokens.push(tok);
      }
      _ => {
        if let Some(v) = value {
          tokens.push(OsString::from(flag));
          tokens.push(v.to_os_string());
        }
      }
    }
  }
  KnobFlags { tokens }
}

impl clap::FromArgMatches for KnobFlags {
  fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
    Ok(build(matches))
  }
  fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
    *self = build(matches);
    Ok(())
  }
}

impl clap::Args for KnobFlags {
  fn augment_args(cmd: Command) -> Command {
    augment(cmd)
  }
  fn augment_args_for_update(cmd: Command) -> Command {
    augment(cmd)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::tail_args::parse_tail_args;
  use crate::launch::knobs::{kid, KnobSet};
  use clap::{Args, FromArgMatches};

  fn parse(argv: &[&str]) -> KnobFlags {
    let cmd = KnobFlags::augment_args(Command::new("test"));
    let matches = cmd.try_get_matches_from(argv).expect("parse");
    KnobFlags::from_arg_matches(&matches).expect("from_arg_matches")
  }

  /// Full round-trip: derived flags → tokens → `parse_tail_args`.
  fn knobs_of(argv: &[&str]) -> KnobSet {
    let flags = parse(argv);
    let (knobs, extras) = parse_tail_args(&flags.tokens).expect("tail parse");
    assert!(
      extras.is_empty(),
      "derived flags must not leak to extras: {extras:?}"
    );
    knobs
  }

  #[test]
  fn every_declared_knob_is_registered_exactly_once() {
    let cmd = KnobFlags::augment_args(Command::new("test"));
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for (_, def) in knobs::registry::iter() {
      if !is_derived(def) || !seen.insert(def.id) {
        continue;
      }
      assert!(
        cmd.get_arguments().any(|a| a.get_id() == def.id),
        "{} has no generated flag",
        def.id
      );
    }
    let ids: Vec<_> = cmd.get_arguments().map(|a| a.get_id()).collect();
    let uniq: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), uniq.len(), "duplicate flag registration");
  }

  /// The parity win. Before the registry, a non-default backend's tunables had
  /// no CLI surface at all — 26 knobs reachable only from the editor.
  #[test]
  fn a_non_default_backends_knob_gets_a_flag() {
    let cmd = KnobFlags::augment_args(Command::new("test"));
    let home = crate::backend::DEFAULT_BACKEND_ID;
    let Some(foreign) = knobs::registry::iter()
      .find(|(b, d)| *b != home && is_derived(d))
      .map(|(_, d)| d.id)
    else {
      return;
    };
    assert!(
      cmd.get_arguments().any(|a| a.get_id() == foreign),
      "{foreign} is declared by a non-default backend but has no flag"
    );
  }

  #[test]
  fn valued_knobs_round_trip_through_parse_tail_args() {
    let k = knobs_of(&[
      "test",
      "--threads",
      "8",
      "--n-gpu-layers",
      "99",
      "--device",
      "Vulkan0",
      "--cache-type-k",
      "q8_0",
    ]);
    assert_eq!(k.u32(kid("threads")), Some(8));
    assert_eq!(k.u32(kid("n-gpu-layers")), Some(99));
    assert_eq!(k.str(kid("device")), Some("Vulkan0"));
    assert_eq!(k.str(kid("cache-type-k")), Some("q8_0"));
  }

  #[test]
  fn placement_knobs_round_trip() {
    let k = knobs_of(&[
      "test",
      "--tensor-split",
      "3,1",
      "--main-gpu",
      "1",
      "--split-mode",
      "row",
    ]);
    assert_eq!(k.str(kid("tensor-split")), Some("3,1"));
    assert_eq!(k.u32(kid("main-gpu")), Some(1));
    assert_eq!(k.str(kid("split-mode")), Some("row"));
  }

  #[test]
  fn bare_bool_is_true() {
    assert_eq!(
      knobs_of(&["test", "--flash-attn"]).bool(kid("flash-attn")),
      Some(true)
    );
  }

  #[test]
  fn bool_equals_false_disables() {
    assert_eq!(
      knobs_of(&["test", "--flash-attn=false"]).bool(kid("flash-attn")),
      Some(false)
    );
  }

  #[test]
  fn bool_space_form_off() {
    assert_eq!(
      knobs_of(&["test", "--mlock", "off"]).bool(kid("mlock")),
      Some(false)
    );
  }

  #[test]
  fn absent_flags_produce_no_tokens() {
    let flags = parse(&["test"]);
    assert!(flags.tokens.is_empty());
    assert!(knobs_of(&["test"]).is_empty());
  }

  #[test]
  fn bad_value_surfaces_usage_via_parse_tail_args() {
    let flags = parse(&["test", "--threads", "xyz"]);
    let err = parse_tail_args(&flags.tokens).unwrap_err();
    assert_eq!(err.code, crate::cli::exit_codes::USAGE);
    assert!(err.to_string().contains("--threads"), "{err}");
  }

  /// The exemption list is a decision, not a dumping ground.
  ///
  /// Each entry keeps a bespoke flag because it carries CLI semantics a plain
  /// value flag cannot, and each still reaches every surface through the same
  /// knob map. Pinned so a knob cannot quietly opt out of generation — which is
  /// the one way the CLI could go back to drifting from the editor.
  #[test]
  fn the_hand_written_exemptions_are_exactly_these() {
    assert_eq!(
      HAND_WRITTEN,
      [
        "ctx-size",
        "ctx",
        "max-model-len",
        "reasoning",
        "mode",
        "mtp",
        "mtp-draft-n",
      ],
      "adding an exemption means adding the reason above it"
    );
    // Every exemption still names a knob some backend declares; a stale entry
    // would silently suppress a flag that no longer has a hand-written twin.
    for id in HAND_WRITTEN {
      assert!(
        knobs::resolve_id(id).is_some(),
        "`{id}` is exempted but no backend declares it"
      );
    }
  }

  #[test]
  fn hand_written_flags_are_not_also_derived() {
    let cmd = KnobFlags::augment_args(Command::new("test"));
    for id in HAND_WRITTEN {
      assert!(
        cmd.get_arguments().all(|a| a.get_id() != *id),
        "{id} keeps a hand-written flag and must not be derived too"
      );
    }
  }
}
