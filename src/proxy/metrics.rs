//! `GET /metrics`: the proxy's own request numbers, as Prometheus text or as
//! JSON.
//!
//! Every number comes from the in-memory request log
//! ([`super::request_log`]), so counters count since daemon start and nothing
//! is persisted across a restart — `requests.jsonl` stays the durable record.
//! The route never contacts a backend; the engines' own numbers are the
//! separate [`super::metrics_backend`] route.
//!
//! Metric names are `llamastash:*`, one prefix of our own beside whatever the
//! engines behind us export; theirs come through the backend route untouched.
//! There are no `gen_ai.*` aliases, no histograms, and one label set per metric name. `model` and
//! `route` are the only labels a default scrape carries; the per-launch series
//! are opt-in with `?launch=1` because launch ids are never reused.
//!
//! Plan: docs/plans/2026-10-11-001-feat-proxy-metrics-plan.md.

use std::collections::HashMap;
use std::sync::Arc;

use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderMap, CONTENT_TYPE};
use hyper::{Request, Response, StatusCode};
use serde_json::{Map, Value};

use super::request_log::{
  clamp_limit, Clients, LaunchTotals, LogScope, LogSnapshot, RequestSummary, ScopeTotals, CAPACITY,
};
use super::route;
use super::router::{self, ProxyResponse};
use super::state::ProxyState;
use crate::daemon::registry::LaunchId;
use crate::daemon::supervisor::ManagedState;
use crate::gguf::identity::ModelId;
use crate::util::datetime;

/// The media type a Prometheus scrape expects for text format 0.0.4.
pub(crate) const TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Which body a metrics route answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
  Text,
  Json,
}

/// A metrics request the proxy refuses, in the shape each route turns into its
/// own OpenAI-shaped error envelope.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reject {
  /// `?format=` named something other than the two accepted values.
  BadFormat(String),
  /// The `Accept` header matched neither candidate type.
  NotAcceptable(String),
  /// `?model=` / `?launch=` named no model and no launch.
  UnknownRef(String),
  /// `?model=` and `?launch=` name a model and a launch that does not serve it.
  ConflictingScope { model: String, launch: String },
  /// A `?model=` reference that matches more than one model and cannot be
  /// settled.
  Ambiguous(String),
  /// `?launch=` named a model more than one live launch is serving, so the
  /// reference has no single answer.
  AmbiguousLaunch {
    value: String,
    launches: Vec<String>,
  },
}

impl Reject {
  pub(crate) fn into_response(self) -> ProxyResponse {
    match self {
      Reject::BadFormat(v) => router::error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        &format!("unknown format `{v}`; use `prometheus` or `json`"),
      ),
      Reject::NotAcceptable(v) => router::error_response(
        StatusCode::NOT_ACCEPTABLE,
        "not_acceptable",
        &format!(
          "Accept `{v}` matches neither text/plain nor application/json; \
           send `Accept: application/json` for JSON or `?format=json`"
        ),
      ),
      Reject::UnknownRef(v) => router::error_response(
        StatusCode::NOT_FOUND,
        "not_found",
        &format!("no model or launch matches `{v}`"),
      ),
      Reject::ConflictingScope { model, launch } => router::error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        &format!("launch `{launch}` does not serve model `{model}`"),
      ),
      Reject::Ambiguous(v) => router::error_response(
        StatusCode::BAD_REQUEST,
        "ambiguous_model",
        &format!("`{v}` matches more than one model; scope it with a launch or a full path"),
      ),
      Reject::AmbiguousLaunch { value, launches } => router::error_response(
        StatusCode::BAD_REQUEST,
        "ambiguous_launch",
        &format!(
          "`{value}` is served by {} live launches ({}); name one by its id or port",
          launches.len(),
          launches.join(", ")
        ),
      ),
    }
  }
}

/// Query parameters both metrics routes read.
#[derive(Debug, Default)]
pub(crate) struct Query {
  /// `?format=json` / `?format=prometheus`.
  pub(crate) format: Option<String>,
  /// `?limit=` for the JSON row list.
  pub(crate) limit: Option<usize>,
  /// `?model=<ref>`.
  pub(crate) model: Option<String>,
  /// `?launch=<ref>`, or `?launch=1` for the per-launch series.
  pub(crate) launch: Option<String>,
}

/// Values of `launch=` that mean "include the per-launch series" rather than
/// "scope to this launch". A launch id is `L<n>`, so `1` can never name one.
const PER_LAUNCH_FLAGS: [&str; 3] = ["1", "true", "yes"];

fn is_per_launch_flag(value: &str) -> bool {
  PER_LAUNCH_FLAGS.contains(&value)
}

impl Query {
  pub(crate) fn parse(query: Option<&str>) -> Self {
    let mut out = Query::default();
    for (key, value) in pairs(query) {
      match key.as_str() {
        "format" => out.format = Some(value),
        "limit" => out.limit = value.parse().ok(),
        "model" => out.model = Some(value),
        "launch" => out.launch = Some(value),
        _ => {}
      }
    }
    out
  }

  /// `?launch=1`: the caller asked for the per-launch series.
  pub(crate) fn per_launch(&self) -> bool {
    self.launch.as_deref().is_some_and(is_per_launch_flag)
  }

  /// The `launch=` value read as a launch reference, `None` for the flag form.
  fn launch_ref(&self) -> Option<&str> {
    self.launch.as_deref().filter(|v| !is_per_launch_flag(v))
  }

  /// Row limit for the JSON shape, clamped to what the log can hold.
  pub(crate) fn limit(&self) -> usize {
    clamp_limit(self.limit)
  }
}

/// Query string as `(key, value)` pairs with percent-escapes and `+` undone. A
/// pair with no `=` has an empty value.
fn pairs(query: Option<&str>) -> Vec<(String, String)> {
  query
    .unwrap_or_default()
    .split('&')
    .filter(|pair| !pair.is_empty())
    .map(|pair| {
      let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
      (decode(key), decode(value))
    })
    .collect()
}

/// Percent-decode one query token: `%XX` runs (a multi-byte character is a run
/// of them), `+` for a space, `%2B` for a literal plus. A stray `%` that does
/// not start a hex pair is kept.
fn decode(raw: &str) -> String {
  let bytes = raw.as_bytes();
  let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    let escaped = if bytes[i] == b'%' && i + 2 < bytes.len() {
      std::str::from_utf8(&bytes[i + 1..i + 3])
        .ok()
        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
    } else {
      None
    };
    match escaped {
      Some(byte) => {
        out.push(byte);
        i += 3;
      }
      None => {
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
      }
    }
  }
  // A decoded value can be arbitrary bytes; lossy keeps the route answering
  // rather than failing on a malformed escape.
  String::from_utf8_lossy(&out).into_owned()
}

/// Best `q` the two candidate types get from an `Accept` header, as
/// `(json, text)` where `0.0` means not accepted.
///
/// Only an exact `application/json` selects the JSON view, and a `text/plain`
/// (or the `text/*` family) selects the text one. A `;` parameter that is not
/// `q` (`version=0.0.4`, `charset=…`) does not affect the match and a missing
/// `q` is `1.0`. `*/*` means "whatever you have": it speaks only for a
/// candidate the header never named by its own name, and then one step below
/// the wildcard's own q, so a naive `curl` and a Prometheus scraper both get
/// text without ever lifting a named type above the q it carried. Anything
/// else — `image/*`, `application/*` — asks for a family neither candidate is
/// in, which is no answer at all.
fn accept_q(header: &str) -> (f32, f32) {
  let mut best = (0.0f32, 0.0f32);
  let mut named = (false, false);
  let mut wildcard: Option<f32> = None;
  for entry in header.split(',') {
    let mut parts = entry.trim().split(';');
    let Some(syntax) = parts.next().map(|s| s.trim().to_ascii_lowercase()) else {
      continue;
    };
    let mut q = 1.0f32;
    for param in parts {
      let Some((name, value)) = param.split_once('=') else {
        continue;
      };
      if name.trim().eq_ignore_ascii_case("q") {
        q = value.trim().parse().unwrap_or(0.0);
      }
    }
    if q <= 0.0 {
      // A refusal is still an answer: it takes that type out of the running for
      // the whole header, including against a wildcard. `text/plain;q=0, */*`
      // means "not text", so the wildcard may not hand text back.
      if syntax == "application/json" {
        named.0 = true;
      }
      if syntax == "text/plain" || syntax == "text/*" {
        named.1 = true;
      }
      continue;
    }
    if syntax == "application/json" {
      named.0 = true;
      best.0 = best.0.max(q);
    }
    if syntax == "text/plain" || syntax == "text/*" {
      named.1 = true;
      best.1 = best.1.max(q);
    }
    if syntax == "*/*" {
      wildcard = Some(wildcard.map_or(q, |w| w.max(q)));
    }
  }
  // RFC 9110's specificity rule: a named `*/*` supplies its q only to a media
  // type the header has no entry of its own for, and never raises one the
  // client priced for itself — `text/plain;q=0.1, */*;q=0.5` must not smuggle
  // text above JSON's own 0.3. One step below the wildcard's q keeps a bare
  // `*/*` inside the tie that goes to text.
  if let Some(w) = wildcard {
    let joined = below(w);
    if !named.0 {
      best.0 = best.0.max(joined);
    }
    if !named.1 {
      best.1 = best.1.max(joined);
    }
  }
  best
}

/// The largest `f32` below `x`, so a wildcard accepts both candidates one step
/// under the q it carries and so loses to that type outright.
fn below(x: f32) -> f32 {
  f32::from_bits(x.to_bits().wrapping_sub(1))
}

/// The response format: `?format=` beats `Accept`, which beats the default of
/// Prometheus text. A wildcard must never select JSON, and a tie goes to text.
pub(crate) fn choose_format(query: &Query, headers: &HeaderMap) -> Result<Format, Reject> {
  if let Some(format) = &query.format {
    return match format.as_str() {
      "json" => Ok(Format::Json),
      "prometheus" => Ok(Format::Text),
      other => Err(Reject::BadFormat(other.to_string())),
    };
  }
  let Some(accept) = headers
    .get(hyper::header::ACCEPT)
    .and_then(|v| v.to_str().ok())
    .filter(|v| !v.trim().is_empty())
  else {
    return Ok(Format::Text);
  };
  let (json, text) = accept_q(accept);
  if json == 0.0 && text == 0.0 {
    return Err(Reject::NotAcceptable(accept.to_string()));
  }
  Ok(if json > text {
    Format::Json
  } else {
    Format::Text
  })
}

/// What one scrape covers.
#[derive(Debug)]
pub(crate) struct Scope {
  /// The request-log scope to read the summary and rows from.
  pub(crate) log: LogScope,
  /// Restrict the per-model series to this catalog path.
  pub(crate) path: Option<String>,
  /// Restrict the per-launch series to this launch id.
  pub(crate) launch: Option<String>,
  /// Display name of the model the scope names, for the JSON `scope` block.
  pub(crate) name: Option<String>,
  /// The per-launch series are in the response.
  pub(crate) per_launch: bool,
}

impl Default for Scope {
  fn default() -> Self {
    Scope {
      log: LogScope::All,
      path: None,
      launch: None,
      name: None,
      per_launch: false,
    }
  }
}

impl Scope {
  /// Whether a per-model entry is in the view.
  pub(crate) fn keeps_model(&self, path: &str) -> bool {
    self.path.as_deref().is_none_or(|want| want == path)
  }

  /// Whether a launch is in the view, given the path it served (`None` when the
  /// scrape cannot tell).
  pub(crate) fn keeps_launch(&self, launch: &str, path: Option<&str>) -> bool {
    if let Some(want) = &self.launch {
      if want != launch {
        return false;
      }
    }
    match (&self.path, path) {
      (Some(want), Some(path)) => want == path,
      // A retired launch with no rows left in the ring cannot be matched to the
      // model, so a model-scoped scrape leaves it out rather than guessing.
      (Some(_), None) => false,
      (None, _) => true,
    }
  }
}

/// One live launch as a metrics scrape sees it.
#[derive(Clone)]
pub(crate) struct LiveLaunch {
  pub(crate) id: String,
  /// Display name of the model it serves.
  pub(crate) model: String,
  /// Full identity of the model it serves, for callers that need more than the
  /// path (`/ui` keys its in-flight guard on it).
  pub(crate) model_id: ModelId,
  /// `model_id.path` again as a plain string. Duplication on purpose: batch 2
  /// rewrites the `l.path == …` comparisons, and until it merges every caller
  /// keeps working off the string. Dropped once that lands.
  pub(crate) path: String,
  pub(crate) port: u16,
  pub(crate) ready: bool,
  /// What the launch was started for. `/ui` needs it to tell a chat launch
  /// (which can be given a chat UI) from an embedding or rerank one.
  pub(crate) mode: crate::launch::mode::LaunchMode,
}

