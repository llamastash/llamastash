//! Daemon-side request context shared with every IPC handler and the
//! proxy listener.
//!
//! [`MethodContext`] bundles the daemon's live handles (catalog,
//! supervisor registry, GPU/host samplers, persisted state, launch
//! env) behind cheap `Arc`/`Clone` so both the IPC dispatcher and the
//! proxy can read daemon state without round-tripping through JSON-RPC.
//! It lives in the `daemon` layer (not `ipc`) so every dependency edge
//! points downward: `ipc` and `proxy` depend on `daemon`, never the
//! reverse.

use std::{
  path::PathBuf,
  sync::{atomic::AtomicUsize, Arc},
  time::Instant,
};

use tokio::sync::{Mutex, RwLock};

use crate::config::loader::PortRange;
use crate::daemon::host_metrics::{HostMetricsSnapshot, SamplerHandles};
use crate::daemon::orphans::ExternalProcess;
use crate::daemon::probe::ProbeOptions;
use crate::daemon::registry::SupervisorRegistry;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::{self, DaemonState};
use crate::discovery::ModelCatalog;
use crate::gpu::GpuInfo;

/// Shared state that the daemon hands to each request handler. Cheap to
/// clone (`Arc` inside).
#[derive(Clone)]
pub struct MethodContext {
  /// Wall-clock instant the daemon began listening. `version` reports
  /// uptime relative to this.
  pub started_at: Instant,
  /// Triggered by the `shutdown` method or by SIGINT/SIGTERM.
  pub shutdown: ShutdownToken,
  /// Live connection count. Maintained by the accept loop; surfaced via
  /// `version` so `daemon status` can show it without a separate method.
  pub active_connections: Arc<AtomicUsize>,
  /// Last time anything reached the daemon, over any surface. Stamped
  /// by the control-plane and proxy accept loops; read only by the
  /// opt-in idle-shutdown poller.
  pub activity: crate::daemon::idle::Activity,
  /// Catalog of currently-discovered models. Populated by the daemon's
  /// discovery task; read by the `list_models` handler. Cheap to clone
  /// (`Arc<RwLock<…>>`).
  pub catalog: ModelCatalog,
  /// Active supervisor instances keyed by `LaunchId`. Populated by
  /// `start_model` and consumed by `status`, `stop_model`,
  /// `logs_tail`. Empty in tests that only exercise the discovery
  /// surface.
  pub supervisors: SupervisorRegistry,
  /// Boot-time snapshot of `gpu::probe()`. Used by `status.gpu` only
  /// when the live sampler hasn't been attached (catalog-only tests);
  /// production wiring always overrides it with the sampler's live
  /// cell via [`Self::with_sampler`].
  pub gpu: Arc<GpuInfo>,
  /// Live `GpuInfo` cell the host-metrics sampler refreshes each
  /// tick. When `Some`, `status.gpu` reads from this lock so the
  /// wire shape follows hotplug / late-driver-load events instead of
  /// staying pinned to the daemon-start snapshot. `None` in
  /// catalog-only tests that skip the sampler.
  pub gpu_live: Option<Arc<RwLock<GpuInfo>>>,
  /// Live host-level metrics (CPU%, RAM, GPU util/temp/VRAM
  /// aggregates). Refreshed by the
  /// [`crate::daemon::host_metrics::spawn`] sampler at 1 Hz; `status`
  /// surfaces the most recent reading under the `host` field. A
  /// `None` value means no sampler was attached (catalog-only tests
  /// stay lightweight by leaving it off).
  pub host_metrics: Option<Arc<RwLock<HostMetricsSnapshot>>>,
  /// Persisted favorites / last_params / running snapshots.
  /// `start_model` and `favorite_*` mutate it and flush to `state.json`
  /// after each change.
  pub state: PersistedState,
  /// Live config-preset store (loaded from `Config.presets` at start,
  /// write-through to `config.yaml`). The single read/write surface the
  /// `presets_*` handlers and the `status` preset hint use. Empty +
  /// write-disabled in catalog-only tests.
  pub presets: crate::daemon::preset_store::ConfigPresetStore,
  /// Inputs the supervisor needs at launch time — binary path, port
  /// range, log directory, probe tuning. Optional because catalog-only
  /// IPC tests don't need to launch anything.
  pub launch: Option<LaunchEnv>,
  /// Snapshot of `llama-server` processes the daemon does *not*
  /// own. Populated by the orphan sweep at startup so `status`
  /// surfaces them read-only. Wrapped in `RwLock` so a periodic
  /// re-sweep can refresh the slot without rebuilding the context.
  pub external: Arc<RwLock<Vec<ExternalProcess>>>,
  /// Read handle to the proxy listener's status cell. The proxy
  /// task is the sole writer (every bind / disable transition lands
  /// here); the IPC `status` handler clones this and reads it to
  /// project the `proxy` block. `None` only in catalog-only tests
  /// that never bring the proxy up — the response then omits the
  /// `proxy` field entirely so existing test fixtures keep their
  /// shape. Production wiring always sets this; if `proxy.enabled:
  /// false` the cell holds the disabled proxy status variant.
  pub proxy_status: Option<crate::proxy::StatusCell>,
  /// HTTP control-plane URL the daemon bound, e.g.
  /// `http://127.0.0.1:48134`. Surfaced under `status.daemon.ipc_url`
  /// so the TUI / CLI can render where IPC is listening (helpful when
  /// debugging port-collision scans). `None` in catalog-only tests
  /// that don't bring up the control plane.
  pub ipc_url: Option<String>,
  /// All backend configuration, grouped under `backend:` in `config.yaml`.
  /// Each backend reads its own typed sub-config (`backend.llamacpp` /
  /// `backend.lemonade` / `backend.ds4`) through its `available`/`installed`/
  /// launch hooks; the generic context names no backend. Defaults to the
  /// factory config, so catalog-only tests never touch an external binary.
  pub backend: crate::backend::BackendConfig,
  /// Per-backend force-enable flags keyed by backend id (`--lemonade` /
  /// `LLAMASTASH_LEMONADE`, `--ds4` / `LLAMASTASH_DS4`). A backend folds its own
  /// entry into its `available` predicate alongside the config `enabled`
  /// tri-state; an absent key means "not forced". Keyed by id so the type names
  /// no backend.
  pub backend_force: std::collections::BTreeMap<String, bool>,
  /// Pre-spawn memory admission ledger. Shared across every launch
  /// entry point so check-and-reserve is atomic against concurrent
  /// launches; settled (released) when each child reaches Ready / Error
  /// / Stopped. In-memory by design.
  pub admission: Arc<crate::launch::admission::Ledger>,
}

