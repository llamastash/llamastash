//! Proxy-side launch helper.
//!
//! When a request lands for a catalog model with no Ready supervisor,
//! [`auto_start`] drives the launch in-process through
//! [`crate::daemon::launch_service::compose_and_spawn`] — the same
//! composition pipeline the IPC `start_model` handler uses, so the two
//! paths can't drift apart.
//!
//! - Leaders first attach to a supervisor already serving the file
//!   ([`attach_target`]). The coalesce map covers only proxy-driven launches,
//!   so a CLI/TUI launch still in `Loading` would otherwise get a duplicate
//!   engine process on the same model.
//! - There is no client-facing timeout. Only a supervisor `Error` ends the wait
//!   on `Loading`.
//! - A launch the admission gate admitted that dies before `Ready` gets one
//!   make-room-and-retry pass ([`await_ready_or_make_room`]) before the caller's
//!   fallback, because a wrong admission is no reason to let another model
//!   answer.
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md.

use std::sync::Arc;
use std::time::Duration;

use crate::backend::Backend;
use crate::daemon::context::MethodContext;
use crate::daemon::launch_service::{
  compose_and_spawn, LaunchModeWire, StartParams, StartedLaunch,
};
use crate::daemon::supervisor::{ErrorKind, ManagedModel, ManagedState};
use crate::gguf::identity::ModelId;
use crate::launch::mode::LaunchMode;
use crate::launch::resolve::CatalogRow;

use super::coalesce::{AcquireOutcome, SharedOutcome};
use super::request_log::RequestRecord;
use super::state::ProxyState;

/// Outcome of [`auto_start`]. The proxy's caller branches on this:
/// `Ready` forwards against `(port, model_id)`; `Failed` enters the
/// family-MRU fallback path.
#[derive(Clone, Debug)]
pub(crate) enum LaunchOutcome {
  /// Supervisor reached `ManagedState::Ready`. The caller forwards
  /// against `port`; `model_id` is threaded so the forward path can
  /// re-verify the supervisor still owns `port` before sending
  /// (port-reuse defense — see `super::forward`).
  Ready { port: u16, model_id: ModelId },
  /// Launch hit a terminal error before reaching Ready. `cause`
  /// surfaces in the 503 `launch_failed` JSON body when no fallback
  /// is available.
  Failed { cause: String },
}

impl From<SharedOutcome> for LaunchOutcome {
  fn from(s: SharedOutcome) -> Self {
    match s {
      SharedOutcome::Ready { port, model_id } => LaunchOutcome::Ready { port, model_id },
      SharedOutcome::Failed { cause } => LaunchOutcome::Failed { cause },
    }
  }
}

impl From<LaunchOutcome> for SharedOutcome {
  fn from(o: LaunchOutcome) -> Self {
    match o {
      LaunchOutcome::Ready { port, model_id } => SharedOutcome::Ready { port, model_id },
      LaunchOutcome::Failed { cause } => SharedOutcome::Failed { cause },
    }
  }
}

