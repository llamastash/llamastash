use std::{
  collections::BTreeMap,
  env,
  ffi::OsString,
  fs,
  io::ErrorKind,
  path::{Path, PathBuf},
};

use log::warn;
use serde::{Deserialize, Serialize};

use crate::theme::{CustomThemeConfig, ThemeName};
use crate::util::paths::user_config_file;

/// Hard cap on config-file size. `serde_yaml` 0.9 expands anchors and aliases
/// without depth limits — a hostile file could mushroom in memory. 1 MiB is
/// far more than any plausible hand-written config and small enough that even
/// pathological YAML can't OOM the process.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// User-authored YAML config, with sensible defaults via `#[serde(default)]`.
///
/// Every field is optional in the file; missing fields use the built-in
/// defaults. Unknown fields are accepted silently so old files keep working
/// when new fields are added (forward-compat). Unknown values within a known
/// field (e.g. a non-existent theme name) still error, which is intentional —
/// silent typo tolerance for theme names would mask a real user problem.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Config {
  pub theme: ThemeName,
  /// Optional user-defined palette. When present it becomes the
  /// `Custom` theme target — selectable via the config `theme:
  /// custom` setting, and joined to the `t:theme` cycle. Absent
  /// (the default) means `Custom` is not selectable and the cycle
  /// stays on the five built-ins. See
  /// [`crate::theme::custom::CustomThemeConfig`] for the slot list.
  pub custom_theme: Option<CustomThemeConfig>,
  pub model_paths: Vec<PathBuf>,
  pub disable_default_cache_paths: CachePathsConfig,
  pub port_range: PortRange,
  pub llama_server_path: Option<PathBuf>,
  pub keybindings: BTreeMap<String, String>,
  pub disable_scan: bool,
  /// Per-launch health-probe timeout in seconds. Defaults to 120 s,
  /// which is enough for the typical 7B–13B model on local NVMe but
  /// can be tight for 70B+ on slow disks. Raise to e.g. 600 if you
  /// hit `health probe timeout (last status 503)` for legitimate
  /// loads.
  pub probe_timeout_secs: u64,
  /// Per-architecture launch defaults (R68). Map keys are GGUF
  /// `general.architecture` strings (`llama`, `qwen2`, `mistral`,
  /// `gemma`, `phi`, `qwen3`, …). At launch time the daemon merges
  /// these into `LaunchParams.advanced` only when the caller has not
  /// already supplied the corresponding flag — preset and last-params
  /// outrank these defaults per R69 precedence.
  pub arch_defaults: BTreeMap<String, ArchDefaults>,
}

/// Per-architecture launch defaults the init wizard writes when it
/// detects a hardware class that benefits from non-default flags
/// (e.g. CUDA → `n_gpu_layers: 99`, Vulkan → `flash_attn: true`).
/// Every field is optional so a partial entry only contributes the
/// keys it supplies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ArchDefaults {
  /// Layers offloaded to the GPU. Maps to `--n-gpu-layers`. Use 99
  /// for "all" (llama-server caps internally).
  pub n_gpu_layers: Option<u32>,
  /// CPU threads. Maps to `--threads`.
  pub threads: Option<u32>,
  /// K-cache quantisation tag (e.g. `q8_0`). Maps to `--cache-type-k`.
  pub cache_type_k: Option<String>,
  /// V-cache quantisation tag. Maps to `--cache-type-v`.
  pub cache_type_v: Option<String>,
  /// Flash-attention. Maps to `--flash-attn` (boolean flag).
  pub flash_attn: Option<bool>,
  /// Lock model in RAM. Maps to `--mlock`.
  pub mlock: Option<bool>,
  /// Disable mmap (forces full load into RAM). Maps to `--no-mmap`.
  pub no_mmap: Option<bool>,
  /// Concurrent request slots. Maps to `--parallel`.
  pub parallel: Option<u32>,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      theme: ThemeName::default(),
      custom_theme: None,
      model_paths: Vec::new(),
      disable_default_cache_paths: CachePathsConfig::default(),
      port_range: PortRange::default(),
      llama_server_path: None,
      keybindings: BTreeMap::new(),
      disable_scan: false,
      probe_timeout_secs: 120,
      arch_defaults: BTreeMap::new(),
    }
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct CachePathsConfig {
  pub huggingface: bool,
  pub ollama: bool,
  pub lm_studio: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PortRange {
  pub start: u16,
  pub end: u16,
}

impl Default for PortRange {
  fn default() -> Self {
    // High, unprivileged, rarely claimed by common dev servers. Resolved
    // during planning (see plan Open Questions).
    Self {
      start: 41100,
      end: 41300,
    }
  }
}

/// Returned by `load_config_from_path`. `warning` is non-`None` when the
/// loader gracefully fell back to defaults but the user should be told why
/// (e.g. malformed YAML).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedConfig {
  pub config: Config,
  pub warning: Option<String>,
}

