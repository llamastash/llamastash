//! SGLang backend integration coverage.
//!
//! The neutrality guard here is the counterpart to the one inside
//! `src/discovery/hf_repos.rs`: that one proves the substrate names no engine,
//! this one proves the engine stays inside its own module.

use std::path::{Path, PathBuf};

/// Files allowed to name the backend, per the "adding a backend" contract in
/// `AGENTS.md`: the backend's own module, the registry, and the config
/// re-export that keeps the typed struct's path stable.
const ALLOWED: &[&str] = &[
  "src/backend/sglang/mod.rs",
  "src/backend/sglang/guard.rs",
  "src/backend/sglang/knobs.rs",
  "src/backend/mod.rs",
  "src/config/mod.rs",
  // The daemon force-flag is user-facing CLI surface, so it names the backend
  // by design — the same sanctioned exception `--lemonade` / `--ds4` / the
  // other safetensors engine carry.
  "src/cli/cli_args.rs",
  "src/cli/daemon.rs",
];

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      rust_sources(&path, out);
    } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
      out.push(path);
    }
  }
}

/// The backend id must not appear anywhere in `src/` outside the module and
/// the registration points — **in code or in comments**. Removing the backend
/// has to be deleting one directory plus a handful of registry lines.
#[test]
fn backend_id_does_not_leak_outside_its_module() {
  let root = repo_root();
  let mut files = Vec::new();
  rust_sources(&root.join("src"), &mut files);
  assert!(!files.is_empty(), "found no sources to scan");

  // Split so this test's own source cannot match when it is scanned.
  let needle = concat!("sg", "lang");
  let mut leaks = Vec::new();
  for file in files {
    let rel = file
      .strip_prefix(&root)
      .unwrap_or(&file)
      .to_string_lossy()
      .replace('\\', "/");
    if ALLOWED.contains(&rel.as_str()) {
      continue;
    }
    let Ok(text) = std::fs::read_to_string(&file) else {
      continue;
    };
    if text.to_ascii_lowercase().contains(needle) {
      leaks.push(rel);
    }
  }
  assert!(
    leaks.is_empty(),
    "backend id leaked outside its module and the registration points: {leaks:?}"
  );
}

/// Every allowlist entry must still earn its place.
#[test]
fn the_leak_allowlist_has_no_stale_entries() {
  let root = repo_root();
  let needle = concat!("sg", "lang");
  let stale: Vec<&str> = ALLOWED
    .iter()
    .copied()
    .filter(|rel| {
      std::fs::read_to_string(root.join(rel))
        .map(|t| !t.to_ascii_lowercase().contains(needle))
        .unwrap_or(true)
    })
    .collect();
  assert!(
    stale.is_empty(),
    "allowlisted files that no longer name the backend (drop them): {stale:?}"
  );
}

/// Every registration point is present, so the backend actually reaches the
/// generic tree rather than sitting in a module nothing dispatches to.
#[test]
fn backend_is_registered_in_the_enum_and_the_registry() {
  let registry = std::fs::read_to_string(repo_root().join("src/backend/mod.rs")).unwrap();
  let id = concat!("Sg", "lang");
  assert!(
    registry.contains(&format!("Backends::{id}($b) => $body")),
    "missing the for_each_backend! arm"
  );
  assert!(
    registry.contains(&format!("Backends::{id}({id}Backend::new())")),
    "missing the Backends::all() line"
  );
}

// ---------------------------------------------------------------------------
// Fixture-backed lifecycle, driven through the production daemon.
// ---------------------------------------------------------------------------

#[cfg(feature = "test-fixtures")]
mod lifecycle {
  use std::path::PathBuf;
  use std::time::Duration;

  use llamastash::backend::{BackendConfig, ServerConfig};
  use llamastash::config::{PortRange, SglangConfig, VllmConfig};
  use llamastash::daemon::{run_foreground, DaemonOptions};
  use llamastash::ipc::Client;
  use serde_json::{json, Value};