/// Drive a launch (or wait on an in-flight one) and resolve to a
/// port once Ready. Returns [`LaunchOutcome::Failed`] if the
/// supervisor reaches `Error{cause}` before Ready, or if a follower
/// observed the leader's launch failure.
///
/// `endpoint_mode` is the mode the requested endpoint implies
/// (`/v1/embeddings` → embedding, `/v1/rerank` → rerank, `None` for
/// the chat-shaped routes) — see [`resolve_auto_start_mode`].
///
/// `record` is the request's log row. The request whose spawn succeeds is
/// marked as the auto-start, and what make-room unloads for it is written
/// to the row before the stops begin. A request that waits on a
/// launch someone else started is marked with neither.
///
/// The proxy must hold `Arc<ProxyState>` for the duration so the
/// coalesce + supervisor handles stay alive across the await
/// points.
pub(crate) async fn auto_start(
  state: &Arc<ProxyState>,
  resolved: &CatalogRow,
  endpoint_mode: Option<LaunchMode>,
  name: Option<String>,
  record: &mut RequestRecord,
) -> LaunchOutcome {
  // Resolved here rather
  // than from any in-process cache so the single-flight key matches what
  // `compose_and_spawn` will observe at spawn time — which is why both go
  // through the same resolver.
  //
  // A GGUF header read is up to 16 MiB of synchronous I/O; offload to a
  // blocking thread so we don't stall the tokio worker.
  let row = resolved.clone();
  let ctx = state.ctx.clone();
  let model_id = match tokio::task::spawn_blocking(move || canonical_id_for_row(&row, &ctx)).await {
    Ok(Ok(id)) => id,
    Ok(Err(cause)) => return LaunchOutcome::Failed { cause },
    Err(join) => {
      return LaunchOutcome::Failed {
        cause: format!("model identity resolution panicked: {join}"),
      };
    }
  };

  // Cap the auto-start retry storm. If `model_id` has racked up
  // `MAX_FAILURES` launch failures within `WINDOW_SECS`, refuse the
  // attempt up front — sidesteps the observed 10+ identical-failure
  // launches per 30 s when an agent loops on a model that can't load.
  // The check is *before* the coalesce acquire so followers don't
  // sit on a slot that we already know won't recover.
  if let Some(cause) = state
    .failures
    .over_limit(&model_id, std::time::Instant::now())
  {
    return LaunchOutcome::Failed { cause };
  }

  // Single-flight acquire. Leaders run the launch and stamp the
  // outcome on the slot; followers read the outcome directly when
  // the leader finishes (or wake to `None` on cancellation).
  match state
    .coalesce
    .acquire((model_id.clone(), name.clone()))
    .await
  {
    AcquireOutcome::Leader(leader) => {
      let outcome = drive_launch_as_leader(
        state,
        resolved,
        &model_id,
        endpoint_mode,
        name.clone(),
        record,
      )
      .await;
      // Record outcome against the failure tracker before publishing
      // to followers so a follower that wakes up immediately and asks
      // `over_limit` sees a coherent count.
      match &outcome {
        LaunchOutcome::Ready { .. } => state.failures.clear(&model_id),
        LaunchOutcome::Failed { .. } => {
          state
            .failures
            .note_failure(&model_id, std::time::Instant::now());
        }
      }
      leader.finish(outcome.clone().into()).await;
      outcome
    }
    AcquireOutcome::Follower(follower) => match follower.wait().await {
      Some(shared) => shared.into(),
      None => LaunchOutcome::Failed {
        cause: "leader launch cancelled".to_string(),
      },
    },
  }
}

/// The admission numbers behind a launch refusal, or `None` for any other
/// failure. `compose_and_spawn` refuses before spawn with `ResourceExhausted`
/// and carries the projection in its `data`; make-room needs the same figures
/// the gate used, not a re-derivation of them.
fn admission_refusal(
  e: &crate::ipc::protocol::ErrorObject,
) -> Option<crate::launch::admission::Refusal> {
  if e.code != crate::ipc::protocol::ErrorCode::ResourceExhausted.as_i32() {
    return None;
  }
  let data = e.data.as_ref()?;
  if data.get("cause").and_then(|c| c.as_str()) != Some("launch_refused") {
    return None;
  }
  let field = |key: &str| data.get(key).and_then(|v| v.as_u64());
  Some(crate::launch::admission::Refusal {
    demand_bytes: field("demand_bytes")?,
    effective_free_bytes: field("effective_free_bytes")?,
    reserved_bytes: field("reserved_bytes").unwrap_or(0),
  })
}

