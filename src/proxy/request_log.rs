//! In-memory log of the model requests the proxy handled.
//!
//! One row per request on a forwarded `/v1/*` route: clock fields, the
//! routing outcome, and the token counts read from the end of the response
//! (see [`super::usage_tap`]). No prompt or response text is stored.
//!
//! Rows live in a ring of [`CAPACITY`]. Totals per model and per launch are
//! kept apart from the ring, so a summary still counts requests whose rows
//! have been pushed out. The ring and the totals are lost when the daemon
//! stops. [`RequestLog::write_to`] also appends each finished row to a file.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::usage_tap::Usage;

/// Rows kept. At roughly 400 bytes a row this is well under 1 MiB.
pub const CAPACITY: usize = 1000;

/// Rows one `requests_tail` call returns when it names no `limit`.
pub const DEFAULT_TAIL: usize = 100;

/// A caller's row limit: the default when it named none, never more than the
/// ring holds, so one query cannot ask for more rows than exist.
pub(crate) fn clamp_limit(raw: Option<usize>) -> usize {
  raw.unwrap_or(DEFAULT_TAIL).min(CAPACITY)
}

/// Name of the request log file inside the daemon's log directory.
pub const FILE_NAME: &str = "requests.jsonl";

/// How many stopped launches keep their totals. A launch id is never reused,
/// so totals deleted the moment a launch stops read as a counter reset to
/// anything that keeps reading them.
pub const RETIRED_LAUNCH_HISTORY: usize = 16;

/// Finished rows that may wait for the file writer. Past this the line is
/// dropped from the file (the in-memory log still has the row) rather
/// than holding up the request that produced it.
const FILE_QUEUE: usize = 1024;

/// The file the log is also written to.
struct FileSink {
  path: PathBuf,
  queue: mpsc::Sender<String>,
  /// Finished rows that did not reach the file: the queue was full, or
  /// the file could not be opened or written.
  dropped: AtomicU64,
}

/// What `status` reports about the request log file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileStatus {
  pub path: PathBuf,
  /// Finished rows that were not written to the file.
  pub dropped: u64,
}

/// Append every line from `rx` to the rotating file at `path`, until the
/// file cannot be opened or written. A line taken off the queue and not
/// written is added to `lost`.
async fn write_lines(
  path: &Path,
  rx: &mut mpsc::Receiver<String>,
  lost: &mut u64,
) -> std::io::Result<()> {
  let mut writer = crate::daemon::supervisor::LogWriter::open(path.to_path_buf()).await?;
  while let Some(line) = rx.recv().await {
    if let Err(e) = writer.write_line(line.as_bytes()).await {
      *lost += 1;
      return Err(e);
    }
  }
  Ok(())
}

/// Strings that come from a client or a child process are cut to these
/// lengths, and stripped of control characters, before they are stored.
const MAX_MODEL_CHARS: usize = 200;
const MAX_CAUSE_CHARS: usize = 300;

/// How a request ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
  /// No response yet, or the response body is still streaming.
  #[default]
  InFlight,
  /// The whole response was sent.
  Done,
  /// The client went away before the response was complete.
  ClientClosed,
  /// The upstream connection failed part-way through the response body.
  UpstreamError,
}

/// One logged request. Every key is always present on the wire; a value
/// the proxy does not know is `null`. Reading tolerates a missing key, so
/// a client and a daemon one field apart still understand each other.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestRow {
  /// Counts up from 1 for the life of the daemon.
  pub seq: u64,
  /// Unix time in milliseconds when the proxy received the request.
  pub started_at_ms: u64,
  /// Peer address of the client connection (`ip:port`).
  pub client: Option<String>,
  /// Request path, e.g. `/v1/chat/completions`.
  pub route: String,
  /// The `model` string the client sent.
  pub requested_model: Option<String>,
  /// Name of the model the row belongs to: the one that served the request,
  /// or the one asked for when nothing served it.
  pub model: Option<String>,
  /// Catalog path of that model. `requests_tail` filters on it.
  pub model_path: Option<String>,
  /// Launch that served the request.
  pub launch_id: Option<String>,
  pub state: RequestState,
  /// HTTP status sent to the client.
  pub status: Option<u16>,
  /// The proxy's error `code`, or its `type` when it has no code, when the
  /// proxy answered the request itself.
  pub error: Option<String>,
  /// That error's message.
  pub cause: Option<String>,
  /// This request spawned the model's launch. Not set when the spawn was
  /// refused, or when the request waited on a launch something else started.
  pub auto_start: bool,
  /// A `proxy.aliases` entry is what routed the request: `requested_model`
  /// holds the alias the client sent, `model` the model it names. Kept as a
  /// flag rather than read off `requested_model != model` so a fallback row
  /// still says the client used an alias.
  pub alias: bool,
  /// The request arrived on the proxy's `/ui` web page instead of an API
  /// client. `route` holds the canonical `/v1/...` path it was served under.
  pub ui: bool,
  /// Launches unloaded to make room for this request's auto-start.
  pub evicted: Vec<String>,
  /// Why another model answered (`launch_failed` / `family_mismatch`).
  pub fallback: Option<String>,
  /// Milliseconds from receipt to the first response body byte.
  pub ttfb_ms: Option<u64>,
  /// Milliseconds from receipt to the end of the response.
  pub duration_ms: Option<u64>,
  pub prompt_tokens: Option<u64>,
  pub completion_tokens: Option<u64>,
  /// Generation speed: what the server reported, or the proxy's estimate.
  pub tokens_per_second: Option<f64>,
  /// The server reported no speed, so the proxy worked it out from the
  /// token count and its own clock.
  pub tokens_per_second_estimated: bool,
  /// Prompt tokens the engine computed, with the ones it reused from the KV
  /// cache left out.
  pub prefill_tokens: Option<u64>,
  /// How fast those tokens were computed, reported by the engine or estimated
  /// by the proxy.
  pub prefill_tokens_per_second: Option<f64>,
  /// Milliseconds from receipt to the first byte that carried a generated
  /// token, which is where prompt computation ended. A stream's first byte can
  /// be response headers flushed before the prompt was computed, so
  /// [`Self::ttfb_ms`] cannot mark that end.
  pub prefill_end_ms: Option<u64>,
  /// No prefill timing reached the proxy, so [`Self::prefill_tokens_per_second`]
  /// is its own estimate.
  pub prefill_estimated: bool,
  /// Prompt tokens served from the KV cache.
  pub cached_tokens: Option<u64>,
  /// [`Self::cached_tokens`] over the prompt it was served from, clamped to
  /// that prompt. `None` when the row reports only one half of the pair or has
  /// no prompt to divide by, which is not the same answer as `0`.
  pub cache_hit_ratio: Option<f64>,
}

/// A row as table-cell text. The CLI table and the TUI tab both render
/// from it, so one field cannot be formatted two ways.
pub struct RequestCells {
  /// `HH:MM:SS` the request arrived, in local time.
  pub time: String,
  pub status: String,
  pub total: String,
  pub ttfb: String,
  /// Prefixed with `~` when the speed is the proxy's estimate.
  pub speed: String,
  /// Prompt-processing speed, `~` when it is the proxy's estimate.
  pub prefill: String,
  pub tokens_in: String,
  /// Cached share of the prompt, `48.8%`.
  pub cache_hit: String,
  pub tokens_out: String,
  /// The route without its `/v1/` prefix, which every logged route has.
  pub route: String,
  pub model: String,
  /// The `model` the client sent. Differs from `model` on an alias or a
  /// fallback, where something else answered.
  pub request_for: String,
  pub launch: String,
  pub client: String,
  /// See [`RequestRow::note`]. Empty for a plain served request.
  pub note: String,
}

impl RequestRow {
  /// Counted as an error in a summary: a 4xx / 5xx answer, or a response
  /// the upstream cut short. A client that hung up is not an error.
  pub fn is_error(&self) -> bool {
    self.state == RequestState::UpstreamError || self.status.is_some_and(|s| s >= 400)
  }

  fn is_success(&self) -> bool {
    self.state == RequestState::Done && self.status.is_some_and(|s| s < 400)
  }

  /// The row's cells. `none` stands in for a value the log does not have.
  pub fn cells(&self, none: &str) -> RequestCells {
    let or_none = |v: Option<String>| v.unwrap_or_else(|| none.to_string());
    let (hour, min, sec) = crate::util::datetime::local_hms(self.started_at_ms / 1000);
    RequestCells {
      time: format!("{hour:02}:{min:02}:{sec:02}"),
      status: or_none(self.status.map(|s| s.to_string())),
      total: or_none(self.duration_ms.map(fmt_ms)),
      ttfb: or_none(self.ttfb_ms.map(fmt_ms)),
      speed: or_none(self.tokens_per_second.map(|t| {
        let mark = if self.tokens_per_second_estimated {
          "~"
        } else {
          ""
        };
        format!("{mark}{}", fmt_tps(t))
      })),
      prefill: or_none(self.prefill_tokens_per_second.map(|t| {
        let mark = if self.prefill_estimated { "~" } else { "" };
        format!("{mark}{}", fmt_tps(t))
      })),
      tokens_in: or_none(self.prompt_tokens.map(fmt_count)),
      cache_hit: or_none(
        self
          .cache_hit_ratio
          // A row from a daemon older than the field still carries both counts,
          // so the cell divides them rather than showing the no-data placeholder.
          .or_else(|| cache_hit_ratio(self.cached_tokens, self.prompt_tokens))
          .map(fmt_pct),
      ),
      tokens_out: or_none(self.completion_tokens.map(fmt_count)),
      route: self
        .route
        .strip_prefix("/v1/")
        .unwrap_or(&self.route)
        .to_string(),
      model: or_none(self.model.clone().or_else(|| self.requested_model.clone())),
      request_for: or_none(self.requested_model.clone()),
      launch: or_none(self.launch_id.clone()),
      client: or_none(self.client.clone()),
      note: self.note(),
    }
  }

  /// What happened beyond the status code, on one line: the auto-start,
  /// what was unloaded for it, a fallback, the proxy's own error, or how
  /// the response was cut short. Empty for a plain served request.
  pub fn note(&self) -> String {
    let mut parts: Vec<String> = Vec::new();
    match self.state {
      // The model is known and no launch has taken the request yet.
      RequestState::InFlight if self.model_path.is_some() && self.launch_id.is_none() => {
        parts.push("loading model".to_string());
      }
      RequestState::InFlight => parts.push("in flight".to_string()),
      RequestState::ClientClosed => parts.push("client closed".to_string()),
      RequestState::UpstreamError => parts.push("upstream error".to_string()),
      RequestState::Done => {}
    }
    if self.alias {
      parts.push("alias".to_string());
    }
    if self.ui {
      parts.push("ui".to_string());
    }
    if self.auto_start && self.state != RequestState::InFlight {
      parts.push("auto-start".to_string());
    }
    if !self.evicted.is_empty() {
      parts.push(format!("unloaded {}", self.evicted.join(" ")));
    }
    if let Some(reason) = &self.fallback {
      parts.push(format!("fallback ({reason})"));
    }
    if let Some(error) = &self.error {
      parts.push(match &self.cause {
        Some(cause) => format!("{error}: {cause}"),
        None => error.clone(),
      });
    }
    parts.join(", ")
  }
}

/// The cached share of one prompt, clamped to the prompt it was served from.
/// Needs both halves and a prompt to divide by, so a row that reported one
/// reads `None` rather than `0`. [`Totals::add`] applies the same rule to the
/// scope figures, which is what ties a row's share and the summary's to one
/// another.
fn cache_hit_ratio(cached: Option<u64>, prompt: Option<u64>) -> Option<f64> {
  let (Some(cached), Some(prompt)) = (cached, prompt) else {
    return None;
  };
  (prompt > 0).then(|| cached.min(prompt) as f64 / prompt as f64)
}

/// A share for a table cell: `48.8%`.
fn fmt_pct(ratio: f64) -> String {
  format!("{:.1}%", ratio * 100.0)
}

/// A token count for a table cell: `9999`, `12.3k`, `1.2M`.
fn fmt_count(n: u64) -> String {
  // One decimal of thousands rounds to `1000.0k` from here up.
  const MILLIONS_FROM: u64 = 999_950;
  if n < 10_000 {
    n.to_string()
  } else if n < MILLIONS_FROM {
    format!("{:.1}k", n as f64 / 1_000.0)
  } else {
    format!("{:.1}M", n as f64 / 1_000_000.0)
  }
}

/// A speed for a table cell: one decimal below 1000, a whole number to
/// 99,999.4, and whole thousands with a `k` from there up, so `~` plus the
/// value always fits the six cells the column gives it.
pub(crate) fn fmt_tps(v: f64) -> String {
  // One decimal of thousands rounds up to `1000.0` from here, which costs a
  // cell more than the column has.
  const WHOLE_FROM: f64 = 999.95;
  // A whole number runs past the column from here, so thousands take a `k`.
  const KILO_FROM: f64 = 99_999.5;
  if v < WHOLE_FROM {
    format!("{v:.1}")
  } else if v < KILO_FROM {
    format!("{v:.0}")
  } else {
    format!("{:.0}k", v / 1_000.0)
  }
}

/// A duration for a table cell: `850ms`, `1.2s`, `2m05s`, `1h02m`.
pub(crate) fn fmt_ms(ms: u64) -> String {
  let secs = ms / 1000;
  if ms < 1000 {
    format!("{ms}ms")
  } else if secs < 60 {
    format!("{:.1}s", ms as f64 / 1000.0)
  } else if secs < 3600 {
    format!("{}m{:02}s", secs / 60, secs % 60)
  } else {
    format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
  }
}