/// Wrapper around the in-memory `DaemonState` plus the directory
/// `state.json` lives in. Mutations go through the wrapped
/// `Mutex`; flushes are best-effort and just log on failure so a
/// transient I/O error doesn't take the daemon down.
#[derive(Clone)]
pub struct PersistedState {
  state: Arc<Mutex<DaemonState>>,
  /// `None` disables persistence; mutations stay in-memory. Tests
  /// that don't care about durability use this mode.
  state_dir: Option<PathBuf>,
}

impl PersistedState {
  pub fn new(state: DaemonState, state_dir: Option<PathBuf>) -> Self {
    Self {
      state: Arc::new(Mutex::new(state)),
      state_dir,
    }
  }

  pub fn ephemeral() -> Self {
    Self::new(DaemonState::default(), None)
  }

  /// Snapshot — cheap clone of the inner state.
  pub async fn snapshot(&self) -> DaemonState {
    self.state.lock().await.clone()
  }

  /// Mutate under the lock and flush. The closure receives the
  /// state mutably and returns a value the caller cares about.
  /// `flush_after` short-circuits the write when persistence is
  /// disabled.
  pub async fn mutate<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&mut DaemonState) -> R,
  {
    let mut guard = self.state.lock().await;
    let out = f(&mut guard);
    if let Some(dir) = self.state_dir.as_ref() {
      if let Err(e) = state_store::save(dir, &guard) {
        log::warn!("state-store: failed to persist after mutation: {e}");
      }
    }
    out
  }
}

