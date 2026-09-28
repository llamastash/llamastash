//! Managed-multiplexer launch orchestration for Lemonade.
//!
//! llama.cpp spawns one supervised child *per model*. Lemonade is different:
//! one long-lived `lemond` umbrella serves every model behind its API
//! This module owns the umbrella's lifecycle — ensuring exactly one
//! `lemond` is supervised and ready before any per-model work is delegated
//! to it.
//!
//! The umbrella reuses the **generic** [`crate::daemon::supervisor`]: it is
//! just a [`ProcessLaunchSpec`] whose
//! readiness is `lemond`'s `/live` endpoint. Because the supervisor keys a
//! [`ManagedModel`] by a [`crate::gguf::identity::ModelId`], the umbrella —
//! which is not a GGUF model — is registered under a synthetic id (path =
//! the `lemond` binary, sentinel header hash) and a reserved
//! [`umbrella_launch_id`].
//!
//! The umbrella is brought up from two callers, both via [`ensure_umbrella`]:
//! the daemon's boot-time supervision (when the Lemonade backend is enabled)
//! and the per-model `start_model` path. Per-model routing reads the catalog
//! `backend` tag and forwards through the proxy to the umbrella's port.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::backend::ProcessLaunchSpec;
use crate::config::LemonadeConfig;
use crate::daemon::probe::ProbeOptions;
use crate::daemon::registry::{LaunchId, SupervisorRegistry};
use crate::daemon::supervisor::{
  spawn, LaunchOrigin, ManagedModel, ManagedSpawn, ManagedState, SpawnError,
};
use crate::gguf::identity::ModelId;
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

/// Process-wide single-flight guard for [`ensure_umbrella`]. Serializes the
/// existence-check + spawn so two concurrent first-callers (e.g. boot
/// supervision racing an early `start_model`) can't both spawn an umbrella.
static ENSURE_LOCK: Mutex<()> = Mutex::const_new(());

/// How long a respawn waits for an exited umbrella to release its port.
const EXITED_PORT_WAIT: Duration = Duration::from_secs(5);

/// Reserved supervisor id for the single `lemond` umbrella. One umbrella
/// per daemon, shared by all Lemonade-backed models.
pub fn umbrella_launch_id() -> LaunchId {
  LaunchId("lemonade-umbrella".to_string())
}

/// Synthetic [`ModelId`] for the umbrella. It is not a GGUF model, but the
/// supervisor keys every [`ManagedModel`] by `ModelId`; using the `lemond`
/// binary path keeps status output legible, and a fixed sentinel header
/// hash distinguishes it from any real model.
fn umbrella_model_id(binary: &Path) -> ModelId {
  ModelId {
    path: binary.to_path_buf(),
    header_blake3: [0u8; 32],
  }
}

/// Ensure the `lemond` umbrella is supervised and ready, returning its
/// [`ManagedModel`] handle. **Idempotent**: if a live umbrella is already
/// registered under [`umbrella_launch_id`], it is reused and `port` +
/// `umbrella` are ignored (one umbrella per daemon). A registered umbrella
/// that has exited (`Error` / `Stopped`) is dropped and respawned, so a crashed
/// `lemond` doesn't fail every later start until the daemon restarts.
///
/// `port` is the loopback port the umbrella's [`ProcessLaunchSpec`] was
/// built to bind (the supervisor probes it for `/live` readiness). On a
/// fresh spawn the supervisor blocks until the umbrella reports ready or
/// the probe times out (surfaced as [`SpawnError`]).
///
/// The existence-check + spawn run under a process-wide lock so concurrent
/// first-callers serialize: the first spawns, the rest observe the registered
/// umbrella and reuse it (single-flight).
pub async fn ensure_umbrella(
  registry: &SupervisorRegistry,
  port: u16,
  umbrella: ProcessLaunchSpec,
  log_path: PathBuf,
) -> Result<ManagedModel, SpawnError> {
  let _guard = ENSURE_LOCK.lock().await;
  let id = umbrella_launch_id();
  if let Some(existing) = registry.get(&id).await {
    match existing.state().await {
      ManagedState::Error { .. } | ManagedState::Stopped => {
        registry.remove(&id).await;
        // A probe timeout reports `Error` right after SIGKILL, before the
        // process has exited and released the port.
        let deadline = std::time::Instant::now() + EXITED_PORT_WAIT;
        while !super::umbrella_port_available(port) && std::time::Instant::now() < deadline {
          tokio::time::sleep(Duration::from_millis(100)).await;
        }
      }
      _ => return Ok(existing),
    }
  }
  // Not yet supervised, so we must bind `port` ourselves to start the umbrella.
  // If another process already holds it (a hand-started `lemond`, a stale
  // instance), our spawned child would lose the bind race while the foreign
  // process keeps answering the `/live` probe — a false "ready". Refuse with a
  // clear error instead of adopting a process we don't own. Checked *after* the
  // reuse path (under the single-flight lock) so our own already-running
  // umbrella, which legitimately holds the port, is never rejected.
  if !super::umbrella_port_available(port) {
    return Err(SpawnError::PortInUse(port));
  }
  let model = spawn(ManagedSpawn {
    id: umbrella_model_id(&umbrella.binary),
    params: LaunchParams::new(umbrella.binary.clone(), LaunchMode::Chat),
    port,
    mode: LaunchMode::Chat,
    log_path,
    plan: umbrella,
    origin: LaunchOrigin::Manual,
    // The Lemonade umbrella has no `--fit` ctx semantics; the strict-fit
    // gate never applies to it (so `fetch_actuals` is never called for it).
    fit_gate: None,
    resolved_backend: super::LEMONADE_BACKEND_ID.to_string(),
  })
  .await?;
  registry.insert(id, model.clone()).await;
  Ok(model)
}

