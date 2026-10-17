//! Reads token counts and generation speed out of a response as it
//! streams through the proxy.
//!
//! The proxy forwards response bytes untouched and does not buffer them,
//! so the tap keeps only the first `HEAD_CAP` and the last `TAIL_CAP`
//! bytes and looks for three objects by key once the response has ended:
//!
//! - `usage`: token counts (OpenAI `prompt_tokens` / `completion_tokens`,
//!   Anthropic and Responses `input_tokens` / `output_tokens`), and speed where
//!   the server reports it here: `completion_tokens_per_second`, or
//!   `decoding_speed_tps` with `prefill_speed_tps`.
//! - `timings`: the server's own counters (`prompt_n`, `prompt_ms`,
//!   `cache_n`, `predicted_n`) and `predicted_per_second`.
//! - `metrics`: `tokens_per_second`, on servers that attach per-request
//!   metrics.
//!
//! Matching is on field names, never on which backend answered. The head
//! is kept because an Anthropic stream reports its input tokens in the
//! first event (`message_start`) and only the output tokens at the end.

use serde_json::{Map, Value};

/// Bytes kept from the start of a response.
pub(crate) const HEAD_CAP: usize = 4 * 1024;
/// Bytes kept from the end of a response.
pub(crate) const TAIL_CAP: usize = 8 * 1024;
/// The fastest speed a row can carry, in tokens per second, reported or
/// estimated. Nothing near this runs today; above it a number is a broken body.
pub(crate) const SPEED_CEILING: f64 = 1_000_000.0;
/// The longest `prompt_ms` accepted: one day, in milliseconds.
const MAX_PROMPT_MS: f64 = 86_400_000.0;

/// What the tap read. A field the response did not carry is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
  /// Whole prompt, cached tokens included.
  pub prompt_tokens: Option<u64>,
  pub completion_tokens: Option<u64>,
  /// Generation speed: what the server reported, or the proxy's estimate
  /// when [`Self::tokens_per_second_estimated`] is set.
  pub tokens_per_second: Option<f64>,
  /// The speed was worked out by [`Self::estimate_speed`].
  pub tokens_per_second_estimated: bool,
  /// Prompt tokens the engine computed, the ones it reused from the KV cache
  /// left out.
  pub prefill_tokens: Option<u64>,
  /// How fast those were computed: what the server reported, or the proxy's
  /// estimate when [`Self::prefill_estimated`] is set.
  pub prefill_tokens_per_second: Option<f64>,
  /// The prefill speed was worked out by [`Self::estimate_prefill`].
  pub prefill_estimated: bool,
  /// Prompt tokens served from the KV cache.
  pub cached_tokens: Option<u64>,
}

impl Usage {
  /// When the server reported no speed, estimate it as the generated
  /// tokens over `window`, the time generation took by the proxy's clock.
  /// A server-reported speed is never replaced, and without a token count
  /// or a window wide enough to say something there is nothing to estimate.
  pub fn estimate_speed(&mut self, window: std::time::Duration) {
    match self.completion_tokens {
      Some(tokens) if self.tokens_per_second.is_none() => {
        if let Some(tps) = usable_speed(tokens as f64 / window.as_secs_f64()) {
          self.tokens_per_second = Some(tps);
          self.tokens_per_second_estimated = true;
        }
      }
      _ => {}
    }
  }

  /// When the server reported no prefill timing, estimate the speed the
  /// uncached part of the prompt was computed at, over `window`, the time the
  /// proxy saw the engine working on the prompt.
  ///
  /// This needs the cached count: without it the proxy cannot tell computed
  /// tokens from cache hits, and an agent prompt is mostly cache hits, so
  /// assuming none were would overstate the speed many times over.
  pub fn estimate_prefill(&mut self, window: std::time::Duration) {
    let (Some(prompt), Some(cached)) = (self.prompt_tokens, self.cached_tokens) else {
      return;
    };
    let Some(uncached) = prompt.checked_sub(cached).filter(|n| *n > 0) else {
      return;
    };
    if self.prefill_tokens_per_second.is_some() {
      return;
    }
    let Some(tps) = usable_speed(uncached as f64 / window.as_secs_f64()) else {
      return;
    };
    self.prefill_tokens = Some(uncached);
    self.prefill_tokens_per_second = Some(tps);
    self.prefill_estimated = true;
  }
}

#[derive(Default)]
pub(crate) struct ResponseTap {
  head: Vec<u8>,
  tail: Vec<u8>,
  /// Bytes between `head` and `tail` were discarded.
  gap: bool,
}