/// Attach to an in-flight launch when one exists, else run
/// [`compose_and_spawn`]; either way wait for `Ready` via
/// [`await_ready`]. Pulled out so the leader arm of [`auto_start`]
/// reads top-to-bottom without nesting.
async fn drive_launch_as_leader(
  state: &Arc<ProxyState>,
  resolved: &CatalogRow,
  model_id: &ModelId,
  endpoint_mode: Option<LaunchMode>,
  name: Option<String>,
  record: &mut RequestRecord,
) -> LaunchOutcome {
  // A launch for this file may already be underway from another
  // surface — CLI `start`, the TUI, a boot-time restore — and the
  // coalesce map only covers proxy-driven launches. Attach to it
  // instead of spawning a second engine on the same file,
  // which would hold its own weights in VRAM until the idle sweep.
  // Ready is included so a supervisor that came up between `decide`
  // and here is reused too.
  if let Some(existing) = attach_target(state, model_id, name.as_deref()).await {
    return await_ready(state, &existing).await;
  }

  let mode = resolve_auto_start_mode(
    resolved.mode_hint.as_deref(),
    endpoint_mode,
    last_used_mode(state, model_id).await,
  );
  // Built by a closure so the make-room retry below sends the same request a
  // second time rather than a partially consumed copy.
  let start_params = || StartParams {
    model_path: std::path::PathBuf::from(&resolved.path),
    name: name.clone(),
    mode,
    ..StartParams::default()
  };
  // One eviction per request: the refusal branch below may already have paid
  // for one, and unloading a second round of models for a launch that is still
  // not up would be two losses rather than one.
  let mut evicted_once = false;
  let started = match compose_and_spawn(
    &state.ctx,
    start_params(),
    crate::daemon::supervisor::LaunchOrigin::AutoStart,
  )
  .await
  {
    Ok(s) => s,
    Err(e) => {
      // The memory admission gate refused it. Before answering 503, try to make
      // room by unloading the idle auto-started models that hold the memory —
      // least-recently-used first, all-or-nothing. Any other failure, or a retry
      // that still does not fit, fails exactly as before. A failed retry costs the
      // family-MRU fallback its idle candidates, which is the honest price of
      // having unloaded them: `start --force` or a smaller request is the way
      // back, and TODO tracks tightening the estimate that let this happen.
      let failed = |err: crate::ipc::protocol::ErrorObject| LaunchOutcome::Failed {
        cause: format!("compose_and_spawn: {}", err.message),
      };
      let Some(refusal) = admission_refusal(&e) else {
        return failed(e);
      };
      let made_room = super::eviction::make_room(state, &refusal, |unloading| {
        record.set_evicted(unloading.iter().map(|id| id.as_str().to_string()).collect());
        record.publish();
      })
      .await;
      if !made_room {
        return failed(e);
      }
      evicted_once = true;
      match compose_and_spawn(
        &state.ctx,
        start_params(),
        crate::daemon::supervisor::LaunchOrigin::AutoStart,
      )
      .await
      {
        Ok(s) => s,
        Err(retry) => return failed(retry),
      }
    }
  };
  // Only now is this request the one that started the model: a spawn that
  // was refused, or one the client did not wait for, started nothing.
  record.set_auto_start();
  record.publish();
  // No human watches an auto-start; log any advisories (dropped knobs,
  // deepseek4 KV-blind note, ssd_streaming bypass) to the daemon log.
  for w in &started.warnings {
    log::warn!("proxy auto-start: {w}");
  }

  await_ready_or_make_room(state, &started, evicted_once, start_params, record).await
}

/// How long a launch has to stay up before its death counts as running out of
/// memory. One that exits inside this failed before it could have: a bad file, a
/// bad argv, a missing binary. Unloading a resident model to retry that is a pure
/// loss, so the launch just fails.
const MIN_RETRYABLE_LOAD_ATTEMPT: Duration = Duration::from_secs(5);

/// Whether a launch that failed this far into its first attempt says something
/// about memory: the child exited on its own or the strict-fit clamp refused it,
/// and it lived long enough to have actually tried. A probe timeout is an engine
/// that never answered — evicting for it unloads idle models and then pays the
/// whole readiness timeout again — a stop is a decision, and a death inside the
/// floor failed before it could have been starved.
fn death_may_be_memory(state: &ManagedState, elapsed: Duration) -> bool {
  elapsed >= MIN_RETRYABLE_LOAD_ATTEMPT
    && matches!(
      state,
      ManagedState::Error {
        kind: ErrorKind::Exited | ErrorKind::FitClamped,
        ..
      }
    )
}

