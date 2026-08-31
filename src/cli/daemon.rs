//! CLI handlers for the `daemon` subcommand.
//!
//! `start [--foreground]` — launch the daemon. Default is detached: a
//! one-line "starting in background" notice prints, the parent re-execs
//! a child, waits for the socket to bind, and returns control to the
//! shell. `--foreground` (`-f`) keeps the daemon attached to the
//! controlling terminal for `systemd` / supervisor wrappers that own
//! stdout/stderr. The historical complaint was that bare `daemon start`
//! ran in the foreground and looked stuck.
//! `stop` — connect to the daemon and call `shutdown`.
//! `status` — connect to the daemon and report PID + uptime; emits "not
//! running" if the socket is missing or the connection fails.

use std::{net::IpAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};

use crate::cli::cli_args::{Cli, DaemonAction};
use crate::config::{Config, DefaultLaunchMode, DEFAULT_FIT_CTX_FLOOR, MAX_CTX_TOKENS};
use crate::daemon::discovery_task::DiscoveryOptions;
use crate::daemon::{
  existing_daemon_pid, run_foreground, runtime_file, start_detached, DaemonOptions, StartOutcome,
};
use crate::discovery::known_caches::{default_set, RootResolution};
use crate::ipc::{Client, ClientError};
use crate::launch::binary::{locate as locate_binary, LocateInputs};
use crate::util::paths::{home_dir, state_dir};

/// Top-level dispatch for `daemon <action>`. The full `Cli` and merged
/// `Config` flow through so `handle_start` can resolve discovery roots
/// from user flags + config; status / stop ignore them.
pub async fn handle(action: DaemonAction, cli: &Cli, config: &Config) -> Result<()> {
  match action {
    DaemonAction::Start {
      foreground,
      state_dir,
      proxy_port,
      ollama_compat,
      no_proxy_fallback,
      proxy_host,
      insecure_no_auth,
      lemonade,
      ds4,
      vllm,
      force,
    } => {
      handle_start(
        foreground,
        state_dir,
        proxy_port,
        ollama_compat,
        no_proxy_fallback,
        proxy_host,
        insecure_no_auth,
        lemonade,
        ds4,
        vllm,
        force,
        cli,
        config,
      )
      .await
    }
    DaemonAction::Stop { force } => handle_stop(force).await,
    DaemonAction::Status { json } => handle_status(json).await,
  }
}

/// Migrate `config.yaml` to the unified knob shape, returning the re-parsed
/// config when a rewrite happened (plan D10).
///
/// Failure is never fatal: a config we could not rewrite still loads, because
/// knob keys normalise on read either way. The user is told what happened, and
/// where the backup is, so a surprising rewrite is recoverable.
fn migrate_knob_config(cli: &Cli) -> Option<Config> {
  let path = crate::config::config_path(cli.config.clone())?;
  match crate::config::knob_migration::migrate(&path) {
    Ok(Some(report)) => {
      log::info!("{report}");
      if !cli.quiet {
        eprintln!("{}", crate::cli::colors::success(&report.to_string()));
      }
      Some(crate::config::load_config_from_path(&path).config)
    }
    Ok(None) => None,
    Err(e) => {
      log::warn!("config knob migration skipped: {e}");
      None
    }
  }
}

// `handle_start` is the single thin shim that unpacks every
// `daemon start` flag and feeds them into `build_options`. Each new
// CLI flag added here costs an argument; the alternative (a typed
// `StartFlags` struct) would just push the unpack one level out
// without changing how much information crosses the boundary. Allow
// the count to grow with the CLI surface instead.
#[allow(clippy::too_many_arguments)]
async fn handle_start(
  foreground: bool,
  state_dir: Option<PathBuf>,
  proxy_port: Option<u16>,
  ollama_compat: bool,
  no_proxy_fallback: bool,
  proxy_host: Option<IpAddr>,
  insecure_no_auth: bool,
  lemonade: bool,
  ds4: bool,
  vllm: bool,
  force: bool,
  cli: &Cli,
  config: &Config,
) -> Result<()> {
  // Bring a pre-registry `config.yaml` to the unified knob shape before
  // anything reads it. The daemon owns config writes, so this is the one
  // place it can run; a plain CLI command must never rewrite the user's file.
  // Idempotent, backs the original up first, and preserves comments.
  let migrated_config = migrate_knob_config(cli);

  let mut opts = build_options(
    state_dir,
    proxy_port,
    ollama_compat,
    no_proxy_fallback,
    proxy_host,
    insecure_no_auth,
    lemonade,
    ds4,
    vllm,
    cli,
    config,
  )?;
  if let Some(fresh) = migrated_config {
    // `config` was parsed from the pre-migration text, so its preset blocks
    // are in the old shape. Take the rewritten file's.
    opts.presets = fresh.presets;
  }
  // Provision the proxy bearer key when a LAN bind is requested. Runs
  // in the parent (or the foreground process) so the generated key is
  // printed to the user's terminal; the detached child re-reads it
  // from config. No-op for loopback / pre-set key / --insecure-no-auth.
  provision_proxy_key(&mut opts, cli, foreground)?;
  // Ride `--force` through to the detached child so it skips the precheck too.
  opts.force = force;
  // Fail-fast: refuse to come up silently degraded when an *indicated* backend
  // can't initialize. `--force` opts out (start degraded; the failed backend is
  // simply unavailable). Skipped when a daemon is already running — that call
  // short-circuits to AlreadyRunning, and a precheck would wrongly flag the
  // umbrella port the running daemon legitimately holds.
  if !force && existing_daemon_pid(&opts.state_dir).is_none() {
    if let Err(msg) = precheck_indicated_backends(&opts) {
      anyhow::bail!(msg);
    }
  }
  if foreground {
    // `--foreground` (or `-f`) keeps the daemon attached to the
    // controlling terminal. Print a one-line notice up front so the
    // user sees *why* the prompt isn't coming back — otherwise the
    // silent hand-off makes a fresh `daemon start --foreground` look
    // stuck. Suppressed when an existing daemon already owns the
    // lockfile (we'd flash the notice then immediately fall through
    // to "already running", which would be confusing).
    if existing_daemon_pid(&opts.state_dir).is_none() {
      println!(
        "{}",
        crate::cli::colors::dim(
          "daemon: running in foreground — Ctrl+C to stop, or omit -f to background it",
        )
      );
    }
    match run_foreground(opts).await? {
      StartOutcome::RanToCompletion => Ok(()),
      StartOutcome::AlreadyRunning(pid) => {
        print_already_running(pid);
        Ok(())
      }
    }
  } else {
    // Default: detach into the background. The hand-off looks like
    // a hang otherwise (the parent waits for the child to bind its
    // socket — usually <100 ms, but a slow disk can stretch it), so
    // print a "starting" notice first and a green-check confirmation
    // once `start_detached` returns. Suppressed when an existing
    // daemon already owns the lockfile (skips straight to the
    // "already running" line below).
    if existing_daemon_pid(&opts.state_dir).is_none() {
      println!(
        "{}",
        crate::cli::colors::dim("daemon: starting in background…")
      );
    }
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
  }
}

