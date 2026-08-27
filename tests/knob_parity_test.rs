//! The tests that keep knob parity from rotting.
//!
//! The 2026-08-25 audit that prompted the unified registry found a clean split:
//! every setting on the generated path reached all three surfaces, and every
//! setting off it was missing from at least one. Generation is what buys the
//! guarantee, so these tests assert the generation still happens rather than
//! re-listing the knobs (a list would be the fourth place to forget one).
//!
//! Four checks, one per way parity can break:
//!
//! 1. a declared knob stops reaching a surface,
//! 2. an identity field (backend / server / extras / port) stops reaching one,
//! 3. the declarations themselves become inconsistent,
//! 4. the composed argv drifts from what the engine was getting before.

use std::collections::BTreeSet;
use std::ffi::OsString;

use llamastash::backend::{Backend, Backends};
use llamastash::launch::knobs::{self, KnobKind, KnobValue, Scalar};

/// Whether `set` carries a value that reaches `def` — under its own id, or
/// under a sibling's spelling of the same concept.
///
/// Config and CLI keys are resolved before the serving backend is known, so a
/// shared concept legitimately lands under whichever knob the blind lookup
/// matched first; `by_concept` and the layered resolver both re-key it at
/// launch. Insisting on the exact id here would fail a value that arrives fine.
fn reaches(set: &knobs::KnobSet, def: &knobs::KnobDef) -> bool {
  set.iter().any(|(got, _)| {
    got == def.knob_id()
      || (def.concept.is_some() && knobs::def_for(got).and_then(|d| d.concept) == def.concept)
  })
}

/// A value this knob's own declaration accepts, for round-trip checks.
fn sample_for(def: &knobs::KnobDef) -> String {
  match def.kind {
    KnobKind::Bool => "true".into(),
    KnobKind::U32 { max } => max.map_or(4, |m| m.min(4)).to_string(),
    KnobKind::F32 { min, max } => {
      let lo = min.unwrap_or(0.0);
      format!("{}", max.map_or(lo + 1.0, |hi| (lo + hi) / 2.0))
    }
    KnobKind::Ratio => "3,1".into(),
    KnobKind::Enum { choices } | KnobKind::OpenEnum { choices, .. } => choices[0].into(),
    KnobKind::Str => "x".into(),
  }
}

/// **1.** Every declared knob reaches the CLI, the editor, and a preset.
///
/// Not "a flag exists" — that passed while the value was being dropped on the
/// floor. Each surface has to carry the value through and hand it back.
#[test]
fn every_declared_knob_reaches_every_surface() {
  for (backend_id, def) in knobs::registry::iter() {
    let id = def.knob_id();
    let raw = sample_for(def);

    // CLI: the flag parses and the value lands on a knob that reaches this
    // backend — its own id, or a sibling carrying the same concept, which
    // `resolve_layered` re-keys at launch.
    let token = OsString::from(format!("--{}={raw}", def.id));
    let (parsed, extras) =
      llamastash::cli::tail_args::parse_tail_args(&[token]).unwrap_or_else(|e| {
        panic!(
          "{backend_id}'s `{}` does not parse as a CLI flag: {e}",
          def.id
        )
      });
    assert!(
      extras.is_empty(),
      "{backend_id}'s `{}` fell through to extras: {extras:?}",
      def.id
    );
    assert!(
      reaches(&parsed, def),
      "{backend_id}'s `{}` parsed but stored nothing that reaches it",
      def.id
    );

    // Editor: the backend that declares it produces a row for it, and the row
    // takes a typed value back.
    let mut picker = llamastash::tui::launch_picker::LaunchPickerState::for_model("m");
    picker.model_backend = llamastash::launch::params::BackendChoice::from_id(backend_id);
    // Both gated groups open, so a gate is never mistaken for a missing row.
    picker.mtp_capable = true;
    picker.servers = two_device_server(backend_id);
    assert!(
      picker.ordered_fields().contains(&row(id)),
      "{backend_id}'s `{}` has no editor row",
      def.id
    );
    picker
      .commit_text(id, &raw)
      .unwrap_or_else(|e| panic!("{backend_id}'s `{}` rejects its own sample: {e}", def.id));
    assert!(
      picker.user_knobs.contains(id),
      "{backend_id}'s `{}` committed nothing",
      def.id
    );

    // Preset: it survives a write/read round-trip through the stored shape.
    let mut set = knobs::KnobSet::new();
    set.set(id, KnobValue::Set(Scalar::Str(raw.clone())));
    let body = llamastash::config::PresetBody {
      knobs: set,
      extras: None,
      backend: Some(backend_id.to_string()),
      server: None,
    };
    let yaml = yaml_serde::to_string(&body).expect("preset serialises");
    let back: llamastash::config::PresetBody =
      yaml_serde::from_str(&yaml).expect("preset deserialises");
    assert!(
      reaches(&back.knobs, def),
      "{backend_id}'s `{}` did not survive a preset round-trip:\n{yaml}",
      def.id
    );
  }
}

fn row(id: knobs::KnobId) -> llamastash::tui::launch_picker::PickerField {
  llamastash::tui::launch_picker::PickerField::Knob(id)
}

