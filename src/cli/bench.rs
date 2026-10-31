//! `llamastash bench <model-ref> [--preset a,b]` — per-preset speed numbers.
//!
//! For each preset: start the model, wait for Ready, send a fixed prompt N times
//! through the proxy, read the numbers the proxy's request log already recorded
//! for those requests, then stop the launch this command started. The numbers come
//! from the same fields `llamastash requests` prints, so a server that reports no
//! speeds still gets numbers: the proxy's estimates, marked `~`.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cli::cli_args::{BenchArgs, Cli, LaunchMode, LaunchOverrides};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult, LAUNCH_FAILED, UNKNOWN, USAGE};
use crate::cli::format::table;
use crate::cli::output::pretty_json;
use crate::cli::resolve::{fetch_catalog, fetch_status, resolve_model, CatalogRow, RunningRow};
use crate::cli::session::{not_chat, proxy_api_key, proxy_base_url, published_id};
use crate::cli::start::{map_start_error, preset_baseline, start_payload, PartialParams};
use crate::config::Config;
use crate::ipc::Client;
use crate::launch::presets::AUTO_DEFAULT;
use crate::launch::resolve::join_named_reference;
use crate::proxy::request_log::{fmt_ms, RequestRow, RequestState, Tail};

/// Cell text for a number no run produced.
const NONE: &str = "-";

/// How long the log can take to finish a row after the client's own read ends.
/// The proxy ends the row when the response body is dropped, which trails the
/// client by a scheduler tick.
const ROW_READBACK: Duration = Duration::from_secs(5);

/// Ceiling on one model load, same as `start --wait`'s.
const LOAD_CEILING: Duration = Duration::from_secs(900);

/// Ready poll interval. Shorter than `start --wait`'s, and it settles on `ready`
/// alone on purpose: `poll_until_settled` waits up to 5 s more for `resolved_ctx`,
/// which would inflate the load number.
const READY_POLL: Duration = Duration::from_millis(250);

/// The paragraph the built-in prompt repeats: plain prose, long enough that a
/// handful of repeats lands near 1k tokens.
const PROMPT_PARAGRAPH: &str = "The harbour kept its own hours. Fishing boats came back before dawn and \
the ice was already hard by the time the crews reached the quay, so the morning sale happened in the dark \
under sodium lamps while the gulls argued overhead. Nobody hurried. The crates went up the slip one at a \
time, weighed on a scale older than anyone still working, and written into a ledger nobody reads any more \
out of habit rather than need. By eight the yard was quiet again, the water flat, the smell of diesel and \
seawater the only thing left to show that anything had happened at all.";

/// The built-in prompt: an instruction plus [`PROMPT_PARAGRAPH`] repeated to
/// roughly 1k tokens, so prompt processing dominates the first number. Same text
/// every run and every preset, which is what makes the rows comparable.
fn built_in_prompt() -> String {
  let mut out = String::from("Summarise the following passage in one sentence.\n\n");
  for _ in 0..8 {
    out.push_str(PROMPT_PARAGRAPH);
    out.push(' ');
  }
  out
}

/// Each run's message gets its own prefix, so the KV cache misses from the first
/// user token and every run re-computes the prompt. Engine-neutral: no
/// engine-specific body field.
fn run_prompt(prompt: &str, run: u32) -> String {
  format!("Run {run}. {prompt}")
}

/// The prompt the runs send: the file, the flag, or the built-in one.
fn resolve_prompt(args: &BenchArgs) -> Result<String, CliExit> {
  if let Some(path) = &args.prompt_file {
    let text = std::fs::read_to_string(path)
      .map_err(|e| CliExit::new(USAGE, format!("cannot read `{}`: {e}", path.display())))?;
    return Ok(text.trim_end().to_string());
  }
  Ok(args.prompt.clone().unwrap_or_else(built_in_prompt))
}

/// One run's numbers, as the request log recorded them.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
struct Sample {
  prompt_tokens: Option<u64>,
  completion_tokens: Option<u64>,
  prefill_tokens_per_second: Option<f64>,
  prefill_estimated: bool,
  tokens_per_second: Option<f64>,
  tokens_per_second_estimated: bool,
  /// Time to the first generated token, i.e. the row's `prefill_end_ms`. Only a
  /// streamed response carries it.
  ttft_ms: Option<u64>,
}

