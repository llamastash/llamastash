//! [`Backend`] implementation for Lemonade (`lemond`) — the first
//! managed-multiplexer (R2 shape 2).
//!
//! Unlike llama.cpp (one process per model), Lemonade is one long-lived
//! `lemond` umbrella that serves many models behind its API. So
//! [`LemonadeBackend::prepare_launch`] produces a
//! [`LaunchPlan::DelegateToManager`]: the umbrella `ProcessLaunchSpec` the
//! generic supervisor uses to ensure `lemond` is up (probed via `/live`),
//! plus the model name to serve. The actual API calls happen at execution
//! time, keeping `prepare_launch` synchronous and infallible.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::super::identity::{BackendModelId, ModelIdentity};
use super::super::{
  Accelerator, AcceleratorSupport, Backend, KnobCapability, LaunchPlan, Lifecycle,
  ManagerLaunchSpec, ManagerModelRef, ProcessLaunchSpec, Readiness,
};
use super::{umbrella_launch_id, LemonadeClient};
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::launch::params::LaunchParams;

/// Stable backend id (mirrors Lemonade's own `lemonade` naming).
pub const LEMONADE_BACKEND_ID: &str = "lemonade";

/// URI scheme for a Lemonade-registry model's synthetic catalog path
/// (`lemonade://<registry name>`). A Lemonade model has no local GGUF, so
/// discovery mints this file-less path as the catalog key and the launch path
/// reads the registry name back off it. Single source of truth for both the
/// minting side ([`crate::backend::lemonade::discovery`]) and the parsing side
/// ([`registry_name_from_path`]).
pub const LEMONADE_PATH_SCHEME: &str = "lemonade://";

/// Parse a Lemonade synthetic path back to the registry model name the umbrella
/// API knows, or `None` for any non-Lemonade path. The inverse of
/// `discovery::synthetic_path`: `lemonade://Whisper-Tiny` →
/// `Some("Whisper-Tiny")`, `/models/x.gguf` → `None`. The launch path uses this
/// to recognise a managed-multiplexer model and skip the GGUF header read.
pub fn registry_name_from_path(path: &Path) -> Option<&str> {
  path.to_str()?.strip_prefix(LEMONADE_PATH_SCHEME)
}

/// `lemond`'s root liveness endpoint — minimal-work readiness probe for
/// the umbrella process (distinct from `/api/v1/health`, which reports
/// loaded models).
const LIVE_PATH: &str = "/live";

/// The Lemonade backend.
#[derive(Debug, Clone)]
pub struct LemonadeBackend {
  capabilities: KnobCapability,
}

impl LemonadeBackend {
  pub fn new() -> Self {
    // lemond is driven by a model name plus a small load-options surface
    // (`POST /api/v1/load`): `ctx_size` maps onto the `Ctx` knob, and the
    // free-form extras ride the recipe-scoped `*_args` string (a separate
    // channel from the typed-knob IR). No other typed knob is honored —
    // notably `-ngl` is on lemond's exclusion list, so offload knobs can
    // never pass through (R6: drop + hide).
    Self {
      capabilities: KnobCapability::of(&[crate::launch::flag_aliases::KnobField::Ctx]),
    }
  }

  /// Derive the `lemond` registry model name from the launch input.
  ///
  /// The catalog feeds the launch a synthetic `lemonade://<name>` path; strip
  /// the scheme so the umbrella API gets the bare registry name (`Whisper-Tiny`,
  /// not `lemonade://Whisper-Tiny`). A non-scheme path (a GGUF launched with an
  /// explicit `--backend lemonade` override) is passed through verbatim.
  fn registry_name(path: &Path) -> String {
    match registry_name_from_path(path) {
      Some(name) => name.to_string(),
      None => path.to_string_lossy().into_owned(),
    }
  }

  /// Build the umbrella spec + model ref directly (the body
  /// [`Backend::prepare_launch`] wraps in a [`LaunchPlan`]).
  pub fn manager_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ManagerLaunchSpec {
    ManagerLaunchSpec {
      umbrella: umbrella_process_spec(port, binary, probe),
      model: ManagerModelRef {
        name: Self::registry_name(&params.model_path),
      },
    }
  }
}

