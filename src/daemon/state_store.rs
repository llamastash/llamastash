//! On-disk persistence for the daemon's user-facing state.
//!
//! `state.json` lives under the XDG state dir and survives daemon
//! restart. It captures:
//! - `favorites` — the user's pinned models (R24 storage half).
//! - `last_params` — last successful launch params per model (R20).
//! - `presets` — named presets per model (R21).
//! - `running` — snapshot of every active supervised process so
//!   orphan re-adoption on next daemon start has something to anchor
//!   on (R42).
//!
//! Writes go through `<state.json>.tmp` + atomic `rename` so a torn
//! write never strands a half-finished file. Reads tolerate `None`
//! (file missing on first run) and surface a typed error on parse
//! failure so the caller can warn instead of silently overwriting
//! the user's data.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::gguf::identity::ModelId;
use crate::launch::favorites::Favorites;
use crate::launch::params::LaunchParams;
use crate::launch::presets::PresetStore;

/// Top-level structure of `state.json`.
///
/// `last_params` and `presets` use `Vec<(id, value)>` rather than a
/// `BTreeMap` because `serde_json` can't serialise a map keyed by a
/// struct: JSON object keys must be strings. In-memory consumers
/// hold this as a `BTreeMap` via [`DaemonState::last_params_map`] /
/// [`DaemonState::presets_map`] for ergonomic look-ups; the on-disk
/// shape stays an explicit array of pairs for compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
  #[serde(default)]
  pub favorites: Favorites,
  #[serde(default)]
  pub last_params: Vec<LastParamsEntry>,
  #[serde(default)]
  pub presets: Vec<PresetsEntry>,
  #[serde(default)]
  pub running: Vec<RunningSnapshot>,
  /// Schema version. Bumped on breaking changes so a future daemon
  /// can refuse to load (or migrate from) an older shape.
  #[serde(default = "current_schema_version")]
  pub schema_version: u32,
}

/// One entry in `last_params`. `params` is the most recently
/// *successful* launch params (R20) — the supervisor only stamps it
/// on the Loading → Ready transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastParamsEntry {
  pub id: ModelId,
  pub params: LaunchParams,
}

/// One entry in `presets` — a model's named-preset list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetsEntry {
  pub id: ModelId,
  pub presets: crate::launch::presets::Presets,
}

impl Default for DaemonState {
  fn default() -> Self {
    Self {
      favorites: Favorites::default(),
      last_params: Vec::new(),
      presets: Vec::new(),
      running: Vec::new(),
      schema_version: current_schema_version(),
    }
  }
}

impl DaemonState {
  /// In-memory map view of `last_params` for `O(log n)` lookup.
  /// Cheap on the typical daemon (a few dozen entries at most).
  pub fn last_params_map(&self) -> BTreeMap<&ModelId, &LaunchParams> {
    self
      .last_params
      .iter()
      .map(|e| (&e.id, &e.params))
      .collect()
  }

  /// In-memory map view of `presets` for `O(log n)` lookup.
  pub fn presets_map(&self) -> PresetStore {
    self
      .presets
      .iter()
      .map(|e| (e.id.clone(), e.presets.clone()))
      .collect()
  }

  /// Insert or replace the last successful params for `id`. New
  /// entries land at the **front** so the storage Vec order doubles
  /// as the "recent launches" projection the TUI surfaces in its
  /// `↺ Recent` section. On re-upsert of an existing id the entry
  /// moves to the front too — re-launching a model "promotes" it.
  pub fn upsert_last_params(&mut self, id: ModelId, params: LaunchParams) {
    self.last_params.retain(|e| e.id != id);
    self.last_params.insert(0, LastParamsEntry { id, params });
  }

  /// Insert or replace the preset list for `id`.
  pub fn upsert_presets(&mut self, id: ModelId, presets: crate::launch::presets::Presets) {
    if let Some(entry) = self.presets.iter_mut().find(|e| e.id == id) {
      entry.presets = presets;
    } else {
      self.presets.push(PresetsEntry { id, presets });
    }
  }
}

/// What the supervisor stamps into `state.json` for every launch
/// while it's running. Orphan sweep on the next start reads this and
/// tries to re-adopt the live `pid` if the recorded `port` is still
/// answering and the model file matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningSnapshot {
  pub id: ModelId,
  pub pid: i32,
  pub port: u16,
  /// Wall-clock seconds since the Unix epoch when the supervisor
  /// transitioned the model to Ready. Serialised as seconds so the
  /// JSON stays human-readable.
  pub started_at: u64,
  pub params: LaunchParams,
}

impl RunningSnapshot {
  pub fn started_at_system(&self) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(self.started_at)
  }
}

fn current_schema_version() -> u32 {
  1
}

/// Path to `state.json` under `state_dir`.
pub fn path(state_dir: &Path) -> PathBuf {
  state_dir.join("state.json")
}