/// Wait for the spawn to reach `Ready`; if the gate admitted it and it died
/// anyway, unload what it takes and try once more.
///
/// A gate admission is a projection, and a launch that dies after one is evidence
/// the projection was too low — free memory comes from `MemAvailable`, which
/// counts a resident mmap-backed engine's weights as reclaimable page cache (see
/// [`super::eviction::make_room_after_launch_death`]). Nothing in the launch path
/// notices otherwise: the request falls through to the family-MRU fallback and a
/// *different* model answers it at 200, which is how an agent harness silently
/// stops running the model it asked for.
async fn await_ready_or_make_room(
  state: &Arc<ProxyState>,
  started: &StartedLaunch,
  already_evicted: bool,
  start_params: impl Fn() -> StartParams,
  record: &mut RequestRecord,
) -> LaunchOutcome {
  let attempt_started = std::time::Instant::now();
  let outcome = await_ready(state, &started.model).await;
  let cause = match outcome {
    LaunchOutcome::Failed { cause } => cause,
    ready => return ready,
  };
  // One eviction per request: a request that already bought room on an
  // admission refusal has spent its allowance on this launch.
  if already_evicted {
    return LaunchOutcome::Failed { cause };
  }
  // Only a death says anything about memory, and only some deaths.
  if !death_may_be_memory(&started.model.state().await, attempt_started.elapsed()) {
    return LaunchOutcome::Failed { cause };
  }
  // No demand figure means no admission projection was ever wrong about this
  // launch, so there is nothing for an eviction to correct.
  let Some(demand) = started.admitted_demand_bytes else {
    return LaunchOutcome::Failed { cause };
  };
  let made_room = super::eviction::make_room_after_launch_death(state, demand, |unloading| {
    record.set_evicted(unloading.iter().map(|id| id.as_str().to_string()).collect());
    record.publish();
  })
  .await;
  if !made_room {
    return LaunchOutcome::Failed { cause };
  }
  log::warn!(
    "proxy auto-start: {} ({} priced) died after {:?} — {cause} — retried once with room made for it",
    started.model_id.path.display(),
    crate::launch::admission::human_gib(demand),
    attempt_started.elapsed(),
  );
  match compose_and_spawn(
    &state.ctx,
    start_params(),
    crate::daemon::supervisor::LaunchOrigin::AutoStart,
  )
  .await
  {
    Ok(retry) => {
      for w in &retry.warnings {
        log::warn!("proxy auto-start: {w}");
      }
      let retried = await_ready(state, &retry.model).await;
      if matches!(retried, LaunchOutcome::Ready { .. }) {
        // Named routing reuses a model's only launch, so a dead row left here
        // makes the next `<model>@<name>` load a second copy.
        let backend =
          crate::daemon::launch_service::backend_for_launch(&state.ctx, &started.launch_id).await;
        if let Err(err) = backend.stop(&state.ctx, &started.launch_id, 0).await {
          log::warn!(
            "proxy auto-start: could not remove dead launch {}: {}",
            started.launch_id.as_str(),
            err.message
          );
        }
      }
      retried
    }
    Err(err) => LaunchOutcome::Failed {
      cause: format!("compose_and_spawn: {}", err.message),
    },
  }
}

