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
    DaemonAction::Status { json } => handle_status(json).await,
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
        println!(
          "{}",
          crate::cli::colors::success("daemon: started (detached)")
        );
        Ok(())
      }
      StartOutcome::AlreadyRunning(pid) => {
        print_already_running(pid);
        Ok(())
      }
    }
  } else {
    match run_foreground(opts).await? {
      StartOutcome::RanToCompletion => Ok(()),
      StartOutcome::AlreadyRunning(pid) => {
        print_already_running(pid);
        Ok(())
      }
    }
  }
}

/// "daemon: already running (pid N)" — emitted from both the `--detach`
/// and the foreground branches of `handle_start` when the daemon is
/// already up. "daemon: already running" stays dim; the pid lifts to
/// bold so it's the scannable token in the otherwise dim line.
fn print_already_running(pid: i32) {
  println!(
    "{} ({} {})",
    crate::cli::colors::dim("daemon: already running"),
    crate::cli::colors::dim("pid"),
    console::style(pid.to_string()).bold(),
  );
}

async fn handle_stop() -> Result<()> {
  let socket = runtime_socket_path();
  match Client::connect(&socket).await {
    Ok(mut client) => {
      let _ = client.call("shutdown", None).await?;
      println!(
        "{}",
        crate::cli::colors::success("daemon: shutdown requested")
      );
      Ok(())
    }
    Err(ClientError::Connect(_)) => {
      println!("{}", crate::cli::colors::dim("daemon: not running"));
      Ok(())
    }
    Err(other) => Err(other).context("daemon stop"),
  }
}

/// Compose [`DaemonOptions`] from the parsed CLI overrides. Hidden
/// `--state-dir` / `--socket-path` flags take precedence; unset fields
/// fall back to the platform-default XDG paths. Centralised so the
/// re-exec'd child of `start_detached` honours the same priority order
/// as a hand-typed `llamastash daemon start --state-dir ...` invocation.
///
/// Discovery roots are resolved here so a real `llamastash daemon start`
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
    env_var: std::env::var_os("LLAMASTASH_LLAMA_SERVER"),
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
  opts.arch_defaults = config.arch_defaults.clone();
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

async fn handle_status(json: bool) -> Result<()> {
  let socket = runtime_socket_path();
  // Short timeout for status — agents shouldn't sit on a dead socket.
  match Client::connect(&socket).await {
    Ok(mut client) => {
      let result = client
        .call_with_timeout("version", None, Duration::from_secs(2))
        .await?;
      if json {
        // Machine contract: the raw `version` IPC response, byte-stable
        // across releases. Agents that previously piped `daemon status`
        // to `jq` should pass `--json` to keep their parser working.
        println!(
          "{}",
          serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
        );
      } else {
        print!("{}", render_daemon_status(&result));
      }
      Ok(())
    }
    Err(ClientError::Connect(_)) => {
      if json {
        // Surface a stable "not running" envelope in --json mode so
        // agents see a parseable object instead of a colored dim line.
        println!("{}", serde_json::json!({"daemon": "not_running"}));
      } else {
        println!("{}", crate::cli::colors::dim("daemon: not running"));
      }
      Ok(())
    }
    Err(other) => Err(other).context("daemon status"),
  }
}

/// Render the daemon's `version` IPC response as a labelled key/value
/// block. Falls back to pretty-JSON when the response doesn't carry the
/// expected fields so we never lose info on an unrecognised schema.
///
/// Fields surfaced (from `version` IPC): `name`, `version`,
/// `protocol_version`, `pid`, `uptime_seconds`, `connections`. Missing
/// fields render as a dim `-`.
fn render_daemon_status(body: &serde_json::Value) -> String {
  use crate::cli::{colors, format};
  use serde_json::Value;

  let Some(obj) = body.as_object() else {
    // Unexpected shape: emit a dim warning and the raw pretty-JSON so
    // the user still sees what came back. Avoids silently swallowing
    // info on an out-of-band response.
    let mut out = colors::dim("daemon: unexpected version response shape; raw body follows");
    out.push('\n');
    out.push_str(&serde_json::to_string_pretty(body).unwrap_or_else(|_| body.to_string()));
    out.push('\n');
    return out;
  };

  let dim_dash = || colors::dim("-");
  let str_field = |key: &str| {
    obj
      .get(key)
      .and_then(Value::as_str)
      .map(str::to_string)
      .unwrap_or_else(dim_dash)
  };
  let u64_field = |key: &str| {
    obj
      .get(key)
      .and_then(Value::as_u64)
      .map(|n| n.to_string())
      .unwrap_or_else(dim_dash)
  };

  let pid = obj
    .get("pid")
    .and_then(Value::as_u64)
    .map(|n| console::style(n.to_string()).bold().to_string())
    .unwrap_or_else(dim_dash);
  let uptime = obj
    .get("uptime_seconds")
    .and_then(Value::as_u64)
    .map(format::format_uptime)
    .unwrap_or_else(dim_dash);

  let mut out = String::new();
  out.push_str(&format::section_header("daemon", None));
  out.push_str(&format::kv_block(&[
    ("name", str_field("name")),
    ("version", str_field("version")),
    ("protocol", u64_field("protocol_version")),
    ("pid", pid),
    ("uptime", uptime),
    ("connections", u64_field("connections")),
  ]));
  out
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
    Cli::try_parse_from(std::iter::once("llamastash").chain(args.iter().copied())).expect("parse")
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
      "/etc/llamastash.yaml",
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
        "/etc/llamastash.yaml",
      ]
    );
  }

  #[test]
  fn propagated_cli_args_is_empty_when_no_global_flags_set() {
    let cli = parse_cli(&["daemon", "start"]);
    assert!(propagated_cli_args(&cli).is_empty());
  }

  #[test]
  fn render_daemon_status_emits_labelled_fields_for_well_formed_response() {
    let body = serde_json::json!({
      "name": "llamastash",
      "version": "0.0.1",
      "protocol_version": 1,
      "pid": 4242,
      "uptime_seconds": 90,
      "connections": 3,
    });
    let rendered = render_daemon_status(&body);
    let plain = console::strip_ansi_codes(&rendered);
    // section header + 6 kv rows.
    assert!(
      plain.starts_with("daemon\n"),
      "missing section header: {plain:?}"
    );
    assert!(plain.contains("name  llamastash"));
    assert!(plain.contains("version  0.0.1"));
    assert!(plain.contains("protocol  1"));
    assert!(plain.contains("pid  4242"));
    assert!(plain.contains("uptime  1m 30s"));
    assert!(plain.contains("connections  3"));
  }

  #[test]
  fn render_daemon_status_renders_missing_fields_as_dim_dash() {
    let body = serde_json::json!({"name": "llamastash"});
    let rendered = render_daemon_status(&body);
    let plain = console::strip_ansi_codes(&rendered);
    // pid / uptime / etc. all fall back to the "-" sentinel.
    assert!(plain.contains("name  llamastash"));
    assert!(plain.contains("pid  -"));
    assert!(plain.contains("uptime  -"));
    assert!(plain.contains("protocol  -"));
    assert!(plain.contains("connections  -"));
  }

  #[test]
  fn render_daemon_status_falls_back_to_raw_json_for_non_object_body() {
    let body = serde_json::json!([1, 2, 3]);
    let rendered = render_daemon_status(&body);
    let plain = console::strip_ansi_codes(&rendered);
    assert!(plain.contains("unexpected version response shape"));
    assert!(plain.contains("[\n  1,"));
  }
}
