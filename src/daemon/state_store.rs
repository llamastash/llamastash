//! On-disk persistence for the daemon's user-facing state.
//!
//! `state.json` lives under the XDG state dir and survives daemon
//! restart. It captures:
//! - `favorites` — the user's pinned models (R24 storage half).
//! - `last_params` — last successful launch params per model.
//! - `running` — snapshot of every active supervised process so
//!   orphan re-adoption on next daemon start has something to anchor
//!   on.
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

use crate::backend::identity::ModelIdentity;
use crate::daemon::registry::LaunchId;
use crate::launch::favorites::Favorites;
use crate::launch::params::LaunchParams;

/// Top-level structure of `state.json`.
///
/// `last_params` uses `Vec<(id, value)>` rather than a `BTreeMap` because
/// `serde_json` can't serialise a map keyed by a struct: JSON object keys must
/// be strings. In-memory consumers use [`DaemonState::last_params_map`] for
/// ergonomic look-ups; the on-disk shape stays an explicit array of pairs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonState {
  #[serde(default)]
  pub favorites: Favorites,
  #[serde(default)]
  pub last_params: Vec<LastParamsEntry>,
  #[serde(default)]
  pub running: Vec<RunningSnapshot>,
  /// Schema version. A coarse marker for future use; we do not carry
  /// migration code (breaking changes ship cleanly pre-1.0).
  #[serde(default = "current_schema_version")]
  pub schema_version: u32,
}

/// One entry in `last_params`. `params` is the most recently
/// *successful* launch params — the supervisor only stamps it
/// on the Loading → Ready transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastParamsEntry {
  pub id: ModelIdentity,
  pub params: LaunchParams,
  /// The backend id this launch *resolved* to (`llamacpp` / `lemonade` /
  /// `ds4`) — the cross-backend contamination guard (D-contamination). The
  /// persisted `params.backend` choice stays `auto` for an auto-routed
  /// launch, so it can't serve as the tag. `default`/`skip_serializing_if`
  /// keep a llama.cpp row byte-stable: the field is omitted when it holds the
  /// `llamacpp` default and reads back as `llamacpp`, so a pre-ds4 `state.json`
  /// re-serializes identically (matching the `backend_knobs` pattern).
  #[serde(
    default = "default_resolved_backend",
    skip_serializing_if = "is_default_backend"
  )]
  pub resolved_backend: String,
}

/// Serde default for [`LastParamsEntry::resolved_backend`]: the default
/// backend, the only one that could have written a pre-tag `state.json` row.
fn default_resolved_backend() -> String {
  crate::backend::DEFAULT_BACKEND_ID.to_string()
}

/// Whether `s` is the default-backend tag — omitted from the serialized shape
/// for byte-stability, so only a non-default tag persists.
fn is_default_backend(s: &str) -> bool {
  s == crate::backend::DEFAULT_BACKEND_ID
}

impl Default for DaemonState {
  fn default() -> Self {
    Self {
      favorites: Favorites::default(),
      last_params: Vec::new(),
      running: Vec::new(),
      schema_version: current_schema_version(),
    }
  }
}

impl DaemonState {
  /// In-memory map view of `last_params` for `O(log n)` lookup.
  /// Cheap on the typical daemon (a few dozen entries at most).
  pub fn last_params_map(&self) -> BTreeMap<&ModelIdentity, &LaunchParams> {
    self
      .last_params
      .iter()
      .map(|e| (&e.id, &e.params))
      .collect()
  }

  /// Insert or replace the last successful params for `id`. New
  /// entries land at the **front** so the storage Vec order doubles
  /// as the "recent launches" projection the TUI surfaces in its
  /// `↺ Recent` section. On re-upsert of an existing id the entry
  /// moves to the front too — re-launching a model "promotes" it.
  pub fn upsert_last_params(
    &mut self,
    id: ModelIdentity,
    params: LaunchParams,
    resolved_backend: String,
  ) {
    self.last_params.retain(|e| e.id != id);
    self.last_params.insert(
      0,
      LastParamsEntry {
        id,
        params,
        resolved_backend,
      },
    );
  }
}