/// Build the `lemond` umbrella's [`ProcessLaunchSpec`] for a loopback
/// `port` and a resolved `binary`. Shared by [`LemonadeBackend::manager_spec`]
/// (the per-model start path) and the daemon's boot-time umbrella supervision
/// — both must produce an *identical* spec so [`super::ensure_umbrella`]
/// treats them as the one shared umbrella.
pub fn umbrella_process_spec(port: u16, binary: PathBuf, probe: ProbeOptions) -> ProcessLaunchSpec {
  // `lemond --host 127.0.0.1 --port <port>`: --host/--port override
  // config.json so llamastash owns the loopback binding. `lemond`'s
  // positional `cache_dir` (config.json + model data, default
  // `~/.cache/lemonade`) is deliberately NOT passed: the default is shared
  // with any manual `lemond` runs, so models download once. Passing the
  // binary's parent here (an earlier revision did) breaks system installs —
  // `lemond` at `/usr/bin/lemond` would try to write
  // `/usr/bin/config.json.tmp` and die on the read-only dir.
  let argv = vec![
    OsString::from("--host"),
    OsString::from("127.0.0.1"),
    OsString::from("--port"),
    OsString::from(port.to_string()),
  ];
  ProcessLaunchSpec {
    binary,
    argv,
    // lemond does not read the llama-server LLAMA_ARG_* bypass vars, and
    // it may legitimately use HF_* to pull models, so nothing is stripped
    // here. (Revisit if lemond honors a loopback-bypass env.)
    env_remove: vec![],
    readiness: Readiness::HttpPoll {
      path: LIVE_PATH.to_string(),
      ready_status: 200,
    },
    probe,
  }
}

/// Resolve the `lemond` executable the daemon should supervise.
///
/// Resolution order (matches `docs/lemonade-setup.md`):
///   1. the explicit `lemonade.binary` path, if it points at a file;
///   2. otherwise `lemond` then `lemonade` on `PATH`.
///
/// Returns the resolved *canonical absolute* path, or `None` when nothing is
/// found — llamastash never installs `lemond`, so a missing binary is a clean
/// "backend unavailable" rather than an error to recover from.
///
/// The path is canonicalized so a relative `lemonade.binary` (or a relative
/// PATH entry) still yields an absolute path: the umbrella is spawned and
/// registered under this path (it doubles as the supervisor's synthetic model
/// id), so it must not depend on the daemon's CWD.
pub fn resolve_lemond_binary(cfg: &crate::config::LemonadeConfig) -> Option<PathBuf> {
  // `is_file()` already confirmed the target exists, so canonicalize should
  // succeed; fall back to the verbatim path if it somehow doesn't.
  fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
  }
  if let Some(explicit) = cfg.primary_binary() {
    return explicit.is_file().then(|| canonical(explicit));
  }
  // Never auto-discover a host `lemond`/`lemonade` on `PATH` under the
  // test-fixtures build: integration tests spawn the real daemon subprocess,
  // which (with Lemonade default-on) would otherwise pick up — and leak — the
  // developer's system `lemond`. Tests point at an explicit fake
  // `lemonade.binary` instead.
  #[cfg(feature = "test-fixtures")]
  {
    None
  }
  #[cfg(not(feature = "test-fixtures"))]
  {
    let names: &[&str] = if cfg!(windows) {
      &["lemond.exe", "lemonade.exe"]
    } else {
      &["lemond", "lemonade"]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
      for name in names {
        let candidate = dir.join(name);
        if candidate.is_file() {
          return Some(canonical(&candidate));
        }
      }
    }
    None
  }
}

/// What the umbrella's loopback `port` looks like to a bind attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UmbrellaPortState {
  /// Bind succeeded — the port is free for the managed `lemond`.
  Free,
  /// Bind failed and something *answers* a connect — a live foreign
  /// process (e.g. a hand-started `lemond`) owns the port. Spawning
  /// would lose the bind race while the foreigner keeps answering the
  /// `/live` readiness probe, logging a false "umbrella supervised"
  /// that only fails later, opaquely, at routing time. Fail fast.
  Listening,
  /// Bind failed but nothing answers — teardown remnants (FIN-WAIT-2 /
  /// TIME-WAIT sockets from a dying `lemond`'s connections) still hold
  /// the port. The kernel clears them within its fin-timeout (~60 s);
  /// callers should wait/retry rather than refuse.
  Remnants,
}

/// Probe the umbrella's loopback `port`: bind (and immediately drop the
/// listener) to test ownership, then distinguish a live owner from
/// kernel teardown remnants with a connect probe. The check-then-spawn
/// gap is a benign race: this is a diagnostic, not a lock.
pub fn umbrella_port_state(port: u16) -> UmbrellaPortState {
  if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
    return UmbrellaPortState::Free;
  }
  let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
  match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(250)) {
    Ok(_) => UmbrellaPortState::Listening,
    // ECONNREFUSED (no listener) → remnants. Treat any other failure
    // (timeout, EPERM, …) as a live-but-unresponsive owner: refusing is
    // the conservative read, matching the old bind-only behavior.
    Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => UmbrellaPortState::Remnants,
    Err(_) => UmbrellaPortState::Listening,
  }
}

/// True when the umbrella's loopback `port` is free to bind. See
/// [`umbrella_port_state`] for the three-way reading callers that can
/// wait out remnants should prefer.
pub fn umbrella_port_available(port: u16) -> bool {
  umbrella_port_state(port) == UmbrellaPortState::Free
}

