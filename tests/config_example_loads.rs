//! `config.example.yaml` is documentation users copy from, so it has to be
//! config the loader actually honours.
//!
//! Nothing checked it before, and it drifted: the shipped `arch_defaults` and
//! `presets` examples kept the pre-registry key shape after the ids changed.
//! Copying them produced a file that parsed without complaint and then did
//! nothing — the silent failure the whole knob refactor exists to remove.

use std::path::PathBuf;

use llamastash::config::load_config_from_path;
use llamastash::launch::knobs;

fn example_path() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml")
}

/// It parses, with no warning and no relocated keys.
#[test]
fn the_shipped_example_config_loads_clean() {
  let loaded = load_config_from_path(&example_path());
  assert_eq!(loaded.warning, None, "config.example.yaml does not load");
  assert!(
    loaded.relocated_keys.is_empty(),
    "config.example.yaml uses keys that have moved: {:?}",
    loaded.relocated_keys
  );
}

/// Its `arch_defaults` knobs resolve to real declared knobs and carry values.
///
/// A key no backend declares is dropped on load with a warning, so an example
/// full of stale spellings would still "load" while setting nothing.
#[test]
fn the_examples_arch_defaults_land_on_declared_knobs() {
  let cfg = load_config_from_path(&example_path()).config;
  assert!(
    !cfg.arch_defaults.is_empty(),
    "the example should demonstrate arch_defaults"
  );
  for (arch, set) in &cfg.arch_defaults {
    assert!(!set.is_empty(), "`{arch}` arch default parsed to nothing");
    for (id, _) in set.iter() {
      assert!(
        knobs::def_for(id).is_some(),
        "`{arch}` sets `{id}`, which no backend declares"
      );
    }
  }
}

/// Its presets carry their knobs, including the `auto` delegation token and
/// the identity fields that let a preset reproduce a whole run.
#[test]
fn the_examples_presets_carry_knobs_and_identity() {
  let cfg = load_config_from_path(&example_path()).config;
  assert!(
    !cfg.presets.is_empty(),
    "the example should demonstrate presets"
  );
  let mut saw_value = false;
  let mut saw_auto = false;
  let mut saw_identity = false;
  for (key, block) in &cfg.presets {
    for (name, body) in &block.entries {
      let where_ = format!("preset `{key}` / `{name}`");
      for (id, value) in body.knobs.iter() {
        assert!(
          knobs::def_for(id).is_some(),
          "{where_} sets `{id}`, which no backend declares"
        );
        if value.is_auto() {
          saw_auto = true;
        } else {
          saw_value = true;
        }
      }
      if body.backend.is_some() || body.server.is_some() {
        saw_identity = true;
      }
    }
  }
  assert!(saw_value, "no example preset pins a concrete knob value");
  assert!(
    saw_auto,
    "no example preset shows the `auto` delegation token"
  );
  assert!(
    saw_identity,
    "no example preset shows pinning `backend` / `server`, so nothing \
     demonstrates reproducing a whole run rather than just its tuning"
  );
}
