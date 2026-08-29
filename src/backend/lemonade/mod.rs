//! Lemonade (`lemond`) managed-multiplexer backend.
//!
//! `lemond` is a long-lived umbrella process exposing an OpenAI-compatible
//! HTTP API; llamastash supervises the one process and delegates per-model
//! start / list to its API. This module holds the typed API
//! [`client`], the [`Backend`](crate::backend::Backend) implementation
//! ([`backend`]), and the umbrella lifecycle ([`orchestrate`]).

pub mod backend;
pub mod client;
pub mod discovery;
pub mod knobs;
pub mod orchestrate;

use serde::{Deserialize, Serialize};

pub use backend::{
  registry_name_from_path, resolve_lemond_binary, umbrella_port_available, umbrella_port_state,
  umbrella_process_spec, LemonadeBackend, UmbrellaPortState, LEMONADE_BACKEND_ID,
  LEMONADE_PATH_SCHEME,
};
pub use client::{LemonadeClient, LemonadeError, LoadOptions, ModelEntry};
pub use orchestrate::{ensure_umbrella, umbrella_launch_id};

/// Lemonade backend configuration (R9/R11 opt-in gate). **Experimental** —
/// the backend is new and lightly road-tested; these keys may change.
///
/// Default-on when the `lemond` binary resolves. Only when enabled (intent +
/// binary) does the daemon run Lemonade discovery, supervise the umbrella, and
/// route to it. llamastash never downloads or installs `lemond` — the user sets
/// up Lemonade manually (see `docs/lemonade-setup.md`). `binary` is an explicit
/// path to the user's `lemond`; when unset the umbrella launch falls back to a
/// `lemond` (or `lemonade`) on `PATH`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct LemonadeConfig {
  /// Tri-state enablement, mirroring [`crate::backend::ds4::Ds4Config::enabled`]:
  /// unset = auto/on-when-`lemond`-found, `false` = force off, `true` = force on.
  #[serde(default)]
  pub enabled: Option<bool>,
  /// The `lemond` umbrella binary (single-entry — Lemonade runs one umbrella).
  /// Unset falls back to a `lemond` (or `lemonade`) on `$PATH`.
  #[serde(default)]
  pub servers: Vec<crate::backend::ServerConfig>,
  /// Loopback port the `lemond` umbrella binds and that discovery probes
  /// for the model list. Defaults to Lemonade's own default (`13305`).
  #[serde(default = "LemonadeConfig::default_port")]
  pub port: u16,
}

impl LemonadeConfig {
  /// Lemonade's documented default port.
  pub fn default_port() -> u16 {
    13305
  }

  /// Whether the user *intends* Lemonade enabled, given the force flag
  /// (`--lemonade` / `LLAMASTASH_LEMONADE`). Actual activation still requires
  /// the `lemond` binary to resolve — this only encodes intent (default-on
  /// unless explicitly `enabled: false`, which the force flag overrides).
  /// Mirrors [`crate::backend::ds4::Ds4Config::intends_enabled`].
  pub fn intends_enabled(&self, force: bool) -> bool {
    force || self.enabled != Some(false)
  }
}

impl Default for LemonadeConfig {
  fn default() -> Self {
    Self {
      enabled: None,
      servers: Vec::new(),
      port: Self::default_port(),
    }
  }
}

impl LemonadeConfig {
  /// The configured `lemond` path (first server), if any.
  pub fn primary_binary(&self) -> Option<&std::path::Path> {
    self.servers.first().map(|s| s.binary.as_path())
  }
}
