//! CLI surface (clap definitions + dispatcher).
//!
//! The dispatcher is `async` because Unit 2's daemon client speaks
//! Tokio. Each subcommand has its own handler module under
//! `src/cli/`. Handlers return [`exit_codes::CliResult`] so the
//! top-level dispatcher can map structured failure into the
//! documented exit-code table without losing the message.

pub mod cli_args;
pub mod client;
pub(crate) mod colors;
pub mod daemon;
pub mod doctor;
pub mod exit_codes;
pub mod favorites;
pub(crate) mod format;
pub mod init;
pub mod last_params;
pub mod list;
pub mod logs;
pub mod output;
pub mod presets;
pub mod pull;
pub mod resolve;
pub mod start;
pub mod status;
pub mod stop;
#[cfg(feature = "uat")]
pub mod uat;

use anyhow::Result;

use crate::config::loader::LoadedConfig;

pub use cli_args::{Cli, Command};
pub use exit_codes::{CliExit, CliResult};

/// Dispatch the parsed CLI to its handler. Returns the OS exit code
/// the binary should propagate. `main.rs` calls
/// `std::process::exit(code)` with the result.
pub async fn dispatch(mut cli: Cli, config: LoadedConfig) -> Result<i32> {
  // Lock in the color policy before any handler prints. The three
  // OR-ed off-conditions (flag, NO_COLOR, non-TTY stdout) live in
  // `colors::init`; downstream sites use `colors::*` helpers and
  // never re-derive whether colors are enabled.
  colors::init(cli.no_colors);
  if let Some(warning) = &config.warning {
    log::warn!("{warning}");
  }
  // Sticky `--llama-server`: when the user passes the flag explicitly,
  // write the resolved path back into the YAML config so next launch
  // picks it up without re-typing. Best-effort — a failed write logs
  // a warning and the command proceeds normally.
  persist_llama_server_override(&cli, &config.config);
  let command = cli.command.take();
  let resolved_config = &config.config;
  let outcome: CliResult = match command {
    None => handle_tui(&cli, resolved_config).await,
    Some(Command::Daemon(action)) => {
      map_anyhow(daemon::handle(action, &cli, resolved_config).await)
    }
    Some(Command::List(args)) => list::handle(args, &cli, resolved_config).await,
    Some(Command::Start(args)) => start::handle(args, &cli, resolved_config).await,
    Some(Command::Stop(args)) => stop::handle(args, &cli, resolved_config).await,
    Some(Command::Status(args)) => status::handle(args, &cli, resolved_config).await,
    Some(Command::Logs(args)) => logs::handle(args, &cli, resolved_config).await,
    Some(Command::Presets(args)) => presets::handle(args, &cli, resolved_config).await,
    Some(Command::Favorites(args)) => favorites::handle(args, &cli, resolved_config).await,
    Some(Command::LastParams(args)) => last_params::handle(args, &cli, resolved_config).await,
    Some(Command::Pull(args)) => pull::handle(args, &cli, resolved_config).await,
    Some(Command::Init(args)) => init::handle(args, &cli, resolved_config).await,
    Some(Command::Recommend(args)) => {
      init::handle(
        cli_args::recommend_to_init_args(args),
        &cli,
        resolved_config,
      )
      .await
    }
    Some(Command::Doctor(args)) => doctor::handle(args, &cli, resolved_config).await,
    #[cfg(feature = "uat")]
    Some(Command::Uat(args)) => uat::handle(args, &cli, resolved_config).await,
  };
  Ok(report(outcome))
}