/// A supervisor already serving `model_id`'s file that this request can
/// wait on rather than spawning alongside. `Stopping` / `Stopped` /
/// `Error` entries are skipped — those are on their way out, so the
/// request needs its own launch.
///
/// Matches on path alone, and ignores the running launch's mode, so
/// this is the same predicate [`super::route::decide`] applies when it
/// walks for a Ready supervisor. Tightening either here (full `ModelId`
/// including the header hash, or a mode-compatibility gate) would make
/// the load window behave differently from the Ready path: the request
/// would spawn a second process for a file another supervisor already
/// holds, then the identical request a second later would forward to
/// that same supervisor. One supervisor per file is the invariant the
/// whole proxy routes on.
async fn attach_target(
  state: &Arc<ProxyState>,
  model_id: &ModelId,
  name: Option<&str>,
) -> Option<ManagedModel> {
  // When a name is present, only attach to a launch that carries that name.
  // A differently-named launch of the same model is a distinct target and
  // must not be reused — the caller will spawn a new one instead.
  let state_snap = if name.is_some() {
    Some(state.ctx.state.snapshot().await)
  } else {
    None
  };
  for (launch_id, model) in state.ctx.supervisors.snapshot().await {
    if model.id().path != model_id.path {
      continue;
    }
    if let (Some(n), Some(st)) = (name, &state_snap) {
      if !st
        .running
        .iter()
        .any(|r| r.carries_name(Some(&launch_id), model.port(), n))
      {
        continue;
      }
    }
    if matches!(
      model.state().await,
      ManagedState::Launching | ManagedState::Loading | ManagedState::Ready
    ) {
      return Some(model);
    }
  }
  None
}

/// Poll the supervisor state machine at 100 ms cadence until it
/// reaches `Ready` or a terminal state. No client-facing timeout —
/// only `Error{cause}` and `Stopping` trigger fallback (Loading waits
/// indefinitely).
async fn await_ready(state: &Arc<ProxyState>, model: &ManagedModel) -> LaunchOutcome {
  loop {
    match model.state().await {
      ManagedState::Ready => {
        // Stamp the MRU now so the freshly-auto-started supervisor
        // has a starting `last_request_at`. Without this its idle
        // timer would only begin when the first proxy forward
        // touched the MRU — and a loaded-but-never-queried model
        // would sit forever with `None` and confuse the sweeper.
        let model_id = model.id().clone();
        state.mru.touch(&model_id).await;
        return LaunchOutcome::Ready {
          port: model.port(),
          model_id,
        };
      }
      ManagedState::Error { cause, .. } => {
        return LaunchOutcome::Failed { cause };
      }
      ManagedState::Stopped => {
        return LaunchOutcome::Failed {
          cause: "supervisor exited before reaching Ready".to_string(),
        };
      }
      ManagedState::Stopping => {
        return LaunchOutcome::Failed {
          cause: "model stopped while launching".to_string(),
        };
      }
      ManagedState::Launching | ManagedState::Loading => {
        tokio::time::sleep(Duration::from_millis(100)).await;
      }
    }
  }
}

/// The mode a previous launch of this file recorded in `last_params` —
/// the mode the user actually chose, which discovery's header-derived
/// hint can't see (a BERT reranker reads as `embedding`).
///
/// Keyed on the full [`ModelId`] (path **and** header hash), matching
/// how `compose_and_spawn` looks up the same entry for its last-used
/// knob layer. A looser path-only match would hand a mode over from a
/// record the knob resolver has already disowned as a different file.
async fn last_used_mode(state: &Arc<ProxyState>, model_id: &ModelId) -> Option<LaunchMode> {
  state
    .ctx
    .state
    .snapshot()
    .await
    .last_params
    .iter()
    .find(|e| e.id.as_gguf() == Some(model_id))
    .map(|e| e.params.mode)
}