/// What the supervisor stamps into `state.json` for every launch
/// while it's running. Orphan sweep on the next start reads this and
/// tries to re-adopt the live `pid` if the recorded `port` is still
/// answering and the model file matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningSnapshot {
  pub id: ModelIdentity,
  pub pid: i32,
  pub port: u16,
  /// Wall-clock seconds since the Unix epoch when the supervisor
  /// transitioned the model to Ready. Serialised as seconds so the
  /// JSON stays human-readable.
  pub started_at: u64,
  /// The `L#` launch handle assigned by [`crate::daemon::registry::SupervisorRegistry::next_id`].
  /// Stamped on every row: a **delegated** (Lemonade) row has no supervisor of
  /// its own, so the snapshot is the sole home for its id (`status` reads it,
  /// `stop_model` reverse-maps it to the umbrella model name); a **process** row
  /// records it too so `backend_for_launch` can resolve the launch's backend and
  /// dispatch its stop to the right one. The boot sweep clears `running`, so a
  /// persisted id never survives a restart to collide with the fresh counter.
  /// `None` only on a legacy/adopted row that predates the stamp.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub launch_id: Option<LaunchId>,
  pub params: LaunchParams,
  /// What `--fit` actually chose, read from the child's `/props` once
  /// on Ready. Empty for adopted/external/Lemonade rows and until
  /// the fetch lands. `#[serde(default)]` keeps older rows loading.
  #[serde(default)]
  pub actuals: crate::daemon::actuals::Actuals,
  /// The backend id this launch *resolved* to (`llamacpp` / `lemonade` /
  /// `ds4`). Stamped at spawn (unlike `last_params.resolved_backend`, which
  /// waits for Ready), so `status` can report the *real* backend on a Loading
  /// row and the orphan sweep can dispatch adoption on it instead of a
  /// fragile process-name match. Same `default`/`skip_serializing_if` as the
  /// `last_params` tag, so a llama.cpp row stays byte-stable.
  #[serde(
    default = "default_resolved_backend",
    skip_serializing_if = "is_default_backend"
  )]
  pub resolved_backend: String,
}

