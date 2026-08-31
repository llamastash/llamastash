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

pub(crate) mod auth;
pub(crate) mod coalesce;
pub mod eviction;
pub(crate) mod failure_tracker;
pub(crate) mod forward;
pub(crate) mod launch;
pub(crate) mod mru;
pub(crate) mod ollama_compat;
pub(crate) mod openai;
pub(crate) mod route;
pub(crate) mod router;
pub mod server;
pub mod state;
pub(crate) mod ui;

pub use auth::{ProxyApiKey, ProxyAuth};
pub use route::DEFAULT_BODY_LIMIT_BYTES;
pub use server::{serve, ProxyStatus, StatusCell};
pub use state::ProxyState;
