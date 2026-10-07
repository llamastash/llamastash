//! Idle-TTL eviction sweeper + make-room unloading for
//! proxy-auto-started supervisors.
//!
//! Two policies over the same set of "which launches may be given back".
//!
//! **The sweep.** A background task alongside the proxy listener. Every tick
//! (~30 s, clamped against the configured TTL so very short TTLs sweep more
//! often) walks the supervisor snapshot and stops a `Ready` supervisor when all
//! of these hold:
//!
//! - `origin == LaunchOrigin::AutoStart` — manually-started models are durable
//!   user intent, mirroring LM Studio's exemption.
//! - `inflight == 0` — the refcount gate. A model with active in-flight requests
//!   stays resident even if its last `touch` is stale, so a long generation is
//!   never SIGTERM'd mid-stream.
//! - `now - last_request_at >= ttl` — the last-touch deadline, where `ttl` is the
//!   launch's own preset override when it pins one and `proxy.idle_ttl_secs`
//!   otherwise.
//!
//! The stop goes through the backend's own `stop` (5 s grace), the same path
//! `stop_model` uses, so the supervisor and its `state.running` row are dropped
//! with it.
//!
//! **Make-room.** When admission refuses a proxy auto-start, idle auto-started
//! launches are unloaded least-recently-used first until the refused demand fits,
//! instead of answering 503. See [`make_room`].
//!
//! A preset's `idle_ttl_secs` is read off the live preset store on every pass,
//! so a `presets save --idle-ttl` moves a running launch's deadline without a
//! relaunch (a hand edit to `config.yaml` still needs a daemon restart, like
//! every other hand edit). `0` there means never unload that launch.
//!
//! A global `proxy.idle_ttl_secs = 0` disables the sweep unless some
//! preset pins its own TTL; per-launch `0` always means "never unload".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::daemon::registry::LaunchId;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::RunningSnapshot;
use crate::daemon::supervisor::{LaunchOrigin, ManagedModel, ManagedState};
use crate::proxy::ProxyState;

/// SIGTERM grace given to evicted supervisors. Llama-server is well-
/// behaved on SIGTERM (flushes the HTTP server then exits) so 5 s
/// is plenty; if it ignores SIGTERM the supervisor escalates to
/// SIGKILL itself.
const EVICT_STOP_GRACE: Duration = Duration::from_secs(5);

/// How long make-room waits for a stopped launch's memory to show up as
/// free before re-running admission anyway, at the default 1 s sampler tick.
/// A slower tick stretches it: see [`room_wait_window`].
const MAKE_ROOM_FREE_WAIT: Duration = Duration::from_secs(20);

/// Run the eviction loop until the shutdown token fires. Sleeps for
/// `cadence` between sweeps. Per-sweep work is bounded by the size
/// of the supervisor snapshot; on a typical daemon (<20 active
/// launches) one sweep is microseconds of CPU.
///
/// `ttl` is the global `proxy.idle_ttl_secs` default; a launch whose
/// preset pins `idle_ttl_secs` overrides it per launch (see
/// `launch_ttls`). A `0` global means "no global deadline" — the loop
/// still runs so a preset-pinned TTL applies, and a launch with no
/// override is simply never picked.
pub async fn run(state: Arc<ProxyState>, ttl: Duration, shutdown: ShutdownToken) {
  let cadence = sweep_cadence(ttl);
  log::info!(
    "proxy eviction sweeper armed: ttl={}, cadence={:?}",
    if ttl.is_zero() {
      "off (preset TTLs still apply)".to_string()
    } else {
      format!("{ttl:?}")
    },
    cadence,
  );
  loop {
    tokio::select! {
      _ = shutdown.wait_until_triggered() => {
        log::debug!("proxy eviction sweeper: shutdown signalled");
        return;
      }
      _ = tokio::time::sleep(cadence) => {}
    }
    sweep_once(&state, ttl).await;
  }
}

/// Sweep cadence: tick at least every 30 s, but never longer than
/// the TTL itself (a 5 s TTL with a 30 s cadence would let idle
/// supervisors linger up to 35 s). Floor at 5 s so a 1 s TTL doesn't
/// turn the daemon into a stop_model storm. A 0 global TTL ("no global
/// deadline, preset TTLs only") takes the slowest cadence.
fn sweep_cadence(ttl: Duration) -> Duration {
  const MIN: Duration = Duration::from_secs(5);
  const MAX: Duration = Duration::from_secs(30);
  if ttl.is_zero() {
    return MAX;
  }
  ttl.min(MAX).max(MIN)
}