/// Resources the supervisor needs to actually launch a child.
/// Centralised here so `start_model` doesn't have to hand-roll five
/// optional fields on `MethodContext`.
#[derive(Clone)]
pub struct LaunchEnv {
  pub binary: PathBuf,
  pub port_range: PortRange,
  pub log_dir: PathBuf,
  pub probe: ProbeOptions,
  /// Per-architecture launch defaults sourced from
  /// `Config.arch_defaults` — user escape hatch over the built-in
  /// `(arch, gpu_backend)` table. The launch composition lands these
  /// on the `LayerLabel::ArchDefault` layer of the resolver, between
  /// `LastUsed` and `BuiltIn`. Empty map = no escape-hatch layer.
  pub arch_defaults: std::collections::BTreeMap<String, crate::config::TypedKnobs>,
  /// Neutral **server** catalog: every backend's configured servers (build /
  /// binary variants), each probed for its `--device` devices, with a derived
  /// stable id (see [`crate::backend::build_server_catalog`]). `start_model`
  /// looks the chosen `knobs.device` selector up here to decide *which* server
  /// binary to spawn; `status` projects it so the TUI picker offers exactly the
  /// selectors `--device` will accept.
  ///
  /// Behind a shared `RwLock` because it is populated by a background
  /// task *after* the daemon binds its listeners — probing each binary
  /// with `--list-devices` is best-effort I/O we never want on the
  /// startup critical path (the detached-start parent only waits a few
  /// seconds for `runtime.json`). Reads start empty and flip to the
  /// full set once the probe completes; a launch in that brief window
  /// finds no selector match and falls back to the default `binary`.
  pub servers: Arc<RwLock<Vec<crate::backend::Server>>>,
  /// Seed mode for knobs no layer filled. Sourced from
  /// `Config.default_launch_mode` (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`).
  pub default_launch_mode: crate::config::DefaultLaunchMode,
}

impl MethodContext {
  pub fn new(shutdown: ShutdownToken) -> Self {
    Self::with_catalog(shutdown, ModelCatalog::new())
  }

  /// Build a context with an externally-owned catalog. The daemon's
  /// `run_foreground` uses this to thread the same catalog into the
  /// discovery task and the dispatcher.
  pub fn with_catalog(shutdown: ShutdownToken, catalog: ModelCatalog) -> Self {
    Self {
      started_at: Instant::now(),
      shutdown,
      active_connections: Arc::new(AtomicUsize::new(0)),
      activity: crate::daemon::idle::Activity::new(),
      catalog,
      supervisors: SupervisorRegistry::new(),
      gpu: Arc::new(GpuInfo::CpuOnly),
      gpu_live: None,
      host_metrics: None,
      state: PersistedState::ephemeral(),
      presets: crate::daemon::preset_store::ConfigPresetStore::empty(),
      launch: None,
      external: Arc::new(RwLock::new(Vec::new())),
      proxy_status: None,
      ipc_url: None,
      backend: crate::backend::BackendConfig::default(),
      backend_force: std::collections::BTreeMap::new(),
      admission: Arc::new(crate::launch::admission::Ledger::default()),
    }
  }

  /// Builder helper: seed the external (unmanaged `llama-server`)
  /// process snapshot. Production wiring populates it from the
  /// startup orphan sweep.
  pub fn with_external(self, external: Vec<ExternalProcess>) -> Self {
    Self {
      external: Arc::new(RwLock::new(external)),
      ..self
    }
  }

  /// Builder helper: attach a supervisor registry. Used by
  /// `run_foreground` so the dispatcher and the daemon share one
  /// supervisor map.
  pub fn with_supervisors(mut self, supervisors: SupervisorRegistry) -> Self {
    self.supervisors = supervisors;
    self
  }

  /// Builder helper: attach a probed GPU info snapshot.
  pub fn with_gpu(mut self, gpu: GpuInfo) -> Self {
    self.gpu = Arc::new(gpu);
    self
  }

  /// Builder helper: attach both sampler cells (host snapshot + live
  /// GpuInfo) in one call. Production wiring uses this.
  pub fn with_sampler(mut self, handles: SamplerHandles) -> Self {
    self.host_metrics = Some(handles.snapshot);
    self.gpu_live = Some(handles.gpu);
    self
  }

  /// Builder helper: attach the persisted state-store handle.
  pub fn with_state(mut self, state: PersistedState) -> Self {
    self.state = state;
    self
  }

  /// Builder helper: attach the live config-preset store.
  pub fn with_presets(mut self, presets: crate::daemon::preset_store::ConfigPresetStore) -> Self {
    self.presets = presets;
    self
  }

