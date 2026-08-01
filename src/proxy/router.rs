//! Per-request dispatch. `route` is the body of the `service_fn`
//! closure each hyper connection runs — a flat `match` over
//! `(method, path)` for the routes the proxy answers, mirroring the
//! style of [`crate::ipc::methods::dispatch_request`].
//!
//! The route table covers three surfaces:
//!
//! - **OpenAI compat** (`/v1/...`): `/v1/models`, `/v1/chat/completions`,
//!   `/v1/completions`, `/v1/embeddings`, `/v1/rerank`. This is the
//!   primary surface — any agent that speaks the OpenAI REST shape
//!   drives every discovered model through one stable URL here.
//! - **Anthropic compat** (`/v1/messages`, `/v1/messages/count_tokens`):
//!   byte-piped to llama-server's native Anthropic Messages endpoints
//!   (it translates to its OpenAI pipeline internally), so Claude Code
//!   and other Anthropic-shape clients attach via `ANTHROPIC_BASE_URL`.
//! - **Ollama-discovery compat** (`/api/...`, Tier 1): `/api/tags`,
//!   `/api/version`, `/api/ps`, `/api/show`. Read-only projections
//!   of the catalog + supervisor registry into the Ollama wire shape,
//!   added so Ollama-shape discovery libraries (the `ollama-python`
//!   default path, `OLLAMA_HOST` env probes, IDE plugins) recognise
//!   llamastash as Ollama-compatible and fall through to the OpenAI
//!   compat endpoints for actual inference. Tier 2 (`/api/chat`,
//!   `/api/generate`, `/api/embed`) is tracked under TODO §R2.

use std::error::Error as StdError;
use std::sync::Arc;

use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::json;

use super::forward;
use super::ollama_compat::{
  digest_for_path, ModelDetails, PsModel, PsResponse, ShowRequest, ShowResponse, TagModel,
  TagsResponse, VersionResponse, FAR_FUTURE_EXPIRY, UNKNOWN_MTIME,
};
use super::openai::{ErrorObject, ErrorResponse, ModelList, ModelObject};
use super::route::{self, RouteDecision};
use super::state::ProxyState;
use crate::daemon::supervisor::ManagedState;
use crate::discovery::DiscoveredModel;
use crate::gguf::metadata::{ModeHint, ModelMetadata};
use crate::launch::resolve::{resolve_model_with_candidates, CatalogRow, ResolveError};

/// The error type our `BoxBody` carries. Forwarding streams upstream
/// `reqwest::Response::bytes_stream()` chunks through `StreamBody`,
/// so the body alias must accept *some* error type at frame time.
/// Boxed dyn errors are the most flexible choice — non-streaming
/// arms (errors, health, /v1/models) still emit `Infallible`-shaped
/// `Full` bodies and coerce into the wider alias via `map_err`.
pub type BodyError = Box<dyn StdError + Send + Sync>;

/// What every handler returns. `Result<_, hyper::Error>` is the
/// `service_fn` contract; the inner body is boxed so each arm can
/// pick whatever concrete `Body` makes sense without poisoning the
/// outer signature.
pub type ProxyResponse = Result<Response<BoxBody<Bytes, BodyError>>, hyper::Error>;