/// Bring the shared `lemond` umbrella up at daemon boot so discovery (which
/// probes its port) and proxy routing (which forwards to it) both work before
/// the user issues an explicit `start`. The caller has already confirmed the
/// backend is enabled (`Backend::available`); this resolves the binary, builds
/// the umbrella spec, and spawns a detached supervision task — boot must not
/// block on `/live` readiness (the detached-start parent only waits a few
/// seconds for `runtime.json`). A missing `lemond` binary is a clean warning,
/// never a daemon-start failure — llamastash never installs it. The task is
/// idempotent with the per-model `start_model` path (both go through
/// [`ensure_umbrella`]), so at most one umbrella exists per daemon.
pub fn supervise_umbrella_at_boot(
  registry: SupervisorRegistry,
  cfg: &LemonadeConfig,
  log_dir: &Path,
  probe_timeout: Option<Duration>,
) {
  let Some(binary) = super::resolve_lemond_binary(cfg) else {
    // Unreachable when the caller gated on `available()` (which requires the
    // binary to resolve); kept as a belt-and-suspenders warning for a TOCTOU
    // where the binary vanished between the two resolves.
    log::warn!(
      "lemonade enabled but no `lemond` binary found (set `backend.lemonade.servers` or put `lemond` on PATH); skipping umbrella supervision"
    );
    return;
  };
  let port = cfg.port;
  if let Err(e) = std::fs::create_dir_all(log_dir) {
    log::warn!(
      "could not create log dir {}: {e}; lemonade umbrella logs may fail to open",
      log_dir.display()
    );
  }
  let probe = match probe_timeout {
    Some(timeout) => ProbeOptions {
      timeout,
      ..ProbeOptions::default()
    },
    None => ProbeOptions::default(),
  };
  let umbrella = super::umbrella_process_spec(port, binary, probe);
  let log_path = log_dir.join("lemonade-umbrella.log");
  // `ensure_umbrella` refuses to adopt a foreign process already holding the
  // port (returns `PortInUse`) rather than logging a false "supervised" and
  // 503-ing opaquely at routing time. Surface that case plainly. Not fatal:
  // the daemon (and llama.cpp routing) stay up; only Lemonade routing is
  // unavailable until the conflict is resolved.
  //
  // A port held only by teardown remnants (FIN-WAIT-2 / TIME-WAIT leftovers of
  // a just-stopped daemon's `lemond`) is waited out first: the kernel clears
  // them within its fin-timeout (~60 s), so a quick `daemon stop && daemon
  // start --lemonade` brings the umbrella up as soon as the port frees instead
  // of failing.
  tokio::spawn(async move {
    use super::{umbrella_port_state, UmbrellaPortState};
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut waiting_logged = false;
    loop {
      if umbrella_port_state(port) == UmbrellaPortState::Free {
        match ensure_umbrella(&registry, port, umbrella.clone(), log_path.clone()).await {
          Ok(_) => {
            log::info!("lemonade umbrella supervised on 127.0.0.1:{port}");
            return;
          }
          // Lost a probe-to-spawn race (e.g. the previous daemon's dying
          // `lemond` flickering through teardown) — retry within the window
          // like any other transient holder.
          Err(SpawnError::PortInUse(_)) => {}
          Err(e) => {
            log::warn!("lemonade umbrella failed to start at boot: {e}");
            return;
          }
        }
      }
      // Held — by a still-exiting previous umbrella (Listening, drops within
      // its SIGTERM→SIGKILL grace) or by kernel teardown remnants (FIN-WAIT-2 /
      // TIME-WAIT, clear within the fin-timeout). Both resolve on their own; a
      // genuinely foreign holder is normally caught by `daemon start`'s
      // precheck before this task ever runs, so only after the window do we
      // call it foreign and give up.
      if std::time::Instant::now() >= deadline {
        log::error!(
          "lemonade: 127.0.0.1:{port} is already in use — llamastash could not start its own \
           managed `lemond`. Stop whatever holds that port (e.g. a manually started `lemond`) or \
           set `backend.lemonade.port`; Lemonade model routing will return 503 until this is resolved."
        );
        return;
      }
      if !waiting_logged {
        log::info!(
          "lemonade: 127.0.0.1:{port} is still held (previous umbrella exiting, or its sockets \
           draining); retrying for up to 90 s…"
        );
        waiting_logged = true;
      }
      tokio::time::sleep(Duration::from_secs(2)).await;
    }
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn umbrella_id_is_stable_and_reserved() {
    assert_eq!(umbrella_launch_id().as_str(), "lemonade-umbrella");
  }

  #[test]
  fn umbrella_model_id_uses_binary_path_and_sentinel_hash() {
    let id = umbrella_model_id(Path::new("/opt/lemonade/lemond"));
    assert_eq!(id.path, PathBuf::from("/opt/lemonade/lemond"));
    assert_eq!(id.header_blake3, [0u8; 32]);
  }
}
