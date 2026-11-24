//! `GET /metrics/backend`: the engines' own Prometheus numbers, merged into one
//! scrape.
//!
//! Every Ready model launch is re-scraped on each call — its own `/metrics`
//! over loopback, 2 s timeout, 4 MiB cap — and each merged sample line gains
//! `launch` and `model` labels. Everything else the engine printed (name, help,
//! type, its own labels) is left exactly as it came, so the format the upstream
//! documents survives a scrape here.
//!
//! One target failing never fails the response: an unreachable engine, or a
//! llama.cpp started without its `metrics` knob answering `501`, contributes
//! nothing but `llamastash:backend_metrics_available` `0`. That is the reason
//! the fan-out is one endpoint: the naive `for port in …; do curl` breaks a
//! Prometheus scrape on the first such model, and with several models loaded a
//! single cold one would make every scrape red.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{future::join_all, StreamExt};
use hyper::body::Incoming;
use hyper::Request;
use serde_json::{Map, Value};

use super::metrics::{
  self, escape_label, live_launches, value_opt, Document, Format, LiveLaunch, Parsed,
};
use super::router::ProxyResponse;
use super::state::ProxyState;
use crate::util::datetime;

/// Hard cap on one target's response, so a chatty engine cannot balloon the
/// merged body.
const MAX_TARGET_BYTES: usize = 4 * 1024 * 1024;

/// Per-target scrape deadline. A load that hangs must not hold up the scrape.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(2);

/// One launch's scrape, as the merge and the JSON view need it.
struct Scrape {
  launch: String,
  model: String,
  url: String,
  /// HTTP status, `None` when no response arrived.
  status: Option<u16>,
  /// Why the target gave nothing, when it did.
  error: Option<String>,
  /// Body, capped at [`MAX_TARGET_BYTES`].
  text: String,
  truncated: bool,
  /// Sample lines merged from this target.
  series: u64,
  /// Lines this target printed that could not be parsed.
  skipped: u64,
  duration_ms: u64,
}

/// The text of a capped read. When the cap cut the body mid-line, the fragment
/// after the last newline is dropped: a Prometheus body is line-based, and half
/// a sample (`foo_total 12` of a real `foo_total 1234`) parses as a sample and
/// prints a figure the engine never reported.
fn capped_text(buf: Vec<u8>, truncated: bool) -> String {
  let whole = match truncated {
    true => buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1),
    false => buf.len(),
  };
  String::from_utf8_lossy(&buf[..whole]).into_owned()
}

impl Scrape {
  /// Whether the target gave a Prometheus document worth merging. Only a plain
  /// `200` counts, the same bar the `error` field keys on.
  fn available(&self) -> bool {
    self.error.is_none() && self.status == Some(200)
  }

  fn labels(&self) -> Vec<(&str, &str)> {
    vec![
      ("launch", self.launch.as_str()),
      ("model", self.model.as_str()),
    ]
  }
}

/// `GET /metrics/backend`.
pub(crate) async fn serve(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let live = live_launches(&state).await;
  // The scope of a scrape is resolved from the same rows the `/metrics` body of
  // the same request would read, so one read answers the whole request.
  let snapshot = state.ctx.requests.snapshot();
  let parsed = match metrics::parse_request(&state, &req, &live, &snapshot).await {
    Ok(parsed) => parsed,
    Err(reject) => return reject.into_response(),
  };
  let targets = targets(&live, &parsed);
  let mut scrapes = join_all(targets.into_iter().map(|target| scrape(&state, target))).await;
  // The per-target counts come from parsing each engine's text, so the text is
  // parsed whichever form the caller asked for; the JSON form reports those
  // counts and leaves the engine's own text out.
  let mut doc = Document::new(false);
  for scrape in &mut scrapes {
    if scrape.available() {
      merge(&mut doc, scrape);
    }
  }
  if parsed.format == Format::Json {
    return metrics::json_response(json_body(&scrapes, &parsed));
  }
  for scrape in &scrapes {
    let labels = scrape.labels();
    doc.sample(
      "llamastash:backend_metrics_available",
      "Whether each scraped engine answered its own /metrics",
      "gauge",
      &labels,
      if scrape.available() { 1.0 } else { 0.0 },
    );
    doc.sample(
      "llamastash:backend_metrics_series",
      "Engine sample lines this scrape merged",
      "gauge",
      &labels,
      scrape.series as f64,
    );
    doc.sample(
      "llamastash:backend_metrics_scrape_duration_seconds",
      "How long this target took to answer",
      "gauge",
      &labels,
      scrape.duration_ms as f64 / 1000.0,
    );
    doc.sample(
      "llamastash:backend_metrics_skipped",
      "Engine lines this scrape could not parse",
      "gauge",
      &labels,
      scrape.skipped as f64,
    );
  }
  metrics::text_response(doc.render())
}

