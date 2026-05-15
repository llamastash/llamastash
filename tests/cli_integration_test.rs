//! Unit 8 end-to-end coverage for the non-interactive subcommands.
//!
//! Drives the real `cli::dispatch` path against a daemon spun up via
//! `run_foreground` at a per-test temp socket. Asserts on the
//! dispatch exit code and on observable daemon state (catalog,
//! `status`, `state.json`) rather than on captured stdout — cargo's
//! thread-local stdout interception fights an in-process fd capture,
//! and the formatting layer has its own unit tests in `cli::output`.
//!
//! Test-fixtures-feature-gated because the daemon launches the
//! shipped `fake_llama_server` binary.

#![cfg(feature = "test-fixtures")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use llamatui::cli::cli_args::{
  Cli, Command, FavoritesAction, FavoritesArgs, LaunchMode as CliLaunchMode, ListArgs, LogsArgs,
  PresetsAction, PresetsArgs, PullAction, PullArgs, ReasoningFlag, StartArgs, StatusArgs, StopArgs,
};
use llamatui::cli::{dispatch, exit_codes};
use llamatui::config::loader::{LoadedConfig, PortRange};
use llamatui::config::Config;
use llamatui::daemon::discovery_task::DiscoveryOptions;
use llamatui::daemon::state_store;
use llamatui::daemon::{run_foreground, DaemonOptions};
use llamatui::discovery::scanner::ScanRoot;
use llamatui::discovery::ModelSource;
use llamatui::gguf::test_fixtures::build_minimal_gguf;
use llamatui::ipc::Client;
use tokio::task::JoinHandle;

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn unique_temp(label: &str) -> PathBuf {
  let nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_nanos();
  let p = std::env::temp_dir().join(format!(
    "llamatui-cli-{label}-{}-{nanos}",
    std::process::id()
  ));
  std::fs::create_dir_all(&p).expect("temp");
  p
}