impl Default for LemonadeBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl Backend for LemonadeBackend {
  fn knobs(&self) -> &'static [crate::launch::knobs::KnobDef] {
    super::knobs::KNOBS
  }
  fn id(&self) -> &'static str {
    LEMONADE_BACKEND_ID
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ManagedMultiplexer
  }

  fn capabilities(&self) -> &KnobCapability {
    &self.capabilities
  }

  fn accelerators(&self) -> AcceleratorSupport {
    // Lemonade's reason to exist is the NPU; it also runs CPU. (GPU support
    // exists in some lemond builds; a live `/api/v1/system-info` probe to
    // confirm it on this host is deferred — see the plan.)
    AcceleratorSupport::from_list([Accelerator::Cpu, Accelerator::Npu])
  }

  fn identify(&self, path: &Path, _header_bytes: &[u8]) -> ModelIdentity {
    // A Lemonade-registry model has no local GGUF header. Identity is
    // (backend, registry name); the header bytes are ignored. See
    // `registry_name` for how the name is sourced.
    ModelIdentity::Backend(BackendModelId {
      backend: LEMONADE_BACKEND_ID.to_string(),
      name: Self::registry_name(path),
    })
  }

  fn synthetic_identity(&self, path: &Path) -> Option<ModelIdentity> {
    // Only a `lemonade://<name>` synthetic path yields an identity; a local
    // GGUF (or any other path) is `None`, so the generic launch / status path
    // falls through to the file-keyed branch for it.
    registry_name_from_path(path).map(|name| {
      ModelIdentity::Backend(BackendModelId {
        backend: LEMONADE_BACKEND_ID.to_string(),
        name: name.to_string(),
      })
    })
  }

  fn umbrella_openai_prefix(&self) -> Option<&'static str> {
    // lemond serves its OpenAI surface under `/api` (`/api/v1/...`).
    Some("/api")
  }

  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan {
    LaunchPlan::DelegateToManager(self.manager_spec(params, port, binary, probe))
  }

  fn available(&self, ctx: &MethodContext) -> bool {
    // Intent (default-on unless `lemonade.enabled: false`, `--lemonade`/env
    // force) AND the `lemond` binary resolves. Consulted by selection and
    // `status`.
    let force = ctx
      .backend_force
      .get(LEMONADE_BACKEND_ID)
      .copied()
      .unwrap_or(false);
    ctx.backend.lemonade.intends_enabled(force)
      && resolve_lemond_binary(&ctx.backend.lemonade).is_some()
  }

  fn installed(&self, ctx: &MethodContext) -> bool {
    // Presence of the binary, independent of the enablement toggle. Honors the
    // full resolution order (explicit path, then PATH).
    resolve_lemond_binary(&ctx.backend.lemonade).is_some()
  }

  fn status_enabled(&self, ctx: &MethodContext) -> Option<bool> {
    Some(self.available(ctx))
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    resolve_lemond_binary(&ctx.backend.lemonade).map(|b| b.display().to_string())
  }

  fn configured_servers(&self, ctx: &MethodContext) -> Vec<crate::backend::ServerSpec> {
    // One umbrella server when Lemonade is available; no device probe.
    if !self.available(ctx) {
      return Vec::new();
    }
    resolve_lemond_binary(&ctx.backend.lemonade)
      .map(|binary| {
        vec![crate::backend::ServerSpec {
          binary,
          name: ctx
            .backend
            .lemonade
            .servers
            .first()
            .and_then(|s| s.name.clone()),
        }]
      })
      .unwrap_or_default()
  }

  fn config_servers(&self, config: &crate::config::Config) -> Vec<crate::backend::ServerConfig> {
    config.backend.lemonade.servers.clone()
  }

  async fn status_accelerators(&self, ctx: &MethodContext, _device: &[Accelerator]) -> Vec<String> {
    // Prefer what lemond actually has installed (live `system-info` probe) over
    // the static capability floor — the static `[cpu, npu]` misses a
    // ROCm/Vulkan build and claims an NPU the host may not have. Falls back to
    // the floor when the umbrella is down or the probe fails. The device catalog
    // is *not* unioned here (unlike the default): the live probe already
    // reflects the real installed accelerators.
    let support = self
      .installed_accelerators(ctx)
      .await
      .unwrap_or_else(|| self.accelerators());
    support.labels().into_iter().map(str::to_string).collect()
  }

  async fn status_extra(&self, ctx: &MethodContext) -> Vec<(String, serde_json::Value)> {
    // Managed-multiplexer health: whether the umbrella llamastash supervises is
    // actually up, so `status` distinguishes "installed" from "running".
    vec![(
      "umbrella".to_string(),
      serde_json::json!(umbrella_state(ctx).await),
    )]
  }

  fn umbrella_launch_id(&self) -> Option<crate::daemon::registry::LaunchId> {
    // The one long-lived `lemond` process llamastash supervises — an infra
    // launch, not a servable model, so the running-launch walkers skip it.
    Some(umbrella_launch_id())
  }

  fn supervise_at_boot(
    &self,
    ctx: &MethodContext,
    log_dir: &Path,
    probe_timeout: Option<std::time::Duration>,
  ) {
    // Opt-in: only when the backend is enabled and the `lemond` binary
    // resolves. Bring the one shared umbrella up in a detached task so
    // discovery + proxy routing work before any explicit `start`.
    if !self.available(ctx) {
      return;
    }
    super::orchestrate::supervise_umbrella_at_boot(
      ctx.supervisors.clone(),
      &ctx.backend.lemonade,
      log_dir,
      probe_timeout,
    );
  }

  async fn stop(
    &self,
    ctx: &MethodContext,
    launch_id: &crate::daemon::registry::LaunchId,
    grace_secs: u64,
  ) -> Result<serde_json::Value, crate::ipc::protocol::ErrorObject> {
    use crate::daemon::supervisor::ManagedState;
    use crate::ipc::protocol::{ErrorCode, ErrorObject};
    // The umbrella itself → stop the shared `lemond` process, then reap every
    // delegated row it took down with it (ghost rows the next fresh umbrella
    // can't honor) and clear the delegated-state map.
    if self.umbrella_launch_id().as_ref() == Some(launch_id) {
      let resp = crate::daemon::launch_service::stop_supervised(ctx, launch_id, grace_secs).await?;
      ctx
        .state
        .mutate(|s| s.running.retain(|r| r.delegated_backend_id().is_none()))
        .await;
      ctx.supervisors.clear_delegated().await;
      return Ok(resp);
    }
    // A delegated model → best-effort unload from the umbrella (which keeps
    // running), then drop its running snapshot so `status` stops emitting the
    // row. Reverse-map the `L#` → model name off the snapshot (delegated rows
    // have no supervisor to hold it). An unload refusal is logged but doesn't
    // fail the stop — the snapshot is the daemon's own bookkeeping, and a model
    // the umbrella already evicted should always be clearable.
    let name = ctx
      .state
      .snapshot()
      .await
      .running
      .into_iter()
      .find(|r| r.launch_id.as_ref() == Some(launch_id))
      .and_then(|r| r.delegated_backend_id().map(|b| b.name.clone()));
    let Some(name) = name else {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("unknown launch_id: {}", launch_id.as_str()),
      ));
    };
    if let Err(e) = self.unload_delegated(ctx, &name).await {
      log::warn!("lemonade: unload of `{name}` failed (dropping the row anyway): {e}");
    }
    ctx.supervisors.remove_delegated(&name).await;
    ctx
      .state
      .mutate(move |s| {
        s.running
          .retain(|r| r.delegated_backend_id().map(|b| b.name.as_str()) != Some(name.as_str()));
      })
      .await;
    Ok(serde_json::json!({
      "launch_id": launch_id,
      "state": crate::ipc::methods::flatten_state(&ManagedState::Stopped),
    }))
  }

  fn resolve_launch_binary(
    &self,
    ctx: &MethodContext,
    _default_binary: PathBuf,
    _port: u16,
  ) -> Result<(PathBuf, u16), String> {
    // The umbrella supervises its own `lemond` executable on its own configured
    // loopback port, not the launch-pool reservation.
    match resolve_lemond_binary(&ctx.backend.lemonade) {
      Some(bin) => Ok((bin, ctx.backend.lemonade.port)),
      None => Err(
        "lemonade backend selected but no `lemond` binary found; set `lemonade.binary` \
         or put `lemond` on PATH (see docs/lemonade-setup.md)"
          .to_string(),
      ),
    }
  }

  /// Delegate the launch to the shared `lemond` umbrella instead of spawning a
  /// child per model: ensure the one umbrella is up, then preload the model
  /// behind it. This is the managed-multiplexer half of the `start` contract —
  /// the whole reason a Lemonade model never touches the process-per-model path.
  async fn start(
    &self,
    ctx: &MethodContext,
    exec: crate::daemon::launch_service::LaunchExec,
  ) -> Result<crate::daemon::launch_service::StartedLaunch, crate::ipc::protocol::ErrorObject> {
    use super::ensure_umbrella;
    use crate::daemon::launch_service::StartedLaunch;
    use crate::daemon::state_store::RunningSnapshot;
    use crate::daemon::supervisor::ManagedState;
    use crate::ipc::protocol::{ErrorCode, ErrorObject};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // The umbrella supervises its own `lemond` on its configured loopback port,
    // not the launch-pool reservation — release the reserved slot (holding it
    // would leak a port from the pool for the umbrella's lifetime).
    let (binary, umbrella_port) =
      match self.resolve_launch_binary(ctx, exec.default_binary.clone(), exec.reserved_port) {
        Ok(bp) => bp,
        Err(msg) => {
          ctx
            .supervisors
            .release_reserved_port(exec.reserved_port)
            .await;
          return Err(ErrorObject::new(ErrorCode::InvalidParams, msg));
        }
      };
    ctx
      .supervisors
      .release_reserved_port(exec.reserved_port)
      .await;

    let mgr = self.manager_spec(&exec.params, umbrella_port, binary, exec.probe);
    let model_name = mgr.model.name.clone();
    let umbrella_spec = mgr.umbrella;

    // The preload must not POST `/api/v1/load` until the umbrella's HTTP server
    // is accepting connections; bound the readiness wait by the umbrella's own
    // probe budget (its probe resolves to Ready/Error first, so this is only a
    // backstop for an umbrella that never settles).
    let ready_timeout = umbrella_spec.probe.timeout + Duration::from_secs(2);

    let umbrella = match ensure_umbrella(
      &ctx.supervisors,
      umbrella_port,
      umbrella_spec,
      exec.log_path.clone(),
    )
    .await
    {
      Ok(m) => m,
      Err(e) => {
        return Err(ErrorObject::new(
          ErrorCode::InternalError,
          format!("lemonade umbrella failed to start: {e}"),
        ));
      }
    };
    // An already-running umbrella keeps its own port; trust the handle over the
    // requested `umbrella_port` so a reused umbrella routes to the right place.
    let serving_port = umbrella.port();

    // One id source for every backend: draw the delegated model's `L#` from the
    // same registry counter the process path uses (`next_id`), so a Lemonade row
    // reads `L3` like any other launch. The model name stays the umbrella's
    // internal load/unload key; the `L#` is the user-facing handle, stamped on
    // the snapshot below (delegated models have no supervisor to hold it) and
    // reverse-mapped to the name on stop.
    let launch_id = ctx.supervisors.next_id();

    // Preload so an explicit launch makes the model resident (chat would autoload
    // too), forwarding the params lemond honors: `ctx_size` plus the free-form
    // extras as the recipe-scoped `*_args` string.
    //
    // Runs as a background task: a cold load can take lemond's full 120 s budget,
    // far past the CLI's IPC reply timeout — awaiting here meant the client hung
    // up and hyper cancelled the handler mid-preload, silently dropping the
    // launch. The task records its outcome in the delegated-state map (`Loading`
    // → `Ready` / `Error{cause}`), which is what `status` reports for the row.
    ctx
      .supervisors
      .set_delegated_state(&model_name, ManagedState::Loading)
      .await;
    {
      let registry = ctx.supervisors.clone();
      let name = model_name.clone();
      let params = exec.params.clone();
      let umbrella = umbrella.clone();
      // Record `last_params` on preload success so a Lemonade model shows up in
      // the TUI's `↺ Recent` section like any other launch. Keyed on the
      // synthetic GGUF id (path = `lemonade://<name>`) because `last_params_list`
      // only emits `model_path` for the GGUF shape.
      let state = ctx.state.clone();
      let last_params_id = ModelIdentity::Gguf(exec.id.clone());
      let backend_tag = exec.resolved_backend_id.clone();
      tokio::spawn(async move {
        // `ensure_umbrella` returns at `Loading`; the load POST would race the
        // umbrella's bind and hit connection-refused on a cold start. Wait for
        // `/live` to pass (Ready) before talking to it.
        if let Err(cause) = umbrella.wait_until_ready(ready_timeout).await {
          let cause = format!("lemonade umbrella not ready: {cause}");
          log::warn!("lemonade: preload of `{name}` aborted: {cause}");
          registry
            .set_delegated_state(&name, ManagedState::Error { cause })
            .await;
          return;
        }
        let outcome = match LemonadeClient::new(serving_port) {
          Ok(client) => {
            let opts = lemonade_load_options(&client, &name, &params).await;
            client.load_with(&name, &opts).await
          }
          Err(e) => Err(e),
        };
        match outcome {
          Ok(()) => {
            registry
              .set_delegated_state(&name, ManagedState::Ready)
              .await;
            state
              .mutate(|s| {
                s.upsert_last_params(last_params_id.clone(), params.clone(), backend_tag.clone())
              })
              .await;
          }
          Err(e) => {
            log::warn!("lemonade: preload of `{name}` failed: {e}");
            registry
              .set_delegated_state(
                &name,
                ManagedState::Error {
                  cause: e.to_string(),
                },
              )
              .await;
          }
        }
      });
    }

    // Persist a running snapshot at the umbrella port for status visibility.
    let pid = umbrella.pid().await.unwrap_or(0) as i32;
    let started_at = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|d| d.as_secs())
      .unwrap_or_default();
    let identity = exec.identity.clone();
    let snapshot_params = exec.params.clone();
    let snapshot_backend = exec.resolved_backend_id.clone();
    let snapshot_launch_id = launch_id.clone();
    ctx
      .state
      .mutate(move |s| {
        s.running
          .retain(|r| !(r.id == identity && r.port == serving_port));
        s.running.push(RunningSnapshot {
          id: identity.clone(),
          pid,
          port: serving_port,
          started_at,
          launch_id: Some(snapshot_launch_id),
          params: snapshot_params,
          actuals: Default::default(),
          resolved_backend: snapshot_backend,
        });
      })
      .await;

    Ok(StartedLaunch {
      // The model's own `L#` (not the umbrella's id) — the handle the client
      // shows and later stops by.
      launch_id,
      model_id: exec.id,
      port: serving_port,
      model: umbrella,
      log_path: exec.log_path,
      // Lemonade's delegated path carries no admission advisories.
      warnings: Vec::new(),
    })
  }
}