impl Sample {
  fn from_row(row: &RequestRow) -> Self {
    Self {
      prompt_tokens: row.prompt_tokens,
      completion_tokens: row.completion_tokens,
      prefill_tokens_per_second: row.prefill_tokens_per_second,
      prefill_estimated: row.prefill_estimated,
      tokens_per_second: row.tokens_per_second,
      tokens_per_second_estimated: row.tokens_per_second_estimated,
      ttft_ms: row.prefill_end_ms,
    }
  }

  /// The request-log shape of these numbers, so the table gets the same `~` marks
  /// and rounding `llamastash requests` prints.
  fn as_row(&self) -> RequestRow {
    RequestRow {
      prompt_tokens: self.prompt_tokens,
      completion_tokens: self.completion_tokens,
      prefill_tokens_per_second: self.prefill_tokens_per_second,
      prefill_estimated: self.prefill_estimated,
      tokens_per_second: self.tokens_per_second,
      tokens_per_second_estimated: self.tokens_per_second_estimated,
      ..RequestRow::default()
    }
  }

  /// Median per metric over the runs that produced numbers. A metric counts as
  /// estimated when any run's value was; one no run reported stays `null`.
  fn aggregate(samples: &[Sample]) -> Self {
    let median_of = |get: fn(&Sample) -> Option<f64>| median(samples.iter().map(get).collect());
    let count = |get: fn(&Sample) -> Option<u64>| {
      let as_f64 = |s: &Sample| get(s).map(|v| v as f64);
      median(samples.iter().map(as_f64).collect()).map(|v| v.round() as u64)
    };
    Self {
      prompt_tokens: count(|s| s.prompt_tokens),
      completion_tokens: count(|s| s.completion_tokens),
      prefill_tokens_per_second: median_of(|s| s.prefill_tokens_per_second),
      prefill_estimated: samples.iter().any(|s| s.prefill_estimated),
      tokens_per_second: median_of(|s| s.tokens_per_second),
      tokens_per_second_estimated: samples.iter().any(|s| s.tokens_per_second_estimated),
      ttft_ms: count(|s| s.ttft_ms),
    }
  }
}

/// Median of the values that came in; `None` when none did. An even count takes
/// the mean of the middle two.
fn median(values: Vec<Option<f64>>) -> Option<f64> {
  let mut v: Vec<f64> = values.into_iter().flatten().collect();
  if v.is_empty() {
    return None;
  }
  v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
  let mid = v.len() / 2;
  Some(if v.len() % 2 == 1 {
    v[mid]
  } else {
    (v[mid - 1] + v[mid]) / 2.0
  })
}

/// One preset's row: the aggregate over its runs, plus the runs themselves.
#[derive(Debug, Default)]
struct PresetResult {
  /// `None` is the model's own `default:` preset; `Some("auto")` is pure fit.
  preset: Option<String>,
  launch_id: Option<String>,
  load_ms: Option<u64>,
  error: Option<String>,
  samples: Vec<Sample>,
}

impl PresetResult {
  fn failed(preset: Option<String>, error: String) -> Self {
    Self {
      preset,
      error: Some(error),
      ..Self::default()
    }
  }

  /// `default` for no preset, otherwise the preset's name — which is `auto` for a
  /// pure-fit cycle.
  fn label(&self) -> String {
    self.preset.clone().unwrap_or_else(|| "default".to_string())
  }

  fn json(&self) -> Value {
    let agg = Sample::aggregate(&self.samples);
    json!({
      "preset": self.preset,
      "launch_id": self.launch_id,
      "load_ms": self.load_ms,
      "prompt_tokens": agg.prompt_tokens,
      "prefill_tokens_per_second": agg.prefill_tokens_per_second,
      "prefill_estimated": agg.prefill_estimated,
      "completion_tokens": agg.completion_tokens,
      "tokens_per_second": agg.tokens_per_second,
      "tokens_per_second_estimated": agg.tokens_per_second_estimated,
      "ttft_ms": agg.ttft_ms,
      "runs_done": self.samples.len(),
      "error": self.error,
      "samples": self.samples,
    })
  }

