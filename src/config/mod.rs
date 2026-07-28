//! User-authored configuration.
//!
//! Configuration sources resolve in priority order:
//! 1. CLI flags
//! 2. Environment variables (`LLAMASTASH_*`)
//! 3. YAML config file (`config.yaml` under the OS config dir)
//! 4. Built-in defaults

pub mod loader;
pub mod presets_writer;
pub mod writer;
pub mod yaml_edit;

pub use crate::backend::ds4::Ds4Config;
pub use crate::backend::lemonade::LemonadeConfig;
pub use crate::backend::llama_cpp::LlamaCppConfig;
pub use crate::backend::BackendConfig;
pub use loader::{
  config_path, config_path_from, load_config, load_config_from_path, validate_scan_settings,
  CachePathsConfig, Config, ConfigPresetBlock, DefaultLaunchMode, GpuConfig, KnobSlotMut,
  KnobSlotRef, KnobValue, KnobValueOpt, LoadedConfig, PortRange, PresetBody, ProxyConfig,
  ScanSettingsError, TypedKnobs, DEFAULT_FIT_CTX_FLOOR, MAX_CTX_TOKENS,
};