/// The launches to scrape: Ready, serving a model, inside the request's scope.
fn targets(live: &[LiveLaunch], parsed: &Parsed) -> Vec<LiveLaunch> {
  live
    .iter()
    .filter(|l| l.ready)
    // `keeps_launch` checks the served path too, so one call covers both.
    .filter(|l| parsed.scope.keeps_launch(&l.id, Some(&l.path)))
    .cloned()
    .collect()
}

/// Scrape one engine, timed, and never fail: whatever goes wrong lands in the
/// [`Scrape`] as `error`.
async fn scrape(state: &ProxyState, target: LiveLaunch) -> Scrape {
  let url = format!("http://127.0.0.1:{}/metrics", target.port);
  let started = Instant::now();
  let attempt = tokio::time::timeout(SCRAPE_TIMEOUT, read_body(&state.http_client, &url)).await;
  let duration_ms = started.elapsed().as_millis() as u64;
  let (status, text, truncated, error) = match attempt {
    Err(_) => (
      None,
      String::new(),
      false,
      Some(format!("timed out after {} s", SCRAPE_TIMEOUT.as_secs())),
    ),
    Ok((status, text, truncated, error)) => (status, text, truncated, error),
  };
  // A non-200 answer is a refusal rather than a scrape, so `error` says that too:
  // one field tells a scraper why a target has no series, without reading `status`
  // and the body together.
  let error = error.or_else(|| {
    status
      .filter(|code| *code != 200)
      .map(|code| format!("engine answered {code}"))
  });
  Scrape {
    launch: target.id,
    model: target.model,
    url,
    status,
    error,
    text,
    truncated,
    series: 0,
    skipped: 0,
    duration_ms,
  }
}

/// `(status, body, truncated, error)` of one GET, with the body capped. A send
/// failure reports no status; a read failure keeps what arrived.
async fn read_body(
  client: &reqwest::Client,
  url: &str,
) -> (Option<u16>, String, bool, Option<String>) {
  let resp = match client.get(url).send().await {
    Ok(resp) => resp,
    Err(e) => return (None, String::new(), false, Some(e.to_string())),
  };
  let status = resp.status().as_u16();
  let mut buf: Vec<u8> = Vec::new();
  let mut truncated = false;
  let mut error = None;
  let mut stream = resp.bytes_stream();
  while let Some(chunk) = stream.next().await {
    match chunk {
      Ok(chunk) => {
        if buf.len() + chunk.len() > MAX_TARGET_BYTES {
          buf.extend_from_slice(&chunk[..MAX_TARGET_BYTES - buf.len()]);
          truncated = true;
          break;
        }
        buf.extend_from_slice(&chunk);
      }
      Err(e) => {
        error = Some(e.to_string());
        break;
      }
    }
  }
  (Some(status), capped_text(buf, truncated), truncated, error)
}

/// Fold one engine's Prometheus text into the merged document and count what
/// landed. `# HELP` / `# TYPE` are kept once per metric name (first target
/// wins) and every sample line gains the `launch` and `model` labels.
fn merge(doc: &mut Document, scrape: &mut Scrape) {
  let ours = format!(
    "launch=\"{}\",model=\"{}\"",
    escape_label(&scrape.launch),
    escape_label(&scrape.model)
  );
  for line in scrape.text.lines() {
    let line = line.trim();
    if line.is_empty() {
      continue;
    }
    if let Some(rest) = line.strip_prefix('#') {
      let mut words = rest.split_whitespace();
      let (Some(kind), Some(name)) = (words.next(), words.next()) else {
        scrape.skipped += 1;
        continue;
      };
      match kind {
        "HELP" => doc.declare(name, Some(&words.collect::<Vec<_>>().join(" ")), None),
        "TYPE" => doc.declare(name, None, Some(&words.collect::<Vec<_>>().join(" "))),
        _ => scrape.skipped += 1,
      }
      continue;
    }
    let (name, labels, value) = match parse_sample(line) {
      Some(parsed) => parsed,
      None => {
        scrape.skipped += 1;
        continue;
      }
    };
    let labels = match labels {
      Some(inner) => {
        // The engine's own labels come first, minus any name we are about to set:
        // a sample cannot carry `launch` or `model` twice and still be scrapeable.
        let engine: Vec<&str> = split_labels(inner.trim())
          .into_iter()
          .filter(|pair| {
            !matches!(
              pair.split('=').next().unwrap_or_default(),
              "launch" | "model"
            )
          })
          .collect();
        if engine.is_empty() {
          ours.clone()
        } else {
          format!("{},{}", engine.join(","), ours)
        }
      }
      None => ours.clone(),
    };
    if doc.sample_raw(&name, labels, value) {
      scrape.series += 1;
    }
  }
}