  fn cells(&self, runs: u32) -> Vec<String> {
    let agg = Sample::aggregate(&self.samples);
    let c = agg.as_row().cells(NONE);
    vec![
      self.label(),
      self.load_ms.map(fmt_ms).unwrap_or_else(|| NONE.into()),
      c.prefill,
      c.speed,
      agg.ttft_ms.map(fmt_ms).unwrap_or_else(|| NONE.into()),
      format!("{}/{}", self.samples.len(), runs),
      self.error.clone().unwrap_or_else(|| NONE.into()),
    ]
  }
}

const HEADER: [&str; 7] = [
  "PRESET", "LOAD", "PP tok/s", "TG tok/s", "TTFT", "RUNS", "NOTE",
];

/// Everything the cycles share, fixed before the first launch so one preset's
/// failure cannot change what the next one does.
struct Runner {
  row: CatalogRow,
  /// The id the proxy publishes this model under.
  published: String,
  http: reqwest::Client,
  proxy: String,
  key: String,
  prompt: String,
  runs: u32,
  max_tokens: u32,
  json: bool,
  quiet: bool,
  overrides: LaunchOverrides,
}

impl Runner {
  fn progress(&self, msg: &str) {
    if !self.json && !self.quiet {
      eprintln!("{msg}");
    }
  }

  /// One preset, end to end. Every failure inside a started launch becomes this
  /// row's `error` and stops the launch, so a preset that does not fit or dies
  /// mid-run costs one row and the rest still get measured.
  async fn bench_preset(
    &self,
    client: &mut Client,
    preset: Option<&str>,
    base: &PartialParams,
  ) -> PresetResult {
    let label = preset.unwrap_or("default");
    let failed = |error: String| PresetResult::failed(preset.map(str::to_string), error);
    let selection = match preset {
      Some(p) if p == AUTO_DEFAULT => "auto",
      Some(_) => "explicit",
      None => "default",
    };
    // Pure fit stamps no preset name, same as `start --preset auto`.
    let stamped = preset.filter(|p| *p != AUTO_DEFAULT);
    self.progress(&format!("starting {label}…"));
    let payload = match start_payload(
      client,
      &self.row,
      base,
      &self.overrides,
      selection,
      stamped,
      false,
      None,
      self.json,
    )
    .await
    {
      Ok(p) => p,
      Err(e) => return failed(e.to_string()),
    };
    let started = Instant::now();
    let resp = match client
      .call("start_model", Some(payload))
      .await
      .map_err(|e| map_start_error(e, &self.row))
    {
      Ok(v) => v,
      Err(e) => return failed(e.to_string()),
    };
    let Some(launch_id) = resp.get("launch_id").and_then(Value::as_str) else {
      return failed("the daemon accepted the launch without an id".to_string());
    };
    let launch_id = launch_id.to_string();

    self.progress(&format!("waiting for {label} to load…"));
    let ready = match wait_until_ready(client, &launch_id).await {
      Ok(row) => row,
      Err(e) => {
        let _ = stop(client, &launch_id).await;
        return failed(e);
      }
    };
    let load_ms = started.elapsed().as_millis() as u64;
    // A `default:` or a named preset can pin a mode, and only chat answers a chat
    // request.
    if ready.mode != "chat" {
      let _ = stop(client, &launch_id).await;
      return failed(format!("the launch came up as a {} model", ready.mode));
    }
    // The proxy routes a named launch as `<published id>@<name>`, and a preset
    // launch takes the preset's name.
    let model = match ready.name.as_deref() {
      Some(name) => join_named_reference(&self.published, name),
      None => self.published.clone(),
    };

    let mut result = PresetResult {
      preset: preset.map(str::to_string),
      launch_id: Some(launch_id.clone()),
      load_ms: Some(load_ms),
      error: None,
      samples: Vec::new(),
    };
    for run in 1..=self.runs {
      self.progress(&format!("{label}: run {run}/{}", self.runs));
      // Shortcut: if this launch died, the proxy can auto-start a fresh one for
      // the same id and answer from it. This check catches that except in the
      // narrow window between it and the request, and the row check below
      // (`launch_id` must be ours) catches a row from any other launch.
      if !is_ready(client, &launch_id).await {
        result.error = Some("the launch stopped before the run".to_string());
        break;
      }
      let before = match newest_seq(client, &self.row.path).await {
        Ok(seq) => seq,
        Err(e) => {
          result.error = Some(e);
          break;
        }
      };
      if let Err(e) = self.send_run(&model, &run_prompt(&self.prompt, run)).await {
        result.error = Some(e);
        break;
      }
      match served_row(client, &self.row.path, before, &launch_id).await {
        Ok(row) => result.samples.push(Sample::from_row(&row)),
        Err(e) => {
          result.error = Some(e);
          break;
        }
      }
    }
    if let Err(e) = stop(client, &launch_id).await {
      result.error = Some(match result.error.take() {
        Some(done) => format!("{done}; {e}"),
        None => e,
      });
    }
    result
  }

