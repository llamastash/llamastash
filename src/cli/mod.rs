//! CLI surface (clap definitions + dispatcher).
//!
//! The dispatcher is `async` because the daemon client speaks
//! Tokio. Each subcommand has its own handler module under
//! `src/cli/`. Handlers return [`exit_codes::CliResult`] so the
//! top-level dispatcher can map structured failure into the
//! documented exit-code table without losing the message.

pub mod cli_args;
pub mod client;
pub(crate) mod colors;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod exit_codes;
pub mod favorites;
pub(crate) mod format;
pub mod init;
pub mod knob_flags;
pub mod last_params;
pub mod list;
pub mod logs;
pub mod output;
pub(crate) mod picker;
pub mod presets;
pub mod pull;
pub mod resolve;
pub mod show;
pub mod start;
pub mod status;
pub mod stop;
pub mod tail_args;
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
  // Applied here rather than in the daemon wiring so every `gpu::probe`
  // caller agrees — `init` and `doctor` probe hardware without ever
  // building `DaemonOptions`.
  crate::gpu::set_vulkan_probe_enabled(config.config.gpu.enable_vulkan_probe);
  // A key that moved into a nested block is silently ignored by the
  // forward-compatible top-level parse, so the user would just lose the
  // setting. Non-fatal — the rest of the config is fine.
  for (old, new) in &config.relocated_keys {
    let msg = format!("config: `{old}` moved to `{new}`; the old key is ignored");
    eprintln!("{}", colors::warning(&msg));
    log::warn!("{msg}");
  }
  let command = cli.command.take();
  if let Some(warning) = &config.warning {
    // A present-but-unparseable config (bad YAML, an unknown `[proxy]` key,
    // a bad value) is a usage error — per the config contract a typo is
    // rejected loudly, never silently papered over with defaults. `config`
    // (opens the file), `init` (rewrites it), and `doctor` (diagnoses setup)
    // are exempt so the user can always repair a broken config.
    let repair = match &command {
      Some(Command::Config(args)) => args.action.is_none(),
      Some(Command::Init(_)) | Some(Command::Doctor(_)) => true,
      _ => false,
    };
    if repair {
      log::warn!("{warning}");
    } else {
      eprintln!("config error: {warning}");
      log::error!("config error: {warning}");
      return Ok(exit_codes::USAGE);
    }
  }
  // Sticky `--llama-server`: when the user passes the flag explicitly,
  // write the resolved path back into the YAML config so next launch
  // picks it up without re-typing. Best-effort — a failed write logs
  // a warning and the command proceeds normally.
  persist_llama_server_override(&cli, &config.config);
  let resolved_config = &config.config;
  let outcome: CliResult = match command {
    None => handle_tui(&cli, resolved_config).await,
    Some(Command::Config(args)) => config::handle(args.action, &cli, resolved_config),
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
    Some(Command::Show(args)) => show::handle(args, &cli, resolved_config).await,
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
  let resolved = crate::util::paths::canonicalize(raw).unwrap_or_else(|_| raw.clone());
  if config.backend.llamacpp.primary_binary().as_deref() == Some(resolved.as_path()) {
    return;
  }
  let Some(path) = crate::config::config_path(cli.config.clone()) else {
    log::warn!("--llama-server: no writable config path; skipping persist");
    return;
  };
  // Rebuild `backend.llamacpp.servers` with the primary (first) binary set to
  // the override, preserving any additional servers + their names. Nested so
  // the recursive merge touches only that key.
  let server_entry = |binary: String, name: Option<String>| {
    let mut entry = yaml_serde::Mapping::new();
    entry.insert(
      yaml_serde::Value::String("binary".into()),
      yaml_serde::Value::String(binary),
    );
    if let Some(name) = name {
      entry.insert(
        yaml_serde::Value::String("name".into()),
        yaml_serde::Value::String(name),
      );
    }
    yaml_serde::Value::Mapping(entry)
  };
  let existing = &config.backend.llamacpp.servers;
  let mut servers = vec![server_entry(
    resolved.display().to_string(),
    existing.first().and_then(|s| s.name.clone()),
  )];
  for extra in existing.iter().skip(1) {
    servers.push(server_entry(
      extra.binary.display().to_string(),
      extra.name.clone(),
    ));
  }
  let additions = yaml_serde::Value::Mapping({
    let mut llamacpp = yaml_serde::Mapping::new();
    llamacpp.insert(
      yaml_serde::Value::String("servers".into()),
      yaml_serde::Value::Sequence(servers),
    );
    let mut backend = yaml_serde::Mapping::new();
    backend.insert(
      yaml_serde::Value::String(crate::backend::DEFAULT_BACKEND_ID.into()),
      yaml_serde::Value::Mapping(llamacpp),
    );
    let mut m = yaml_serde::Mapping::new();
    m.insert(
      yaml_serde::Value::String("backend".into()),
      yaml_serde::Value::Mapping(backend),
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
pub(crate) async fn handle_tui(cli: &Cli, config: &crate::config::Config) -> CliResult {
  // Pick the glyph set once, before any frame renders. Env wins over
  // the config flag per the project's env-truthy convention. Covers
  // both the `--render` snapshot and the interactive loop below.
  crate::tui::glyphs::init(crate::tui::glyphs::ascii_env(), config.ascii_glyphs);
  // Ensure the daemon is up. The TUI's writer task reconnects per
  // command, so we don't hold the connection past startup priming.
  //
  // A failure here does NOT abort the TUI: the backend fail-fast
  // precheck refuses to auto-spawn a degraded daemon (missing
  // `llama-server`, blocked Lemonade umbrella port, …), and dying on
  // stderr before the UI exists would hide that. Launch daemon-less
  // instead and surface the refusal in the Daemon panel's server row;
  // if a daemon comes up later the refresher clears it.
  let (client, daemon_start_error) = match client::connect_or_spawn(cli, config).await {
    Ok(c) => (Some(c), None),
    Err(e) => {
      let msg = e
        .message
        .unwrap_or_else(|| "daemon: failed to start".to_string());
      (None, Some(msg))
    }
  };
  if cli.render {
    return render_snapshot(cli, config, client, daemon_start_error).await;
  }
  drop(client);
  // TUI attaches via the HTTP control plane which reads bearer token +
  // URL out of `state_dir/runtime.json`. The parameter is still named
  // `socket` in `tui::events` for minimum churn.
  let socket = crate::util::paths::state_dir().ok_or_else(|| {
    CliExit::new(
      exit_codes::DAEMON_UNREACHABLE,
      "could not resolve state directory",
    )
  })?;
  let keymap = resolve_keymap(config);
  // Resolve the same DaemonOptions the auto-spawn path would use,
  // so the TUI's `R:restart daemon` hotkey re-spawns with matching
  // `--model-path` / `--no-scan` / `--llama-server` settings rather
  // than dropping back to bare platform defaults. A failure to
  // resolve options is non-fatal: the writer task falls back to
  // `from_defaults` and logs.
  let daemon_opts = daemon::build_options(
    None, None, false, false, None, false, false, false, cli, config,
  )
  .ok();
  let offline = crate::init::fetch::offline_requested(false);
  // CLI flag wins as a one-way opt-in: `--mouse-focus` flips on even
  // when `config.mouse_focus` is unset / false. Matches `--offline`
  // and `--no-scan` — there's no negating counter-flag because the
  // default is already the conservative "off" path.
  let mouse_focus = cli.mouse_focus || config.mouse_focus;
  match crate::tui::events::launch(
    config.theme,
    resolve_custom_palette(config),
    keymap,
    offline,
    mouse_focus,
    crate::config::loader::sanitize_left_pane_ratios(&config.left_pane_ratios),
    &socket,
    daemon_opts,
    daemon_start_error,
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
  client: Option<crate::ipc::Client>,
  daemon_start_error: Option<String>,
) -> CliResult {
  use crate::tui::app::{App, AppOptions};
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  let (width, height) = match cli.render_size.as_deref() {
    // A malformed `--render-size` is a CLI usage error, not an internal
    // failure — map it to USAGE (64) so it matches clap's own arg rejections.
    Some(raw) => cli_args::parse_render_size(raw)
      .map_err(|msg| CliExit::new(exit_codes::USAGE, format!("--render-size: {msg}")))?,
    None => (120, 40),
  };

  // Prime the App with whatever the daemon knows right now.
  let mut app = App::new(AppOptions {
    theme: config.theme,
    custom_palette: resolve_custom_palette(config),
    keymap: resolve_keymap(config),
    offline: crate::init::fetch::offline_requested(false),
    mouse_focus: cli.mouse_focus || config.mouse_focus,
    left_pane_ratios: crate::config::loader::sanitize_left_pane_ratios(&config.left_pane_ratios),
  });
  // Auto-spawn refused (backend precheck) — snapshot the daemon-less
  // frame so `--render` shows exactly what the interactive TUI would:
  // the refusal in the Daemon panel. No daemon will appear, so the
  // status-priming wait below is skipped.
  app.daemon_start_error = daemon_start_error;
  if let Some(mut client) = client {
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