/// Pure per-row decision. Keeps `sweep_once` a thin orchestrator
/// and lets unit tests cover every branch without spinning up real
/// supervisors. `last_request_at = None` means "no MRU stamp yet";
/// the sweeper treats that as `Skip` because `auto_start` is
/// supposed to touch the MRU when the supervisor reaches Ready, so a
/// missing stamp signals either a race or a test fixture where the
/// eviction predicate shouldn't fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepDecision {
  Skip,
  Evict,
}

pub(crate) fn decide(
  origin: LaunchOrigin,
  state: &ManagedState,
  inflight: u64,
  idle_for: Option<Duration>,
  ttl: Duration,
) -> SweepDecision {
  if origin != LaunchOrigin::AutoStart {
    return SweepDecision::Skip;
  }
  if !matches!(state, ManagedState::Ready) {
    return SweepDecision::Skip;
  }
  if inflight > 0 {
    return SweepDecision::Skip;
  }
  match idle_for {
    Some(elapsed) if elapsed >= ttl => SweepDecision::Evict,
    _ => SweepDecision::Skip,
  }
}

/// One sweep pass. Public for integration tests; production use
/// comes via [`run`].
///
/// `default_ttl` is the global `proxy.idle_ttl_secs`; a launch whose preset
/// pins `idle_ttl_secs` uses that instead, and `0` on a launch means "never
/// unload", so that one row is skipped while its neighbours still sweep.
///
/// Each stop is dispatched via `tokio::spawn` so a sweep with N eligible
/// rows doesn't serialise into `N × grace` seconds of cadence drift.
pub async fn sweep_once(state: &Arc<ProxyState>, default_ttl: Duration) {
  let ttls = launch_ttls(state).await;
  let snap = state.ctx.supervisors.snapshot().await;
  for (launch_id, model) in snap {
    let ttl = ttls.get(&launch_id).copied().unwrap_or(default_ttl);
    // An infrastructure launch (a managed-multiplexer umbrella) gets
    // lifecycle-aware eviction: never SIGTERM the shared process — free its
    // idle loaded model via the backend's unload API instead (the umbrella
    // stays Ready and autoloads on the next request). This is the `model.stop`
    // vs API-unload branch.
    if crate::backend::umbrella_owner(&launch_id).is_some() {
      // The umbrella's own TTL is not consulted: idleness is shared inside the
      // multiplexer, but which rows are due is decided per row, and a global TTL
      // of `0` ("presets only") must not blind the pass to a row that pins one.
      let targets = umbrella_idle_rows(state, &ttls, default_ttl, &model).await;
      if targets.is_empty() {
        continue;
      }
      let ctx = state.ctx.clone();
      tokio::spawn(async move {
        for target in &targets {
          log::info!(
            "proxy eviction: unloading idle {} (umbrella stays up)",
            target.as_str()
          );
          stop_launch(&ctx, target).await;
        }
      });
      continue;
    }
    if ttl.is_zero() {
      continue;
    }
    let current_state = model.state().await;
    let idle_for = state
      .mru
      .last_request_at(model.id())
      .await
      .map(|t| t.elapsed());
    if decide(
      model.origin(),
      &current_state,
      model.inflight(),
      idle_for,
      ttl,
    ) != SweepDecision::Evict
    {
      continue;
    }
    log::info!(
      "proxy eviction: stopping {launch_id} ({served}) — idle {idle:?} >= ttl {ttl:?}",
      launch_id = launch_id.as_str(),
      served = model.params().model_path.display(),
      idle = idle_for,
    );
    let ctx = state.ctx.clone();
    tokio::spawn(async move {
      // The snapshot is a moment old by the time this runs: a request that
      // landed in that window must not be cut off mid-generation.
      if !matches!(model.state().await, ManagedState::Ready) || model.inflight() > 0 {
        log::debug!(
          "proxy eviction: {} busy again — left running",
          launch_id.as_str()
        );
        return;
      }
      // A bare `model.stop` left the row in `state.running`, where it kept
      // holding the launch name and refused the next `<model>@<name>`.
      stop_launch(&ctx, &launch_id).await;
    });
  }
}