/// Load `DaemonState` from `state_dir/state.json`. Returns the
/// default state when the file is absent (first-run behaviour);
/// returns a typed error when the file exists but is malformed so
/// the daemon can surface a warning before continuing with defaults.
pub fn load(state_dir: &Path) -> Result<DaemonState, LoadError> {
  let p = path(state_dir);
  match std::fs::read_to_string(&p) {
    Ok(s) => serde_json::from_str(&s).map_err(|e| LoadError::Parse {
      path: p,
      error: e.to_string(),
    }),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DaemonState::default()),
    Err(e) => Err(LoadError::Io {
      path: p,
      error: e.to_string(),
    }),
  }
}

/// Persist `state` to `state_dir/state.json` atomically. Creates the
/// directory if it doesn't exist. Writes to a unique
/// `state.json.tmp.*` first, then `rename`s — which is atomic on
/// every POSIX filesystem and guarantees the on-disk file is always
/// either the old content or the new content, never partial.
///
/// Implementation: `tempfile::NamedTempFile` handles the unique-name
/// + O_EXCL + 0o600 dance natively (audit §2.1 #2 — replaces ~80
/// lines of hand-rolled `write_tmp_safely` + `random_suffix`). The
/// crate's mkstemp-style naming is unpredictable across processes,
/// preserving the same-UID-symlink-DoS defence the manual code had.
pub fn save(state_dir: &Path, state: &DaemonState) -> Result<(), SaveError> {
  use std::io::Write as _;

  std::fs::create_dir_all(state_dir).map_err(|e| SaveError::Io {
    path: state_dir.to_path_buf(),
    error: e.to_string(),
  })?;
  let final_path = path(state_dir);
  let body = serde_json::to_vec_pretty(state).map_err(|e| SaveError::Serialise(e.to_string()))?;

  let mut tmp = tempfile::Builder::new()
    .prefix("state.json.tmp.")
    .tempfile_in(state_dir)
    .map_err(|e| SaveError::Io {
      path: state_dir.to_path_buf(),
      error: e.to_string(),
    })?;
  tmp.write_all(&body).map_err(|e| SaveError::Io {
    path: tmp.path().to_path_buf(),
    error: e.to_string(),
  })?;
  tmp.as_file().sync_all().map_err(|e| SaveError::Io {
    path: tmp.path().to_path_buf(),
    error: e.to_string(),
  })?;
  tmp.persist(&final_path).map_err(|e| SaveError::Io {
    path: final_path,
    error: e.error.to_string(),
  })?;
  Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
  #[error("state-store I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error("state-store parse at {}: {error}; the daemon is running with defaults — back up the file and remove it to clear", path.display())]
  Parse { path: PathBuf, error: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
  #[error("state-store I/O at {}: {error}", path.display())]
  Io { path: PathBuf, error: String },
  #[error("state-store serialise: {0}")]
  Serialise(String),
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  use crate::launch::mode::LaunchMode;
  use crate::launch::presets::{NamedPreset, Presets};

  fn temp_state_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamadash-state-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&p).expect("temp");
    p
  }

  fn id(path: &str, tag: u8) -> ModelId {
    ModelId {
      path: PathBuf::from(path),
      header_blake3: [tag; 32],
    }
  }

  fn fake_params(path: &str) -> LaunchParams {
    LaunchParams::new(PathBuf::from(path), LaunchMode::Chat)
  }

  #[test]
  fn load_returns_default_when_file_absent() {
    let dir = temp_state_dir("missing");
    let s = load(&dir).expect("absent file = defaults");
    assert!(s.favorites.is_empty());
    assert!(s.last_params.is_empty());
    assert!(s.presets.is_empty());
    assert!(s.running.is_empty());
    assert_eq!(s.schema_version, 1);
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn save_then_load_round_trips_every_field() {
    let dir = temp_state_dir("rt");
    let mut s = DaemonState::default();
    s.favorites.add(id("/m/a.gguf", 1));
    s.favorites.add(id("/m/b.gguf", 2));
    s.upsert_last_params(id("/m/a.gguf", 1), fake_params("/m/a.gguf"));
    let mut presets_for_a = Presets::new();
    presets_for_a.upsert(NamedPreset {
      name: "coding".into(),
      params: fake_params("/m/a.gguf"),
    });
    s.upsert_presets(id("/m/a.gguf", 1), presets_for_a);
    s.running.push(RunningSnapshot {
      id: id("/m/a.gguf", 1),
      pid: 1234,
      port: 41100,
      started_at: 1_700_000_000,
      params: fake_params("/m/a.gguf"),
    });

    save(&dir, &s).expect("save");
    let back = load(&dir).expect("load");
    assert_eq!(back, s, "every field must round-trip exactly");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn upsert_last_params_replaces_in_place_and_promotes_to_front() {
    // Re-upserting an existing id replaces the params *and* moves
    // the entry to the front. That keeps the storage Vec order in
    // sync with recency, which the TUI's `↺ Recent` section reads
    // directly via `last_params_list`.
    let mut s = DaemonState::default();
    let a = id("/m/a.gguf", 1);
    let b = id("/m/b.gguf", 2);
    s.upsert_last_params(a.clone(), fake_params("/m/a.gguf"));
    s.upsert_last_params(b.clone(), fake_params("/m/b.gguf"));
    assert_eq!(s.last_params[0].id, b, "newest insertion sits at the front");

    // Re-launch `a` — it should hop to the front again.
    let mut p2 = fake_params("/m/a.gguf");
    p2.ctx = Some(32768);
    s.upsert_last_params(a.clone(), p2.clone());
    assert_eq!(s.last_params.len(), 2, "re-upsert is in-place, not append");
    assert_eq!(s.last_params[0].id, a, "promoted to front on re-launch");
    assert_eq!(s.last_params[0].params, p2);
  }

  #[test]
  fn map_views_align_with_pair_storage() {
    let mut s = DaemonState::default();
    s.upsert_last_params(id("/m/a.gguf", 1), fake_params("/m/a.gguf"));
    s.upsert_last_params(id("/m/b.gguf", 2), fake_params("/m/b.gguf"));
    let view = s.last_params_map();
    assert_eq!(view.len(), 2);
    assert!(view.contains_key(&id("/m/a.gguf", 1)));
    assert!(view.contains_key(&id("/m/b.gguf", 2)));
  }

  #[test]
  fn save_creates_state_dir_when_missing() {
    let dir = temp_state_dir("create-parent").join("nested/state");
    let s = DaemonState::default();
    save(&dir, &s).expect("save creates dir");
    assert!(path(&dir).exists());
    fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
  }

  #[test]
  fn save_is_atomic_via_tmp_rename() {
    // After a successful save, no `.tmp` sibling must linger. The
    // exact tmp basename now includes the PID, so we scan rather than
    // hard-code the suffix.
    let dir = temp_state_dir("atomic");
    save(&dir, &DaemonState::default()).expect("save");
    for entry in fs::read_dir(&dir).expect("readdir") {
      let entry = entry.expect("dirent");
      let name = entry.file_name();
      let name = name.to_string_lossy();
      assert!(
        !name.starts_with("state.json.tmp"),
        ".tmp sibling lingered: {name}"
      );
    }
    assert!(dir.join("state.json").exists());
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn save_does_not_follow_symlink_at_tmp_path() {
    use std::os::unix::fs::symlink;

    let dir = temp_state_dir("symlink-tmp");
    fs::create_dir_all(&dir).expect("mk dir");
    // Plant a symlink at the historical PID-only tmp path shape. The
    // current `save` uses a `state.json.tmp.<pid>.<rand>` filename so
    // the planted path no longer matches the chosen tmp path —
    // randomisation defeats prediction in addition to the
    // `O_NOFOLLOW + O_EXCL` guard. The victim must remain untouched
    // and the final state.json must be a regular file.
    let victim = dir.join("victim.dat");
    fs::write(&victim, b"important data").expect("write victim");
    let planted = dir.join(format!("state.json.tmp.{}", std::process::id()));
    symlink(&victim, &planted).expect("plant symlink");

    save(&dir, &DaemonState::default()).expect("save with planted symlink should succeed");
    let after = fs::read(&victim).expect("victim still readable");
    assert_eq!(after, b"important data");
    let meta = fs::symlink_metadata(path(&dir)).expect("state.json metadata");
    assert!(meta.is_file(), "state.json must be a regular file");
    // The planted symlink should still exist — `save` worked around
    // it rather than through it.
    let planted_meta = fs::symlink_metadata(&planted).expect("planted symlink should remain");
    assert!(
      planted_meta.file_type().is_symlink(),
      "save must not touch the planted symlink",
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn corrupt_state_file_surfaces_typed_parse_error() {
    let dir = temp_state_dir("corrupt");
    fs::write(path(&dir), b"{not valid json").unwrap();
    let err = load(&dir).unwrap_err();
    match err {
      LoadError::Parse { path: p, .. } => assert_eq!(p, path(&dir)),
      other => panic!("expected Parse error, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn json_uses_hex_blake3_for_human_diffing() {
    let dir = temp_state_dir("hex-id");
    let mut s = DaemonState::default();
    s.favorites.add(id("/m/a.gguf", 0xAB));
    save(&dir, &s).unwrap();
    let raw = fs::read_to_string(path(&dir)).unwrap();
    assert!(
      raw.contains("\"abababababababababababababababababababababababababababababababab\""),
      "BLAKE3 should serialise as 64-char hex, got: {raw}"
    );
    fs::remove_dir_all(&dir).ok();
  }
}
