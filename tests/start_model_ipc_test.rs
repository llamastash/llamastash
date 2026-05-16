//! Fixture-backed integration coverage for the Unit 5 IPC surface
//! that lands in the U5-P1 review follow-up: `start_model`,
//! `presets_*`, and `favorite_*` driven through the daemon against
//! the `fake_llama_server` fixture. Verifies that
//! `status` / `logs_tail` / `stop_model` / `last_params`
//! / preset CRUD / favorite toggle all work end-to-end through the
//! production `run_foreground` startup.
//!
//! Built only when `--features test-fixtures` so the production
//! `cargo install` artefact never ships the fake binary.

#![cfg(feature = "test-fixtures")]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llamadash::config::loader::PortRange;
use llamadash::daemon::state_store;
use llamadash::daemon::{run_foreground, DaemonOptions};
use llamadash::gguf::test_fixtures::build_minimal_gguf;
use llamadash::ipc::Client;
use serde_json::json;
use tokio::time::timeout;

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let p = std::env::temp_dir().join(format!(
    "llamadash-startipc-{label}-{}-{nanos}",
    std::process::id()
  ));
  std::fs::create_dir_all(&p).expect("temp");
  p
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon socket never appeared: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

fn allocate_port_range() -> PortRange {
  // Pick a fresh ephemeral port and use it as both ends of the
  // range so the daemon hands it to the fake server. Avoids
  // colliding with the production default (`41100..=41300`).
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  PortRange {
    start: port,
    end: port,
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_model_drives_supervisor_status_logs_stop_and_last_params() {
  let state = unique_temp("happy");
  let model_dir = unique_temp("happy-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = std::fs::canonicalize(&model_path).unwrap();

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let state_dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  let mut client = Client::connect(&socket).await.expect("connect");

  // 1) start_model spawns the fake server and reports a launch_id.
  let start_body = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "mode": "chat",
      })),
    )
    .await
    .expect("start_model");
  let launch_id = start_body["launch_id"]
    .as_str()
    .expect("launch_id present")
    .to_string();
  let _port = start_body["port"].as_u64().expect("port present");
  assert!(start_body["model_id"].is_object(), "model_id present");
  assert!(
    start_body["log_path"]
      .as_str()
      .map(|s| s.contains(".log"))
      .unwrap_or(false),
    "log_path present and ends in .log: {start_body:?}"
  );

  // 2) status reports the supervised model. Wait briefly for Ready.
  let ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    let body = client.call("status", None).await.expect("status");
    let models = body["models"].as_array().expect("models");
    if let Some(m) = models.iter().find(|m| m["launch_id"] == launch_id) {
      if m["state"]["state"] == json!("ready") {
        break;
      }
    }
    if std::time::Instant::now() > ready_deadline {
      panic!("supervisor never reached Ready via status");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  }

  // 3) logs_tail returns at least the fake server's `listening on …`
  // line (proves stdout/stderr tee + ring buffer are wired).
  let logs_deadline = std::time::Instant::now() + Duration::from_secs(2);
  let mut saw_listening = false;
  while std::time::Instant::now() < logs_deadline {
    let body = client
      .call(
        "logs_tail",
        Some(json!({"launch_id": &launch_id, "lines": 200})),
      )
      .await
      .expect("logs_tail");
    let lines = body["lines"].as_array().expect("lines array");
    if lines.iter().any(|l| {
      l.as_str()
        .map(|s| s.contains("listening on"))
        .unwrap_or(false)
    }) {
      saw_listening = true;
      break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  assert!(saw_listening, "logs_tail must surface child stdout/stderr");

  // 4) last_params is upserted on Loading → Ready. Poll the
  // state.json on disk to confirm — the supervisor stamps it via
  // the recorder task so timing is asynchronous.
  let last_params_deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    let s = state_store::load(&state_dir).expect("load state");
    if !s.last_params.is_empty() {
      let entry = &s.last_params[0];
      assert_eq!(entry.params.model_path, model_path_canon);
      break;
    }
    if std::time::Instant::now() > last_params_deadline {
      panic!("last_params never persisted after Ready transition");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  // running snapshot is also persisted while the model is up.
  let s = state_store::load(&state_dir).expect("load state");
  assert_eq!(
    s.running.len(),
    1,
    "running snapshot must contain the active launch"
  );

  // 5) stop_model brings it down + drops the running snapshot.
  let stop_body = client
    .call(
      "stop_model",
      Some(json!({"launch_id": &launch_id, "grace_secs": 5})),
    )
    .await
    .expect("stop_model");
  assert_eq!(stop_body["state"]["state"], json!("stopped"));

  let s = state_store::load(&state_dir).expect("load state");
  assert!(
    s.running.is_empty(),
    "stop_model must drop the running snapshot, got {:?}",
    s.running
  );

  // 6) presets_save / list / show / delete round-trip via IPC.
  let save_body = client
    .call(
      "presets_save",
      Some(json!({
        "model_path": &model_path_canon,
        "name": "long-ctx",
        "ctx": 32768,
        "reasoning": true,
        "advanced": ["--threads", "4"],
      })),
    )
    .await
    .expect("presets_save");
  assert_eq!(save_body["saved"]["name"], json!("long-ctx"));
  assert_eq!(save_body["saved"]["params"]["ctx"], json!(32768));
  assert!(save_body["replaced"].is_null());

  let list_body = client
    .call(
      "presets_list",
      Some(json!({"model_path": &model_path_canon})),
    )
    .await
    .expect("presets_list");
  let presets = list_body["presets"].as_array().expect("array");
  assert_eq!(presets.len(), 1);
  assert_eq!(presets[0]["name"], json!("long-ctx"));

  let show_body = client
    .call(
      "presets_show",
      Some(json!({"model_path": &model_path_canon, "name": "long-ctx"})),
    )
    .await
    .expect("presets_show");
  assert_eq!(show_body["preset"]["name"], json!("long-ctx"));

  let delete_body = client
    .call(
      "presets_delete",
      Some(json!({"model_path": &model_path_canon, "name": "long-ctx"})),
    )
    .await
    .expect("presets_delete");
  assert_eq!(delete_body["removed"]["name"], json!("long-ctx"));

  // 7) favorite_add / list / remove round-trip. `added: true` on
  // first call, `false` on a duplicate (idempotent).
  let add_body = client
    .call(
      "favorite_add",
      Some(json!({"model_path": &model_path_canon})),
    )
    .await
    .expect("favorite_add");
  assert_eq!(add_body["added"], json!(true));

  let dup_body = client
    .call(
      "favorite_add",
      Some(json!({"model_path": &model_path_canon})),
    )
    .await
    .expect("favorite_add dup");
  assert_eq!(dup_body["added"], json!(false));

  let list_fav = client
    .call("favorite_list", None)
    .await
    .expect("favorite_list");
  assert_eq!(list_fav["favorites"].as_array().map(|a| a.len()), Some(1));

  let remove_fav = client
    .call(
      "favorite_remove",
      Some(json!({"model_path": &model_path_canon})),
    )
    .await
    .expect("favorite_remove");
  assert_eq!(remove_fav["removed"], json!(true));

  // Persistence check — favorites + presets edits all flushed.
  let final_state = state_store::load(&state_dir).expect("load state");
  assert!(final_state.favorites.is_empty(), "favorites cleared");
  assert!(
    final_state
      .presets_map()
      .get(&final_state.last_params[0].id)
      .map(|p| p.is_empty())
      .unwrap_or(true),
    "presets cleared"
  );

  // Shutdown.
  let _ = client.call("shutdown", None).await;
  timeout(Duration::from_secs(3), daemon)
    .await
    .expect("daemon exits")
    .expect("join")
    .expect("daemon result");
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_model_returns_error_when_binary_unconfigured() {
  // Production daemon resolves the binary at startup; if it wasn't
  // resolved (e.g. user has no `llama-server` on PATH), `start_model`
  // must surface a clear error rather than blowing up internally.
  let state = unique_temp("no-binary");
  let opts = DaemonOptions::rooted_at(state.clone());
  let socket = opts.socket_path.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;

  let mut client = Client::connect(&socket).await.expect("connect");
  let err = client
    .call(
      "start_model",
      Some(json!({"model_path": "/nowhere/m.gguf"})),
    )
    .await
    .expect_err("must error without binary");
  let msg = format!("{err}");
  assert!(
    msg.to_lowercase().contains("launch environment"),
    "got: {msg}"
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
}