/// Stop one launch through its own backend — the path `stop_model` uses, so the
/// supervisor and its `state.running` row go with it and a backend that
/// overrides `stop` is dispatched rather than defaulted. For a delegated
/// (multiplexer) row that `stop` is the umbrella-unload call, which is why the
/// sweep and make-room need only one stop helper.
async fn stop_launch(ctx: &crate::daemon::context::MethodContext, launch_id: &LaunchId) {
  use crate::backend::Backend;
  let backend = crate::daemon::launch_service::backend_for_launch(ctx, launch_id).await;
  // The backend's last look at the live child. A backend that can carry
  // conversation state across a stop writes it out here, which is what lets the
  // next launch of this model skip reprocessing. Best-effort: a slow or failing
  // save must not hold up an eviction that a request is waiting on.
  let live = ctx
    .state
    .snapshot()
    .await
    .running
    .into_iter()
    .find(|r| r.launch_id.as_ref() == Some(launch_id))
    .map(|r| (r.port, r.params));
  if let Some((port, params)) = live {
    backend.on_evict(port, &params).await;
  }
  let _ = backend
    .stop(ctx, launch_id, EVICT_STOP_GRACE.as_secs())
    .await;
}

/// The rows living inside a managed multiplexer: a delegated launch shares the
/// umbrella's port and carries a delegated backend id.
fn delegated_rows(
  running: &[crate::daemon::state_store::RunningSnapshot],
  port: u16,
) -> impl Iterator<Item = &crate::daemon::state_store::RunningSnapshot> {
  running
    .iter()
    .filter(move |r| r.port == port && r.delegated_backend_id().is_some())
}

/// Whether a multiplexer row may be given back. A delegated row has no
/// supervisor of its own, so its origin and its preset's `idle_ttl_secs` are read
/// off the row: a manually started or preloaded model is durable user intent, and
/// an adopted row (no origin) is never a candidate.
fn row_evictable(row: &RunningSnapshot, ttls: &HashMap<LaunchId, Duration>) -> bool {
  if !matches!(row.origin, Some(LaunchOrigin::AutoStart)) {
    return false;
  }
  !row
    .launch_id
    .as_ref()
    .is_some_and(|id| ttls.get(id).is_some_and(Duration::is_zero))
}

/// The rows inside `umbrella` that are due to be freed: the umbrella is `Ready`
/// and quiet, and each row is auto-started, not pinned to `idle_ttl_secs: 0`, and
/// idle for at least its own TTL.
///
/// Idleness is umbrella-granular — every delegated request touches the umbrella's
/// inflight guard and MRU stamp — but the TTL and the origin exemptions are the
/// row's own, so one never-unload model does not hold its neighbours resident and
/// a manual launch inside a multiplexer is never given up.
async fn umbrella_idle_rows(
  state: &Arc<ProxyState>,
  ttls: &HashMap<LaunchId, Duration>,
  default_ttl: Duration,
  umbrella: &ManagedModel,
) -> Vec<LaunchId> {
  if !matches!(umbrella.state().await, ManagedState::Ready) || umbrella.inflight() > 0 {
    return Vec::new();
  }
  let Some(touched) = state.mru.last_request_at(umbrella.id()).await else {
    return Vec::new();
  };
  let idle = touched.elapsed();
  let snapshot = state.ctx.state.snapshot().await;
  delegated_rows(&snapshot.running, umbrella.port())
    .filter(|row| row_evictable(row, ttls))
    .filter(|row| {
      let row_ttl = row
        .launch_id
        .as_ref()
        .and_then(|id| ttls.get(id))
        .copied()
        .unwrap_or(default_ttl);
      // `0` is never-unload for that row, and with the global TTL at `0` a row
      // that pins nothing is `0` too: it waits for a preset, not for nothing.
      !row_ttl.is_zero() && idle >= row_ttl
    })
    .filter_map(|row| row.launch_id.clone())
    .collect()
}

