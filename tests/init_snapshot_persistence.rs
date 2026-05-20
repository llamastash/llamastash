//! Integration tests for `init/snapshot.rs` — atomic write, corruption
//! recovery, idempotent re-save.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use llamastash::init::snapshot::{self, InitSnapshot, InstallMethod, LoadError, ManagedKey};

fn temp_state_dir(label: &str) -> PathBuf {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let p = std::env::temp_dir().join(format!(
    "llamastash-init-snapshot-it-{label}-{}-{nanos}",
    std::process::id()
  ));
  fs::create_dir_all(&p).expect("temp");
  p
}

#[test]
fn save_then_load_round_trips_a_real_world_snapshot() {
  let dir = temp_state_dir("round-trip");
  let snap = InitSnapshot {
    gpu_vendor: Some("nvidia".into()),
    vram_gb: Some(24.0),
    gpu_device_count: Some(1),
    llama_server_version: Some("b9219".into()),
    install_method: Some(InstallMethod::GhReleases),
    init_date: Some("2026-05-19T00:00:00Z".into()),
    llama_server_path: Some(PathBuf::from(
      "/home/u/.local/share/llamastash/llama-cpp/b9219/llama-server",
    )),
    llama_server_digest: Some("a".repeat(64)),
    snapshot_bundle_date: Some("2026-05-18".into()),
    remote_fetch_failures: 0,
    managed_keys: vec![
      ManagedKey::new(
        "port_range",
        b"{start: 41100, end: 41300}",
        UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
      ),
      ManagedKey::new(
        "arch_defaults.qwen2",
        b"{n_gpu_layers: 99, flash_attn: true}",
        UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
      ),
    ],
    schema_version: 1,
  };
  snapshot::save(&dir, &snap).expect("save");
  let back = snapshot::load(&dir).expect("load").expect("Some");
  assert_eq!(back, snap);
  fs::remove_dir_all(&dir).ok();
}

#[test]
fn re_save_overwrites_without_leftover_tmp() {
  let dir = temp_state_dir("re-save");
  let mut snap = InitSnapshot::default();
  for i in 0..5_u32 {
    snap.remote_fetch_failures = i;
    snapshot::save(&dir, &snap).expect("save");
  }
  let back = snapshot::load(&dir).expect("load").expect("Some");
  assert_eq!(back.remote_fetch_failures, 4);
  let stray: Vec<_> = fs::read_dir(&dir)
    .unwrap()
    .filter_map(|e| e.ok())
    .filter(|e| {
      e.file_name()
        .to_string_lossy()
        .starts_with("init_snapshot.json.tmp")
    })
    .collect();
  assert!(stray.is_empty(), "no .tmp lingered after 5 saves");
  fs::remove_dir_all(&dir).ok();
}

#[test]
fn corrupted_snapshot_is_quarantined_and_recoverable() {
  let dir = temp_state_dir("quarantine");
  let target = snapshot::path_for(&dir);
  fs::write(&target, b"\x00\x01\x02not json").unwrap();
  let err = snapshot::load(&dir).unwrap_err();
  let quarantine = match err {
    LoadError::Parse { quarantine, .. } => quarantine,
    other => panic!("expected Parse error, got {other:?}"),
  };
  assert!(quarantine.exists(), "broken-<ts> sidecar exists");
  assert!(
    !target.exists(),
    "original file moved out of the way so re-init can rebuild"
  );
  // Re-saving a fresh snapshot to the same dir should succeed.
  snapshot::save(&dir, &InitSnapshot::default()).expect("recover");
  assert!(target.exists());
  let recovered = snapshot::load(&dir).expect("load").expect("Some");
  assert_eq!(recovered, InitSnapshot::default());
  fs::remove_dir_all(&dir).ok();
}

#[test]
fn schema_too_new_is_refused_with_typed_error() {
  let dir = temp_state_dir("schema-too-new");
  // Hand-craft a JSON document declaring schema_version: 99.
  let body = serde_json::json!({
    "schema_version": 99,
    "remote_fetch_failures": 0,
    "managed_keys": [],
  });
  fs::write(snapshot::path_for(&dir), body.to_string()).unwrap();
  let err = snapshot::load(&dir).unwrap_err();
  assert!(
    matches!(err, LoadError::SchemaTooNew { found: 99, .. }),
    "expected SchemaTooNew, got {err:?}"
  );
  fs::remove_dir_all(&dir).ok();
}