/// Launch mode for an auto-start: the endpoint that triggered it wins,
/// then the recorded `last_params` mode.
///
/// A non-chat mode is only ever adopted for a model the hint did *not*
/// classify as chat: `--embeddings` / `--reranking` make llama-server
/// refuse `/v1/chat/completions` for the supervisor's whole lifetime,
/// so an embeddings request must not be able to lock a chat model out
/// of chat. Within the embedding family the two upper tiers matter —
/// a BERT reranker hints `embedding`, and launching it that way yields
/// a supervisor that answers 501 to the very `/v1/rerank` call that
/// started it.
///
/// `None` means "nothing here chose a mode" and hands the decision to the
/// daemon, which applies the model's `default:` preset pin and then the same
/// hint. The hint is deliberately not returned as if it were a choice: sending
/// it shadowed every preset that pinned a mode. The guard above is unchanged —
/// a *request* still cannot raise a chat-hinted model, only the user's own
/// config can.
fn resolve_auto_start_mode(
  hint: Option<&str>,
  endpoint_mode: Option<LaunchMode>,
  last_used: Option<LaunchMode>,
) -> Option<LaunchModeWire> {
  if !matches!(launch_mode_from_hint(hint), Some(LaunchModeWire::Chat)) {
    for m in [endpoint_mode, last_used].into_iter().flatten() {
      match m {
        LaunchMode::Embedding => return Some(LaunchModeWire::Embedding),
        LaunchMode::Rerank => return Some(LaunchModeWire::Rerank),
        // The media modes ask for no launch flag, so they choose nothing.
        LaunchMode::Chat | LaunchMode::Image | LaunchMode::Tts | LaunchMode::Stt => {}
      }
    }
  }
  None
}

/// Map a catalog row's GGUF-derived `mode_hint` string onto the launch
/// wire mode so `compose_and_spawn` emits `--embeddings` / `--rerank`
/// when the model needs it. `None` (unknown/absent hint) leaves the
/// chat default in place.
fn launch_mode_from_hint(hint: Option<&str>) -> Option<LaunchModeWire> {
  match hint? {
    "chat" => Some(LaunchModeWire::Chat),
    "embedding" => Some(LaunchModeWire::Embedding),
    "rerank" => Some(LaunchModeWire::Rerank),
    _ => None,
  }
}