/// The idle-TTL each running launch is held to, keyed by launch id — its
/// preset's `idle_ttl_secs` when that preset pins one.
///
/// Resolved from the live preset store on every call rather than frozen at
/// launch, so `presets save --idle-ttl` moves a running launch's deadline without
/// a relaunch. A launch with no preset, or a preset that pins no TTL, is absent
/// from the table and keeps `proxy.idle_ttl_secs`.
pub(crate) async fn launch_ttls(state: &Arc<ProxyState>) -> HashMap<LaunchId, Duration> {
  let mut out = HashMap::new();
  let store = state.ctx.presets.snapshot().await;
  if store.is_empty() {
    return out;
  }
  let snapshot = state.ctx.state.snapshot().await;
  let rows = crate::ipc::methods::catalog_rows(&state.ctx).await;
  // `effective_presets` re-merges the whole store, so do it once per distinct
  // model path rather than once per launch of it.
  let mut by_path: HashMap<String, crate::launch::presets::EffectivePresets> = HashMap::new();
  for row in &snapshot.running {
    let (Some(launch_id), Some(preset)) = (row.launch_id.as_ref(), row.preset.as_deref()) else {
      continue;
    };
    let path = row.params.model_path.display().to_string();
    let eff = by_path.entry(path.clone()).or_insert_with(|| {
      let arch = rows
        .iter()
        .find(|r| r.path == path)
        .and_then(|r| r.arch.as_deref());
      crate::launch::presets::effective_presets(
        &crate::util::paths::model_file_label(&row.params.model_path),
        &path,
        arch,
        &store,
        &rows,
      )
    });
    if let Some(secs) = eff.named(preset).and_then(|p| p.idle_ttl_secs) {
      out.insert(launch_id.clone(), Duration::from_secs(secs));
    }
  }
  out
}

/// One unload make-room may buy with, and what it is worth in bytes.
struct RoomCandidate {
  /// Bytes this unload is credited for freeing — the demand each row was
  /// admitted at, the same projection the refused launch is priced with. For a
  /// managed multiplexer it is the sum over the rows given up.
  bytes: u64,
  last_request_at: Option<Instant>,
  /// The supervisor whose idle gates this candidate: the launch itself, or the
  /// umbrella whose rows are unloaded. Re-checked immediately before the stop,
  /// because selection and stopping are separated by up to the stop grace.
  guard: ManagedModel,
  /// The launches to stop: one for a process launch, or the rows resident inside
  /// a multiplexer (the shared process itself is never stopped).
  targets: Vec<LaunchId>,
}

/// Unload idle launches so a refused auto-start can fit, least-recently-used
/// first. Public for integration tests; production use comes via
/// `crate::proxy::launch`'s auto-start retry.
///
/// Returns `true` when something was freed, so the caller should retry the
/// launch — admission runs again on that retry and stays the authority.
/// All-or-nothing: when every eligible launch together cannot cover the
/// shortfall, nothing is stopped and this returns `false`, because unloading
/// models for a launch that still will not fit is a pure loss.
///
/// Eligible: `Ready`, zero in-flight, `LaunchOrigin::AutoStart` (manual and
/// preloaded launches are durable user intent, the same exemption the sweep
/// applies) and not pinned to `idle_ttl_secs: 0`. Inside a managed multiplexer
/// those two gates are read off each resident row, because a delegated row has
/// no supervisor of its own. Credit and refusal are one figure: each row is
/// worth the demand the gate admitted it at, or `launch_resident_bytes` when
/// it carries no stamp.
pub async fn make_room(
  state: &Arc<ProxyState>,
  refusal: &crate::launch::admission::Refusal,
) -> bool {
  let short = refusal
    .demand_bytes
    .saturating_sub(refusal.available_bytes());
  if short == 0 {
    return false;
  }
  let ttls = launch_ttls(state).await;
  let candidates = room_candidates(state, &ttls).await;
  let mut picked: Vec<RoomCandidate> = Vec::new();
  let mut freed = 0u64;
  for candidate in candidates {
    freed = freed.saturating_add(candidate.bytes);
    picked.push(candidate);
    if freed >= short {
      break;
    }
  }
  if freed < short {
    log::info!(
      "proxy make-room: {} more needed, only {} freeable from {} idle launch(es) — refusing",
      crate::launch::admission::human_gib(short),
      crate::launch::admission::human_gib(freed),
      picked.len(),
    );
    return false;
  }
  log::info!(
    "proxy make-room: unloading {} idle launch(es) ({} freeable) to fit a {} launch",
    picked.len(),
    crate::launch::admission::human_gib(freed),
    crate::launch::admission::human_gib(refusal.demand_bytes),
  );
  // Every pick is re-checked before any stop starts, and all-or-nothing is
  // decided again on what survives: giving up the whole plan is better than
  // unloading half of it for a launch that still will not fit.
  let mut go: Vec<RoomCandidate> = Vec::new();
  let mut freed_now = 0u64;
  for candidate in picked {
    if !matches!(candidate.guard.state().await, ManagedState::Ready)
      || candidate.guard.inflight() > 0
    {
      log::info!(
        "proxy make-room: {} is busy again — left it running",
        candidate.targets[0].as_str(),
      );
      continue;
    }
    freed_now = freed_now.saturating_add(candidate.bytes);
    go.push(candidate);
  }
  if freed_now < short {
    log::info!(
      "proxy make-room: {} needed but only {} still freeable after re-check — nothing unloaded",
      crate::launch::admission::human_gib(short),
      crate::launch::admission::human_gib(freed_now),
    );
    return false;
  }
  let ctx = &state.ctx;
  let unloaded: usize = go.iter().map(|c| c.targets.len()).sum();
  // Candidates stop together, the way the sweep unloads: the re-check above is
  // only honest if nothing else runs before the stops, and stopping one after
  // another would stretch the request's wait to N x the stop grace. The models
  // inside one multiplexer still go one at a time, as the sweep sends them.
  futures::future::join_all(go.iter().map(|candidate| async move {
    for target in &candidate.targets {
      stop_launch(ctx, target).await;
    }
  }))
  .await;
  log::info!(
    "proxy make-room: unloaded {} launch(es) ({}), waiting for the memory to land",
    unloaded,
    crate::launch::admission::human_gib(freed_now),
  );
  wait_for_room(state, refusal.demand_bytes).await;
  true
}

