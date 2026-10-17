//! OpenAI-compatible HTTP proxy router.
//!
//! The proxy runs alongside the IPC Unix-socket server inside the
//! daemon process, exposing a single loopback TCP listener that
//! agent clients (OpenCode, Pi, anything OpenAI-shaped) can attach
//! to. It stands up the listener, then layers `/v1/models`,
//! body-model resolution, forwarding, auto-start + fallback, and the
//! status surface on top.
//!
//! Scope: loopback-only, same-UID, no auth, no TLS, no LAN binding,
//! no MCP, no HTTP/2.

pub(crate) mod alias;
pub(crate) mod auth;
pub(crate) mod coalesce;
pub mod eviction;
pub(crate) mod failure_tracker;
pub(crate) mod forward;
pub(crate) mod launch;
pub(crate) mod metrics;
pub(crate) mod metrics_backend;
pub(crate) mod mru;
pub(crate) mod ollama_compat;
pub(crate) mod openai;
pub mod request_log;
pub(crate) mod route;
pub(crate) mod router;
pub mod server;
pub mod state;
pub(crate) mod ui;
pub(crate) mod ui_bundle;
pub mod usage_tap;

pub use auth::{ProxyApiKey, ProxyAuth};
pub use route::DEFAULT_BODY_LIMIT_BYTES;
pub use server::{serve, ProxyStatus, StatusCell};
pub use state::ProxyState;

/// Shared fixtures for the proxy modules' inline `#[cfg(test)]` modules. Not
/// behind the `test-fixtures` feature: these build no child process.
#[cfg(test)]
pub(crate) mod test_support {
  use std::sync::Arc;

  use super::{state::ProxyState, DEFAULT_BODY_LIMIT_BYTES};
  use crate::daemon::{
    context::MethodContext,
    registry::{LaunchId, SupervisorRegistry},
    shutdown::ShutdownToken,
    supervisor::{test_support::parked, ManagedState},
  };

  /// A `ProxyState` whose registry holds `(launch id, state, path, port)`.
  /// The catalog stays empty, so served names fall back to the file stem.
  pub(crate) async fn state_with(rows: &[(&str, ManagedState, &str, u16)]) -> Arc<ProxyState> {
    let supervisors = SupervisorRegistry::new();
    for (id, state, path, port) in rows {
      supervisors
        .insert(
          LaunchId((*id).to_string()),
          parked((*state).clone(), path, *port),
        )
        .await;
    }
    let ctx = MethodContext::new(ShutdownToken::new()).with_supervisors(supervisors);
    ProxyState::from_context(&ctx, false, true, DEFAULT_BODY_LIMIT_BYTES)
  }
}