/// Persist `--llama-server <PATH>` back into the user's YAML config so
/// subsequent launches pick it up without re-typing the flag. No-op
/// when the flag is unset or the resolved path already matches the
/// configured value. Best-effort: errors (no config dir, write failure,
/// symlink target) downgrade to a `log::warn!` so the rest of the
/// command runs unaffected.
fn persist_llama_server_override(cli: &Cli, config: &crate::config::Config) {
  let Some(raw) = cli.llama_server.as_ref() else {
    return;
  };
  // Canonicalize so equivalent paths (relative vs absolute, symlinks)
  // compare equal and don't trigger a rewrite each invocation. Fall
  // back to the raw value when canonicalization fails so a missing
  // file still gets persisted — the daemon's own locator will surface
  // the path error later.
  let resolved = std::fs::canonicalize(raw).unwrap_or_else(|_| raw.clone());
  if config
    .llama_server_path
    .as_ref()
    .map(|p| p == &resolved)
    .unwrap_or(false)
  {
    return;
  }
  let Some(path) = crate::config::config_path(cli.config.clone()) else {
    log::warn!("--llama-server: no writable config path; skipping persist");
    return;
  };
  let additions = serde_yaml::Value::Mapping({
    let mut m = serde_yaml::Mapping::new();
    m.insert(
      serde_yaml::Value::String("llama_server_path".into()),
      serde_yaml::Value::String(resolved.display().to_string()),
    );
    m
  });
  match crate::config::writer::merge_and_write(&path, additions) {
    Ok(_) => log::info!(
      "persisted --llama-server {} to {}",
      resolved.display(),
      path.display()
    ),
    Err(e) => log::warn!(
      "--llama-server: failed to persist to {}: {e}",
      path.display()
    ),
  }
}

/// Translate an anyhow-bearing handler result into the CliResult
/// shape. The `daemon` subcommand still uses `anyhow::Result` for its
/// internal start/stop/status flow; we treat any anyhow error as a
/// `UNKNOWN` exit unless it's already a `CliExit`.
fn map_anyhow(r: Result<()>) -> CliResult {
  match r {
    Ok(()) => Ok(()),
    Err(e) => match e.downcast::<CliExit>() {
      Ok(exit) => Err(exit),
      Err(other) => Err(CliExit::new(exit_codes::UNKNOWN, format!("{other}"))),
    },
  }
}

/// Print any error message and return the exit code.
fn report(result: CliResult) -> i32 {
  match result {
    Ok(()) => exit_codes::SUCCESS,
    Err(exit) => {
      if let Some(msg) = &exit.message {
        // Single render site for all CLI failures — wrapping in
        // `colors::error` adds the standard ✗ prefix and red colouring
        // when the global color policy allows.
        eprintln!("{}", colors::error(msg));
      }
      exit.code
    }
  }
}

/// Entry point for the TUI (`llamastash` with no subcommand). Returns a
/// `CliResult` so the dispatcher's exit-code surface stays uniform;
/// any anyhow failure from the TUI runtime maps to `UNKNOWN`.
///
/// Mirrors the auto-spawn behavior of every other CLI handler: if
/// the daemon socket isn't connectable, `connect_or_spawn` starts a
/// detached daemon configured with `cli.model_paths` / `--no-scan` /
/// `--llama-server` so discovery and host-metrics populate as soon
/// as the TUI's event loop attaches. Without this, `llamastash -p
/// /path` ran with the daemon down displayed an empty Models pane
/// and "daemon connecting…" indefinitely.
async fn handle_tui(cli: &Cli, config: &crate::config::Config) -> CliResult {
  // Ensure the daemon is up. The TUI's writer task reconnects per
  // command, so we don't hold the connection past startup priming.
  let mut client = client::connect_or_spawn(cli, config).await?;
  if cli.render {
    return render_snapshot(cli, config, &mut client).await;
  }
  drop(client);
  let socket = crate::util::paths::runtime_socket_path();
  let keymap = resolve_keymap(config);
  // Resolve the same DaemonOptions the auto-spawn path would use,
  // so the TUI's `R:restart daemon` hotkey re-spawns with matching
  // `--model-path` / `--no-scan` / `--llama-server` settings rather
  // than dropping back to bare platform defaults. A failure to
  // resolve options is non-fatal: the writer task falls back to
  // `from_defaults` and logs.
  let daemon_opts = daemon::build_options(None, None, cli, config).ok();
  let offline = crate::init::fetch::offline_requested(false);
  match crate::tui::events::launch(
    config.theme,
    resolve_custom_palette(config),
    keymap,
    offline,
    &socket,
    daemon_opts,
  )
  .await
  {
    Ok(()) => Ok(()),
    Err(e) => Err(CliExit::new(exit_codes::UNKNOWN, format!("tui: {e}"))),
  }
}

/// Build the runtime keymap from defaults + `config.keybindings`
/// overrides. Parse warnings (unknown action names, malformed key
/// specs) flow through `log::warn!` so a typo doesn't silently
/// drop the user's rebind.
fn resolve_keymap(config: &crate::config::Config) -> crate::tui::keybindings::KeyMap {
  let mut keymap = crate::tui::keybindings::KeyMap::default();
  if !config.keybindings.is_empty() {
    for warning in keymap.apply_overrides(&config.keybindings) {
      log::warn!("{warning}");
    }
  }
  keymap
}