/// Entry point invoked by the `service_fn` closure. Returns a fully
/// constructed `Response`; the caller hands it back to hyper.
pub async fn route(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let method = req.method().clone();
  let path = req.uri().path().to_string();

  // Bearer auth on the data plane. The liveness / identity probes
  // (`GET|HEAD /`, `GET /health`) stay open so health checks and the
  // Ollama handshake work without a key; everything else (`/v1/*`,
  // `/api/*`, `/ui*`) requires the configured key. When no key is
  // configured `enforced()` is `false` and we skip the check before
  // touching the headers — the loopback default path is unchanged (no
  // added allocation, one boolean test).
  let auth_exempt = matches!(
    (&method, path.as_str()),
    (&Method::GET | &Method::HEAD, "/") | (&Method::GET, "/health")
  );
  // `/ui*` is a browser surface: on auth failure it gets a `Basic`
  // challenge so the browser prompts (the API path keeps `Bearer`).
  // `check` itself accepts both schemes — only the challenge differs.
  let is_ui = path == "/ui" || path.starts_with("/ui/");
  if !auth_exempt && state.auth.enforced() && !state.auth.check(req.headers()) {
    return if is_ui {
      unauthorized_basic()
    } else {
      unauthorized()
    };
  }

  // Route table — OpenAI compat (`/v1/...`) is the primary surface;
  // Ollama compat (`/api/...`, Tier 1) is the discovery surface so
  // Ollama-shape clients recognise the proxy.
  match (&method, path.as_str()) {
    // Server-identity handshake. The official `ollama` CLI (and other
    // Ollama-Go-based clients) issue `HEAD /` before any `/api/*`
    // call; without a 200 on this path they bail with a generic
    // "something went wrong" error. The body is mode-dependent so a
    // user who opts into Ollama drop-in mode gets the byte-exact
    // `"Ollama is running"` string Go-clients sometimes match against.
    (&Method::GET | &Method::HEAD, "/") => root_identity(state),
    (&Method::GET, "/health") => health(state).await,
    (&Method::GET, "/v1/models") => list_models(state).await,
    (&Method::POST, "/v1/chat/completions") => forward_request(state, req).await,
    (&Method::POST, "/v1/completions") => forward_request(state, req).await,
    (&Method::POST, "/v1/embeddings") => forward_request(state, req).await,
    (&Method::POST, "/v1/rerank") => forward_request(state, req).await,
    // Anthropic Messages API. llama-server (b6961+) speaks `/v1/messages`
    // + `/v1/messages/count_tokens` natively, translating to its OpenAI
    // pipeline internally, so the proxy just byte-pipes them like any
    // other `/v1` route — same body-`model` resolution, same streaming.
    // Tool calling on this surface needs the backend launched with
    // `--jinja` (config `jinja: true` by default). Lets Claude Code and
    // other Anthropic-shape clients attach via `ANTHROPIC_BASE_URL`.
    (&Method::POST, "/v1/messages") => forward_request(state, req).await,
    (&Method::POST, "/v1/messages/count_tokens") => forward_request(state, req).await,
    // OpenAI Responses API. Both backends speak it natively — llama-server
    // (`POST /v1/responses` + `/v1/responses/input_tokens`) and ds4-server —
    // so the proxy byte-pipes it like any other `/v1` route (same body-`model`
    // resolution, same streaming). Agents that prefer the Responses surface
    // attach through the one stable proxy URL.
    (&Method::POST, "/v1/responses") => forward_request(state, req).await,
    (&Method::POST, "/v1/responses/input_tokens") => forward_request(state, req).await,
    // Ollama-compat Tier 1: discovery-only endpoints.
    (&Method::GET, "/api/tags") => ollama_tags(state).await,
    (&Method::GET, "/api/version") => ollama_version(),
    (&Method::GET, "/api/ps") => ollama_ps(state).await,
    (&Method::POST, "/api/show") => ollama_show(state, req).await,
    // Web-UI surface. `GET /ui` 302s to `/ui/` (trailing slash so the
    // stock UI's relative base resolves); every `/ui/...` request —
    // any method, for assets + base-relative API calls — delegates to
    // the `ui` module, which strips the prefix and reverse-proxies to
    // the chosen backend. See docs/plans/2026-06-15-001-...-plan.md.
    (&Method::GET, "/ui") => super::ui::redirect_to_ui_slash(),
    (_, p) if p.starts_with("/ui/") => super::ui::serve(state, req).await,
    _ => not_found(),
  }
}

/// `GET / | HEAD /` — server-identity probe. Returns `200 OK` with a
/// `text/plain` body matching real Ollama's `"<server> is running"`
/// pattern so Ollama-Go clients (the official `ollama` CLI, Cline's
/// Ollama provider, IDE plugins built on the Go SDK) recognise the
/// proxy and proceed to `/api/tags`. Hyper drops the body for `HEAD`
/// automatically (the `Content-Length` header still reflects the
/// would-be body length, matching real Ollama).
fn root_identity(state: Arc<ProxyState>) -> ProxyResponse {
  // Trailing newline matches `curl http://127.0.0.1:11434/` against
  // a real Ollama install — small detail, but agents that strcmp the
  // body see the same bytes.
  let body = if state.ollama_compat {
    "Ollama is running\n"
  } else {
    "LlamaStash is running\n"
  };
  Ok(text_response(StatusCode::OK, body))
}