/// Every live launch that is a model, with its display name, identity and port,
/// sorted by launch order. The one registry walk, shared by `/metrics` and
/// `/ui`'s chooser; infrastructure launches (a managed multiplexer's umbrella)
/// serve no model of their own and are left out. A launch that is not Ready is
/// still listed, with `ready: false`.
pub(crate) async fn live_launches(state: &ProxyState) -> Vec<LiveLaunch> {
  let sup_snap = state.ctx.supervisors.snapshot().await;
  let cat_snap = state.ctx.catalog.shared_view().await;
  let by_path = route::index_catalog_by_path(&cat_snap);
  let mut out = Vec::new();
  for (launch_id, model) in sup_snap {
    if crate::backend::is_infra_launch(&launch_id) {
      continue;
    }
    let id = model.id().clone();
    let path = id.path.to_string_lossy().into_owned();
    let name = route::served_name_for_model(by_path.get(&path).copied(), &id.path);
    let ready = matches!(model.state().await, ManagedState::Ready);
    out.push(LiveLaunch {
      id: launch_id.as_str().to_string(),
      model: name,
      model_id: id,
      path,
      port: model.port(),
      ready,
      mode: model.mode(),
    });
  }
  out.sort_by(|a, b| launch_order(&a.id).cmp(&launch_order(&b.id)));
  out
}

/// Sort key for a launch id: `L10` after `L2`.
pub(crate) fn launch_order(id: &str) -> (u8, u64, &str) {
  match LaunchId(id.to_string()).counter() {
    Some(n) => (0, n, id),
    None => (1, 0, id),
  }
}

/// Resolve `?model=` / `?launch=` into a [`Scope`], in the order the plan sets:
/// launch id, port, exact catalog path, then a case-insensitive display-name
/// match through the proxy's own reference resolver. `live` is the caller's
/// [`live_launches`] snapshot, so one request asks the registry once.
pub(crate) async fn resolve_scope(
  state: &ProxyState,
  q: &Query,
  live: &[LiveLaunch],
  snapshot: &LogSnapshot,
) -> Result<Scope, Reject> {
  let mut scope = Scope {
    log: LogScope::All,
    per_launch: q.per_launch(),
    ..Scope::default()
  };

  if let Some(value) = q.launch_ref() {
    let (id, path) = resolve_launch(state, snapshot, live, value).await?;
    scope.launch = Some(id);
    scope.path = path;
    // Naming one launch is also asking for its series.
    scope.per_launch = true;
    scope.log = LogScope::Launch {
      id: scope.launch.clone().unwrap_or_default(),
    };
    scope.name = scope.path.as_deref().and_then(|wanted| {
      live
        .iter()
        .find(|l| l.path == wanted)
        .map(|l| l.model.clone())
        .or_else(|| {
          snapshot
            .models
            .iter()
            .find(|m| m.key == wanted)
            .and_then(|m| m.label.clone())
        })
    });
  }

  if let Some(value) = &q.model {
    // Whatever `?model=` cannot settle is the answer the same reference gets
    // everywhere else: a 404 naming the value, or the 400 that says several
    // models matched.
    let (path, name) = resolve_model(state, value).await?;
    // A launch scope and a model scope have to agree. A `?launch=` whose model
    // is not the one `?model=` names has no honest answer, and a half-empty body
    // would read as idle traffic rather than as the contradiction it is.
    if let (Some(launch_value), Some(served)) = (q.launch_ref(), scope.path.as_ref()) {
      if served != &path {
        return Err(Reject::ConflictingScope {
          model: value.clone(),
          launch: launch_value.to_string(),
        });
      }
    }
    if scope.launch.is_none() {
      scope.log = LogScope::Model { path: path.clone() };
    }
    scope.name = Some(name);
    scope.path = Some(path);
  }
  Ok(scope)
}

/// `(launch id, catalog path)` for a `?launch=` value: a live or retired launch
/// id first, then a live launch's port. The path is `None` when the launch has
/// stopped and no row of it is left in the ring.
async fn resolve_launch(
  state: &ProxyState,
  snapshot: &LogSnapshot,
  live: &[LiveLaunch],
  value: &str,
) -> Result<(String, Option<String>), Reject> {
  let by_id = |id: &str| {
    live
      .iter()
      .find(|l| l.id.eq_ignore_ascii_case(id))
      .map(|l| (l.id.clone(), Some(l.path.clone())))
      .or_else(|| {
        snapshot
          .launches
          .iter()
          .find(|l| l.totals.key.eq_ignore_ascii_case(id))
          .map(|l| {
            (
              l.totals.key.clone(),
              snapshot.launch_path(&l.totals.key).map(str::to_string),
            )
          })
      })
  };
  if let Some(found) = by_id(value) {
    return Ok(found);
  }
  if let Ok(port) = value.parse::<u16>() {
    if let Some(found) = live.iter().find(|l| l.port == port) {
      return Ok((found.id.clone(), Some(found.path.clone())));
    }
  }
  // A full catalog path is also a launch reference, and one live launch of that
  // model is what it names. Two of them is the same ambiguity a fuzzy `?model=`
  // is: settling on whichever the registry walked past first would report one
  // launch's figures for a reference that meant both, so it is refused.
  if let Ok((path, _)) = resolve_model(state, value).await {
    let matches: Vec<&LiveLaunch> = live.iter().filter(|l| l.path == path).collect();
    match matches.as_slice() {
      [] => {}
      [only] => return Ok((only.id.clone(), Some(path))),
      many => {
        return Err(Reject::AmbiguousLaunch {
          value: value.to_string(),
          launches: many.iter().map(|l| l.id.clone()).collect(),
        });
      }
    }
  }
  Err(Reject::UnknownRef(value.to_string()))
}

/// `(catalog path, display name)` for a `?model=` value: an exact catalog path,
/// then the same reference rules `/v1/chat/completions` uses (an outright name,
/// a `proxy.aliases` entry, a partial match).
pub(crate) async fn resolve_model(
  state: &ProxyState,
  value: &str,
) -> Result<(String, String), Reject> {
  let (views, rows) = state.ctx.catalog.shared_pair().await;
  if let Some(model) = route::index_catalog_by_path(&views).get(value).copied() {
    return Ok((
      value.to_string(),
      route::served_name_for_model(Some(model), &model.path),
    ));
  }
  match route::resolve_client_reference(state, rows.as_ref(), value) {
    // The launch half of a `name@launch` reference is the caller's business,
    // not the scope's: the scope is the model it names.
    Ok((_, found)) => Ok((
      found.row.path.clone(),
      route::served_name_for_row(&found.row),
    )),
    Err(route::ClientRefMiss::NotFound) => Err(Reject::UnknownRef(value.to_string())),
    Err(route::ClientRefMiss::Ambiguous(_)) => Err(Reject::Ambiguous(value.to_string())),
  }
}

/// What both routes parse out of a request before they differ.
pub(crate) struct Parsed {
  pub(crate) format: Format,
  pub(crate) scope: Scope,
  pub(crate) limit: usize,
}

/// Negotiate the format and resolve the scope of a metrics request.
pub(crate) async fn parse_request(
  state: &ProxyState,
  req: &Request<Incoming>,
  live: &[LiveLaunch],
  snapshot: &LogSnapshot,
) -> Result<Parsed, Reject> {
  let q = Query::parse(req.uri().query());
  let format = choose_format(&q, req.headers())?;
  let scope = resolve_scope(state, &q, live, snapshot).await?;
  Ok(Parsed {
    format,
    scope,
    limit: q.limit(),
  })
}

/// `GET /metrics`.
pub(crate) async fn serve(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let live = live_launches(&state).await;
  // One read of the log for the whole request: the scope resolution and the body
  // answer from the same rows, so a scrape cannot show a request that arrived
  // between two reads, or miss one.
  let snapshot = state.ctx.requests.snapshot();
  let parsed = match parse_request(&state, &req, &live, &snapshot).await {
    Ok(parsed) => parsed,
    Err(reject) => return reject.into_response(),
  };
  let loaded = live.iter().filter(|l| l.ready).count();
  let discovered = state.ctx.catalog.len().await;
  if parsed.format == Format::Json {
    return json_response(json_body(&snapshot, &live, &parsed, loaded, discovered));
  }
  let mut doc = Document::new(true);
  render(
    &mut doc,
    &snapshot,
    &live,
    &parsed.scope,
    loaded,
    discovered,
  );
  if let Some(status) = state.ctx.requests.file_status() {
    doc.sample(
      "llamastash:request_log_file_dropped_total",
      "Finished requests that did not reach the request log file",
      "counter",
      &[],
      status.dropped as f64,
    );
  }
  text_response(doc.render())
}

/// Every `llamastash:*` series of a `/metrics` scrape into `doc`.
fn render(
  doc: &mut Document,
  snapshot: &LogSnapshot,
  live: &[LiveLaunch],
  scope: &Scope,
  loaded: usize,
  discovered: usize,
) {
  // Daemon gauges: no labels, always answered.
  doc.sample(
    "llamastash:models_loaded",
    "Model launches currently Ready",
    "gauge",
    &[],
    loaded as f64,
  );
  doc.sample(
    "llamastash:models_discovered",
    "Models in the catalog",
    "gauge",
    &[],
    discovered as f64,
  );
  doc.sample(
    "llamastash:request_log_rows",
    "Requests held in the in-memory request log",
    "gauge",
    &[],
    snapshot.rows.len() as f64,
  );
  doc.sample(
    "llamastash:request_log_capacity",
    "How many requests the request log ring holds",
    "gauge",
    &[],
    CAPACITY as f64,
  );

  for (route, count) in &snapshot.routes {
    doc.sample(
      "llamastash:route_requests_total",
      "Requests the proxy finished, by request path",
      "counter",
      &[("route", route)],
      *count as f64,
    );
  }

  // The `model` label value of every model in the log, settled before the scope
  // cuts anything: the same model has to print the same value in a scoped and an
  // unscoped scrape, or the two series read as two models.
  let labels = model_labels(&snapshot.models);
  // The series that carry a `model` label. A launch scope is strict, exactly as
  // the JSON summary is: the scrape prints that launch's own figures, not
  // everything every launch of its model has served since daemon start.
  let mut scoped: Vec<(String, ScopeTotals, Clients)> = match &scope.launch {
    Some(id) => snapshot
      .launches
      .iter()
      .filter(|entry| entry.totals.key == *id)
      .map(|entry| {
        (
          launch_label(&labels, live, snapshot, entry),
          entry.totals.clone(),
          snapshot.in_flight(|row| row.launch_id.as_deref() == Some(id.as_str())),
        )
      })
      .collect(),
    None => snapshot
      .models
      .iter()
      .filter(|m| scope.keeps_model(&m.key))
      .map(|totals| {
        (
          labels[&totals.key].clone(),
          totals.clone(),
          snapshot.in_flight(|row| row.model_path.as_deref() == Some(totals.key.as_str())),
        )
      })
      .collect(),
  };
  // A live scope with no rows recorded yet still answers with its zeros: an
  // alerting rule on a loaded model's `requests_in_flight == 0` must see the
  // zero, not a missing series. The label sets are the ones the same series
  // print once rows exist, so nothing appears under a second label shape.
  if scoped.is_empty() {
    let idle = match &scope.launch {
      Some(id) => live.iter().find(|l| &l.id == id),
      None => match &scope.path {
        Some(path) => live.iter().find(|l| &l.path == path),
        None => live
          .iter()
          .find(|l| scope.name.as_deref() == Some(l.model.as_str())),
      },
    };
    if let Some(entry) = idle {
      scoped.push((
        zero_model_label(entry, live, snapshot),
        zero_totals(),
        Clients::default(),
      ));
    }
  }
  for (label, totals, live_clients) in &scoped {
    model_series(doc, label, totals, live_clients);
  }

  // Requests the proxy answered without ever reaching a model (an unknown model
  // name, an unreadable body). They are in `summary` but under no model, so an
  // unscoped scrape gives them their own series instead of dropping them.
  if scope.path.is_none() && scope.launch.is_none() {
    unattributed_series(doc, snapshot);
  }

  if !scope.per_launch {
    return;
  }
  for entry in live {
    // The scope is the response: a scrape that named one model or one launch
    // describes that scope, and a launch from outside it in the body would say
    // the scope held something it does not.
    if !scope.keeps_launch(&entry.id, Some(&entry.path)) {
      continue;
    }
    doc.sample(
      "llamastash:launch_info",
      "Live model launches, labelled by launch and model",
      "gauge",
      &[
        ("launch", &entry.id),
        ("model", &entry.model),
        ("port", &entry.port.to_string()),
      ],
      1.0,
    );
  }
  for totals in &snapshot.launches {
    let id = &totals.totals.key;
    if !scope.keeps_launch(id, launch_path(live, snapshot, id)) {
      continue;
    }
    let label = launch_label(&labels, live, snapshot, totals);
    let per_launch_labels = [("launch", id.as_str()), ("model", label.as_str())];
    let s = &totals.totals.summary;
    doc.sample(
      "llamastash:launch_requests_total",
      "Requests one launch finished",
      "counter",
      &per_launch_labels,
      s.requests as f64,
    );
    doc.sample(
      "llamastash:launch_request_errors_total",
      "Requests one launch failed",
      "counter",
      &per_launch_labels,
      s.errors as f64,
    );
    doc.sample(
      "llamastash:launch_prompt_tokens_total",
      "Prompt tokens one launch served",
      "counter",
      &per_launch_labels,
      s.prompt_tokens as f64,
    );
    doc.sample(
      "llamastash:launch_completion_tokens_total",
      "Completion tokens one launch served",
      "counter",
      &per_launch_labels,
      s.completion_tokens as f64,
    );
    // In-flight is a live quantity: a stopped launch has none, and printing the
    // zero would read as a request that never finished.
    if totals.live {
      let running = snapshot.in_flight(|row| row.launch_id.as_deref() == Some(id.as_str()));
      doc.sample(
        "llamastash:launch_requests_in_flight",
        "Requests one launch is still serving",
        "gauge",
        &per_launch_labels,
        running.requests() as f64,
      );
      // No per-launch host gauge: one metric name carries one label set, and
      // `hosts_in_flight` already carries `model`. A launch's live hosts are
      // on the JSON surfaces (`launches[]`, `requests_tail --launch`).
    }
  }
  // A live launch named by `?launch=` that has no rows yet still answers with
  // the per-launch counter that survives a launch stopping; the model-labelled
  // series took their zeros from the scoped block above.
  for entry in live {
    if scope.launch.as_deref() != Some(entry.id.as_str()) {
      continue;
    }
    if snapshot.launches.iter().any(|t| t.totals.key == entry.id) {
      continue;
    }
    let per_launch_labels = [
      ("launch", entry.id.as_str()),
      ("model", entry.model.as_str()),
    ];
    doc.sample(
      "llamastash:launch_requests_total",
      "Requests one launch finished",
      "counter",
      &per_launch_labels,
      0.0,
    );
  }
}