/// Project the launch params onto lemond's load-option surface: `ctx` (when the
/// user set one) becomes `ctx_size`; non-empty extras become the recipe-scoped
/// args string (`llamacpp_args` / `whispercpp_args` / `flm_args` — lemond names
/// the field after the model's recipe, read from the umbrella's own model list).
/// Extras are dropped with a warning when the recipe can't be resolved — guessing
/// the field would silently feed flags to the wrong engine.
async fn lemonade_load_options(
  client: &LemonadeClient,
  name: &str,
  params: &LaunchParams,
) -> super::LoadOptions {
  let recipe_args = if params.extras.is_empty() {
    None
  } else {
    let joined = params
      .extras
      .iter()
      .map(|s| s.to_string_lossy())
      .collect::<Vec<_>>()
      .join(" ");
    let recipe = client
      .list_model_entries()
      .await
      .ok()
      .and_then(|entries| entries.into_iter().find(|e| e.id == name))
      .and_then(|e| e.recipe);
    match recipe {
      Some(recipe) => Some((format!("{recipe}_args"), joined)),
      None => {
        log::warn!(
          "lemonade: dropping extras for `{name}` — could not resolve its recipe \
           from the umbrella's model list"
        );
        None
      }
    }
  };
  super::LoadOptions {
    ctx_size: params.ctx,
    recipe_args,
  }
}