/// 200-OK `text/plain` response. Carved out so the root-identity
/// handler doesn't reach for `json_response` (wrong content-type) and
/// future plain-text surfaces have an obvious helper to share.
fn text_response(status: StatusCode, body: &'static str) -> Response<BoxBody<Bytes, BodyError>> {
  Response::builder()
    .status(status)
    .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
    .body(full_body(Bytes::from_static(body.as_bytes())))
    .expect("static text response must build")
}

/// Forwarding pipeline: buffer the body under the 2 MiB cap, extract
/// `body.model`, run the resolver, pick a Ready supervisor, forward.
async fn forward_request(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let (method, uri, headers, body) = forward::deconstruct(req);

  let parsed = match route::buffer_and_extract(body).await {
    Ok(p) => p,
    Err(e) => return route::body_error_response(e),
  };

  let decision = route::decide(&state, parsed.model).await;
  // Some backends serve chat/completions but not embeddings/rerank. Refuse such
  // a request bound for a backend that doesn't serve the mode with a clear JSON
  // error instead of forwarding into the backend's bare 404 — covering both a
  // *running* model (its real backend) and a *not-running* model that would
  // auto-start (its would-be backend). Registry-driven via `serves_mode`.
  use crate::backend::Backend;
  let req_path = uri.path().to_string();
  let req_mode = match req_path.as_str() {
    "/v1/embeddings" => Some(crate::launch::mode::LaunchMode::Embedding),
    "/v1/rerank" => Some(crate::launch::mode::LaunchMode::Rerank),
    _ => None,
  };
  if let Some(mode) = req_mode {
    let target = match &decision {
      RouteDecision::ReadyAt {
        served_model_key, ..
      } => route::running_model_backend(&state, served_model_key).await,
      RouteDecision::NotRunning { resolved_row, .. } => {
        route::would_route_backend(&state, resolved_row).await
      }
      _ => None,
    };
    if let Some(b) = target {
      if !b.serves_mode(mode) {
        // Name the actual backend for the user (dynamic, so the code names none).
        let msg = format!(
          "the {} backend serves chat/completions only, not embeddings or rerank; \
           launch an embedding-capable model for this endpoint",
          b.id()
        );
        return error_response(StatusCode::BAD_REQUEST, "unsupported_endpoint", &msg);
      }
    }
  }
  // Both the Ready and NotRunning arms need the same inbound bundle;
  // build it once before the match so the two arms stay short and the
  // forward path sees one canonical shape.
  let inbound = forward::InboundRequest {
    method,
    uri,
    headers,
    body_bytes: parsed.bytes,
  };
  match decision {
    RouteDecision::ReadyAt {
      port,
      served_model_id,
      served_model_key,
      upstream_path_prefix,
      fallback,
      fallback_reason,
    } => {
      // MRU touch on the Ready path. Stamped *before* forwarding
      // begins (per the plan's "as it starts forwarding, not on
      // completion" rule) so long-running streams don't delay the
      // timestamp. Direct touch by ModelId avoids the second
      // supervisor snapshot the port-only path used to take.
      state.mru.touch(&served_model_key).await;
      forward::forward_to_upstream(
        &state,
        inbound,
        forward::Target {
          port,
          served_model_id: &served_model_id,
          served_model_key: &served_model_key,
          upstream_path_prefix: upstream_path_prefix.as_deref(),
          fallback,
          fallback_reason: fallback_reason.as_deref(),
        },
      )
      .await
    }
    RouteDecision::NotRunning {
      requested_model,
      resolved_row,
      arch,
    } => route::handle_not_running(&state, inbound, requested_model, *resolved_row, arch).await,
    RouteDecision::NotFound { requested_model } => error_with_matches(
      StatusCode::NOT_FOUND,
      "model_not_found",
      &format!("{requested_model} not found"),
      Vec::<String>::new(),
    ),
    RouteDecision::Ambiguous {
      requested_model,
      candidates,
    } => {
      let message = format!(
        "`{requested_model}` matched {n} models; refine the reference (full path or unique substring)",
        n = candidates.len()
      );
      error_with_matches(
        StatusCode::BAD_REQUEST,
        "ambiguous_model",
        &message,
        candidates,
      )
    }
    RouteDecision::ModelRequired => error_with_code(
      StatusCode::BAD_REQUEST,
      "invalid_request",
      "the `model` field is required",
      "model_required",
      Some("model"),
    ),
    RouteDecision::BackendUnavailable {
      backend,
      requested_model,
    } => error_response(
      StatusCode::SERVICE_UNAVAILABLE,
      "backend_unavailable",
      &format!(
        "`{requested_model}` is served by the {backend} backend, but the llamastash managed \
         instance is not running; set up {backend} and start the daemon with `--lemonade` \
         (see docs/lemonade-setup.md)"
      ),
    ),
  }
}