/// The `model` label value for a request that never reached one.
const NO_MODEL: &str = "none";

/// The four series that carry nothing but a `model` label: the two counters
/// and the two in-flight gauges. One declaration because `Document` keeps the
/// first HELP and TYPE written for a name, so a second copy of those strings
/// could disagree with the one that rendered and stay invisible.
fn core_series(doc: &mut Document, model: &str, requests: u64, errors: u64, live: &Clients) {
  let labels = [("model", model)];
  doc.sample(
    "llamastash:requests_total",
    "Requests the proxy finished, since daemon start",
    "counter",
    &labels,
    requests as f64,
  );
  doc.sample(
    "llamastash:request_errors_total",
    "Finished requests that failed or were cut short",
    "counter",
    &labels,
    errors as f64,
  );
  doc.sample(
    "llamastash:requests_in_flight",
    "Requests the proxy is still serving",
    "gauge",
    &labels,
    live.requests() as f64,
  );
  doc.sample(
    "llamastash:hosts_in_flight",
    "Distinct client hosts the proxy is serving right now",
    "gauge",
    &labels,
    live.hosts() as f64,
  );
}

/// Series for the requests no model answered, as `model="none"`. Everything
/// the log holds for a model is attributed to it, so the unattributed share is
/// the whole log minus the models; a request still in flight before any model
/// is picked is counted there too.
fn unattributed_series(doc: &mut Document, snapshot: &LogSnapshot) {
  let attributed_requests: u64 = snapshot.models.iter().map(|m| m.summary.requests).sum();
  let attributed_errors: u64 = snapshot.models.iter().map(|m| m.summary.errors).sum();
  let requests = snapshot
    .all
    .summary
    .requests
    .saturating_sub(attributed_requests);
  let in_flight = snapshot.in_flight(|row| row.model_path.is_none());
  if requests == 0 && in_flight.requests() == 0 {
    return;
  }
  let errors = snapshot
    .all
    .summary
    .errors
    .saturating_sub(attributed_errors);
  core_series(doc, NO_MODEL, requests, errors, &in_flight);
}

/// The counters and gauges that carry one `model` label.
fn model_series(doc: &mut Document, label: &str, totals: &ScopeTotals, live: &Clients) {
  let labels = [("model", label)];
  let s = &totals.summary;
  core_series(doc, label, s.requests, s.errors, live);
  doc.sample(
    "llamastash:prompt_tokens_total",
    "Prompt tokens the finished requests reported",
    "counter",
    &labels,
    s.prompt_tokens as f64,
  );
  doc.sample(
    "llamastash:prompt_tokens_cached_total",
    "Prompt tokens served from the KV cache by requests that report a prompt count",
    "counter",
    &labels,
    s.cached_tokens as f64,
  );
  doc.sample(
    "llamastash:prefill_tokens_total",
    "Prompt tokens the finished requests had to compute",
    "counter",
    &labels,
    totals.prefill_tokens as f64,
  );
  // The seconds those tokens took, so `rate(prefill_tokens_total) /
  // rate(prefill_seconds_total)` is the prefill speed without the log. An
  // estimated row's window is the proxy's own clock, queue wait inside the
  // engine included, which is why the speed can read low under concurrency.
  doc.sample(
    "llamastash:prefill_seconds_total",
    "Time spent computing prompt tokens, estimated windows included",
    "counter",
    &labels,
    totals.prefill_seconds,
  );
  doc.sample(
    "llamastash:completion_tokens_total",
    "Completion tokens the finished requests reported",
    "counter",
    &labels,
    s.completion_tokens as f64,
  );
  doc.sample(
    "llamastash:request_duration_seconds_total",
    "Total duration of the finished requests",
    "counter",
    &labels,
    totals.duration_ms_sum as f64 / 1000.0,
  );
  doc.sample(
    "llamastash:request_ttft_seconds_total",
    "Total time to the first response byte of the finished requests",
    "counter",
    &labels,
    totals.ttfb_ms_sum as f64 / 1000.0,
  );
  // The denominator of both sums, and of the two averages below. It is not
  // `llamastash:requests_total`: an error or a client that hung up finished a
  // request the sums never counted, so dividing by the request count reads the
  // mean low by exactly as much as the traffic that failed.
  doc.sample(
    "llamastash:request_timed_total",
    "Requests the duration and first-byte totals are over",
    "counter",
    &labels,
    totals.timed as f64,
  );
  doc.sample(
    "llamastash:auto_starts_total",
    "Requests that started a model launch",
    "counter",
    &labels,
    s.auto_starts as f64,
  );
  doc.sample(
    "llamastash:evictions_total",
    "Launches unloaded to make room for a request",
    "counter",
    &labels,
    s.evictions as f64,
  );
  // A gauge with no figure yet has no sample; the metric is then omitted whole
  // rather than printed as a made-up zero.
  if let Some(ms) = s.avg_duration_ms {
    doc.sample(
      "llamastash:request_duration_seconds_avg",
      "Mean duration of the completed successful requests",
      "gauge",
      &labels,
      ms as f64 / 1000.0,
    );
  }
  if let Some(ms) = s.avg_ttfb_ms {
    doc.sample(
      "llamastash:request_ttft_seconds_avg",
      "Mean time to first byte of the completed successful requests",
      "gauge",
      &labels,
      ms as f64 / 1000.0,
    );
  }
  if let Some(tps) = s.tokens_per_second_last {
    doc.sample(
      "llamastash:request_tokens_per_second",
      "Generation speed the most recent request reported",
      "gauge",
      &labels,
      tps,
    );
  }
  if let Some(tps) = s.tokens_per_second_avg {
    doc.sample(
      "llamastash:request_tokens_per_second_avg",
      "Mean of the per-request generation speeds",
      "gauge",
      &labels,
      tps,
    );
  }
  // Weighted by prompt tokens rather than a mean of the per-row speeds, so it
  // answers how fast this model prefills rather than how fast one turn did.
  // Like every other total here it counts since daemon start, not just the
  // rows the log still keeps.
  if let Some(tps) = s.prefill_tokens_per_second {
    doc.sample(
      "llamastash:request_prefill_tokens_per_second",
      "Prompt tokens per second since daemon start",
      "gauge",
      &labels,
      tps,
    );
  }
  // The same traffic read as the box's throughput: every token over the time
  // they were being generated, so concurrent requests raise this and not the
  // mean above. It covers the requests the log holds, not all of them.
  if let Some(tps) = s.tokens_per_second_aggregate {
    doc.sample(
      "llamastash:request_tokens_per_second_aggregate",
      "Tokens per second over the generation time of the requests the log holds",
      "gauge",
      &labels,
      tps,
    );
  }
  // The prompt side of the same figure, so a model that prefills several prompts
  // at once reads above the token-weighted mean above.
  if let Some(tps) = s.prefill_tokens_per_second_aggregate {
    doc.sample(
      "llamastash:request_prefill_tokens_per_second_aggregate",
      "Prompt tokens per second over the prompt-computation time of the requests the log holds",
      "gauge",
      &labels,
      tps,
    );
  }
}

/// The `model` label a live scope answers with before any row exists: the
/// display name, or the catalog path when the name is already taken by another
/// model. It consults the whole log, not just the live launches or what the
/// scope happens to include, so a model keeps one label for its whole life
/// however it is scraped. See `model_labels` for the same rule on the totals.
fn zero_model_label(entry: &LiveLaunch, live: &[LiveLaunch], snapshot: &LogSnapshot) -> String {
  let shared = live
    .iter()
    .any(|l| l.model == entry.model && l.path != entry.path)
    || snapshot
      .models
      .iter()
      .any(|m| m.label.as_deref() == Some(entry.model.as_str()) && m.key != entry.path);
  label_or_path(&entry.model, &entry.path, shared)
}

/// The collision rule both label pickers apply: a model prints its display name
/// unless another model shares it, or the name is `none`, which names the
/// requests that reached no model. Either way two samples of one metric would
/// land under one label set, so the catalog path stands in.
fn label_or_path(name: &str, path: &str, shared: bool) -> String {
  if shared || name == NO_MODEL {
    path.to_string()
  } else {
    name.to_string()
  }
}

/// The summary a scope with no recorded rows still answers with: every counter
/// and sum at zero, no average computed from nothing.
fn zero_totals() -> ScopeTotals {
  ScopeTotals {
    key: String::new(),
    label: None,
    summary: RequestSummary::default(),
    duration_ms_sum: 0,
    ttfb_ms_sum: 0,
    timed: 0,
    prefill_tokens: 0,
    prefill_seconds: 0.0,
  }
}

/// The catalog path one launch served: the registry's answer for a live launch,
/// then the rows of a retired one. `None` when neither can say.
fn launch_path<'a>(live: &'a [LiveLaunch], snapshot: &'a LogSnapshot, id: &str) -> Option<&'a str> {
  live
    .iter()
    .find(|l| l.id == id)
    .map(|l| l.path.as_str())
    .or_else(|| snapshot.launch_path(id))
}

/// The `model` label a launch's series print: the value the per-model series
/// already use for the model it served, so a join on `model` matches when two
/// models share a display name and both fell back to their paths; then the name
/// its own rows carried, then the registry's, then `unknown`.
fn launch_label(
  labels: &HashMap<String, String>,
  live: &[LiveLaunch],
  snapshot: &LogSnapshot,
  entry: &LaunchTotals,
) -> String {
  let id = entry.totals.key.as_str();
  launch_path(live, snapshot, id)
    .and_then(|p| labels.get(p))
    .cloned()
    .or_else(|| entry.totals.label.clone())
    .or_else(|| live.iter().find(|l| l.id == id).map(|l| l.model.clone()))
    .unwrap_or_else(|| "unknown".to_string())
}

/// The `model` label value of each entry in `models`. Two models can share a
/// display name (same-named GGUFs in different roots), and one metric name may
/// not carry two samples with the same label set, so a shared name falls back to
/// the catalog path for the models that collide. Callers pass the whole log, so
/// the pick does not depend on what a scope happens to include.
fn model_labels(models: &[ScopeTotals]) -> HashMap<String, String> {
  let mut counts: HashMap<String, usize> = HashMap::new();
  for totals in models {
    *counts
      .entry(totals.label.clone().unwrap_or_default())
      .or_default() += 1;
  }
  models
    .iter()
    .map(|totals| {
      let label = totals.label.clone().unwrap_or_else(|| totals.key.clone());
      let shared = counts.get(&label).copied().unwrap_or(0) > 1;
      (
        totals.key.clone(),
        label_or_path(&label, &totals.key, shared),
      )
    })
    .collect()
}