/// Totals of the finished requests of one model, one launch, or the whole
/// log, since the daemon started. Averages cover completed 2xx / 3xx
/// requests only, so a fast 404 does not pull the latency down.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestSummary {
  pub requests: u64,
  pub errors: u64,
  pub avg_duration_ms: Option<u64>,
  pub avg_ttfb_ms: Option<u64>,
  /// Mean of the per-request speeds, estimated ones included.
  pub tokens_per_second_avg: Option<f64>,
  /// Tokens per second across the whole scope: every completion token the
  /// rows it retains carry over the wall time they were generating. Where
  /// [`Self::tokens_per_second_avg`] answers how fast one stream runs, this
  /// answers how fast the box does, so four tools streaming at once move it
  /// and not the mean. Computed over the rows the ring retains, so it ages
  /// with the table rather than with the daemon. Every finished row that
  /// generated tokens counts, whatever it ended as.
  pub tokens_per_second_aggregate: Option<f64>,
  /// The busy wall [`Self::tokens_per_second_aggregate`] divides by, in ms:
  /// the union of its rows' generation windows, so time two tools spent
  /// generating at once counts once. Present exactly when it is.
  pub busy_ms: Option<u64>,
  /// Mean speed of the newest launch of the scoped model only: what this
  /// session is running at, with a reloaded model no older launch pulling
  /// the average. Set for a model scope only; `None` for the whole log and
  /// for a scope no request has reported a speed for yet.
  pub tokens_per_second_session: Option<f64>,
  /// Prompt tokens per second across the scope: every prefill token the rows
  /// carry over the seconds they took. Weighted by tokens rather than the mean
  /// of the per-row speeds, because an agent turn prefills a few hundred
  /// uncached tokens at a low per-token rate while a cold long prompt runs
  /// much faster, and a plain mean would mostly report the small turns. Engine
  /// figures and estimates both count, and so do rows however they ended.
  pub prefill_tokens_per_second: Option<f64>,
  /// Prompt tokens per second across the box: every prefill token the retained
  /// rows carry over the wall time prompts were being computed, a second two
  /// requests computed in counted once. Where
  /// [`Self::prefill_tokens_per_second`] answers how fast one prompt ran,
  /// this answers how fast the box prefills, so it rises when requests compute
  /// prompts together. With no overlap it is the same number as the weighted
  /// mean, which is why the strips only print it when it differs.
  pub prefill_tokens_per_second_aggregate: Option<f64>,
  /// The prompt-computation wall
  /// [`Self::prefill_tokens_per_second_aggregate`] divides by, in ms. Present
  /// exactly when it is.
  pub prefill_busy_ms: Option<u64>,
  /// The most recent speed a request reported. Kept in the JSON and in the
  /// row table, but no longer on the summary strips: the session figure says
  /// the same thing about what is running now.
  pub tokens_per_second_last: Option<f64>,
  /// Distinct client IPs among the rows the scope retains. Loopback tools
  /// all share `127.0.0.1`, so this counts machines and reads 1 until a LAN
  /// bind; [`Self::connections`] is the figure that counts tools.
  pub hosts: Option<u64>,
  /// Distinct client peers on `ip:port` among the retained rows — what one
  /// tool's live connection looks like to the proxy.
  pub connections: Option<u64>,
  /// [`Self::hosts`] over the requests in flight right now.
  pub live_hosts: Option<u64>,
  /// [`Self::connections`] over the requests in flight right now.
  pub live_connections: Option<u64>,
  pub prompt_tokens: u64,
  /// Prompt tokens served from the KV cache, on the rows that report a prompt
  /// count as well. A row with a cached count and no prompt count is in neither
  /// half of the pair below.
  pub cached_tokens: u64,
  /// [`Self::cached_tokens`] over the prompt tokens they were served from.
  /// `None` when no row reported both, which is not the same answer as `0`.
  pub cache_hit_ratio: Option<f64>,
  pub completion_tokens: u64,
  /// Requests that started the model.
  pub auto_starts: u64,
  /// Launches unloaded to make room.
  pub evictions: u64,
}

/// Whether a summary keeps an aggregate figure that reads as the average beside
/// it. A launch nothing runs together makes the two the same number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregates {
  /// Print every aggregate the data supports, so the block has the same cells
  /// whatever is running.
  Always,
  /// Leave out one that prints its own average or lands within a few percent of
  /// it, since then they are one figure.
  WhenTheyDiffer,
}

impl RequestSummary {
  /// Label and text of the figures, in display order. `none` stands in for a
  /// figure no request has supplied yet. The two `Agg` figures drop out entirely
  /// when nothing produced them, and follow `agg` for the rest.
  pub fn cells(&self, none: &str, agg: Aggregates) -> Vec<(&'static str, String)> {
    let [counts, speeds_and_totals] = self.cell_rows(none, agg);
    counts.into_iter().chain(speeds_and_totals).collect()
  }

  /// [`Self::cells`] split into the two rows the terminal shows: what happened
  /// (Live, hosts, counts) and how fast it went (speeds, latencies, token
  /// totals). Each row wraps on its own, so a narrow pane keeps the split rather
  /// than letting one row's tail push the other's figures around.
  pub fn cell_rows(&self, none: &str, agg: Aggregates) -> [Vec<(&'static str, String)>; 2] {
    let ms = |v: Option<u64>| v.map(fmt_ms).unwrap_or_else(|| none.to_string());
    let tps = |v: Option<f64>| v.map(fmt_tps).unwrap_or_else(|| none.to_string());
    let num = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_else(|| none.to_string());
    let counts = self
      .live_cell()
      .into_iter()
      .chain([
        ("Hosts", num(self.hosts)),
        ("Connections", num(self.connections)),
        ("Requests", self.requests.to_string()),
        ("Errors", self.errors.to_string()),
        ("Auto-starts", self.auto_starts.to_string()),
        ("Unloaded", self.evictions.to_string()),
      ])
      .collect();
    let speeds_and_totals = [("Avg tok/s", tps(self.tokens_per_second_avg))]
      .into_iter()
      .chain(
        self
          .tokens_per_second_session
          .map(|t| ("Tok/s session", fmt_tps(t))),
      )
      .chain(Self::aggregate_cell(
        self.tokens_per_second_avg,
        self.tokens_per_second_aggregate,
        self.busy_ms,
        "Agg tok/s",
        agg,
      ))
      .chain([("Avg PP tok/s", tps(self.prefill_tokens_per_second))])
      .chain(Self::aggregate_cell(
        self.prefill_tokens_per_second,
        self.prefill_tokens_per_second_aggregate,
        self.prefill_busy_ms,
        "Agg PP tok/s",
        agg,
      ))
      .chain([
        ("Avg total", ms(self.avg_duration_ms)),
        ("Avg TTFB", ms(self.avg_ttfb_ms)),
        ("Tokens in", fmt_count(self.prompt_tokens)),
        (
          "Cached",
          match self.cache_hit_ratio {
            Some(ratio) => format!("{} ({})", fmt_count(self.cached_tokens), fmt_pct(ratio)),
            None => none.to_string(),
          },
        ),
        ("Tokens out", fmt_count(self.completion_tokens)),
      ])
      .collect();
    [counts, speeds_and_totals]
  }

  /// The `X over <wall>` cell of an aggregate figure: absent when nothing
  /// produced it, and otherwise subject to `agg`.
  fn aggregate_cell(
    avg: Option<f64>,
    aggregate: Option<f64>,
    busy_ms: Option<u64>,
    label: &'static str,
    agg: Aggregates,
  ) -> Option<(&'static str, String)> {
    let (Some(value), Some(busy_ms)) = (aggregate, busy_ms) else {
      return None;
    };
    let printed = fmt_tps(value);
    let text = || format!("{printed} over {}", fmt_ms(busy_ms));
    if agg == Aggregates::Always {
      return Some((label, text()));
    }
    // Speeds this close read as one number at a glance: a lone stream lands
    // here, and so does a hair of overlap between two.
    const NOISE: f64 = 0.05;
    // An average that never landed prints the placeholder, so it is not the
    // same number and the aggregate stands on its own.
    let differs = match avg {
      // Under the first decimal `fmt_tps` collapses distinct speeds onto one
      // print, so the relative test alone would print a cell repeating its own
      // average. The matching print is its own answer.
      Some(avg) if avg > 0.0 => printed != fmt_tps(avg) && (value - avg).abs() / avg > NOISE,
      _ => true,
    };
    differs.then(|| (label, text()))
  }

  /// The in-flight pair as one cell: `Live N` when every host in flight is on
  /// one connection and `Live N/M` when they differ (an agent runner with two
  /// tools open reads `1/2`). Absent when nothing is in flight, so an idle
  /// launch's strip looks the way it did before the figure existed, and
  /// absent from a daemon that never computed it.
  fn live_cell(&self) -> Option<(&'static str, String)> {
    let (hosts, connections) = (self.live_hosts?, self.live_connections?);
    (hosts > 0 || connections > 0).then(|| {
      let text = if hosts == connections {
        hosts.to_string()
      } else {
        format!("{hosts}/{connections}")
      };
      ("Live", text)
    })
  }
}

/// How many hosts and `ip:port` peers a set of rows came from. Both figures
/// count rows a scope still retains rather than the since-daemon-start
/// totals, so they describe the table on screen and do not grow as the daemon
/// ages.
#[derive(Debug, Clone, Default)]
pub struct Clients {
  requests: u64,
  hosts: HashSet<String>,
  connections: HashSet<String>,
}

impl Clients {
  fn add(&mut self, row: &RequestRow) {
    self.requests += 1;
    let Some(client) = &row.client else {
      return;
    };
    self.connections.insert(client.clone());
    self.hosts.insert(client_host(client).to_string());
  }

  /// Rows counted, in flight ones included.
  pub fn requests(&self) -> u64 {
    self.requests
  }

  /// Distinct client IPs.
  pub fn hosts(&self) -> u64 {
    self.hosts.len() as u64
  }

  /// Distinct `ip:port` peers.
  pub fn connections(&self) -> u64 {
    self.connections.len() as u64
  }
}

/// The throughput of a scope, read off the rows the ring retains.
///
/// The mean of the per-request speeds answers how fast one stream runs. This
/// answers how fast the box does: every token the counted rows carry over the
/// wall time they were generating, so four tools streaming at once move it
/// where the mean cannot.
#[derive(Default)]
struct Throughput {
  /// Generation windows as `(start, end)` in ms, on the rows' own clock.
  windows: Vec<(u64, u64)>,
  tokens: u64,
}

impl Throughput {
  /// Take one row's prompt-computation contribution: the tokens the engine had
  /// to compute, over the window it spent computing them. That window ends at
  /// the first generated token, so it sits at the front of the request where
  /// [`Self::add`]'s window sits at the back.
  ///
  /// An engine-reported `prompt_ms` is compute time and carries no queue wait,
  /// while a proxy estimate is measured from receipt, so an estimated window is
  /// longer than the compute inside it by whatever the request waited. A scope
  /// mixing the two divides by a wall that runs long, and its aggregate reads
  /// below the average it belongs to.
  fn add_prefill(&mut self, row: &RequestRow) {
    let (Some(tokens), Some(pps)) = (row.prefill_tokens, row.prefill_tokens_per_second) else {
      return;
    };
    if row.state == RequestState::InFlight || tokens == 0 || pps <= 0.0 {
      return;
    }
    // Where prompt computation ended. The proxy watching a stream's first token
    // arrive is the direct answer. Failing that: a response that arrives whole
    // sends its first byte at the end of generation, so the prompt finished a
    // generation-span earlier, and a stream whose tokens the proxy could not
    // spot is about at its first byte. The earlier of that and the first byte
    // lands on the end of prompt computation for both.
    let wait = row.prefill_end_ms.unwrap_or_else(|| {
      let generated = match (row.completion_tokens, row.tokens_per_second) {
        (Some(generated), Some(tps)) if generated > 0 && tps > 0.0 => {
          (generated as f64 / tps * 1000.0).round() as u64
        }
        _ => 0,
      };
      let ttfb = row.ttfb_ms.unwrap_or(0);
      row
        .duration_ms
        .unwrap_or(ttfb)
        .saturating_sub(generated)
        .min(ttfb)
    });
    let end = row.started_at_ms.saturating_add(wait);
    let span = (tokens as f64 / pps * 1000.0).round() as u64;
    let start = end.saturating_sub(span).max(row.started_at_ms);
    if start >= end {
      return;
    }
    self.push(start, end, tokens);
  }

  /// Take one row's contribution. A row counts when it has a speed, a duration
  /// and generated tokens — the three the figure is made of — so in-flight rows
  /// and rows that never saw a usage block are in neither sum, and a newer idle
  /// request cannot pull the figure down. The status is no filter: a generation
  /// the client cut short or the upstream broke off still put those tokens out,
  /// and llama.cpp folds an aborted slot's tokens into its own counters, so
  /// counting them is what makes this agree with the engine.
  fn add(&mut self, row: &RequestRow) {
    let (Some(tps), Some(duration_ms), Some(tokens)) = (
      row.tokens_per_second,
      row.duration_ms,
      row.completion_tokens,
    ) else {
      return;
    };
    if row.state == RequestState::InFlight || tps <= 0.0 || tokens == 0 || duration_ms == 0 {
      return;
    }
    // The row ends when its response did, and the speed on it says how much of
    // that was generation. Anchoring the span to the end and clamping it into
    // the request is what keeps a load that precedes generation, or a
    // non-streamed request whose first byte is the whole wait, from stretching
    // or collapsing the wall.
    let end = row.started_at_ms.saturating_add(duration_ms);
    let span = (tokens as f64 / tps * 1000.0).round() as u64;
    let start = end.saturating_sub(span).max(row.started_at_ms);
    if start >= end {
      return;
    }
    self.push(start, end, tokens);
  }

  fn push(&mut self, start: u64, end: u64, tokens: u64) {
    self.windows.push((start, end));
    self.tokens = self.tokens.saturating_add(tokens);
  }

  /// `(tokens per second, busy wall ms)`, both `None` when no row counted.
  ///
  /// The wall is the union of the windows: a second with four streams running
  /// is one second of the box being busy, not four. Sorting is in place: a
  /// snapshot takes this once per scope while it holds the log lock.
  fn figures(&mut self) -> (Option<f64>, Option<u64>) {
    if self.windows.is_empty() {
      return (None, None);
    }
    self.windows.sort_unstable();
    let mut busy: u64 = 0;
    let mut run = self.windows[0];
    for &(start, end) in &self.windows[1..] {
      if start > run.1 {
        busy = busy.saturating_add(run.1 - run.0);
        run = (start, end);
      } else {
        run.1 = run.1.max(end);
      }
    }
    busy = busy.saturating_add(run.1 - run.0);
    if busy == 0 {
      return (None, None);
    }
    (Some(self.tokens as f64 * 1000.0 / busy as f64), Some(busy))
  }
}

/// The figures a summary can only get from the rows a scope retains: its
/// clients and its throughput. Both [`RequestLog::tail`] and
/// [`RequestLog::snapshot`] stamp these, so the two paths cannot differ.
#[derive(Default)]
struct RowFigures {
  all: Clients,
  live: Clients,
  throughput: Throughput,
  prefill: Throughput,
}

impl RowFigures {
  fn add(&mut self, row: &RequestRow) {
    self.all.add(row);
    if row.state == RequestState::InFlight {
      self.live.add(row);
    }
    self.throughput.add(row);
    self.prefill.add_prefill(row);
  }

