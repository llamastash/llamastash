//! Shared state every proxy connection's handlers read from.
//!
//! Cloned-and-`Arc`-wrapped fields mirror the relevant slots of
//! [`crate::daemon::context::MethodContext`] — the catalog, the
//! supervisor registry, persisted state, and the launch env — so
//! the proxy can answer requests without round-tripping through the
//! IPC dispatcher. `/health` reads `catalog` and `supervisors` (for
//! its `models_loaded` / `models_discovered` counts); the routing and
//! forwarding paths lean on the rest.

use std::sync::Arc;
use std::time::Duration;

use crate::daemon::context::MethodContext;

use super::coalesce::Coalesce;
use super::failure_tracker::FailureTracker;
use super::mru::MruTracker;

/// Cheap-to-clone bundle of the daemon-side handles the proxy needs.
/// The inner `Arc`s make per-connection cloning a single refcount
/// bump — the `service_fn` closure clones a fresh handle for every
/// inbound HTTP connection so handler futures don't borrow across
/// scheduler boundaries.
///
/// Catalog / supervisor registry / persisted state / launch env are
/// reached through the embedded [`MethodContext`] so
/// there is a single source for daemon-side handles — no duplication
/// between flattened fields and `ctx`, which would let readers drift
/// out of sync with each other.
#[derive(Clone)]
pub struct ProxyState {
  /// Pooled HTTP client used by the forwarding path. One per proxy
  /// process — hyper handles keep-alive per-host inside the pool, so
  /// a second client would just be cargo cult. Wrapped in `Arc` so
  /// the per-connection clone is a refcount bump rather than
  /// rebuilding the pool.
  pub http_client: Arc<reqwest::Client>,
  /// Single-flight coalesce map for the auto-start path. Keyed on
  /// [`crate::gguf::identity::ModelId`] so two concurrent requests
  /// with different fuzzy spellings of the same model share one
  /// launch.
  pub(crate) coalesce: Coalesce,
  /// In-memory `last_request_at` tracker. The fallback selector
  /// reads from it; `route::forward_request` writes to it as
  /// forwarding starts (per the plan's "as it starts forwarding,
  /// not on completion" rule).
  pub(crate) mru: MruTracker,
  /// Per-`ModelId` recent-failure log for the auto-start path. Once a
  /// model trips the threshold, further auto-start requests
  /// short-circuit with a clear cause instead of spawning another
  /// llama-server child that will fail the same way. Cleared on a
  /// successful launch.
  pub(crate) failures: Arc<FailureTracker>,
  /// Full IPC context handle. Cheap to clone (every field is
  /// already `Arc`-wrapped); the proxy reads catalog / supervisors /
  /// persisted state / launch env through it so there is no
  /// duplicate-state risk.
  pub(crate) ctx: MethodContext,
  /// `true` when the daemon is running in Ollama drop-in mode (see
  /// [`crate::config::loader::ProxyConfig::ollama_compat`]). Drives
  /// the `GET /` identity body (`"Ollama is running"` vs the default
  /// `"LlamaStash is running"`); no other surface branches on it. Set
  /// once at daemon startup; never mutated thereafter.
  pub(crate) ollama_compat: bool,
  /// `true` when the family-MRU fallback is enabled (the default). When
  /// `false`, a failed auto-start returns a 503 `launch_failed`
  /// envelope directly instead of picking another Ready supervisor and
  /// serving the request with `x-llamastash-fallback-reason`. Set once
  /// at daemon startup from
  /// [`crate::config::loader::ProxyConfig::fallback_enabled`] (after
  /// the CLI / env OR-chain that may force it off); never mutated
  /// thereafter.
  pub(crate) fallback_enabled: bool,
  /// Bearer-token policy for the data routes. Built from the resolved
  /// `ProxyConfig::api_key`; when no key is configured this is the
  /// disabled policy and [`crate::proxy::auth::ProxyAuth::enforced`]
  /// returns `false`, so the router skips the check entirely. Set once
  /// at daemon startup; never mutated thereafter.
  pub(crate) auth: super::auth::ProxyAuth,
  /// Whether the listener binds a loopback address; gates the router's `Host` check.
  pub(crate) loopback_bind: bool,
  /// Inbound body size cap in bytes, applied to every body the proxy
  /// buffers before forwarding (data plane + `/ui`). From
  /// [`crate::config::loader::ProxyConfig::max_body_size`] (default
  /// [`super::route::DEFAULT_BODY_LIMIT_BYTES`], 16 MiB); `0` disables
  /// the check (no cap). Set once at daemon startup; never mutated
  /// thereafter.
  pub(crate) max_body_size: usize,
}