/// The unloadable launches, least-recently-used first.
async fn room_candidates(
  state: &Arc<ProxyState>,
  ttls: &HashMap<LaunchId, Duration>,
) -> Vec<RoomCandidate> {
  let supervisors = state.ctx.supervisors.snapshot().await;
  let running = state.ctx.state.snapshot().await;
  let mut out = Vec::new();
  for (launch_id, model) in supervisors {
    if !matches!(model.state().await, ManagedState::Ready) || model.inflight() > 0 {
      continue;
    }
    let last_request_at = state.mru.last_request_at(model.id()).await;
    // A managed multiplexer holds its models inside the shared process, so its
    // credit is what its *eligible* rows are worth and freeing them is an unload
    // call rather than a SIGTERM. Row-level gates are read off the rows here: a
    // delegated row has no supervisor, so this is the only place its origin and
    // its preset's `idle_ttl_secs` can be honoured.
    if crate::backend::umbrella_owner(&launch_id).is_some() {
      let mut bytes = 0u64;
      let mut targets: Vec<LaunchId> = Vec::new();
      for row in delegated_rows(&running.running, model.port()) {
        if !row_evictable(row, ttls) {
          continue;
        }
        let Some(size) = resident_estimate(&state.ctx, row).await else {
          continue;
        };
        bytes = bytes.saturating_add(size);
        if let Some(id) = row.launch_id.clone() {
          targets.push(id);
        }
      }
      if bytes > 0 && !targets.is_empty() {
        out.push(RoomCandidate {
          bytes,
          last_request_at,
          guard: model,
          targets,
        });
      }
      continue;
    }
    if model.origin() != LaunchOrigin::AutoStart {
      continue;
    }
    if ttls.get(&launch_id).is_some_and(Duration::is_zero) {
      continue;
    }
    let Some(row) = running
      .running
      .iter()
      .find(|r| r.launch_id.as_ref() == Some(&launch_id))
    else {
      continue;
    };
    match resident_estimate(&state.ctx, row).await {
      Some(bytes) if bytes > 0 => out.push(RoomCandidate {
        bytes,
        last_request_at,
        guard: model,
        targets: vec![launch_id],
      }),
      _ => log::debug!(
        "proxy make-room: {} has no size to credit — not a candidate",
        launch_id.as_str()
      ),
    }
  }
  // Never-touched rows sort first: an auto-start stamps the MRU on Ready, so a
  // missing stamp means it came up and was never used, which is the least the
  // user can want kept warm.
  out.sort_by_key(|c| {
    std::cmp::Reverse(
      c.last_request_at
        .map(|t| t.elapsed())
        .unwrap_or(Duration::MAX),
    )
  });
  out
}