  /// Stamp the row-derived figures onto `summary`. A daemon that computes the
  /// client figures always answers, so a scope with no rows left reads `Some(0)`
  /// and only an older daemon leaves them `None`. The two throughput figures are
  /// `None` until a row of the scope has both a token count and a speed.
  fn stamp(&mut self, summary: &mut RequestSummary) {
    summary.hosts = Some(self.all.hosts());
    summary.connections = Some(self.all.connections());
    summary.live_hosts = Some(self.live.hosts());
    summary.live_connections = Some(self.live.connections());
    let (tps, busy_ms) = self.throughput.figures();
    summary.tokens_per_second_aggregate = tps;
    summary.busy_ms = busy_ms;
    let (prefill_tps, prefill_busy_ms) = self.prefill.figures();
    summary.prefill_tokens_per_second_aggregate = prefill_tps;
    summary.prefill_busy_ms = prefill_busy_ms;
  }
}

/// The address part of a client string, which is always a `SocketAddr`'s text
/// (`127.0.0.1:5000`, `[::1]:5000`). Anything that does not carry a port is
/// its own host.
fn client_host(client: &str) -> &str {
  let host = match client.rsplit_once(':') {
    Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => host,
    _ => client,
  };
  host.trim_matches(&['[', ']'][..])
}

#[derive(Debug, Clone, Default)]
struct Totals {
  requests: u64,
  errors: u64,
  duration_ms_sum: u64,
  ttfb_ms_sum: u64,
  /// Requests in the two sums above.
  timed: u64,
  tps_sum: f64,
  tps_n: u64,
  tps_last: Option<f64>,
  /// The two halves of the token-weighted prefill speed: the tokens counted
  /// and the seconds they took.
  prefill_tokens: u64,
  prefill_seconds: f64,
  /// Cached tokens, and the prompt tokens they were served from. One row
  /// supplies both halves of the hit ratio or neither.
  cached_tokens: u64,
  cached_prompt_tokens: u64,
  prompt_tokens: u64,
  completion_tokens: u64,
  auto_starts: u64,
  evictions: u64,
  /// Display name of the model these rows belong to, taken from them, so a
  /// launch's figures and the model's print the same name.
  label: Option<String>,
}

impl Totals {
  fn add(&mut self, row: &RequestRow) {
    if row.model.is_some() {
      self.label = row.model.clone();
    }
    self.requests += 1;
    if row.is_error() {
      self.errors += 1;
    }
    if let (true, Some(total), Some(ttfb)) = (row.is_success(), row.duration_ms, row.ttfb_ms) {
      self.duration_ms_sum = self.duration_ms_sum.saturating_add(total);
      self.ttfb_ms_sum = self.ttfb_ms_sum.saturating_add(ttfb);
      self.timed += 1;
    }
    if let Some(tps) = row.tokens_per_second {
      self.tps_sum += tps;
      self.tps_n += 1;
      self.tps_last = Some(tps);
    }
    // A row needs both halves to contribute: a token count with no speed would
    // add seconds out of nothing.
    if let (Some(tokens), Some(tps)) = (row.prefill_tokens, row.prefill_tokens_per_second) {
      if tokens > 0 && tps > 0.0 {
        self.prefill_tokens = self.prefill_tokens.saturating_add(tokens);
        self.prefill_seconds += tokens as f64 / tps;
      }
    }
    if let (Some(cached), Some(prompt)) = (row.cached_tokens, row.prompt_tokens) {
      // One row fills both halves or neither, so the count printed beside the
      // ratio is the count the ratio used, and no row can claim it reused more
      // of a prompt than the prompt it reports.
      self.cached_tokens = self.cached_tokens.saturating_add(cached.min(prompt));
      self.cached_prompt_tokens = self.cached_prompt_tokens.saturating_add(prompt);
    }
    self.prompt_tokens = self
      .prompt_tokens
      .saturating_add(row.prompt_tokens.unwrap_or(0));
    self.completion_tokens = self
      .completion_tokens
      .saturating_add(row.completion_tokens.unwrap_or(0));
    self.auto_starts += u64::from(row.auto_start);
    self.evictions += row.evicted.len() as u64;
  }

  fn summary(&self) -> RequestSummary {
    RequestSummary {
      requests: self.requests,
      errors: self.errors,
      avg_duration_ms: self.duration_ms_sum.checked_div(self.timed),
      avg_ttfb_ms: self.ttfb_ms_sum.checked_div(self.timed),
      tokens_per_second_avg: (self.tps_n > 0).then(|| self.tps_sum / self.tps_n as f64),
      prefill_tokens_per_second: (self.prefill_seconds > 0.0)
        .then(|| self.prefill_tokens as f64 / self.prefill_seconds),
      cached_tokens: self.cached_tokens,
      cache_hit_ratio: (self.cached_prompt_tokens > 0)
        .then(|| self.cached_tokens as f64 / self.cached_prompt_tokens as f64),
      // `RequestLog::tail` fills this for a model scope; a single set of
      // totals has no notion of "the newest launch".
      tokens_per_second_session: None,
      tokens_per_second_last: self.tps_last,
      prefill_tokens_per_second_aggregate: None,
      prefill_busy_ms: None,
      // Both throughput figures come from the rows behind the totals, which a
      // set of totals no longer holds; `tail` and `snapshot` fill them in.
      tokens_per_second_aggregate: None,
      busy_ms: None,
      // The client figures count rows, which a set of totals no longer holds.
      // `RequestLog::tail` and `snapshot` fill them in.
      hosts: None,
      connections: None,
      live_hosts: None,
      live_connections: None,
      prompt_tokens: self.prompt_tokens,
      completion_tokens: self.completion_tokens,
      auto_starts: self.auto_starts,
      evictions: self.evictions,
    }
  }
}

#[derive(Default)]
struct Inner {
  rows: VecDeque<RequestRow>,
  next_seq: u64,
  all: Totals,
  // Finished requests per request path. The router logs only the fixed paths
  // it matches exactly, so this cannot grow past the route table.
  by_route: HashMap<String, u64>,
  // One entry per model that got a request. Never removed: the keys are
  // catalog paths, so the map is as large as the catalog at most.
  by_model: HashMap<String, Totals>,
  // One entry per launch a request was handed to. A launch id is never
  // reused, so [`RequestLog::forget_launch`] moves the entry out when the
  // launch stops, or every auto-start would leave one behind.
  by_launch: HashMap<String, Totals>,
  // The entries `forget_launch` moved out, oldest first, capped at
  // [`RETIRED_LAUNCH_HISTORY`].
  retired: VecDeque<(String, Totals)>,
}

impl Inner {
  /// Totals for `scope`, or `None` for a scope nothing has contributed to.
  fn totals(&self, scope: &LogScope) -> Option<&Totals> {
    match scope {
      LogScope::All => Some(&self.all),
      LogScope::Model { path } => self.by_model.get(path),
      LogScope::Launch { id } => RequestLog::launch_totals(self, id),
    }
  }

  /// Overwrite the ring's copy of `row`, if the ring still holds it.
  fn store(&mut self, row: &RequestRow) {
    let Some(front) = self.rows.front().map(|r| r.seq) else {
      return;
    };
    // Rows are pushed in `seq` order and only ever popped from the front,
    // so a row's slot is its distance from the front.
    let Some(slot) = row
      .seq
      .checked_sub(front)
      .and_then(|i| self.rows.get_mut(i as usize))
    else {
      return;
    };
    *slot = row.clone();
  }
}

/// What `requests_tail` returns. It is the method's JSON body, so the
/// daemon writes it and the CLI and the TUI read it through this type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tail {
  pub summary: RequestSummary,
  /// Newest first.
  #[serde(rename = "requests")]
  pub rows: Vec<RequestRow>,
}

/// Shared handle to the log. Cheap to clone.
#[derive(Clone, Default)]
pub struct RequestLog {
  inner: Arc<Mutex<Inner>>,
  /// Set once [`Self::write_to`] has started a file writer.
  file: Arc<OnceLock<FileSink>>,
}

impl RequestLog {
  pub fn new() -> Self {
    Self::default()
  }

  // The lock is never held across an await and every critical section is a
  // few field writes, so a poisoned lock still guards consistent data.
  fn lock(&self) -> MutexGuard<'_, Inner> {
    self.inner.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// Also append every finished row to `path`, one JSON object per line
  /// with the keys `requests_tail` returns. The file rotates like a
  /// launch log. Call once, from inside the daemon's runtime.
  ///
  /// A failure to open or write the file is logged once and ends the
  /// writing, the way a launch's log capture pauses. Rows that do not
  /// reach the file are counted, and [`Self::file_status`] reports the
  /// count.
  pub fn write_to(&self, path: PathBuf) {
    let (queue, mut rx) = mpsc::channel::<String>(FILE_QUEUE);
    let sink = FileSink {
      path: path.clone(),
      queue,
      dropped: AtomicU64::new(0),
    };
    if self.file.set(sink).is_err() {
      return;
    }
    let file = Arc::clone(&self.file);
    tokio::spawn(async move {
      let mut lost = 0;
      if let Err(e) = write_lines(&path, &mut rx, &mut lost).await {
        log::warn!(
          "proxy: request log file {} failed: {e}; writing to it stopped (the in-memory log continues, `status` counts the rows dropped)",
          path.display()
        );
      }
      // Closing the queue makes every later row count as dropped at the
      // sender. The rows still in it will never be written either.
      rx.close();
      while rx.try_recv().is_ok() {
        lost += 1;
      }
      if let Some(installed) = file.get() {
        installed.dropped.fetch_add(lost, Ordering::Relaxed);
      }
    });
  }

  /// Where the log is also written and how many finished rows did not
  /// make it there. `None` when no file was asked for.
  pub fn file_status(&self) -> Option<FileStatus> {
    self.file.get().map(|sink| FileStatus {
      path: sink.path.clone(),
      dropped: sink.dropped.load(Ordering::Relaxed),
    })
  }

  /// Start a row for a request the proxy just received. The returned
  /// record finishes the row when it is dropped, so every exit path is
  /// logged, including a client that hangs up while a model loads.
  ///
  /// `route` is stored uncut: the router logs only the fixed paths it
  /// matches exactly, so a client cannot make it long.
  pub fn begin(&self, route: &str, client: Option<SocketAddr>) -> RequestRecord {
    let started_at_ms = crate::util::datetime::now_ms();
    let mut inner = self.lock();
    inner.next_seq += 1;
    let row = RequestRow {
      seq: inner.next_seq,
      started_at_ms,
      client: client.map(|c| c.to_string()),
      route: route.to_string(),
      ..RequestRow::default()
    };
    if inner.rows.len() >= CAPACITY {
      inner.rows.pop_front();
    }
    inner.rows.push_back(row.clone());
    RequestRecord {
      log: self.clone(),
      row,
      started: Instant::now(),
      finished: false,
    }
  }

  /// The newest `limit` rows of `scope`, newest first, and the summary for
  /// that same scope.
  pub fn tail(&self, scope: LogScope, limit: usize) -> Tail {
    let inner = self.lock();
    let totals = inner.totals(&scope);
    let mut summary = totals.map(Totals::summary).unwrap_or_default();
    let mut figures = RowFigures::default();
    for row in inner.rows.iter().filter(|row| scope.matches(row)) {
      figures.add(row);
    }
    figures.stamp(&mut summary);
    // The speed of the newest launch that served this model, so a model
    // reloaded at a different speed shows both numbers.
    if let LogScope::Model { .. } = scope {
      summary.tokens_per_second_session = inner
        .rows
        .iter()
        .rev()
        .find(|r| scope.matches(r))
        .and_then(|r| r.launch_id.as_deref())
        .and_then(|id| RequestLog::launch_totals(&inner, id))
        .and_then(|t| Totals::summary(t).tokens_per_second_avg);
    }
    Tail {
      summary,
      rows: inner
        .rows
        .iter()
        .rev()
        .filter(|r| scope.matches(r))
        .take(limit)
        .cloned()
        .collect(),
    }
  }

  /// Retire a stopped launch's totals. `status` only reads the totals of
  /// launches that are still registered; [`Self::launch_summary`] still
  /// answers for the last [`RETIRED_LAUNCH_HISTORY`] stopped launches.
  pub fn forget_launch(&self, launch_id: &str) {
    let mut inner = self.lock();
    let Some(totals) = inner.by_launch.remove(launch_id) else {
      return;
    };
    inner.retired.push_back((launch_id.to_string(), totals));
    while inner.retired.len() > RETIRED_LAUNCH_HISTORY {
      inner.retired.pop_front();
    }
  }

  /// Summary of the requests one launch served, live first then retired.
  ///
  /// This is [`Self::tail`] on a launch scope with no rows asked for, so the
  /// stamping `tail` does reaches the per-launch surfaces too.
  pub fn launch_summary(&self, launch_id: &str) -> RequestSummary {
    self
      .tail(
        LogScope::Launch {
          id: launch_id.to_string(),
        },
        0,
      )
      .summary
  }

  /// Display name of the model `launch_id` served, or `None` when no finished
  /// row of it carries one.
  pub fn launch_label(&self, launch_id: &str) -> Option<String> {
    Self::launch_totals(&self.lock(), launch_id).and_then(|totals| totals.label.clone())
  }

  fn launch_totals<'a>(inner: &'a Inner, launch_id: &str) -> Option<&'a Totals> {
    inner.by_launch.get(launch_id).or_else(|| {
      inner
        .retired
        .iter()
        .rev()
        .find(|(retired, _)| retired == launch_id)
        .map(|(_, totals)| totals)
    })
  }

  /// The whole log in owned data, taken under one lock. A `/metrics` scrape
  /// renders from this instead of holding the lock while it builds a body.
  pub fn snapshot(&self) -> LogSnapshot {
    let inner = self.lock();
    // The row figures of every scope, in one pass over the ring, so a
    // `/metrics` summary prints the same numbers as `requests --json` of the
    // same scope: both count these rows and nothing else.
    let mut figures = RowFigures::default();
    let mut model_figures: HashMap<&str, RowFigures> = HashMap::new();
    let mut launch_figures: HashMap<&str, RowFigures> = HashMap::new();
    for row in inner.rows.iter() {
      figures.add(row);
      if let Some(path) = row.model_path.as_deref() {
        model_figures.entry(path).or_default().add(row);
      }
      if let Some(launch) = row.launch_id.as_deref() {
        launch_figures.entry(launch).or_default().add(row);
      }
    }
    let view = |key: String, totals: &Totals| ScopeTotals {
      key,
      label: totals.label.clone(),
      summary: totals.summary(),
      duration_ms_sum: totals.duration_ms_sum,
      ttfb_ms_sum: totals.ttfb_ms_sum,
      timed: totals.timed,
      prefill_tokens: totals.prefill_tokens,
      prefill_seconds: totals.prefill_seconds,
    };
    let mut models: Vec<ScopeTotals> = inner
      .by_model
      .iter()
      .map(|(path, totals)| view(path.clone(), totals))
      .collect();
    models.sort_by(|a, b| a.key.cmp(&b.key));
    // A scope whose rows all left the ring has no counts of its own; zero is
    // the answer, not "not computed".
    for model in &mut models {
      model_figures
        .remove(model.key.as_str())
        .unwrap_or_default()
        .stamp(&mut model.summary);
    }
    let mut launches: Vec<LaunchTotals> = inner
      .by_launch
      .iter()
      .map(|(id, totals)| LaunchTotals {
        totals: view(id.clone(), totals),
        live: true,
      })
      .collect();
    launches.sort_by(|a, b| a.totals.key.cmp(&b.totals.key));
    launches.extend(inner.retired.iter().map(|(id, totals)| LaunchTotals {
      totals: view(id.clone(), totals),
      live: false,
    }));
    for launch in &mut launches {
      launch_figures
        .remove(launch.totals.key.as_str())
        .unwrap_or_default()
        .stamp(&mut launch.totals.summary);
    }
    let mut all = view(String::new(), &inner.all);
    figures.stamp(&mut all.summary);
    let mut routes: Vec<(String, u64)> = inner
      .by_route
      .iter()
      .map(|(route, n)| (route.clone(), *n))
      .collect();
    routes.sort();
    LogSnapshot {
      rows: inner.rows.iter().cloned().collect(),
      all,
      models,
      launches,
      routes,
    }
  }
}