/// One sample line: the metric name, the text between the braces when the line
/// has labels, and the value. A trailing timestamp is accepted and dropped.
fn parse_sample(line: &str) -> Option<(String, Option<String>, f64)> {
  let end = line
    .char_indices()
    .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == ':' || *c == '_'))
    .map(|(i, _)| i)
    .unwrap_or(line.len());
  if end == 0 {
    return None;
  }
  let name = &line[..end];
  let rest = line[end..].trim_start();
  let (labels, rest) = match rest.strip_prefix('{') {
    Some(after) => {
      let mut in_string = false;
      let mut escaped = false;
      let mut close = None;
      for (i, c) in after.char_indices() {
        if escaped {
          escaped = false;
          continue;
        }
        match c {
          '\\' if in_string => escaped = true,
          '"' => in_string = !in_string,
          '}' if !in_string => {
            close = Some(i);
            break;
          }
          _ => {}
        }
      }
      let close = close?;
      (
        Some(after[..close].to_string()),
        after[close + 1..].trim_start(),
      )
    }
    None => (None, rest),
  };
  let value = rest.split_whitespace().next()?.parse::<f64>().ok()?;
  Some((name.to_string(), labels, value))
}

/// The text between the braces split into `name="value"` pairs, respecting
/// quoted values so a comma inside a value does not split a label.
fn split_labels(text: &str) -> Vec<&str> {
  let mut out = Vec::new();
  let mut start = 0;
  let mut in_string = false;
  let mut escaped = false;
  for (i, c) in text.char_indices() {
    if escaped {
      escaped = false;
      continue;
    }
    match c {
      '\\' if in_string => escaped = true,
      '"' => in_string = !in_string,
      ',' if !in_string => {
        let pair = text[start..i].trim();
        if !pair.is_empty() {
          out.push(pair);
        }
        start = i + 1;
      }
      _ => {}
    }
  }
  let last = text[start..].trim();
  if !last.is_empty() {
    out.push(last);
  }
  out
}