/// Resolve which config file to load, given an optional CLI override, an
/// optional env override, and the directory `directories` would pick. Pure
/// function for testability — mirrors `kdash::config::config_path_from`.
///
/// Precedence: `--config` flag > `LLAMASTASH_CONFIG` env > XDG default. The
/// CLI is highest because users explicitly typed it; env beats the default
/// for the same reason.
pub fn config_path_from(
  cli_override: Option<PathBuf>,
  env_override: Option<OsString>,
  config_file: Option<PathBuf>,
) -> Option<PathBuf> {
  cli_override
    .or_else(|| {
      env_override
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
    })
    .or(config_file)
}

/// Resolve the active config-file path. Caller passes the optional
/// `--config` value parsed from the CLI; if it's `Some`, that wins.
pub fn config_path(cli_override: Option<PathBuf>) -> Option<PathBuf> {
  config_path_from(
    cli_override,
    env::var_os("LLAMASTASH_CONFIG"),
    user_config_file(),
  )
}

fn parse_config(contents: &str, path: &Path) -> LoadedConfig {
  match serde_yaml::from_str::<Config>(contents) {
    Ok(config) => LoadedConfig {
      config,
      warning: None,
    },
    Err(error) => LoadedConfig {
      config: Config::default(),
      warning: Some(format!(
        "failed to parse config file {}: {}. Using defaults.",
        path.display(),
        error
      )),
    },
  }
}

/// Load a YAML config from `path`. Missing files yield defaults with no
/// warning. Read or parse errors yield defaults with a warning so the caller
/// can surface them without aborting startup.
///
/// Two adversarial mitigations sit between the path and the YAML parser:
/// 1. `fs::metadata` rejects anything that isn't a regular file — a config
///    path pointed at a FIFO or `/dev/urandom` would otherwise hang the main
///    thread.
/// 2. A 1 MiB size cap (`MAX_CONFIG_BYTES`) prevents `serde_yaml`'s
///    unbounded anchor/alias expansion from being weaponised by a hostile
///    config file.
pub fn load_config_from_path(path: &Path) -> LoadedConfig {
  match fs::metadata(path) {
    Ok(meta) => {
      if !meta.is_file() {
        return LoadedConfig {
          config: Config::default(),
          warning: Some(format!(
            "config path {} is not a regular file (named pipe, device, or directory). Using defaults.",
            path.display()
          )),
        };
      }
      if meta.len() > MAX_CONFIG_BYTES {
        return LoadedConfig {
          config: Config::default(),
          warning: Some(format!(
            "config file {} is {} bytes; exceeds the {}-byte cap. Using defaults.",
            path.display(),
            meta.len(),
            MAX_CONFIG_BYTES
          )),
        };
      }
    }
    Err(error) if error.kind() == ErrorKind::NotFound => {
      return LoadedConfig::default();
    }
    Err(error) => {
      return LoadedConfig {
        config: Config::default(),
        warning: Some(format!(
          "failed to stat config file {}: {}. Using defaults.",
          path.display(),
          error
        )),
      };
    }
  }
  match fs::read_to_string(path) {
    Ok(contents) => parse_config(&contents, path),
    Err(error) if error.kind() == ErrorKind::NotFound => LoadedConfig::default(),
    Err(error) => LoadedConfig {
      config: Config::default(),
      warning: Some(format!(
        "failed to read config file {}: {}. Using defaults.",
        path.display(),
        error
      )),
    },
  }
}

/// Load the user's config, honoring the `--config` CLI override if supplied.
/// Warnings are forwarded to the `warn!` log macro in addition to being
/// returned.
pub fn load_config(cli_override: Option<PathBuf>) -> LoadedConfig {
  let loaded = config_path(cli_override)
    .map(|path| load_config_from_path(&path))
    .unwrap_or_default();
  if let Some(warning) = &loaded.warning {
    warn!("{warning}");
  }
  loaded
}