/// Resolve `config.custom_theme` into a concrete palette. Parse
/// warnings are forwarded to the user via the normal `log::warn!`
/// channel so a bad colour value surfaces without aborting startup.
/// If the user picked `theme: custom` in the config but did not
/// supply a `custom_theme:` block, this returns `None` and the App
/// falls back to the default theme on render.
fn resolve_custom_palette(config: &crate::config::Config) -> Option<crate::theme::Palette> {
  let cfg = config.custom_theme.as_ref()?;
  let (palette, warnings) = cfg.resolve();
  for w in warnings {
    log::warn!("{w}");
  }
  Some(palette)
}

/// `--render`: draw a single TUI frame against `ratatui::TestBackend`
/// and print it as plain text. Uses the same App + render path the
/// interactive loop and the e2e golden test exercise.
///
/// The snapshot polls `status` for up to ~1.5s so the host-metrics
/// sampler's first 1 Hz tick has landed; if the wait times out the
/// Host pane shows the `unsampled` sentinel — which is honest output
/// for a daemon that just started.
async fn render_snapshot(
  cli: &Cli,
  config: &crate::config::Config,
  client: &mut crate::ipc::Client,
) -> CliResult {
  use crate::tui::app::{App, AppOptions};
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  let (width, height) = match cli.render_size.as_deref() {
    Some(raw) => cli_args::parse_render_size(raw)
      .map_err(|msg| CliExit::new(exit_codes::UNKNOWN, format!("--render-size: {msg}")))?,
    None => (120, 40),
  };

  // Prime the App with whatever the daemon knows right now.
  let mut app = App::new(AppOptions {
    theme: config.theme,
    custom_palette: resolve_custom_palette(config),
    keymap: resolve_keymap(config),
    offline: crate::init::fetch::offline_requested(false),
  });
  if let Ok(body) = client.call("list_models", None).await {
    app.ingest_list_models(&body);
  }

  // Poll `status` for up to ~1.5s so the host-metrics sampler's
  // first 1 Hz tick has landed before we draw. Without this wait
  // the snapshot would always show `backend unsampled` and a 0%
  // CPU bar, which is correct but unhelpful as a debug tool.
  let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
  loop {
    if let Ok(body) = client.call("status", None).await {
      app.daemon_connected = true;
      app.ingest_status(&body);
      let primed =
        app.host_metrics.gpu_backend != "unsampled" && app.host_metrics.ram_total_bytes > 0;
      if primed || std::time::Instant::now() >= deadline {
        break;
      }
    } else if std::time::Instant::now() >= deadline {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  }

  let backend = TestBackend::new(width, height);
  let mut terminal = Terminal::new(backend)
    .map_err(|e| CliExit::new(exit_codes::UNKNOWN, format!("--render: terminal: {e}")))?;
  terminal
    .draw(|f| crate::tui::render::render(f, &mut app))
    .map_err(|e| CliExit::new(exit_codes::UNKNOWN, format!("--render: draw: {e}")))?;

  let buf = terminal.backend().buffer();
  let mut out = String::with_capacity((width as usize + 1) * height as usize);
  for y in 0..buf.area.height {
    let mut row = String::with_capacity(width as usize);
    for x in 0..buf.area.width {
      row.push_str(buf[(x, y)].symbol());
    }
    // Trim trailing whitespace per row so diffs stay readable.
    out.push_str(row.trim_end());
    out.push('\n');
  }
  print!("{out}");
  Ok(())
}

/// Cross-module test serialisation for tests that toggle the global
/// `console::set_colors_enabled` flag or the `NO_COLOR` env var. A
/// single static mutex shared by `cli::colors::tests`,
/// `cli::format::tests`, and `cli::output::tests` so the modules don't
/// race each other on global state.
#[cfg(test)]
pub(crate) mod test_lock {
  use std::sync::{Mutex, MutexGuard, OnceLock};

  /// Acquire the cross-module color/env mutex. Poisoned guards are
  /// unwrapped so a panic in one test never silently disables the next.
  pub(crate) fn serialize() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK
      .get_or_init(|| Mutex::new(()))
      .lock()
      .unwrap_or_else(|poison| poison.into_inner())
  }
}