  /// One streamed chat request through the proxy. The body is drained and never
  /// parsed: the numbers come from the row the proxy logs for it, and only a
  /// streamed response has the first-token time.
  async fn send_run(&self, model: &str, prompt: &str) -> Result<(), String> {
    let body = json!({
      "model": model,
      "messages": [{ "role": "user", "content": prompt }],
      "max_tokens": self.max_tokens,
      "temperature": 0,
      "stream": true,
      "stream_options": { "include_usage": true },
    });
    let resp = self
      .http
      .post(format!("{}/chat/completions", self.proxy))
      .bearer_auth(&self.key)
      .json(&body)
      .send()
      .await
      .map_err(|e| format!("the request did not reach the proxy: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
      let detail = resp.text().await.unwrap_or_default();
      let first = detail.lines().next().unwrap_or_default();
      return Err(format!("the proxy answered {status}: {first}"));
    }
    let mut bytes = 0usize;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
      match chunk {
        Ok(b) => bytes += b.len(),
        Err(e) => return Err(format!("the stream ended early: {e}")),
      }
    }
    if bytes == 0 {
      return Err("the response carried no body".to_string());
    }
    Ok(())
  }
}

/// The newest `seq` the log holds for this model, so the row this run produced is
/// the one after it.
async fn newest_seq(client: &mut Client, model_path: &str) -> Result<u64, String> {
  let rows = tail(client, model_path, 1).await?;
  Ok(rows.first().map(|r| r.seq).unwrap_or(0))
}

async fn tail(
  client: &mut Client,
  model_path: &str,
  limit: u32,
) -> Result<Vec<RequestRow>, String> {
  let body = client
    .call(
      "requests_tail",
      Some(json!({ "model_path": model_path, "limit": limit })),
    )
    .await
    .map_err(|e| format!("requests_tail: {e}"))?;
  Tail::deserialize(&body)
    .map(|t| t.rows)
    .map_err(|e| format!("requests_tail: unreadable response: {e}"))
}

/// The oldest row after `after` is this run's: a later one from another client on
/// the same launch must not stand in for it, so it is `None` while ours is in flight.
fn finished_run_row(rows: Vec<RequestRow>, after: u64) -> Option<RequestRow> {
  rows
    .into_iter()
    .filter(|r| r.seq > after)
    .min_by_key(|r| r.seq)
    .filter(|r| r.state != RequestState::InFlight)
}

