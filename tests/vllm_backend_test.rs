//! vLLM backend integration coverage.
//!
//! The neutrality guard here is the counterpart to the one inside
//! `src/discovery/hf_repos.rs`: that one proves the substrate names no engine,
//! this one proves the engine stays inside its own module.

use std::path::{Path, PathBuf};

/// Files allowed to name the backend, per the "adding a backend" contract in
/// `AGENTS.md`: the backend's own module, the registry, and the config
/// re-export that keeps the typed struct's path stable.
const ALLOWED: &[&str] = &[
  "src/backend/vllm/mod.rs",
  "src/backend/vllm/discovery.rs",
  "src/backend/mod.rs",
  "src/config/mod.rs",
  "src/config/loader.rs",
  // The daemon force-flag is user-facing CLI surface, so it names the backend
  // by design — the same sanctioned exception `--lemonade` / `--ds4` carry.
  // `daemon/mod.rs` is on the list for one reason only: re-appending that same
  // flag across the detached re-exec, which it already does for the others.
  "src/cli/cli_args.rs",
  "src/cli/daemon.rs",
  "src/daemon/mod.rs",
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
  let needle = concat!("v", "llm");
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

/// Every registration point is present, so the backend actually reaches the
/// generic tree rather than sitting in a module nothing dispatches to.
#[test]
fn backend_is_registered_in_the_enum_and_the_registry() {
  let registry = std::fs::read_to_string(repo_root().join("src/backend/mod.rs")).unwrap();
  let id = concat!("V", "llm");
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
  use llamastash::config::{PortRange, VllmConfig};
  use llamastash::daemon::{run_foreground, DaemonOptions};
  use llamastash::ipc::Client;
  use serde_json::{json, Value};

  fn fake_llama_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
  }

  fn fake_vllm_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_vllm_server"))
  }

  fn unique_temp(label: &str) -> PathBuf {
    llamastash::test_support::unique_temp_dir("ls-vllm", label)
  }

  fn allocate_port_range() -> PortRange {
    llamastash::test_support::allocate_port_range(8)
  }

  /// A safetensors snapshot laid out the way the HF cache does, so the
  /// discovery leaf's repo-id recovery and the backend's served-name
  /// derivation both have something real to read.
  fn seed_repo(root: &std::path::Path, repo: &str) -> PathBuf {
    let snapshot = root
      .join(format!("models--{}", repo.replace('/', "--")))
      .join("snapshots/rev0");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(
      snapshot.join("config.json"),
      br#"{"model_type":"qwen2","max_position_embeddings":4096,"hidden_size":8,"num_hidden_layers":2}"#,
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

  fn opts_with_vllm(state: PathBuf, delay_args: &[&str]) -> DaemonOptions {
    let base = DaemonOptions::rooted_at(state);
    let mut binary = fake_vllm_binary();
    // The fixture takes its extra switches through the same argv the backend
    // builds, so a wrapper script is the only way to inject them. Keep it
    // simple: when no extra args are needed, use the fixture directly.
    if !delay_args.is_empty() {
      binary = wrapper_for(&base.state_dir, &fake_vllm_binary(), delay_args);
    }
    DaemonOptions {
      binary: Some(fake_llama_binary()),
      port_range: allocate_port_range(),
      backend: BackendConfig {
        vllm: VllmConfig {
          enabled: Some(true),
          servers: vec![ServerConfig { binary, name: None }],
        },
        ..base.backend.clone()
      },
      ..base
    }
  }

  /// A shell shim that appends fixture-only switches to whatever argv
  /// llamastash builds — the same shape a user needs on a host where vLLM
  /// only exists inside a container.
  fn wrapper_for(dir: &std::path::Path, real: &std::path::Path, extra: &[&str]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("vllm-wrapper.sh");
    std::fs::write(
      &path,
      format!(
        "#!/bin/sh\nexec {} \"$@\" {}\n",
        real.display(),
        extra.join(" ")
      ),
    )
    .unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
  }

  /// The discovery chain end to end: an HF-layout tree under a **configured
  /// scan root** reaches the catalog as a launchable row.
  ///
  /// Two things this pins that nothing else did. The walk used to re-derive
  /// its roots from `$HOME` instead of the configured ones, so a repo outside
  /// the default cache was invisible no matter what the config said. And the
  /// projector set was resolved once at daemon boot, so this row only appears
  /// if the set is recomputed per rescan.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_safetensors_repo_under_a_configured_root_reaches_the_catalog() {
    let state = unique_temp("discovery");
    let cache = unique_temp("discovery-cache");
    seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");

    let mut opts = opts_with_vllm(state.clone(), &[]);
    opts.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
      path: cache.clone(),
      source: llamastash::discovery::ModelSource::HuggingFace,
    }];
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let row = loop {
      let models = client
        .call("list_models", None)
        .await
        .expect("list_models")
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
      let hit = models
        .into_iter()
        .find(|m| m.get("name").and_then(Value::as_str) == Some("Qwen/Qwen2.5-0.5B-Instruct"));
      if let Some(hit) = hit {
        break hit;
      }
      assert!(
        std::time::Instant::now() < deadline,
        "the repo never reached the catalog"
      );
      tokio::time::sleep(Duration::from_millis(100)).await;
    };

    assert_eq!(
      row.get("supported_backends").and_then(Value::as_array),
      Some(&vec![Value::String("vllm".into())]),
      "row: {row}"
    );
    assert_eq!(
      row
        .get("metadata")
        .and_then(|m| m.get("weights_bytes"))
        .and_then(Value::as_u64),
      Some(64),
      "a directory row must still carry its summed safetensors size: {row}"
    );

    let _ = client.call("shutdown", None).await;
    let _ = daemon.await;
    let _ = std::fs::remove_dir_all(&cache);
  }

  /// A live child must survive a daemon restart.
  ///
  /// The orphan sweep gated re-adoption on the snapshot carrying a GGUF
  /// identity, on the reasoning that a non-GGUF row is a managed multiplexer
  /// with no child of its own. This backend is the first that is non-GGUF
  /// *and* process-per-model, so that inference was wrong: the row was dropped
  /// as stale without ever probing the port, leaving the server running with
  /// its GPU allocation and port held, in neither `running` nor `external` and
  /// reachable by no stop command.
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

    // Stand up the fixture on the recorded port, exactly as the real child
    // would be left behind by a daemon restart.
    let mut child = std::process::Command::new(fake_vllm_binary())
      .arg("serve")
      .arg(&snapshot)
      .arg("--served-model-name")
      .arg("Qwen/Qwen2.5-0.5B-Instruct")
      .arg("--host")
      .arg("127.0.0.1")
      .arg("--port")
      .arg(port.to_string())
      .spawn()
      .expect("spawn fixture");

    // Wait for it to answer before sweeping.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
      assert!(std::time::Instant::now() < deadline, "fixture never bound");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let recorded = vec![RunningSnapshot {
      id: ModelIdentity::Backend(BackendModelId {
        backend: "vllm".to_string(),
        name: "Qwen/Qwen2.5-0.5B-Instruct".to_string(),
      }),
      pid: child.id() as i32,
      port,
      started_at: 1_700_000_000,
      launch_id: None,
      params: LaunchParams::new(snapshot.clone(), LaunchMode::Chat),
      actuals: Default::default(),
      resolved_backend: "vllm".to_string(),
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

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn safetensors_repo_is_discovered_launched_and_stopped() {
    let state = unique_temp("happy");
    let cache = unique_temp("happy-cache");
    let snapshot = seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");

    let opts = opts_with_vllm(state.clone(), &[]);
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let start = client
      .call(
        "start_model",
        Some(json!({ "model_path": snapshot.to_string_lossy() })),
      )
      .await
      .expect("start_model");
    assert!(start.get("port").is_some(), "no port in {start}");

    let row = wait_settled(&mut client).await;
    assert_eq!(row_state(&row), "ready", "row: {row}");
    assert_eq!(
      row.get("backend").and_then(Value::as_str),
      Some("vllm"),
      "the running row must report the real resolved backend"
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

  /// The readiness contract: a server that binds its port immediately but
  /// serves an empty `/v1/models` until the engine finishes must **not** be
  /// called ready early. This is the case a bare status check gets wrong.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn early_bind_with_empty_model_list_is_not_ready_yet() {
    let state = unique_temp("earlybind");
    let cache = unique_temp("earlybind-cache");
    let snapshot = seed_repo(&cache, "Qwen/Qwen2.5-0.5B-Instruct");

    let opts = opts_with_vllm(state.clone(), &["--bind-early", "--load-delay-ms", "1500"]);
    let socket = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    wait_for_socket(&socket).await;
    let mut client = Client::connect(&socket).await.expect("connect");

    let began = std::time::Instant::now();
    client
      .call(
        "start_model",
        Some(json!({ "model_path": snapshot.to_string_lossy() })),
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
}

/// The detached `daemon start` re-exec must re-append `--vllm`, or a
/// `--vllm` that overrides a config `enabled: false` is silently lost in the
/// child. Caught in E2E after the unit tests passed: `LLAMASTASH_VLLM=1`
/// worked (env survives exec) while the flag did not. Both re-exec sites
/// carry it, which is why this asserts on the count.
#[test]
fn force_flag_is_re_appended_on_both_detached_re_execs() {
  let daemon = std::fs::read_to_string(repo_root().join("src/daemon/mod.rs")).unwrap();
  let flag = concat!("--v", "llm");
  let sites = daemon.matches(&format!("cmd.arg(\"{flag}\")")).count();
  let ds4_sites = daemon.matches(r#"cmd.arg("--ds4")"#).count();
  assert_eq!(
    sites, ds4_sites,
    "the force flag must ride every re-exec path the other detected backends do"
  );
  assert!(sites >= 2, "expected both re-exec sites, found {sites}");
}