impl ResponseTap {
  pub(crate) fn push(&mut self, chunk: &[u8]) {
    let into_head = HEAD_CAP.saturating_sub(self.head.len()).min(chunk.len());
    self.head.extend_from_slice(&chunk[..into_head]);
    let rest = &chunk[into_head..];
    if rest.is_empty() {
      return;
    }
    // A chunk that alone fills the tail replaces it, so a multi-megabyte
    // body costs one `TAIL_CAP` copy per chunk, not a copy of every byte.
    if rest.len() >= TAIL_CAP {
      self.gap |= !self.tail.is_empty() || rest.len() > TAIL_CAP;
      self.tail.clear();
      self.tail.extend_from_slice(&rest[rest.len() - TAIL_CAP..]);
      return;
    }
    self.tail.extend_from_slice(rest);
    // Trim at twice the cap so the front is not shifted on every chunk.
    if self.tail.len() > 2 * TAIL_CAP {
      let cut = self.tail.len() - TAIL_CAP;
      self.tail.drain(..cut);
      self.gap = true;
    }
  }

  pub(crate) fn usage(&self) -> Usage {
    let mut fields = Fields::default();
    if self.gap {
      fields.scan(&self.head);
      fields.scan(&self.tail);
    } else {
      // Nothing was discarded, so the two halves are one contiguous body
      // and an object may straddle the join.
      fields.scan(&[self.head.as_slice(), self.tail.as_slice()].concat());
    }
    fields.usage()
  }
}

/// Every field read so far. A later object overrides an earlier one field
/// by field, so a stream's last `usage` wins without erasing what only an
/// earlier event carried.
#[derive(Default)]
struct Fields {
  prompt_tokens: Option<u64>,
  input_tokens: Option<u64>,
  cache_read_input_tokens: Option<u64>,
  cache_creation_input_tokens: Option<u64>,
  completion_tokens: Option<u64>,
  usage_tps: Option<f64>,
  usage_prefill_tps: Option<f64>,
  prompt_n: Option<u64>,
  prompt_ms: Option<f64>,
  cache_n: Option<u64>,
  cached_in_prompt_details: Option<u64>,
  cached_in_input_details: Option<u64>,
  predicted_n: Option<u64>,
  timings_tps: Option<f64>,
  metrics_tps: Option<f64>,
}

impl Fields {
  fn scan(&mut self, buf: &[u8]) {
    for obj in objects_keyed(buf, b"\"usage\"") {
      set(&mut self.prompt_tokens, count(&obj, "prompt_tokens"));
      set(&mut self.input_tokens, count(&obj, "input_tokens"));
      set(
        &mut self.cache_read_input_tokens,
        count(&obj, "cache_read_input_tokens"),
      );
      set(
        &mut self.cache_creation_input_tokens,
        count(&obj, "cache_creation_input_tokens"),
      );
      set(
        &mut self.completion_tokens,
        count(&obj, "completion_tokens").or_else(|| count(&obj, "output_tokens")),
      );
      set(
        &mut self.usage_tps,
        rate(&obj, "completion_tokens_per_second").or_else(|| rate(&obj, "decoding_speed_tps")),
      );
      set(&mut self.usage_prefill_tps, rate(&obj, "prefill_speed_tps"));
      // The cached count lives in a nested object on the OpenAI routes.
      set(
        &mut self.cached_in_prompt_details,
        nested_count(&obj, "prompt_tokens_details", "cached_tokens"),
      );
      set(
        &mut self.cached_in_input_details,
        nested_count(&obj, "input_tokens_details", "cached_tokens"),
      );
    }
    for obj in objects_keyed(buf, b"\"timings\"") {
      set(&mut self.prompt_n, count(&obj, "prompt_n"));
      set(&mut self.prompt_ms, duration_ms(&obj, "prompt_ms"));
      set(&mut self.cache_n, count(&obj, "cache_n"));
      set(&mut self.predicted_n, count(&obj, "predicted_n"));
      set(&mut self.timings_tps, rate(&obj, "predicted_per_second"));
    }
    for obj in objects_keyed(buf, b"\"metrics\"") {
      set(&mut self.metrics_tps, rate(&obj, "tokens_per_second"));
    }
  }