impl LemonadeBackend {
  /// Free a model from the shared umbrella via its API; the umbrella stays up and
  /// autoloads on the next request. Private to this backend — the delegated-unload
  /// is an internal step of [`Backend::stop`]'s delegated branch (and idle
  /// eviction, which routes through `stop`), not a concept the generic tree names.
  /// A missing umbrella means nothing is resident: a no-op success (the stop's row
  /// cleanup still runs).
  async fn unload_delegated(&self, ctx: &MethodContext, name: &str) -> Result<(), String> {
    let Some(umbrella) = ctx.supervisors.get(&umbrella_launch_id()).await else {
      return Ok(());
    };
    let client = LemonadeClient::new(umbrella.port()).map_err(|e| e.to_string())?;
    client.unload(name).await.map_err(|e| e.to_string())
  }

  /// Accelerators lemond actually has installed, from a live `system-info`
  /// probe (the HTTP form of `lemonade backends`): the distinct backend classes
  /// with at least one `state: "installed"` entry across recipes. `None` when
  /// the umbrella isn't registered or the probe fails, so the caller keeps the
  /// static capability floor.
  async fn installed_accelerators(&self, ctx: &MethodContext) -> Option<AcceleratorSupport> {
    if !self.available(ctx) {
      return None;
    }
    // The managed umbrella's live port when llamastash spawned it; otherwise the
    // configured port, so an externally-run lemond is probed too.
    let port = match ctx.supervisors.get(&umbrella_launch_id()).await {
      Some(umbrella) => umbrella.port(),
      None => ctx.backend.lemonade.port,
    };
    let client = LemonadeClient::new(port).ok()?;
    // Bounded independently of the client's own (minutes-long, model-load) timeout:
    // a slow/hung lemond must never stall a `status` response.
    let info = tokio::time::timeout(std::time::Duration::from_secs(2), client.system_info())
      .await
      .ok()?
      .ok()?;
    let acc = installed_accelerators_from_system_info(&info);
    (!acc.labels().is_empty()).then_some(acc)
  }
}