async fn wait_for_socket(path: &Path) {
  let deadline = Instant::now() + Duration::from_secs(3);
  loop {
    if Instant::now() > deadline {
      panic!("daemon socket never appeared: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

fn allocate_port_range() -> PortRange {
  let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
  let port = listener.local_addr().unwrap().port();
  drop(listener);
  PortRange {
    start: port,
    end: port,
  }
}

/// A wider range so a duplicate-launch test can grab two ports.
fn allocate_port_range_pair() -> PortRange {
  // Bind two ephemerals to claim consecutive-ish free slots, then
  // hand the daemon a 32-wide window starting at the lower of them
  // so a second `start_model` doesn't collide.
  let l1 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 1");
  let l2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 2");
  let p1 = l1.local_addr().unwrap().port();
  let p2 = l2.local_addr().unwrap().port();
  drop(l1);
  drop(l2);
  let lo = p1.min(p2);
  PortRange {
    start: lo,
    end: lo.saturating_add(31),
  }
}

struct DaemonHandle {
  join: JoinHandle<anyhow::Result<llamatui::daemon::StartOutcome>>,
  socket: PathBuf,
  state: PathBuf,
  model_dir: PathBuf,
}

impl DaemonHandle {
  async fn shutdown(self) {
    if let Ok(mut client) = Client::connect(&self.socket).await {
      let _ = client.call("shutdown", None).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), self.join).await;
    std::fs::remove_dir_all(&self.state).ok();
    std::fs::remove_dir_all(&self.model_dir).ok();
  }

  async fn client(&self) -> Client {
    Client::connect(&self.socket).await.expect("connect")
  }
}

async fn spawn_daemon_with_model(label: &str, model_name: &str, arch: &str) -> DaemonHandle {
  let state = unique_temp(&format!("{label}-state"));
  let model_dir = unique_temp(&format!("{label}-models"));
  std::fs::write(model_dir.join(model_name), build_minimal_gguf(arch))
    .expect("write fixture model");
  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range(),
    discovery: DiscoveryOptions::new(vec![ScanRoot {
      path: model_dir.clone(),
      source: ModelSource::UserPath,
    }]),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let join = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  await_catalog_populated(&socket).await;
  DaemonHandle {
    join,
    socket,
    state,
    model_dir,
  }
}

async fn await_catalog_populated(socket: &Path) {
  let deadline = Instant::now() + Duration::from_secs(3);
  loop {
    if Instant::now() > deadline {
      panic!(
        "discovery never populated the catalog at {}",
        socket.display()
      );
    }
    if let Ok(mut client) = Client::connect(socket).await {
      if let Ok(body) = client.call("list_models", None).await {
        if body["models"]
          .as_array()
          .map(|a| !a.is_empty())
          .unwrap_or(false)
        {
          return;
        }
      }
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  }
}

fn build_cli(model_dir: &Path, command: Command) -> (Cli, LoadedConfig) {
  let cli = Cli {
    config: None,
    llama_server: Some(fake_binary()),
    model_paths: vec![model_dir.to_path_buf()],
    no_scan: true,
    no_spawn: true,
    verbose: false,
    command: Some(command),
  };
  let config = LoadedConfig {
    config: Config {
      disable_scan: true,
      ..Config::default()
    },
    warning: None,
  };
  (cli, config)
}

/// Serialises `LLAMATUI_SOCKET` env-var swap so two parallel tests
/// don't read each other's daemon. Held across an `.await` so we use
/// tokio's async-aware `Mutex` (the std `Mutex` would block worker
/// threads while a dispatch is in flight).
static SOCKET_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn run_dispatch_at(socket: Option<&Path>, model_dir: &Path, command: Command) -> i32 {
  let (cli, cfg) = build_cli(model_dir, command);
  let _guard = SOCKET_ENV_LOCK.lock().await;
  let prev = std::env::var_os("LLAMATUI_SOCKET");
  match socket {
    Some(s) => std::env::set_var("LLAMATUI_SOCKET", s),
    None => std::env::remove_var("LLAMATUI_SOCKET"),
  }
  let code = dispatch(cli, cfg).await.expect("dispatch");
  match prev {
    Some(v) => std::env::set_var("LLAMATUI_SOCKET", v),
    None => std::env::remove_var("LLAMATUI_SOCKET"),
  }
  code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_script_round_trip_list_start_status_logs_stop() {
  let h = spawn_daemon_with_model("happy", "m.gguf", "llama").await;
  let model_path = h.model_dir.join("m.gguf");
  let model_path_canon = std::fs::canonicalize(&model_path).unwrap();

  // 1. `list` succeeds (catalog has the seeded model).
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::List(ListArgs {
      json: true,
      filter: None,
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);
  // Verify the model is there via the daemon directly.
  let mut client = h.client().await;
  let body = client.call("list_models", None).await.unwrap();
  let arr = body["models"].as_array().expect("array");
  assert!(arr
    .iter()
    .any(|r| r["path"] == serde_json::Value::String(model_path_canon.display().to_string())));
  drop(client);

  // 2. `start <name>` launches the model.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Start(StartArgs {
      model: "m.gguf".into(),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      extra: vec![],
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // Wait for ready via the daemon.
  let mut client = h.client().await;
  let ready_deadline = Instant::now() + Duration::from_secs(5);
  let launch_id = loop {
    let body = client.call("status", None).await.unwrap();
    let models = body["models"].as_array().unwrap();
    if let Some(m) = models.iter().find(|m| m["state"]["state"] == "ready") {
      break m["launch_id"].as_str().unwrap().to_string();
    }
    if Instant::now() > ready_deadline {
      panic!("supervisor never reached ready");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };
  drop(client);

  // 3. `status` reports zero exit + correct daemon snapshot.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Status(StatusArgs {
      target: None,
      json: true,
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // 4. `logs -n 50` exits zero (we don't follow).
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Logs(LogsArgs {
      target: launch_id.clone(),
      follow: false,
      lines: Some(50),
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // 5. `stop <launch_id>` succeeds + daemon now shows zero models.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Stop(StopArgs {
      target: Some(launch_id),
      all: false,
      yes: true,
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);
  let mut client = h.client().await;
  let body = client.call("status", None).await.unwrap();
  assert_eq!(body["models"].as_array().map(|a| a.len()), Some(0));
  drop(client);

  h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_filter_and_unknown_ref_exit_codes() {
  let h = spawn_daemon_with_model("filter", "qwen.gguf", "qwen2").await;

  // `list --filter qwen` exits zero.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::List(ListArgs {
      json: true,
      filter: Some("qwen".into()),
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // `start phi` matches no model → MODEL_NOT_FOUND.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Start(StartArgs {
      model: "phi".into(),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      extra: vec![],
    }),
  )
  .await;
  assert_eq!(code, exit_codes::MODEL_NOT_FOUND);

  h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presets_save_list_delete_round_trip() {
  let h = spawn_daemon_with_model("presets", "m.gguf", "llama").await;

  // save
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Presets(PresetsArgs {
      model: "m.gguf".into(),
      action: PresetsAction::Save {
        name: "long-ctx".into(),
        ctx: Some(32768),
        port: None,
        reasoning: Some(ReasoningFlag::On),
        mode: Some(CliLaunchMode::Chat),
        extra: vec![OsString::from("--threads"), OsString::from("4")],
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // confirm via state.json (not stdout)
  let s = state_store::load(&h.state).expect("load state");
  let presets = s.presets;
  assert!(
    presets.iter().any(|e| e
      .presets
      .iter()
      .any(|p| p.name == "long-ctx" && p.params.ctx == Some(32768))),
    "preset should round-trip into state.json: {presets:?}"
  );

  // list
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Presets(PresetsArgs {
      model: "m.gguf".into(),
      action: PresetsAction::List { json: false },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // delete
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Presets(PresetsArgs {
      model: "m.gguf".into(),
      action: PresetsAction::Delete {
        name: "long-ctx".into(),
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  let s = state_store::load(&h.state).expect("load state");
  assert!(
    s.presets
      .iter()
      .all(|e| e.presets.iter().all(|p| p.name != "long-ctx")),
    "preset should be gone after delete"
  );

  // delete again → USAGE.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Presets(PresetsArgs {
      model: "m.gguf".into(),
      action: PresetsAction::Delete {
        name: "long-ctx".into(),
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::USAGE);

  h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn favorites_round_trip_through_dispatcher() {
  let h = spawn_daemon_with_model("favs", "m.gguf", "llama").await;

  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Favorites(FavoritesArgs {
      action: FavoritesAction::Add {
        model: "m.gguf".into(),
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);
  let s = state_store::load(&h.state).expect("load state");
  assert_eq!(s.favorites.len(), 1, "favorite should be persisted");

  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Favorites(FavoritesArgs {
      action: FavoritesAction::List { json: false },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Favorites(FavoritesArgs {
      action: FavoritesAction::Remove {
        model: "m.gguf".into(),
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);
  let s = state_store::load(&h.state).expect("load state");
  assert_eq!(s.favorites.len(), 0, "favorite should be cleared");

  h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_spawn_with_dead_daemon_exits_daemon_unreachable() {
  let model_dir = unique_temp("nospawn-models");
  let dead_socket = unique_temp("nospawn-state").join("dead.sock");
  let code = run_dispatch_at(
    Some(&dead_socket),
    &model_dir,
    Command::List(ListArgs {
      json: true,
      filter: None,
    }),
  )
  .await;
  assert_eq!(code, exit_codes::DAEMON_UNREACHABLE);
  std::fs::remove_dir_all(&model_dir).ok();
}

/// Variant of [`spawn_daemon_with_model`] with a 32-wide port range
/// so tests can launch two instances of the same model.
async fn spawn_daemon_with_model_wide_range(
  label: &str,
  model_name: &str,
  arch: &str,
) -> DaemonHandle {
  let state = unique_temp(&format!("{label}-state"));
  let model_dir = unique_temp(&format!("{label}-models"));
  std::fs::write(model_dir.join(model_name), build_minimal_gguf(arch))
    .expect("write fixture model");
  let opts = DaemonOptions {
    binary: Some(fake_binary()),
    port_range: allocate_port_range_pair(),
    discovery: DiscoveryOptions::new(vec![ScanRoot {
      path: model_dir.clone(),
      source: ModelSource::UserPath,
    }]),
    ..DaemonOptions::rooted_at(state.clone())
  };
  let socket = opts.socket_path.clone();
  let join = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  await_catalog_populated(&socket).await;
  DaemonHandle {
    join,
    socket,
    state,
    model_dir,
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presets_list_json_emits_array_for_agents() {
  let h = spawn_daemon_with_model("plj", "m.gguf", "llama").await;

  // Save a preset first so list has content.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Presets(PresetsArgs {
      model: "m.gguf".into(),
      action: PresetsAction::Save {
        name: "coding".into(),
        ctx: Some(32768),
        port: None,
        reasoning: Some(ReasoningFlag::On),
        mode: Some(CliLaunchMode::Chat),
        extra: vec![],
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // `presets list --json` exits zero. Stdout shape is asserted by
  // the unit tests in `cli::output`; here we want the dispatch
  // wiring to surface the flag without crashing.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Presets(PresetsArgs {
      model: "m.gguf".into(),
      action: PresetsAction::List { json: true },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // Also assert the preset reached state.json with the JSON flag
  // off so we know the test daemon has anything to render.
  let s = state_store::load(&h.state).expect("load state");
  assert!(
    s.presets
      .iter()
      .any(|e| e.presets.iter().any(|p| p.name == "coding")),
    "preset should round-trip into state.json: {:?}",
    s.presets,
  );

  h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_preset_chain_seeds_supervisor_with_saved_params() {
  let h = spawn_daemon_with_model("pchain", "m.gguf", "llama").await;

  // Save a preset that pins ctx + reasoning + advanced flags.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Presets(PresetsArgs {
      model: "m.gguf".into(),
      action: PresetsAction::Save {
        name: "coding".into(),
        ctx: Some(16384),
        port: None,
        reasoning: Some(ReasoningFlag::On),
        mode: Some(CliLaunchMode::Chat),
        extra: vec![OsString::from("--threads"), OsString::from("8")],
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // Now `start --preset coding` — supervisor should receive the
  // bundled flags.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Start(StartArgs {
      model: "m.gguf".into(),
      preset: Some("coding".into()),
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      extra: vec![],
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // The supervisor persists `last_params` only after reaching
  // Ready, *and* the recorder polls state every 200 ms — so wait
  // for the write rather than racing it.
  let mut client = h.client().await;
  let deadline = Instant::now() + Duration::from_secs(8);
  loop {
    let lp = client.call("last_params_list", None).await.unwrap();
    let arr = lp["last_params"].as_array().cloned().unwrap_or_default();
    if arr.iter().any(|row| {
      row["params"]["ctx"] == serde_json::json!(16384)
        && row["params"]["reasoning"] == serde_json::json!(true)
        && row["params"]["advanced"]
          .as_array()
          .map(|a| a.iter().any(|v| v == "--threads"))
          .unwrap_or(false)
    }) {
      break;
    }
    if Instant::now() > deadline {
      panic!("supervisor should have recorded preset ctx + reasoning + advanced: {arr:?}",);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  drop(client);

  h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_ctx_above_native_succeeds_and_duplicate_launch_uses_new_port() {
  let h = spawn_daemon_with_model_wide_range("dup", "m.gguf", "llama").await;

  // First launch: pass a deliberately huge ctx so we exercise the
  // "ctx > native_ctx" path. The plan calls for the daemon to log
  // a warning but still exit zero on the CLI side.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Start(StartArgs {
      model: "m.gguf".into(),
      preset: None,
      ctx: Some(131_072),
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      extra: vec![],
    }),
  )
  .await;
  assert_eq!(
    code,
    exit_codes::SUCCESS,
    "ctx > native should still exit 0"
  );

  // Second launch of the same model — duplicate-launch path. The
  // daemon allocates a different port; both rows must appear in
  // status.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Start(StartArgs {
      model: "m.gguf".into(),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      extra: vec![],
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  let mut client = h.client().await;
  let deadline = Instant::now() + Duration::from_secs(8);
  let (port_a, port_b) = loop {
    let body = client.call("status", None).await.unwrap();
    let models = body["models"].as_array().unwrap();
    if models.len() >= 2 {
      let ports: Vec<u16> = models
        .iter()
        .map(|m| m["port"].as_u64().unwrap() as u16)
        .collect();
      break (ports[0], ports[1]);
    }
    if Instant::now() > deadline {
      panic!("only one supervisor row surfaced: {models:?}");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };
  assert_ne!(port_a, port_b, "duplicate launches must use distinct ports");
  drop(client);

  h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logs_follow_returns_daemon_unreachable_when_daemon_dies() {
  let h = spawn_daemon_with_model_wide_range("logsdrop", "m.gguf", "llama").await;

  // Launch a model so `logs --follow` has a target.
  let code = run_dispatch_at(
    Some(&h.socket),
    &h.model_dir,
    Command::Start(StartArgs {
      model: "m.gguf".into(),
      preset: None,
      ctx: None,
      port: None,
      reasoning: None,
      mode: Some(CliLaunchMode::Chat),
      extra: vec![],
    }),
  )
  .await;
  assert_eq!(code, exit_codes::SUCCESS);

  // Resolve the launch id by talking to the daemon directly.
  let mut client = h.client().await;
  let deadline = Instant::now() + Duration::from_secs(5);
  let launch_id = loop {
    let body = client.call("status", None).await.unwrap();
    let models = body["models"].as_array().unwrap();
    if let Some(m) = models.iter().find(|m| m["state"]["state"] == "ready") {
      break m["launch_id"].as_str().unwrap().to_string();
    }
    if Instant::now() > deadline {
      panic!("supervisor never reached ready");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
  };
  drop(client);

  // Kick off `logs --follow` in a background task. The dispatch
  // takes the env-var lock for the duration of its call, so we
  // hold off the daemon kill until the env swap has happened.
  let socket = h.socket.clone();
  let model_dir = h.model_dir.clone();
  let launch_id_for_task = launch_id.clone();
  let follow_handle = tokio::spawn(async move {
    run_dispatch_at(
      Some(&socket),
      &model_dir,
      Command::Logs(LogsArgs {
        target: launch_id_for_task,
        follow: true,
        lines: Some(5),
      }),
    )
    .await
  });

  // Give the follower a moment to enter its poll loop.
  tokio::time::sleep(Duration::from_millis(300)).await;

  // Shut the daemon down. The follower's next `logs_tail` call
  // should fail with a connect error → DAEMON_UNREACHABLE.
  if let Ok(mut client) = Client::connect(&h.socket).await {
    let _ = client.call("shutdown", None).await;
  }

  let code = tokio::time::timeout(Duration::from_secs(5), follow_handle)
    .await
    .expect("follow exited")
    .expect("join handle");
  assert_eq!(
    code,
    exit_codes::DAEMON_UNREACHABLE,
    "logs --follow must exit 65 when daemon disappears",
  );

  // Drop temp dirs (daemon already shut down).
  std::fs::remove_dir_all(&h.state).ok();
  std::fs::remove_dir_all(&h.model_dir).ok();
  let _ = tokio::time::timeout(Duration::from_secs(2), h.join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_subcommand_exits_pull_failed_pending_v2() {
  let model_dir = unique_temp("pull-models");
  let code = run_dispatch_at(
    None,
    &model_dir,
    Command::Pull(PullArgs {
      action: PullAction::Cancel {
        job_id: "job-abc".into(),
      },
    }),
  )
  .await;
  assert_eq!(code, exit_codes::PULL_FAILED);
  std::fs::remove_dir_all(&model_dir).ok();
}
