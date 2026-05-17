//! CLI handlers for the `daemon` subcommand.
//!
//! `start [--detach]` — launch the daemon. Foreground holds the
//! terminal; `--detach` returns once the socket is bound.
//! `stop` — connect to the daemon and call `shutdown`.
//! `status` — connect to the daemon and report PID + uptime; emits "not
//! running" if the socket is missing or the connection fails.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};

use crate::cli::cli_args::{Cli, DaemonAction};
use crate::config::Config;
use crate::daemon::discovery_task::DiscoveryOptions;
use crate::daemon::{run_foreground, start_detached, DaemonOptions, StartOutcome};
use crate::discovery::known_caches::{default_set, RootResolution};
use crate::ipc::{Client, ClientError};
use crate::launch::binary::{locate as locate_binary, LocateInputs};
use crate::util::paths::{home_dir, runtime_socket_path};

/// Top-level dispatch for `daemon <action>`. The full `Cli` and merged
/// `Config` flow through so `handle_start` can resolve discovery roots
/// from user flags + config; status / stop ignore them.
pub async fn handle(action: DaemonAction, cli: &Cli, config: &Config) -> Result<()> {
  match action {
    DaemonAction::Start {
      detach,
      state_dir,
      socket_path,
    } => handle_start(detach, state_dir, socket_path, cli, config).await,
    DaemonAction::Stop => handle_stop().await,
    DaemonAction::Status => handle_status().await,
  }
}

async fn handle_start(
  detach: bool,
  state_dir: Option<PathBuf>,
  socket_path: Option<PathBuf>,
  cli: &Cli,
  config: &Config,
) -> Result<()> {
  let opts = build_options(state_dir, socket_path, cli, config)?;
  if detach {
    // `start_detached` blocks until the child reports socket bound.
    match start_detached(opts)? {
      StartOutcome::RanToCompletion => {
        println!("daemon: started (detached)");
        Ok(())
      }
      StartOutcome::AlreadyRunning(pid) => {
        println!("daemon: already running (pid {pid})");
        Ok(())
      }
    }
  } else {
    match run_foreground(opts).await? {
      StartOutcome::RanToCompletion => Ok(()),
      StartOutcome::AlreadyRunning(pid) => {
        println!("daemon: already running (pid {pid})");
        Ok(())
      }
    }
  }
}

async fn handle_stop() -> Result<()> {
  let socket = runtime_socket_path();
  match Client::connect(&socket).await {
    Ok(mut client) => {
      let _ = client.call("shutdown", None).await?;
      println!("daemon: shutdown requested");
      Ok(())
    }
    Err(ClientError::Connect(_)) => {
      println!("daemon: not running");
      Ok(())
    }
    Err(other) => Err(other).context("daemon stop"),
  }
}

/// Compose [`DaemonOptions`] from the parsed CLI overrides. Hidden
/// `--state-dir` / `--socket-path` flags take precedence; unset fields
/// fall back to the platform-default XDG paths. Centralised so the
/// re-exec'd child of `start_detached` honours the same priority order
/// as a hand-typed `llamadash daemon start --state-dir ...` invocation.
///
/// Discovery roots are resolved here so a real `llamadash daemon start`
/// (not just tests) populates the catalog: global `--model-path` /
/// `--no-scan` / `--config` plus `config.model_paths` /
/// `config.disable_default_cache_paths` / `config.disable_scan` feed
/// `known_caches::default_set`. An empty config + no flags still
/// produces a working daemon — the daemon just operates with whichever
/// HF/Ollama/LM Studio caches exist on disk.
pub(crate) fn build_options(
  state_dir: Option<PathBuf>,
  socket_path: Option<PathBuf>,
  cli: &Cli,
  config: &Config,
) -> Result<DaemonOptions> {
  let mut opts = DaemonOptions::from_defaults()?;
  if let Some(p) = state_dir {
    opts.state_dir = p;
  }
  if let Some(p) = socket_path {
    opts.socket_path = p;
  }
  let scan_roots = resolve_scan_roots(cli, config, home_dir().as_deref());
  opts.discovery = DiscoveryOptions::new(scan_roots);
  // Best-effort `llama-server` resolution. A miss leaves
  // `opts.binary = None`; the daemon still starts and `start_model`
  // surfaces an actionable error to the caller. We log so the user
  // sees *why* a later launch fails.
  opts.binary = match locate_binary(LocateInputs {
    cli_flag: cli.llama_server.clone(),
    env_var: std::env::var_os("LLAMADASH_LLAMA_SERVER"),
    config_path: config.llama_server_path.clone(),
  }) {
    Ok(p) => Some(p),
    Err(e) => {
      log::warn!("llama-server lookup failed: {e}");
      None
    }
  };
  opts.port_range = config.port_range;
  opts.probe_timeout_secs = Some(config.probe_timeout_secs);
  opts.propagated_cli_args = propagated_cli_args(cli);
  Ok(opts)
}

/// Collect the global CLI flags that the re-exec'd detached daemon
/// must inherit so it resolves the same discovery / binary / config
/// surface the parent would have. Without this, the child rebuilds
/// `DaemonOptions` from an empty `Cli` and the user's `-p
/// /some/path`, `--no-scan`, `--llama-server`, `--config` flags are
/// silently dropped.
fn propagated_cli_args(cli: &Cli) -> Vec<std::ffi::OsString> {
  let mut args: Vec<std::ffi::OsString> = Vec::new();
  for path in &cli.model_paths {
    args.push("--model-path".into());
    args.push(path.into());
  }
  if cli.no_scan {
    args.push("--no-scan".into());
  }
  if let Some(p) = &cli.llama_server {
    args.push("--llama-server".into());
    args.push(p.into());
  }
  if let Some(p) = &cli.config {
    args.push("--config".into());
    args.push(p.into());
  }
  args
}