  fn fake_llama_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
  }

  fn fake_sglang_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_sglang_server"))
  }

  fn unique_temp(label: &str) -> PathBuf {
    llamastash::test_support::unique_temp_dir("ls-sglang", label)
  }

  fn allocate_port_range() -> PortRange {
    llamastash::test_support::allocate_port_range(8)
  }

  /// A safetensors snapshot laid out the way the HF cache does, with enough
  /// attention geometry in `config.json` for the guard to price a token:
  /// 2 layers, 1 KV head of dim 4, bf16 — 32 bytes per token.
  fn seed_repo(root: &std::path::Path, repo: &str) -> PathBuf {
    let snapshot = root
      .join(format!("models--{}", repo.replace('/', "--")))
      .join("snapshots/rev0");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(
      snapshot.join("config.json"),
      br#"{"model_type":"qwen2","max_position_embeddings":4096,"hidden_size":8,"num_hidden_layers":2,"num_attention_heads":2,"num_key_value_heads":1,"torch_dtype":"bfloat16"}"#,
    )
    .unwrap();
    std::fs::write(snapshot.join("model.safetensors"), vec![0u8; 64]).unwrap();
    snapshot
  }

  async fn wait_for_socket(path: &std::path::Path) {
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

  async fn wait_settled(client: &mut Client) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
      let status = client.call("status", None).await.expect("status");
      if let Some(row) = status
        .get("models")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
      {
        let state = row
          .get("state")
          .and_then(|s| s.get("state"))
          .and_then(Value::as_str)
          .unwrap_or("");
        if state == "ready" || state == "error" {
          return row.clone();
        }
      }
      if std::time::Instant::now() > deadline {
        let status = client.call("status", None).await.expect("status");
        panic!("launch never settled; status={status}");
      }
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
  }

  fn row_state(row: &Value) -> &str {
    row
      .get("state")
      .and_then(|s| s.get("state"))
      .and_then(Value::as_str)
      .unwrap_or("")
  }

  fn opts_with_sglang(state: PathBuf) -> DaemonOptions {
    let base = DaemonOptions::rooted_at(state);
    DaemonOptions {
      binary: Some(fake_llama_binary()),
      port_range: allocate_port_range(),
      backend: BackendConfig {
        sglang: SglangConfig {
          enabled: Some(true),
          servers: vec![ServerConfig {
            binary: fake_sglang_binary(),
            name: None,
          }],
        },
        ..base.backend.clone()
      },
      ..base
    }
  }

  async fn wait_for_row(client: &mut Client, name: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
      let models = client
        .call("list_models", None)
        .await
        .expect("list_models")
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
      if let Some(hit) = models
        .into_iter()
        .find(|m| m.get("name").and_then(Value::as_str) == Some(name))
      {
        return hit;
      }
      assert!(
        std::time::Instant::now() < deadline,
        "the repo never reached the catalog"
      );
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
  }

  /// The discovery chain end to end: an HF-layout tree under a configured
  /// scan root reaches the catalog as a row this backend claims.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_safetensors_repo_under_a_configured_root_reaches_the_catalog() {
    let state = unique_temp("discovery");
    let cache = unique_temp("discovery-cache");
    seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");

    let mut opts = opts_with_sglang(state.clone());
    opts.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
      path: cache.clone(),
      source: llamastash::discovery::ModelSource::HuggingFace,
    }];
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let row = wait_for_row(&mut client, "Qwen/Qwen2.5-0.5B-Instruct").await;
    assert_eq!(
      row.get("supported_backends").and_then(Value::as_array),
      Some(&vec![Value::String("sglang".into())]),
      "row: {row}"
    );

    let _ = client.call("shutdown", None).await;
    let _ = daemon.await;
    let _ = std::fs::remove_dir_all(&cache);
  }

  /// Two safetensors engines enabled at once yield **one** row per repo that
  /// lists both, higher launch priority first. The catalog is keyed by path,
  /// so before the merge the second projector's row silently replaced the
  /// first and the repo showed a single backend.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn two_safetensors_engines_share_one_row_per_repo() {
    let state = unique_temp("both");
    let cache = unique_temp("both-cache");
    seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");

    let mut opts = opts_with_sglang(state.clone());
    opts.backend.vllm = VllmConfig {
      enabled: Some(true),
      servers: vec![ServerConfig {
        binary: PathBuf::from(env!("CARGO_BIN_EXE_fake_vllm_server")),
        name: None,
      }],
      ..VllmConfig::default()
    };
    opts.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
      path: cache.clone(),
      source: llamastash::discovery::ModelSource::HuggingFace,
    }];
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let row = wait_for_row(&mut client, "Qwen/Qwen2.5-0.5B-Instruct").await;
    assert_eq!(
      row.get("supported_backends").and_then(Value::as_array),
      Some(&vec![
        Value::String("vllm".into()),
        Value::String("sglang".into())
      ]),
      "row: {row}"
    );
    let count = client
      .call("list_models", None)
      .await
      .expect("list_models")
      .get("models")
      .and_then(Value::as_array)
      .map(|a| {
        a.iter()
          .filter(|m| m.get("name").and_then(Value::as_str) == Some("Qwen/Qwen2.5-0.5B-Instruct"))
          .count()
      })
      .unwrap_or(0);
    assert_eq!(count, 1, "one row per repo, not one per engine");

    let _ = client.call("shutdown", None).await;
    let _ = daemon.await;
    let _ = std::fs::remove_dir_all(&cache);
  }

  /// The happy path through the production daemon, plus the guard: with no
  /// host reading in a test daemon the guard falls back to the default byte
  /// budget, which at 32 bytes per token is the cap the fixture must have
  /// been handed.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn safetensors_repo_is_launched_with_a_token_cap_and_stopped() {
    let state = unique_temp("happy");
    let cache = unique_temp("happy-cache");
    let snapshot = seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");

    let opts = opts_with_sglang(state.clone());
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let start = client
      .call(
        "start_model",
        Some(json!({ "model_path": snapshot.to_string_lossy(), "ctx": 2048 })),
      )
      .await
      .expect("start_model");
    assert!(start.get("port").is_some(), "no port in {start}");

    let row = wait_settled(&mut client).await;
    assert_eq!(row_state(&row), "ready", "row: {row}");
    assert_eq!(
      row.get("backend").and_then(Value::as_str),
      Some("sglang"),
      "the running row must report the real resolved backend"
    );
    // Actuals are fetched once the child is ready, so the window can land a
    // beat after the state flips.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let resolved_ctx = loop {
      let status = client.call("status", None).await.expect("status");
      let ctx = status
        .get("models")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|r| r.get("resolved_ctx"))
        .and_then(Value::as_u64);
      if ctx.is_some() || std::time::Instant::now() > deadline {
        break ctx;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
      resolved_ctx,
      Some(2048),
      "the resolved window is read back from /get_server_info: {row}"
    );

    let argv = std::fs::read_to_string(snapshot.join("fake_sglang_argv")).expect("fixture argv");
    let argv: Vec<&str> = argv.lines().collect();
    let cap_bytes = 8u64 * 1024 * 1024 * 1024;
    let expected_cap = (cap_bytes / 32).to_string();
    assert!(
      argv
        .windows(2)
        .any(|w| w[0] == "--max-total-tokens" && w[1] == expected_cap),
      "the guard's token cap must reach the launcher: {argv:?}"
    );
    assert!(
      argv
        .windows(2)
        .any(|w| w[0] == "--host" && w[1] == "127.0.0.1"),
      "{argv:?}"
    );

    let launch_id = row.get("launch_id").and_then(Value::as_str).unwrap();
    client
      .call("stop_model", Some(json!({ "launch_id": launch_id })))
      .await
      .expect("stop_model");

    let _ = client.call("shutdown", None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&cache);
  }

  /// A repo whose `config.json` carries no attention geometry cannot be
  /// priced, so the guard refuses before spawn rather than guessing; the
  /// user's explicit cap lifts the refusal.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn unreadable_geometry_refuses_until_a_cap_is_given() {
    let state = unique_temp("nogeom");
    let cache = unique_temp("nogeom-cache");
    let snapshot = seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");
    std::fs::write(
      snapshot.join("config.json"),
      br#"{"model_type":"qwen2","hidden_size":8,"num_hidden_layers":2}"#,
    )
    .unwrap();

    let opts = opts_with_sglang(state.clone());
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let err = client
      .call(
        "start_model",
        Some(json!({ "model_path": snapshot.to_string_lossy() })),
      )
      .await
      .expect_err("a launch with no readable geometry must be refused");
    let msg = err.to_string();
    assert!(
      msg.contains("max-total-tokens"),
      "the refusal must name the override: {msg}"
    );

    let start = client
      .call(
        "start_model",
        Some(json!({
          "model_path": snapshot.to_string_lossy(),
          "knobs": { "max-total-tokens": "4096" },
        })),
      )
      .await
      .expect("an explicit cap lifts the refusal");
    assert!(start.get("port").is_some(), "no port in {start}");
    let row = wait_settled(&mut client).await;
    assert_eq!(row_state(&row), "ready", "row: {row}");
    let argv = std::fs::read_to_string(snapshot.join("fake_sglang_argv")).expect("fixture argv");
    assert!(
      argv
        .lines()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0] == "--max-total-tokens" && w[1] == "4096"),
      "the user's cap must reach the launcher verbatim: {argv}"
    );

    let _ = client.call("shutdown", None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&cache);
  }

  /// Before the host has been sampled the guard spends the default budget,
  /// and a model costing more per token than that budget holds
  /// `MIN_POOL_TOKENS` of is refused there too, not launched with a pool no
  /// request fits in. Two layers of one 2,000,000-wide KV head at bf16 is
  /// 16 MB per token, so the 8 GiB default holds 512 tokens.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_pool_under_the_token_floor_is_refused_before_the_host_is_sampled() {
    let state = unique_temp("floor");
    let cache = unique_temp("floor-cache");
    let snapshot = seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");
    std::fs::write(
      snapshot.join("config.json"),
      br#"{"model_type":"qwen2","num_hidden_layers":2,"num_attention_heads":1,
          "num_key_value_heads":1,"head_dim":2000000,"torch_dtype":"bfloat16"}"#,
    )
    .unwrap();

    let opts = opts_with_sglang(state.clone());
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let err = client
      .call(
        "start_model",
        Some(json!({ "model_path": snapshot.to_string_lossy() })),
      )
      .await
      .expect_err("a pool under the token floor must be refused");
    let msg = err.to_string();
    assert!(
      msg.contains("max-total-tokens") && msg.contains("2048 tokens"),
      "the refusal must name the floor and the override: {msg}"
    );

    let _ = client.call("shutdown", None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&cache);
  }

  /// The readiness contract: a server that binds its port immediately but
  /// serves an empty `/v1/models` until the engine finishes must **not** be
  /// called ready early. This is the case a bare status check gets wrong.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn early_bind_with_empty_model_list_is_not_ready_yet() {
    let state = unique_temp("earlybind");
    let cache = unique_temp("earlybind-cache");
    let snapshot = seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");

    let opts = opts_with_sglang(state.clone());
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let began = std::time::Instant::now();
    client
      .call(
        "start_model",
        Some(json!({
          "model_path": snapshot.to_string_lossy(),
          "extras": ["--bind-early", "--load-delay-ms", "1500"],
        })),
      )
      .await
      .expect("start_model");
    let row = wait_settled(&mut client).await;

    assert_eq!(row_state(&row), "ready", "row: {row}");
    assert!(
      began.elapsed() >= Duration::from_millis(1400),
      "readiness flipped after {:?} — the probe accepted an empty model list",
      began.elapsed()
    );

    let _ = client.call("shutdown", None).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&cache);
  }

  /// A live child must survive a daemon restart: the orphan sweep re-adopts a
  /// process-per-model child that advertises its served name.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_live_child_is_readopted_rather_than_dropped_as_stale() {
    use llamastash::backend::identity::{BackendModelId, ModelIdentity};
    use llamastash::daemon::orphans::{sweep, SweepInputs};
    use llamastash::daemon::state_store::RunningSnapshot;
    use llamastash::launch::mode::LaunchMode;
    use llamastash::launch::params::LaunchParams;

    let cache = unique_temp("adopt-cache");
    let snapshot = seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");
    let port = allocate_port_range().start;

    let mut child = std::process::Command::new(fake_sglang_binary())
      .arg("serve")
      .arg("--model-path")
      .arg(&snapshot)
      .arg("--served-model-name")
      .arg("Qwen/Qwen2.5-0.5B-Instruct")
      .arg("--host")
      .arg("127.0.0.1")
      .arg("--port")
      .arg(port.to_string())
      .spawn()
      .expect("spawn fixture");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
      assert!(std::time::Instant::now() < deadline, "fixture never bound");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let recorded = vec![RunningSnapshot {
      id: ModelIdentity::Backend(BackendModelId {
        backend: "sglang".to_string(),
        name: "Qwen/Qwen2.5-0.5B-Instruct".to_string(),
      }),
      pid: child.id() as i32,
      port,
      started_at: 1_700_000_000,
      launch_id: None,
      name: None,
      preset: None,
      params: LaunchParams::new(snapshot.clone(), LaunchMode::Chat),
      actuals: Default::default(),
      resolved_backend: "sglang".to_string(),
    }];

    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: Vec::new(),
      probe_timeout: Duration::from_secs(2),
    })
    .await;

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&cache);

    assert_eq!(
      report.adopted.len(),
      1,
      "a live process-per-model child must be re-adopted, not dropped: {:?}",
      report.stale.len()
    );
    assert!(report.stale.is_empty(), "nothing should be stale here");
  }
}

/// The detached `daemon start` re-exec must re-append `--sglang`, or a
/// `--sglang` that overrides a config `enabled: false` is silently lost in
/// the child.
#[test]
fn force_flag_is_re_appended_on_the_detached_re_exec() {
  use llamastash::daemon::{backend_force_flags, DaemonOptions};

  let id = concat!("sg", "lang");
  let mut opts = DaemonOptions::rooted_at(std::env::temp_dir().join("ls-sglang-force-flag"));
  assert!(backend_force_flags(&opts).is_empty());
  opts.backend_force.insert(id.to_string(), false);
  assert!(
    backend_force_flags(&opts).is_empty(),
    "an explicit `false` must not become a force flag"
  );
  opts.backend_force.insert(id.to_string(), true);
  let flags = backend_force_flags(&opts);
  assert!(flags.contains(&format!("--{id}")), "got {flags:?}");
}