/// The row this run produced, once the proxy has finished it. A row scoped to the
/// model that belongs to another launch means the request was served elsewhere,
/// including by a launch the proxy auto-started after this one died — which bench
/// caused, so it stops that one too.
async fn served_row(
  client: &mut Client,
  model_path: &str,
  after: u64,
  ours: &str,
) -> Result<RequestRow, String> {
  let deadline = Instant::now() + ROW_READBACK;
  let row = loop {
    if let Some(row) = finished_run_row(tail(client, model_path, 20).await?, after) {
      break row;
    }
    if Instant::now() >= deadline {
      return Err("the request never reached the request log".to_string());
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  };
  if row.launch_id.as_deref() != Some(ours) {
    let other = row.launch_id.clone().unwrap_or_else(|| "?".into());
    if row.auto_start {
      let _ = stop(client, &other).await;
    }
    return Err(format!(
      "launch {other} answered the request instead of {ours}"
    ));
  }
  if row.is_error() {
    let note = row.note();
    let status = row.status.map(|s| s.to_string()).unwrap_or_default();
    return Err(if note.is_empty() {
      format!("the proxy answered {status}")
    } else {
      format!("the proxy answered {status}: {note}")
    });
  }
  Ok(row)
}

/// Poll `status` until this launch reports `ready`. The load number is the
/// client's own clock between the `start_model` call and this first Ready, so
/// nothing extra is waited for.
async fn wait_until_ready(client: &mut Client, launch_id: &str) -> Result<RunningRow, String> {
  let deadline = Instant::now() + LOAD_CEILING;
  while Instant::now() < deadline {
    let snap = fetch_status(client)
      .await
      .map_err(|e| format!("status: {e}"))?;
    match snap.models.into_iter().find(|m| m.launch_id == launch_id) {
      Some(row) if row.state == "ready" => return Ok(row),
      Some(row) if matches!(row.state.as_str(), "error" | "stopped") => {
        return Err(match row.state_cause {
          Some(cause) => format!("the launch is {}: {cause}", row.state),
          None => format!("the launch is {}", row.state),
        })
      }
      _ => tokio::time::sleep(READY_POLL).await,
    }
  }
  Err("the model never came ready in time".to_string())
}

async fn is_ready(client: &mut Client, launch_id: &str) -> bool {
  match fetch_status(client).await {
    Ok(snap) => snap
      .models
      .iter()
      .any(|m| m.launch_id == launch_id && m.state == "ready"),
    Err(_) => false,
  }
}

/// Stop one launch this command started. A failure is that preset's note, not an
/// abort: the remaining presets still get measured.
async fn stop(client: &mut Client, launch_id: &str) -> Result<(), String> {
  // The daemon can wait the launch's full stop grace before SIGKILL; a client
  // timeout inside it cancels the stop half-done. Same budget `stop` uses.
  let grace = fetch_status(client)
    .await
    .ok()
    .and_then(|s| s.models.into_iter().find(|m| m.launch_id == launch_id))
    .map_or(0, |m| m.stop_grace_secs);
  client
    .call_with_timeout(
      "stop_model",
      Some(json!({ "launch_id": launch_id })),
      Duration::from_secs(grace.max(5).saturating_add(5)),
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("stop_model {launch_id}: {e}"))
}

/// A launch of `path` that still holds the model, i.e. is not stopped or errored.
fn live_launch_of<'a>(rows: &'a [RunningRow], path: &str) -> Option<&'a RunningRow> {
  rows
    .iter()
    .find(|r| r.model_path == path && !matches!(r.state.as_str(), "stopped" | "error"))
}

/// How to point at a launch in a message: its id, plus its name when it has one.
fn describe_launch(row: &RunningRow) -> String {
  match row.name.as_deref() {
    Some(name) => format!("{} ({})", row.launch_id, name),
    None => row.launch_id.clone(),
  }
}

/// Stop every live launch of one model path and say which ones went. Ctrl-C owes
/// this: bench's launch is the only one of the model, which is what the up-front
/// refusal guaranteed.
async fn stop_live_of(client: &mut Client, path: &str) -> Vec<String> {
  let Ok(snap) = fetch_status(client).await else {
    return Vec::new();
  };
  let mut stopped = Vec::new();
  for row in snap
    .models
    .iter()
    .filter(|r| r.model_path == path && !matches!(r.state.as_str(), "stopped" | "error"))
  {
    let label = describe_launch(row);
    match stop(client, &row.launch_id).await {
      Ok(()) => stopped.push(label),
      Err(e) => eprintln!("{e}"),
    }
  }
  stopped
}

/// A `--mode` that cannot answer the prompt bench sends. `--mode chat` is the one
/// bench launches, so it is accepted like the `start` override it mirrors.
fn non_chat_mode(mode: Option<LaunchMode>) -> Option<&'static str> {
  match mode {
    Some(m) if m != LaunchMode::Chat => Some(m.as_label()),
    _ => None,
  }
}

