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
use std::time::Duration;

use std::collections::BTreeMap;

use llamastash::config::loader::PortRange;
use llamastash::config::{ConfigPresetBlock, PresetBody};
use llamastash::daemon::state_store;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::gguf::test_fixtures::build_minimal_gguf;
use llamastash::ipc::Client;
use serde_json::json;
use tokio::time::timeout;

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-si", label)
}

async fn wait_for_socket(path: &Path) {
  // 30 s, not 3 s: the daemon's first bind can lag on loaded CI runners
  // (slow Windows / macOS, larger `uat`-featured binaries) — matches the
  // bump in `cli_integration_test.rs::wait_for_socket`.
  let deadline = std::time::Instant::now() + Duration::from_secs(30);
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
  llamastash::test_support::allocate_port_range(8)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_model_drives_supervisor_status_logs_stop_and_last_params() {
  let state = unique_temp("happy");
  let model_dir = unique_temp("happy-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  // A multimodal projector companion next to the model (issue #13).
  // The daemon must auto-detect it and thread `--mmproj` into the spawn
  // params — asserted on the running snapshot below. Folded into this
  // happy-path test rather than a separate daemon-spawning test to
  // avoid adding parallel load to this contended integration binary.
  std::fs::write(model_dir.join("mmproj-m.gguf"), build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
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

  // 2) status reports the supervised model. Wait for Ready. Budget is
  // generous: under cargo test parallel load on GH-hosted runners the
  // fake_llama_server fork+exec+listen sequence has been observed past
  // 5 s on macOS (matches the same-file precedent for last_params).
  let ready_deadline = std::time::Instant::now() + Duration::from_secs(30);
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
  // the recorder task so timing is asynchronous. 60 s deadline
  // tolerates slow GH-hosted runners under cargo test parallel load
  // (20 s flaked windows-latest UAT in CI).
  let last_params_deadline = std::time::Instant::now() + Duration::from_secs(60);
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
  // The auto-detected projector rode through into the spawn params.
  // Compare against the canonical parent: the daemon resolves the
  // projector relative to the canonical model path, which differs from
  // `model_dir` on macOS (/tmp → /private/tmp).
  assert_eq!(
    s.running[0].params.mmproj_path,
    Some(model_path_canon.parent().unwrap().join("mmproj-m.gguf")),
    "daemon must auto-detect the mmproj companion and thread it into spawn params"
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

  // Persistence check — favorites edits are flushed.
  let final_state = state_store::load(&state_dir).expect("load state");
  assert!(final_state.favorites.is_empty(), "favorites cleared");

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

/// How many times [`prefer_port_fallback_once`] may lose the race for its
/// preferred port before the test gives up and asserts.
const PREFER_PORT_ATTEMPTS: usize = 5;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefer_port_falls_back_to_range_allocator_when_busy() {
  for attempt in 1..=PREFER_PORT_ATTEMPTS {
    if prefer_port_fallback_once(attempt == PREFER_PORT_ATTEMPTS).await {
      return;
    }
  }
}

/// One attempt at the `prefer_port` collision scenario.
///
/// `prefer_port` is the soft preference the TUI uses to honour the
/// user's previous binding. When the requested port is already
/// taken by another launch, the daemon must auto-allocate from the
/// configured range instead of refusing the launch — that's the
/// "Daemon picks any free port, update memory" behaviour the user
/// confirmed during planning. We force the collision by reusing
/// the first launch's port as the second launch's preference.
///
/// The preferred port is only ours between the probe releasing it and the
/// daemon binding it, so a loaded CI runner can take it in between and push
/// the first launch onto the fallback path. That is the box losing a port,
/// not a regression, so return `false` and let the caller retry. `strict`
/// (the final attempt) asserts instead of retrying.
async fn prefer_port_fallback_once(strict: bool) -> bool {
  let state = unique_temp("prefer-port");
  let model_dir = unique_temp("prefer-port-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  // Two-port range so the fallback has somewhere to land. Both probes stay
  // bound until the daemon is accepting, so the steal window is only the
  // gap between the drop below and the daemon's own bind.
  let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p0 = probe.local_addr().unwrap().port();
  let probe2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p1 = probe2.local_addr().unwrap().port();
  let (lo, hi) = if p0 < p1 { (p0, p1) } else { (p1, p0) };
  let range = PortRange { start: lo, end: hi };

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: range,
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  drop(probe);
  drop(probe2);

  // First launch — prefer the low port; daemon should bind it.
  let first = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "mode": "chat",
        "prefer_port": lo,
      })),
    )
    .await
    .expect("start_model 1");
  if !strict && first["port"].as_u64() != Some(lo as u64) {
    let _ = client.call("shutdown", None).await;
    let _ = timeout(Duration::from_secs(3), daemon).await;
    std::fs::remove_dir_all(&state).ok();
    std::fs::remove_dir_all(&model_dir).ok();
    return false;
  }
  assert_eq!(
    first["port"].as_u64(),
    Some(lo as u64),
    "first launch must honour prefer_port when free"
  );

  // Second launch — same preference, but lo is now busy. Daemon
  // must auto-allocate the remaining port rather than error.
  let second = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "mode": "chat",
        "prefer_port": lo,
      })),
    )
    .await
    .expect("start_model 2 must succeed via fallback");
  let assigned = second["port"].as_u64().expect("port present");
  assert_ne!(
    assigned, lo as u64,
    "fallback must pick a different port: {second:?}"
  );
  // The allocator walks `lo..=hi` linearly probing free ports — the
  // exact landing slot depends on which ports happen to be free on
  // the test box. Assert it stays inside the configured range so
  // the test doesn't paper over a regression that escapes the
  // range entirely.
  assert!(
    assigned > lo as u64 && assigned <= hi as u64,
    "fallback must stay within the configured range ({lo}..={hi}), got {assigned}"
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
  true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_model_refuses_both_port_and_prefer_port() {
  // Strict pin (`port`) and soft preference (`prefer_port`) express
  // different intents — accepting both leaves the daemon guessing
  // which to honour. The handler rejects the combination at parse
  // time before any reservation work.
  let state = unique_temp("port-conflict");
  let opts = DaemonOptions::rooted_at(state.clone());
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  let err = client
    .call(
      "start_model",
      Some(json!({
        "model_path": "/nowhere/m.gguf",
        "port": 41100,
        "prefer_port": 41100,
      })),
    )
    .await
    .expect_err("daemon must refuse both");
  let msg = format!("{err}");
  assert!(
    msg.contains("port") && msg.contains("prefer_port"),
    "error must name the conflicting fields: {msg}"
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_model_refuses_forbidden_extras_without_leaking_secret_values() {
  // The IPC `extras flags refused` error path stringifies the banned
  // list back to the caller. Secret-bearing flags (`--api-key`,
  // `--ssl-*`) must redact their value before serialisation — a
  // typo'd secret would otherwise land in the caller's stderr and
  // any daemon log that captures the response.
  let state = unique_temp("forbidden-redact");
  let model_dir = unique_temp("forbidden-redact-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  let err = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "extras": ["--api-key=supersecret", "--ssl-key-file=/etc/key.pem"],
      })),
    )
    .await
    .expect_err("forbidden extras must be refused");
  let msg = format!("{err}");
  assert!(
    !msg.contains("supersecret"),
    "api-key value leaked into IPC error: {msg}"
  );
  assert!(
    !msg.contains("/etc/key.pem"),
    "ssl path leaked into IPC error: {msg}"
  );
  assert!(
    msg.contains("<value-redacted>"),
    "redaction marker absent: {msg}"
  );
  assert!(
    msg.contains("--api-key") && msg.contains("--ssl-key-file"),
    "redacted form must still name the rejected flags: {msg}"
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_params_persists_only_user_supplied_knob_deltas() {
  // Source-chip semantics on the editor depend on a precise contract:
  // `last_params.params.knobs` holds the *user-supplied delta*, not
  // the resolved knob set. A knob the user never touched must stay
  // `None` on disk so the next launch can re-resolve it from yaml /
  // built-in / model default and surface the correct chip; freezing
  // it as `(last used)` would silently erode the chain.
  //
  // We exercise the contract end-to-end by doing two starts: the
  // first stamps `last_used`, the second supplies a *different*
  // user delta. The resolver will pull the first call's `threads`
  // into call 2's resolved knobs (via the `last_used` layer), but
  // the persisted entry for call 2 must only carry the new delta
  // (`mlock = true`), not the carried-over `threads`.
  let state = unique_temp("last-params-delta");
  let model_dir = unique_temp("last-params-delta-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  // Two-port range so the second start has somewhere to land after
  // the first call's socket releases.
  let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p0 = probe.local_addr().unwrap().port();
  drop(probe);
  let probe2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p1 = probe2.local_addr().unwrap().port();
  drop(probe2);
  let (lo, hi) = if p0 < p1 { (p0, p1) } else { (p1, p0) };

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: PortRange { start: lo, end: hi },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // Call 1: user supplies `threads = 4`. After Ready, the recorder
  // task persists this entry so call 2's resolver sees it under the
  // `LastUsed` layer.
  let first = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "knobs": {"threads": 4},
      })),
    )
    .await
    .expect("start_model call 1");
  let first_launch = first["launch_id"].as_str().unwrap().to_string();

  // Wait for call 1's persistence to land before issuing call 2.
  // 60 s deadline tolerates slow GH-hosted runners under cargo test
  // parallel load (20 s flaked the windows-latest UAT job in CI even
  // though the same binary ran the same test in ~1 s without the
  // `uat` feature).
  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  loop {
    let s = state_store::load(&state_dir).expect("load state");
    if !s.last_params.is_empty()
      && s.last_params[0]
        .params
        .knobs
        .u32(llamastash::launch::knobs::kid("threads"))
        == Some(4)
    {
      break;
    }
    if std::time::Instant::now() > deadline {
      panic!("call 1 last_params.threads never persisted");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  }

  // Stop call 1 so its port is free and the second start doesn't
  // collide.
  let _ = client
    .call(
      "stop_model",
      Some(json!({"launch_id": &first_launch, "grace_secs": 5})),
    )
    .await
    .expect("stop_model");

  // Call 2: user supplies a *different* delta (`mlock = true`). The
  // resolver will inherit `threads = 4` from `last_used`, but the
  // *persisted* knobs for call 2 must NOT carry it forward — only
  // the new user-supplied `mlock` belongs in the delta.
  let _ = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "knobs": {"mlock": true},
      })),
    )
    .await
    .expect("start_model call 2");

  // Poll for call 2's persistence — upsert promotes the entry to the
  // front of the Vec, so once `mlock == Some(true)` lands at index 0
  // we know the recorder fired for call 2. 60 s deadline matches the
  // call-1 wait for the same runner-load reason.
  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  let knobs = loop {
    let s = state_store::load(&state_dir).expect("load state");
    if let Some(entry) = s.last_params.first() {
      if entry
        .params
        .knobs
        .bool(llamastash::launch::knobs::kid("mlock"))
        == Some(true)
      {
        break entry.params.knobs.clone();
      }
    }
    if std::time::Instant::now() > deadline {
      panic!("call 2 last_params.mlock never persisted");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };

  // The contract: only the call-2 delta survives on disk.
  assert_eq!(
    knobs.bool(llamastash::launch::knobs::kid("mlock")),
    Some(true),
    "user-supplied mlock must persist verbatim"
  );
  assert_eq!(
    knobs.u32(llamastash::launch::knobs::kid("threads")),
    None,
    "threads came from `last_used` resolver layer, NOT user input on \
     call 2 — persistence must drop it so the source chip can stay \
     `(last used)` on the next launch rather than collapsing to \
     `(user)`. Got: {:?}",
    knobs.u32(llamastash::launch::knobs::kid("threads"))
  );
  // Spot-check a few other fields that the resolver might fill (GPU
  // backends seed `n_gpu_layers`; some arches seed `flash_attn`).
  // Whatever the resolver decided, none of it is in the user delta.
  assert_eq!(
    knobs.u32(llamastash::launch::knobs::kid("n-gpu-layers")),
    None
  );
  assert_eq!(
    knobs.bool(llamastash::launch::knobs::kid("flash-attn")),
    None
  );
  assert_eq!(knobs.u32(llamastash::launch::knobs::kid("ctx-size")), None);
  assert_eq!(
    knobs.bool(llamastash::launch::knobs::kid("reasoning")),
    None
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_selection_start_inherits_last_params_extras() {
  // A no-selection launch (plain `start` / proxy auto-start) inherits the
  // model's effective default extras — last_params here, since no config
  // `default:` is set. This mirrors how knobs already inherit last_params on
  // a plain start, and supersedes the old origin gate (which wrongly kept a
  // plain manual launch from inheriting). The clean "inherit nothing" gesture
  // is `selection: auto`, exercised separately.
  let state = unique_temp("no-selection-extras-inherit");
  let model_dir = unique_temp("no-selection-extras-inherit-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  // Two-port range so call 2 has somewhere to land after call 1 stops.
  let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p0 = probe.local_addr().unwrap().port();
  drop(probe);
  let probe2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p1 = probe2.local_addr().unwrap().port();
  drop(probe2);
  let (lo, hi) = if p0 < p1 { (p0, p1) } else { (p1, p0) };

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: PortRange { start: lo, end: hi },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // Call 1 (manual): ship a free-form extra. After Ready it persists
  // into last_params, where call 2's resolver could pick it up.
  let first = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "extras": ["--chat-template-file", "/tmp/custom.jinja"],
      })),
    )
    .await
    .expect("start_model call 1");
  let first_launch = first["launch_id"].as_str().unwrap().to_string();

  // Wait for call 1's extras to land on disk (60 s for slow CI runners,
  // matching the sibling last_params tests).
  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  loop {
    let s = state_store::load(&state_dir).expect("load state");
    if s.last_params.first().is_some_and(|e| {
      e.params
        .extras
        .iter()
        .any(|x| x.to_str() == Some("--chat-template-file"))
    }) {
      break;
    }
    if std::time::Instant::now() > deadline {
      panic!("call 1 last_params.extras never persisted");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  }

  // Stop call 1 to free its port.
  let _ = client
    .call(
      "stop_model",
      Some(json!({"launch_id": &first_launch, "grace_secs": 5})),
    )
    .await
    .expect("stop_model");

  // Call 2 (no selection): no extras, no `selection` field (defaults to the
  // no-selection `default`), plus a distinguishing knob (`mlock`) so we can
  // tell call 2's persisted entry apart from call 1's.
  let _ = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "knobs": {"mlock": true},
      })),
    )
    .await
    .expect("start_model call 2");

  // Poll until call 2's entry lands (mlock marks it), then check extras.
  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  let extras = loop {
    let s = state_store::load(&state_dir).expect("load state");
    if let Some(entry) = s.last_params.first() {
      if entry
        .params
        .knobs
        .bool(llamastash::launch::knobs::kid("mlock"))
        == Some(true)
      {
        break entry.params.extras.clone();
      }
    }
    if std::time::Instant::now() > deadline {
      panic!("call 2 last_params (mlock marker) never persisted");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };

  assert!(
    extras
      .iter()
      .any(|x| x.to_str() == Some("--chat-template-file")),
    "a no-selection launch inherits call 1's last_params extras; got {extras:?}"
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

// Seed a daemon with a single per-model config preset block.
fn preset_block(default: Option<&str>, name: &str, ctx: u32, extras: &[&str]) -> ConfigPresetBlock {
  let mut entries = BTreeMap::new();
  entries.insert(
    name.to_string(),
    PresetBody {
      knobs: llamastash::knobset! {
        ctx: ctx
      },
      extras: (!extras.is_empty()).then(|| extras.iter().map(|s| s.to_string()).collect()),
      ..PresetBody::default()
    },
  );
  ConfigPresetBlock {
    default: default.map(str::to_string),
    entries,
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_selection_start_applies_configured_default_preset() {
  // A model with a config `default:` preset launches with that preset's
  // knobs + extras on a no-selection start (plain `start` / proxy
  // auto-start) — the daemon resolves the default server-side.
  let state = unique_temp("default-preset-apply");
  let model_dir = unique_temp("default-preset-apply-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  let mut presets = BTreeMap::new();
  presets.insert(
    "m.gguf".to_string(),
    preset_block(
      Some("long"),
      "long",
      65536,
      &["--chat-template-file", "/tmp/preset.jinja"],
    ),
  );

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    presets,
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // No selection field, no knobs, no extras → daemon applies the default.
  let resp = client
    .call(
      "start_model",
      Some(json!({"model_path": &model_path_canon})),
    )
    .await
    .expect("start_model");
  let port = resp["port"].as_u64().unwrap() as u16;

  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  let params = loop {
    let s = state_store::load(&state_dir).expect("load state");
    if let Some(r) = s.running.iter().find(|r| r.port == port) {
      break r.params.clone();
    }
    if std::time::Instant::now() > deadline {
      panic!("default-preset launch never recorded a running snapshot");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };

  assert_eq!(
    params.ctx,
    Some(65536),
    "default preset's ctx applied on a no-selection launch"
  );
  assert!(
    params
      .extras
      .iter()
      .any(|x| x.to_str() == Some("--chat-template-file")),
    "default preset's extras applied; got {:?}",
    params.extras
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_selection_start_applies_the_default_presets_pinned_mode() {
  // A `default:` preset pinning `mode:` has to reach the launch, not just the
  // config file. This is the surface that made it a silent mis-launch: proxy
  // auto-start and the TUI send no mode of their own, so the daemon's own
  // `unwrap_or(Chat)` decided, and `--embeddings` never reached argv while
  // `presets show` happily reported `mode: embedding`.
  let state = unique_temp("default-preset-mode");
  let model_dir = unique_temp("default-preset-mode-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  let mut entries = BTreeMap::new();
  entries.insert(
    "emb".to_string(),
    PresetBody {
      knobs: llamastash::knobset! { mode: "embedding" },
      ..PresetBody::default()
    },
  );
  let mut presets = BTreeMap::new();
  presets.insert(
    "m.gguf".to_string(),
    ConfigPresetBlock {
      default: Some("emb".to_string()),
      entries,
    },
  );

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    presets,
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // No `mode` on the wire — exactly what proxy auto-start sends for a model
  // whose hint is chat/unknown.
  let resp = client
    .call(
      "start_model",
      Some(json!({"model_path": &model_path_canon})),
    )
    .await
    .expect("start_model");
  let port = resp["port"].as_u64().unwrap() as u16;

  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  let params = loop {
    let s = state_store::load(&state_dir).expect("load state");
    if let Some(r) = s.running.iter().find(|r| r.port == port) {
      break r.params.clone();
    }
    if std::time::Instant::now() > deadline {
      panic!("default-preset launch never recorded a running snapshot");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };

  assert_eq!(
    params.mode,
    llamastash::launch::mode::LaunchMode::Embedding,
    "the default preset's pinned mode drives the launch"
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_selection_start_inherits_nothing() {
  // `selection: auto` is pure fit: it skips last_params (and the default
  // preset), so a prior launch's extras are NOT carried forward. This is
  // the clean "inherit nothing" gesture.
  let state = unique_temp("auto-selection-clean");
  let model_dir = unique_temp("auto-selection-clean-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p0 = probe.local_addr().unwrap().port();
  drop(probe);
  let probe2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p1 = probe2.local_addr().unwrap().port();
  drop(probe2);
  let (lo, hi) = if p0 < p1 { (p0, p1) } else { (p1, p0) };

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: PortRange { start: lo, end: hi },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // Call 1: extras → last_params.
  let first = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "extras": ["--chat-template-file", "/tmp/custom.jinja"],
      })),
    )
    .await
    .expect("start_model call 1");
  let first_launch = first["launch_id"].as_str().unwrap().to_string();

  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  loop {
    let s = state_store::load(&state_dir).expect("load state");
    if s.last_params.first().is_some_and(|e| {
      e.params
        .extras
        .iter()
        .any(|x| x.to_str() == Some("--chat-template-file"))
    }) {
      break;
    }
    if std::time::Instant::now() > deadline {
      panic!("call 1 last_params.extras never persisted");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  }

  let _ = client
    .call(
      "stop_model",
      Some(json!({"launch_id": &first_launch, "grace_secs": 5})),
    )
    .await
    .expect("stop_model");

  // Call 2: selection=auto → pure fit, no extras inherited.
  let resp = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "selection": "auto",
      })),
    )
    .await
    .expect("start_model call 2");
  let port = resp["port"].as_u64().unwrap() as u16;

  // Pure-fit (selection=auto, no user knobs, no arch defaults for this model)
  // resolves no real layer, so the provenance field is omitted from the response.
  assert!(
    resp.get("layer_sources").is_none(),
    "a pure-fit launch must omit layer_sources; got {:?}",
    resp.get("layer_sources")
  );

  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  let params = loop {
    let s = state_store::load(&state_dir).expect("load state");
    if let Some(r) = s.running.iter().find(|r| r.port == port) {
      break r.params.clone();
    }
    if std::time::Instant::now() > deadline {
      panic!("auto-selection launch never recorded a running snapshot");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };

  assert!(
    params.extras.is_empty(),
    "selection=auto inherits nothing; got {:?}",
    params.extras
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_preset_launch_does_not_inherit_stale_last_params() {
  // The PR's namesake scenario, end-to-end: an inline launch persists a knob
  // (`threads`) into `last_params`; a subsequent *named-preset* launch
  // (`selection: explicit`, whose preset does not set `threads`) must NOT
  // inherit that stale knob from the `LastUsed` layer. A preset is
  // self-contained — before the fix it leaked `last_used` here. The unit test
  // on `is_pure_fit` guards the gate in isolation; this drives the real
  // resolver + recorder through the daemon.
  let state = unique_temp("named-preset-no-leak");
  let model_dir = unique_temp("named-preset-no-leak-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p0 = probe.local_addr().unwrap().port();
  drop(probe);
  let probe2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let p1 = probe2.local_addr().unwrap().port();
  drop(probe2);
  let (lo, hi) = if p0 < p1 { (p0, p1) } else { (p1, p0) };

  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: PortRange { start: lo, end: hi },
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let state_dir = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  // Call 1 (inline, no selection): user supplies `threads = 4`. After Ready,
  // the recorder persists it so a later launch's resolver would see it under
  // the `LastUsed` layer.
  let first = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "knobs": {"threads": 4},
      })),
    )
    .await
    .expect("start_model call 1");
  let first_launch = first["launch_id"].as_str().unwrap().to_string();

  let deadline = std::time::Instant::now() + Duration::from_secs(60);
  loop {
    let s = state_store::load(&state_dir).expect("load state");
    if !s.last_params.is_empty()
      && s.last_params[0]
        .params
        .knobs
        .u32(llamastash::launch::knobs::kid("threads"))
        == Some(4)
    {
      break;
    }
    if std::time::Instant::now() > deadline {
      panic!("call 1 last_params.threads never persisted");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  }

  // Stop call 1 so its port is free and the second start doesn't collide.
  let _ = client
    .call(
      "stop_model",
      Some(json!({"launch_id": &first_launch, "grace_secs": 5})),
    )
    .await
    .expect("stop_model");

  // Call 2 (named preset): `selection: explicit` with the preset's flattened
  // knobs. The preset sets `ctx-size` but NOT `threads`, so a stale
  // `last_params.threads` must not leak in from the `LastUsed` layer.
  let resp = client
    .call(
      "start_model",
      Some(json!({
        "model_path": &model_path_canon,
        "selection": "explicit",
        "knobs": {"ctx-size": 8192},
      })),
    )
    .await
    .expect("start_model call 2");

  let sources = resp.get("layer_sources").cloned().unwrap_or(json!({}));
  // The preset's own knob resolves from the user layer — proves the preset was
  // actually applied (guards against a false pass where nothing resolved).
  assert_eq!(
    sources.get("ctx-size").and_then(|v| v.as_str()),
    Some("user"),
    "the named preset's ctx-size must resolve from the user layer; got {:?}",
    sources.get("ctx-size")
  );
  // The stale last_params knob must NOT leak in from the LastUsed layer.
  assert_ne!(
    sources.get("threads").and_then(|v| v.as_str()),
    Some("last_used"),
    "a named-preset launch must not inherit a stale last_params knob; got {:?}",
    sources.get("threads")
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
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
  let socket = opts.state_dir.clone();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_surfaces_resolved_ctx_actuals_from_props() {
  // A launch with no pinned ctx emits `--fit-ctx <floor>`; the fake
  // server's `/props` reports it as the resolved n_ctx; the daemon's
  // post-Ready actuals fetch stamps it on the running snapshot and
  // `status` surfaces it (R6).
  let state = unique_temp("actuals");
  let model_dir = unique_temp("actuals-models");
  let model_path = model_dir.join("m.gguf");
  std::fs::write(&model_path, build_minimal_gguf("llama")).unwrap();
  let model_path_canon = llamastash::util::paths::canonicalize(&model_path).unwrap();

  let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let base = probe.local_addr().unwrap().port();
  drop(probe);
  let range = PortRange {
    start: base,
    end: base.saturating_add(8),
  };

  // `DaemonOptions::rooted_at` defaults `fit_ctx_floor` to 16384.
  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: range,
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.state_dir.clone();
  let daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let mut client = Client::connect(&socket).await.expect("connect");

  client
    .call(
      "start_model",
      Some(json!({"model_path": &model_path_canon, "mode": "chat"})),
    )
    .await
    .expect("start_model");

  // The actuals fetch runs asynchronously on the Ready transition, so
  // poll status rather than racing it.
  let deadline = std::time::Instant::now() + Duration::from_secs(30);
  let mut resolved = None;
  while std::time::Instant::now() < deadline {
    let status = client.call("status", None).await.unwrap();
    if let Some(rc) = status["models"]
      .as_array()
      .and_then(|ms| ms.iter().find_map(|m| m["resolved_ctx"].as_u64()))
    {
      resolved = Some(rc);
      break;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  assert_eq!(
    resolved,
    Some(16384),
    "status must surface the fit-resolved ctx read from /props"
  );

  let _ = client.call("shutdown", None).await;
  let _ = timeout(Duration::from_secs(3), daemon).await;
  std::fs::remove_dir_all(&state).ok();
  std::fs::remove_dir_all(&model_dir).ok();
}