/// What unloading `row` is worth: the demand the admission gate priced it at, or
/// the same figure a launch is measured with at admission — catalog metadata,
/// then the total across every shard of a split GGUF, then the file or repo size.
/// `None` when nothing sizes it, which keeps the row out of the candidate set
/// rather than crediting it for memory nobody knows it holds.
async fn resident_estimate(
  ctx: &crate::daemon::context::MethodContext,
  row: &crate::daemon::state_store::RunningSnapshot,
) -> Option<u64> {
  if let Some(demand) = row.projected_demand_bytes {
    return Some(demand);
  }
  let bytes = crate::daemon::launch_service::launch_resident_bytes(
    ctx,
    &row.params.model_path,
    &row.params.extras,
  )
  .await;
  (bytes > 0).then_some(bytes)
}

/// Poll the sampled free memory until it covers `needed` or the window closes, so
/// the retry is not priced against memory a stopped launch still holds. On a
/// timeout this returns anyway and admission decides — this is a wait, not a
/// second gate.
///
/// The figure is the gate's own: post-headroom free minus what other launches
/// have reserved, so a reservation still held by a launching peer cannot read as
/// room. One deliberate coarseness: this always budgets the default pool, while a
/// fully GPU-offloaded launch inside an LXC container on an AMD APU is priced on
/// GTT alone (see `admission::gtt_only_budget`). There this can wait out the
/// window when the launch would in fact fit; the window is bounded and the retry's
/// own admission call stays the authority.
async fn wait_for_room(state: &Arc<ProxyState>, needed: u64) {
  let Some(slot) = state.ctx.host_metrics.as_ref() else {
    return;
  };
  let deadline = Instant::now() + room_wait_window(state.ctx.host_metrics_interval);
  while Instant::now() < deadline {
    let free = crate::launch::admission::effective_free_bytes(&slot.read().await.clone())
      .saturating_sub(state.ctx.admission.reserved_bytes());
    if free >= needed {
      return;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
  }
}

/// The wait has to outlast two sampler ticks: the tick in flight when the stops
/// finish may have read memory before they did, and the retry's admission reads
/// the same sample, so ending the wait before a post-stop tick refuses the launch
/// after the models were already unloaded.
fn room_wait_window(sample_interval: Duration) -> Duration {
  MAKE_ROOM_FREE_WAIT.max(sample_interval.saturating_mul(2))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn room_wait_outlasts_two_sampler_ticks() {
    assert_eq!(
      room_wait_window(Duration::from_secs(1)),
      MAKE_ROOM_FREE_WAIT
    );
    assert_eq!(
      room_wait_window(Duration::from_secs(60)),
      Duration::from_secs(120)
    );
  }

  fn ttl() -> Duration {
    Duration::from_secs(60)
  }

  #[test]
  fn decide_skips_manual_origin() {
    let d = decide(
      LaunchOrigin::Manual,
      &ManagedState::Ready,
      0,
      Some(Duration::from_secs(3600)),
      ttl(),
    );
    assert_eq!(d, SweepDecision::Skip);
  }

  #[test]
  fn decide_skips_non_ready_states() {
    for s in [
      ManagedState::Launching,
      ManagedState::Loading,
      ManagedState::Stopping,
      ManagedState::Stopped,
      ManagedState::Error { cause: "x".into() },
    ] {
      let d = decide(
        LaunchOrigin::AutoStart,
        &s,
        0,
        Some(Duration::from_secs(3600)),
        ttl(),
      );
      assert_eq!(d, SweepDecision::Skip, "state {s:?} should skip");
    }
  }

  #[test]
  fn decide_skips_when_inflight_gt_zero() {
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      1,
      Some(Duration::from_secs(3600)),
      ttl(),
    );
    assert_eq!(
      d,
      SweepDecision::Skip,
      "in-flight requests must not be evicted mid-stream"
    );
  }

  #[test]
  fn decide_skips_when_idle_under_ttl() {
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      0,
      Some(Duration::from_secs(30)),
      ttl(),
    );
    assert_eq!(d, SweepDecision::Skip);
  }

  #[test]
  fn decide_skips_when_no_mru_stamp_yet() {
    // auto_start touches the MRU on Ready, so missing stamp signals a
    // race. Skip rather than evict so a first request doesn't get
    // pre-empted.
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      0,
      None,
      ttl(),
    );
    assert_eq!(d, SweepDecision::Skip);
  }

  #[test]
  fn decide_evicts_idle_auto_start_ready_supervisor() {
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      0,
      Some(Duration::from_secs(61)),
      ttl(),
    );
    assert_eq!(d, SweepDecision::Evict);
  }

  #[test]
  fn sweep_cadence_clamps_against_short_and_long_ttls() {
    assert_eq!(
      sweep_cadence(Duration::from_secs(1)),
      Duration::from_secs(5)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(10)),
      Duration::from_secs(10)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(30)),
      Duration::from_secs(30)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(120)),
      Duration::from_secs(30)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(30 * 60)),
      Duration::from_secs(30)
    );
    // A 0 global TTL ("no global deadline, preset TTLs only") takes the slowest
    // cadence rather than busy-sweeping.
    assert_eq!(sweep_cadence(Duration::ZERO), Duration::from_secs(30));
  }

  fn snapshot_row(path: &str, demand: Option<u64>) -> crate::daemon::state_store::RunningSnapshot {
    let mut row = crate::test_support::running_row(path);
    if let Some(bytes) = demand {
      row = row.projected_demand(bytes);
    }
    row.build()
  }

  fn empty_ctx() -> crate::daemon::context::MethodContext {
    crate::daemon::context::MethodContext::with_catalog(
      crate::daemon::shutdown::ShutdownToken::new(),
      crate::discovery::ModelCatalog::new(),
    )
  }

  /// The two row-level exemptions, read off the row itself because a delegated
  /// row inside a multiplexer has no supervisor to consult: only a proxy
  /// auto-start is ever given up, and a preset pinned to `idle_ttl_secs: 0` keeps
  /// its own row resident without touching its neighbours.
  #[test]
  fn row_evictable_requires_an_auto_start_and_a_nonzero_ttl() {
    let mut ttls = HashMap::new();
    ttls.insert(LaunchId("L1".to_string()), Duration::from_secs(60));
    ttls.insert(LaunchId("L2".to_string()), Duration::ZERO);

    let auto = crate::test_support::running_row("/m.gguf")
      .origin(LaunchOrigin::AutoStart)
      .build();
    assert!(row_evictable(&auto, &ttls));

    let manual = crate::test_support::running_row("/m.gguf")
      .origin(LaunchOrigin::Manual)
      .build();
    assert!(!row_evictable(&manual, &ttls));

    let adopted = crate::test_support::running_row("/m.gguf").build();
    assert!(!row_evictable(&adopted, &ttls));

    let pinned = crate::test_support::running_row("/m.gguf")
      .launch_id("L2")
      .origin(LaunchOrigin::AutoStart)
      .build();
    assert!(!row_evictable(&pinned, &ttls));
  }

  /// Make-room credits a launch with the figure admission priced it at, so the
  /// credit and the refused demand are in one unit — that stamp wins over every
  /// other size available.
  #[tokio::test]
  async fn resident_estimate_prefers_the_admission_projection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("m.gguf");
    std::fs::write(&model, vec![0u8; 10]).expect("write");
    let row = snapshot_row(model.to_str().unwrap(), Some(4096));
    assert_eq!(resident_estimate(&empty_ctx(), &row).await, Some(4096));
  }

  /// A row the gate never budgeted (adopted, or delegated inside a multiplexer)
  /// falls back to the figure a launch is measured with at admission: catalog
  /// metadata when the catalog knows the model, else the size on disk across
  /// every shard of a split file.
  #[tokio::test]
  async fn resident_estimate_falls_back_to_the_launch_weight_figure() {
    let ctx = empty_ctx();
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("m.gguf");
    std::fs::write(&model, vec![0u8; 4096]).expect("write");
    let row = snapshot_row(model.to_str().unwrap(), None);
    assert_eq!(resident_estimate(&ctx, &row).await, Some(4096));

    // Nothing sizes it: the row stays out of the candidate set instead of being
    // credited for memory nobody knows it holds.
    let ghost = snapshot_row("/no/such/model.gguf", None);
    assert_eq!(resident_estimate(&ctx, &ghost).await, None);
  }
}
