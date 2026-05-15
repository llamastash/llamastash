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
      action: PresetsAction::List,
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