/// Pure parse of a lemond `system-info` body into the accelerator classes it
/// has installed: the distinct `recipes.*.backends.*` names whose `state` is
/// `"installed"`. Split out from the HTTP path so it's unit-testable against a
/// captured body.
fn installed_accelerators_from_system_info(info: &serde_json::Value) -> AcceleratorSupport {
  let mut acc = AcceleratorSupport::default();
  let Some(recipes) = info.get("recipes").and_then(|v| v.as_object()) else {
    return acc;
  };
  for recipe in recipes.values() {
    let Some(backends) = recipe.get("backends").and_then(|v| v.as_object()) else {
      continue;
    };
    for (name, body) in backends {
      if body.get("state").and_then(|v| v.as_str()) != Some("installed") {
        continue;
      }
      if let Some(a) = accelerator_from_label(name) {
        acc.insert(a);
      }
    }
  }
  acc
}

/// Map a lemond backend/recipe name (`"rocm"`, `"npu"`, `"vulkan"`, …) to an
/// accelerator class. Same label vocabulary as [`Accelerator::label`], so it
/// round-trips.
fn accelerator_from_label(name: &str) -> Option<Accelerator> {
  match name.to_ascii_lowercase().as_str() {
    "cpu" => Some(Accelerator::Cpu),
    "cuda" => Some(Accelerator::Cuda),
    "rocm" => Some(Accelerator::Rocm),
    "vulkan" => Some(Accelerator::Vulkan),
    "metal" => Some(Accelerator::Metal),
    "npu" => Some(Accelerator::Npu),
    _ => None,
  }
}