async fn health(state: Arc<ProxyState>) -> ProxyResponse {
  // Contract: this handler always returns 200 when the listener is
  // bound — even if zero models are Ready. The status of the
  // listener itself (Disabled / Listening / PortInUse / Unbound) is
  // surfaced via the IPC `status.proxy` block, not via the HTTP
  // health probe. If a future caller wants a real "is the proxy
  // serving usefully" signal (e.g. for a systemd readiness gate or a
  // load-balancer probe), introduce a second endpoint rather than
  // overloading `/health` — agents that already pin against 200 on
  // this path would break otherwise. See
  // docs/plans/2026-05-21-001-feat-proxy-router-plan.md (Key
  // Decisions: /health always-200).
  //
  // `models_loaded` filters the supervisor snapshot to entries
  // currently in `ManagedState::Ready`. `models_discovered` is the
  // catalog length — discovery surfaces every row, even parse-error
  // rows, so this matches what `/v1/models` returns.
  let models_loaded = count_ready(&state).await;
  let models_discovered = state.ctx.catalog.len().await;
  let body = json!({
    "status": "ok",
    "models_loaded": models_loaded,
    "models_discovered": models_discovered,
  });
  // serde_json::to_vec on a hand-built `Value` cannot fail.
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// Count supervisors currently in `ManagedState::Ready`. Each
/// `state()` call acquires a per-supervisor read lock, so the
/// snapshot is a sequence of cheap clones rather than one global
/// lock — consistent with how `status_handler` walks the registry.
async fn count_ready(state: &ProxyState) -> usize {
  let snap = state.ctx.supervisors.snapshot().await;
  let mut ready = 0usize;
  for (_id, model) in snap {
    if matches!(model.state().await, ManagedState::Ready) {
      ready += 1;
    }
  }
  ready
}

/// `GET /v1/models` — list every discovered model in OpenAI shape,
/// sorted alphabetically by `id`. Empty catalog returns
/// `{"object":"list","data":[]}` (not a 404, not an error).
async fn list_models(state: Arc<ProxyState>) -> ProxyResponse {
  let snap = state.ctx.catalog.snapshot().await;
  let mut rows: Vec<ModelObject> = snap
    .iter()
    .map(|m| ModelObject::new(model_id_for(m)))
    .collect();
  // ASCII-lexicographic sort: stable, deterministic across runs, and
  // independent of the catalog's underlying BTreeMap key (canonical
  // path) which orders by filesystem layout instead of display name.
  rows.sort_by(|a, b| a.id.cmp(&b.id));
  let list = ModelList::new(rows);
  let bytes = serde_json::to_vec(&list).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// Project a [`DiscoveredModel`] onto the `id` field of an OpenAI
/// `model` object. Rule: `display_label` wins when set (Ollama
/// surfaces `<name>:<tag>` here), otherwise fall back to
/// `path.file_stem()` via [`crate::util::paths::model_display_name`].
/// This matches what the TUI and `llamastash list` show, so the same
/// model identifier appears in every surface.
///
/// Note: `CatalogRow::name()` falls back to `path.file_name()`
/// (basename *with* extension) rather than the file stem. We use the
/// stem here so the OpenAI `id` reads cleanly (`qwen2.5-coder` rather
/// than `qwen2.5-coder.gguf`). The resolver's substring matching is
/// tolerant to either form, so this divergence is intentional and
/// bounded.
fn model_id_for(m: &DiscoveredModel) -> String {
  if let Some(label) = &m.display_label {
    return label.clone();
  }
  crate::util::paths::model_display_name(&m.path)
}

// === Ollama-compat handlers ============================================
//
// Tier 1: read-only discovery endpoints. All four share `model_id_for`
// with the OpenAI compat surface so a model has a single user-visible
// identifier no matter which API a client probes against.

/// `GET /api/tags` — list every discovered model in Ollama shape,
/// sorted alphabetically by `name` (parity with `/v1/models`). Empty
/// catalog returns `{"models":[]}` (not a 404).
async fn ollama_tags(state: Arc<ProxyState>) -> ProxyResponse {
  let snap = state.ctx.catalog.snapshot().await;
  let mut models: Vec<TagModel> = snap.iter().map(ollama_tag_from_discovered).collect();
  models.sort_by(|a, b| a.name.cmp(&b.name));
  let body = TagsResponse { models };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// `GET /api/version` — daemon build version. Same value
/// `status.daemon.build` surfaces; cargo's `CARGO_PKG_VERSION` at
/// compile time.
fn ollama_version() -> ProxyResponse {
  let body = VersionResponse {
    version: env!("CARGO_PKG_VERSION"),
  };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// `GET /api/ps` — currently-Ready supervisors projected into Ollama's
/// running-list shape. Empty when no model is Ready.
async fn ollama_ps(state: Arc<ProxyState>) -> ProxyResponse {
  let sup_snap = state.ctx.supervisors.snapshot().await;
  let cat_snap = state.ctx.catalog.snapshot().await;
  let by_path = route::index_catalog_by_path(&cat_snap);
  let mut models: Vec<PsModel> = Vec::new();
  for (launch_id, sup) in sup_snap.into_iter() {
    // An infrastructure launch (a managed-multiplexer umbrella) is a supervised
    // process, not a servable model — exclude it from /api/ps just as the
    // fallback and /ui walkers do.
    if crate::backend::is_infra_launch(&launch_id) {
      continue;
    }
    if !matches!(sup.state().await, ManagedState::Ready) {
      continue;
    }
    let id = sup.id().clone();
    let path_key = id.path.to_string_lossy().into_owned();
    let name = if let Some(d) = by_path.get(&path_key) {
      model_id_for(d)
    } else {
      crate::util::paths::model_display_name(&id.path)
    };
    let details = by_path
      .get(&path_key)
      .map(|d| ollama_details_from_metadata(d.metadata.as_ref()))
      .unwrap_or_else(ollama_details_unknown);
    let size = by_path
      .get(&path_key)
      .and_then(|d| d.metadata.as_ref())
      .and_then(|m| m.weights_bytes)
      .unwrap_or(0);
    models.push(PsModel {
      name: name.clone(),
      model: name,
      size,
      // Path-derived digest, identical to what /api/tags emits for
      // the same model — see `ollama_compat::digest_for_path`.
      digest: digest_for_path(&id.path),
      details,
      expires_at: FAR_FUTURE_EXPIRY.to_string(),
      size_vram: 0,
    });
  }
  models.sort_by(|a, b| a.name.cmp(&b.name));
  let body = PsResponse { models };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(StatusCode::OK, bytes))
}

/// `POST /api/show` — return per-model metadata for the model named
/// in the request body. Body shape: `{"model": "<name>"}` (or `"name"`
/// for older clients — both spellings accepted). Resolution rules
/// match the OpenAI compat surface — same fuzzy matcher as
/// `body.model` on `/v1/...` so a name that works on one endpoint
/// works on the other.
async fn ollama_show(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let (_method, _uri, _headers, body) = forward::deconstruct(req);
  let parsed = match route::buffer_and_extract(body).await {
    Ok(p) => p,
    Err(e) => return route::body_error_response(e),
  };
  // Re-parse the body bytes into the Ollama-shape ShowRequest. The
  // proxy's `JustModel` peek picked out `body.model`; for /api/show we
  // also tolerate `body.name`, so we re-parse rather than reuse
  // `parsed.model`.
  let show_req: ShowRequest = if parsed.bytes.is_empty() {
    ShowRequest {
      model: None,
      name: None,
    }
  } else {
    match serde_json::from_slice(&parsed.bytes) {
      Ok(r) => r,
      Err(e) => {
        return error_response(
          StatusCode::BAD_REQUEST,
          "invalid_request",
          &format!("request body is not valid JSON: {e}"),
        );
      }
    }
  };
  let reference = match show_req.reference() {
    Some(r) => r.to_string(),
    None => {
      return error_with_code(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "the `model` field is required",
        "model_required",
        Some("model"),
      );
    }
  };
  // Resolve against the catalog using the same matcher the OpenAI
  // compat surface uses, so identical names work across both APIs.
  let snap = state.ctx.catalog.snapshot().await;
  let rows: Vec<CatalogRow> = snap
    .iter()
    .map(route::catalog_row_from_discovered)
    .collect::<Vec<_>>();
  match resolve_model_with_candidates(&rows, &reference) {
    Ok(resolved) => {
      // Re-find the DiscoveredModel for the resolved path so we have
      // the live metadata. The resolver returns a CatalogRow clone;
      // metadata projection wants the source DiscoveredModel for the
      // ModelId.
      let discovered = snap
        .iter()
        .find(|m| m.path.to_string_lossy() == resolved.path)
        .cloned();
      let response = build_show_response(discovered.as_ref(), &resolved);
      let bytes = serde_json::to_vec(&response).expect("json encoding of fixed shape");
      Ok(json_response(StatusCode::OK, bytes))
    }
    Err(ResolveError::Empty) | Err(ResolveError::None) => error_with_matches(
      StatusCode::NOT_FOUND,
      "model_not_found",
      &format!("{reference} not found"),
      Vec::<String>::new(),
    ),
    Err(ResolveError::Many(candidates)) => {
      let names: Vec<String> = candidates.iter().map(|r| r.name()).collect();
      let n = names.len();
      error_with_matches(
        StatusCode::BAD_REQUEST,
        "ambiguous_model",
        &format!(
          "`{reference}` matched {n} models; refine the reference (full path or unique substring)"
        ),
        names,
      )
    }
  }
}

/// Build an Ollama `details` block from optional GGUF metadata. Empty
/// strings stand in for fields we can't fill (parse-error rows, models
/// missing arch/parameter-label metadata).
fn ollama_details_from_metadata(meta: Option<&ModelMetadata>) -> ModelDetails {
  match meta {
    Some(m) => {
      let family = m.arch.clone().unwrap_or_default();
      let families = if family.is_empty() {
        Vec::new()
      } else {
        vec![family.clone()]
      };
      ModelDetails {
        parent_model: String::new(),
        format: "gguf",
        family,
        families,
        parameter_size: m.parameter_label.clone().unwrap_or_default(),
        quantization_level: m.quant.label().to_string(),
      }
    }
    None => ollama_details_unknown(),
  }
}

/// Empty-shape details for catalog rows where the GGUF header parse
/// failed.
fn ollama_details_unknown() -> ModelDetails {
  ModelDetails {
    parent_model: String::new(),
    format: "gguf",
    family: String::new(),
    families: Vec::new(),
    parameter_size: String::new(),
    quantization_level: "Unknown".to_string(),
  }
}

/// Project a [`DiscoveredModel`] onto an Ollama `/api/tags` row.
fn ollama_tag_from_discovered(m: &DiscoveredModel) -> TagModel {
  let name = model_id_for(m);
  let size = m
    .metadata
    .as_ref()
    .and_then(|md| md.weights_bytes)
    .unwrap_or(0);
  // Path-derived digest, identical to what /api/ps emits for the
  // same model. Parse-error rows hash their path too — clients see
  // a stable, comparable value rather than a special-case sentinel.
  let digest = digest_for_path(&m.path);
  TagModel {
    name: name.clone(),
    model: name,
    modified_at: UNKNOWN_MTIME.to_string(),
    size,
    digest,
    details: ollama_details_from_metadata(m.metadata.as_ref()),
  }
}

/// Build the `/api/show` response from a resolved catalog row +
/// optional source discovery row (for the `model_info` slot).
fn build_show_response(discovered: Option<&DiscoveredModel>, row: &CatalogRow) -> ShowResponse {
  let template = discovered
    .and_then(|d| d.metadata.as_ref())
    .and_then(|m| m.chat_template.clone())
    .unwrap_or_default();
  let details = ollama_details_from_metadata(discovered.and_then(|d| d.metadata.as_ref()));
  let model_info = build_model_info(discovered, row);
  let capabilities = capabilities_for(discovered);
  ShowResponse {
    modelfile: String::new(),
    parameters: String::new(),
    template,
    details,
    model_info,
    capabilities,
  }
}

/// Build the `model_info` flat map. Mirrors the GGUF metadata slots
/// Ollama clients commonly read.
fn build_model_info(
  discovered: Option<&DiscoveredModel>,
  row: &CatalogRow,
) -> serde_json::Map<String, serde_json::Value> {
  let mut map = serde_json::Map::new();
  if let Some(arch) = &row.arch {
    map.insert("general.architecture".into(), json!(arch));
  }
  if let Some(meta) = discovered.and_then(|d| d.metadata.as_ref()) {
    if let Some(n) = meta.total_parameters {
      map.insert("general.parameter_count".into(), json!(n));
    }
    if let Some(label) = &meta.parameter_label {
      map.insert("general.parameter_label".into(), json!(label));
    }
    if let Some(ctx) = meta.native_ctx {
      map.insert("general.context_length".into(), json!(ctx));
    }
    if let Some(tok) = &meta.tokenizer_kind {
      map.insert("tokenizer.ggml.model".into(), json!(tok));
    }
    map.insert("general.quantization".into(), json!(meta.quant.label()));
  }
  map
}

/// Derive the Ollama-shape `capabilities` list from the discovered
/// model's mode hint. Ollama uses `"completion"` for chat/text models,
/// `"embedding"` for embedding models, `"tools"` for tool-use-capable
/// chat models. llamastash maps from [`ModeHint`]:
///   - `Chat` → `["completion"]`
///   - `Embedding` → `["embedding"]`
///   - `Rerank` → `["rerank"]` (Ollama doesn't define this; emitted
///     for parity with our own `/v1/rerank` surface)
///   - `Unknown` → empty (catalog has no hint)
fn capabilities_for(discovered: Option<&DiscoveredModel>) -> Vec<&'static str> {
  let mode = discovered
    .and_then(|d| d.metadata.as_ref())
    .map(|m| m.mode_hint)
    .unwrap_or(ModeHint::Unknown);
  match mode {
    ModeHint::Chat => vec!["completion"],
    ModeHint::Embedding => vec!["embedding"],
    ModeHint::Rerank => vec!["rerank"],
    ModeHint::Unknown => Vec::new(),
  }
}

fn not_found() -> ProxyResponse {
  error_response(StatusCode::NOT_FOUND, "not_found", "no such route")
}

/// `401 Unauthorized` for the data plane when bearer auth is enforced
/// and the request's `Authorization` header is missing or wrong.
/// OpenAI-shaped body (so SDK clients surface it as an auth error)
/// plus a `WWW-Authenticate: Bearer` challenge. The message is
/// deliberately generic — it never echoes the supplied token.
fn unauthorized() -> ProxyResponse {
  let mut resp = json_response(StatusCode::UNAUTHORIZED, unauthorized_body());
  resp.headers_mut().insert(
    hyper::header::WWW_AUTHENTICATE,
    hyper::header::HeaderValue::from_static("Bearer"),
  );
  Ok(resp)
}

/// `401 Unauthorized` for the `/ui*` browser surface. Same OpenAI-shaped
/// body as [`unauthorized`], but the challenge is `Basic` so a browser
/// navigating to `/ui` pops its native credential prompt; the user
/// pastes the proxy key as the password and the browser resends it
/// per-origin. See the proxy-UI plan §"LAN /ui + browser auth".
fn unauthorized_basic() -> ProxyResponse {
  let mut resp = json_response(StatusCode::UNAUTHORIZED, unauthorized_body());
  resp.headers_mut().insert(
    hyper::header::WWW_AUTHENTICATE,
    hyper::header::HeaderValue::from_static("Basic realm=\"llamastash\""),
  );
  Ok(resp)
}

/// Shared 401 body for both challenge variants. Deliberately generic —
/// it never echoes the supplied credential.
fn unauthorized_body() -> Vec<u8> {
  serde_json::to_vec(&ErrorResponse {
    error: ErrorObject::new("invalid_request_error", "missing or invalid API key")
      .with_code("invalid_api_key"),
  })
  .expect("json encoding of fixed shape")
}

/// Build an OpenAI-shaped error response from a `(status, type,
/// message)` triple. Centralised so the 404 / `model_not_running`
/// arms all emit the same
/// `{"error":{"type":..., "message":...}}` envelope.
pub(crate) fn error_response(status: StatusCode, r#type: &str, message: &str) -> ProxyResponse {
  let body = ErrorResponse {
    error: ErrorObject::new(r#type, message),
  };
  let bytes = serde_json::to_vec(&body).expect("json encoding of fixed shape");
  Ok(json_response(status, bytes))
}

/// Variant of [`error_response`] that stamps `code` (e.g.
/// `"model_required"`) + `param` (e.g. `"model"`). Used by the
/// `invalid_request` arm so OpenAI SDK clients can branch on `code`
/// without parsing `message`.
pub(crate) fn error_with_code(
  status: StatusCode,
  r#type: &str,
  message: &str,
  code: &str,
  param: Option<&str>,
) -> ProxyResponse {
  let mut error = ErrorObject::new(r#type, message).with_code(code);
  if let Some(p) = param {
    error = error.with_param(p);
  }
  let bytes = serde_json::to_vec(&ErrorResponse { error }).expect("json encoding of fixed shape");
  Ok(json_response(status, bytes))
}

/// Variant of [`error_response`] that stamps the candidate-name
/// `matches` list. Used by `model_not_found` (empty list) and
/// `ambiguous_model` (≥ 2 names).
pub(crate) fn error_with_matches<I, S>(
  status: StatusCode,
  r#type: &str,
  message: &str,
  matches: I,
) -> ProxyResponse
where
  I: IntoIterator<Item = S>,
  S: Into<String>,
{
  let error = ErrorObject::new(r#type, message).with_matches(matches);
  let bytes = serde_json::to_vec(&ErrorResponse { error }).expect("json encoding of fixed shape");
  Ok(json_response(status, bytes))
}

pub(crate) fn json_response(
  status: StatusCode,
  body: Vec<u8>,
) -> Response<BoxBody<Bytes, BodyError>> {
  let body = full_body(Bytes::from(body));
  Response::builder()
    .status(status)
    .header(hyper::header::CONTENT_TYPE, "application/json")
    .body(body)
    .expect("static headers always parse")
}

/// Wrap an in-memory `Bytes` payload as a `BoxBody` whose error
/// type is the wider `BodyError` alias. `Full`'s native error is
/// `Infallible`; `map_err` widens it (via the `From<Infallible>`
/// blanket impl) so non-streaming arms share the streaming arm's
/// body alias.
fn full_body(bytes: Bytes) -> BoxBody<Bytes, BodyError> {
  Full::new(bytes).map_err(|never| match never {}).boxed()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::discovery::{DiscoveredModel, ModelSource};
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

  fn discovered_with_mode(mode: ModeHint) -> DiscoveredModel {
    DiscoveredModel {
      path: std::path::PathBuf::from("/tmp/fake.gguf"),
      parent: std::path::PathBuf::from("/tmp"),
      source: ModelSource::HuggingFace,
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
      parse_error: None,
      split_siblings: vec![],
      metadata: Some(ModelMetadata {
        arch: Some("llama".into()),
        quant: Quant::Q4_K,
        native_ctx: Some(4096),
        parameter_label: Some("7B".into()),
        weights_bytes: Some(100),
        mtp: None,
        chat_template: None,
        tokenizer_kind: Some("llama".into()),
        total_parameters: Some(7_000_000_000),
        reasoning_hint: false,
        mode_hint: mode,
      }),
    }
  }

  #[test]
  fn capabilities_for_maps_each_mode_hint() {
    // Ollama-shape capabilities derived from the GGUF mode hint. Chat →
    // completion, embedding/rerank pass through verbatim.
    assert_eq!(
      capabilities_for(Some(&discovered_with_mode(ModeHint::Chat))),
      vec!["completion"]
    );
    assert_eq!(
      capabilities_for(Some(&discovered_with_mode(ModeHint::Embedding))),
      vec!["embedding"]
    );
    assert_eq!(
      capabilities_for(Some(&discovered_with_mode(ModeHint::Rerank))),
      vec!["rerank"]
    );
  }

  #[test]
  fn capabilities_for_is_empty_without_hint() {
    // Unknown mode and a `None` discovery row (or one with no metadata)
    // both produce an empty capability list — the catalog has no signal.
    assert!(capabilities_for(Some(&discovered_with_mode(ModeHint::Unknown))).is_empty());
    assert!(capabilities_for(None).is_empty());

    let mut no_meta = discovered_with_mode(ModeHint::Chat);
    no_meta.metadata = None;
    assert!(capabilities_for(Some(&no_meta)).is_empty());
  }
}