/// The JSON view: one entry per target with its own status and the counts parsed
/// from its text. The engine's metric text is not part of it — an engine's
/// series are documented by the engine, not restated here.
fn json_body(scrapes: &[Scrape], parsed: &Parsed) -> Value {
  let mut scope = Map::new();
  scope.insert("model".into(), value_opt(parsed.scope.name.clone()));
  scope.insert("launch".into(), value_opt(parsed.scope.launch.clone()));
  let backends = scrapes
    .iter()
    .map(|scrape| {
      metrics::json_object(|entry| {
        entry.insert("launch".into(), Value::String(scrape.launch.clone()));
        entry.insert("model".into(), Value::String(scrape.model.clone()));
        entry.insert("url".into(), Value::String(scrape.url.clone()));
        entry.insert("status".into(), value_opt(scrape.status));
        entry.insert("ok".into(), Value::Bool(scrape.available()));
        entry.insert("error".into(), value_opt(scrape.error.clone()));
        entry.insert("truncated".into(), Value::Bool(scrape.truncated));
        entry.insert("series".into(), Value::from(scrape.series));
        entry.insert("skipped".into(), Value::from(scrape.skipped));
        entry.insert("duration_ms".into(), Value::from(scrape.duration_ms));
      })
    })
    .collect::<Vec<_>>();
  metrics::json_object(|entry| {
    entry.insert("generated_at_ms".into(), Value::from(datetime::now_ms()));
    entry.insert("scope".into(), Value::Object(scope));
    entry.insert("backends".into(), Value::Array(backends));
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// One engine body plus the counts it should produce.
  fn merged(text: &str, launch: &str, model: &str) -> (String, u64, u64) {
    let mut doc = Document::new(false);
    let mut scrape = Scrape {
      launch: launch.to_string(),
      model: model.to_string(),
      url: String::new(),
      status: Some(200),
      error: None,
      text: text.to_string(),
      truncated: false,
      series: 0,
      skipped: 0,
      duration_ms: 0,
    };
    merge(&mut doc, &mut scrape);
    (doc.render(), scrape.series, scrape.skipped)
  }

  /// A capped read stops wherever the byte cap falls, and a Prometheus sample
  /// is one line: the tail of a cut line is a fragment that parses, so it has to
  /// go before anything reads the body.
  #[test]
  fn backend_drops_the_sample_a_cap_cut_in_half() {
    let whole = "a_total 1\nb_total 222222\n";
    // Cut three bytes into the second line: `b_total 222` would read as a real
    // sample of a figure the engine never printed.
    let cut = &whole.as_bytes()[.."a_total 1\nb_total 22".len()];
    assert_eq!(capped_text(cut.to_vec(), true), "a_total 1\n");
    // Nothing of the last line survived, so nothing of it is printed.
    assert_eq!(
      capped_text("a_total 1\nb_".as_bytes().to_vec(), true),
      "a_total 1\n"
    );
    assert_eq!(capped_text("b_".as_bytes().to_vec(), true), "");
    // Uncapped reads are untouched, trailing newline or not.
    assert_eq!(capped_text(whole.as_bytes().to_vec(), false), whole);
    assert_eq!(
      capped_text("a_total 1\nb_total 2".as_bytes().to_vec(), false),
      "a_total 1\nb_total 2"
    );
  }

  #[test]
  fn backend_parses_sample_lines_with_and_without_labels() {
    assert_eq!(
      parse_sample("llamacpp:tokens_predicted_total 42"),
      Some(("llamacpp:tokens_predicted_total".to_string(), None, 42.0))
    );
    let (name, labels, value) = parse_sample(
      r#"llamacpp:time_to_first_token_seconds_sum{model="x",finish="a\"b"} 1.5 1700000000"#,
    )
    .expect("parses");
    assert_eq!(name, "llamacpp:time_to_first_token_seconds_sum");
    assert_eq!(labels.unwrap(), r#"model="x",finish="a\"b""#);
    assert_eq!(value, 1.5);
    assert!(parse_sample("not a metric").is_none());
    assert!(parse_sample("{orphan=1} 3").is_none());
    assert!(parse_sample(r#"unterminated{model="x 3"#).is_none());
  }

  #[test]
  fn backend_merge_labels_every_sample_and_counts_the_rest() {
    let engine = concat!(
      "# HELP llamacpp:x counted\n",
      "# TYPE llamacpp:x counter\n",
      "llamacpp:x{model_name=\"m\"} 1\n",
      "# a comment we cannot place\n",
      "llamacpp:x 2\n",
      "garbage line\n",
    );
    let (text, series, skipped) = merged(engine, "L1", "m1");
    assert_eq!(series, 2);
    assert_eq!(skipped, 2);
    assert_eq!(
      text,
      concat!(
        "# HELP llamacpp:x counted\n",
        "# TYPE llamacpp:x counter\n",
        "llamacpp:x{model_name=\"m\",launch=\"L1\",model=\"m1\"} 1\n",
        "llamacpp:x{launch=\"L1\",model=\"m1\"} 2\n"
      )
    );
  }

  /// An engine can declare a family under one name and print every sample under
  /// a derived one. Keying the block on the sample line's name instead loses the
  /// declared type — a histogram and a counter both come out `untyped` to
  /// anything that reads `# TYPE` — so the family has to survive while the sample
  /// keeps the name the engine printed.
  #[test]
  fn backend_merge_keeps_a_declared_family_for_derived_sample_names() {
    let engine = concat!(
      "# HELP llamacpp:prompt_tokens prompt tokens\n",
      "# TYPE llamacpp:prompt_tokens counter\n",
      "llamacpp:prompt_tokens_total 21\n",
      "# TYPE llamacpp:time_to_first_token_seconds histogram\n",
      "llamacpp:time_to_first_token_seconds_bucket{le=\"0.1\"} 1\n",
      "llamacpp:time_to_first_token_seconds_sum 0.05\n",
      "llamacpp:time_to_first_token_seconds_count 1\n",
      "# TYPE llamacpp:finished_total counter\n",
      "llamacpp:finished_total 3\n",
    );
    let (text, series, skipped) = merged(engine, "L1", "m1");
    assert_eq!((series, skipped), (5, 0));
    assert_eq!(
      text,
      concat!(
        "# HELP llamacpp:prompt_tokens prompt tokens\n",
        "# TYPE llamacpp:prompt_tokens counter\n",
        "llamacpp:prompt_tokens_total{launch=\"L1\",model=\"m1\"} 21\n",
        "# TYPE llamacpp:time_to_first_token_seconds histogram\n",
        "llamacpp:time_to_first_token_seconds_bucket{le=\"0.1\",launch=\"L1\",model=\"m1\"} 1\n",
        "llamacpp:time_to_first_token_seconds_sum{launch=\"L1\",model=\"m1\"} 0.05\n",
        "llamacpp:time_to_first_token_seconds_count{launch=\"L1\",model=\"m1\"} 1\n",
        // A name the engine declared and sampled identically — every llama.cpp
        // counter — stays under its own name, `untyped` only where nothing said
        // otherwise.
        "# TYPE llamacpp:finished_total counter\n",
        "llamacpp:finished_total{launch=\"L1\",model=\"m1\"} 3\n",
      )
    );
    assert!(!text.contains("untyped"), "{text}");
  }

  #[test]
  fn backend_merge_replaces_a_label_it_already_sets() {
    // An engine publishing its own `model` label does not get the name twice, and
    // our value is the one that survives.
    let (text, series, skipped) =
      merged("llamacpp:x{model=\"stale\",launch=\"L9\"} 4\n", "L1", "m1");
    assert_eq!(
      text,
      "# TYPE llamacpp:x untyped\nllamacpp:x{launch=\"L1\",model=\"m1\"} 4\n"
    );
    assert_eq!((series, skipped), (1, 0));
  }

  #[test]
  fn backend_merge_counts_only_the_samples_that_survive() {
    // A `NaN` line parses, so it is not a skipped line, but no sample lands, so
    // the count a scraper divides by must not claim it did.
    let (text, series, skipped) = merged("llamacpp:x nan\nllamacpp:y 1\n", "L1", "m1");
    assert_eq!((series, skipped), (1, 0));
    assert!(!text.contains("llamacpp:x"), "{text}");
    assert!(
      text.contains("llamacpp:y{launch=\"L1\",model=\"m1\"} 1"),
      "{text}"
    );
  }

  #[test]
  fn backend_merge_keeps_the_first_help_and_type_per_name() {
    let first = merged("# HELP n:x first\n# TYPE n:x counter\nn:x 1\n", "L1", "m1").0;
    let second = merged("# HELP n:x second\n# TYPE n:x gauge\nn:x 2\n", "L2", "m2").0;
    assert!(first.contains("# HELP n:x first"));
    assert!(first.contains("# TYPE n:x counter"));
    // Each target renders on its own here; a shared document keeps the first
    // HELP/TYPE it saw, which is what the first scrape of two gives.
    assert!(second.contains("# HELP n:x second"));
    assert!(second.contains("# TYPE n:x gauge"));
  }

  #[test]
  fn backend_json_lists_one_entry_per_target() {
    let scrape = Scrape {
      launch: "L1".into(),
      model: "m1".into(),
      url: "http://127.0.0.1:1/metrics".into(),
      status: Some(501),
      error: None,
      text: "{\"error\":\"not_supported_error\"}".into(),
      truncated: false,
      series: 0,
      skipped: 0,
      duration_ms: 4,
    };
    let parsed = Parsed {
      format: Format::Json,
      scope: Default::default(),
      limit: 10,
    };
    let body = json_body(&[scrape], &parsed);
    assert_eq!(body["backends"][0]["status"], 501);
    assert_eq!(body["backends"][0]["ok"], false);
    assert_eq!(body["backends"][0]["launch"], "L1");
    assert_eq!(body["scope"]["model"], Value::Null);
  }
}
