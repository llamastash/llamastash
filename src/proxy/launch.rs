//! Proxy-side launch helper.
//!
//! When a `/v1/...` request lands for a model that exists in the
//! catalog but has no Ready supervisor, [`auto_start`] drives the
//! launch in-process by calling
//! [`crate::daemon::launch_service::compose_and_spawn`] — the same
//! composition pipeline the IPC `start_model` handler uses, so the two
//! paths can't drift apart.
//!
//! The flow:
//!   1. Build a default [`StartParams`] from the resolved catalog
//!      row (just the path; mode defaults to Chat, no port
//!      preference, no caller knobs — `compose_and_spawn` then
//!      replays the same `last_params → arch_defaults → built-in`
//!      cascade the IPC handler does).
//!   2. Acquire single-flight rights via
//!      [`crate::proxy::coalesce::Coalesce::acquire`]. Leaders run
//!      `compose_and_spawn`; followers `.wait()` and receive the
//!      leader's outcome directly from the slot (no re-snapshot).
//!   3. Poll [`crate::daemon::supervisor::ManagedModel::state`] at
//!      100 ms cadence until it reaches `Ready` (forward) or
//!      `Error{cause}` (fallback). No client-facing timeout — per
//!      the locked Key Decision "Hard supervisor Error only; wait
//!      indefinitely on Loading."
//!
//! Plan: docs/plans/2026-05-21-001-feat-proxy-router-plan.md.

use std::sync::Arc;
use std::time::Duration;

use crate::daemon::launch_service::{compose_and_spawn, LaunchModeWire, StartParams};
use crate::daemon::supervisor::ManagedState;
use crate::gguf::identity::ModelId;
use crate::launch::resolve::CatalogRow;

use super::coalesce::{AcquireOutcome, SharedOutcome};
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
/// The proxy must hold `Arc<ProxyState>` for the duration so the
/// coalesce + supervisor handles stay alive across the await
/// points.
pub(crate) async fn auto_start(state: &Arc<ProxyState>, resolved: &CatalogRow) -> LaunchOutcome {
  // Compute the canonical ModelId from the resolved row. We read
  // the header here rather than trusting any in-process cache so
  // the single-flight key matches what `compose_and_spawn` will
  // observe at spawn time (it does the same read internally).
  //
  // The header read is up to 16 MiB of synchronous I/O; offload to
  // a blocking thread so we don't stall the tokio worker.
  let row = resolved.clone();
  let model_id = match tokio::task::spawn_blocking(move || canonical_id_for_row(&row)).await {
    Ok(Ok(id)) => id,
    Ok(Err(cause)) => return LaunchOutcome::Failed { cause },
    Err(join) => {
      return LaunchOutcome::Failed {
        cause: format!("GGUF header read panicked: {join}"),
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
  match state.coalesce.acquire(model_id.clone()).await {
    AcquireOutcome::Leader(leader) => {
      let outcome = drive_launch_as_leader(state, resolved, model_id.clone()).await;
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

/// Run [`compose_and_spawn`], then poll `state()` at 100 ms until
/// the supervisor reaches `Ready` or `Error`. Pulled out so the
/// leader arm of [`auto_start`] reads top-to-bottom without nesting.
async fn drive_launch_as_leader(
  state: &Arc<ProxyState>,
  resolved: &CatalogRow,
  model_id: ModelId,
) -> LaunchOutcome {
  // Mode is read from the catalog row's GGUF-derived mode_hint so
  // `compose_and_spawn` composes the right argv (`--embeddings` for
  // embedding-only models, `--rerank` for rerank models). Without
  // this the proxy auto-start path defaulted every model to chat
  // mode and embedding requests against a nomic/jina/etc model
  // came back 501 (`This server does not support embeddings`) even
  // though `llamastash start <model>` worked fine. `Unknown` /
  // missing hint leaves `mode = None` so `compose_and_spawn`'s
  // chat default still applies.
  let params = StartParams {
    model_path: std::path::PathBuf::from(&resolved.path),
    mode: launch_mode_from_hint(resolved.mode_hint.as_deref()),
    ..StartParams::default()
  };
  let started = match compose_and_spawn(
    &state.ctx,
    params,
    crate::daemon::supervisor::LaunchOrigin::AutoStart,
  )
  .await
  {
    Ok(s) => s,
    Err(e) => {
      return LaunchOutcome::Failed {
        cause: format!("compose_and_spawn: {}", e.message),
      };
    }
  };
  // No human watches an auto-start; log any advisories (dropped knobs,
  // deepseek4 KV-blind note, ssd_streaming bypass) to the daemon log.
  for w in &started.warnings {
    log::warn!("proxy auto-start: {w}");
  }

  // Poll the supervisor state machine. 100 ms cadence per the Key
  // Decision; no client-facing timeout — only `Error{cause}` and
  // `Stopping` trigger fallback (Loading waits indefinitely).
  loop {
    match started.model.state().await {
      ManagedState::Ready => {
        // Stamp the MRU now so the freshly-auto-started supervisor
        // has a starting `last_request_at`. Without this its idle
        // timer would only begin when the first proxy forward
        // touched the MRU — and a loaded-but-never-queried model
        // would sit forever with `None` and confuse the sweeper.
        state.mru.touch(&model_id).await;
        return LaunchOutcome::Ready {
          port: started.port,
          model_id,
        };
      }
      ManagedState::Error { cause } => {
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

/// Map a catalog row's GGUF-derived `mode_hint` string onto the launch
/// wire mode so `compose_and_spawn` emits `--embeddings` / `--rerank`
/// when the model needs it. `None` (unknown/absent hint) leaves the
/// chat default in place — this is the seam that regressed embedding
/// auto-start to a 501 before the mode hint was threaded through.
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
fn canonical_id_for_row(row: &CatalogRow) -> Result<ModelId, String> {
  let path = std::path::Path::new(&row.path);
  let header =
    crate::gguf::header::read_path(path, crate::gguf::header::HeaderReadOptions::default())
      .map_err(|e| format!("could not read GGUF header: {e}"))?;
  Ok(crate::gguf::identity::compute(path, &header.raw))
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
    // Absent hint and unrecognised label both leave the chat default to
    // `compose_and_spawn` (None) rather than guessing a mode.
    assert!(launch_mode_from_hint(None).is_none());
    assert!(launch_mode_from_hint(Some("")).is_none());
    assert!(launch_mode_from_hint(Some("unknown")).is_none());
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

  #[test]
  fn canonical_id_for_row_errors_on_missing_file() {
    // A row pointing at a non-existent GGUF returns the wrapped header
    // read error under the "could not read GGUF header" prefix.
    let r = row("/nonexistent/secret-model.gguf", None);
    let err = canonical_id_for_row(&r).expect_err("missing file must error");
    assert!(err.starts_with("could not read GGUF header"), "got: {err}");
  }

  #[test]
  fn canonical_id_for_row_computes_id_for_real_gguf() {
    use crate::gguf::test_fixtures::build_minimal_gguf;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiny.gguf");
    std::fs::write(&path, build_minimal_gguf("llama")).expect("write gguf");
    let r = row(path.to_str().unwrap(), Some("chat"));
    let id = canonical_id_for_row(&r).expect("real gguf resolves");
    assert_eq!(id.path, crate::util::paths::canonicalize(&path).unwrap());
    // Header hash is populated (not the all-zero synthetic placeholder).
    assert_ne!(id.header_blake3, [0u8; 32]);
  }
}