/// Merge CLI + config + default-cache enumeration into the canonical
/// scan-root list the daemon should walk. Exposed as a pure function
/// so unit tests can drive it without a daemon.
pub(crate) fn resolve_scan_roots(
  cli: &Cli,
  config: &Config,
  home: Option<&std::path::Path>,
) -> Vec<crate::discovery::scanner::ScanRoot> {
  let mut user_paths: Vec<PathBuf> = config.model_paths.clone();
  for p in &cli.model_paths {
    if !user_paths.iter().any(|x| x == p) {
      user_paths.push(p.clone());
    }
  }
  let no_scan = cli.no_scan || config.disable_scan;
  default_set(RootResolution {
    user_paths: &user_paths,
    disable: &config.disable_default_cache_paths,
    no_scan,
    home,
  })
}

async fn handle_status() -> Result<()> {
  let socket = runtime_socket_path();
  // Short timeout for status — agents shouldn't sit on a dead socket.
  match Client::connect(&socket).await {
    Ok(mut client) => {
      let result = client
        .call_with_timeout("version", None, Duration::from_secs(2))
        .await?;
      println!("{}", serde_json::to_string_pretty(&result)?);
      Ok(())
    }
    Err(ClientError::Connect(_)) => {
      println!("daemon: not running");
      Ok(())
    }
    Err(other) => Err(other).context("daemon status"),
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use clap::Parser;

  use super::*;
  use crate::cli::cli_args::Cli;
  use crate::config::Config;
  use crate::discovery::ModelSource;

  fn parse_cli(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("llamadash").chain(args.iter().copied())).expect("parse")
  }

  #[test]
  fn resolve_uses_config_model_paths_and_cli_model_paths_together() {
    let cli = parse_cli(&["--model-path", "/work/cli-only", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/work/cfg-only")],
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, Some(&home));
    let user_paths: Vec<&Path> = roots
      .iter()
      .filter(|r| r.source == ModelSource::UserPath)
      .map(|r| r.path.as_path())
      .collect();
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/work/cfg-only")),
      "config path missing: {user_paths:?}"
    );
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/work/cli-only")),
      "cli path missing: {user_paths:?}"
    );
  }

  #[test]
  fn no_scan_flag_suppresses_default_caches() {
    let cli = parse_cli(&["--no-scan", "--model-path", "/work/keep", "daemon", "start"]);
    let config = Config::default();
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, Some(&home));
    let cache_sources: Vec<_> = roots
      .iter()
      .filter(|r| r.source != ModelSource::UserPath)
      .map(|r| r.source)
      .collect();
    assert!(
      cache_sources.is_empty(),
      "--no-scan must suppress default caches, got {cache_sources:?}"
    );
    assert_eq!(roots.len(), 1, "only --model-path remains");
  }

  #[test]
  fn config_disable_scan_also_suppresses_default_caches() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      disable_scan: true,
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, Some(&home));
    assert!(roots.is_empty(), "no user paths + disable_scan = empty");
  }

  #[test]
  fn config_disable_default_cache_paths_honoured() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      disable_default_cache_paths: crate::config::CachePathsConfig {
        huggingface: true,
        ollama: true,
        lm_studio: false,
      },
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, Some(&home));
    let sources: std::collections::BTreeSet<_> = roots.iter().map(|r| r.source).collect();
    assert!(!sources.contains(&ModelSource::HuggingFace));
    assert!(!sources.contains(&ModelSource::Ollama));
    // LM Studio default is still enabled.
    assert!(sources.contains(&ModelSource::LmStudio));
  }

  #[test]
  fn duplicate_paths_across_cli_and_config_collapse() {
    let cli = parse_cli(&["--model-path", "/shared", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/shared")],
      ..Config::default()
    };
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, Some(&home));
    let matches: Vec<&Path> = roots
      .iter()
      .filter(|r| r.path == Path::new("/shared"))
      .map(|r| r.path.as_path())
      .collect();
    assert_eq!(matches.len(), 1, "duplicate must collapse, got {matches:?}");
  }

  #[test]
  fn propagated_cli_args_carry_model_paths_no_scan_and_overrides() {
    // Regression: `start_detached` previously dropped every global
    // flag on re-exec, so an auto-spawned daemon silently ignored
    // the user's `-p /some/path` and `--no-scan`. `build_options`
    // now stamps these onto `DaemonOptions::propagated_cli_args` so
    // the child sees the same CLI surface as the parent.
    let cli = parse_cli(&[
      "--model-path",
      "/work/a",
      "--model-path",
      "/work/b",
      "--no-scan",
      "--llama-server",
      "/usr/local/bin/llama-server",
      "--config",
      "/etc/llamadash.yaml",
      "daemon",
      "start",
    ]);
    let args = propagated_cli_args(&cli);
    let as_str: Vec<&str> = args.iter().map(|s| s.to_str().unwrap()).collect();
    assert_eq!(
      as_str,
      vec![
        "--model-path",
        "/work/a",
        "--model-path",
        "/work/b",
        "--no-scan",
        "--llama-server",
        "/usr/local/bin/llama-server",
        "--config",
        "/etc/llamadash.yaml",
      ]
    );
  }

  #[test]
  fn propagated_cli_args_is_empty_when_no_global_flags_set() {
    let cli = parse_cli(&["daemon", "start"]);
    assert!(propagated_cli_args(&cli).is_empty());
  }
}