impl RunningSnapshot {
  pub fn started_at_system(&self) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(self.started_at)
  }

  /// The [`BackendModelId`](crate::backend::identity::BackendModelId) behind a
  /// delegated (managed-multiplexer) row, or `None` for a process-per-model row
  /// (GGUF identity). The one predicate shared by the `status` projection, the
  /// `stop_model` snapshot sweeps, and the proxy eviction filter, so "is this a
  /// delegated row" can't drift between them. Names no backend — a delegated row
  /// is any backend-identity row whose backend is a managed multiplexer.
  pub fn delegated_backend_id(&self) -> Option<&crate::backend::identity::BackendModelId> {
    self
      .id
      .as_backend()
      .filter(|b| crate::backend::is_managed_multiplexer(&b.backend))
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
/// Implementation: `tempfile::NamedTempFile` handles the unique-name,
/// O_EXCL, and 0o600 dance natively (replaces ~80
/// lines of hand-rolled `write_tmp_safely` + `random_suffix`). The
/// crate's mkstemp-style naming is unpredictable across processes,
/// preserving the same-UID-symlink-DoS defence the manual code had.
pub fn save(state_dir: &Path, state: &DaemonState) -> Result<(), SaveError> {
  let final_path = path(state_dir);
  let body = serde_json::to_vec_pretty(state).map_err(|e| SaveError::Serialise(e.to_string()))?;
  crate::util::atomic_write::write_secure(state_dir, "state.json.tmp.", &final_path, &body, None)
    .map_err(|e| SaveError::Io {
    path: final_path,
    error: e.to_string(),
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

  use crate::gguf::identity::ModelId;
  use crate::launch::mode::LaunchMode;

  fn temp_state_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-state-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&p).expect("temp");
    p
  }

  fn id(path: &str, tag: u8) -> ModelIdentity {
    ModelIdentity::Gguf(ModelId {
      path: PathBuf::from(path),
      header_blake3: [tag; 32],
    })
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
    s.upsert_last_params(
      id("/m/a.gguf", 1),
      fake_params("/m/a.gguf"),
      "llamacpp".into(),
    );
    s.running.push(RunningSnapshot {
      id: id("/m/a.gguf", 1),
      pid: 1234,
      port: 41100,
      started_at: 1_700_000_000,
      launch_id: None,
      params: fake_params("/m/a.gguf"),
      actuals: Default::default(),
      resolved_backend: "llamacpp".to_string(),
    });

    save(&dir, &s).expect("save");
    let back = load(&dir).expect("load");
    assert_eq!(back, s, "every field must round-trip exactly");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn resolved_backend_tag_round_trips_and_legacy_rows_default_to_llamacpp() {
    // A tagged row survives save→load; a legacy row (no `resolved_backend`
    // key) reads back as `llamacpp` (D-contamination default).
    let dir = temp_state_dir("resolved-backend-tag");
    let mut s = DaemonState::default();
    s.upsert_last_params(
      id("/m/a.gguf", 1),
      fake_params("/m/a.gguf"),
      "ds4".to_string(),
    );
    save(&dir, &s).expect("save");
    let back = load(&dir).expect("load");
    assert_eq!(back.last_params[0].resolved_backend, "ds4");
    // Legacy row without the field deserialises to the llamacpp default:
    // serialise a real entry, strip `resolved_backend`, read it back.
    let entry = LastParamsEntry {
      id: id("/m/legacy.gguf", 2),
      params: fake_params("/m/legacy.gguf"),
      resolved_backend: "ds4".to_string(),
    };
    let mut v = serde_json::to_value(&entry).unwrap();
    v.as_object_mut().unwrap().remove("resolved_backend");
    let legacy: LastParamsEntry = serde_json::from_value(v).expect("legacy row parses");
    assert_eq!(legacy.resolved_backend, "llamacpp");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn llamacpp_resolved_backend_is_omitted_from_serialized_shape() {
    // Byte-stability: a llama.cpp row omits `resolved_backend` (defaults back
    // to llamacpp on load), so a pre-ds4 state.json re-serializes identically.
    // A non-default tag (ds4) still serializes.
    let llama = LastParamsEntry {
      id: id("/m/a.gguf", 1),
      params: fake_params("/m/a.gguf"),
      resolved_backend: "llamacpp".to_string(),
    };
    let v = serde_json::to_value(&llama).unwrap();
    assert!(
      v.as_object().unwrap().get("resolved_backend").is_none(),
      "llamacpp tag must be omitted for byte-stability: {v}"
    );
    let ds4 = LastParamsEntry {
      id: id("/m/b.gguf", 2),
      params: fake_params("/m/b.gguf"),
      resolved_backend: "ds4".to_string(),
    };
    let v4 = serde_json::to_value(&ds4).unwrap();
    assert_eq!(v4["resolved_backend"], "ds4", "non-default tag persists");
  }

  #[test]
  fn non_empty_backend_knobs_round_trips_through_state_json() {
    // The `skip_serializing_if = is_empty` field must still survive a
    // save→load when populated (no silent data loss on daemon restart).
    let dir = temp_state_dir("backend-knobs-rt");
    let mut s = DaemonState::default();
    let mut params = fake_params("/m/a.gguf");
    params.knobs.set_by_name("kv-disk-dir", "/tmp/kv");
    params
      .knobs
      .set_auto(crate::launch::knobs::kid("ssd-streaming"));
    s.upsert_last_params(id("/m/a.gguf", 1), params.clone(), "llamacpp".into());
    save(&dir, &s).expect("save");
    let back = load(&dir).expect("load");
    let restored = &back.last_params.first().unwrap().params;
    assert_eq!(restored.knobs, params.knobs);
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
    s.upsert_last_params(a.clone(), fake_params("/m/a.gguf"), "llamacpp".into());
    s.upsert_last_params(b.clone(), fake_params("/m/b.gguf"), "llamacpp".into());
    assert_eq!(s.last_params[0].id, b, "newest insertion sits at the front");

    // Re-launch `a` — it should hop to the front again.
    let mut p2 = fake_params("/m/a.gguf");
    p2.ctx = Some(32768);
    s.upsert_last_params(a.clone(), p2.clone(), "llamacpp".into());
    assert_eq!(s.last_params.len(), 2, "re-upsert is in-place, not append");
    assert_eq!(s.last_params[0].id, a, "promoted to front on re-launch");
    assert_eq!(s.last_params[0].params, p2);
  }

  #[test]
  fn map_views_align_with_pair_storage() {
    let mut s = DaemonState::default();
    s.upsert_last_params(
      id("/m/a.gguf", 1),
      fake_params("/m/a.gguf"),
      "llamacpp".into(),
    );
    s.upsert_last_params(
      id("/m/b.gguf", 2),
      fake_params("/m/b.gguf"),
      "llamacpp".into(),
    );
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
  fn legacy_gguf_state_json_loads_into_gguf_identities() {
    // A state.json written by pre-Phase-2 code: every `id` is a bare
    // ModelId object `{path, header_blake3}` with no enum tag. The
    // untagged `ModelIdentity` must load each one as `Gguf` — the
    // no-migration guarantee for existing users.
    let dir = temp_state_dir("legacy-load");
    let legacy = r#"{
      "favorites": [
        { "id": { "path": "/models/qwen.gguf", "header_blake3": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" } }
      ],
      "last_params": [],
      "running": [],
      "schema_version": 1
    }"#;
    fs::write(path(&dir), legacy).unwrap();
    let s = load(&dir).expect("legacy state.json must load without migration");
    let fav = s.favorites.iter().next().expect("one favorite");
    let gguf = fav
      .id
      .as_gguf()
      .expect("a legacy favorite must deserialize as a Gguf identity");
    assert_eq!(gguf.path.to_string_lossy(), "/models/qwen.gguf");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn legacy_presets_are_ignored_and_not_resaved() {
    let dir = temp_state_dir("legacy-presets");
    let legacy_presets = serde_json::json!([
      {
        "id": id("/m/legacy.gguf", 7),
        "presets": [{"name": "legacy", "params": fake_params("/m/legacy.gguf")}]
      }
    ]);
    fs::write(
      path(&dir),
      serde_json::json!({"presets": legacy_presets, "schema_version": 1}).to_string(),
    )
    .unwrap();

    let state = load(&dir).expect("legacy preset field is ignored");
    save(&dir, &state).expect("save current state");
    let saved: serde_json::Value =
      serde_json::from_str(&fs::read_to_string(path(&dir)).unwrap()).expect("saved state is JSON");
    assert!(saved.get("presets").is_none(), "legacy field stays removed");
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn backend_identity_persists_and_reloads_across_state() {
    // A backend-registry identity (no local file) must persist + reload
    // through favorites / last_params / running alongside GGUF rows.
    use crate::backend::identity::BackendModelId;
    let dir = temp_state_dir("backend-id");
    let bid: ModelIdentity = BackendModelId {
      backend: "example".into(),
      name: "Qwen2.5-7B-Instruct-GGUF".into(),
    }
    .into();
    let mut s = DaemonState::default();
    s.favorites.add(bid.clone());
    s.upsert_last_params(bid.clone(), fake_params("/unused"), "llamacpp".into());
    s.running.push(RunningSnapshot {
      id: bid.clone(),
      pid: 4321,
      port: 9100,
      started_at: 1_700_000_001,
      launch_id: None,
      params: fake_params("/unused"),
      actuals: Default::default(),
      resolved_backend: "llamacpp".to_string(),
    });

    save(&dir, &s).expect("save");
    let back = load(&dir).expect("load");
    assert_eq!(back, s, "backend identity must round-trip every map");
    assert_eq!(
      back.last_params[0].id.as_backend().unwrap().name,
      "Qwen2.5-7B-Instruct-GGUF",
      "the reloaded identity is the backend registry model"
    );
    assert!(back.favorites.contains(&bid));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn gguf_identity_stays_wire_compatible_in_state_json() {
    // A GGUF identity must serialize as a bare `{path, header_blake3}`
    // object — no untagged-enum variant key — so a new daemon's
    // state.json is byte-shape-identical to what the pre-Phase-2 code
    // wrote and read.
    let dir = temp_state_dir("wire-compat");
    let mut s = DaemonState::default();
    s.upsert_last_params(
      id("/m/a.gguf", 1),
      fake_params("/m/a.gguf"),
      "llamacpp".into(),
    );
    save(&dir, &s).unwrap();
    let raw = fs::read_to_string(path(&dir)).unwrap();
    assert!(raw.contains("\"path\""), "id keeps the bare path field");
    assert!(raw.contains("\"header_blake3\""), "id keeps header_blake3");
    assert!(
      !raw.contains("\"Gguf\"") && !raw.contains("\"Backend\""),
      "no enum variant tag may leak into the wire shape: {raw}"
    );
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