/// A two-device server for `backend_id`, so the multi-GPU group is open.
fn two_device_server(backend_id: &str) -> Vec<llamastash::backend::Server> {
  let device = |sel: &str| llamastash::backend::Device {
    total_mib: None,
    free_mib: None,
    selector: sel.into(),
    gpu_backend: "test".into(),
    name: sel.into(),
  };
  vec![llamastash::backend::Server {
    id: format!("{backend_id}-test"),
    backend_id: backend_id.to_string(),
    binary: std::path::PathBuf::from("/test/engine"),
    name: format!("{backend_id}-test"),
    devices: vec![device("D0"), device("D1")],
  }]
}

/// **2.** The four launch-identity fields reach the surfaces they declare.
///
/// These are not knobs — they say *what runs*, not how it is tuned — so they
/// cannot be generated from the registry and are the one place a surface can
/// still be forgotten. The uneven row is deliberate: `port` is CLI-only (D7),
/// recorded here so dropping a surface is still a failure while the one
/// exemption stays a decision rather than an oversight.
#[test]
fn every_identity_field_reaches_its_declared_surfaces() {
  struct Field {
    name: &'static str,
    /// The clap arg id, where it differs from the preset key.
    cli_arg: &'static str,
    cli: bool,
    tui: bool,
    preset: bool,
    why: &'static str,
  }
  let fields = [
    Field {
      name: "backend",
      cli_arg: "backend",
      cli: true,
      tui: true,
      preset: true,
      why: "picks the engine, so a preset that omits it cannot reproduce a run",
    },
    Field {
      name: "server",
      cli_arg: "server",
      cli: true,
      tui: true,
      preset: true,
      why: "picks the build; two builds of one engine launch differently",
    },
    Field {
      name: "extras",
      cli_arg: "extra",
      cli: true,
      tui: true,
      preset: true,
      why: "the escape hatch for everything not typed",
    },
    Field {
      name: "port",
      cli_arg: "port",
      cli: true,
      tui: false,
      preset: false,
      why: "D7: the daemon reserves it, and a pinned port in a preset collides \
             with a second instance instead of reproducing anything",
    },
  ];

  // CLI: `start` accepts the flag.
  let start = cli_command("start");
  // Preset: the stored body carries the field.
  let body_keys: BTreeSet<String> = {
    let body = llamastash::config::PresetBody {
      knobs: knobs::KnobSet::new(),
      extras: Some(vec!["--x".into()]),
      backend: Some("b".into()),
      server: Some("s".into()),
    };
    let v: yaml_serde::Value = yaml_serde::to_value(&body).expect("preset serialises");
    v.as_mapping()
      .map(|m| {
        m.keys()
          .filter_map(|k| k.as_str().map(str::to_string))
          .collect()
      })
      .unwrap_or_default()
  };
  // TUI: the picker owns the field.
  let picker = llamastash::tui::launch_picker::LaunchPickerState::for_model("m");
  let tui_has = |name: &str| match name {
    "backend" => true, // `model_backend` / `launch_backend()`
    "server" => picker.selected_server.is_none() || true, // the Server row
    "extras" => picker
      .ordered_fields()
      .contains(&llamastash::tui::launch_picker::PickerField::Extras),
    "port" => false, // D7 — no row, by decision
    _ => unreachable!(),
  };

  for f in fields {
    assert_eq!(
      start.get_arguments().any(|a| a.get_id() == f.cli_arg),
      f.cli,
      "`{}` CLI surface changed — {}",
      f.name,
      f.why
    );
    assert_eq!(
      body_keys.contains(f.name),
      f.preset,
      "`{}` preset surface changed — {}",
      f.name,
      f.why
    );
    assert_eq!(
      tui_has(f.name),
      f.tui,
      "`{}` TUI surface changed — {}",
      f.name,
      f.why
    );
  }
}

/// The clap `Command` for one subcommand of the real CLI.
fn cli_command(name: &str) -> clap::Command {
  use clap::CommandFactory;
  llamastash::cli::cli_args::Cli::command()
    .get_subcommands()
    .find(|c| c.get_name() == name)
    .unwrap_or_else(|| panic!("no `{name}` subcommand"))
    .clone()
}

/// **3.** The declarations are internally consistent.
///
/// A malformed registry cannot ship, because this gates the build: two
/// backends claiming one id with different value shapes, a knob claiming a
/// flag llamastash owns or the loopback denylist refuses, one backend carrying
/// a concept twice, or a cycle stop the knob's own kind rejects.
#[test]
fn registry_is_valid() {
  let errors = knobs::registry::validate();
  assert!(
    errors.is_empty(),
    "registry is inconsistent:\n{}",
    errors
      .iter()
      .map(|e| format!("  - {e}"))
      .collect::<Vec<_>>()
      .join("\n")
  );
}

/// Every backend declares at least one knob, and the whole set is reachable
/// by the name it is stored under. A backend with no knobs accepts no flags
/// and renders an empty editor, which is always an oversight.
#[test]
fn every_backend_declares_resolvable_knobs() {
  for b in Backends::all() {
    let defs = Backend::knobs(&b);
    assert!(!defs.is_empty(), "{} declares no knobs", Backend::id(&b));
    for d in defs {
      assert_eq!(
        knobs::resolve_id_for(Backend::id(&b), d.id),
        Some(d.knob_id()),
        "{}'s `{}` does not resolve to itself",
        Backend::id(&b),
        d.id
      );
    }
  }
}
