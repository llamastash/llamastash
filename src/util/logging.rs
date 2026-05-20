//! Process-wide log initialisation and panic hook.
//!
//! Logs are written to `cache_dir/logs/llamastash.log` in append mode.
//! Verbose mode bumps the default Info level up to Debug; further levels
//! (Trace, Warn, Error) are accessible via the env var `LLAMASTASH_LOG`.

use std::{fs, fs::File, path::PathBuf, str::FromStr};

use anyhow::{Context, Result};
use log::LevelFilter;
use simplelog::{CombinedLogger, Config, ConfigBuilder, SharedLogger, WriteLogger};

use super::paths::log_dir;

/// Initialise the global logger. Returns the path of the log file that was
/// opened so the caller can surface it in error output. In `--verbose`
/// mode the file logger is teed to stderr at the same level, filtered to
/// `llamastash`-emitted records so dependency noise (hyper, reqwest, etc.)
/// doesn't drown out our own logs.
pub fn init(verbose: bool) -> Result<PathBuf> {
  let level = resolve_level(verbose, std::env::var("LLAMASTASH_LOG").ok().as_deref());
  let dir = log_dir().context("could not resolve a log directory for this platform")?;
  fs::create_dir_all(&dir)
    .with_context(|| format!("failed to create log directory at {}", dir.display()))?;
  let path = dir.join("llamastash.log");
  let file = open_log_file(&path)
    .with_context(|| format!("failed to open log file at {}", path.display()))?;
  let mut loggers: Vec<Box<dyn SharedLogger>> =
    vec![WriteLogger::new(level, Config::default(), file)];
  if verbose {
    let stderr_config = ConfigBuilder::new()
      .add_filter_allow("llamastash".to_string())
      .build();
    loggers.push(WriteLogger::new(level, stderr_config, std::io::stderr()));
  }
  CombinedLogger::init(loggers).context("logger already initialised")?;
  Ok(path)
}

/// Open the log file in append mode. On Unix, force mode `0600` so log
/// contents (prompts, model paths, error context) aren't world-readable —
/// see the Unit 1 review findings. The explicit `set_permissions` is needed
/// because `OpenOptionsExt::mode` only applies on create; existing log files
/// from older builds would otherwise keep their broader permissions.
fn open_log_file(path: &std::path::Path) -> std::io::Result<File> {
  let mut opts = File::options();
  opts.create(true).append(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
  }
  let file = opts.open(path)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
  }
  Ok(file)
}

fn resolve_level(verbose: bool, env: Option<&str>) -> LevelFilter {
  if let Some(raw) = env {
    if let Ok(level) = LevelFilter::from_str(raw) {
      return level;
    }
  }
  if verbose {
    LevelFilter::Debug
  } else {
    LevelFilter::Info
  }
}

/// Install a panic hook that records the panic to the log file and surfaces a
/// concise message on stderr. The terminal-restoration logic for the TUI
/// path lives in `tui::events` (added in Unit 6); this hook is the
/// always-installed baseline.
pub fn install_panic_hook() {
  std::panic::set_hook(Box::new(|info| {
    log::error!("panic: {info}");
    eprintln!("\nllamastash panicked: {info}");
  }));
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolve_level_prefers_env_when_valid() {
    assert_eq!(resolve_level(false, Some("trace")), LevelFilter::Trace);
    assert_eq!(resolve_level(true, Some("warn")), LevelFilter::Warn);
  }

  #[test]
  fn resolve_level_ignores_invalid_env() {
    assert_eq!(resolve_level(false, Some("noisy")), LevelFilter::Info);
    assert_eq!(resolve_level(true, Some("verybad")), LevelFilter::Debug);
  }

  #[test]
  fn resolve_level_defaults_to_info_or_debug_with_verbose() {
    assert_eq!(resolve_level(false, None), LevelFilter::Info);
    assert_eq!(resolve_level(true, None), LevelFilter::Debug);
  }

  #[cfg(unix)]
  #[test]
  fn open_log_file_enforces_mode_0600_on_unix() {
    use std::{
      os::unix::fs::PermissionsExt,
      time::{SystemTime, UNIX_EPOCH},
    };

    let suffix = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock should be after epoch")
      .as_nanos();
    let dir = std::env::temp_dir().join(format!(
      "llamastash-logtest-{}-{suffix}",
      std::process::id()
    ));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    let path = dir.join("llamastash.log");

    // Pre-create the file with a permissive mode to verify we tighten it.
    fs::write(&path, "stale").expect("seed write should succeed");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
      .expect("seed chmod should succeed");

    let _file = open_log_file(&path).expect("open_log_file should succeed");

    let mode = fs::metadata(&path)
      .expect("metadata should succeed")
      .permissions()
      .mode()
      & 0o777;
    assert_eq!(mode, 0o600, "log file must be 0600 after open_log_file");

    fs::remove_dir_all(&dir).expect("temp dir should be removed");
  }
}