  /// Builder helper: attach the launch environment. `start_model`
  /// requires this to be set; without it the handler returns an
  /// `InvalidRequest`.
  pub fn with_launch_env(mut self, env: LaunchEnv) -> Self {
    self.launch = Some(env);
    self
  }

  /// Builder helper: attach the proxy listener's status cell. The
  /// IPC `status` handler reads from this to surface the `proxy`
  /// block. Catalog-only tests skip this — the response
  /// then omits the `proxy` field entirely.
  pub fn with_proxy_status(mut self, status: crate::proxy::StatusCell) -> Self {
    self.proxy_status = Some(status);
    self
  }

  /// Builder helper: record the HTTP control-plane URL the daemon
  /// bound on so `status` can surface it. Set after the listener
  /// resolves its bound address (the port may differ from the
  /// configured one when a scan landed on an offset).
  pub fn with_ipc_url(mut self, url: impl Into<String>) -> Self {
    self.ipc_url = Some(url.into());
    self
  }

  /// Builder helper: attach the aggregate backend config + per-backend
  /// force-enable map so each backend's `available`/`installed`/launch hooks can
  /// resolve their own binary / enablement from `ctx`. Names no backend.
  pub fn with_backend(
    mut self,
    backend: crate::backend::BackendConfig,
    force: std::collections::BTreeMap<String, bool>,
  ) -> Self {
    self.backend = backend;
    self.backend_force = force;
    self
  }

  /// Whether the Lemonade backend is available on this daemon. Thin wrapper
  /// over [`crate::backend::Backend::available`] — the availability logic lives
  /// in the backend's own file. Retained only for the remaining direct callers;
  /// prefer iterating the registry (`Backends::all()` + `available`).
  pub fn lemonade_available(&self) -> bool {
    use crate::backend::Backend;
    crate::backend::lemonade::LemonadeBackend::new().available(self)
  }

  /// Whether the ds4 backend is available on this daemon. Thin wrapper over
  /// [`crate::backend::Backend::available`] — the availability logic lives in
  /// the backend's own file. Retained only for the remaining direct callers;
  /// prefer iterating the registry (`Backends::all()` + `available`).
  pub fn ds4_available(&self) -> bool {
    use crate::backend::Backend;
    crate::backend::ds4::Ds4Backend::new().available(self)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::ds4::{Ds4Backend, DS4_BACKEND_ID};
  use crate::backend::lemonade::{LemonadeBackend, LEMONADE_BACKEND_ID};
  use crate::backend::{Backend, BackendConfig};
  use crate::daemon::shutdown::ShutdownToken;

  /// The force-enable map key must match each backend's own id const, or the
  /// `--ds4` / `--lemonade` (and env) force can never override an explicit
  /// `enabled: false`. Points `binary` at a real file (this test binary) so
  /// availability's binary-resolve half passes under the `test-fixtures` build,
  /// which compiles out the PATH search — isolating the force-key logic.
  #[test]
  fn backend_force_key_overrides_explicit_off() {
    let exe = std::env::current_exe().expect("current exe");
    let force: std::collections::BTreeMap<String, bool> = [
      (DS4_BACKEND_ID.to_string(), true),
      (LEMONADE_BACKEND_ID.to_string(), true),
    ]
    .into_iter()
    .collect();
    let backend = BackendConfig {
      ds4: crate::config::Ds4Config {
        enabled: Some(false),
        servers: vec![crate::backend::ServerConfig {
          binary: exe.clone(),
          name: None,
        }],
      },
      lemonade: crate::config::LemonadeConfig {
        enabled: Some(false),
        servers: vec![crate::backend::ServerConfig {
          binary: exe,
          name: None,
        }],
        port: 13305,
      },
      ..Default::default()
    };
    let ctx = MethodContext::new(ShutdownToken::new()).with_backend(backend.clone(), force);
    assert!(
      Ds4Backend::new().available(&ctx),
      "ds4 force must override an explicit enabled:false"
    );
    assert!(
      LemonadeBackend::new().available(&ctx),
      "lemonade force must override an explicit enabled:false"
    );

    // Without the force entries, the explicit `enabled: false` wins → unavailable.
    let ctx_off = MethodContext::new(ShutdownToken::new())
      .with_backend(backend, std::collections::BTreeMap::new());
    assert!(!Ds4Backend::new().available(&ctx_off));
    assert!(!LemonadeBackend::new().available(&ctx_off));
  }
}