/// One scope's totals as a `/metrics` scrape reads them: the summary
/// `requests --json` returns, plus the sums behind its averages. A counter
/// needs the total; a mean cannot be re-expanded into one.
#[derive(Debug, Clone)]
pub struct ScopeTotals {
  /// Catalog path for a model, launch id for a launch, empty for the whole log.
  pub key: String,
  /// Display name of the model these totals belong to.
  pub label: Option<String>,
  pub summary: RequestSummary,
  pub duration_ms_sum: u64,
  pub ttfb_ms_sum: u64,
  /// How many requests the two sums above cover. Not `summary.requests`: a sum
  /// only holds a request the proxy could time, so a mean has to divide by this,
  /// and an error or a cancellation moves the request count but neither sum.
  pub timed: u64,
  /// Prompt tokens the counted rows had to compute, and the seconds they took.
  /// A counter needs both: `rate(prefill_tokens_total) /
  /// rate(prefill_seconds_total)` is the prefill speed Prometheus reads.
  pub prefill_tokens: u64,
  pub prefill_seconds: f64,
}

/// A launch's totals, and whether the launch is still running.
pub struct LaunchTotals {
  pub totals: ScopeTotals,
  pub live: bool,
}

impl LogSnapshot {
  /// The [`Tail`] [`RequestLog::tail`] answers for the log this snapshot came
  /// from, taken from the snapshot instead of the log, so one `/metrics` JSON
  /// scrape reads the log once. The summaries carry the figures stamped when
  /// the snapshot was taken, so a scope whose rows have all left the ring
  /// answers the same zeros here, and a scope nothing reached the same zeros.
  pub fn tail(&self, scope: &LogScope, limit: usize) -> Tail {
    let scoped: Option<&ScopeTotals> = match scope {
      LogScope::All => Some(&self.all),
      LogScope::Model { path } => self.models.iter().find(|m| &m.key == path),
      LogScope::Launch { id } => self
        .launches
        .iter()
        .find(|l| &l.totals.key == id)
        .map(|l| &l.totals),
    };
    let mut summary = match scoped {
      Some(totals) => totals.summary.clone(),
      None => {
        let mut empty = RequestSummary::default();
        RowFigures::default().stamp(&mut empty);
        empty
      }
    };
    // The speed of the newest launch that served this model, which a stored
    // summary has no notion of on its own.
    if let LogScope::Model { .. } = scope {
      summary.tokens_per_second_session = self
        .rows
        .iter()
        .rev()
        .find(|r| scope.matches(r))
        .and_then(|r| r.launch_id.as_deref())
        .and_then(|id| self.launches.iter().find(|l| l.totals.key == id))
        .and_then(|l| l.totals.summary.tokens_per_second_avg);
    }
    Tail {
      summary,
      rows: self
        .rows
        .iter()
        .rev()
        .filter(|r| scope.matches(r))
        .take(limit)
        .cloned()
        .collect(),
    }
  }
}

/// Owned view of the log for one scrape.
pub struct LogSnapshot {
  /// The ring's rows, oldest first.
  pub rows: Vec<RequestRow>,
  /// Totals of every finished request.
  pub all: ScopeTotals,
  /// One entry per model a request reached, sorted by catalog path.
  pub models: Vec<ScopeTotals>,
  /// Live launches sorted by id, then the retired ones, oldest first.
  pub launches: Vec<LaunchTotals>,
  /// Finished requests per request path, sorted by path.
  pub routes: Vec<(String, u64)>,
}

impl LogSnapshot {
  /// The in-flight rows one scope holds: how many there are, and how many
  /// distinct hosts and peers they came from.
  pub fn in_flight(&self, mut pred: impl FnMut(&RequestRow) -> bool) -> Clients {
    let mut clients = Clients::default();
    for row in self
      .rows
      .iter()
      .filter(|row| row.state == RequestState::InFlight)
      .filter(|row| pred(row))
    {
      clients.add(row);
    }
    clients
  }

  /// Catalog path a launch served, from the rows that carry both. A retired
  /// launch whose rows left the ring has no entry.
  pub fn launch_path(&self, launch_id: &str) -> Option<&str> {
    self
      .rows
      .iter()
      .rev()
      .find(|row| row.launch_id.as_deref() == Some(launch_id))
      .and_then(|row| row.model_path.as_deref())
  }
}

/// Which requests a read covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogScope {
  /// Every request the log holds.
  All,
  /// One model's, across every launch that served it.
  Model { path: String },
  /// One launch's, live or retired. Strictly the launch's own rows: a request
  /// filed under the model that never reached this launch belongs to the
  /// model's figures, not here.
  Launch { id: String },
}

impl LogScope {
  fn matches(&self, row: &RequestRow) -> bool {
    match self {
      LogScope::All => true,
      LogScope::Model { path } => row.model_path.as_deref() == Some(path.as_str()),
      LogScope::Launch { id } => row.launch_id.as_deref() == Some(id.as_str()),
    }
  }
}

/// Write handle for one request's row. Setters change the record's own
/// copy; [`Self::publish`] and the finishing calls copy it into the log.
pub struct RequestRecord {
  log: RequestLog,
  row: RequestRow,
  started: Instant,
  finished: bool,
}

impl RequestRecord {
  pub fn set_requested_model(&mut self, model: Option<&str>) {
    self.row.requested_model = model.map(|m| clean(m, MAX_MODEL_CHARS));
  }

  /// Attribute the row to a model: its display name and catalog path.
  pub fn set_model(&mut self, name: &str, path: &str) {
    self.row.model = Some(clean(name, MAX_MODEL_CHARS));
    self.row.model_path = Some(path.to_string());
  }

  /// The request was handed to `launch_id`. This is also where the
  /// launch's totals entry is created; see [`Self::finish`] for why not
  /// there.
  pub fn set_launch(&mut self, launch_id: &str) {
    self.row.launch_id = Some(launch_id.to_string());
    self
      .log
      .lock()
      .by_launch
      .entry(launch_id.to_string())
      .or_default();
  }

  pub fn set_auto_start(&mut self) {
    self.row.auto_start = true;
  }

  /// Stamp that a `proxy.aliases` entry is what routed this request.
  pub fn set_alias(&mut self) {
    self.row.alias = true;
  }

  /// Stamp that this request came from the proxy's own `/ui` web page.
  pub fn set_ui(&mut self) {
    self.row.ui = true;
  }

  pub fn set_evicted(&mut self, launches: Vec<String>) {
    self.row.evicted = launches;
  }

  pub fn set_fallback(&mut self, reason: &str) {
    self.row.fallback = Some(reason.to_string());
  }

  /// The upstream answered with `status`; the body is about to stream.
  pub fn set_status(&mut self, status: u16) {
    self.row.status = Some(status);
  }

  /// The first response body byte arrived. Later calls change nothing.
  pub fn mark_first_byte(&mut self) {
    if self.row.ttfb_ms.is_none() {
      self.row.ttfb_ms = Some(self.elapsed_ms());
    }
  }

  /// The first body byte carrying a generated token arrived, so the prompt is
  /// computed. Later calls change nothing.
  pub fn mark_first_token(&mut self) {
    if self.row.prefill_end_ms.is_none() {
      self.row.prefill_end_ms = Some(self.elapsed_ms());
    }
  }

  /// Copy the row as it stands into the log, so a reader sees a request
  /// that is still waiting on a load or still streaming.
  ///
  /// A row that already names its model opens that model's totals entry here
  /// rather than leaving it to [`Self::finish`]: the first request to a model is
  /// the one that carries the whole load, and the per-model in-flight gauge
  /// needs an entry to sit on while that request is still running.
  pub fn publish(&self) {
    let mut inner = self.log.lock();
    inner.store(&self.row);
    if let Some(path) = &self.row.model_path {
      let totals = inner.by_model.entry(path.clone()).or_default();
      // Only the label, not `Totals::add`: the request has not finished. The
      // label has to be the display name now, or the first scrape of a model
      // prints a different `model` value than every scrape after it.
      if totals.label.is_none() {
        totals.label = self.row.model.clone();
      }
    }
  }

  /// The proxy answered the request itself, with `error` (its error code
  /// or type) and `cause` (the error message). Finishes the row.
  pub fn respond(mut self, status: u16, error: Option<&str>, cause: Option<&str>) {
    self.row.status = Some(status);
    self.row.error = error.map(str::to_string);
    self.row.cause = cause.map(|c| clean(c, MAX_CAUSE_CHARS));
    self.finish(RequestState::Done, Usage::default());
  }

  /// The response body ended. Stores the final row and adds it to the totals.
  pub fn finish(&mut self, state: RequestState, usage: Usage) {
    if self.finished {
      return;
    }
    self.finished = true;
    self.row.state = state;
    self.row.duration_ms = Some(self.elapsed_ms());
    self.row.prompt_tokens = usage.prompt_tokens;
    self.row.completion_tokens = usage.completion_tokens;
    self.row.tokens_per_second = usage.tokens_per_second;
    self.row.tokens_per_second_estimated = usage.tokens_per_second_estimated;
    self.row.prefill_tokens = usage.prefill_tokens;
    self.row.prefill_tokens_per_second = usage.prefill_tokens_per_second;
    self.row.prefill_estimated = usage.prefill_estimated;
    self.row.cached_tokens = usage.cached_tokens;
    self.row.cache_hit_ratio = cache_hit_ratio(usage.cached_tokens, usage.prompt_tokens);
    {
      let mut inner = self.log.lock();
      let inner = &mut *inner;
      inner.store(&self.row);
      inner.all.add(&self.row);
      *inner.by_route.entry(self.row.route.clone()).or_default() += 1;
      if let Some(path) = &self.row.model_path {
        inner
          .by_model
          .entry(path.clone())
          .or_default()
          .add(&self.row);
      }
      // No `or_default` here: a response can end after its launch was
      // stopped and forgotten, and must not bring the entry back.
      if let Some(totals) = self
        .row
        .launch_id
        .as_ref()
        .and_then(|launch| inner.by_launch.get_mut(launch))
      {
        totals.add(&self.row);
      }
    }
    if let Some(sink) = self.log.file.get() {
      let queued = serde_json::to_string(&self.row)
        .ok()
        .is_some_and(|line| sink.queue.try_send(line).is_ok());
      if !queued {
        sink.dropped.fetch_add(1, Ordering::Relaxed);
      }
    }
  }

  fn elapsed_ms(&self) -> u64 {
    self.started.elapsed().as_millis() as u64
  }
}

impl Drop for RequestRecord {
  fn drop(&mut self) {
    // Reached without a finishing call only when hyper dropped the request
    // future, which it does when the client disconnects.
    self.finish(RequestState::ClientClosed, Usage::default());
  }
}

/// `s` cut to `max_chars`, with every control character and every
/// invisible format character turned into a space. The model name is
/// whatever a client sent and a cause can quote a child's output, and both
/// are later printed to a terminal, where a newline breaks a table row and
/// an escape sequence could do worse.
fn clean(s: &str, max_chars: usize) -> String {
  s.chars()
    .take(max_chars)
    .map(|c| if is_invisible(c) { ' ' } else { c })
    .collect()
}

/// What a terminal hides or reorders rather than prints. `is_control()` is
/// the Cc category only; the rest is the Cf set — soft hyphen, zero-width and
/// bidi controls, word joiners and invisible operators, BOM, interlinear
/// annotations and tag characters — which hides or reorders what a row shows
/// and shifts every later column by the cells it claims for nothing.
fn is_invisible(c: char) -> bool {
  c.is_control()
    || matches!(
      c as u32,
      0x00ad
        | 0x061c
        | 0x180e
        | 0x200b..=0x200f
        | 0x202a..=0x202e
        | 0x2060..=0x2064
        | 0x2066..=0x2069
        | 0x206a..=0x206f
        | 0xfeff
        | 0xfff9..=0xfffb
        | 0xe0001
        | 0xe0020..=0xe007f
    )
}

#[cfg(test)]
mod tests {
  use super::*;

  const ROUTE: &str = "/v1/chat/completions";

  fn served(log: &RequestLog, path: &str, launch: &str, usage: Usage) -> u64 {
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", path);
    rec.set_launch(launch);
    rec.set_status(200);
    rec.mark_first_byte();
    let seq = rec.row.seq;
    rec.finish(RequestState::Done, usage);
    seq
  }

  /// One served request from one client peer.
  fn served_from(log: &RequestLog, path: &str, launch: &str, client: &str) {
    let mut rec = log.begin(ROUTE, Some(client.parse().unwrap()));
    rec.set_model("m", path);
    rec.set_launch(launch);
    rec.set_status(200);
    rec.mark_first_byte();
    rec.finish(RequestState::Done, Usage::default());
  }

  /// A current daemon always answers the client questions, so an empty scope
  /// reads computed zeros. `None` belongs to a daemon that never had them.
  fn zeros() -> RequestSummary {
    RequestSummary {
      hosts: Some(0),
      connections: Some(0),
      live_hosts: Some(0),
      live_connections: Some(0),
      ..RequestSummary::default()
    }
  }

