//! User-authored configuration.
//!
//! Configuration sources resolve in priority order:
//! 1. CLI flags
//! 2. Environment variables (`LLAMASTASH_*`)
//! 3. YAML config file (`config.yaml` under the OS config dir)
//! 4. Built-in defaults

pub mod knob_migration;
pub mod loader;
pub mod presets_writer;
pub mod writer;
pub mod yaml_edit;

pub use crate::backend::ds4::Ds4Config;
pub use crate::backend::lemonade::LemonadeConfig;
pub use crate::backend::llama_cpp::LlamaCppConfig;
pub use crate::backend::vllm::VllmConfig;
pub use crate::backend::BackendConfig;
pub use loader::{
  config_path, config_path_from, load_config, load_config_from_path, validate_port_range,
  validate_scan_settings, CachePathsConfig, Config, ConfigPresetBlock, DaemonConfig,
  DefaultLaunchMode, GpuConfig, KnobValue, KnobValueOpt, LoadedConfig,
  PortRange, PortRangeError, PresetBody, ProxyConfig, ScanSettingsError, DEFAULT_FIT_CTX_FLOOR, MAX_CTX_TOKENS,
};