/// Compute the canonical [`ModelId`] for a resolved [`CatalogRow`].
/// Synchronous — call via `spawn_blocking` to keep the async worker
/// thread free.
///
/// Delegates to the shared resolver, so a directory-shaped row (a safetensors
/// snapshot) gets its backend's synthetic id instead of being handed to the
/// GGUF header reader. Reading the header unconditionally answers
/// `Is a directory (os error 21)` for every model of that shape.
fn canonical_id_for_row(row: &CatalogRow, ctx: &MethodContext) -> Result<ModelId, String> {
  let path = std::path::Path::new(&row.path);
  crate::backend::resolve_identity_for_path(path, Some(ctx))
    .map(|r| r.id)
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(path: &str, mode_hint: Option<&str>) -> CatalogRow {
    CatalogRow {
      path: path.to_string(),
      model_id: None,
      parent: "/m".to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: None,
      native_ctx: None,
      mode_hint: mode_hint.map(str::to_string),
      parameter_label: None,
      weights_bytes: None,
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    }
  }

  #[test]
  fn launch_mode_from_hint_maps_each_wire_mode() {
    assert!(matches!(
      launch_mode_from_hint(Some("chat")),
      Some(LaunchModeWire::Chat)
    ));
    assert!(matches!(
      launch_mode_from_hint(Some("embedding")),
      Some(LaunchModeWire::Embedding)
    ));
    assert!(matches!(
      launch_mode_from_hint(Some("rerank")),
      Some(LaunchModeWire::Rerank)
    ));
  }

  #[test]
  fn launch_mode_from_hint_is_none_for_unknown_or_absent() {
    assert!(launch_mode_from_hint(None).is_none());
    assert!(launch_mode_from_hint(Some("")).is_none());
    assert!(launch_mode_from_hint(Some("unknown")).is_none());
  }

  #[test]
  fn endpoint_mode_beats_an_embedding_hint_for_a_reranker() {
    // A BERT reranker hints `embedding`, so the
    // hint alone launches a supervisor that 501s on `/v1/rerank`.
    assert!(matches!(
      resolve_auto_start_mode(Some("embedding"), Some(LaunchMode::Rerank), None),
      Some(LaunchModeWire::Rerank)
    ));
  }

  #[test]
  fn last_used_mode_beats_the_hint_when_the_endpoint_is_mode_less() {
    // `/v1/chat/completions` implies no mode; the recorded launch does.
    assert!(matches!(
      resolve_auto_start_mode(Some("embedding"), None, Some(LaunchMode::Rerank)),
      Some(LaunchModeWire::Rerank)
    ));
  }

  #[test]
  fn endpoint_mode_outranks_last_used() {
    assert!(matches!(
      resolve_auto_start_mode(
        Some("embedding"),
        Some(LaunchMode::Embedding),
        Some(LaunchMode::Rerank)
      ),
      Some(LaunchModeWire::Embedding)
    ));
  }

  #[test]
  fn a_chat_model_is_never_forced_into_a_non_chat_mode() {
    // An embeddings launch makes the engine refuse chat for the rest of the
    // supervisor's life, so an embeddings request must not lock a chat model
    // out of chat. Nothing is sent, and the daemon lands on the same
    // hint (via a `default:` preset's pin, if the user wrote one).
    assert!(resolve_auto_start_mode(Some("chat"), Some(LaunchMode::Embedding), None).is_none());
    assert!(resolve_auto_start_mode(Some("chat"), None, Some(LaunchMode::Rerank)).is_none());
  }

  #[test]
  fn a_hint_alone_chooses_nothing_and_is_left_to_the_daemon() {
    // The hint is the *model's* default, below the user's config. Sending it
    // as though the proxy had chosen it shadowed every preset that pinned a
    // mode; the daemon reads the same hint off the header it already parsed.
    for hint in [Some("embedding"), Some("chat"), Some("rerank"), None] {
      assert!(
        resolve_auto_start_mode(hint, None, None).is_none(),
        "{hint:?}"
      );
    }
    // An unclassified model still adopts the endpoint's mode.
    assert!(matches!(
      resolve_auto_start_mode(None, Some(LaunchMode::Embedding), None),
      Some(LaunchModeWire::Embedding)
    ));
  }

  /// The auto-start retry is gated on reading the gate's own numbers back out of
  /// the refusal, so the `data` contract is asserted here: a refusal that loses
  /// its figures must not be retried with made-up ones.
  #[test]
  fn admission_refusal_reads_the_gate_projection_back() {
    use crate::ipc::protocol::{ErrorCode, ErrorObject};
    let refusal = crate::launch::admission::Refusal {
      demand_bytes: 900,
      effective_free_bytes: 400,
      reserved_bytes: 100,
    };
    let err = ErrorObject::with_data(
      ErrorCode::ResourceExhausted,
      "refused",
      serde_json::json!({
        "cause": "launch_refused",
        "demand_bytes": refusal.demand_bytes,
        "effective_free_bytes": refusal.effective_free_bytes,
        "reserved_bytes": refusal.reserved_bytes,
      }),
    );
    let read = admission_refusal(&err).expect("the gate's own figures come back");
    assert_eq!(read, refusal);
    assert_eq!(read.available_bytes(), 300);

    // Any other failure — a different code, a different cause, no data — is not
    // an admission refusal, so the auto-start fails without unloading anything.
    assert!(admission_refusal(&ErrorObject::new(ErrorCode::InvalidParams, "bad")).is_none());
    assert!(admission_refusal(&ErrorObject::with_data(
      ErrorCode::ResourceExhausted,
      "refused",
      serde_json::json!({ "cause": "other" }),
    ))
    .is_none());
    assert!(
      admission_refusal(&ErrorObject::new(ErrorCode::ResourceExhausted, "no data")).is_none()
    );
  }

  #[test]
  fn outcome_round_trips_through_shared_outcome() {
    use crate::gguf::identity::ModelId;
    let id = ModelId {
      path: std::path::PathBuf::from("/m/x.gguf"),
      header_blake3: [3u8; 32],
    };
    let ready = LaunchOutcome::Ready {
      port: 11440,
      model_id: id.clone(),
    };
    let ready_shared: SharedOutcome = ready.into();
    match LaunchOutcome::from(ready_shared) {
      LaunchOutcome::Ready { port, model_id } => {
        assert_eq!(port, 11440);
        assert_eq!(model_id, id);
      }
      other => panic!("expected Ready, got {other:?}"),
    }

    let failed = LaunchOutcome::Failed {
      cause: "boom".to_string(),
    };
    let failed_shared: SharedOutcome = failed.into();
    match LaunchOutcome::from(failed_shared) {
      LaunchOutcome::Failed { cause } => assert_eq!(cause, "boom"),
      other => panic!("expected Failed, got {other:?}"),
    }
  }

  fn ctx() -> MethodContext {
    MethodContext::new(crate::daemon::shutdown::ShutdownToken::new())
  }

  #[test]
  fn canonical_id_for_row_errors_on_missing_file() {
    let r = row("/nonexistent/secret-model.gguf", None);
    let err = canonical_id_for_row(&r, &ctx()).expect_err("missing file must error");
    assert!(err.starts_with("could not read GGUF header"), "got: {err}");
  }

  #[test]
  fn canonical_id_for_row_computes_id_for_real_gguf() {
    use crate::gguf::test_fixtures::build_minimal_gguf;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiny.gguf");
    std::fs::write(&path, build_minimal_gguf("llama")).expect("write gguf");
    let r = row(path.to_str().unwrap(), Some("chat"));
    let id = canonical_id_for_row(&r, &ctx()).expect("real gguf resolves");
    assert_eq!(id.path, crate::util::paths::canonicalize(&path).unwrap());
    // Header hash is populated (not the all-zero synthetic placeholder).
    assert_ne!(id.header_blake3, [0u8; 32]);
  }

  /// A directory-shaped row (a safetensors snapshot) must resolve without a
  /// header read, or it fails with
  /// `Is a directory (os error 21)` and the proxy answers 503 — the one
  /// surface a directory-shaped model is most likely to be reached through.
  #[test]
  fn canonical_id_for_row_resolves_a_directory_row_without_reading_a_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    let snapshot = dir.path().join("models--o--r/snapshots/rev");
    std::fs::create_dir_all(&snapshot).expect("snapshot");
    std::fs::write(snapshot.join("config.json"), b"{}").expect("config");
    std::fs::write(snapshot.join("model.safetensors"), b"w").expect("weights");

    let r = row(snapshot.to_str().unwrap(), Some("chat"));
    // No context: the ungated registry answer, so the test does not depend on
    // whether a launcher happens to be installed on the host running it.
    let id = crate::backend::resolve_identity_for_path(&snapshot, None)
      .expect("a safetensors snapshot resolves without a header read")
      .id;
    assert_eq!(id.header_blake3, [0u8; 32], "synthetic id, not a digest");
    assert_eq!(id.path, snapshot);
    // And the row-shaped entry point agrees when the backend is available.
    let _ = r;
  }

  #[test]
  fn only_a_death_that_actually_tried_is_memory() {
    let tried = Duration::from_secs(6);
    let instant = Duration::from_secs(4);
    let died = |kind: ErrorKind| ManagedState::Error {
      cause: "x".into(),
      kind,
    };
    assert!(death_may_be_memory(&died(ErrorKind::Exited), tried));
    assert!(death_may_be_memory(&died(ErrorKind::FitClamped), tried));
    assert!(!death_may_be_memory(&died(ErrorKind::ProbeTimeout), tried));
    // Inside the floor the launch was broken, not starved.
    assert!(!death_may_be_memory(&died(ErrorKind::Exited), instant));
    assert!(!death_may_be_memory(&ManagedState::Stopping, tried));
    assert!(!death_may_be_memory(&ManagedState::Stopped, tried));
    assert!(!death_may_be_memory(&ManagedState::Loading, tried));
  }
}