/// The managed umbrella's state for `status`, distinct from the `installed`
/// (binary-resolvable) signal. `disabled` when the backend is off; otherwise
/// reflects the supervised umbrella: `running` (Ready), `starting` (spawned,
/// probing), or `not running` (never came up / exited — commonly a boot-time
/// port conflict).
async fn umbrella_state(ctx: &MethodContext) -> &'static str {
  use crate::daemon::supervisor::ManagedState;
  if !LemonadeBackend::new().available(ctx) {
    return "disabled";
  }
  match ctx.supervisors.get(&umbrella_launch_id()).await {
    Some(m) => match m.state().await {
      ManagedState::Ready => "running",
      ManagedState::Launching | ManagedState::Loading => "starting",
      ManagedState::Error { .. } | ManagedState::Stopping | ManagedState::Stopped => "not running",
    },
    None => "not running",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::flag_aliases::knob_specs;
  use crate::launch::mode::LaunchMode;

  fn manager_of(plan: LaunchPlan) -> ManagerLaunchSpec {
    match plan {
      LaunchPlan::DelegateToManager(spec) => spec,
      LaunchPlan::SpawnProcess(_) => panic!("Lemonade must produce a DelegateToManager plan"),
    }
  }

  #[test]
  fn id_and_lifecycle() {
    let b = LemonadeBackend::new();
    assert_eq!(b.id(), "lemonade");
    assert_eq!(b.lifecycle(), Lifecycle::ManagedMultiplexer);
  }

  #[test]
  fn registry_name_round_trips_through_synthetic_scheme() {
    // A synthetic catalog path strips back to the bare registry name the
    // umbrella API knows; a non-scheme path is not a Lemonade model.
    assert_eq!(
      registry_name_from_path(Path::new("lemonade://Whisper-Tiny")),
      Some("Whisper-Tiny")
    );
    assert_eq!(
      registry_name_from_path(Path::new("lemonade://qwen3.5-4b-FLM")),
      Some("qwen3.5-4b-FLM")
    );
    assert_eq!(registry_name_from_path(Path::new("/models/x.gguf")), None);
    // `registry_name` (the umbrella's model ref) drops the scheme but passes a
    // bare name through verbatim (GGUF + explicit `--backend lemonade`).
    assert_eq!(
      LemonadeBackend::registry_name(Path::new("lemonade://Whisper-Tiny")),
      "Whisper-Tiny"
    );
    assert_eq!(
      LemonadeBackend::registry_name(Path::new("Qwen2.5-7B")),
      "Qwen2.5-7B"
    );
  }

  #[test]
  fn umbrella_port_available_reflects_bind_state() {
    // A port we hold is reported unavailable; once released, available.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
    let port = held.local_addr().unwrap().port();
    assert!(
      !umbrella_port_available(port),
      "port {port} is held, should read unavailable"
    );
    drop(held);
    assert!(
      umbrella_port_available(port),
      "port {port} was released, should read available"
    );
  }

  #[test]
  fn umbrella_port_state_distinguishes_listener_from_free() {
    // A live listener reads `Listening` (fail-fast case); a released
    // port reads `Free`. `Remnants` needs orphaned FIN-WAIT-2 sockets
    // the kernel owns — not fabricable deterministically in a unit
    // test, so its mapping (bind fails + connect refused) is covered
    // by the match arms above and the live restart flow.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
    let port = held.local_addr().unwrap().port();
    assert_eq!(umbrella_port_state(port), UmbrellaPortState::Listening);
    drop(held);
    assert_eq!(umbrella_port_state(port), UmbrellaPortState::Free);
  }

  #[test]
  fn capabilities_cover_exactly_ctx() {
    // `ctx` maps onto lemond's `ctx_size` load option; everything else in
    // the typed-knob IR is llama.cpp-process vocabulary lemond either owns
    // itself (offload) or doesn't expose. Extras ride a separate channel
    // (`*_args`), not the knob IR.
    let b = LemonadeBackend::new();
    for spec in knob_specs() {
      let expected = spec.field == crate::launch::flag_aliases::KnobField::Ctx;
      assert_eq!(
        b.capabilities().supports(spec.field),
        expected,
        "lemonade capability for {:?}",
        spec.field
      );
    }
  }

  #[test]
  fn prepare_launch_yields_delegate_with_umbrella_and_model() {
    let b = LemonadeBackend::new();
    let p = LaunchParams::new(PathBuf::from("Qwen2.5-7B-Instruct-GGUF"), LaunchMode::Chat);
    let spec = manager_of(b.prepare_launch(
      &p,
      41100,
      PathBuf::from("/opt/lemonade/lemond"),
      ProbeOptions::default(),
    ));
    // Umbrella binds the loopback port we assigned.
    assert_eq!(spec.umbrella.binary, PathBuf::from("/opt/lemonade/lemond"));
    let argv: Vec<String> = spec
      .umbrella
      .argv
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    // No positional cache_dir: lemond's own default (`~/.cache/lemonade`)
    // is shared with manual runs; the binary's parent may be read-only
    // (`/usr/bin` on a system install).
    assert_eq!(argv, vec!["--host", "127.0.0.1", "--port", "41100"]);
    // Readiness is the umbrella liveness endpoint, not /health.
    assert_eq!(
      spec.umbrella.readiness,
      Readiness::HttpPoll {
        path: "/live".to_string(),
        ready_status: 200,
      }
    );
    // The model to serve is named for the umbrella's API.
    assert_eq!(spec.model.name, "Qwen2.5-7B-Instruct-GGUF");
  }

  #[test]
  fn identify_returns_backend_identity() {
    let b = LemonadeBackend::new();
    let id = b.identify(Path::new("Llama-3.1-8B"), b"");
    let backend = id.as_backend().expect("lemonade identity is BackendModel");
    assert_eq!(backend.backend, "lemonade");
    assert_eq!(backend.name, "Llama-3.1-8B");
    assert!(id.as_gguf().is_none());
  }

  #[test]
  fn umbrella_process_spec_matches_manager_spec_umbrella() {
    // Boot supervision and the per-model start path must build an identical
    // umbrella spec so `ensure_umbrella` treats them as the one umbrella.
    let b = LemonadeBackend::new();
    let p = LaunchParams::new(PathBuf::from("ignored-for-umbrella"), LaunchMode::Chat);
    let binary = PathBuf::from("/opt/lemonade/lemond");
    let via_manager = b
      .manager_spec(&p, 13305, binary.clone(), ProbeOptions::default())
      .umbrella;
    let direct = umbrella_process_spec(13305, binary, ProbeOptions::default());
    assert_eq!(via_manager.binary, direct.binary);
    assert_eq!(via_manager.argv, direct.argv);
    assert_eq!(via_manager.readiness, direct.readiness);
  }

  #[test]
  fn resolve_binary_prefers_explicit_path_then_path_lookup() {
    use crate::config::LemonadeConfig;

    // Explicit binary that exists resolves to its canonical path.
    let this_exe = std::env::current_exe().expect("current exe");
    let cfg = LemonadeConfig {
      enabled: Some(true),
      servers: vec![crate::backend::ServerConfig {
        binary: this_exe.clone(),
        name: None,
      }],
      port: 13305,
    };
    let expected = this_exe.canonicalize().unwrap_or(this_exe);
    assert_eq!(resolve_lemond_binary(&cfg), Some(expected));

    // Explicit binary that does NOT exist resolves to None (we never fall
    // back to PATH when the user named a specific file).
    let cfg_missing = LemonadeConfig {
      enabled: Some(true),
      servers: vec![crate::backend::ServerConfig {
        binary: PathBuf::from("/nonexistent/lemond-xyz"),
        name: None,
      }],
      port: 13305,
    };
    assert_eq!(resolve_lemond_binary(&cfg_missing), None);
  }

  #[test]
  fn installed_accelerators_parses_lemond_system_info() {
    use serde_json::json;
    // Shape mirrors the real `/api/v1/system-info` `recipes` block: only the
    // `state: "installed"` backends count (flm/npu, whispercpp/rocm+vulkan);
    // `installable` / `unsupported` are ignored. Order is by accelerator class.
    let info = json!({
      "recipes": {
        "flm": { "backends": { "npu": { "state": "installed", "version": "v0.9.44" } } },
        "whispercpp": { "backends": {
          "cpu":    { "state": "installable" },
          "rocm":   { "state": "installed", "version": "v1.8.4" },
          "vulkan": { "state": "installed", "version": "v1.8.4" }
        } },
        "llamacpp": { "backends": {
          "cpu":  { "state": "installable" },
          "cuda": { "state": "unsupported" },
          "rocm": { "state": "installable" }
        } }
      }
    });
    let acc = installed_accelerators_from_system_info(&info);
    assert_eq!(acc.labels(), vec!["rocm", "vulkan", "npu"]);
    // A body with no recipes yields an empty set (caller falls back to static).
    assert!(installed_accelerators_from_system_info(&json!({}))
      .labels()
      .is_empty());
  }

  #[tokio::test]
  async fn umbrella_state_is_disabled_when_backend_off() {
    // An explicit `enabled: false` forces Lemonade off (the default is
    // on-when-found), so the umbrella state short-circuits to "disabled" without
    // touching the supervisor registry — regardless of whether a `lemond`
    // happens to sit on PATH.
    use crate::backend::BackendConfig;
    use crate::daemon::context::MethodContext;
    use crate::daemon::shutdown::ShutdownToken;
    let c = MethodContext::new(ShutdownToken::new()).with_backend(
      BackendConfig {
        lemonade: crate::config::LemonadeConfig {
          enabled: Some(false),
          ..Default::default()
        },
        ..Default::default()
      },
      std::collections::BTreeMap::new(),
    );
    assert_eq!(super::umbrella_state(&c).await, "disabled");
  }
}