  #[test]
  fn begin_shows_an_in_flight_row_and_finish_completes_it() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, Some("127.0.0.1:5000".parse().unwrap()));
    let tail = log.tail(LogScope::All, 10);
    assert_eq!(tail.rows.len(), 1);
    assert_eq!(tail.rows[0].state, RequestState::InFlight);
    assert_eq!(tail.rows[0].client.as_deref(), Some("127.0.0.1:5000"));
    assert_eq!(tail.summary.requests, 0, "the summary counts finished rows");

    rec.set_status(200);
    rec.finish(RequestState::Done, Usage::default());
    let done = log.tail(LogScope::All, 10);
    assert_eq!(done.rows[0].state, RequestState::Done);
    assert_eq!(done.rows[0].status, Some(200));
    assert!(done.rows[0].duration_ms.is_some());
    assert_eq!(done.summary.requests, 1);
  }

  #[test]
  fn setters_stay_private_until_published() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", "/m/a.gguf");
    assert_eq!(log.tail(LogScope::All, 10).rows[0].model_path, None);
    rec.publish();
    assert_eq!(
      log.tail(LogScope::All, 10).rows[0].model_path.as_deref(),
      Some("/m/a.gguf")
    );
  }

  #[test]
  fn publishing_a_model_row_opens_its_totals_entry() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", "/m/a.gguf");
    rec.publish();
    let snap = log.snapshot();
    // The in-flight gauge only has a series to sit on once the model has an
    // entry, and the entry has to carry the display name from the start or the
    // first scrape of a model prints a different `model` than later ones.
    assert_eq!(snap.models.len(), 1);
    assert_eq!(snap.models[0].key, "/m/a.gguf");
    assert_eq!(snap.models[0].label.as_deref(), Some("m"));
    assert_eq!(snap.models[0].summary.requests, 0);
    assert_eq!(snap.in_flight(|row| row.model_path.is_some()).requests(), 1);
  }

  #[test]
  fn hosts_and_connections_count_the_clients_of_the_rows_a_scope_keeps() {
    let log = RequestLog::new();
    // Three finished requests over three peers from two hosts, then two
    // requests still streaming from one host over two peers.
    served_from(&log, "/m/a.gguf", "L1", "127.0.0.1:5001");
    served_from(&log, "/m/a.gguf", "L1", "127.0.0.1:5002");
    served_from(&log, "/m/a.gguf", "L2", "192.168.1.7:6001");
    // The two live requests share a host but not a peer, which is the case
    // the pair exists for: one machine, two tools talking. The records have
    // to stay alive — dropping one finishes the row.
    let streaming: Vec<RequestRecord> = [6002u16, 6003]
      .into_iter()
      .map(|port| {
        let mut rec = log.begin(ROUTE, Some(format!("192.168.1.7:{port}").parse().unwrap()));
        rec.set_model("m", "/m/a.gguf");
        rec.set_launch("L2");
        // Setters stay private to the record until the row is published.
        rec.publish();
        rec
      })
      .collect();

    let s = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        0,
      )
      .summary;
    assert_eq!((s.requests, s.hosts, s.connections), (3, Some(2), Some(5)));
    assert_eq!((s.live_hosts, s.live_connections), (Some(1), Some(2)));

    // A launch scope sees its own clients only: L1 never met the LAN host.
    let l1 = log.launch_summary("L1");
    assert_eq!(
      (l1.hosts, l1.connections, l1.live_hosts, l1.live_connections),
      (Some(1), Some(2), Some(0), Some(0))
    );

    // An IPv6 loopback is its own host, brackets and all.
    served_from(&log, "/m/b.gguf", "L3", "[::1]:5001");
    let all = log.tail(LogScope::All, 0).summary;
    assert_eq!((all.hosts, all.connections), (Some(3), Some(6)));
    drop(streaming);
  }

  #[test]
  fn an_empty_scope_answers_zero_where_an_old_daemon_answers_nothing() {
    let log = RequestLog::new();
    let idle = log.tail(
      LogScope::Model {
        path: "/m/none.gguf".to_string(),
      },
      10,
    );
    assert_eq!(idle.summary, zeros());
    assert_eq!(log.launch_summary("L9"), zeros());
    // With nothing in flight the live pair is not displayed at all.
    assert!(!idle
      .summary
      .cells("—", Aggregates::WhenTheyDiffer)
      .iter()
      .any(|(label, _)| *label == "Live"));
  }

  #[test]
  fn a_summary_from_an_older_daemon_leaves_the_client_figures_out() {
    // The wire shape grew; a summary written before it grew must not read as
    // "zero hosts", and must not gain a Live cell.
    let old: RequestSummary = serde_json::from_str(r#"{"requests": 2}"#).unwrap();
    assert_eq!((old.hosts, old.connections), (None, None));
    assert_eq!((old.live_hosts, old.live_connections), (None, None));
    let cells = old.cells("—", Aggregates::WhenTheyDiffer);
    assert_eq!(cells[0], ("Hosts", "—".to_string()));
    assert!(!cells.iter().any(|(label, _)| *label == "Live"));
  }

  #[test]
  fn the_live_pair_reads_as_one_number_when_hosts_and_peers_agree() {
    let cells = |hosts, connections| {
      RequestSummary {
        live_hosts: Some(hosts),
        live_connections: Some(connections),
        ..RequestSummary::default()
      }
      .cells("—", Aggregates::WhenTheyDiffer)
    };
    assert_eq!(
      cells(2, 2).iter().find(|c| c.0 == "Live").unwrap().1,
      "2".to_string()
    );
    assert_eq!(
      cells(1, 2).iter().find(|c| c.0 == "Live").unwrap().1,
      "1/2".to_string()
    );
    assert!(!cells(0, 0).iter().any(|c| c.0 == "Live"));
  }

  #[test]
  fn a_dropped_record_is_logged_as_client_closed() {
    let log = RequestLog::new();
    drop(log.begin(ROUTE, None));
    let tail = log.tail(LogScope::All, 10);
    assert_eq!(tail.rows[0].state, RequestState::ClientClosed);
    assert_eq!(tail.rows[0].status, None);
    assert_eq!(tail.summary.errors, 0);
  }

  #[test]
  fn respond_records_the_proxy_error() {
    let log = RequestLog::new();
    let rec = log.begin(ROUTE, None);
    rec.respond(503, Some("launch_failed"), Some("out of memory"));
    let tail = log.tail(LogScope::All, 10);
    assert_eq!(tail.rows[0].status, Some(503));
    assert_eq!(tail.rows[0].error.as_deref(), Some("launch_failed"));
    assert_eq!(tail.rows[0].cause.as_deref(), Some("out of memory"));
    assert_eq!(tail.summary.errors, 1);
  }

  #[test]
  fn tail_filters_by_model_path_and_returns_newest_first() {
    let log = RequestLog::new();
    let a1 = served(&log, "/m/a.gguf", "L1", Usage::default());
    served(&log, "/m/b.gguf", "L2", Usage::default());
    let a2 = served(&log, "/m/a.gguf", "L1", Usage::default());

    let tail = log.tail(
      LogScope::Model {
        path: "/m/a.gguf".to_string(),
      },
      10,
    );
    assert_eq!(
      tail.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![a2, a1]
    );
    assert_eq!(tail.summary.requests, 2);
    assert_eq!(log.tail(LogScope::All, 10).summary.requests, 3);
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/a.gguf".to_string()
          },
          1
        )
        .rows
        .len(),
      1
    );
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/none.gguf".to_string()
          },
          10
        )
        .summary,
      zeros(),
      "a scope with no rows has no clients, not unknown ones"
    );
  }

  #[test]
  fn summary_averages_only_successful_requests() {
    let log = RequestLog::new();
    let usage = |tps| Usage {
      prompt_tokens: Some(10),
      completion_tokens: Some(20),
      tokens_per_second: Some(tps),
      ..Usage::default()
    };
    served(&log, "/m/a.gguf", "L1", usage(40.0));
    served(&log, "/m/a.gguf", "L1", usage(60.0));
    let mut failed = log.begin(ROUTE, None);
    failed.set_model("m", "/m/a.gguf");
    failed.respond(503, Some("launch_failed"), None);

    let s = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        0,
      )
      .summary;
    assert_eq!(s.requests, 3);
    assert_eq!(s.errors, 1);
    assert_eq!(s.prompt_tokens, 20);
    assert_eq!(s.completion_tokens, 40);
    assert_eq!(s.tokens_per_second_avg, Some(50.0));
    assert_eq!(s.tokens_per_second_last, Some(60.0));
    assert!(s.avg_duration_ms.is_some());
    assert!(s.avg_ttfb_ms.is_some());

    // The 503 never reached a launch, so the launch's totals leave it out.
    let l = log.launch_summary("L1");
    assert_eq!((l.requests, l.errors), (2, 0));
    assert_eq!(log.launch_summary("L9"), zeros());
  }

  #[test]
  fn summary_counts_auto_starts_and_evictions() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", "/m/a.gguf");
    rec.set_auto_start();
    rec.set_evicted(vec!["L3".to_string(), "L4".to_string()]);
    rec.set_status(200);
    rec.finish(RequestState::Done, Usage::default());
    let s = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        0,
      )
      .summary;
    assert_eq!((s.auto_starts, s.evictions), (1, 2));
  }

  /// A finished row that generated `tokens` at `tps` between `start` and
  /// `start + wall_ms`, built without a log so a test can place windows.
  fn generated_row(start: u64, wall_ms: u64, tokens: u64, tps: f64) -> RequestRow {
    RequestRow {
      started_at_ms: start,
      duration_ms: Some(wall_ms),
      completion_tokens: Some(tokens),
      tokens_per_second: Some(tps),
      state: RequestState::Done,
      status: Some(200),
      ..RequestRow::default()
    }
  }

  fn throughput_of(rows: impl IntoIterator<Item = RequestRow>) -> (Option<f64>, Option<u64>) {
    let mut throughput = Throughput::default();
    for row in rows {
      throughput.add(&row);
    }
    throughput.figures()
  }

  #[test]
  fn aggregate_throughput_divides_tokens_by_the_wall_time_they_took() {
    // Two rows over the same 10 s: 200 tokens out of a box busy for 10 s. The
    // mean of the two speeds says 10 tok/s, which is one stream; the box did 20.
    assert_eq!(
      throughput_of([
        generated_row(0, 10_000, 100, 10.0),
        generated_row(0, 10_000, 100, 10.0),
      ]),
      (Some(20.0), Some(10_000))
    );

    // Back to back, the same two rows are 20 s of wall for 200 tokens, and the
    // two figures agree.
    assert_eq!(
      throughput_of([
        generated_row(0, 10_000, 100, 10.0),
        generated_row(10_000, 10_000, 100, 10.0),
      ]),
      (Some(10.0), Some(20_000))
    );

    // Half-overlapping: 0-10 s and 5-15 s is 15 s of wall.
    assert_eq!(
      throughput_of([
        generated_row(0, 10_000, 100, 10.0),
        generated_row(5_000, 10_000, 100, 10.0),
      ])
      .1,
      Some(15_000)
    );

    // A short row inside a long one adds tokens and no wall: the box was busy
    // either way.
    assert_eq!(
      throughput_of([
        generated_row(0, 10_000, 100, 10.0),
        generated_row(1_000, 1_000, 50, 50.0),
      ]),
      (Some(15.0), Some(10_000))
    );
  }

  #[test]
  fn aggregate_throughput_counts_generation_time_not_the_whole_request() {
    // A non-streamed request: the client waited 5 s and 5 s of it was
    // generation, so the wall is the request and the figure is its speed.
    assert_eq!(
      throughput_of([generated_row(0, 5_000, 100, 20.0)]),
      (Some(20.0), Some(5_000))
    );
    // A speed that cannot account for its own tokens inside the request keeps
    // the whole duration as the wall, so the figure cannot claim a busier box
    // than the clock saw.
    assert_eq!(
      throughput_of([generated_row(0, 2_000, 100, 10.0)]),
      (Some(50.0), Some(2_000))
    );
  }

  #[test]
  fn aggregate_throughput_ignores_rows_that_cannot_contribute() {
    let good = generated_row(0, 10_000, 100, 10.0);
    let rows = vec![
      good.clone(),
      // In flight: its work is not over, so neither sum knows it.
      RequestRow {
        state: RequestState::InFlight,
        ..generated_row(1_000, 5_000, 50, 50.0)
      },
      // No tokens, no speed, a zero speed, no duration.
      RequestRow {
        completion_tokens: None,
        ..good.clone()
      },
      RequestRow {
        tokens_per_second: None,
        ..good.clone()
      },
      RequestRow {
        tokens_per_second: Some(0.0),
        completion_tokens: Some(0),
        ..good.clone()
      },
      RequestRow {
        duration_ms: None,
        ..good.clone()
      },
    ];
    assert_eq!(throughput_of(rows), (Some(10.0), Some(10_000)));
    assert_eq!(
      throughput_of(vec![RequestRow::default()]),
      (None, None),
      "a scope of rows nothing was generated for has no throughput"
    );
  }

  #[test]
  fn a_whole_response_with_an_estimated_speed_covers_its_whole_wait() {
    // A response that arrives whole and gets its speed estimated spans the whole
    // wait, queue time and prompt processing included. The window cannot be
    // shorter than that wait, so such a row pulls the aggregate down and
    // stretches `busy_ms`, which is what it really was.
    let whole = RequestRow {
      tokens_per_second_estimated: true,
      ..generated_row(0, 10_000, 100, 10.0)
    };
    assert_eq!(throughput_of([whole]), (Some(10.0), Some(10_000)));
  }

  #[test]
  fn aggregate_throughput_counts_rows_cut_short_by_client_or_upstream() {
    // A cancelled generation still happened: the engine put those tokens out,
    // so the box did produce them and the figure has to say so.
    let cut_short = |state, status| RequestRow {
      state,
      status: Some(status),
      ..generated_row(0, 10_000, 100, 10.0)
    };
    assert_eq!(
      throughput_of([
        cut_short(RequestState::ClientClosed, 200),
        cut_short(RequestState::UpstreamError, 502),
      ]),
      (Some(20.0), Some(10_000)),
      "two cut-short rows over one shared 10 s wall"
    );
  }

  #[test]
  fn the_average_covers_the_rows_the_aggregate_counts() {
    let log = RequestLog::new();
    let usage = |tokens, tps| Usage {
      prompt_tokens: None,
      completion_tokens: Some(tokens),
      tokens_per_second: Some(tps),
      ..Usage::default()
    };
    for (state, status, tps) in [
      (RequestState::Done, 200u16, 10.0),
      (RequestState::ClientClosed, 200, 30.0),
      (RequestState::UpstreamError, 502, 50.0),
    ] {
      let mut rec = log.begin(ROUTE, None);
      rec.set_status(status);
      rec.finish(state, usage(100, tps));
    }
    let summary = log.tail(LogScope::All, 0).summary;
    assert_eq!(
      summary.tokens_per_second_avg,
      Some(30.0),
      "the mean of 10, 30 and 50: a row that ended badly is no exception"
    );
    assert_eq!(
      summary.errors, 1,
      "only the upstream cut counts as an error"
    );
  }

  #[test]
  fn the_prefill_aggregate_counts_a_shared_prompt_wall_once() {
    // Two 1,000-token prompts computed at the same moment at 500 tok/s each:
    // the box put out 2,000 tokens over one wall second, where the mean of the
    // per-request speeds still reads 500.
    let row = |start: u64| RequestRow {
      started_at_ms: start,
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(2_000),
      duration_ms: Some(2_000),
      prefill_tokens: Some(1_000),
      prefill_tokens_per_second: Some(500.0),
      ..RequestRow::default()
    };
    let figures = |rows: Vec<RequestRow>| {
      let mut figures = RowFigures::default();
      for each in rows {
        figures.add(&each);
      }
      let mut summary = RequestSummary {
        prefill_tokens_per_second: Some(500.0),
        ..RequestSummary::default()
      };
      figures.stamp(&mut summary);
      summary
    };
    let together = figures(vec![row(0), row(0)]);
    assert_eq!(together.prefill_busy_ms, Some(2_000));
    assert_eq!(together.prefill_tokens_per_second_aggregate, Some(1_000.0));
    assert!(
      together
        .cells("—", Aggregates::WhenTheyDiffer)
        .iter()
        .any(|(label, text)| *label == "Agg PP tok/s" && text == "1000 over 2.0s"),
      "{together:?}"
    );
    // Run one after the other and the wall is the sum of the two, so the
    // aggregate is the mean again and the cell drops out of the strip.
    let apart = figures(vec![row(0), row(2_000)]);
    assert_eq!(apart.prefill_busy_ms, Some(4_000));
    assert_eq!(apart.prefill_tokens_per_second_aggregate, Some(500.0));
    assert!(
      !apart
        .cells("—", Aggregates::WhenTheyDiffer)
        .iter()
        .any(|(label, _)| *label == "Agg PP tok/s"),
      "{:?}",
      apart.cells("—", Aggregates::WhenTheyDiffer)
    );
  }

  #[test]
  fn a_streams_prompt_window_ends_at_its_first_token_not_its_first_byte() {
    // An engine can flush response headers before it computes the prompt, so a
    // 40 ms first byte sits nowhere near the 600 ms the prompt took. Anchoring
    // on the first byte crams 300 tokens into a 40 ms window, and two prompts
    // run one after the other read as one 80 ms wall at 7,500 tok/s.
    let row = |start: u64| RequestRow {
      started_at_ms: start,
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(40),
      prefill_end_ms: Some(600),
      duration_ms: Some(3_000),
      completion_tokens: Some(120),
      tokens_per_second: Some(60.0),
      prefill_tokens: Some(300),
      prefill_tokens_per_second: Some(500.0),
      ..RequestRow::default()
    };
    let mut figures = RowFigures::default();
    for each in [row(0), row(1_000)] {
      figures.add(&each);
    }
    let mut summary = RequestSummary {
      prefill_tokens_per_second: Some(500.0),
      ..RequestSummary::default()
    };
    figures.stamp(&mut summary);
    assert_eq!(
      summary.prefill_busy_ms,
      Some(1_200),
      "two 600 ms prompt windows, not two 40 ms ones"
    );
    assert_eq!(summary.prefill_tokens_per_second_aggregate, Some(500.0));
  }

  #[test]
  fn a_whole_responses_prompt_window_sits_before_its_generation() {
    // A response that arrives whole sends its first byte when generation ends,
    // so its prompt ran at the front of the request: 1,000 prompt tokens at
    // 500 tok/s inside a 5 s wait that spent the last 2 s generating puts its
    // window at [1000, 3000], not at the tail.
    let whole = RequestRow {
      started_at_ms: 0,
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(5_000),
      duration_ms: Some(5_000),
      completion_tokens: Some(1_000),
      tokens_per_second: Some(500.0),
      prefill_tokens: Some(1_000),
      prefill_tokens_per_second: Some(500.0),
      ..RequestRow::default()
    };
    // A streamed prompt over [0, 2000], so it shares part of the wait.
    let streamed = RequestRow {
      started_at_ms: 0,
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(2_000),
      duration_ms: Some(2_000),
      prefill_tokens: Some(1_000),
      prefill_tokens_per_second: Some(500.0),
      ..RequestRow::default()
    };
    let mut figures = RowFigures::default();
    figures.add(&whole);
    figures.add(&streamed);
    let mut summary = RequestSummary::default();
    figures.stamp(&mut summary);
    // [0, 3000] merged, not 2 s + 2 s: the two windows overlap by one second.
    assert_eq!(summary.prefill_busy_ms, Some(3_000));
    assert_eq!(
      summary.prefill_tokens_per_second_aggregate,
      Some(2_000.0 * 1_000.0 / 3_000.0)
    );
  }

  #[test]
  fn a_row_without_both_halves_of_its_prompt_adds_no_window() {
    let aggregate_for = |row: RequestRow| {
      let mut figures = RowFigures::default();
      figures.add(&row);
      let mut summary = RequestSummary::default();
      figures.stamp(&mut summary);
      summary.prefill_tokens_per_second_aggregate
    };
    let base = RequestRow {
      started_at_ms: 0,
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(2_000),
      duration_ms: Some(2_000),
      prefill_tokens: Some(1_000),
      prefill_tokens_per_second: Some(500.0),
      ..RequestRow::default()
    };
    assert_eq!(aggregate_for(base.clone()), Some(500.0));
    // Each of these leaves nothing to place, so none opens a window.
    for broken in [
      RequestRow {
        state: RequestState::InFlight,
        ..base.clone()
      },
      RequestRow {
        prefill_tokens: Some(0),
        ..base.clone()
      },
      RequestRow {
        prefill_tokens: None,
        ..base.clone()
      },
      RequestRow {
        prefill_tokens_per_second: Some(0.0),
        ..base.clone()
      },
      RequestRow {
        prefill_tokens_per_second: None,
        ..base.clone()
      },
      RequestRow {
        ttfb_ms: Some(0),
        ..base.clone()
      },
      RequestRow {
        ttfb_ms: None,
        ..base.clone()
      },
    ] {
      assert_eq!(aggregate_for(broken), None);
    }
    // A prompt that had finished before its response began: no wall left to
    // place it in.
    assert_eq!(
      aggregate_for(RequestRow {
        completion_tokens: Some(1_000),
        tokens_per_second: Some(500.0),
        ..base.clone()
      }),
      None
    );
    // A prompt claiming more time than the request had is clamped to the
    // request rather than stretching the wall: a 10 s claim inside a 2 s row.
    assert_eq!(
      aggregate_for(RequestRow {
        prefill_tokens_per_second: Some(100.0),
        ..base.clone()
      }),
      Some(500.0)
    );
  }

  #[test]
  fn an_aggregate_within_noise_of_its_average_is_left_off_the_strip() {
    let agg_labels = |summary: &RequestSummary| -> Vec<&'static str> {
      summary
        .cells("—", Aggregates::WhenTheyDiffer)
        .into_iter()
        .filter(|(label, _)| label.starts_with("Agg "))
        .map(|(label, _)| label)
        .collect()
    };
    let labels = |summary: &RequestSummary| {
      summary
        .cells("—", Aggregates::WhenTheyDiffer)
        .iter()
        .map(|c| c.0)
        .collect::<Vec<_>>()
    };
    // Both pair 1 and pair 2 round to the same printed number as their average,
    // so neither is worth the cells it costs.
    let same = RequestSummary {
      tokens_per_second_avg: Some(41.26),
      tokens_per_second_aggregate: Some(41.29),
      busy_ms: Some(60_000),
      prefill_tokens_per_second: Some(1_043.0),
      prefill_tokens_per_second_aggregate: Some(1_043.4),
      prefill_busy_ms: Some(60_000),
      ..RequestSummary::default()
    };
    assert_eq!(agg_labels(&same), Vec::<&str>::new());
    // 52.4 against 53 prints two numbers and means the same speed: one stream
    // with nothing sharing the wall.
    let near = RequestSummary {
      tokens_per_second_avg: Some(52.4),
      tokens_per_second_aggregate: Some(53.0),
      busy_ms: Some(60_000),
      prefill_tokens_per_second: Some(1_043.0),
      prefill_tokens_per_second_aggregate: Some(1_090.0),
      prefill_busy_ms: Some(60_000),
      ..RequestSummary::default()
    };
    assert_eq!(agg_labels(&near), Vec::<&str>::new());
    // A trickle prints `0.0` on both sides however far apart the figures are,
    // and a cell repeating the number beside it is the cell this rule drops.
    let trickle = RequestSummary {
      tokens_per_second_avg: Some(0.033),
      tokens_per_second_aggregate: Some(0.049),
      busy_ms: Some(30_000),
      ..RequestSummary::default()
    };
    assert_eq!(agg_labels(&trickle), Vec::<&str>::new());
    // Past the band the aggregate is a different figure again, so it prints.
    let over = RequestSummary {
      tokens_per_second_avg: Some(52.4),
      tokens_per_second_aggregate: Some(56.0),
      busy_ms: Some(60_000),
      ..RequestSummary::default()
    };
    assert_eq!(agg_labels(&over), ["Agg tok/s"]);
    // The pair with no average to answer to has nothing to sit beside, so it
    // prints whatever it is.
    let no_average = RequestSummary {
      tokens_per_second_aggregate: Some(52.4),
      busy_ms: Some(6_300),
      ..RequestSummary::default()
    };
    assert_eq!(agg_labels(&no_average), ["Agg tok/s"]);
    let apart = RequestSummary {
      tokens_per_second_avg: Some(41.3),
      tokens_per_second_aggregate: Some(52.4),
      busy_ms: Some(6_300),
      prefill_tokens_per_second: Some(1_043.0),
      prefill_tokens_per_second_aggregate: Some(2_100.0),
      prefill_busy_ms: Some(6_300),
      ..RequestSummary::default()
    };
    assert_eq!(agg_labels(&apart), ["Agg tok/s", "Agg PP tok/s"]);
    // Each aggregate sits beside the average it belongs to, and one with no
    // average to answer to prints on its own.
    assert_eq!(
      labels(&apart)
        .windows(2)
        .filter(|pair| pair[0] == "Avg tok/s" || pair[0] == "Avg PP tok/s")
        .map(|pair| (pair[0], pair[1]))
        .collect::<Vec<_>>(),
      [("Avg tok/s", "Agg tok/s"), ("Avg PP tok/s", "Agg PP tok/s")]
    );
  }

  #[test]
  fn a_scopes_throughput_counts_only_its_own_rows() {
    let log = RequestLog::new();
    // Two models, one launch each, two rows per model 10 s apart at 10 tok/s.
    let usage = Usage {
      completion_tokens: Some(100),
      tokens_per_second: Some(10.0),
      ..Usage::default()
    };
    for (model, launch) in [("/m/a.gguf", "L1"), ("/m/b.gguf", "L2")] {
      for start in [0u64, 10_000] {
        let seq = served(&log, model, launch, usage);
        let mut inner = log.lock();
        let row = inner.rows.iter_mut().find(|r| r.seq == seq).unwrap();
        row.started_at_ms = start;
        row.duration_ms = Some(10_000);
      }
    }
    // The two models generated at once, so all of it is 400 tokens over 20 s.
    let all = log.tail(LogScope::All, 0).summary;
    assert_eq!(
      (all.tokens_per_second_aggregate, all.busy_ms),
      (Some(20.0), Some(20_000)),
      "the figure covers the scope, not the `limit` of rows asked for"
    );
    // Each scope sees its own two rows: 200 tokens over 20 s.
    for scope in [
      LogScope::Model {
        path: "/m/a.gguf".to_string(),
      },
      LogScope::Launch {
        id: "L1".to_string(),
      },
    ] {
      let s = log.tail(scope, 0).summary;
      assert_eq!(
        (s.tokens_per_second_aggregate, s.busy_ms),
        (Some(10.0), Some(20_000))
      );
    }
    // The same figures a `/metrics` scrape reads, which stamps from the same
    // rows.
    let snap = log.snapshot();
    assert_eq!(
      (
        snap.all.summary.tokens_per_second_aggregate,
        snap.all.summary.busy_ms
      ),
      (Some(20.0), Some(20_000))
    );
    assert_eq!(
      (
        snap.models[0].summary.tokens_per_second_aggregate,
        snap.models[0].summary.busy_ms
      ),
      (Some(10.0), Some(20_000))
    );
    assert_eq!(
      (
        snap.launches[0].totals.summary.tokens_per_second_aggregate,
        snap.launches[0].totals.summary.busy_ms
      ),
      (Some(10.0), Some(20_000))
    );
  }

  #[test]
  fn agg_tok_s_sits_beside_avg_tok_s_and_names_its_wall() {
    let cells = RequestSummary {
      tokens_per_second_avg: Some(10.0),
      tokens_per_second_aggregate: Some(50.3),
      busy_ms: Some(214_000),
      ..RequestSummary::default()
    }
    .cells("—", Aggregates::WhenTheyDiffer);
    let at = cells.iter().position(|c| c.0 == "Avg tok/s").unwrap();
    assert_eq!(cells[at + 1], ("Agg tok/s", "50.3 over 3m34s".to_string()));
    // The prompt speed follows the two generation figures, and the token
    // counts follow it.
    assert_eq!(cells[at + 2], ("Avg PP tok/s", "—".to_string()));
    assert_eq!(cells[at + 3].0, "Avg total");
  }

  #[test]
  fn a_snapshot_answers_the_tails_the_log_answers() {
    let log = RequestLog::new();
    // Two models, one launch each, one speed each, and a launch that has since
    // stopped.
    let usage = Usage {
      completion_tokens: Some(10),
      tokens_per_second: Some(40.0),
      ..Usage::default()
    };
    served(&log, "/m/a.gguf", "L1", usage);
    served(&log, "/m/a.gguf", "L1", usage);
    served(&log, "/m/b.gguf", "L2", usage);
    log.forget_launch("L2");
    let snap = log.snapshot();
    for scope in [
      LogScope::All,
      LogScope::Model {
        path: "/m/a.gguf".to_string(),
      },
      LogScope::Model {
        path: "/m/b.gguf".to_string(),
      },
      // A scope nothing has ever reached.
      LogScope::Model {
        path: "/m/never.gguf".to_string(),
      },
      LogScope::Launch {
        id: "L1".to_string(),
      },
      LogScope::Launch {
        id: "L2".to_string(),
      },
      LogScope::Launch {
        id: "L9".to_string(),
      },
    ] {
      for limit in [0usize, 2, 10_000] {
        assert_eq!(
          snap.tail(&scope, limit),
          log.tail(scope.clone(), limit),
          "{scope:?} at limit {limit}"
        );
      }
    }
  }

  #[test]
  fn ring_is_bounded_and_totals_outlive_the_rows() {
    let log = RequestLog::new();
    for _ in 0..CAPACITY + 5 {
      served(&log, "/m/a.gguf", "L1", Usage::default());
    }
    let tail = log.tail(LogScope::All, usize::MAX);
    assert_eq!(tail.rows.len(), CAPACITY);
    assert_eq!(tail.rows[0].seq, (CAPACITY + 5) as u64);
    assert_eq!(tail.summary.requests, (CAPACITY + 5) as u64);
  }

  #[test]
  fn finishing_a_row_pushed_out_of_the_ring_still_counts() {
    let log = RequestLog::new();
    let mut slow = log.begin(ROUTE, None);
    slow.set_model("m", "/m/slow.gguf");
    for _ in 0..CAPACITY {
      served(&log, "/m/a.gguf", "L1", Usage::default());
    }
    slow.set_status(200);
    slow.finish(RequestState::Done, Usage::default());
    // The newest row is untouched by the late finish.
    assert_eq!(
      log.tail(LogScope::All, 1).rows[0].model_path.as_deref(),
      Some("/m/a.gguf")
    );
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/slow.gguf".to_string()
          },
          0
        )
        .summary
        .requests,
      1
    );
  }

  #[test]
  fn client_strings_are_cut_and_cleaned_before_they_are_stored() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_requested_model(Some(&"é".repeat(MAX_MODEL_CHARS + 50)));
    let long_cause = "x".repeat(MAX_CAUSE_CHARS + 50);
    rec.respond(503, Some("launch_failed"), Some(&long_cause));
    let row = log.tail(LogScope::All, 1).rows.remove(0);
    assert_eq!(
      row.requested_model.unwrap().chars().count(),
      MAX_MODEL_CHARS
    );
    assert_eq!(row.cause.unwrap().len(), MAX_CAUSE_CHARS);

    let mut hostile = log.begin(ROUTE, None);
    hostile.set_requested_model(Some(
      "evil\x1b[2Jmodel\nname\ttab\u{202e}rlo\u{200b}zwsp\u{ad}shy\u{feff}end\u{2061}func\u{fffb}annot\u{61c}ar",
    ));
    hostile.respond(503, Some("launch_failed"), Some("line one\nline two"));
    let cleaned = log.tail(LogScope::All, 1).rows.remove(0);
    assert_eq!(
      cleaned.requested_model.as_deref(),
      Some("evil [2Jmodel name tab rlo zwsp shy end func annot ar")
    );
    assert_eq!(cleaned.cause.as_deref(), Some("line one line two"));
  }

  #[test]
  fn note_describes_what_happened_beyond_the_status() {
    let base = RequestRow {
      state: RequestState::Done,
      status: Some(200),
      ..RequestRow::default()
    };
    assert_eq!(base.note(), "");
    // Waiting on a load, whoever started it: the model is known and no
    // launch has the request yet.
    let loading = RequestRow {
      state: RequestState::InFlight,
      model_path: Some("/m/a.gguf".to_string()),
      ..RequestRow::default()
    };
    assert_eq!(loading.note(), "loading model");
    // A launch has it. No status yet on a non-streamed request, which
    // only arrives with the whole response.
    let generating = RequestRow {
      launch_id: Some("L1".to_string()),
      ..loading.clone()
    };
    assert_eq!(generating.note(), "in flight");
    // Still reading the request body: no model yet.
    assert_eq!(RequestRow::default().note(), "in flight");
    let made_room = RequestRow {
      auto_start: true,
      evicted: vec!["L3".to_string(), "L4".to_string()],
      ..base.clone()
    };
    assert_eq!(made_room.note(), "auto-start, unloaded L3 L4");
    let fallback = RequestRow {
      auto_start: true,
      fallback: Some("launch_failed".to_string()),
      requested_model: Some("big".to_string()),
      ..base.clone()
    };
    assert_eq!(fallback.note(), "auto-start, fallback (launch_failed)");
    let failed = RequestRow {
      status: Some(503),
      auto_start: true,
      error: Some("launch_failed".to_string()),
      cause: Some("out of memory".to_string()),
      ..base.clone()
    };
    assert_eq!(failed.note(), "auto-start, launch_failed: out of memory");
    let closed = RequestRow {
      state: RequestState::ClientClosed,
      ..RequestRow::default()
    };
    assert_eq!(closed.note(), "client closed");
  }

  #[test]
  fn the_time_cell_is_the_local_clock_time_the_request_started() {
    // 22:13:20 UTC. A zone's offset is a whole number of quarter hours,
    // so in any zone the minute is 13, 28, 43 or 58 and the second is 20.
    let row = RequestRow {
      started_at_ms: 1_700_000_000_000,
      ..RequestRow::default()
    };
    let time = row.cells("-").time;
    let parts: Vec<u32> = time
      .split(':')
      .map(|part| part.parse().expect("digits"))
      .collect();
    assert_eq!(time.len(), 8, "{time}");
    assert_eq!(parts.len(), 3, "{time}");
    assert!(parts[0] < 24, "{time}");
    assert_eq!(parts[1] % 15, 13, "{time}");
    assert_eq!(parts[2], 20, "{time}");
  }

  #[test]
  fn cells_format_each_field_and_fall_back_to_the_placeholder() {
    let row = RequestRow {
      route: ROUTE.to_string(),
      requested_model: Some("qwen".to_string()),
      model: Some("Qwen3.8-27B".to_string()),
      launch_id: Some("L1".to_string()),
      client: Some("127.0.0.1:5000".to_string()),
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(180),
      duration_ms: Some(1_240),
      prompt_tokens: Some(41),
      completion_tokens: Some(12_345),
      tokens_per_second: Some(39.64),
      ..RequestRow::default()
    };
    let c = row.cells("-");
    assert_eq!(c.time.len(), 8);
    assert_eq!(
      [
        c.status,
        c.total,
        c.ttfb,
        c.speed,
        c.prefill,
        c.tokens_in,
        c.cache_hit,
        c.tokens_out,
        c.route,
        c.model,
        c.launch,
        c.client,
        c.note
      ],
      [
        "200",
        "1.2s",
        "180ms",
        "39.6",
        "-",
        "41",
        "-",
        "12.3k",
        "chat/completions",
        "Qwen3.8-27B",
        "L1",
        "127.0.0.1:5000",
        ""
      ]
    );
    let estimated = RequestRow {
      tokens_per_second_estimated: true,
      ..row.clone()
    };
    assert_eq!(estimated.cells("-").speed, "~39.6");
    // A prefill speed keeps one decimal below 1000 and drops it from there up,
    // so an estimate of twelve thousand still fits the column's six cells.
    assert_eq!(
      RequestRow {
        prefill_tokens_per_second: Some(38.904),
        prefill_estimated: true,
        ..row.clone()
      }
      .cells("-")
      .prefill,
      "~38.9"
    );
    assert_eq!(
      RequestRow {
        prefill_tokens_per_second: Some(999.96),
        ..row.clone()
      }
      .cells("-")
      .prefill,
      "1000"
    );
    assert_eq!(
      RequestRow {
        prefill_tokens_per_second: Some(12_345.0),
        ..row.clone()
      }
      .cells("-")
      .prefill,
      "12345"
    );
    // Nothing served it: the model asked for stands in, the rest is blank.
    let pending = RequestRow {
      requested_model: Some("qwen".to_string()),
      ..RequestRow::default()
    };
    let p = pending.cells("-");
    assert_eq!(p.model, "qwen");
    assert_eq!(
      [
        p.status,
        p.total,
        p.ttfb,
        p.speed,
        p.prefill,
        p.tokens_in,
        p.cache_hit,
        p.tokens_out,
        p.launch,
        p.client
      ],
      ["-"; 10]
    );
  }

  #[test]
  fn a_row_from_an_older_daemon_loads_the_new_figures_as_missing() {
    let row: RequestRow =
      serde_json::from_str(r#"{"seq":3,"route":"/v1/chat/completions"}"#).expect("a row");
    assert_eq!(
      (
        row.prefill_tokens,
        row.prefill_tokens_per_second,
        row.cached_tokens,
        row.cache_hit_ratio
      ),
      (None, None, None, None)
    );
    assert!(!row.prefill_estimated);
    let summary: RequestSummary = serde_json::from_str("{}").expect("a summary");
    assert_eq!(
      (summary.prefill_tokens_per_second, summary.cache_hit_ratio),
      (None, None)
    );
    assert_eq!(summary.cached_tokens, 0);
  }

  /// A finished request that computed `tokens` prompt tokens at `tps`.
  fn prefill_usage(tokens: u64, tps: f64) -> Usage {
    Usage {
      prefill_tokens: Some(tokens),
      prefill_tokens_per_second: Some(tps),
      ..Usage::default()
    }
  }

  #[test]
  fn the_prefill_figure_weighs_rows_by_tokens_not_by_a_plain_mean() {
    let log = RequestLog::new();
    // An agent turn that prefills 100 uncached tokens at 100 tok/s, then a cold
    // prompt that prefills 10,000 at 5,000: 10,100 tokens over three seconds.
    // The mean of the two speeds would say 2,550 and mostly report the turn.
    served(&log, "/m/a.gguf", "L1", prefill_usage(100, 100.0));
    served(&log, "/m/a.gguf", "L1", prefill_usage(10_000, 5_000.0));
    assert_eq!(
      log.tail(LogScope::All, 0).summary.prefill_tokens_per_second,
      Some(10_100.0 / 3.0)
    );
  }

  #[test]
  fn an_estimated_prefill_counts_and_a_row_needs_both_halves() {
    let log = RequestLog::new();
    let estimated = Usage {
      prefill_estimated: true,
      ..prefill_usage(500, 500.0)
    };
    // 5,000 computed tokens with no speed to place them in: the figure is 600
    // tokens over two seconds, not 5,600 over two.
    let no_speed = Usage {
      prompt_tokens: Some(5_000),
      prefill_tokens: Some(5_000),
      ..Usage::default()
    };
    served(&log, "/m/a.gguf", "L1", estimated);
    served(&log, "/m/a.gguf", "L1", prefill_usage(100, 100.0));
    served(&log, "/m/a.gguf", "L1", no_speed);
    assert_eq!(
      log.tail(LogScope::All, 0).summary.prefill_tokens_per_second,
      Some(300.0)
    );
  }

  #[test]
  fn the_cache_ratio_covers_only_the_rows_that_report_a_cached_count() {
    let cached = |prompt, hit| Usage {
      prompt_tokens: Some(prompt),
      cached_tokens: Some(hit),
      ..Usage::default()
    };
    let log = RequestLog::new();
    served(&log, "/m/a.gguf", "L1", cached(1_000, 800));
    served(&log, "/m/a.gguf", "L1", cached(1_000, 200));
    let s = log.tail(LogScope::All, 0).summary;
    assert_eq!(s.cached_tokens, 1_000);
    assert_eq!(s.cache_hit_ratio, Some(0.5));

    // A row that reports no cached count is in neither half of the ratio, even
    // though its prompt tokens still count toward `Tokens in`.
    let quiet = RequestLog::new();
    served(
      &quiet,
      "/m/a.gguf",
      "L1",
      Usage {
        prompt_tokens: Some(9_000),
        ..Usage::default()
      },
    );
    served(&quiet, "/m/a.gguf", "L1", cached(1_000, 500));
    let q = quiet.tail(LogScope::All, 0).summary;
    assert_eq!((q.prompt_tokens, q.cached_tokens), (10_000, 500));
    assert_eq!(q.cache_hit_ratio, Some(0.5));

    // Nothing reported one: no ratio rather than a zero that reads as a miss.
    let none = RequestLog::new();
    served(
      &none,
      "/m/a.gguf",
      "L1",
      Usage {
        prompt_tokens: Some(10),
        ..Usage::default()
      },
    );
    let n = none.tail(LogScope::All, 0).summary;
    assert_eq!((n.cached_tokens, n.cache_hit_ratio), (0, None));
  }

  #[test]
  fn a_cached_count_with_no_prompt_count_cannot_tip_the_ratio() {
    // The two halves come from different objects, so a body can send one without
    // the other. Such a row belongs to neither half, and one row cannot claim it
    // reused more than its whole prompt either.
    let log = RequestLog::new();
    served(
      &log,
      "/m/a.gguf",
      "L1",
      Usage {
        prompt_tokens: Some(1_000),
        cached_tokens: Some(500),
        ..Usage::default()
      },
    );
    served(
      &log,
      "/m/a.gguf",
      "L1",
      Usage {
        cached_tokens: Some(9_000),
        ..Usage::default()
      },
    );
    let s = log.tail(LogScope::All, 0).summary;
    // The 9,000 cached tokens are real, but there is no prompt to read them
    // against, so they are in neither half of what the cell prints.
    assert_eq!(s.cached_tokens, 500);
    assert_eq!(s.cache_hit_ratio, Some(0.5));

    let boast = RequestLog::new();
    served(
      &boast,
      "/m/a.gguf",
      "L1",
      Usage {
        prompt_tokens: Some(10),
        cached_tokens: Some(500),
        ..Usage::default()
      },
    );
    assert_eq!(
      boast.tail(LogScope::All, 0).summary.cache_hit_ratio,
      Some(1.0),
      "a ratio above 1 is not a hit rate"
    );
  }

  #[test]
  fn each_row_carries_its_own_cache_hit_share() {
    let cached = |prompt, hit| Usage {
      prompt_tokens: Some(prompt),
      cached_tokens: Some(hit),
      ..Usage::default()
    };
    let log = RequestLog::new();
    served(&log, "/m/a.gguf", "L1", cached(1_000, 200));
    // A cached count over its own prompt clamps to the whole prompt.
    served(&log, "/m/a.gguf", "L1", cached(80, 1_000));
    let rows = log.tail(LogScope::All, 10).rows;
    assert_eq!(rows[0].cells("-").cache_hit, "100.0%");
    assert_eq!(rows[1].cache_hit_ratio, Some(0.2));
    assert_eq!(rows[1].cells("-").cache_hit, "20.0%");

    // No cached count: the placeholder, not a zero that reads as a total miss.
    let quiet = RequestLog::new();
    served(
      &quiet,
      "/m/a.gguf",
      "L1",
      Usage {
        prompt_tokens: Some(80),
        ..Usage::default()
      },
    );
    assert_eq!(
      quiet.tail(LogScope::All, 1).rows[0].cells("-").cache_hit,
      "-"
    );

    // An empty prompt leaves nothing to divide by, which is also not a miss.
    let empty = RequestLog::new();
    served(&empty, "/m/a.gguf", "L1", cached(0, 0));
    let row = &empty.tail(LogScope::All, 1).rows[0];
    assert_eq!(
      (row.cache_hit_ratio, row.cells("-").cache_hit.as_str()),
      (None, "-")
    );

    // The widest the cell can reach is the clamp: one whole prompt, which is
    // exactly the six cells the column has.
    assert_eq!(fmt_pct(1.0).chars().count(), 6);
  }

  #[test]
  fn a_row_from_an_older_daemon_divides_its_own_counts() {
    // A daemon predating the field sends both counts and no ratio, so the cell
    // works the share out instead of printing the no-data placeholder.
    let row: RequestRow =
      serde_json::from_str(r#"{"prompt_tokens":1000,"cached_tokens":250}"#).expect("a row");
    assert_eq!(row.cache_hit_ratio, None);
    assert_eq!(row.cells("-").cache_hit, "25.0%");
  }

  #[test]
  fn speeds_pick_a_unit_by_size() {
    assert_eq!(fmt_tps(38.9), "38.9");
    assert_eq!(fmt_tps(999.9), "999.9");
    assert_eq!(fmt_tps(1_000.0), "1000");
    assert_eq!(fmt_tps(99_999.4), "99999");
    assert_eq!(fmt_tps(99_999.5), "100k");
    // The ceiling the tap allows, with the `~` an estimate adds: six cells.
    assert_eq!(fmt_tps(1_000_000.0), "1000k");
  }

  #[test]
  fn durations_and_counts_pick_a_unit_by_size() {
    assert_eq!(fmt_ms(0), "0ms");
    assert_eq!(fmt_ms(850), "850ms");
    assert_eq!(fmt_ms(1_240), "1.2s");
    assert_eq!(fmt_ms(59_900), "59.9s");
    assert_eq!(fmt_ms(125_000), "2m05s");
    assert_eq!(fmt_ms(3_720_000), "1h02m");
    assert_eq!(fmt_count(0), "0");
    assert_eq!(fmt_count(9_999), "9999");
    assert_eq!(fmt_count(12_345), "12.3k");
    assert_eq!(fmt_count(999_949), "999.9k");
    assert_eq!(fmt_count(999_950), "1.0M");
    assert_eq!(fmt_count(999_999), "1.0M");
    assert_eq!(fmt_count(1_234_567), "1.2M");
  }

  #[test]
  fn summary_cells_use_the_placeholder_for_missing_figures() {
    let text = |cells: &[(&'static str, String)], label: &str| {
      cells.iter().find(|c| c.0 == label).unwrap().1.clone()
    };
    let cells = RequestSummary::default().cells("—", Aggregates::WhenTheyDiffer);
    // The counts lead, one row, and the speeds and token totals follow on the
    // second.
    assert_eq!(cells[0], ("Hosts", "—".to_string()));
    assert_eq!(cells[2], ("Requests", "0".to_string()));
    assert_eq!(text(&cells, "Avg total"), "—");
    assert_eq!(text(&cells, "Avg tok/s"), "—");
    assert_eq!(text(&cells, "Avg PP tok/s"), "—");
    // An aggregate nothing produced has no cell at all, not a placeholder: a
    // figure with no rows behind it is not the same answer as zero.
    assert!(!cells.iter().any(|c| c.0.starts_with("Agg ")));
    let filled = RequestSummary {
      avg_duration_ms: Some(1_240),
      tokens_per_second_avg: Some(39.64),
      prompt_tokens: 12_345,
      prefill_tokens_per_second: Some(3_366.7),
      cached_tokens: 1_200,
      cache_hit_ratio: Some(0.9756),
      ..RequestSummary::default()
    };
    let filled_cells = filled.cells("—", Aggregates::WhenTheyDiffer);
    assert_eq!(text(&filled_cells, "Avg total"), "1.2s");
    assert_eq!(text(&filled_cells, "Avg tok/s"), "39.6");
    // A speed from 1000 up loses the decimal here too.
    assert_eq!(text(&filled_cells, "Avg PP tok/s"), "3367");
    assert_eq!(text(&filled_cells, "Tokens in"), "12.3k");
    assert_eq!(text(&filled_cells, "Cached"), "1200 (97.6%)".to_string());
    // The session speed is the newest one a stream reported, and the only
    // speed on the strip besides the average.
    assert!(!filled_cells.iter().any(|c| c.0 == "Tok/s last"));
  }

  #[test]
  fn each_summary_row_holds_its_figures_in_order() {
    let labels = |cells: &[(&'static str, String)]| cells.iter().map(|c| c.0).collect::<Vec<_>>();
    let summary = RequestSummary::default();
    let [counts, speeds_and_totals] = summary.cell_rows("—", Aggregates::WhenTheyDiffer);
    assert_eq!(
      labels(&counts),
      [
        "Hosts",
        "Connections",
        "Requests",
        "Errors",
        "Auto-starts",
        "Unloaded"
      ]
    );
    assert_eq!(
      labels(&speeds_and_totals),
      [
        "Avg tok/s",
        "Avg PP tok/s",
        "Avg total",
        "Avg TTFB",
        "Tokens in",
        "Cached",
        "Tokens out"
      ]
    );
    // `cells` is the two rows end to end, which is the order `requests` prints.
    assert_eq!(
      summary.cells("—", Aggregates::WhenTheyDiffer),
      counts
        .into_iter()
        .chain(speeds_and_totals)
        .collect::<Vec<_>>()
    );
    // The two optional figures join the row they belong to.
    let [live_counts, live_speeds] = RequestSummary {
      live_hosts: Some(1),
      live_connections: Some(2),
      tokens_per_second_session: Some(40.0),
      ..RequestSummary::default()
    }
    .cell_rows("—", Aggregates::WhenTheyDiffer);
    assert_eq!(labels(&live_counts)[0], "Live");
    assert_eq!(labels(&live_speeds)[1], "Tok/s session");
  }

  #[tokio::test]
  async fn finished_rows_are_appended_to_the_file_as_json_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("logs").join(FILE_NAME);
    let log = RequestLog::new();
    log.write_to(path.clone());

    let first = served(&log, "/m/a.gguf", "L1", Usage::default());
    let _in_flight = log.begin(ROUTE, None);
    let failed = log.begin(ROUTE, None);
    failed.respond(404, Some("model_not_found"), Some("nope not found"));

    // The writer runs on its own task; wait for both lines to land.
    let mut text = String::new();
    for _ in 0..200 {
      text = std::fs::read_to_string(&path).unwrap_or_default();
      if text.lines().count() >= 2 {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows: Vec<RequestRow> = text
      .lines()
      .map(|line| serde_json::from_str(line).expect("one JSON row per line"))
      .collect();
    // Finished rows only, in the order they finished. The in-flight row
    // (`seq` 2) is not written.
    assert_eq!(
      rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![first, 3]
    );
    assert_eq!(rows[0].model_path.as_deref(), Some("/m/a.gguf"));
    assert_eq!(rows[1].error.as_deref(), Some("model_not_found"));
    // The in-memory log is unchanged by the file.
    assert_eq!(log.tail(LogScope::All, 10).rows.len(), 3);
    assert_eq!(log.file_status(), Some(FileStatus { path, dropped: 0 }));
    assert_eq!(RequestLog::new().file_status(), None, "no file asked for");
  }

  #[tokio::test]
  async fn rows_that_cannot_be_written_are_counted_not_lost_silently() {
    // The path's parent is a regular file, so the log file cannot be
    // created.
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"").expect("write blocker");
    let log = RequestLog::new();
    log.write_to(blocker.join(FILE_NAME));

    for _ in 0..3 {
      served(&log, "/m/a.gguf", "L1", Usage::default());
    }
    // A row finished before the writer gave up sits in the queue until
    // the writer drains it; one finished after is counted at once.
    let mut dropped = 0;
    for _ in 0..200 {
      dropped = log.file_status().expect("file status").dropped;
      if dropped == 3 {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(dropped, 3);
    served(&log, "/m/a.gguf", "L1", Usage::default());
    assert_eq!(log.file_status().expect("file status").dropped, 4);
    // The in-memory log has all four.
    assert_eq!(log.tail(LogScope::All, 10).summary.requests, 4);
  }

  /// `/dev/full` opens and then fails every write with `ENOSPC`.
  #[cfg(target_os = "linux")]
  #[tokio::test]
  async fn a_write_failure_stops_the_writer_and_later_rows_are_counted() {
    let log = RequestLog::new();
    log.write_to(PathBuf::from("/dev/full"));
    // The writer gives up on the first row it cannot write. Rows keep
    // being finished meanwhile; none of them reaches a file.
    let dropped = || log.file_status().expect("file status").dropped;
    let mut finished = 0;
    for _ in 0..200 {
      served(&log, "/m/a.gguf", "L1", Usage::default());
      finished += 1;
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      if dropped() > 0 {
        break;
      }
    }
    assert!(dropped() > 0, "the writer never stopped");
    served(&log, "/m/a.gguf", "L1", Usage::default());
    finished += 1;
    // Every row is counted: the one whose write failed, the ones queued
    // behind it, and the ones finished after the writer stopped.
    for _ in 0..200 {
      if dropped() == finished {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(dropped(), finished);
  }

  #[test]
  fn a_full_queue_drops_the_line_and_counts_it() {
    // No runtime here, so nothing drains the queue: fill it by hand.
    let log = RequestLog::new();
    let (queue, _rx) = mpsc::channel::<String>(1);
    let sink = FileSink {
      path: PathBuf::from("/unused"),
      queue,
      dropped: AtomicU64::new(0),
    };
    assert!(log.file.set(sink).is_ok());
    served(&log, "/m/a.gguf", "L1", Usage::default());
    assert_eq!(log.file_status().expect("file status").dropped, 0);
    served(&log, "/m/a.gguf", "L1", Usage::default());
    served(&log, "/m/a.gguf", "L1", Usage::default());
    assert_eq!(log.file_status().expect("file status").dropped, 2);
  }

  #[test]
  fn a_stopped_launch_keeps_its_totals_and_a_late_finish_does_not_bring_it_back() {
    let log = RequestLog::new();
    served(&log, "/m/a.gguf", "L1", Usage::default());
    // Still streaming when the launch is stopped.
    let mut late = log.begin(ROUTE, None);
    late.set_model("m", "/m/a.gguf");
    late.set_launch("L1");
    late.set_status(200);
    assert_eq!(log.launch_summary("L1").requests, 1);

    log.forget_launch("L1");
    assert_eq!(log.launch_summary("L1").requests, 1);
    assert_eq!(
      log.lock().by_launch.len(),
      0,
      "the live entry is retired, not left behind"
    );
    late.finish(RequestState::UpstreamError, Usage::default());
    assert_eq!(log.lock().by_launch.len(), 0);
    // The model's own totals keep both requests.
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/a.gguf".to_string()
          },
          0
        )
        .summary
        .requests,
      2
    );
  }

  /// `launch_summary` answers with `tail`'s summary for the same launch scope,
  /// so a figure `tail` stamps cannot skip the per-launch surfaces.
  #[test]
  fn a_launch_summary_equals_the_launch_scope_tail_summary() {
    let log = RequestLog::new();
    let scope = |id: &str| LogScope::Launch { id: id.to_string() };
    served_from(&log, "/m/a.gguf", "L1", "192.168.1.7:5001");
    served(&log, "/m/b.gguf", "L1", Usage::default());
    assert_eq!(log.launch_summary("L1"), log.tail(scope("L1"), 0).summary);

    log.forget_launch("L1");
    assert_eq!(log.launch_summary("L1"), log.tail(scope("L1"), 0).summary);
    // A launch the log never heard of answers the same way on both paths.
    assert_eq!(log.launch_summary("L9"), log.tail(scope("L9"), 0).summary);
  }

  #[test]
  fn retirement_keeps_the_last_sixteen_stopped_launches_oldest_first() {
    let log = RequestLog::new();
    let ids: Vec<String> = (1..=17).map(|n| format!("L{n}")).collect();
    for id in &ids {
      served(&log, "/m/a.gguf", id, Usage::default());
    }
    for id in &ids {
      log.forget_launch(id);
    }
    assert_eq!(
      log.launch_summary("L1"),
      zeros(),
      "the oldest of seventeen is pushed out"
    );
    for id in &ids[1..] {
      assert_eq!(log.launch_summary(id).requests, 1, "{id} is retired");
    }
  }

  #[test]
  fn a_model_scope_reports_the_newest_launch_speed_as_the_session() {
    let speed = |t: f64| Usage {
      completion_tokens: Some(10),
      tokens_per_second: Some(t),
      ..Usage::default()
    };
    let log = RequestLog::new();
    served(&log, "/m/a.gguf", "L1", speed(80.0));
    served(&log, "/m/a.gguf", "L1", speed(60.0));
    served(&log, "/m/a.gguf", "L2", speed(20.0));

    let model = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        10,
      )
      .summary;
    assert_eq!(
      model.tokens_per_second_avg,
      Some((80.0 + 60.0 + 20.0) / 3.0)
    );
    // The newest launch of the model runs at 20, and that is the session.
    assert_eq!(model.tokens_per_second_session, Some(20.0));
    // The whole log has no single session to name, and a launch scope is
    // already one launch, so neither gets the extra figure.
    assert_eq!(
      log
        .tail(LogScope::All, 10)
        .summary
        .tokens_per_second_session,
      None
    );
    assert_eq!(
      log
        .tail(
          LogScope::Launch {
            id: "L1".to_string()
          },
          10,
        )
        .summary
        .tokens_per_second_session,
      None
    );
    // The strip shows the figure only when there is one, so an old daemon
    // (no key in the JSON) renders exactly as before.
    let with = model.cells("-", Aggregates::WhenTheyDiffer);
    assert!(with.iter().any(|(k, _)| *k == "Tok/s session"));
    assert_eq!(
      with[with
        .iter()
        .position(|(k, _)| *k == "Tok/s session")
        .unwrap()]
      .1,
      "20.0"
    );
    assert!(!RequestSummary::default()
      .cells("-", Aggregates::WhenTheyDiffer)
      .iter()
      .any(|(k, _)| *k == "Tok/s session"));
  }

  #[test]
  fn a_launch_scope_sees_one_launch_of_a_model() {
    let log = RequestLog::new();
    let a1 = served(&log, "/m/a.gguf", "L1", Usage::default());
    served(&log, "/m/a.gguf", "L2", Usage::default());
    let a3 = served(&log, "/m/a.gguf", "L1", Usage::default());

    let scoped = log.tail(
      LogScope::Launch {
        id: "L1".to_string(),
      },
      10,
    );
    assert_eq!(
      scoped.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![a3, a1]
    );
    assert_eq!(scoped.summary.requests, 2);
    // The same model under another launch is the wider scope.
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/a.gguf".to_string()
          },
          0
        )
        .summary
        .requests,
      3
    );
    // A launch's series carries the model's own name, not the launch id.
    assert_eq!(log.launch_label("L1").as_deref(), Some("m"));
    assert_eq!(
      log
        .tail(
          LogScope::Launch {
            id: "L9".to_string()
          },
          10
        )
        .summary,
      zeros(),
      "a launch that served nothing is an empty scope"
    );
  }

  #[test]
  fn a_retired_launch_still_scopes_rows_and_totals() {
    let log = RequestLog::new();
    let seq = served(&log, "/m/a.gguf", "L1", Usage::default());
    log.forget_launch("L1");

    let scoped = log.tail(
      LogScope::Launch {
        id: "L1".to_string(),
      },
      10,
    );
    assert_eq!(
      scoped.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![seq]
    );
    assert_eq!(scoped.summary.requests, 1);
    assert_eq!(log.launch_label("L1").as_deref(), Some("m"));
  }

  #[test]
  fn reading_tolerates_missing_keys() {
    let row: RequestRow = serde_json::from_str(r#"{"seq": 4, "status": 200}"#).unwrap();
    assert_eq!((row.seq, row.status), (4, Some(200)));
    assert_eq!(row.state, RequestState::InFlight);
    assert!(!row.alias, "a row that predates `alias` is not one");
    let summary: RequestSummary = serde_json::from_str(r#"{"requests": 2}"#).unwrap();
    assert_eq!(summary.requests, 2);
    assert_eq!(summary.avg_duration_ms, None);
    assert_eq!(summary.tokens_per_second_aggregate, None);
    assert_eq!(summary.busy_ms, None);
  }

  #[test]
  fn row_wire_shape_keeps_every_key() {
    let log = RequestLog::new();
    drop(log.begin(ROUTE, None));
    let v = serde_json::to_value(&log.tail(LogScope::All, 1).rows[0]).unwrap();
    for key in [
      "seq",
      "started_at_ms",
      "client",
      "route",
      "requested_model",
      "model",
      "model_path",
      "launch_id",
      "state",
      "status",
      "error",
      "cause",
      "auto_start",
      "alias",
      "evicted",
      "fallback",
      "ttfb_ms",
      "duration_ms",
      "prompt_tokens",
      "completion_tokens",
      "tokens_per_second",
      "tokens_per_second_estimated",
      "prefill_tokens",
      "prefill_tokens_per_second",
      "prefill_estimated",
      "cached_tokens",
      "cache_hit_ratio",
    ] {
      assert!(v.get(key).is_some(), "missing key {key}");
    }
    assert_eq!(v["state"], "client_closed");
  }

  #[test]
  fn an_alias_reads_in_the_note_without_replacing_anything() {
    // A bare alias row: `alias` is the only thing beyond the status.
    let alias = RequestRow {
      state: RequestState::Done,
      status: Some(200),
      alias: true,
      ..RequestRow::default()
    };
    assert_eq!(alias.note(), "alias");

    // The flag composes with the rest and leads the note.
    let all = RequestRow {
      auto_start: true,
      evicted: vec!["L3".to_string()],
      fallback: Some("launch_failed".to_string()),
      ..alias.clone()
    };
    assert_eq!(
      all.note(),
      "alias, auto-start, unloaded L3, fallback (launch_failed)"
    );

    // In flight: the alias is settled at routing time, so it shows while the
    // auto-start mark is still held back.
    let inflight = RequestRow {
      state: RequestState::InFlight,
      launch_id: Some("L1".to_string()),
      auto_start: true,
      alias: true,
      ..RequestRow::default()
    };
    assert_eq!(inflight.note(), "in flight, alias");
  }
}
