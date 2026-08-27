//! First-class `start` flags generated from the typed-knob spec table.
//!
//! Every knob in [`crate::launch::flag_aliases::knob_specs`] (except
//! `Ctx`/`Reasoning`, which keep their dedicated `--ctx`/`--reasoning`
//! flags) becomes a real `start --<flag>`, so the CLI surface can never
//! drift from the TUI editor — adding a spec row adds a flag and a
//! `--help` line automatically.
//!
//! This type is *flattened* into `StartArgs`. clap is responsible only
//! for **discovery** (the flags show in `start --help`), **acceptance**
//! (no `--` separator needed), and **raw capture**. All value typing,
//! range checks, and the good `USAGE` error messages stay in the single
//! [`crate::cli::tail_args::parse_tail_args`] parser: `from_arg_matches`
//! reconstructs a canonical `--flag value` token stream which the
//! `start` handler feeds, together with the trailing `-- <raw>` args,
//! through that one parser.

use std::ffi::{OsStr, OsString};

use clap::{Arg, ArgMatches, Command};

use crate::launch::flag_aliases::{is_cli_derived, knob_specs, KnobField, KnobSpec, ValueKind};

/// Help heading the derived flags are grouped under in `start --help`.
const HELP_HEADING: &str = "Advanced launch params";

/// Captured knob flags as a canonical token stream (`["--threads", "8",
/// "--flash-attn=true", ...]`), ready to hand to `parse_tail_args`.
/// Empty when no derived flag was passed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnobFlags {
  pub tokens: Vec<OsString>,
}

/// Long flag name (no `--`) used as both the clap arg id and `.long()`.
/// Borrows the spec's `'static` string, so no `string` feature needed.
fn long_name(spec: &KnobSpec) -> &'static str {
  spec.canonical.strip_prefix("--").unwrap_or(spec.canonical)
}

/// Placeholder shown after the flag in `--help`. Refines the two
/// free-form `Str` knobs (and `--main-gpu`) past the generic name.
fn value_name(spec: &KnobSpec) -> &'static str {
  match spec.field {
    KnobField::Device => "SPEC",
    KnobField::TensorSplit => "RATIO",
    KnobField::MainGpu => "INDEX",
    _ => spec.kind.cli_value_name(),
  }
}

fn augment(mut cmd: Command) -> Command {
  for spec in knob_specs() {
    if !is_cli_derived(spec.field) {
      continue;
    }
    let long = long_name(spec);
    // Capture raw OsStrings so non-UTF8 paths / selectors survive; we
    // never parse them here — `parse_tail_args` does the real work.
    let mut arg = Arg::new(long)
      .long(long)
      .help(spec.help)
      .help_heading(HELP_HEADING)
      .value_name(value_name(spec))
      .value_parser(clap::value_parser!(OsString));
    arg = match spec.kind {
      // `--flash-attn` (bare → true), `--flash-attn=false`, or
      // `--flash-attn off` all work; bare uses the missing value.
      ValueKind::Bool => arg.num_args(0..=1).default_missing_value("true"),
      _ => arg.num_args(1),
    };
    // Short/long aliases from the spec table (`-ngl`, `-t`, ...) are
    // intentionally NOT registered as clap aliases: single-dash
    // multi-char forms aren't expressible, and single-char shorts risk
    // colliding with global flags. They all still work via the `--`
    // passthrough, which routes through the same `parse_tail_args`.
    cmd = cmd.arg(arg);
  }
  cmd
}

fn build(matches: &ArgMatches) -> KnobFlags {
  let mut tokens = Vec::new();
  for spec in knob_specs() {
    if !is_cli_derived(spec.field) {
      continue;
    }
    let long = long_name(spec);
    // `get_raw` yields `Some` only when the flag was present (including
    // a bool's `default_missing_value`); `None` means absent.
    let Some(raw) = matches.get_raw(long) else {
      continue;
    };
    let value = raw.into_iter().next();
    match spec.kind {
      ValueKind::Bool => {
        // Emit `--flag=<value>` so `recognise`'s `split_once('=')` +
        // `parse_bool` interpret it (bare flag carries "true").
        let v = value.unwrap_or_else(|| OsStr::new("true"));
        let mut tok = OsString::from(spec.canonical);
        tok.push("=");
        tok.push(v);
        tokens.push(tok);
      }
      _ => {
        if let Some(v) = value {
          tokens.push(OsString::from(spec.canonical));
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
  use clap::{Args, FromArgMatches};

  /// Build a throwaway command with the derived flags, parse `argv`,
  /// and return the reconstructed token stream.
  fn parse(argv: &[&str]) -> KnobFlags {
    let cmd = KnobFlags::augment_args(Command::new("test"));
    let matches = cmd.try_get_matches_from(argv).expect("parse");
    KnobFlags::from_arg_matches(&matches).expect("from_arg_matches")
  }

  /// Full round-trip: derived flags → tokens → `parse_tail_args`.
  fn knobs(argv: &[&str]) -> crate::launch::knobs::KnobSet {
    let flags = parse(argv);
    let (knobs, extras) = parse_tail_args(&flags.tokens).expect("tail parse");
    assert!(
      extras.is_empty(),
      "derived flags must not leak to extras: {extras:?}"
    );
    knobs
  }







  #[test]
  fn absent_flags_produce_no_tokens() {
    let flags = parse(&["test"]);
    assert!(flags.tokens.is_empty());
    assert_eq!(knobs(&["test"]), crate::launch::knobs::KnobSet::new());
  }

  #[test]
  fn bad_value_surfaces_usage_via_parse_tail_args() {
    let flags = parse(&["test", "--threads", "xyz"]);
    let err = parse_tail_args(&flags.tokens).unwrap_err();
    assert_eq!(err.code, crate::cli::exit_codes::USAGE);
    assert!(err.to_string().contains("--threads"), "{err}");
  }

  #[test]
  fn ctx_and_reasoning_are_not_registered() {
    let cmd = KnobFlags::augment_args(Command::new("test"));
    assert!(cmd.get_arguments().all(|a| a.get_id() != "ctx-size"));
    assert!(cmd.get_arguments().all(|a| a.get_id() != "reasoning"));
  }
}