/// Fail-fast gate for `daemon start`: refuse to come up silently degraded when
/// an *indicated* backend can't initialize. llama.cpp is always indicated, so a
/// missing `llama-server` fails; Lemonade is indicated only when enabled, so a
/// missing `lemond` binary or an already-held umbrella port fails. `--force`
/// skips this whole gate and starts degraded. The port check is a fast
/// bind-probe (not a readiness wait), so it never delays startup; every message
/// names the override so the user can get past it deliberately.
///
/// Every indicated backend is checked (not just the first failure) so one run
/// reports everything that needs fixing. The `Err` joins one single-line
/// failure per backend with `\n` — the TUI's Daemon panel splits on lines to
/// render each backend's failure separately.
pub(crate) fn precheck_indicated_backends(opts: &DaemonOptions) -> std::result::Result<(), String> {
  let mut failures: Vec<String> = Vec::new();
  if opts.binary.is_none() {
    failures.push(
      "llama-server binary not found — point `--llama-server` / `LLAMASTASH_LLAMA_SERVER` at it, \
       run `llamastash init` to install one, or `llamastash daemon start --force` to start without \
       llama.cpp."
        .to_string(),
    );
  }
  // Lemonade is flagged only when *explicitly* requested (`--lemonade` / env,
  // or `lemonade.enabled: true`); the default-on-when-found path stays silent
  // when `lemond` is simply absent (zero footprint, like ds4).
  let lemonade_force = opts
    .backend_force
    .get(crate::backend::lemonade::LEMONADE_BACKEND_ID)
    .copied()
    .unwrap_or(false);
  let lemonade_explicit = lemonade_force || opts.backend.lemonade.enabled == Some(true);
  if opts.backend.lemonade.intends_enabled(lemonade_force) {
    if crate::backend::lemonade::resolve_lemond_binary(&opts.backend.lemonade).is_none() {
      if lemonade_explicit {
        failures.push(
          "lemonade is enabled but no `lemond` binary was found — set `lemonade.binary` or put \
           `lemond` on PATH (see docs/lemonade-setup.md), or `llamastash daemon start --force` to \
           start without it."
            .to_string(),
        );
      }
    } else if crate::backend::lemonade::umbrella_port_state(opts.backend.lemonade.port)
      == crate::backend::lemonade::UmbrellaPortState::Listening
    {
      // Only probed when a `lemond` binary resolved: without one the port
      // conflict is moot and reporting both would just be noise. Only a
      // *live listener* refuses; teardown remnants from a just-stopped
      // daemon's `lemond` (FIN-WAIT-2 / TIME-WAIT) clear within the
      // kernel's fin-timeout, and the boot-side umbrella supervision
      // waits them out — refusing here made every quick
      // `daemon stop && daemon start --lemonade` fail for up to a minute.
      failures.push(format!(
        "lemonade umbrella port 127.0.0.1:{} is already in use — stop whatever holds it \
         (e.g. a manually started `lemond`) or set `lemonade.port`, or `llamastash daemon start \
         --force` to start without the managed umbrella.",
        opts.backend.lemonade.port
      ));
    }
  }
  // ds4 is indicated only when *explicitly* requested (`--ds4` / env, or
  // `ds4.enabled: true`) — the default-on-when-found path stays silent when
  // the binary is simply absent (zero footprint, D4). An explicit request
  // with no resolvable binary fails fast, naming the configured path.
  let ds4_force = opts
    .backend_force
    .get(crate::backend::ds4::DS4_BACKEND_ID)
    .copied()
    .unwrap_or(false);
  let ds4_explicit = ds4_force || opts.backend.ds4.enabled == Some(true);
  if ds4_explicit
    && crate::backend::ds4::resolve_ds4_binary(opts.backend.ds4.primary_binary()).is_none()
  {
    let where_ = match opts.backend.ds4.primary_binary() {
      Some(p) => format!("`ds4.servers` ({})", p.display()),
      None => "`ds4-server` on PATH".to_string(),
    };
    failures.push(format!(
      "ds4 was requested but no `ds4-server` binary was found at {where_} — build ds4-server and \
       add it to `backend.ds4.servers` (see docs/usage.md), or `llamastash daemon start --force` to start without it."
    ));
  }
  if failures.is_empty() {
    Ok(())
  } else {
    Err(failures.join("\n"))
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

/// Provision the proxy bearer key when a non-loopback bind is
/// requested. Runs in the CLI parent (and the foreground path) so the
/// generated key reaches the user's terminal. No-op for a loopback
/// bind, when a key is already set (config or `LLAMASTASH_PROXY_API_KEY`),
/// or when `--insecure-no-auth` waives auth. Otherwise it generates a
/// key, persists `proxy.api_key` to `config.yaml` (atomic, mode 0600),
/// sets it on `opts`, and prints it once.
///
/// Idempotent across the detached re-exec: the child re-reads the now-
/// persisted key from config, so this short-circuits without
/// regenerating or reprinting.
///
/// `foreground` controls how a persistence failure is handled. The
/// generated key lives on `opts` regardless, so a `--foreground` daemon
/// (same process) keeps working for this run even if the write failed.
/// A detached daemon re-execs and reads the key back from config — the
/// key is never passed via argv — so if the write failed the child
/// can't see it, hits the backstop, and silently drops the proxy. In
/// that case we refuse to start rather than print a key that will never
/// authenticate.
fn provision_proxy_key(opts: &mut DaemonOptions, cli: &Cli, foreground: bool) -> Result<()> {
  let host = opts.proxy.effective_host();
  // Loopback needs no key; a configured / env key is used as-is (and an
  // env key is deliberately never written back to disk).
  if host.is_loopback() || opts.proxy.api_key.is_some() {
    return Ok(());
  }
  let port = opts.proxy.effective_port();
  if opts.proxy.insecure_no_auth {
    eprintln!(
      "{}",
      crate::cli::colors::warning(&format!(
        "proxy: binding {host}:{port} with NO authentication (--insecure-no-auth). \
         Anyone who can reach this address can use your models — trusted networks only."
      ))
    );
    return Ok(());
  }
  let key = crate::proxy::ProxyApiKey::generate();
  let key_str = key.as_str().to_string();
  opts.proxy.api_key = Some(key_str.clone());
  let persisted = match crate::config::config_path(cli.config.clone()) {
    Some(path) => {
      match crate::config::writer::merge_and_write(&path, proxy_api_key_additions(&key_str)) {
        Ok(_) => true,
        Err(e) => {
          log::warn!("failed to persist generated proxy api_key: {e}");
          false
        }
      }
    }
    None => {
      log::warn!("no writable config path; generated proxy api_key was not persisted");
      false
    }
  };
  // A detached daemon reads the key back from config; an unpersisted key
  // can't reach it, so the proxy would refuse to bind while we'd have
  // claimed LAN access is up. Fail loudly instead.
  if !persisted && !foreground {
    return Err(anyhow::anyhow!(
      "proxy: generated a LAN API key but could not save it to config, so the \
       backgrounded daemon can't read it back and the proxy would not start. Set a \
       writable config (e.g. --config <path>) or set proxy.api_key yourself, or run \
       with --foreground. Daemon not started."
    ));
  }
  print_provisioned_key(host, port, &key_str, persisted);
  Ok(())
}

/// Build the `{ proxy: { api_key: <key> } }` YAML fragment the config
/// merge persists. Nested so the recursive merge sets only `api_key`
/// and preserves the user's other `proxy` keys.
fn proxy_api_key_additions(key: &str) -> yaml_serde::Value {
  let mut proxy = yaml_serde::Mapping::new();
  proxy.insert(
    yaml_serde::Value::String("api_key".into()),
    yaml_serde::Value::String(key.to_string()),
  );
  let mut root = yaml_serde::Mapping::new();
  root.insert(
    yaml_serde::Value::String("proxy".into()),
    yaml_serde::Value::Mapping(proxy),
  );
  yaml_serde::Value::Mapping(root)
}

/// One-time banner shown when a LAN proxy key is auto-generated. When
/// `persisted` the key is saved to config and reused on the next start;
/// otherwise (a foreground run whose write failed) it lives only in
/// this process and a fresh key is generated next time — the banner
/// says so rather than claiming it was saved.
fn print_provisioned_key(host: IpAddr, port: u16, key: &str, persisted: bool) {
  println!(
    "{}",
    crate::cli::colors::success(&format!("proxy: LAN access enabled on {host}:{port}"))
  );
  let save_note = if persisted {
    "proxy: generated an API key (saved to your config, shown once):"
  } else {
    "proxy: generated an API key (could NOT save to config — valid for this run only, regenerates next start):"
  };
  println!("{}", crate::cli::colors::dim(save_note));
  println!("    {key}");
  // For 0.0.0.0 / :: the user must substitute the box's LAN IP; show a
  // bearer-token usage hint either way.
  let example_host = if host.is_unspecified() {
    "<lan-ip>".to_string()
  } else {
    host.to_string()
  };
  println!(
    "{}",
    crate::cli::colors::dim(&format!(
      "    use it as a bearer token, e.g.\n    \
       curl http://{example_host}:{port}/v1/models -H \"Authorization: Bearer {key}\""
    ))
  );
}

async fn handle_stop(force: bool) -> Result<()> {
  let attach_dir = state_dir().context("could not resolve state directory")?;
  if !force {
    match Client::connect(&attach_dir).await {
      Ok(mut client) => {
        let _ = client.call("shutdown", None).await?;
        // Wait (bounded) for the process to actually exit. `shutdown`
        // only *requests* teardown; returning while the old daemon
        // still holds the lockfile (and its `lemond` umbrella is still
        // dying) makes a chained `daemon stop && daemon start` race
        // straight into "already running" / a half-released umbrella
        // port. Ten seconds covers the slowest observed teardown
        // (umbrella SIGTERM→SIGKILL escalation is 5 s); on timeout we
        // fall back to the old fire-and-forget message.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
          match existing_daemon_pid(&attach_dir) {
            None => {
              println!("{}", crate::cli::colors::success("daemon: stopped"));
              return Ok(());
            }
            Some(pid) => {
              if std::time::Instant::now() >= deadline {
                println!(
                  "{} ({} {})",
                  crate::cli::colors::success("daemon: shutdown requested"),
                  crate::cli::colors::dim("still exiting, pid"),
                  pid
                );
                return Ok(());
              }
              tokio::time::sleep(Duration::from_millis(100)).await;
            }
          }
        }
      }
      // No IPC channel — either the daemon is genuinely down, or it's
      // a stale process that didn't publish runtime.json. The
      // `existing_daemon_pid` check below distinguishes the two.
      Err(ClientError::Connect(_)) => {}
      Err(other) => return Err(other).context("daemon stop"),
    }
  }
  match existing_daemon_pid(&attach_dir) {
    None => {
      println!("{}", crate::cli::colors::dim("daemon: not running"));
      Ok(())
    }
    Some(pid) => force_stop_via_pid(pid, &attach_dir),
  }
}

/// Best-effort PID-based shutdown. Used when the IPC channel is
/// unusable (no `runtime.json`) or when the user passed `--force`.
/// Sends `SIGTERM` via [`ProcessControl`], waits up to ~3s for the
/// lockfile to release, then surfaces a clear next-step (`SIGKILL`)
/// if the daemon ignores the signal.
fn force_stop_via_pid(pid: i32, attach_dir: &std::path::Path) -> Result<()> {
  use crate::util::process_control::{platform_default, SignalTarget};
  use std::time::{Duration, Instant};
  if pid <= 0 {
    return Err(anyhow::anyhow!(
      "daemon stop --force: invalid pid {pid} in lockfile"
    ));
  }
  let pc = platform_default();
  let pid_u = pid as u32;
  // Pre-check via `is_alive` so we can surface "already exited" without
  // having to inspect signal-syscall errno. The trait swallows ESRCH
  // internally, which is the right behavior for the supervisor but
  // would hide the user-relevant distinction here.
  if !pc.is_alive(pid_u) {
    println!(
      "{}",
      crate::cli::colors::dim(&format!("daemon: pid {pid} already exited"))
    );
    return Ok(());
  }
  pc.signal_graceful(SignalTarget::SinglePid(pid_u));
  let deadline = Instant::now() + Duration::from_secs(3);
  while Instant::now() < deadline {
    if existing_daemon_pid(attach_dir).is_none() {
      // A hard kill skips the daemon's own shutdown cleanup, so the
      // `runtime.json` it published outlives it — pointing at a now-dead
      // control-plane URL. Remove it here so the next CLI/TUI launch
      // sees "no daemon" and auto-spawns cleanly instead of trying the
      // stale URL. (The client also self-heals via the PID lock, but
      // leaving clean state is tidier and avoids a wasted connect.)
      runtime_file::remove(attach_dir);
      println!(
        "{}",
        crate::cli::colors::success(&format!("daemon: stopped (pid {pid})"))
      );
      return Ok(());
    }
    std::thread::sleep(Duration::from_millis(50));
  }
  Err(anyhow::anyhow!(
    "daemon stop --force: pid {pid} did not exit within 3s after SIGTERM; \
     try `kill -KILL {pid}` and check the daemon logs"
  ))
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
// Same rationale as `handle_start`: each `daemon start` knob costs an
// argument here. A typed bundle would just relocate the unpack.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_options(
  state_dir: Option<PathBuf>,
  proxy_port: Option<u16>,
  ollama_compat_cli: bool,
  no_proxy_fallback_cli: bool,
  proxy_host: Option<IpAddr>,
  insecure_no_auth_cli: bool,
  lemonade_cli: bool,
  ds4_cli: bool,
  vllm_cli: bool,
  cli: &Cli,
  config: &Config,
) -> Result<DaemonOptions> {
  let env_paths = env_model_paths();
  let env_no_scan_v = env_no_scan();
  // Refuse to start with discovery completely off and no user-supplied
  // paths. Without this the daemon would come up healthy, the catalog
  // would stay empty forever, and the user would see "no models found"
  // with no signal that it's a config dead-end.
  crate::config::validate_scan_settings(
    cli.no_scan || env_no_scan_v || config.disable_scan,
    &cli.model_paths,
    &env_paths,
    &config.model_paths,
  )
  .map_err(anyhow::Error::from)?;

  // Same posture for the launch port range: the allocator is the only
  // thing that rejects an inverted / zero-start range, and it doesn't run
  // until the first `start` — so without this the daemon boots healthy and
  // the typo resurfaces later as a launch failure naming no config key.
  crate::config::validate_port_range(&config.daemon.port_range).map_err(anyhow::Error::from)?;

  let mut opts = DaemonOptions::from_defaults()?;
  if let Some(p) = state_dir {
    opts.state_dir = p;
  }
  let scan_roots = resolve_scan_roots(
    cli,
    config,
    &env_paths,
    env_no_scan_v,
    home_dir().as_deref(),
  );
  opts.discovery = DiscoveryOptions::new(scan_roots);
  // Best-effort `llama-server` resolution. A miss leaves
  // `opts.binary = None`; the daemon still starts and `start_model`
  // surfaces an actionable error to the caller. We log so the user
  // sees *why* a later launch fails.
  opts.binary = match locate_binary(LocateInputs {
    cli_flag: cli.llama_server.clone(),
    env_var: std::env::var_os("LLAMASTASH_LLAMA_SERVER"),
    config_path: config.backend.llamacpp.primary_binary(),
  }) {
    Ok(p) => Some(p),
    Err(e) => {
      log::warn!("llama-server lookup failed: {e}");
      None
    }
  };
  // Additional `llama-server` servers (multi-backend installs) are resolved by
  // the llama.cpp backend's own `configured_servers` hook when the boot builds
  // the server catalog — no separate resolution here.
  opts.port_range = config.daemon.port_range;
  opts.probe_timeout_secs = Some(config.daemon.probe_timeout_secs);
  opts.idle_timeout = config.daemon.idle_timeout();
  opts.metrics_interval = config.daemon.metrics_interval();
  opts.gpu_reprobe_interval = config.gpu.reprobe_interval();
  opts.arch_defaults = config.arch_defaults.clone();
  // Config presets seed the daemon's in-memory store; `config_path` is where
  // preset writes land.
  opts.presets = config.presets.clone();
  opts.config_path = crate::config::config_path(cli.config.clone());

  // Auto-fit launch options: config layer first, then the
  // `LLAMASTASH_*` env overrides. A bad env value is logged and ignored
  // so a typo never blocks daemon startup — the config / factory value
  // still applies.
  opts.default_launch_mode = config.default_launch_mode;
  if let Some(raw) = std::env::var_os("LLAMASTASH_DEFAULT_LAUNCH_MODE") {
    match raw.to_string_lossy().trim().to_ascii_lowercase().as_str() {
      "auto" => opts.default_launch_mode = DefaultLaunchMode::Auto,
      "inherited" => opts.default_launch_mode = DefaultLaunchMode::Inherited,
      other => log::warn!(
        "ignoring LLAMASTASH_DEFAULT_LAUNCH_MODE={other:?}: expected `auto` or `inherited`"
      ),
    }
  }
  // Backend config: clone the whole `backend:` block (llama.cpp knobs +
  // lemonade + ds4), then apply the `LLAMASTASH_*` env overrides + range clamp
  // onto the llama.cpp fit knobs. `jinja` stays config-only (factory `true`) —
  // unlike the opt-in booleans below it defaults *on* and the `"1"`-truthy env
  // contract can't express "force off" — so it rides through from the clone.
  opts.backend = config.backend.clone();
  if let Some(raw) = std::env::var_os("LLAMASTASH_FIT_CTX_FLOOR") {
    let s = raw.to_string_lossy();
    match s.trim().parse::<u32>() {
      Ok(v) => opts.backend.llamacpp.fit_ctx_floor = v,
      Err(_) => log::warn!("ignoring LLAMASTASH_FIT_CTX_FLOOR={s:?}: not a positive integer"),
    }
  }
  if opts.backend.llamacpp.fit_ctx_floor == 0
    || opts.backend.llamacpp.fit_ctx_floor > MAX_CTX_TOKENS
  {
    log::warn!(
      "fit_ctx_floor {} out of range (1..={MAX_CTX_TOKENS}); using factory {DEFAULT_FIT_CTX_FLOOR}",
      opts.backend.llamacpp.fit_ctx_floor
    );
    opts.backend.llamacpp.fit_ctx_floor = DEFAULT_FIT_CTX_FLOOR;
  }
  // Strict-fit is an opt-in: config OR `LLAMASTASH_STRICT_FIT=1` (the
  // strict-`"1"` env contract shared with the other boolean envs).
  opts.backend.llamacpp.strict_fit =
    config.backend.llamacpp.strict_fit || env_flag_truthy("LLAMASTASH_STRICT_FIT");
  // Proxy: config layer first, then CLI / env overrides. Without this
  // thread-through the daemon silently ignored `proxy:` from the config
  // file and ran with `ProxyConfig::default()` regardless.
  opts.proxy = config.proxy.clone();
  if let Some(p) = proxy_port {
    opts.proxy.port = Some(p);
  }
  // Proxy bind host: CLI > env (`LLAMASTASH_PROXY_HOST`) > config.
  // A bad env value is logged and ignored rather than failing startup
  // — the config / default host still applies.
  if let Some(h) = proxy_host {
    opts.proxy.host = Some(h);
  } else if let Some(raw) = std::env::var_os("LLAMASTASH_PROXY_HOST") {
    match raw.to_string_lossy().trim().parse::<IpAddr>() {
      Ok(h) => opts.proxy.host = Some(h),
      Err(e) => log::warn!(
        "ignoring LLAMASTASH_PROXY_HOST={:?}: not a valid IP address ({e})",
        raw
      ),
    }
  }
  // Insecure-no-auth opt-out: OR of (config field, `--insecure-no-auth`
  // CLI flag, `LLAMASTASH_PROXY_INSECURE_NO_AUTH` env var). Any one of
  // the three waives the LAN auth requirement; the key auto-provision
  // path and the daemon backstop both read the resolved value.
  let env_insecure = env_flag_truthy("LLAMASTASH_PROXY_INSECURE_NO_AUTH");
  opts.proxy.insecure_no_auth = opts.proxy.insecure_no_auth || insecure_no_auth_cli || env_insecure;
  // Proxy API key: fold the `LLAMASTASH_PROXY_API_KEY` env override
  // (wins over config, never written back to disk — containers /
  // secret managers) and blank-normalization into one resolved value
  // via `ProxyConfig::effective_api_key`, the shared resolver the init
  // wizard's external-tool writers also read. Blank on either side →
  // `None` is load-bearing: it makes the fail-closed backstop refuse
  // (or the CLI provision a real key) instead of binding an
  // *unauthenticated* LAN proxy while `ProxyAuth` treats the blank key
  // as no auth and skips the refusal.
  opts.proxy.api_key = opts.proxy.effective_api_key();
  // Ollama-compat: OR of (config field, `--ollama-compat` CLI flag,
  // `LLAMASTASH_OLLAMA_COMPAT` env var). Any one of the three enables
  // the mode — clearing it requires unsetting all three. The env var
  // accepts the usual truthy strings; anything else (incl. unset) is
  // treated as false.
  let env_compat = env_flag_truthy("LLAMASTASH_OLLAMA_COMPAT");
  opts.proxy.ollama_compat = opts.proxy.ollama_compat || ollama_compat_cli || env_compat;
  // Fallback-disable: OR of (config field cleared, `--no-proxy-fallback`
  // CLI flag, `LLAMASTASH_NO_PROXY_FALLBACK` env var). Any one of the
  // three turns the family-MRU fallback off — re-enabling requires
  // unsetting all of them and keeping `fallback_enabled: true` in the
  // config (which is the default). The CLI flag can only disable; we
  // never re-enable from CLI on top of a config-level `false`.
  let env_no_fallback = env_flag_truthy("LLAMASTASH_NO_PROXY_FALLBACK");
  if no_proxy_fallback_cli || env_no_fallback {
    opts.proxy.fallback_enabled = false;
  }
  // Per-backend force-enable map, keyed by backend id. Lemonade + ds4 both
  // default-on when their binary resolves (config `[lemonade]` / `[ds4]` blocks
  // already rode through in `opts.backend` above); `--lemonade` /
  // `LLAMASTASH_LEMONADE` and `--ds4` / `LLAMASTASH_DS4` force each on over a
  // config `enabled: false`. Kept separate from `opts.backend` so the detached
  // re-exec can re-append the flags (env/flag don't survive detach). The two id
  // consts are the kept CLI-flag boundary.
  opts.backend_force = [
    (
      crate::backend::lemonade::LEMONADE_BACKEND_ID.to_string(),
      lemonade_cli || env_flag_truthy("LLAMASTASH_LEMONADE"),
    ),
    (
      crate::backend::ds4::DS4_BACKEND_ID.to_string(),
      ds4_cli || env_flag_truthy("LLAMASTASH_DS4"),
    ),
    (
      crate::backend::vllm::VLLM_BACKEND_ID.to_string(),
      vllm_cli || env_flag_truthy("LLAMASTASH_VLLM"),
    ),
  ]
  .into_iter()
  .collect();
  opts.propagated_cli_args = propagated_cli_args(cli);
  Ok(opts)
}

/// True when the named env var is set to a recognised truthy string.
/// Accepts the usual flavours (`1`, `true`, `yes`, `on`) regardless of
/// case; empty / unset / anything else is false. Trimmed before
/// matching so a stray newline (common in `.env` files) doesn't bite.
fn env_flag_truthy(name: &str) -> bool {
  std::env::var(name)
    .ok()
    .map(|v| {
      matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
      )
    })
    .unwrap_or(false)
}