/// Validate that we have *some* place to look for models. If scanning is
/// disabled and no user-supplied paths exist, llamastash would start with an
/// empty list and no path forward — a confusing dead-end. Surface it
/// early.
pub fn validate_scan_settings(
  disable_scan: bool,
  cli_paths: &[PathBuf],
  env_paths: &[PathBuf],
  config_paths: &[PathBuf],
) -> Result<(), ScanSettingsError> {
  if disable_scan && cli_paths.is_empty() && env_paths.is_empty() && config_paths.is_empty() {
    Err(ScanSettingsError::NoScanWithoutPaths)
  } else {
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanSettingsError {
  NoScanWithoutPaths,
}

impl std::fmt::Display for ScanSettingsError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NoScanWithoutPaths => write!(
        f,
        "scanning is disabled but no model paths were supplied via --model-path, \
         LLAMASTASH_MODEL_PATHS, or the `model_paths` config key — llamastash has nothing to list. \
         Provide at least one path or re-enable scanning."
      ),
    }
  }
}

impl std::error::Error for ScanSettingsError {}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
  };

  use super::*;

  fn temp_test_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("system time should be after epoch")
      .as_nanos();
    let path = env::temp_dir().join(format!(
      "llamastash-config-tests-{}-{}-{}",
      name,
      std::process::id(),
      suffix
    ));
    fs::create_dir_all(&path).expect("temp test dir should be created");
    path
  }

  #[test]
  fn config_path_from_prefers_env_override() {
    let path = config_path_from(
      None,
      Some(OsString::from("/tmp/custom.yaml")),
      Some(PathBuf::from("/tmp/ignored.yaml")),
    );
    assert_eq!(path, Some(PathBuf::from("/tmp/custom.yaml")));
  }

  #[test]
  fn config_path_from_falls_back_to_xdg() {
    let path = config_path_from(
      None,
      None,
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml")),
    );
    assert_eq!(
      path,
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml"))
    );
  }

  #[test]
  fn config_path_from_ignores_empty_env_value() {
    let path = config_path_from(
      None,
      Some(OsString::new()),
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml")),
    );
    assert_eq!(
      path,
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml"))
    );
  }

  #[test]
  fn config_path_from_returns_none_when_all_sources_absent() {
    assert_eq!(config_path_from(None, None, None), None);
  }

  #[test]
  fn config_path_from_cli_override_beats_env_and_xdg() {
    let path = config_path_from(
      Some(PathBuf::from("/tmp/from-cli.yaml")),
      Some(OsString::from("/tmp/from-env.yaml")),
      Some(PathBuf::from("/tmp/from-xdg.yaml")),
    );
    assert_eq!(path, Some(PathBuf::from("/tmp/from-cli.yaml")));
  }

  #[test]
  fn config_path_from_env_beats_xdg_when_cli_absent() {
    let path = config_path_from(
      None,
      Some(OsString::from("/tmp/from-env.yaml")),
      Some(PathBuf::from("/tmp/from-xdg.yaml")),
    );
    assert_eq!(path, Some(PathBuf::from("/tmp/from-env.yaml")));
  }

  #[test]
  fn load_config_from_path_reads_valid_yaml() {
    let dir = temp_test_dir("valid");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
theme: latte
disable_scan: false
model_paths:
  - /home/u/models
  - /mnt/storage/gguf
disable_default_cache_paths:
  ollama: true
port_range:
  start: 50000
  end: 50100
keybindings:
  quit: ctrl+q
",
    )
    .expect("config fixture should be written");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    assert_eq!(loaded.config.theme, ThemeName::Latte);
    assert_eq!(
      loaded.config.model_paths,
      vec![
        PathBuf::from("/home/u/models"),
        PathBuf::from("/mnt/storage/gguf"),
      ]
    );
    assert!(loaded.config.disable_default_cache_paths.ollama);
    assert!(!loaded.config.disable_default_cache_paths.huggingface);
    assert!(!loaded.config.disable_default_cache_paths.lm_studio);
    assert_eq!(
      loaded.config.port_range,
      PortRange {
        start: 50000,
        end: 50100
      }
    );
    assert_eq!(
      loaded.config.keybindings.get("quit"),
      Some(&"ctrl+q".to_string())
    );

    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_missing_file_returns_defaults_silently() {
    let dir = temp_test_dir("missing");
    let path = dir.join("missing.yaml");
    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    assert!(loaded.warning.is_none());
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_malformed_yaml_uses_defaults_with_warning() {
    let dir = temp_test_dir("malformed");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: latte\nport_range: not-a-mapping").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("malformed YAML must surface a warning");
    assert!(
      warning.contains("failed to parse config file"),
      "warning should name the failure: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_unknown_theme_surfaces_warning() {
    let dir = temp_test_dir("unknown_theme");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: dracula\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("unknown theme must surface a warning");
    assert!(
      warning.contains("dracula"),
      "warning should name the bad value: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_partial_config_uses_defaults_for_unset_fields() {
    let dir = temp_test_dir("partial");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: gruvbox-dark\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none());
    assert_eq!(loaded.config.theme, ThemeName::GruvboxDark);
    assert_eq!(loaded.config.port_range, PortRange::default());
    assert!(loaded.config.model_paths.is_empty());
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn default_config_uses_macchiato_and_default_port_range() {
    let cfg = Config::default();
    assert_eq!(cfg.theme, ThemeName::Macchiato);
    assert_eq!(
      cfg.port_range,
      PortRange {
        start: 41100,
        end: 41300
      }
    );
    assert!(!cfg.disable_scan);
  }

  #[test]
  fn validate_scan_settings_errors_when_disabled_with_no_paths() {
    let result = validate_scan_settings(true, &[], &[], &[]);
    assert_eq!(result, Err(ScanSettingsError::NoScanWithoutPaths));
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("scanning is disabled"), "{msg}");
    assert!(msg.contains("--model-path"), "{msg}");
  }

  #[test]
  fn validate_scan_settings_ok_when_paths_supplied_via_any_source() {
    assert!(validate_scan_settings(true, &[PathBuf::from("/a")], &[], &[]).is_ok());
    assert!(validate_scan_settings(true, &[], &[PathBuf::from("/b")], &[]).is_ok());
    assert!(validate_scan_settings(true, &[], &[], &[PathBuf::from("/c")]).is_ok());
  }

  #[test]
  fn validate_scan_settings_ok_when_scan_enabled() {
    assert!(validate_scan_settings(false, &[], &[], &[]).is_ok());
  }

  #[test]
  fn load_config_from_path_rejects_oversized_file_with_warning() {
    let dir = temp_test_dir("oversize");
    let path = dir.join("config.yaml");
    // Write 1 MiB + 1 byte of valid YAML so the size cap, not the YAML
    // parser, is what trips the warning.
    let mut content = String::from("theme: latte\nkeybindings:\n");
    while content.len() <= MAX_CONFIG_BYTES as usize {
      content.push_str("  filler_key_filler_key_filler_key: 'pad pad pad pad pad'\n");
    }
    fs::write(&path, &content).expect("oversize fixture should write");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("oversized config must surface a warning");
    assert!(
      warning.contains("exceeds") && warning.contains("cap"),
      "warning should name the cap, got: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn arch_defaults_round_trip_through_yaml() {
    let dir = temp_test_dir("arch-defaults");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
theme: latte
arch_defaults:
  qwen2:
    n_gpu_layers: 99
    flash_attn: true
    cache_type_k: q8_0
    cache_type_v: q8_0
  llama:
    threads: 8
    parallel: 4
",
    )
    .expect("config fixture should be written");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    let qwen2 = loaded
      .config
      .arch_defaults
      .get("qwen2")
      .expect("qwen2 entry present");
    assert_eq!(qwen2.n_gpu_layers, Some(99));
    assert_eq!(qwen2.flash_attn, Some(true));
    assert_eq!(qwen2.cache_type_k.as_deref(), Some("q8_0"));
    assert_eq!(qwen2.cache_type_v.as_deref(), Some("q8_0"));
    let llama = loaded
      .config
      .arch_defaults
      .get("llama")
      .expect("llama entry present");
    assert_eq!(llama.threads, Some(8));
    assert_eq!(llama.parallel, Some(4));
    assert!(
      llama.n_gpu_layers.is_none(),
      "partial entry leaves rest None"
    );

    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn arch_defaults_absent_defaults_to_empty_map() {
    let cfg = Config::default();
    assert!(cfg.arch_defaults.is_empty());
  }

  #[test]
  fn load_config_from_path_rejects_directory_target_with_warning() {
    let dir = temp_test_dir("dir-target");
    // Point load_config_from_path at the directory itself, not a file in it.
    let loaded = load_config_from_path(&dir);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("non-regular-file target must surface a warning");
    assert!(
      warning.contains("not a regular file"),
      "warning should mention non-regular file, got: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }
}