impl ProxyState {
  /// Stamp `last_request_at` for `id`. Production use is the
  /// proxy's auto-start path (`proxy::launch::drive_launch_as_leader`)
  /// and the request-forward path (`proxy::router::forward_request`);
  /// integration tests under the `test-fixtures` feature also reach
  /// for this to seed the MRU before a manual `eviction::sweep_once`
  /// call.
  pub async fn touch_mru(&self, id: &crate::gguf::identity::ModelId) {
    self.mru.touch(id).await;
  }

  /// Project the relevant handles out of an existing [`MethodContext`].
  /// The proxy task receives this handle from `run_foreground` after
  /// the rest of the daemon context has been assembled. `ollama_compat`
  /// and `fallback_enabled` reflect the resolved bools from
  /// `ProxyConfig` (after the CLI / env OR-chain); `max_body_size` the
  /// resolved body cap. Auth is disabled (loopback, same-UID posture)
  /// — production uses [`Self::from_context_with_auth`] to attach a
  /// bearer key, and the bind is loopback. The `proxy.aliases` table rides
  /// in `ctx` like every other
  /// daemon-side handle, so the proxy and the `proxy_aliases` handler read one
  /// table with one warn set.
  pub fn from_context(
    ctx: &MethodContext,
    ollama_compat: bool,
    fallback_enabled: bool,
    max_body_size: usize,
  ) -> Arc<Self> {
    Self::from_context_with_auth(
      ctx,
      ollama_compat,
      fallback_enabled,
      None,
      max_body_size,
      true,
    )
  }

  /// Like [`Self::from_context`] but with a resolved bearer `api_key`
  /// (`None` disables auth) and the resolved `loopback_bind` (whether the
  /// listener binds a loopback address). The daemon's `run_foreground`
  /// passes both from the loaded [`crate::config::loader::ProxyConfig`];
  /// tests use the keyless [`Self::from_context`].
  pub fn from_context_with_auth(
    ctx: &MethodContext,
    ollama_compat: bool,
    fallback_enabled: bool,
    api_key: Option<String>,
    max_body_size: usize,
    loopback_bind: bool,
  ) -> Arc<Self> {
    Arc::new(Self {
      http_client: Arc::new(build_http_client()),
      coalesce: Coalesce::new(),
      mru: MruTracker::new(),
      failures: Arc::new(FailureTracker::new()),
      ctx: ctx.clone(),
      ollama_compat,
      fallback_enabled,
      auth: super::auth::ProxyAuth::new(api_key),
      max_body_size,
      loopback_bind,
    })
  }
}

/// Build the proxy's pooled HTTP client. Single source so tests can
/// reach into the same pool if they ever need to. The settings here
/// target the loopback `llama-server` upstream: short-ish connect
/// timeout (the child is on the same machine, anything > 5 s is a
/// real bug), no request timeout (chat completions are arbitrarily
/// long-running by design), pooling kept on so repeated requests
/// against the same port reuse keep-alive.
fn build_http_client() -> reqwest::Client {
  reqwest::Client::builder()
    .connect_timeout(Duration::from_secs(5))
    .pool_idle_timeout(Duration::from_secs(90))
    .build()
    // Builder failures here would be a misconfigured TLS stack /
    // missing certificate root. Loopback HTTP has none of those — we
    // never hit a network. If this ever panics in production the
    // build is broken, not the runtime.
    .expect("reqwest client must build on a healthy runtime")
}