/// The JSON body: the same `summary` and `requests` objects
/// `llamastash requests --json` returns, plus the per-model and per-launch
/// totals the text format carries as samples.
fn json_body(
  snapshot: &LogSnapshot,
  live: &[LiveLaunch],
  parsed: &Parsed,
  loaded: usize,
  discovered: usize,
) -> Value {
  let scope = &parsed.scope;
  let tail = snapshot.tail(&scope.log, parsed.limit);
  let mut scope_block = Map::new();
  scope_block.insert("model".into(), value_opt(scope.name.clone()));
  scope_block.insert("launch".into(), value_opt(scope.launch.clone()));

  let mut models = Vec::new();
  // A live scope that no row has reached yet is answered the way the text
  // scrape answers it: a zero row keyed by the live launch, not a missing one.
  let zero_live: Vec<&LiveLaunch> = match &scope.launch {
    Some(id)
      if snapshot
        .models
        .iter()
        .all(|m| Some(m.key.as_str()) != scope.path.as_deref()) =>
    {
      live.iter().filter(|l| &l.id == id).collect()
    }
    None
      if scope.path.is_some()
        && snapshot
          .models
          .iter()
          .all(|m| Some(m.key.as_str()) != scope.path.as_deref()) =>
    {
      live
        .iter()
        .filter(|l| Some(l.path.as_str()) == scope.path.as_deref())
        .collect()
    }
    _ => Vec::new(),
  };
  // One row per path however many live launches serve it, the way the totals
  // loop below reports a model with several launches. `model` is the display
  // name, as on every other JSON row: `path` already tells two same-named models
  // apart, and the collision rule exists for the text format's label sets.
  let mut zero_rows: Vec<(String, String, usize)> = Vec::new();
  for entry in &zero_live {
    match zero_rows
      .iter_mut()
      .find(|(_, path, _)| path == &entry.path)
    {
      Some((_, _, count)) => *count += 1,
      None => zero_rows.push((entry.model.clone(), entry.path.clone(), 1)),
    }
  }
  for (model, path, launches) in zero_rows {
    models.push(merged(
      &[
        ("model", Value::String(model)),
        ("path", Value::String(path)),
        ("launches", Value::from(launches)),
      ],
      &RequestSummary::default(),
    ));
  }
  for totals in snapshot.models.iter().filter(|m| scope.keeps_model(&m.key)) {
    let launches = snapshot
      .launches
      .iter()
      .filter(|l| {
        live
          .iter()
          .find(|entry| entry.id == l.totals.key)
          .map(|entry| entry.path == totals.key)
          .or_else(|| snapshot.launch_path(&l.totals.key).map(|p| p == totals.key))
          .unwrap_or(false)
      })
      .count();
    models.push(merged(
      &[
        ("model", value_opt(totals.label.clone())),
        ("path", Value::String(totals.key.clone())),
        ("launches", Value::from(launches)),
      ],
      &totals.summary,
    ));
  }

  let mut launches = Vec::new();
  for entry in live {
    if scope.launch.as_deref() != Some(entry.id.as_str()) {
      continue;
    }
    if snapshot.launches.iter().any(|t| t.totals.key == entry.id) {
      continue;
    }
    launches.push(merged(
      &[
        ("launch", Value::String(entry.id.clone())),
        ("model", Value::String(entry.model.clone())),
        ("port", Value::from(entry.port)),
        ("live", Value::Bool(true)),
      ],
      &RequestSummary::default(),
    ));
  }
  for entry in &snapshot.launches {
    let id = &entry.totals.key;
    let live_entry = live.iter().find(|l| &l.id == id);
    let path = live_entry
      .map(|l| l.path.clone())
      .or_else(|| snapshot.launch_path(id).map(str::to_string));
    if !scope.keeps_launch(id, path.as_deref()) {
      continue;
    }
    let port = live_entry.map(|l| l.port);
    let model = entry
      .totals
      .label
      .clone()
      .or_else(|| live_entry.map(|l| l.model.clone()));
    let body = merged(
      &[
        ("launch", Value::String(id.clone())),
        ("model", value_opt(model)),
        ("port", value_opt(port)),
        ("live", Value::Bool(live_entry.is_some())),
      ],
      &entry.totals.summary,
    );
    launches.push(body);
  }

  json_object(|map| {
    map.insert("generated_at_ms".into(), Value::from(datetime::now_ms()));
    map.insert("scope".into(), Value::Object(scope_block));
    map.insert(
      "summary".into(),
      serde_json::to_value(&tail.summary).unwrap_or(Value::Null),
    );
    map.insert("models".into(), Value::Array(models));
    map.insert("launches".into(), Value::Array(launches));
    map.insert(
      "requests".into(),
      Value::Array(
        tail
          .rows
          .iter()
          .map(|row| serde_json::to_value(row).unwrap_or(Value::Null))
          .collect(),
      ),
    );
    map.insert("models_loaded".into(), Value::from(loaded));
    map.insert("models_discovered".into(), Value::from(discovered));
  })
}

/// A [`RequestSummary`] with extra keys in front of its own, so a client that
/// parses `requests --json` parses this too.
fn merged(extra: &[(&str, Value)], summary: &RequestSummary) -> Value {
  json_object(|map| {
    for (key, value) in extra {
      map.insert((*key).into(), value.clone());
    }
    if let Value::Object(fields) = serde_json::to_value(summary).unwrap_or(Value::Null) {
      for (key, value) in fields {
        map.insert(key, value);
      }
    }
  })
}

/// A JSON object built in insertion order.
pub(crate) fn json_object(build: impl FnOnce(&mut Map<String, Value>)) -> Value {
  let mut map = Map::new();
  build(&mut map);
  Value::Object(map)
}

/// A JSON value for an `Option`: the value itself, or `null` when there is
/// nothing to name. An absent value keeps its key, so a client never tells a
/// missing field from an unset one.
pub(crate) fn value_opt<T: Into<Value>>(value: Option<T>) -> Value {
  match value {
    Some(v) => v.into(),
    None => Value::Null,
  }
}

/// One metric name's HELP/TYPE text and its samples, in print order.
struct Block {
  /// The `# HELP` text in wire form, escaped exactly once — by us for our own
  /// strings, by the engine that printed it for a scraped one.
  help: String,
  ty: String,
  samples: Vec<Sample>,
}

impl Block {
  /// A block with no HELP yet and no declared type: `untyped` is what a scraper
  /// reads when nothing said otherwise.
  fn new() -> Self {
    Block {
      help: String::new(),
      ty: "untyped".to_string(),
      samples: Vec::new(),
    }
  }
}

/// One sample line of a block.
struct Sample {
  /// What this sample appends to the block's name when it prints, normally
  /// nothing. An engine that declares one family and prints its samples under
  /// derived names has to keep each sample's own name while `# TYPE` prints the
  /// family's once, or the samples land in blocks of their own and every one of
  /// them reads as `untyped`.
  suffix: &'static str,
  /// The label text between the braces, in wire form; empty when there are none.
  labels: String,
  value: f64,
}

/// What an engine appends to a declared family name on a sample line: a counter
/// prints `_total` (plus `_created` in OpenMetrics), a histogram `_bucket`,
/// `_sum` and `_count`, a summary `_sum` and `_count`.
const FAMILY_SUFFIXES: [&str; 5] = ["_total", "_created", "_bucket", "_sum", "_count"];

/// A Prometheus text document under construction.
///
/// `# HELP` and `# TYPE` are written once per metric name, first writer wins.
/// A metric whose only samples were refused (non-finite) prints nothing at all,
/// which is what a scraper expects: no empty `# TYPE` block.
pub(crate) struct Document {
  blocks: Vec<(String, Block)>,
  index: HashMap<String, usize>,
  /// Our own series sort by name and by label text; a merged engine scrape
  /// keeps the order the targets printed.
  sorted: bool,
}

impl Document {
  pub(crate) fn new(sorted: bool) -> Self {
    Document {
      blocks: Vec::new(),
      index: HashMap::new(),
      sorted,
    }
  }

  /// The block kept under exactly `name`, created when nothing is there yet.
  fn entry(&mut self, name: &str) -> usize {
    if let Some(i) = self.index.get(name) {
      return *i;
    }
    self.index.insert(name.to_string(), self.blocks.len());
    self.blocks.push((name.to_string(), Block::new()));
    self.blocks.len() - 1
  }

  fn block(&mut self, name: &str) -> &mut Block {
    let at = self.entry(name);
    &mut self.blocks[at].1
  }

  /// The block one scraped sample belongs to, and what that sample appends to
  /// the block's name when it prints. The sample's own name wins when that is
  /// what was declared — an engine that prints the names its own `# TYPE` lines
  /// declare, `foo_total` counters included, stays byte-identical. Otherwise a
  /// declared family that is one Prometheus suffix short of the sample's name
  /// does, which is the shape of a family declared once and printed suffixed.
  /// Anything else gets a block of its own, keyed and printed under the name the
  /// engine used.
  fn scrape_block(&mut self, name: &str) -> (usize, &'static str) {
    if let Some(at) = self.index.get(name) {
      return (*at, "");
    }
    for suffix in FAMILY_SUFFIXES {
      if let Some(stem) = name.strip_suffix(suffix) {
        if let Some(at) = self.index.get(stem) {
          return (*at, suffix);
        }
      }
    }
    (self.entry(name), "")
  }

  /// Record a `# HELP` / `# TYPE` line read from a scraped engine. The first
  /// one for a name wins, so merging N targets prints one block per name. The
  /// HELP text is stored exactly as it arrived: it is already in wire form, and
  /// escaping it again would double every escape the engine wrote.
  pub(crate) fn declare(&mut self, name: &str, help: Option<&str>, ty: Option<&str>) {
    let block = self.block(name);
    if block.help.is_empty() {
      if let Some(help) = help {
        block.help = help.to_string();
      }
    }
    if block.ty == "untyped" {
      if let Some(ty) = ty {
        block.ty = ty.to_string();
      }
    }
  }

  /// One sample of a `llamastash:*` metric. `labels` print as `key="value"`
  /// pairs in the order given. `help` is Rust text with real characters, so it
  /// is escaped here and printed as stored.
  pub(crate) fn sample(
    &mut self,
    name: &str,
    help: &str,
    ty: &str,
    labels: &[(&str, &str)],
    value: f64,
  ) {
    if !value.is_finite() {
      return;
    }
    let rendered = labels
      .iter()
      .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
      .collect::<Vec<_>>()
      .join(",");
    let block = self.block(name);
    if block.help.is_empty() {
      block.help = escape_help(help);
    }
    if block.ty == "untyped" {
      block.ty = ty.to_string();
    }
    block.samples.push(Sample {
      suffix: "",
      labels: rendered,
      value,
    });
  }

  /// One sample taken verbatim from a scraped engine: `labels` is the text
  /// between the braces, kept as the engine printed it. `false` when the value
  /// was not a number a scraper can read, in which case nothing was stored.
  pub(crate) fn sample_raw(&mut self, name: &str, labels: String, value: f64) -> bool {
    if !value.is_finite() {
      return false;
    }
    let (at, suffix) = self.scrape_block(name);
    self.blocks[at].1.samples.push(Sample {
      suffix,
      labels,
      value,
    });
    true
  }

  /// How many samples landed in the document.
  #[cfg(test)]
  pub(crate) fn samples(&self) -> usize {
    self.blocks.iter().map(|(_, b)| b.samples.len()).sum()
  }

  pub(crate) fn render(&self) -> String {
    let mut order: Vec<&(String, Block)> = self.blocks.iter().collect();
    if self.sorted {
      order.sort_by(|a, b| a.0.cmp(&b.0));
    }
    let mut out = String::new();
    for (name, block) in order {
      if block.samples.is_empty() {
        continue;
      }
      let mut samples: Vec<&Sample> = block.samples.iter().collect();
      if self.sorted {
        samples.sort_by_key(|s| (s.suffix, s.labels.as_str()));
      }
      if !block.help.is_empty() {
        out.push_str(&format!("# HELP {name} {}\n", block.help));
      }
      out.push_str(&format!("# TYPE {name} {}\n", block.ty));
      for sample in samples {
        let printed = format!("{name}{}", sample.suffix);
        if sample.labels.is_empty() {
          out.push_str(&format!("{printed} {}\n", fmt_value(sample.value)));
        } else {
          out.push_str(&format!(
            "{printed}{{{}}} {}\n",
            sample.labels,
            fmt_value(sample.value)
          ));
        }
      }
    }
    out
  }
}