pub async fn handle(args: BenchArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let catalog = fetch_catalog(&mut client).await?;
  let row = resolve_model(&catalog, &args.model)?;

  // Chat only: nothing else answers the prompt bench sends. `not_chat` is the same
  // gate `session` uses; `resolve_mode`'s answer would not do here because it is
  // `None` for a mode the model's header merely implies.
  let published = published_id(&catalog, &row);
  if let Some(kind) = not_chat(&row, &published) {
    return Err(CliExit::new(
      USAGE,
      format!(
        "`{}` is {kind} model; bench sends chat requests",
        row.name()
      ),
    ));
  }
  if let Some(label) = non_chat_mode(args.overrides.mode) {
    return Err(CliExit::new(
      USAGE,
      format!("bench sends chat requests; --mode {label} is not chat"),
    ));
  }

  // A model that already runs is refused before any launch, so the launch bench
  // starts is the only one of the model: nothing else gets measured or stopped.
  let status = fetch_status(&mut client).await?;
  if let Some(live) = live_launch_of(&status.models, &row.path) {
    return Err(CliExit::new(
      USAGE,
      format!(
        "`{}` is already running as {}; stop it first",
        row.name(),
        describe_launch(live)
      ),
    ));
  }

  // The presets and their baselines, before the first launch: an unknown preset
  // name is a usage error, not a failed row after a minute of loading.
  let presets: Vec<Option<String>> = if args.preset.is_empty() {
    vec![None]
  } else {
    args.preset.iter().map(|p| Some(p.clone())).collect()
  };
  let mut baselines = Vec::with_capacity(presets.len());
  for preset in &presets {
    baselines.push(preset_baseline(&mut client, &row, preset.as_deref()).await?);
  }

  let runner = Runner {
    row: row.clone(),
    published,
    // No total timeout: a long generation is the thing being measured. Same shape
    // as the TUI's client.
    http: reqwest::Client::builder()
      .connect_timeout(Duration::from_secs(10))
      .read_timeout(Duration::from_secs(300))
      .build()
      .map_err(|e| CliExit::new(UNKNOWN, format!("http client: {e}")))?,
    proxy: proxy_base_url(&status)?,
    key: proxy_api_key(config),
    prompt: resolve_prompt(&args)?,
    runs: args.runs,
    max_tokens: args.max_tokens,
    json: args.json,
    quiet: cli.quiet,
    overrides: args.overrides.clone(),
  };

  let mut results: Vec<PresetResult> = Vec::with_capacity(presets.len());
  let cycles = async {
    for (preset, base) in presets.iter().zip(baselines) {
      results.push(
        runner
          .bench_preset(&mut client, preset.as_deref(), &base)
          .await,
      );
    }
  };
  tokio::select! {
    _ = tokio::signal::ctrl_c() => {
      // The cycle above is dropped here, mid-request if need be, and it holds the
      // only client, so the stop goes through one of its own.
      let mut stopper = connect_or_spawn(cli, config).await?;
      let stopped = stop_live_of(&mut stopper, &row.path).await;
      eprintln!(
        "interrupted; stopped {}",
        if stopped.is_empty() { "nothing".to_string() } else { stopped.join(", ") }
      );
      return Err(CliExit::new(UNKNOWN, "interrupted before the bench finished"));
    }
    _ = cycles => {}
  }

  if args.json {
    println!(
      "{}",
      pretty_json(&json!({
        "model": row.name(),
        "runs": args.runs,
        "max_tokens": args.max_tokens,
        "results": results.iter().map(PresetResult::json).collect::<Vec<_>>(),
      }))
    );
  } else {
    let rows: Vec<Vec<String>> = results.iter().map(|r| r.cells(args.runs)).collect();
    print!("{}", table(&HEADER, &rows));
  }
  // A preset that failed is a printed row, and the exit code says so too.
  if results.iter().any(|r| r.error.is_some()) {
    return Err(CliExit::code_only(LAUNCH_FAILED));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::cli_args::Command;
  use crate::cli::resolve::running_rows_in;
  use clap::Parser;

  fn tps(v: Option<f64>) -> Sample {
    Sample {
      tokens_per_second: v,
      ..Sample::default()
    }
  }

  fn bench(argv: &[&str]) -> BenchArgs {
    match Cli::try_parse_from(std::iter::once("llamastash").chain(argv.iter().copied()))
      .unwrap()
      .command
    {
      Some(Command::Bench(args)) => args,
      other => panic!("expected bench, got {other:?}"),
    }
  }

  #[test]
  fn the_run_row_is_the_oldest_after_the_mark_and_only_once_finished() {
    let row = |seq, state| RequestRow {
      seq,
      state,
      ..RequestRow::default()
    };
    // Newest first, as `requests_tail` returns them.
    let ours_in_flight = vec![
      row(7, RequestState::Done),
      row(6, RequestState::InFlight),
      row(5, RequestState::Done),
    ];
    assert_eq!(finished_run_row(ours_in_flight, 5), None);
    let ours_done = vec![row(7, RequestState::Done), row(6, RequestState::Done)];
    assert_eq!(finished_run_row(ours_done, 5).map(|r| r.seq), Some(6));
    assert_eq!(finished_run_row(vec![row(5, RequestState::Done)], 5), None);
  }

  #[test]
  fn an_odd_count_takes_the_middle_run() {
    let samples = vec![tps(Some(30.0)), tps(Some(41.0)), tps(Some(39.0))];
    assert_eq!(Sample::aggregate(&samples).tokens_per_second, Some(39.0));
  }

  #[test]
  fn an_even_count_averages_the_middle_two() {
    let samples = vec![
      tps(Some(30.0)),
      tps(Some(41.0)),
      tps(Some(39.0)),
      tps(Some(20.0)),
    ];
    assert_eq!(Sample::aggregate(&samples).tokens_per_second, Some(34.5));
  }

  #[test]
  fn one_run_is_its_own_number() {
    assert_eq!(
      Sample::aggregate(&[tps(Some(41.0))]).tokens_per_second,
      Some(41.0)
    );
  }

  /// One estimated run makes the aggregate estimated, because the number the row
  /// prints is not one the server reported.
  #[test]
  fn a_metric_counts_as_estimated_when_any_run_was() {
    let samples = vec![
      Sample {
        tokens_per_second: Some(30.0),
        tokens_per_second_estimated: true,
        ..Sample::default()
      },
      tps(Some(40.0)),
    ];
    let agg = Sample::aggregate(&samples);
    assert!(agg.tokens_per_second_estimated);
    assert!(
      !agg.prefill_estimated,
      "a metric nobody estimated stays false"
    );
  }

  /// A metric no run reported stays `null`, which is not the same answer as a
  /// speed of zero.
  #[test]
  fn a_metric_no_run_reported_stays_null() {
    let agg = Sample::aggregate(&[Sample::default(), Sample::default()]);
    assert_eq!(agg.tokens_per_second, None);
    assert_eq!(agg.prefill_tokens_per_second, None);
    assert_eq!(agg.ttft_ms, None);
    assert_eq!(agg.prompt_tokens, None);
  }

  #[test]
  fn each_run_gets_its_own_prefix() {
    assert_eq!(run_prompt("hello", 1), "Run 1. hello");
    assert_eq!(run_prompt("hello", 3), "Run 3. hello");
    assert_ne!(run_prompt("hello", 1), run_prompt("hello", 2));
  }

  /// The built-in prompt has to be long enough that prefill means something, and
  /// the same text every time or the rows are not comparable.
  #[test]
  fn the_built_in_prompt_is_long_and_stable() {
    let prompt = built_in_prompt();
    assert!(
      prompt.len() > 4_000,
      "{} bytes is no 1k-token prompt",
      prompt.len()
    );
    assert_eq!(prompt, built_in_prompt());
  }

  #[test]
  fn a_custom_prompt_still_gets_the_run_prefix() {
    let args = bench(&["bench", "m", "--prompt", "be brief"]);
    assert_eq!(resolve_prompt(&args).unwrap(), "be brief");
    assert_eq!(
      run_prompt(&resolve_prompt(&args).unwrap(), 2),
      "Run 2. be brief"
    );
  }

  #[test]
  fn a_row_renders_the_documented_json_keys() {
    let ok = PresetResult {
      preset: Some("coder".into()),
      launch_id: Some("L4".into()),
      load_ms: Some(8123),
      error: None,
      samples: vec![Sample {
        prompt_tokens: Some(1012),
        completion_tokens: Some(256),
        prefill_tokens_per_second: Some(812.4),
        tokens_per_second: Some(41.2),
        ttft_ms: Some(1310),
        ..Sample::default()
      }],
    };
    let v = ok.json();
    assert_eq!(v["preset"], "coder");
    assert_eq!(v["launch_id"], "L4");
    assert_eq!(v["load_ms"], 8123);
    assert_eq!(v["prompt_tokens"], 1012);
    assert_eq!(v["prefill_tokens_per_second"], 812.4);
    assert_eq!(v["prefill_estimated"], false);
    assert_eq!(v["completion_tokens"], 256);
    assert_eq!(v["tokens_per_second"], 41.2);
    assert_eq!(v["tokens_per_second_estimated"], false);
    assert_eq!(v["ttft_ms"], 1310);
    assert!(v["error"].is_null());
    assert_eq!(v["samples"].as_array().unwrap().len(), 1);

    let bad = PresetResult::failed(None, "the model did not fit".into()).json();
    assert!(bad["preset"].is_null(), "no preset is null, not a name");
    assert!(bad["tokens_per_second"].is_null());
    assert_eq!(bad["error"], "the model did not fit");
    assert!(bad["samples"].as_array().unwrap().is_empty());
  }

  /// `preset: null` prints as `default`, and a number nothing produced prints as
  /// the placeholder instead of a figure of zero.
  #[test]
  fn the_table_labels_an_unnamed_preset_and_blanks_what_is_missing() {
    let cells = PresetResult::failed(None, "boom".into()).cells(3);
    assert_eq!(cells[0], "default");
    assert_eq!(&cells[1..5], [NONE, NONE, NONE, NONE]);
    assert_eq!(cells[5], "0/3");
    assert_eq!(cells[6], "boom");
  }

  /// An estimated speed prints with the `~` `llamastash requests` uses, which is
  /// the point of reading the numbers off the request log.
  #[test]
  fn the_table_marks_an_estimated_speed() {
    let row = PresetResult {
      preset: Some("auto".into()),
      samples: vec![Sample {
        tokens_per_second: Some(41.2),
        tokens_per_second_estimated: true,
        ..Sample::default()
      }],
      ..PresetResult::default()
    };
    let cells = row.cells(1);
    assert_eq!(cells[0], "auto");
    assert!(cells[3].starts_with('~'), "{}", cells[3]);
  }

  #[test]
  fn a_running_launch_is_named_by_id_and_name() {
    let rows = running_rows_in(&json!({
      "models": [
        { "launch_id": "L7", "id": {"path": "/m/m.gguf"}, "port": 41100,
          "mode": "chat", "state": "ready", "name": "coder" },
        { "launch_id": "L8", "id": {"path": "/m/other.gguf"}, "port": 41101,
          "mode": "chat", "state": "stopped" },
      ],
    }));
    let live = live_launch_of(&rows, "/m/m.gguf").expect("L7 holds the model");
    assert_eq!(describe_launch(live), "L7 (coder)");
    assert!(
      live_launch_of(&rows, "/m/other.gguf").is_none(),
      "a stopped launch holds nothing"
    );
    assert!(live_launch_of(&rows, "/m/gone.gguf").is_none());
  }

  /// An errored launch holds nothing either: bench may start the model again.
  #[test]
  fn an_errored_launch_is_not_live() {
    let rows = running_rows_in(&json!({
      "models": [{ "launch_id": "L1", "id": {"path": "/m/m.gguf"}, "port": 41100,
                   "mode": "chat", "state": "error" }],
    }));
    assert!(live_launch_of(&rows, "/m/m.gguf").is_none());
  }

  /// `--mode chat` is what bench launches, so only the other two are refused.
  #[test]
  fn only_a_non_chat_mode_override_is_refused() {
    assert_eq!(non_chat_mode(None), None);
    assert_eq!(non_chat_mode(Some(LaunchMode::Chat)), None);
    assert_eq!(
      non_chat_mode(Some(LaunchMode::Embedding)),
      Some("embedding"),
      "an embedding launch cannot answer the prompt"
    );
    assert_eq!(non_chat_mode(Some(LaunchMode::Rerank)), Some("rerank"));
  }
}