/// Parse `LLAMASTASH_MODEL_PATHS` into a path list, using the
/// platform's path separator (`:` on POSIX, `;` on Windows) via
/// `std::env::split_paths`. Unset / empty / all-empty-entries returns
/// an empty vec. Empty entries (`""` between separators) are skipped
/// rather than producing `PathBuf::new()` — a stray colon shouldn't
/// register as "scan the empty path".
pub(crate) fn env_model_paths() -> Vec<PathBuf> {
  std::env::var_os("LLAMASTASH_MODEL_PATHS")
    .map(|raw| {
      std::env::split_paths(&raw)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
    })
    .unwrap_or_default()
}

/// True when `LLAMASTASH_NO_SCAN` is set to a truthy value. Same
/// truthy set as `env_flag_truthy` — matches the documented
/// `LLAMASTASH_NO_SCAN=1` recipe in the README, and accepts
/// `true`/`yes`/`on` (case-insensitive) for parity with other env
/// flags in this binary.
pub(crate) fn env_no_scan() -> bool {
  env_flag_truthy("LLAMASTASH_NO_SCAN")
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

/// Merge CLI + env + config + default-cache enumeration into the
/// canonical scan-root list the daemon should walk. Exposed as a pure
/// function so unit tests can drive it without touching process env.
///
/// Priority for `no_scan`: any of (`--no-scan`,
/// `LLAMASTASH_NO_SCAN=1`, `disable_scan: true`) turns the default-
/// cache walk off. Model paths from all three sources are merged and
/// de-duplicated; merge order is `config > env > cli` (irrelevant for
/// correctness since the list is order-insensitive at the scanner,
/// but it keeps the daemon's `ps`-visible argv readable).
pub(crate) fn resolve_scan_roots(
  cli: &Cli,
  config: &Config,
  env_paths: &[PathBuf],
  env_no_scan: bool,
  home: Option<&std::path::Path>,
) -> Vec<crate::discovery::scanner::ScanRoot> {
  let mut user_paths: Vec<PathBuf> = config.model_paths.clone();
  for p in env_paths {
    if !user_paths.iter().any(|x| x == p) {
      user_paths.push(p.clone());
    }
  }
  for p in &cli.model_paths {
    if !user_paths.iter().any(|x| x == p) {
      user_paths.push(p.clone());
    }
  }
  let no_scan = cli.no_scan || env_no_scan || config.disable_scan;
  default_set(RootResolution {
    user_paths: &user_paths,
    disable: &config.disable_default_cache_paths,
    no_scan,
    home,
  })
}

async fn handle_status(json: bool) -> Result<()> {
  let attach_dir = state_dir().context("could not resolve state directory")?;
  // Short timeout for status — agents shouldn't sit on a dead daemon.
  match Client::connect(&attach_dir).await {
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
      // A live process owning `daemon.pid` but no `runtime.json` is a
      // stale daemon from an older binary (or a crash between
      // lock-acquire and runtime-file save). Report it distinctly so
      // an operator knows to run `daemon stop --force` instead of
      // assuming the daemon is genuinely down.
      let stale_pid = existing_daemon_pid(&attach_dir);
      if json {
        let envelope = match stale_pid {
          Some(pid) => serde_json::json!({"daemon": "stale", "pid": pid}),
          None => serde_json::json!({"daemon": "not_running"}),
        };
        println!("{envelope}");
      } else {
        match stale_pid {
          Some(pid) => {
            println!(
              "{}",
              crate::cli::colors::dim(&format!(
                "daemon: stale (pid {pid}) — run `llamastash daemon stop --force` to recover"
              ))
            );
          }
          None => {
            println!("{}", crate::cli::colors::dim("daemon: not running"));
          }
        }
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
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
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
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
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
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
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
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
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
    let roots = resolve_scan_roots(&cli, &config, &[], false, Some(&home));
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

  #[test]
  fn build_options_threads_config_proxy_block_into_daemon_options() {
    let _env = crate::cli::test_lock::serialize();
    // Regression: before this wiring landed, config.proxy.port was
    // parsed and validated but `build_options` never copied it onto
    // DaemonOptions.proxy. The daemon silently ran with
    // ProxyConfig::default() no matter what the user put in
    // config.yaml.
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      proxy: crate::config::loader::ProxyConfig {
        enabled: true,
        port: Some(22222),
        ollama_compat: false,
        fallback_enabled: true,
        header_read_timeout_secs: 45,
        idle_ttl_secs: 1800,
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert_eq!(
      opts.proxy.port,
      Some(22222),
      "config proxy.port must reach daemon"
    );
    assert_eq!(opts.proxy.effective_port(), 22222);
    assert_eq!(opts.proxy.header_read_timeout_secs, 45);
    assert!(opts.proxy.enabled);
    assert!(!opts.proxy.ollama_compat);
  }

  #[test]
  fn build_options_threads_the_daemon_and_gpu_blocks_into_daemon_options() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      daemon: crate::config::DaemonConfig {
        port_range: crate::config::PortRange {
          start: 50000,
          end: 50100,
        },
        probe_timeout_secs: 600,
        idle_timeout_secs: 900,
        metrics_interval_secs: 5,
      },
      gpu: crate::config::GpuConfig {
        enable_vulkan_probe: false,
        reprobe_interval_secs: 120,
      },
      ..Config::default()
    };
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert_eq!(opts.port_range.start, 50000);
    assert_eq!(opts.probe_timeout_secs, Some(600));
    assert_eq!(opts.idle_timeout, Some(Duration::from_secs(900)));
    assert_eq!(opts.metrics_interval, Duration::from_secs(5));
    assert_eq!(opts.gpu_reprobe_interval, Some(Duration::from_secs(120)));
  }

  /// `0` means "off" for the two optional timers and "use the factory"
  /// for the sampler cadence — the asymmetry is deliberate, since a
  /// zero-second tick would busy-loop.
  #[test]
  fn build_options_resolves_zero_valued_daemon_and_gpu_keys() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      daemon: crate::config::DaemonConfig {
        idle_timeout_secs: 0,
        metrics_interval_secs: 0,
        ..Default::default()
      },
      gpu: crate::config::GpuConfig {
        reprobe_interval_secs: 0,
        ..Default::default()
      },
      ..Config::default()
    };
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert_eq!(opts.idle_timeout, None, "0 disables the idle timer");
    assert_eq!(
      opts.gpu_reprobe_interval, None,
      "0 disables periodic re-probes"
    );
    assert_eq!(
      opts.metrics_interval,
      Duration::from_secs(1),
      "0 must not become a zero-length sampler tick"
    );
  }

  #[test]
  fn build_options_clamps_an_out_of_range_metrics_interval() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      daemon: crate::config::DaemonConfig {
        metrics_interval_secs: 9_000,
        ..Default::default()
      },
      ..Config::default()
    };
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert_eq!(opts.metrics_interval, Duration::from_secs(60));
  }

  #[test]
  fn build_options_threads_auto_fit_options_from_config() {
    let _env = crate::cli::test_lock::serialize();
    std::env::remove_var("LLAMASTASH_DEFAULT_LAUNCH_MODE");
    std::env::remove_var("LLAMASTASH_FIT_CTX_FLOOR");
    std::env::remove_var("LLAMASTASH_STRICT_FIT");
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      default_launch_mode: DefaultLaunchMode::Inherited,
      backend: crate::backend::BackendConfig {
        llamacpp: crate::config::LlamaCppConfig {
          fit_ctx_floor: 8192,
          strict_fit: true,
          jinja: false,
          ..Default::default()
        },
        ..Default::default()
      },
      ..Config::default()
    };
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert_eq!(opts.default_launch_mode, DefaultLaunchMode::Inherited);
    assert_eq!(opts.backend.llamacpp.fit_ctx_floor, 8192);
    assert!(opts.backend.llamacpp.strict_fit);
    // `jinja: false` from config threads through (factory default is true).
    assert!(!opts.backend.llamacpp.jinja);
  }

  #[test]
  fn build_options_auto_fit_env_overrides_config() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      default_launch_mode: DefaultLaunchMode::Auto,
      backend: crate::backend::BackendConfig {
        llamacpp: crate::config::LlamaCppConfig {
          strict_fit: false,
          ..Default::default()
        },
        ..Default::default()
      },
      ..Config::default()
    };
    std::env::set_var("LLAMASTASH_DEFAULT_LAUNCH_MODE", "inherited");
    std::env::set_var("LLAMASTASH_STRICT_FIT", "1");
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    std::env::remove_var("LLAMASTASH_DEFAULT_LAUNCH_MODE");
    std::env::remove_var("LLAMASTASH_STRICT_FIT");
    assert_eq!(
      opts.default_launch_mode,
      DefaultLaunchMode::Inherited,
      "env overrides config for launch mode"
    );
    assert!(
      opts.backend.llamacpp.strict_fit,
      "LLAMASTASH_STRICT_FIT=1 enables strict"
    );
  }

  #[test]
  fn build_options_fit_ctx_floor_out_of_range_falls_back_to_factory() {
    let _env = crate::cli::test_lock::serialize();
    std::env::remove_var("LLAMASTASH_FIT_CTX_FLOOR");
    let cli = parse_cli(&["daemon", "start"]);
    for bad in [0u32, 2_000_000] {
      let config = Config {
        backend: crate::backend::BackendConfig {
          llamacpp: crate::config::LlamaCppConfig {
            fit_ctx_floor: bad,
            ..Default::default()
          },
          ..Default::default()
        },
        ..Config::default()
      };
      let opts = build_options(
        None, None, false, false, None, false, false, false, false, &cli, &config,
      )
      .expect("build_options");
      assert_eq!(
        opts.backend.llamacpp.fit_ctx_floor, DEFAULT_FIT_CTX_FLOOR,
        "out-of-range floor {bad} must fall back to the factory value"
      );
    }
  }

  #[test]
  fn build_options_proxy_port_cli_overrides_config_value() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      proxy: crate::config::loader::ProxyConfig {
        enabled: true,
        port: Some(22222),
        ollama_compat: false,
        fallback_enabled: true,
        header_read_timeout_secs: 30,
        idle_ttl_secs: 1800,
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    // The CLI override (Some(8080)) beats config.proxy.port.
    let opts = build_options(
      None,
      Some(8080),
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config,
    )
    .expect("build_options");
    assert_eq!(opts.proxy.port, Some(8080), "CLI flag overrides config");
    assert_eq!(opts.proxy.effective_port(), 8080);
    // Other proxy fields still come from config (not reset).
    assert!(opts.proxy.enabled);
    assert_eq!(opts.proxy.header_read_timeout_secs, 30);
  }

  #[test]
  fn build_options_no_cli_override_falls_back_to_config_then_default() {
    let _env = crate::cli::test_lock::serialize();
    // Defaults all the way down: no CLI override, no proxy block in
    // config → daemon uses ProxyConfig::default(), which resolves to
    // 11435 (default mode) when nothing pins `port` explicitly.
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert_eq!(opts.proxy.port, None);
    assert_eq!(opts.proxy.effective_port(), 11435);
    assert!(!opts.proxy.ollama_compat);
  }

  #[test]
  fn build_options_ollama_compat_cli_flag_flips_mode_and_default_port() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    let opts = build_options(
      None, None, true, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert!(opts.proxy.ollama_compat);
    // Port stays None at the schema level — the CLI flag drives the
    // mode bool, and `effective_port()` derives the runtime port.
    assert_eq!(opts.proxy.port, None);
    assert_eq!(opts.proxy.effective_port(), 11434);
  }

  #[test]
  fn build_options_ollama_compat_or_combines_config_cli_env() {
    let _env = crate::cli::test_lock::serialize();
    // Config-only: config says compat=true, CLI flag off → enabled.
    let cli = parse_cli(&["daemon", "start"]);
    let config_compat = Config {
      proxy: crate::config::loader::ProxyConfig {
        ollama_compat: true,
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let opts_config = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_compat,
    )
    .expect("build_options");
    assert!(opts_config.proxy.ollama_compat);

    // CLI-only: config has compat=false, CLI flag on → enabled.
    let config_off = Config::default();
    let opts_cli = build_options(
      None,
      None,
      true,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_off,
    )
    .expect("build_options");
    assert!(opts_cli.proxy.ollama_compat);

    // Both off (neither config nor CLI) → disabled.
    let opts_neither = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_off,
    )
    .expect("build_options");
    assert!(!opts_neither.proxy.ollama_compat);
  }

  #[test]
  fn build_options_proxy_host_cli_overrides_config() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      proxy: crate::config::loader::ProxyConfig {
        host: Some("1.2.3.4".parse().unwrap()),
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let cli_host: std::net::IpAddr = "9.9.9.9".parse().unwrap();
    let opts = build_options(
      None,
      None,
      false,
      false,
      Some(cli_host),
      false,
      false,
      false,
      false,
      &cli,
      &config,
    )
    .expect("build_options");
    assert_eq!(
      opts.proxy.host,
      Some(cli_host),
      "CLI host must win over config"
    );
    assert_eq!(opts.proxy.effective_host(), cli_host);
  }

  #[test]
  fn build_options_proxy_host_from_config_when_no_cli() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config {
      proxy: crate::config::loader::ProxyConfig {
        host: Some("0.0.0.0".parse().unwrap()),
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let opts = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options");
    assert_eq!(opts.proxy.host, Some("0.0.0.0".parse().unwrap()));
    assert!(!opts.proxy.effective_host().is_loopback());
  }

  #[test]
  fn build_options_insecure_no_auth_or_combines_config_cli() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    // CLI flag on, config off → on.
    let opts_cli = build_options(
      None,
      None,
      false,
      false,
      None,
      true,
      false,
      false,
      false,
      &cli,
      &Config::default(),
    )
    .expect("build_options");
    assert!(opts_cli.proxy.insecure_no_auth);
    // Config on, CLI off → on.
    let config_insecure = Config {
      proxy: crate::config::loader::ProxyConfig {
        insecure_no_auth: true,
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let opts_config = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_insecure,
    )
    .expect("build_options");
    assert!(opts_config.proxy.insecure_no_auth);
    // Both off → off (the safe default).
    let opts_off = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &Config::default(),
    )
    .expect("build_options");
    assert!(!opts_off.proxy.insecure_no_auth);
  }

  #[test]
  fn build_options_proxy_host_and_key_from_env() {
    let _env = crate::cli::test_lock::serialize();
    let prev_host = std::env::var_os("LLAMASTASH_PROXY_HOST");
    let prev_key = std::env::var_os("LLAMASTASH_PROXY_API_KEY");
    std::env::set_var("LLAMASTASH_PROXY_HOST", "0.0.0.0");
    std::env::set_var("LLAMASTASH_PROXY_API_KEY", "sk-llamastash-fromenv");
    let cli = parse_cli(&["daemon", "start"]);
    let opts = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &Config::default(),
    );
    let restore = |k: &str, v: Option<std::ffi::OsString>| match v {
      Some(v) => std::env::set_var(k, v),
      None => std::env::remove_var(k),
    };
    restore("LLAMASTASH_PROXY_HOST", prev_host);
    restore("LLAMASTASH_PROXY_API_KEY", prev_key);
    let opts = opts.expect("build_options");
    assert_eq!(opts.proxy.host, Some("0.0.0.0".parse().unwrap()));
    assert_eq!(opts.proxy.api_key.as_deref(), Some("sk-llamastash-fromenv"));
  }

  /// Build a non-loopback `DaemonOptions` + a `Cli` pinned to
  /// `config_path` so `provision_proxy_key` reads/writes a temp config.
  fn non_loopback_opts(config_path: &std::path::Path) -> (DaemonOptions, Cli) {
    let mut opts = DaemonOptions::from_defaults().expect("defaults");
    opts.proxy.host = Some("0.0.0.0".parse().unwrap());
    let cli = parse_cli(&["--config", config_path.to_str().unwrap(), "daemon", "start"]);
    (opts, cli)
  }

  #[test]
  fn build_options_normalizes_blank_api_key_to_none() {
    // Fail-closed guard: a blank `proxy.api_key` must not count as a
    // key, or a non-loopback host would bind unauthenticated while the
    // backstop (is_none) stayed silent.
    // Serialize + clear the proxy env overrides so a concurrent
    // env-driven test can't leak a key into this one.
    let _env = crate::cli::test_lock::serialize();
    let prev_key = std::env::var_os("LLAMASTASH_PROXY_API_KEY");
    std::env::remove_var("LLAMASTASH_PROXY_API_KEY");
    let cli = parse_cli(&["daemon", "start"]);
    for blank in ["", "   ", "\t\n"] {
      let config = Config {
        proxy: crate::config::loader::ProxyConfig {
          host: Some("0.0.0.0".parse().unwrap()),
          api_key: Some(blank.to_string()),
          ..crate::config::loader::ProxyConfig::default()
        },
        ..Config::default()
      };
      let opts = build_options(
        None, None, false, false, None, false, false, false, false, &cli, &config,
      )
      .expect("build_options");
      assert_eq!(
        opts.proxy.api_key, None,
        "blank api_key {blank:?} must normalize to None"
      );
      assert!(!opts.proxy.auth_enforced());
    }
    if let Some(v) = prev_key {
      std::env::set_var("LLAMASTASH_PROXY_API_KEY", v);
    }
  }

  #[test]
  fn provision_generates_persists_and_sets_key_for_lan() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("config.yaml");
    let (mut opts, cli) = non_loopback_opts(&cfg);
    assert!(opts.proxy.api_key.is_none());
    provision_proxy_key(&mut opts, &cli, false).expect("provision");
    let key = opts.proxy.api_key.clone().expect("key set on opts");
    assert!(key.starts_with("sk-llamastash-"), "unexpected key: {key}");
    let written = std::fs::read_to_string(&cfg).expect("config written");
    assert!(written.contains(&key), "key not persisted: {written}");
    assert!(written.contains("api_key"));
  }

  #[test]
  fn provision_is_idempotent_when_key_already_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("config.yaml");
    let (mut opts, cli) = non_loopback_opts(&cfg);
    provision_proxy_key(&mut opts, &cli, false).expect("first");
    let first = opts.proxy.api_key.clone().unwrap();
    let first_file = std::fs::read_to_string(&cfg).unwrap();
    // Second call: key already present → no regeneration, no rewrite.
    provision_proxy_key(&mut opts, &cli, false).expect("second");
    assert_eq!(opts.proxy.api_key.as_deref(), Some(first.as_str()));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), first_file);
  }

  #[test]
  fn provision_insecure_generates_no_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("config.yaml");
    let (mut opts, cli) = non_loopback_opts(&cfg);
    opts.proxy.insecure_no_auth = true;
    provision_proxy_key(&mut opts, &cli, false).expect("provision");
    assert!(
      opts.proxy.api_key.is_none(),
      "insecure must not generate a key"
    );
    assert!(!cfg.exists(), "insecure must not write config");
  }

  #[test]
  fn provision_loopback_is_noop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("config.yaml");
    // host stays None → loopback default.
    let mut opts = DaemonOptions::from_defaults().expect("defaults");
    let cli = parse_cli(&["--config", cfg.to_str().unwrap(), "daemon", "start"]);
    provision_proxy_key(&mut opts, &cli, false).expect("provision");
    assert!(opts.proxy.api_key.is_none());
    assert!(!cfg.exists(), "loopback must not write config");
  }

  #[cfg(unix)]
  #[test]
  fn provision_detached_errors_when_key_cannot_persist() {
    // Detached daemon re-reads the key from config; if the write fails
    // the child can't see the key, hits the backstop, and drops the
    // proxy. provision must refuse to start rather than print a dead
    // key. Force a write failure with a group/world-writable config dir
    // (the writer refuses to drop a 0600 file into a permissive dir).
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let cfg = dir.path().join("config.yaml");
    let (mut opts, cli) = non_loopback_opts(&cfg);
    let err = provision_proxy_key(&mut opts, &cli, false).expect_err("must refuse to start");
    assert!(
      err.to_string().contains("could not save"),
      "error must explain the unpersisted key: {err}"
    );
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).ok();
  }

  #[cfg(unix)]
  #[test]
  fn provision_foreground_tolerates_persist_failure() {
    // Same write failure, but a --foreground daemon is the same process
    // that holds the key, so it works for this run. provision keeps the
    // key on opts and returns Ok (the banner flags it as unsaved).
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let cfg = dir.path().join("config.yaml");
    let (mut opts, cli) = non_loopback_opts(&cfg);
    provision_proxy_key(&mut opts, &cli, true).expect("foreground tolerates write failure");
    assert!(
      opts
        .proxy
        .api_key
        .as_deref()
        .is_some_and(|k| k.starts_with("sk-llamastash-")),
      "key must still be set on opts for the in-process run"
    );
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).ok();
  }

  #[test]
  fn provision_preserves_existing_proxy_keys_in_config() {
    // A recursive merge must keep the user's other proxy settings.
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "proxy:\n  port: 18080\n  ollama_compat: true\n").unwrap();
    let (mut opts, cli) = non_loopback_opts(&cfg);
    provision_proxy_key(&mut opts, &cli, false).expect("provision");
    let written = std::fs::read_to_string(&cfg).unwrap();
    assert!(
      written.contains("18080"),
      "existing port dropped: {written}"
    );
    assert!(
      written.contains("ollama_compat"),
      "existing flag dropped: {written}"
    );
    assert!(written.contains(opts.proxy.api_key.as_ref().unwrap().as_str()));
  }

  #[test]
  fn build_options_no_proxy_fallback_cli_flag_clears_fallback_enabled() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    // Default is fallback_enabled = true.
    let baseline = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options baseline");
    assert!(baseline.proxy.fallback_enabled);
    // CLI flag forces it off.
    let opts = build_options(
      None, None, false, true, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options no-fallback");
    assert!(!opts.proxy.fallback_enabled);
  }

  #[test]
  fn build_options_no_proxy_fallback_or_combines_config_cli() {
    let _env = crate::cli::test_lock::serialize();
    // Config-only: config has fallback_enabled=false, CLI off → disabled.
    let cli = parse_cli(&["daemon", "start"]);
    let config_off_fallback = Config {
      proxy: crate::config::loader::ProxyConfig {
        fallback_enabled: false,
        ..crate::config::loader::ProxyConfig::default()
      },
      ..Config::default()
    };
    let opts_config = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_off_fallback,
    )
    .expect("build_options");
    assert!(!opts_config.proxy.fallback_enabled);

    // CLI-only: config has fallback_enabled=true (default), CLI on → disabled.
    let config_default = Config::default();
    let opts_cli = build_options(
      None,
      None,
      false,
      true,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_default,
    )
    .expect("build_options");
    assert!(!opts_cli.proxy.fallback_enabled);

    // Both off → fallback_enabled stays true (the default).
    let opts_neither = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_default,
    )
    .expect("build_options");
    assert!(opts_neither.proxy.fallback_enabled);
  }

  #[test]
  fn build_options_lemonade_defaults_on_and_force_flag_captured() {
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    // Read the Lemonade force flag out of the per-backend force map.
    let force = |o: &DaemonOptions| {
      o.backend_force
        .get(crate::backend::lemonade::LEMONADE_BACKEND_ID)
        .copied()
        .unwrap_or(false)
    };
    // Default: enablement intent is on (default-on-when-found, like ds4), the
    // config `enabled` stays unset, and no force flag is captured.
    let baseline = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect("build_options baseline");
    assert_eq!(baseline.backend.lemonade.enabled, None);
    assert!(!force(&baseline));
    assert!(baseline.backend.lemonade.intends_enabled(force(&baseline)));

    // CLI flag captured as force (overrides a config `enabled: false`).
    let opts_cli = build_options(
      None, None, false, false, None, false, true, false, false, &cli, &config,
    )
    .expect("build_options lemonade");
    assert!(force(&opts_cli));
    assert!(opts_cli.backend.lemonade.intends_enabled(force(&opts_cli)));

    // Config `enabled: false` disables the intent (no force present).
    let config_off = Config {
      backend: crate::backend::BackendConfig {
        lemonade: crate::config::LemonadeConfig {
          enabled: Some(false),
          ..Default::default()
        },
        ..Default::default()
      },
      ..Config::default()
    };
    let opts_off = build_options(
      None,
      None,
      false,
      false,
      None,
      false,
      false,
      false,
      false,
      &cli,
      &config_off,
    )
    .expect("build_options config-off");
    assert!(!opts_off.backend.lemonade.intends_enabled(force(&opts_off)));
  }

  #[test]
  fn resolve_uses_env_paths_when_supplied() {
    // No CLI / config paths — env-only source must still populate
    // UserPath roots so a `LLAMASTASH_MODEL_PATHS=/foo:/bar` recipe
    // matches the documented contract in the README.
    let cli = parse_cli(&["daemon", "start"]);
    let config = Config::default();
    let home = PathBuf::from("/home/alice");
    let env_paths = vec![PathBuf::from("/env/one"), PathBuf::from("/env/two")];
    let roots = resolve_scan_roots(&cli, &config, &env_paths, false, Some(&home));
    let user_paths: Vec<&Path> = roots
      .iter()
      .filter(|r| r.source == ModelSource::UserPath)
      .map(|r| r.path.as_path())
      .collect();
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/env/one")),
      "env path missing: {user_paths:?}"
    );
    assert!(
      user_paths.iter().any(|p| *p == Path::new("/env/two")),
      "env path missing: {user_paths:?}"
    );
  }

  #[test]
  fn resolve_dedupes_env_paths_against_cli_and_config() {
    let cli = parse_cli(&["--model-path", "/shared", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/shared")],
      ..Config::default()
    };
    let env_paths = vec![PathBuf::from("/shared")];
    let home = PathBuf::from("/home/alice");
    let roots = resolve_scan_roots(&cli, &config, &env_paths, false, Some(&home));
    let matches: Vec<&Path> = roots
      .iter()
      .filter(|r| r.path == Path::new("/shared"))
      .map(|r| r.path.as_path())
      .collect();
    assert_eq!(
      matches.len(),
      1,
      "config/env/cli duplicate must collapse, got {matches:?}"
    );
  }

  #[test]
  fn resolve_env_no_scan_suppresses_default_caches() {
    let cli = parse_cli(&["--model-path", "/work/keep", "daemon", "start"]);
    let config = Config::default();
    let home = PathBuf::from("/home/alice");
    // env_no_scan=true must drop the default-cache walk even when
    // neither `--no-scan` nor `config.disable_scan` is set.
    let roots = resolve_scan_roots(&cli, &config, &[], true, Some(&home));
    let cache_sources: Vec<_> = roots
      .iter()
      .filter(|r| r.source != ModelSource::UserPath)
      .map(|r| r.source)
      .collect();
    assert!(
      cache_sources.is_empty(),
      "LLAMASTASH_NO_SCAN must suppress default caches, got {cache_sources:?}"
    );
    assert_eq!(roots.len(), 1, "only --model-path remains");
  }

  #[test]
  fn env_model_paths_splits_on_platform_separator() {
    // Serialize against the other `LLAMASTASH_MODEL_PATHS` /
    // `LLAMASTASH_NO_SCAN` tests: process-global env vars race across
    // parallel test threads (one test's set_var landing between
    // another's remove_var and read), which flaked CI on Windows.
    let _env = crate::cli::test_lock::serialize();
    // Drive the production helper directly. Two paths joined with the
    // platform separator must round-trip. `join_paths` is the inverse
    // of `split_paths`, so this also documents the public contract
    // shell users see (`:` on POSIX, `;` on Windows).
    let joined = std::env::join_paths([PathBuf::from("/a"), PathBuf::from("/b")])
      .expect("join_paths should succeed for two simple paths");
    let prev = std::env::var_os("LLAMASTASH_MODEL_PATHS");
    std::env::set_var("LLAMASTASH_MODEL_PATHS", &joined);
    let parsed = env_model_paths();
    match prev {
      Some(v) => std::env::set_var("LLAMASTASH_MODEL_PATHS", v),
      None => std::env::remove_var("LLAMASTASH_MODEL_PATHS"),
    }
    assert_eq!(parsed, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
  }

  #[test]
  fn env_model_paths_unset_returns_empty() {
    let _env = crate::cli::test_lock::serialize();
    let prev = std::env::var_os("LLAMASTASH_MODEL_PATHS");
    std::env::remove_var("LLAMASTASH_MODEL_PATHS");
    let parsed = env_model_paths();
    if let Some(v) = prev {
      std::env::set_var("LLAMASTASH_MODEL_PATHS", v);
    }
    assert!(parsed.is_empty());
  }

  #[test]
  fn build_options_rejects_disable_scan_with_no_paths() {
    let _env = crate::cli::test_lock::serialize();
    // The dead-end combo: scanning off, zero user paths anywhere.
    // Today this would leave the catalog empty forever — the
    // validator must turn it into a startup error so the user sees
    // *why* the daemon refused.
    let cli = parse_cli(&["--no-scan", "daemon", "start"]);
    let config = Config::default();
    let err = build_options(
      None, None, false, false, None, false, false, false, false, &cli, &config,
    )
    .expect_err("--no-scan with zero paths must error");
    let msg = format!("{err:#}");
    assert!(
      msg.contains("scanning is disabled"),
      "error must name the failure mode, got: {msg}"
    );
  }

  #[test]
  fn build_options_accepts_disable_scan_when_cli_path_supplied() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["--no-scan", "--model-path", "/work/keep", "daemon", "start"]);
    let config = Config::default();
    assert!(
      build_options(None, None, false, false, None, false, false, false, false, &cli, &config)
        .is_ok(),
      "--no-scan + --model-path must build cleanly"
    );
  }

  #[test]
  fn build_options_accepts_disable_scan_when_config_path_supplied() {
    let _env = crate::cli::test_lock::serialize();
    let cli = parse_cli(&["--no-scan", "daemon", "start"]);
    let config = Config {
      model_paths: vec![PathBuf::from("/work/cfg")],
      ..Config::default()
    };
    assert!(
      build_options(None, None, false, false, None, false, false, false, false, &cli, &config)
        .is_ok(),
      "--no-scan + config model_paths must build cleanly"
    );
  }

  #[test]
  fn env_no_scan_accepts_documented_truthy_values() {
    let _env = crate::cli::test_lock::serialize();
    // `1` is what the README documents; `true`/`yes`/`on` ride along
    // because every other LLAMASTASH_* bool in this binary accepts
    // them, and a script that already uses LLAMASTASH_OFFLINE=true
    // shouldn't be surprised when LLAMASTASH_NO_SCAN=true is rejected.
    let prev = std::env::var_os("LLAMASTASH_NO_SCAN");
    for value in &["1", "true", "TRUE", "yes", "On"] {
      std::env::set_var("LLAMASTASH_NO_SCAN", value);
      assert!(env_no_scan(), "value {value:?} should disable scan");
    }
    for value in &["0", "false", "no", "off", ""] {
      std::env::set_var("LLAMASTASH_NO_SCAN", value);
      assert!(!env_no_scan(), "value {value:?} should leave scan on");
    }
    std::env::remove_var("LLAMASTASH_NO_SCAN");
    assert!(!env_no_scan(), "unset should leave scan on");
    if let Some(v) = prev {
      std::env::set_var("LLAMASTASH_NO_SCAN", v);
    }
  }
}
