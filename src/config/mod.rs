//! User-authored configuration.
//!
//! Configuration sources resolve in priority order:
//! 1. CLI flags
//! 2. Environment variables (`LLAMASTASH_*`)
//! 3. YAML config file (`config.yaml` under the OS config dir)
//! 4. Built-in defaults
//!
//! Unit 1 implements the YAML side of this and the path-resolution helper.
//! The full CLI/env/file merge into an `EffectiveConfig` is wired by
//! `cli::dispatch` once the CLI surface lands.

pub mod loader;
pub mod writer;

// Public surface ready for later units (scanner, supervisor, TUI). Quiet the
// dead-code re-export warning until consumers land.
#[allow(unused_imports)]
pub use loader::{
  config_path, config_path_from, load_config, load_config_from_path, validate_scan_settings,
  ArchDefaults, CachePathsConfig, Config, LoadedConfig, PortRange, ScanSettingsError,
};