  fn usage(&self) -> Usage {
    // `prompt_n` is what the server evaluated and `cache_n` what it
    // reused, so their sum is the whole prompt on every route.
    let from_timings = match (self.prompt_n, self.cache_n) {
      (None, None) => None,
      (evaluated, cached) => Some(evaluated.unwrap_or(0).saturating_add(cached.unwrap_or(0))),
    };
    // `input_tokens` is the last resort, because servers disagree on it.
    // Anthropic's leaves the cached part out and reports it in the two
    // `cache_*` fields; the Responses API's counts the whole prompt and
    // has no such fields; adding them is right for both. But a server can
    // also count the cached tokens in `input_tokens` and again in
    // `cache_read_input_tokens`, and then the sum is double. Such a
    // server was seen to send `timings` too, which is why that wins.
    let from_input = self.input_tokens.map(|n| {
      n.saturating_add(self.cache_read_input_tokens.unwrap_or(0))
        .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
    });
    // The engine's own two inputs, not the `prompt_per_second` it also sends:
    // one formula covers the reported and the estimated case, and on the
    // bodies captured from llama-server it lands on exactly that field. A `ms`
    // small enough to overflow the quotient is no speed.
    let prefill_tps = match (self.prompt_n, self.prompt_ms) {
      (Some(tokens), Some(ms)) if tokens > 0 => usable_speed(tokens as f64 * 1000.0 / ms),
      _ => None,
    }
    .or(self.usage_prefill_tps);
    let cached = self
      .cache_n
      .or(self.cached_in_prompt_details)
      .or(self.cached_in_input_details)
      .or(self.cache_read_input_tokens);
    Usage {
      // OpenAI's `prompt_tokens` is the whole prompt by definition.
      prompt_tokens: self.prompt_tokens.or(from_timings).or(from_input),
      completion_tokens: self.completion_tokens.or(self.predicted_n),
      tokens_per_second: self.timings_tps.or(self.usage_tps).or(self.metrics_tps),
      tokens_per_second_estimated: false,
      prefill_tokens: self.prompt_n,
      prefill_tokens_per_second: prefill_tps,
      prefill_estimated: false,
      cached_tokens: cached,
    }
  }
}

fn set<T>(slot: &mut Option<T>, value: Option<T>) {
  if value.is_some() {
    *slot = value;
  }
}

fn count(obj: &Map<String, Value>, key: &str) -> Option<u64> {
  let v = obj.get(key)?;
  v.as_u64().or_else(|| {
    v.as_f64()
      .filter(|f| f.is_finite() && *f >= 0.0)
      .map(|f| f as u64)
  })
}

/// A count inside the object under `key`, e.g. the `cached_tokens` of
/// `usage.prompt_tokens_details`.
fn nested_count(obj: &Map<String, Value>, key: &str, field: &str) -> Option<u64> {
  count(obj.get(key)?.as_object()?, field)
}

fn rate(obj: &Map<String, Value>, key: &str) -> Option<f64> {
  obj.get(key)?.as_f64().and_then(usable_speed)
}

/// A `prompt_ms` a row may use. It is a duration, not a rate, so it gets its
/// own ceiling: one day of prompt processing. Anything longer is a body that
/// means something else, and a totals sum lives for the life of the daemon.
fn duration_ms(obj: &Map<String, Value>, key: &str) -> Option<f64> {
  obj
    .get(key)?
    .as_f64()
    .filter(|ms| ms.is_finite() && *ms > 0.0 && *ms <= MAX_PROMPT_MS)
}

/// Every speed a row can carry, from an engine or from the proxy's own clock,
/// goes through here. A speed past `SPEED_CEILING` is not a speed: it prints
/// hundreds of digits in a six-cell column, and `Totals` turns it into seconds
/// that swallow the scope's sum and the `/metrics` sample with it.
fn usable_speed(v: f64) -> Option<f64> {
  (v.is_finite() && v > 0.0 && v <= SPEED_CEILING).then_some(v)
}

/// Every JSON object in `buf` that is the value of `quoted_key` (the key
/// with its quotes), in order. An occurrence whose value is not a complete
/// object (`null`, or cut off by the edge of the buffer) is skipped.
///
/// A match can only be a real key: inside a JSON string the closing quote
/// of the needle would be escaped, and a string *value* equal to the key
/// is followed by `,` or `}`, not `:`.
fn objects_keyed(buf: &[u8], quoted_key: &[u8]) -> Vec<Map<String, Value>> {
  let mut found = Vec::new();
  let mut from = 0;
  while let Some(at) = find(&buf[from..], quoted_key) {
    from += at + quoted_key.len();
    let rest = skip_ws(&buf[from..]);
    let Some(rest) = rest.strip_prefix(b":") else {
      continue;
    };
    let rest = skip_ws(rest);
    if !rest.starts_with(b"{") {
      continue;
    }
    if let Some(Ok(obj)) = serde_json::Deserializer::from_slice(rest)
      .into_iter::<Map<String, Value>>()
      .next()
    {
      found.push(obj);
    }
  }
  found
}