/// A label value in wire form. A quoted label value may only use `\\`, `\"` and
/// `\n`: a strict parser reads `\t` as an illegal escape, so a tab or a carriage
/// return goes out as itself and nothing else is touched.
pub(crate) fn escape_label(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  for c in value.chars() {
    match c {
      '\\' => out.push_str(r"\\"),
      '"' => out.push_str(r#"\""#),
      '\n' => out.push_str(r"\n"),
      _ => out.push(c),
    }
  }
  out
}

/// HELP text in wire form. Unlike a label value it has no escape for a quote,
/// so `"` stays a plain quote and only `\` and the newline are escaped.
fn escape_help(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  for c in value.chars() {
    match c {
      '\\' => out.push_str(r"\\"),
      '\n' => out.push_str(r"\n"),
      _ => out.push(c),
    }
  }
  out
}

/// A sample value. Whole numbers print without a decimal point; Prometheus
/// reads either form and an integer counter reads cleaner.
fn fmt_value(value: f64) -> String {
  if value.fract() == 0.0 && value.abs() < 1e15 {
    format!("{}", value as i64)
  } else {
    format!("{value}")
  }
}

/// 200 OK with a Prometheus text body.
pub(crate) fn text_response(body: String) -> ProxyResponse {
  Ok(
    Response::builder()
      .status(StatusCode::OK)
      .header(CONTENT_TYPE, TEXT_CONTENT_TYPE)
      .body(router::full_body(Bytes::from(body)))
      .expect("static text response must build"),
  )
}

/// 200 OK with a JSON body built by one of these routes.
pub(crate) fn json_response(body: Value) -> ProxyResponse {
  let bytes = serde_json::to_vec(&body).expect("json encoding of a built value cannot fail");
  Ok(router::json_response(StatusCode::OK, bytes))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::proxy::request_log::{RequestLog, RequestState, DEFAULT_TAIL};
  use crate::proxy::usage_tap::Usage;
  use std::collections::BTreeSet;
  use std::path::PathBuf;

  use crate::proxy::test_support::state_with;

  /// A Ready live-launch row for the scrape tests, keyed on the model's path.
  fn live_row(id: &str, model: &str, path: &str, port: u16) -> LiveLaunch {
    LiveLaunch {
      id: id.into(),
      model: model.into(),
      model_id: ModelId {
        path: path.into(),
        header_blake3: [0u8; 32],
      },
      path: path.into(),
      port,
      ready: true,
      mode: crate::launch::mode::LaunchMode::Chat,
    }
  }

  /// The one registry walk: every model launch in launch order with the
  /// identity and port its callers need, and a not-Ready launch still listed
  /// with `ready: false`.
  #[tokio::test]
  async fn live_launches_lists_model_launches_in_launch_order() {
    let state = state_with(&[
      ("L10", ManagedState::Ready, "/models/zulu.gguf", 41110),
      ("L2", ManagedState::Loading, "/models/alpha.gguf", 41102),
    ])
    .await;
    let live = live_launches(&state).await;
    assert_eq!(
      live
        .iter()
        .map(|l| (l.id.as_str(), l.model.as_str(), l.port, l.ready))
        .collect::<Vec<_>>(),
      vec![("L2", "alpha", 41102, false), ("L10", "zulu", 41110, true)]
    );
    assert_eq!(live[0].model_id.path, PathBuf::from("/models/alpha.gguf"));
  }

  fn format_of(accept: Option<&str>) -> Result<Format, Reject> {
    let mut headers = HeaderMap::new();
    if let Some(a) = accept {
      headers.insert(hyper::header::ACCEPT, a.parse().unwrap());
    }
    choose_format(&Query::default(), &headers)
  }

  /// `?format=` is read off the parsed query, the same `Query` the scope comes
  /// from, so it is parsed exactly once per request.
  fn format_of_query(query: &str, accept: Option<&str>) -> Result<Format, Reject> {
    let mut headers = HeaderMap::new();
    if let Some(a) = accept {
      headers.insert(hyper::header::ACCEPT, a.parse().unwrap());
    }
    choose_format(&Query::parse(Some(query)), &headers)
  }

  #[test]
  fn metrics_accept_contract_matches_the_plan() {
    assert_eq!(format_of(None).unwrap(), Format::Text);
    assert_eq!(format_of(Some("*/*")).unwrap(), Format::Text);
    assert_eq!(format_of(Some("text/plain")).unwrap(), Format::Text);
    assert_eq!(
      format_of(Some("text/plain; version=0.0.4; charset=utf-8")).unwrap(),
      Format::Text
    );
    assert_eq!(
      format_of(Some(
        "application/openmetrics-text; version=1.0.0,text/plain"
      ))
      .unwrap(),
      Format::Text
    );
    // A wildcard never selects JSON on its own, and `application/*` is not a
    // family either candidate is in.
    assert_eq!(format_of(Some("text/*")).unwrap(), Format::Text);
    assert_eq!(format_of(Some("*/*;q=0.5")).unwrap(), Format::Text);
    assert!(matches!(
      format_of(Some("application/*")),
      Err(Reject::NotAcceptable(_))
    ));
    assert!(matches!(
      format_of(Some("image/*")),
      Err(Reject::NotAcceptable(_))
    ));
    assert!(matches!(
      format_of(Some("application/xml")),
      Err(Reject::NotAcceptable(_))
    ));
    assert_eq!(format_of(Some("application/json")).unwrap(), Format::Json);
    // A q that is not a number makes that entry no answer; the rest of the
    // header still is.
    assert!(matches!(
      format_of(Some("application/json;q=x")),
      Err(Reject::NotAcceptable(_))
    ));
    assert_eq!(
      format_of(Some("application/json;q=, text/plain")).unwrap(),
      Format::Text
    );
  }

  /// An explicit `Accept` that only the JSON view satisfies must not be sent
  /// Prometheus text, and one that both satisfy must not pick JSON on its own.
  /// A `q=0` is a refusal, not a missing entry: it takes that type out of the
  /// running for the whole header, so a wildcard cannot hand it back.
  #[test]
  fn metrics_a_zero_q_refuses_the_type_it_names() {
    assert_eq!(
      format_of(Some("text/plain;q=0, */*")).unwrap(),
      Format::Json,
      "refusing text leaves the wildcard speaking only for JSON"
    );
    assert_eq!(
      format_of(Some("application/json;q=0, */*")).unwrap(),
      Format::Text,
      "the mirror image"
    );
    assert_eq!(
      format_of(Some("text/plain;q=0, application/json;q=0.3")).unwrap(),
      Format::Json
    );
    assert!(
      matches!(
        format_of(Some("text/plain;q=0, application/json;q=0")),
        Err(Reject::NotAcceptable(_))
      ),
      "refusing both candidates is no answer at all"
    );
  }

  #[test]
  fn metrics_accept_prefers_the_format_the_client_wants() {
    assert_eq!(
      format_of(Some("application/json;q=0.9, text/plain;q=0.5")).unwrap(),
      Format::Json
    );
    assert_eq!(
      format_of(Some("application/json;q=0.5, text/plain;q=0.9")).unwrap(),
      Format::Text
    );
    // Equal q, either order: the tie goes to text, so the JSON view is never
    // what a client gets by accident.
    assert_eq!(
      format_of(Some("application/json;q=0.9, text/plain;q=0.9")).unwrap(),
      Format::Text
    );
    assert_eq!(
      format_of(Some("text/plain;q=0.9, application/json;q=0.9")).unwrap(),
      Format::Text
    );
    // A `*/*` accepts either view one step below the q it carries, so it never
    // outranks a type the client actually named: naming JSON alongside the
    // wildcard is asking for JSON, and a bare wildcard is no preference at all.
    assert_eq!(
      format_of(Some("*/*, application/json")).unwrap(),
      Format::Json
    );
    assert_eq!(
      format_of(Some("application/json, */*")).unwrap(),
      Format::Json
    );
    assert_eq!(
      format_of(Some("*/*;q=0.1, application/json;q=0.9")).unwrap(),
      Format::Json
    );
    // A wildcard raises only a candidate the header never named, so it can
    // never lift a named type above the q the client gave it: text is priced at
    // 0.1, JSON at 0.3, and the `*;q=0.5` speaks for neither's own price.
    assert_eq!(
      format_of(Some("text/plain;q=0.1, */*;q=0.5, application/json;q=0.3")).unwrap(),
      Format::Json
    );
    // The same rule through the `text/*` family form.
    assert_eq!(
      format_of(Some("text/*;q=0.1, */*;q=0.5, application/json;q=0.3")).unwrap(),
      Format::Json
    );
    assert!(matches!(
      format_of(Some("application/xml, image/*")),
      Err(Reject::NotAcceptable(_))
    ));
    // Spaces around the syntax and the q parameter are part of the grammar.
    assert_eq!(
      format_of(Some("application/json ; q = 0.9 , text/plain ; q = 0.1")).unwrap(),
      Format::Json
    );
  }

  #[test]
  fn metrics_format_query_beats_the_accept_header() {
    assert_eq!(
      format_of_query("format=json", Some("*/*")).unwrap(),
      Format::Json
    );
    assert_eq!(
      format_of_query("format=prometheus", Some("*/*")).unwrap(),
      Format::Text
    );
    assert!(matches!(
      format_of_query("format=yaml", Some("*/*")),
      Err(Reject::BadFormat(v)) if v == "yaml"
    ));
    // A `?format=` beats a header that would otherwise be refused outright.
    assert_eq!(
      format_of_query("format=prometheus", Some("application/xml")).unwrap(),
      Format::Text
    );
  }

  #[test]
  fn metrics_query_parses_every_parameter() {
    let q = Query::parse(Some("format=json&limit=5&model=llama3&launch=1"));
    assert_eq!(q.format.as_deref(), Some("json"));
    assert_eq!(q.limit, Some(5));
    assert_eq!(q.model.as_deref(), Some("llama3"));
    assert!(q.per_launch());
    assert_eq!(q.launch_ref(), None);
    // A percent-escaped space and `+` come back decoded.
    let escaped = Query::parse(Some("model=qwen3%204b%2Fq4"));
    assert_eq!(escaped.model.as_deref(), Some("qwen3 4b/q4"));
    // A multi-byte character is a run of escapes and has to survive whole, and
    // `%2B` stays a literal plus while a bare `+` is a space.
    let unicode = Query::parse(Some("model=caf%C3%A9+%2Bx&launch=L2%20b"));
    assert_eq!(unicode.model.as_deref(), Some("caf\u{e9} +x"));
    assert_eq!(unicode.launch.as_deref(), Some("L2 b"));
    // A `%` that starts no hex pair is itself.
    assert_eq!(
      Query::parse(Some("model=100%off")).model.as_deref(),
      Some("100%off")
    );
    assert_eq!(Query::parse(None).limit(), DEFAULT_TAIL);
    assert_eq!(Query::parse(Some("limit=99999")).limit(), CAPACITY);
  }

  /// One finished request, straight into a fresh log.
  fn request(log: &RequestLog, path: &str, launch: &str, prompt: u64, completion: u64) {
    request_named(log, "m1", path, launch, prompt, completion);
  }

  /// As [`request`], with the display name the row carries.
  fn request_named(
    log: &RequestLog,
    name: &str,
    path: &str,
    launch: &str,
    prompt: u64,
    completion: u64,
  ) {
    let mut rec = log.begin("/v1/chat/completions", None);
    rec.set_model(name, path);
    rec.set_launch(launch);
    rec.set_status(200);
    rec.mark_first_byte();
    rec.finish(
      RequestState::Done,
      Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        tokens_per_second: Some(40.0),
        ..Usage::default()
      },
    );
  }

  /// A snapshot of one model whose summary carries the throughput figures, for
  /// the series that a sub-millisecond test row cannot produce on its own.
  fn snapshot_with_throughput(tps: Option<f64>, busy_ms: Option<u64>) -> LogSnapshot {
    let mut summary = RequestSummary {
      requests: 1,
      tokens_per_second_aggregate: tps,
      busy_ms,
      ..RequestSummary::default()
    };
    summary.hosts = Some(0);
    LogSnapshot {
      rows: Vec::new(),
      all: ScopeTotals {
        key: String::new(),
        label: None,
        summary: summary.clone(),
        duration_ms_sum: 0,
        ttfb_ms_sum: 0,
        timed: 0,
        prefill_tokens: 0,
        prefill_seconds: 0.0,
      },
      models: vec![ScopeTotals {
        key: "/models/a.gguf".to_string(),
        label: Some("m1".to_string()),
        summary,
        duration_ms_sum: 0,
        ttfb_ms_sum: 0,
        timed: 0,
        prefill_tokens: 0,
        prefill_seconds: 0.0,
      }],
      launches: Vec::new(),
      routes: Vec::new(),
    }
  }

  #[test]
  fn the_throughput_gauge_prints_only_when_the_scope_has_one() {
    let text_of = |snap: &LogSnapshot| {
      let mut doc = Document::new(true);
      render(&mut doc, snap, &[], &Scope::default(), 1, 1);
      doc.render()
    };
    // A scope no request has reported a speed for has no sample to print, rather
    // than a made-up zero.
    let text = text_of(&snapshot_with_throughput(None, None));
    assert!(
      !text.contains("request_tokens_per_second_aggregate"),
      "{text}"
    );
    let printed = text_of(&snapshot_with_throughput(Some(20.0), Some(10_000)));
    assert!(
      printed.contains("llamastash:request_tokens_per_second_aggregate{model=\"m1\"} 20\n"),
      "{printed}"
    );
  }

  #[test]
  fn the_prompt_throughput_gauge_prints_only_when_the_scope_has_one() {
    let text_of = |snap: &LogSnapshot| {
      let mut doc = Document::new(true);
      render(&mut doc, snap, &[], &Scope::default(), 1, 1);
      doc.render()
    };
    let mut snap = snapshot_with_throughput(None, None);
    let text = text_of(&snap);
    assert!(
      !text.contains("request_prefill_tokens_per_second_aggregate"),
      "{text}"
    );
    snap.models[0].summary.prefill_tokens_per_second_aggregate = Some(1_000.0);
    let printed = text_of(&snap);
    assert!(
      printed
        .contains("llamastash:request_prefill_tokens_per_second_aggregate{model=\"m1\"} 1000\n"),
      "{printed}"
    );
  }

  #[test]
  fn the_json_summary_is_the_snapshot_it_came_from() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    let mut snap = log.snapshot();
    let parsed = Parsed {
      format: Format::Json,
      scope: Scope::default(),
      limit: 10,
    };
    // A sub-millisecond test row leaves the throughput nothing to divide by, so
    // the scrape says so rather than inventing a speed.
    let body = json_body(&snap, &[], &parsed, 1, 2);
    assert_eq!(body["summary"]["tokens_per_second_aggregate"], Value::Null);
    // The body reports the snapshot the scope was resolved from, not a second
    // read of the log, so a request that arrives mid-scrape cannot be half in.
    snap.all.summary.tokens_per_second_aggregate = Some(20.0);
    snap.all.summary.busy_ms = Some(10_000);
    let stamped = json_body(&snap, &[], &parsed, 1, 2);
    assert_eq!(
      stamped["summary"]["tokens_per_second_aggregate"],
      Value::from(20.0)
    );
    assert_eq!(stamped["summary"]["busy_ms"], Value::from(10_000_u64));
  }

  #[test]
  fn metrics_snapshot_counts_routes_and_retired_launches() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    log.forget_launch("L1");
    let snap = log.snapshot();
    assert_eq!(snap.routes, vec![("/v1/chat/completions".to_string(), 1)]);
    assert_eq!(snap.models.len(), 1);
    assert_eq!(snap.launches.len(), 1);
    assert!(!snap.launches[0].live);
    assert_eq!(snap.models[0].summary.requests, 1);
    assert_eq!(snap.all.duration_ms_sum, snap.models[0].duration_ms_sum);
  }

  #[test]
  fn metrics_text_renders_counters_and_gauges() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    let snap = log.snapshot();
    let scope = Scope::default();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &scope, 1, 2);
    let text = doc.render();
    assert!(text.contains("llamastash:requests_total{model=\"m1\"} 1\n"));
    assert!(text.contains("llamastash:prompt_tokens_total{model=\"m1\"} 10\n"));
    assert!(text.contains("llamastash:route_requests_total{route=\"/v1/chat/completions\"} 1\n"));
    assert!(text.contains("llamastash:requests_in_flight{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:hosts_in_flight{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:models_loaded 1\n"));
    assert!(text.contains("llamastash:request_log_capacity 1000\n"));
    // A finished successful request has a duration and a first byte, so both
    // averages are there; they are sub-millisecond in a unit test.
    assert!(text.contains("llamastash:request_duration_seconds_avg{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_ttft_seconds_avg{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_duration_seconds_total{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_ttft_seconds_total{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_tokens_per_second{model=\"m1\"} 40\n"));
    // No `?launch=1`, so no per-launch series.
    assert!(!text.contains("llamastash:launch_"));
  }

  /// The value of one printed sample line, by its `name{labels}` head.
  fn sample_value(text: &str, head: &str) -> f64 {
    text
      .lines()
      .find(|line| line.starts_with(head))
      .unwrap_or_else(|| panic!("{head} is not in the scrape:\n{text}"))
      .rsplit(' ')
      .next()
      .and_then(|last| last.parse().ok())
      .expect("a numeric value")
  }

  /// Both latency sums hold fewer requests than `requests_total` counts, so the
  /// denominator has to travel with them: dividing a sum by the request count
  /// reads the mean low by exactly the traffic that failed.
  #[test]
  fn metrics_exports_the_count_the_latency_sums_divide_by() {
    let log = RequestLog::new();
    // A duration big enough that the two candidate denominators give different
    // numbers.
    let mut slow = log.begin("/v1/chat/completions", None);
    slow.set_model("m1", "/models/a.gguf");
    slow.set_launch("L1");
    slow.set_status(200);
    slow.mark_first_byte();
    std::thread::sleep(std::time::Duration::from_millis(5));
    slow.finish(RequestState::Done, Usage::default());
    // A finished request that was never timed: the client hung up.
    let mut hung = log.begin("/v1/chat/completions", None);
    hung.set_model("m1", "/models/a.gguf");
    hung.set_launch("L1");
    hung.set_status(200);
    hung.finish(RequestState::ClientClosed, Usage::default());
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 2);
    let text = doc.render();
    assert!(
      text.contains("llamastash:requests_total{model=\"m1\"} 2\n"),
      "{text}"
    );
    assert!(
      text.contains("llamastash:request_timed_total{model=\"m1\"} 1\n"),
      "{text}"
    );
    let sum = sample_value(
      &text,
      r#"llamastash:request_duration_seconds_total{model="m1"}"#,
    );
    let avg = sample_value(
      &text,
      r#"llamastash:request_duration_seconds_avg{model="m1"}"#,
    );
    assert!(sum > 0.0, "the timed request is in the sum:\n{text}");
    assert_eq!(
      avg, sum,
      "the mean is the sum over the count, not over the request total:\n{text}"
    );
  }

  #[test]
  fn metrics_text_omits_labels_the_scope_excludes() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    request(&log, "/models/b.gguf", "L2", 1, 1);
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/a.gguf".into(),
      },
      path: Some("/models/a.gguf".into()),
      name: Some("m1".into()),
      per_launch: true,
      launch: None,
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &scope, 2, 2);
    let text = doc.render();
    assert_eq!(text.matches("llamastash:requests_total{model=").count(), 1);
    // Both launches served a model displaying `m1`, so every series prints the
    // catalog path instead, and the per-launch one prints the same value.
    assert!(
      text.contains("llamastash:launch_requests_total{launch=\"L1\",model=\"/models/a.gguf\"} 1"),
      "{text}"
    );
    assert!(!text.contains("launch=\"L2\""));
  }

  /// A launch scope is strict in the text as well as in the summary: the
  /// `model`-labelled series carry that launch's own figures, so an earlier
  /// launch of the same model is not counted in.
  #[test]
  fn metrics_a_launch_scope_prints_that_launchs_own_figures() {
    let log = RequestLog::new();
    // Two launches of one model, so the whole-model figures cannot be mistaken
    // for the launch's own.
    request(&log, "/models/a.gguf", "L1", 10, 3);
    request(&log, "/models/a.gguf", "L2", 1, 1);
    request(&log, "/models/a.gguf", "L2", 1, 1);
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Launch { id: "L1".into() },
      path: Some("/models/a.gguf".into()),
      launch: Some("L1".into()),
      name: Some("m1".into()),
      per_launch: true,
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &scope, 1, 2);
    let text = doc.render();
    // The model as a whole served 3 requests and 12 prompt tokens; this launch
    // served one and 10.
    assert!(
      text.contains("llamastash:requests_total{model=\"m1\"} 1\n"),
      "the model series must be the launch's own:\n{text}"
    );
    assert!(
      text.contains("llamastash:prompt_tokens_total{model=\"m1\"} 10\n"),
      "{text}"
    );
    assert!(
      text.contains("llamastash:launch_requests_total{launch=\"L1\",model=\"m1\"} 1\n"),
      "{text}"
    );
    // The other launch is outside the scope, so nothing of it is in the body.
    assert!(!text.contains("launch=\"L2\""), "{text}");
  }

  /// `launch_info` describes the scope like every other series: a scrape that
  /// named one model does not list the daemon's other live launches.
  #[test]
  fn metrics_launch_info_respects_the_scope() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    request(&log, "/models/b.gguf", "L2", 1, 1);
    let live = [
      live_row("L1", "m1", "/models/a.gguf", 1111),
      live_row("L2", "m2", "/models/b.gguf", 1112),
    ];
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/a.gguf".into(),
      },
      path: Some("/models/a.gguf".into()),
      launch: None,
      name: Some("m1".into()),
      per_launch: true,
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &live, &scope, 1, 2);
    let text = doc.render();
    assert!(
      text.contains("llamastash:launch_info{launch=\"L1\",model=\"m1\",port=\"1111\"} 1\n"),
      "{text}"
    );
    assert!(
      !text.contains("launch=\"L2\""),
      "one model's scrape lists one launch:\n{text}"
    );
  }

  #[test]
  fn metrics_an_in_flight_request_has_a_model_series_before_it_finishes() {
    let log = RequestLog::new();
    let mut rec = log.begin("/v1/chat/completions", None);
    rec.set_model("m1", "/models/a.gguf");
    rec.set_launch("L1");
    rec.publish();
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    let text = doc.render();
    // The record has to stay alive: dropping it finishes the row. The request
    // that pays for a model load must read as busy work while it runs, or the
    // first request to a model is invisible on a fresh daemon — the one case
    // the gauge exists for.
    assert!(
      text.contains("llamastash:requests_in_flight{model=\"m1\"} 1\n"),
      "{text}"
    );
    assert!(
      text.contains("llamastash:requests_total{model=\"m1\"} 0\n"),
      "{text}"
    );
  }

  #[test]
  fn metrics_in_flight_requests_from_one_host_are_one_host() {
    let log = RequestLog::new();
    // Two tools on one machine, each with a request still streaming. The
    // records have to stay alive: dropping one finishes the row.
    let streaming: Vec<crate::proxy::request_log::RequestRecord> = [5001u16, 5002]
      .into_iter()
      .map(|port| {
        let mut rec = log.begin(
          "/v1/chat/completions",
          Some(format!("127.0.0.1:{port}").parse().unwrap()),
        );
        rec.set_model("m1", "/models/a.gguf");
        rec.set_launch("L1");
        rec.publish();
        rec
      })
      .collect();
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    let text = doc.render();
    assert!(
      text.contains("llamastash:requests_in_flight{model=\"m1\"} 2\n"),
      "{text}"
    );
    assert!(
      text.contains("llamastash:hosts_in_flight{model=\"m1\"} 1\n"),
      "{text}"
    );
    drop(streaming);
  }

  /// A live launch that has not served a request yet answers its own scrape
  /// with zeros, not with a missing series: an alerting rule written against a
  /// loaded model must read `0`, and every label set stays the one that name
  /// uses once rows exist.
  #[test]
  fn metrics_a_live_launch_with_no_rows_answers_its_zeros() {
    let log = RequestLog::new();
    let live = [live_row("L1", "m1", "/models/a.gguf", 1111)];
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Launch { id: "L1".into() },
      path: Some("/models/a.gguf".into()),
      launch: Some("L1".into()),
      name: Some("m1".into()),
      per_launch: true,
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &live, &scope, 1, 1);
    let text = doc.render();
    for line in [
      "llamastash:requests_total{model=\"m1\"} 0\n",
      "llamastash:requests_in_flight{model=\"m1\"} 0\n",
      "llamastash:hosts_in_flight{model=\"m1\"} 0\n",
      "llamastash:prompt_tokens_total{model=\"m1\"} 0\n",
      "llamastash:launch_requests_total{launch=\"L1\",model=\"m1\"} 0\n",
    ] {
      assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    // Averages stay omitted: a mean has no figure to print from no rows, and
    // the launch gauge's absence still means "stopped", never "zero".
    assert!(!text.contains("request_duration_seconds_avg"), "{text}");
    assert!(!text.contains("launch_requests_in_flight"), "{text}");
  }

  /// The same zeros through the model scope: `?model=` names the live launch's
  /// model, and the scrape prints that model's series at zero.
  #[test]
  fn metrics_a_live_model_with_no_rows_answers_its_zeros() {
    let log = RequestLog::new();
    let live = [live_row("L1", "m1", "/models/a.gguf", 1111)];
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/a.gguf".into(),
      },
      path: Some("/models/a.gguf".into()),
      name: Some("m1".into()),
      per_launch: true,
      ..Scope::default()
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &live, &scope, 1, 1);
    let text = doc.render();
    for line in [
      "llamastash:requests_total{model=\"m1\"} 0\n",
      "llamastash:requests_in_flight{model=\"m1\"} 0\n",
      "llamastash:launch_info{launch=\"L1\",model=\"m1\",port=\"1111\"} 1\n",
    ] {
      assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
  }

  /// A live model with no rows still takes its label from the whole log, not
  /// from what the scope can see: a stopped model that already used the display
  /// name keeps it, so the idle one answers under its path.
  #[test]
  fn metrics_an_idle_model_label_avoids_a_name_already_used_in_the_log() {
    let log = RequestLog::new();
    request_named(&log, "m", "/models/a.gguf", "L1", 10, 5);
    log.forget_launch("L1");
    let live = [live_row("L2", "m", "/models/b.gguf", 2222)];
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/b.gguf".into(),
      },
      path: Some("/models/b.gguf".into()),
      name: Some("m".into()),
      per_launch: false,
      ..Scope::default()
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &live, &scope, 1, 1);
    let text = doc.render();
    assert!(
      text.contains("llamastash:requests_total{model=\"/models/b.gguf\"} 0\n"),
      "the idle model answers under its path:\n{text}"
    );
    assert!(
      !text.contains("{model=\"m\"} 0"),
      "the zeros must not borrow the stopped model's label:\n{text}"
    );
  }

  /// `none` is the label of the requests no model answered, so an idle model
  /// that displays as `none` answers under its path, as it will once it has rows.
  #[test]
  fn metrics_an_idle_model_named_none_answers_under_its_path() {
    let log = RequestLog::new();
    let live = [live_row("L1", NO_MODEL, "/models/none.gguf", 1111)];
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/none.gguf".into(),
      },
      path: Some("/models/none.gguf".into()),
      name: Some(NO_MODEL.into()),
      ..Scope::default()
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &live, &scope, 1, 1);
    let text = doc.render();
    assert!(
      text.contains("llamastash:requests_total{model=\"/models/none.gguf\"} 0\n"),
      "{text}"
    );
    assert!(!text.contains("{model=\"none\"}"), "{text}");
  }

  /// JSON rows carry `path` beside `model`, so `model` stays the display name
  /// on an idle row too, even where the text format falls back to the path.
  #[test]
  fn metrics_json_idle_row_keeps_the_display_name() {
    let log = RequestLog::new();
    request_named(&log, "m", "/models/a.gguf", "L1", 10, 5);
    log.forget_launch("L1");
    let live = [live_row("L2", "m", "/models/b.gguf", 2222)];
    let snap = log.snapshot();
    let parsed = Parsed {
      format: Format::Json,
      scope: Scope {
        log: LogScope::Model {
          path: "/models/b.gguf".into(),
        },
        path: Some("/models/b.gguf".into()),
        name: Some("m".into()),
        ..Scope::default()
      },
      limit: 10,
    };
    let body = json_body(&snap, &live, &parsed, 1, 2);
    let rows = body["models"].as_array().expect("models");
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["model"], "m", "{body}");
    assert_eq!(rows[0]["path"], "/models/b.gguf", "{body}");
  }

  /// A scope naming a launch the daemon does not have keeps answering nothing
  /// but the daemon gauges: the zeros exist because the scope names a live
  /// launch, not because a scrape asks nicely.
  #[test]
  fn metrics_an_unscoped_scrape_does_not_invent_per_launch_zeros() {
    let log = RequestLog::new();
    let live = [live_row("L1", "m1", "/models/a.gguf", 1111)];
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &live, &Scope::default(), 1, 1);
    let text = doc.render();
    assert!(!text.contains("launch_requests_total"), "{text}");
    assert!(!text.contains("model=\"m1\""), "{text}");
  }

  /// The `# HELP`, `# TYPE` and sample lines one metric name prints, verbatim.
  fn metric_block<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = text.lines().collect();
    let help = format!("# HELP {name} ");
    let at = lines
      .iter()
      .position(|line| line.starts_with(&help))
      .unwrap_or_else(|| panic!("{name} is not in the scrape:\n{text}"));
    let rest = &lines[at + 2..];
    let end = rest
      .iter()
      .position(|line| !line.starts_with(name))
      .unwrap_or(rest.len());
    lines[at..at + 2 + end].to_vec()
  }

  /// The four series that carry nothing but a `model` label print one
  /// `# HELP`/`# TYPE` pair for both the attributed and the `none` copy.
  #[test]
  fn the_core_series_declare_one_help_and_type_for_both_model_labels() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    // One request still streaming on a model, one that has reached no model.
    // Both records have to stay alive: dropping one finishes the row.
    let mut on_model = log.begin(
      "/v1/chat/completions",
      Some("127.0.0.1:5001".parse().unwrap()),
    );
    on_model.set_model("m1", "/models/a.gguf");
    on_model.set_launch("L1");
    on_model.publish();
    let no_model = log.begin(
      "/v1/chat/completions",
      Some("127.0.0.1:5002".parse().unwrap()),
    );
    no_model.publish();
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    let text = doc.render();

    assert_eq!(
      metric_block(&text, "llamastash:requests_total"),
      [
        "# HELP llamastash:requests_total Requests the proxy finished, since daemon start",
        "# TYPE llamastash:requests_total counter",
        "llamastash:requests_total{model=\"m1\"} 1",
        "llamastash:requests_total{model=\"none\"} 0",
      ]
    );
    assert_eq!(
      metric_block(&text, "llamastash:request_errors_total"),
      [
        "# HELP llamastash:request_errors_total Finished requests that failed or were cut short",
        "# TYPE llamastash:request_errors_total counter",
        "llamastash:request_errors_total{model=\"m1\"} 0",
        "llamastash:request_errors_total{model=\"none\"} 0",
      ]
    );
    assert_eq!(
      metric_block(&text, "llamastash:requests_in_flight"),
      [
        "# HELP llamastash:requests_in_flight Requests the proxy is still serving",
        "# TYPE llamastash:requests_in_flight gauge",
        "llamastash:requests_in_flight{model=\"m1\"} 1",
        "llamastash:requests_in_flight{model=\"none\"} 1",
      ]
    );
    assert_eq!(
      metric_block(&text, "llamastash:hosts_in_flight"),
      [
        "# HELP llamastash:hosts_in_flight Distinct client hosts the proxy is serving right now",
        "# TYPE llamastash:hosts_in_flight gauge",
        "llamastash:hosts_in_flight{model=\"m1\"} 1",
        "llamastash:hosts_in_flight{model=\"none\"} 1",
      ]
    );
  }

  #[test]
  fn metrics_colliding_model_names_fall_back_to_paths() {
    let log = RequestLog::new();
    request(&log, "/models/a/x.gguf", "L1", 1, 1);
    request(&log, "/models/b/x.gguf", "L2", 2, 1);
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 2, 2);
    let text = doc.render();
    assert!(text.contains("llamastash:requests_total{model=\"/models/a/x.gguf\"} 1\n"));
    assert!(text.contains("llamastash:requests_total{model=\"/models/b/x.gguf\"} 1\n"));
  }

  #[test]
  fn metrics_a_model_named_none_does_not_take_the_reserved_label() {
    let log = RequestLog::new();
    request_named(&log, "none", "/models/none.gguf", "L1", 1, 1);
    rejected(&log, "/v1/chat/completions", 404);
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    let text = doc.render();
    // `none` already belongs to the requests that reached no model, and one
    // metric name cannot carry the same label set twice in a scrape.
    assert!(
      text.contains("llamastash:requests_total{model=\"/models/none.gguf\"} 1\n"),
      "{text}"
    );
    assert_eq!(
      text
        .matches("llamastash:requests_total{model=\"none\"}")
        .count(),
      1,
      "one metric name cannot carry a label set twice:\n{text}"
    );
  }

  /// One finished request that reached no model: the proxy answered it itself.
  fn rejected(log: &RequestLog, route: &str, status: u16) {
    let mut rec = log.begin(route, None);
    rec.set_status(status);
    rec.finish(RequestState::Done, Usage::default());
  }

  #[test]
  fn metrics_unattributed_requests_get_a_model_none_series() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    rejected(&log, "/v1/chat/completions", 404);
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    let text = doc.render();
    for line in [
      "llamastash:requests_total{model=\"none\"} 1\n",
      "llamastash:request_errors_total{model=\"none\"} 1\n",
      "llamastash:requests_in_flight{model=\"none\"} 0\n",
      "llamastash:hosts_in_flight{model=\"none\"} 0\n",
      "llamastash:requests_total{model=\"m1\"} 1\n",
    ] {
      assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }

    // A model scope answers for that model, so the unattributed share sits
    // outside it; the log-wide totals stay in `summary` either way.
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/a.gguf".into(),
      },
      path: Some("/models/a.gguf".into()),
      name: Some("m1".into()),
      ..Scope::default()
    };
    let mut scoped = Document::new(true);
    render(&mut scoped, &snap, &[], &scope, 1, 1);
    let scoped_text = scoped.render();
    assert!(
      !scoped_text.contains("model=\"none\""),
      "a model scope is that model's numbers:\n{scoped_text}"
    );
    assert!(scoped_text.contains("llamastash:requests_total{model=\"m1\"} 1\n"));
  }

  #[test]
  fn metrics_no_model_none_series_when_everything_is_attributed() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 1, 1);
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    // Nothing to say, so no zero-filled series beside the real ones.
    assert!(!doc.render().contains("model=\"none\""));
  }

  #[test]
  fn metrics_model_label_does_not_change_with_the_scope() {
    // Two models that share a display name, so both print as their catalog path.
    let log = RequestLog::new();
    request(&log, "/models/a/x.gguf", "L1", 1, 1);
    request(&log, "/models/b/x.gguf", "L2", 2, 1);
    let snap = log.snapshot();
    let render_with = |scope: Scope| {
      let mut doc = Document::new(true);
      render(&mut doc, &snap, &[], &scope, 2, 2);
      doc.render()
    };
    let unscoped = render_with(Scope::default());
    let scoped = render_with(Scope {
      log: LogScope::Model {
        path: "/models/a/x.gguf".into(),
      },
      path: Some("/models/a/x.gguf".into()),
      ..Scope::default()
    });
    let want = "llamastash:requests_total{model=\"/models/a/x.gguf\"} 1\n";
    assert!(unscoped.contains(want), "unscoped:\n{unscoped}");
    assert!(scoped.contains(want), "scoped:\n{scoped}");
    assert!(
      !scoped.contains("{model=\"m1\"}"),
      "scoping to one of two colliding names must not rename it back:\n{scoped}"
    );
  }

  #[test]
  fn metrics_retired_launches_keep_totals_but_not_the_in_flight_gauge() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 1, 1);
    log.forget_launch("L1");
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(
      &mut doc,
      &snap,
      &[],
      &Scope {
        per_launch: true,
        ..Scope::default()
      },
      0,
      1,
    );
    let text = doc.render();
    assert!(
      text.contains("llamastash:launch_requests_total{launch=\"L1\",model=\"m1\"} 1\n"),
      "a stopped launch keeps its totals:\n{text}"
    );
    // In-flight is a live quantity, so a stopped launch prints no zero for it.
    assert!(
      !text.contains("llamastash:launch_requests_in_flight"),
      "in-flight is live-only:\n{text}"
    );
    // Hosts are a live quantity too, and one metric name carries one label
    // set, so there is no per-launch host series to print.
    assert!(
      !text.contains("hosts_in_flight{launch"),
      "no per-launch host series:\n{text}"
    );
    assert!(
      !text.contains("llamastash:launch_info"),
      "live-only:\n{text}"
    );
  }

  /// Every `llamastash:*` name the two metrics modules can emit, read from the
  /// module source ahead of its test module.
  fn emitted_series_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for module in ["metrics.rs", "metrics_backend.rs"] {
      let path = format!("{}/src/proxy/{module}", env!("CARGO_MANIFEST_DIR"));
      let src = std::fs::read_to_string(&path).expect("module source");
      let src = src.split("#[cfg(test)]").next().unwrap_or(&src);
      let mut rest = src;
      while let Some(at) = rest.find("\"llamastash:") {
        rest = &rest[at + 1..];
        names.insert(
          rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_'))
            .collect(),
        );
      }
    }
    names
  }

  /// Every `llamastash:*` name `docs/usage.md` lists in its two Metrics sections.
  fn documented_series_names() -> BTreeSet<String> {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/usage.md"))
      .expect("usage.md");
    let start = doc
      .find("### Metrics (`/metrics`)")
      .expect("a Metrics section");
    let rest = &doc[start..];
    let end = rest
      .find("### Ollama-compat surface")
      .expect("a section after the two Metrics ones");
    let section = &rest[..end];
    let mut names = BTreeSet::new();
    let mut at = 0;
    while let Some(found) = section[at..].find("llamastash:") {
      let from = at + found + "llamastash:".len();
      names.insert(format!(
        "llamastash:{}",
        section[from..]
          .chars()
          .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_'))
          .collect::<String>()
      ));
      at = from;
    }
    names
  }

  #[test]
  fn metrics_series_names_match_the_docs() {
    let emitted = emitted_series_names();
    let documented = documented_series_names();
    let undocumented: Vec<&String> = emitted.difference(&documented).collect();
    let never_emitted: Vec<&String> = documented.difference(&emitted).collect();
    assert!(
      undocumented.is_empty() && never_emitted.is_empty(),
      "docs/usage.md and the code disagree; undocumented {undocumented:?}, \
       listed but never emitted {never_emitted:?}"
    );
  }

  /// One finished request that knows its prefill and cache figures.
  fn prefill_request(log: &RequestLog) {
    let mut rec = log.begin("/v1/chat/completions", None);
    rec.set_model("m1", "/models/a.gguf");
    rec.set_launch("L1");
    rec.set_status(200);
    rec.mark_first_byte();
    rec.finish(
      RequestState::Done,
      Usage {
        prompt_tokens: Some(500),
        completion_tokens: Some(4),
        tokens_per_second: Some(40.0),
        prefill_tokens: Some(100),
        prefill_tokens_per_second: Some(650.0),
        cached_tokens: Some(400),
        ..Usage::default()
      },
    );
  }

  fn text_of(snap: &LogSnapshot) -> String {
    let mut doc = Document::new(true);
    render(&mut doc, snap, &[], &Scope::default(), 0, 0);
    doc.render()
  }

  #[test]
  fn metrics_publish_prefill_and_cache_counters() {
    let log = RequestLog::new();
    prefill_request(&log);
    let text = text_of(&log.snapshot());
    assert!(
      text.contains("llamastash:prompt_tokens_cached_total{model=\"m1\"} 400\n"),
      "{text}"
    );
    assert!(
      text.contains("llamastash:prefill_tokens_total{model=\"m1\"} 100\n"),
      "{text}"
    );
    // 100 tokens at 650.0/s.
    assert!(
      text.contains("llamastash:prefill_seconds_total{model=\"m1\"} 0.15384615384615385\n"),
      "{text}"
    );
    assert!(
      text.contains("llamastash:request_prefill_tokens_per_second{model=\"m1\"} 650\n"),
      "{text}"
    );
  }

  #[test]
  fn metrics_count_an_estimated_prefill_like_any_other() {
    // A /v1/messages stream carries no prefill timing, so the figure behind these
    // samples is the proxy's estimate. It counts, and nothing flags it.
    let log = RequestLog::new();
    let mut rec = log.begin("/v1/messages", None);
    rec.set_model("m1", "/models/a.gguf");
    rec.set_launch("L1");
    rec.set_status(200);
    rec.mark_first_byte();
    rec.finish(
      RequestState::Done,
      Usage {
        prompt_tokens: Some(4_000),
        completion_tokens: Some(20),
        tokens_per_second: Some(50.0),
        prefill_tokens: Some(4_000),
        prefill_tokens_per_second: Some(96.5),
        prefill_estimated: true,
        ..Usage::default()
      },
    );
    let text = text_of(&log.snapshot());
    assert!(
      text.contains("llamastash:prefill_tokens_total{model=\"m1\"} 4000\n"),
      "{text}"
    );
    assert!(
      text.contains("llamastash:request_prefill_tokens_per_second{model=\"m1\"} 96.5\n"),
      "{text}"
    );
  }

  #[test]
  fn metrics_omit_the_prefill_gauge_without_a_speed() {
    // The counters are always there and read 0; a gauge of a mean with nothing to
    // average would read as an instant prefill rather than an unknown one.
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    let text = text_of(&log.snapshot());
    assert!(text.contains("llamastash:prompt_tokens_cached_total{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:prefill_tokens_total{model=\"m1\"} 0\n"));
    assert!(!text.contains("llamastash:request_prefill_tokens_per_second"));
  }

  #[test]
  fn metrics_json_carries_the_prefill_and_cache_summary_fields() {
    let log = RequestLog::new();
    prefill_request(&log);
    let snap = log.snapshot();
    let parsed = Parsed {
      format: Format::Json,
      scope: Scope::default(),
      limit: 10,
    };
    let body = json_body(&snap, &[], &parsed, 1, 2);
    assert_eq!(body["summary"]["cached_tokens"], 400);
    assert_eq!(body["summary"]["prefill_tokens_per_second"], 650.0);
    assert_eq!(body["summary"]["cache_hit_ratio"], 0.8);
    assert_eq!(body["models"][0]["cached_tokens"], 400);
    assert_eq!(body["requests"][0]["prefill_tokens"], 100);
  }

  #[test]
  fn metrics_json_shape_carries_the_requests_contract_keys() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    let snap = log.snapshot();
    let parsed = Parsed {
      format: Format::Json,
      scope: Scope::default(),
      limit: 10,
    };
    let body = json_body(&snap, &[], &parsed, 1, 2);
    assert_eq!(body["models_loaded"], 1);
    assert_eq!(body["summary"]["requests"], 1);
    assert_eq!(body["requests"].as_array().unwrap().len(), 1);
    // The row keys are the ones `requests --json` returns.
    assert!(body["requests"][0].get("launch_id").is_some());
    assert_eq!(body["models"][0]["path"], "/models/a.gguf");
    assert_eq!(body["models"][0]["requests"], 1);
    assert_eq!(body["launches"][0]["launch"], "L1");
    assert_eq!(body["launches"][0]["live"], false);
    assert_eq!(body["scope"]["launch"], Value::Null);
  }

  #[test]
  fn metrics_text_omits_an_average_with_nothing_to_average() {
    // A failed request still counts towards the duration counter, but the mean is
    // only over successful requests. Printing a made-up zero would read as fast.
    let log = RequestLog::new();
    let mut rec = log.begin("/v1/chat/completions", None);
    rec.set_model("m1", "/models/a.gguf");
    rec.set_launch("L1");
    rec.set_status(500);
    rec.finish(RequestState::Done, Usage::default());
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 0, 0);
    let text = doc.render();
    assert!(text.contains("llamastash:request_errors_total{model=\"m1\"} 1\n"));
    assert!(text.contains("llamastash:request_duration_seconds_total{model=\"m1\"} 0\n"));
    assert!(!text.contains("llamastash:request_duration_seconds_avg"));
    assert!(!text.contains("llamastash:request_ttft_seconds_avg"));
    assert!(!text.contains("llamastash:request_tokens_per_second"));
  }

  #[test]
  fn metrics_document_sorts_and_skips_non_finite_values() {
    let mut doc = Document::new(true);
    doc.sample("b:name", "second", "gauge", &[], 1.0);
    doc.sample("a:name", "first", "counter", &[("k", "z")], 2.0);
    doc.sample("a:name", "first", "counter", &[("k", "a")], 3.0);
    doc.sample("c:name", "nan", "gauge", &[], f64::NAN);
    let text = doc.render();
    assert_eq!(
      text,
      "# HELP a:name first\n# TYPE a:name counter\na:name{k=\"a\"} 3\na:name{k=\"z\"} 2\n\
       # HELP b:name second\n# TYPE b:name gauge\nb:name 1\n"
    );
  }

  #[test]
  fn metrics_document_keeps_the_first_help_and_type() {
    let mut doc = Document::new(false);
    doc.declare("llamacpp:x", Some("first help"), Some("counter"));
    doc.declare("llamacpp:x", Some("second help"), Some("gauge"));
    doc.sample_raw("llamacpp:x", String::new(), 5.0);
    assert_eq!(
      doc.render(),
      "# HELP llamacpp:x first help\n# TYPE llamacpp:x counter\nllamacpp:x 5\n"
    );
    assert_eq!(doc.samples(), 1);
  }

  /// An engine may declare a family under one name and print its samples under
  /// suffixed ones — a `prompt_tokens_total` sample under the declared
  /// `prompt_tokens`. The sample keeps the name the engine printed and inherits
  /// the family's type: a counter that arrives under a suffixed name is still a
  /// counter, so the text prints one `# TYPE` naming one family.
  #[test]
  fn metrics_an_engine_keeps_the_family_a_suffixed_sample_belongs_to() {
    let mut doc = Document::new(true);
    doc.declare(
      "engine:prompt_tokens",
      Some("prompt tokens"),
      Some("counter"),
    );
    doc.sample_raw("engine:prompt_tokens_total", String::new(), 4.0);
    doc.sample(
      "llamastash:prompt_tokens_total",
      "prompt tokens",
      "counter",
      &[("model", "m1")],
      4.0,
    );
    assert_eq!(
      doc.render(),
      concat!(
        "# HELP engine:prompt_tokens prompt tokens\n",
        "# TYPE engine:prompt_tokens counter\n",
        "engine:prompt_tokens_total 4\n",
        // Our own counter is a separate family and declares itself, HELP and
        // TYPE included.
        "# HELP llamastash:prompt_tokens_total prompt tokens\n",
        "# TYPE llamastash:prompt_tokens_total counter\n",
        "llamastash:prompt_tokens_total{model=\"m1\"} 4\n",
      )
    );
  }

  /// The type an engine declared holds for every name under that family. A
  /// histogram prints `_sum` and `_count` samples like a summary does, and
  /// calling them `untyped` because their own name was never declared makes the
  /// whole family unusable to a scrape that only ingests typed series.
  #[test]
  fn metrics_an_engine_keeps_the_metric_type_it_declared() {
    let mut doc = Document::new(false);
    doc.declare("engine:ttft", None, Some("histogram"));
    doc.sample_raw("engine:ttft_bucket", "le=\"0.1\"".to_string(), 1.0);
    doc.sample_raw("engine:ttft_sum", String::new(), 0.05);
    doc.sample_raw("engine:ttft_count", String::new(), 1.0);
    assert_eq!(
      doc.render(),
      concat!(
        "# TYPE engine:ttft histogram\n",
        "engine:ttft_bucket{le=\"0.1\"} 1\n",
        "engine:ttft_sum 0.05\n",
        "engine:ttft_count 1\n",
      )
    );
  }

  #[test]
  fn metrics_escapes_only_the_escapes_prometheus_allows() {
    // A quoted label value accepts `\\`, `\"` and `\n` and nothing else, so a
    // tab and a carriage return go out as themselves: `\t` is the illegal form.
    assert_eq!(escape_label("a\"b\\c\nd\te\rf"), "a\\\"b\\\\c\\nd\te\rf");
    // HELP text has no `\"`, so a quote in one is a plain quote and a strict
    // parser still reads the line.
    assert_eq!(escape_help("say \"hi\"\\y\nz\tw"), "say \"hi\"\\\\y\\nz\tw");
    assert_eq!(fmt_value(2.0), "2");
    assert_eq!(fmt_value(0.25), "0.25");
  }

  #[test]
  fn metrics_escapes_our_own_text_once_and_an_engines_not_at_all() {
    let mut doc = Document::new(true);
    // Our strings are Rust text with real characters, so escaping them once, in
    // the type that reads them, is what makes the line legal: a backslash grows,
    // a tab and a carriage return go out as themselves (`\t` is the illegal
    // form), and a HELP line has no escape for a quote, so a quote stays one.
    doc.sample(
      "a:name",
      "quote \" and a \\ backslash",
      "gauge",
      &[("k", "v\"x\t\r")],
      1.0,
    );
    // An engine's HELP arrived in wire form: the `\n` in it is two bytes
    // already, so printing it verbatim is the whole round trip. Escaping it a
    // second time is what a parser then reads as a literal backslash-n.
    doc.declare(
      "b:name",
      Some(r#"time to first \n second"#),
      Some("counter"),
    );
    doc.sample_raw("b:name", String::new(), 2.0);
    let text = doc.render();
    assert!(
      text.contains(r#"# HELP a:name quote " and a \\ backslash"#),
      "{text}"
    );
    assert!(text.contains("a:name{k=\"v\\\"x\t\r\"} 1\n"), "{text}");
    assert!(
      text.contains(r#"# HELP b:name time to first \n second"#),
      "an engine HELP survives one round trip unchanged:\n{text}"
    );
  }
}