fn skip_ws(buf: &[u8]) -> &[u8] {
  let n = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
  &buf[n..]
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether a piece of a streamed body holds a generated token.
///
/// A stream's first byte does not mean the prompt is done: an engine can open
/// with a status event (`message_start`, `response.created`, a chat chunk that
/// only sets the role) before it computes the prompt. The first token does.
/// The needles are keys and event types, and a quote inside a JSON string is
/// escaped, so token text cannot match one. A needle split across two pieces
/// matches on the next token instead.
pub(crate) fn carries_token(chunk: &[u8]) -> bool {
  // Anthropic text, thinking and tool input all arrive in this event, and
  // every Responses event that streams output has a type ending `.delta`.
  // `message_delta` ends `_delta"`, and it carries no token.
  const EVENTS: [&[u8]; 2] = [b"content_block_delta", b".delta\""];
  // Chat and text completion deltas. A role-only chunk has `"content":""`
  // or `null`, and a `response.created` echoes `"text":{"format":...}`.
  const STRINGS: [&[u8]; 4] = [
    b"\"content\":",
    b"\"reasoning_content\":",
    b"\"reasoning\":",
    b"\"text\":",
  ];
  EVENTS.iter().any(|n| find(chunk, n).is_some())
    || STRINGS
      .iter()
      .any(|key| some_value(chunk, key, |v| matches!(v, [b'"', c, ..] if *c != b'"')))
    || some_value(
      chunk,
      b"\"tool_calls\":",
      |v| matches!(v, [b'[', rest @ ..] if skip_ws(rest).starts_with(b"{")),
    )
}

/// Whether the value after some occurrence of `key` in `buf` passes `test`,
/// given the bytes from the value's first character on.
fn some_value(buf: &[u8], key: &[u8], test: impl Fn(&[u8]) -> bool) -> bool {
  let mut from = 0;
  while let Some(at) = find(&buf[from..], key) {
    from += at + key.len();
    if test(skip_ws(&buf[from..])) {
      return true;
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;

  fn usage_of(body: &str) -> Usage {
    let mut tap = ResponseTap::default();
    tap.push(body.as_bytes());
    tap.usage()
  }

  // The bodies below are responses captured from llama-server b11390
  // (2026-10-05), with the model path shortened.

  #[test]
  fn chat_non_streamed() {
    let body = r#"{"choices":[{"finish_reason":"stop","index":0,"message":{"role":"assistant","content":"Hi, How are you?"}}],"created":1791206934,"model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion","usage":{"completion_tokens":7,"prompt_tokens":41,"total_tokens":48,"prompt_tokens_details":{"cached_tokens":0}},"id":"chatcmpl-19uf","timings":{"cache_n":0,"prompt_n":41,"prompt_ms":149.353,"prompt_per_token_ms":3.6427560975609756,"prompt_per_second":274.5174184649789,"predicted_n":7,"predicted_ms":151.366,"predicted_per_token_ms":25.227666666666668,"predicted_per_second":39.639020651929755}}"#;
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(7),
        tokens_per_second: Some(39.639020651929755),
        // All 41 prompt tokens were computed, and `prompt_n` over `prompt_ms`
        // is exactly the `prompt_per_second` the body reports.
        prefill_tokens: Some(41),
        prefill_tokens_per_second: Some(274.5174184649789),
        cached_tokens: Some(0),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn a_prompt_timing_too_small_to_divide_by_is_no_speed() {
    // A `prompt_ms` this small overflows the quotient to infinity. A speed of
    // infinity prints, counts towards the scope's figure, and takes the
    // seconds counter with it when it is rendered.
    let body = r#"{"usage":{"prompt_tokens":41,"completion_tokens":3},"timings":{"cache_n":0,"prompt_n":41,"prompt_ms":1e-320,"predicted_n":3,"predicted_per_second":39.6}}"#;
    let usage = usage_of(body);
    assert_eq!(usage.prefill_tokens, Some(41));
    assert_eq!(usage.prefill_tokens_per_second, None);
  }

  #[test]
  fn a_prompt_timing_longer_than_a_day_is_no_timing() {
    // A `prompt_ms` this large is not a prompt. `Totals` divides the tokens by
    // the speed and keeps the seconds for the life of the daemon, so an
    // astronomic one moves `prefill_seconds_total` forever and drops the
    // `/metrics` sample once the sum is not finite.
    let body = r#"{"usage":{"prompt_tokens":41,"completion_tokens":3},"timings":{"cache_n":0,"prompt_n":41,"prompt_ms":86400001,"predicted_n":3,"predicted_per_second":39.6}}"#;
    let usage = usage_of(body);
    assert_eq!(usage.prefill_tokens_per_second, None);
    assert_eq!(usage.tokens_per_second, Some(39.6));
  }

  #[test]
  fn a_speed_past_the_ceiling_is_no_speed() {
    // Reported, not estimated: a body can send anything under
    // `predicted_per_second`, and one past the ceiling is not a speed.
    let body = r#"{"usage":{"prompt_tokens":41,"completion_tokens":3},"timings":{"cache_n":0,"prompt_n":41,"prompt_ms":0.00004,"predicted_n":3,"predicted_per_second":5e6}}"#;
    let usage = usage_of(body);
    assert_eq!(usage.tokens_per_second, None);
    // The prefill quotient clears the ceiling the same way: 41 tokens in
    // 0.04 microseconds.
    assert_eq!(usage.prefill_tokens_per_second, None);
  }

  #[test]
  fn an_estimate_past_the_ceiling_is_no_estimate() {
    // A window the clock can barely see would say a million tokens a second.
    let mut usage = Usage {
      completion_tokens: Some(10_000),
      ..Usage::default()
    };
    usage.estimate_speed(std::time::Duration::from_nanos(1));
    assert_eq!(usage.tokens_per_second, None);
    assert!(!usage.tokens_per_second_estimated);

    let mut prefill = Usage {
      prompt_tokens: Some(2_000_000),
      cached_tokens: Some(0),
      ..Usage::default()
    };
    prefill.estimate_prefill(std::time::Duration::from_millis(1));
    assert_eq!(prefill.prefill_tokens_per_second, None);
    assert!(!prefill.prefill_estimated);
    assert_eq!(prefill.prefill_tokens, None);
  }

  #[test]
  fn chat_streamed_has_timings_but_no_usage() {
    let body = concat!(
      r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"Hi"}}],"created":1791206934,"id":"chatcmpl-Vs5P","model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion.chunk"}"#,
      "\n\n",
      r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"created":1791206934,"id":"chatcmpl-Vs5P","model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion.chunk","timings":{"cache_n":40,"prompt_n":1,"prompt_ms":25.704,"prompt_per_token_ms":25.704,"prompt_per_second":38.90445066915655,"predicted_n":3,"predicted_ms":51.759,"predicted_per_token_ms":25.8795,"predicted_per_second":38.64062288684094}}"#,
      "\n\ndata: [DONE]\n\n",
    );
    // No `usage` object: the counts come from `timings`, and the prompt
    // is the evaluated token plus the 40 cached ones.
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(3),
        tokens_per_second: Some(38.64062288684094),
        prefill_tokens: Some(1),
        prefill_tokens_per_second: Some(38.90445066915655),
        cached_tokens: Some(40),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn chat_streamed_with_include_usage() {
    let body = concat!(
      r#"data: {"choices":[],"created":1791206935,"id":"chatcmpl-tTRS","model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion.chunk","usage":{"completion_tokens":7,"prompt_tokens":41,"total_tokens":48,"prompt_tokens_details":{"cached_tokens":40}},"timings":{"cache_n":40,"prompt_n":1,"prompt_ms":26.4,"prompt_per_token_ms":26.4,"prompt_per_second":37.87878787878788,"predicted_n":7,"predicted_ms":153.356,"predicted_per_token_ms":25.55933333333333,"predicted_per_second":39.12465113852735}}"#,
      "\n\ndata: [DONE]\n\n",
    );
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(7),
        tokens_per_second: Some(39.12465113852735),
        prefill_tokens: Some(1),
        prefill_tokens_per_second: Some(37.87878787878788),
        cached_tokens: Some(40),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn anthropic_messages_streamed() {
    let body = concat!(
      "event: message_start\n",
      r#"data: {"type":"message_start","message":{"id":"chatcmpl-x","type":"message","role":"assistant","content":[],"model":"m.gguf","stop_reason":null,"stop_sequence":null,"usage":{"cache_read_input_tokens":40,"input_tokens":1,"output_tokens":0}}}"#,
      "\n\nevent: content_block_delta\n",
      r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
      "\n\nevent: message_delta\n",
      r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#,
      "\n\nevent: message_stop\n",
      r#"data: {"type":"message_stop"}"#,
      "\n\n",
    );
    // Input tokens come from the first event, output tokens from the
    // last, and this surface carries no speed.
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(4),
        tokens_per_second: None,
        cached_tokens: Some(40),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn anthropic_messages_non_streamed() {
    let body = r#"{"id":"chatcmpl-11kF","type":"message","role":"assistant","content":[{"type":"text","text":"Hi, How are you?"}],"model":"m.gguf","stop_reason":"end_turn","stop_sequence":null,"usage":{"cache_read_input_tokens":40,"input_tokens":1,"output_tokens":7}}"#;
    assert_eq!(
      usage_of(body),
      // No `timings` on this surface, so no prefill speed: the proxy has to
      // estimate one from its own clock.
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(7),
        tokens_per_second: None,
        cached_tokens: Some(40),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn responses_streamed() {
    let body = concat!(
      "event: response.completed\n",
      r#"data: {"type":"response.completed","response":{"id":"resp_k7MN","object":"response","created_at":1791206936,"status":"completed","model":"m.gguf","output":[{"type":"message","status":"completed","id":"msg_p0CB","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello friend."}],"role":"assistant"}],"usage":{"input_tokens":41,"output_tokens":4,"total_tokens":45,"input_tokens_details":{"cached_tokens":40}}},"timings":{"cache_n":40,"prompt_n":1,"prompt_ms":22.405,"prompt_per_token_ms":22.405,"prompt_per_second":44.63289444320464,"predicted_n":4,"predicted_ms":75.864,"predicted_per_token_ms":25.288,"predicted_per_second":39.544447959506485}}"#,
      "\n\n",
    );
    // The Responses `input_tokens` already counts the cached tokens.
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(4),
        tokens_per_second: Some(39.544447959506485),
        prefill_tokens: Some(1),
        prefill_tokens_per_second: Some(44.63289444320464),
        cached_tokens: Some(40),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn status_events_carry_no_token() {
    // What engines send before they compute the prompt. Engines were seen
    // sending the first of these 2 to 48 ms after the request, long before
    // the prompt was done.
    let status = [
      concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":41,"output_tokens":0}}}"#,
        "\n\n",
      ),
      "event: ping\ndata: {\"type\":\"ping\"}\n\n",
      concat!(
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
      ),
      concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_1","object":"response","status":"in_progress","model":"m","output":[],"text":{"format":{"type":"text"}},"usage":null}}"#,
        "\n\n",
      ),
      concat!(
        "event: response.in_progress\n",
        r#"data: {"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_1","object":"response","status":"in_progress","output":[]}}"#,
        "\n\n",
      ),
      concat!(
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","status":"in_progress","role":"assistant","content":[]}}"#,
        "\n\n",
      ),
      concat!(
        "event: response.content_part.added\n",
        r#"data: {"type":"response.content_part.added","item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}"#,
        "\n\n",
      ),
      r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}],"object":"chat.completion.chunk"}"#,
      r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":null}}],"object":"chat.completion.chunk"}"#,
      r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[]}}],"object":"chat.completion.chunk"}"#,
      r#"data: {"choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}"#,
    ];
    for event in status {
      assert!(!carries_token(event.as_bytes()), "{event}");
    }
  }

  #[test]
  fn token_events_carry_a_token() {
    let tokens = [
      r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
      r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let"}}"#,
      r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
      r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hi"}"#,
      r#"data: {"type":"response.reasoning_text.delta","item_id":"rs_1","output_index":0,"content_index":0,"delta":"Let"}"#,
      r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\""}"#,
      r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"}}],"object":"chat.completion.chunk"}"#,
      r#"data: {"choices":[{"index":0,"delta":{"content":"\n"}}],"object":"chat.completion.chunk"}"#,
      r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"Let"}}],"object":"chat.completion.chunk"}"#,
      r#"data: {"choices":[{"index":0,"delta":{"reasoning":"Let"}}],"object":"chat.completion.chunk"}"#,
      r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get","arguments":""}}]}}]}"#,
      r#"data: {"choices":[{"text":"Hi","index":0,"finish_reason":null}],"object":"text_completion"}"#,
      r#"data: {"choices": [{"index": 0, "delta": {"content": "Hi"}}]}"#,
    ];
    for event in tokens {
      assert!(carries_token(event.as_bytes()), "{event}");
    }
    // A role-only chunk and the first token in one piece.
    let both = concat!(
      r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
      "\n\n",
      r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
      "\n\n",
    );
    assert!(carries_token(both.as_bytes()));
  }

  #[test]
  fn speed_is_estimated_only_when_the_server_reported_none() {
    use std::time::Duration;
    let mut counted = Usage {
      completion_tokens: Some(30),
      ..Usage::default()
    };
    counted.estimate_speed(Duration::from_millis(1500));
    assert_eq!(counted.tokens_per_second, Some(20.0));
    assert!(counted.tokens_per_second_estimated);

    let mut reported = Usage {
      completion_tokens: Some(30),
      tokens_per_second: Some(41.5),
      ..Usage::default()
    };
    reported.estimate_speed(Duration::from_millis(1500));
    assert_eq!(reported.tokens_per_second, Some(41.5));
    assert!(!reported.tokens_per_second_estimated);

    // No token count, no tokens, or no elapsed time: nothing to divide.
    for (tokens, window) in [
      (None, Duration::from_secs(1)),
      (Some(0), Duration::from_secs(1)),
      (Some(30), Duration::ZERO),
    ] {
      let mut usage = Usage {
        completion_tokens: tokens,
        ..Usage::default()
      };
      usage.estimate_speed(window);
      assert_eq!(usage.tokens_per_second, None, "{tokens:?} {window:?}");
      assert!(!usage.tokens_per_second_estimated);
    }
  }

  #[test]
  fn cached_tokens_falls_back_through_the_usage_fields() {
    // `timings.cache_n` first. Where a body carries no `timings`, the cached
    // count is in the field the route happens to use.
    let in_prompt_details = r#"{"usage":{"prompt_tokens":41,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40}}}"#;
    assert_eq!(usage_of(in_prompt_details).cached_tokens, Some(40));
    let in_input_details = r#"{"usage":{"input_tokens":41,"output_tokens":4,"input_tokens_details":{"cached_tokens":32}}}"#;
    assert_eq!(usage_of(in_input_details).cached_tokens, Some(32));
    let in_anthropic_field = r#"{"usage":{"input_tokens":1,"cache_read_input_tokens":40}}"#;
    assert_eq!(usage_of(in_anthropic_field).cached_tokens, Some(40));
    // A body that carries both detail objects reads the OpenAI one, and
    // either beats the bare Anthropic field.
    let both_details = r#"{"usage":{"prompt_tokens_details":{"cached_tokens":40},"input_tokens_details":{"cached_tokens":32},"cache_read_input_tokens":7}}"#;
    assert_eq!(usage_of(both_details).cached_tokens, Some(40));
    // And `timings` wins over all of them.
    let with_timings = r#"{"usage":{"prompt_tokens_details":{"cached_tokens":7}},"timings":{"prompt_n":34,"cache_n":40}}"#;
    assert_eq!(usage_of(with_timings).cached_tokens, Some(40));
  }

  #[test]
  fn prefill_is_estimated_only_from_a_cached_count_and_a_real_window() {
    use std::time::Duration;
    let mut usage = Usage {
      prompt_tokens: Some(1000),
      cached_tokens: Some(600),
      ..Usage::default()
    };
    usage.estimate_prefill(Duration::from_millis(500));
    assert_eq!(usage.prefill_tokens, Some(400), "only the uncached part");
    assert_eq!(usage.prefill_tokens_per_second, Some(800.0));
    assert!(usage.prefill_estimated);

    // An engine figure is never replaced.
    let mut reported = Usage {
      prompt_tokens: Some(1000),
      cached_tokens: Some(600),
      prefill_tokens: Some(300),
      prefill_tokens_per_second: Some(1234.5),
      ..Usage::default()
    };
    reported.estimate_prefill(Duration::from_millis(500));
    assert_eq!(reported.prefill_tokens_per_second, Some(1234.5));
    assert_eq!(reported.prefill_tokens, Some(300));
    assert!(!reported.prefill_estimated);

    // Without a cached count the proxy cannot tell what was computed, a whole
    // prompt of cache hits has nothing to time, and a zero window divides by
    // nothing.
    for (prompt, cached, window) in [
      (Some(1000), None, Duration::from_millis(500)),
      (None, Some(600), Duration::from_millis(500)),
      (Some(600), Some(600), Duration::from_millis(500)),
      (Some(1000), Some(600), Duration::ZERO),
    ] {
      let mut guess = Usage {
        prompt_tokens: prompt,
        cached_tokens: cached,
        ..Usage::default()
      };
      guess.estimate_prefill(window);
      assert_eq!(
        guess.prefill_tokens_per_second, None,
        "{prompt:?} {cached:?} {window:?}"
      );
      assert!(!guess.prefill_estimated);
    }
  }

  // The next two bodies are from a second engine (its own `timings`
  // object, llama-server's field names), captured 2026-10-06 and cut down.

  #[test]
  fn timings_win_over_an_input_tokens_that_already_counts_the_cache() {
    // A 58-token prompt, all of it cached. This server reports 58 as
    // `input_tokens` and 58 again as `cache_read_input_tokens`.
    let body = r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"Hi"}],"usage":{"input_tokens":58,"output_tokens":8,"cache_creation_input_tokens":0,"cache_read_input_tokens":58},"timings":{"prompt_n":0,"prompt_ms":0,"prompt_per_second":0,"predicted_n":8,"predicted_ms":153.92376399999998,"predicted_per_second":51.97378099459679,"cache_n":58,"draft_n":6,"draft_n_accepted":6}}"#;
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(58),
        completion_tokens: Some(8),
        tokens_per_second: Some(51.97378099459679),
        // Nothing had to be computed, so there is no prefill speed to read.
        prefill_tokens: Some(0),
        cached_tokens: Some(58),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn a_stream_whose_last_chunk_has_only_timings_reads_a_fully_cached_prompt() {
    let body = concat!(
      r#"data: {"id":"chatcmpl-19b6","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":{"reasoning_content":" ""},"finish_reason":null}]}"#,
      "

",
      r#"data: {"id":"chatcmpl-19b6","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"length"}],"timings":{"prompt_n":0,"prompt_ms":0,"prompt_per_token_ms":0,"prompt_per_second":0,"predicted_n":8,"predicted_ms":142.473023,"predicted_per_token_ms":17.809127875,"predicted_per_second":56.15098094745978,"cache_n":58,"cache_restore_ms":3.082299,"draft_rounds":1,"draft_n":6,"draft_n_accepted":6}}"#,
      "

data: [DONE]

",
    );
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(58),
        completion_tokens: Some(8),
        tokens_per_second: Some(56.15098094745978),
        prefill_tokens: Some(0),
        cached_tokens: Some(58),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn speed_is_read_from_usage_and_from_metrics_when_timings_is_absent() {
    let in_usage = r#"{"usage":{"prompt_tokens":10,"completion_tokens":20,"completion_tokens_per_second":55.5}}"#;
    assert_eq!(usage_of(in_usage).tokens_per_second, Some(55.5));
    let in_metrics = r#"{"usage":{"prompt_tokens":10,"completion_tokens":20},"metrics":{"time_to_first_token_ms":12.0,"tokens_per_second":61.25}}"#;
    assert_eq!(usage_of(in_metrics).tokens_per_second, Some(61.25));
  }

  #[test]
  fn speed_reported_under_usage_is_read_from_both_spellings() {
    // A body captured from FastFlowLM on an AMD NPU: no `timings`, no
    // `metrics`, and its own names for both speeds.
    let body = r#"{"model":"qwen3:0.6b","usage":{"prompt_tokens":14,
      "prompt_tokens_details":{"cached_tokens":0},"completion_tokens":12,
      "prefill_speed_tps":27.665165623284167,"decoding_speed_tps":97.59191938907458}}"#;
    let u = usage_of(body);
    assert_eq!(u.prompt_tokens, Some(14));
    assert_eq!(u.completion_tokens, Some(12));
    assert_eq!(u.cached_tokens, Some(0));
    assert_eq!(u.tokens_per_second, Some(97.59191938907458));
    assert!(!u.tokens_per_second_estimated);
    assert_eq!(u.prefill_tokens_per_second, Some(27.665165623284167));
    assert!(!u.prefill_estimated);
  }

  #[test]
  fn a_null_usage_on_earlier_chunks_is_skipped() {
    let body = concat!(
      r#"data: {"choices":[{"delta":{"content":"a"}}],"usage":null}"#,
      "\n\n",
      r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
      "\n\ndata: [DONE]\n\n",
    );
    let u = usage_of(body);
    assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(5), Some(2)));
  }

  #[test]
  fn usage_text_inside_the_reply_is_not_read() {
    // The model wrote a `usage` object in its answer. In the JSON body
    // its quotes are escaped, so it is not a key.
    let body = r#"{"choices":[{"message":{"content":"{\"usage\":{\"prompt_tokens\":999,\"completion_tokens\":999}}"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
    let u = usage_of(body);
    assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(5), Some(2)));
    let only_text =
      r#"{"choices":[{"message":{"content":"{\"usage\":{\"prompt_tokens\":999}}"}}]}"#;
    assert_eq!(usage_of(only_text), Usage::default());
  }

  #[test]
  fn a_body_without_the_fields_reads_as_nothing() {
    assert_eq!(usage_of(""), Usage::default());
    assert_eq!(
      usage_of(r#"{"error":{"message":"boom"}}"#),
      Usage::default()
    );
    assert_eq!(
      usage_of(r#"{"usage":"none","timings":[1,2]}"#),
      Usage::default()
    );
    assert_eq!(usage_of(r#"{"usage":{"prompt_tokens":"#), Usage::default());
  }

  #[test]
  fn fields_survive_any_chunking() {
    let body = concat!(
      "event: message_start\n",
      r#"data: {"type":"message_start","message":{"usage":{"cache_read_input_tokens":40,"input_tokens":1,"output_tokens":0}}}"#,
      "\n\n",
      r#"data: {"type":"message_delta","usage":{"output_tokens":4}}"#,
      "\n\n",
    );
    for size in [1, 2, 7, 64] {
      let mut tap = ResponseTap::default();
      for chunk in body.as_bytes().chunks(size) {
        tap.push(chunk);
      }
      let u = tap.usage();
      assert_eq!(
        (u.prompt_tokens, u.completion_tokens),
        (Some(41), Some(4)),
        "chunk size {size}"
      );
    }
  }

  #[test]
  fn a_long_stream_keeps_the_first_event_and_the_last() {
    let mut tap = ResponseTap::default();
    tap.push(
      br#"data: {"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}"#,
    );
    let delta = br#"data: {"type":"content_block_delta","delta":{"text":"word "}}"#;
    for _ in 0..2000 {
      tap.push(delta);
      tap.push(b"\n\n");
    }
    tap.push(br#"data: {"type":"message_delta","usage":{"output_tokens":2000}}"#);
    assert!(tap.gap);
    assert!(tap.head.len() <= HEAD_CAP);
    assert!(tap.tail.len() <= 2 * TAIL_CAP);
    let u = tap.usage();
    assert_eq!(
      (u.prompt_tokens, u.completion_tokens),
      (Some(12), Some(2000))
    );
  }

  #[test]
  fn a_large_single_chunk_keeps_only_its_tail() {
    let mut body = vec![b' '; 3 * 1024 * 1024];
    body.extend_from_slice(br#"{"usage":{"prompt_tokens":9,"completion_tokens":1}}"#);
    let mut tap = ResponseTap::default();
    tap.push(&body);
    assert_eq!(tap.head.len(), HEAD_CAP);
    assert_eq!(tap.tail.len(), TAIL_CAP);
    assert_eq!(tap.usage().prompt_tokens, Some(9));
  }

  #[test]
  fn an_object_across_the_head_and_tail_join_is_read() {
    let mut body = vec![b' '; HEAD_CAP - 10];
    body.extend_from_slice(br#"{"usage":{"prompt_tokens":9,"completion_tokens":1}}"#);
    let mut tap = ResponseTap::default();
    tap.push(&body);
    assert!(!tap.gap);
    assert_eq!(tap.usage().prompt_tokens, Some(9));
  }
}
