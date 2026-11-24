//! Thin OpenAI-compatible HTTP client used by the right-pane tabs.
//!
//! `tabs::chat`, `tabs::embed`, and `tabs::rerank` all hit the engine
//! over loopback on the daemon's recorded port.
//!
//! The chat path streams SSE chunks via a `mpsc` channel rather
//! than holding the renderer's mutable App while the response is
//! still arriving — keeps the render loop's input-to-redraw budget
//! intact.

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Shared `reqwest::Client` for all right-pane tabs. `reqwest` builds
/// a TLS context and an HTTP connection pool the first time you
/// construct a `Client`; rebuilding per request (chat / embed /
/// rerank) drops the pool on every send. We don't talk to anything
/// over TLS, but the build cost is non-trivial and the pool is what
/// keeps successive sends to the same loopback port cheap.
///
/// Timeouts are tuned for streaming inference — `connect_timeout` and
/// `read_timeout` catch a wedged daemon, while `timeout` (total
/// wall-clock) is intentionally absent. A 27B reasoning model can
/// easily spend a minute thinking before the first content byte; a
/// 60-second total-request budget would abort the stream mid-flight
/// and surface as the opaque "error decoding response body" reqwest
/// returns for any body-stream failure.
fn shared_oai_client() -> &'static reqwest::Client {
  static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
  CLIENT.get_or_init(|| {
    reqwest::Client::builder()
      .connect_timeout(Duration::from_secs(10))
      .read_timeout(Duration::from_secs(120))
      .build()
      .expect("reqwest client should build with default features")
  })
}

/// Render a `reqwest::Error` plus the underlying source chain so chat
/// failures don't collapse to the bare "error decoding response body"
/// reqwest emits for every body-stream error.
fn format_chain(err: &dyn std::error::Error) -> String {
  let mut out = err.to_string();
  let mut src = err.source();
  while let Some(e) = src {
    out.push_str(": ");
    out.push_str(&e.to_string());
    src = e.source();
  }
  out
}

/// Outcome of one `/v1/chat/completions` stream chunk.
#[derive(Debug, Clone)]
pub enum ChatStreamMsg {
  /// One incremental delta the renderer should append.
  Delta(String),
  /// Stream finished cleanly. `finish_reason` may be `None` if the
  /// server stopped reporting one.
  Finished { finish_reason: Option<String> },
  /// Transport or protocol error — the stream is dead.
  Error(String),
}

/// RAII guard that guarantees the chat tab observes a terminal
/// frame (`Finished` or `Error`) even if the spawned task exits
/// unexpectedly — e.g. a panic inside the SSE parser, or a future
/// refactor adding an early `return` that forgets the explicit
/// terminal send. Without this, `App::chat.streaming` would stay
/// `true` forever and the chat tab would appear wedged.
///
/// On normal exit, every terminal-send site calls `mark_finished()`
/// before sending, so Drop is a no-op. On panic / early-return, Drop
/// fires and synthesises a clean `Finished { finish_reason: None }`
/// via `try_send` — synchronous because Drop has no async context.
struct ChatStreamTerminalGuard {
  tx: mpsc::Sender<crate::tui::events::Event>,
  finished: bool,
}

impl ChatStreamTerminalGuard {
  fn mark_finished(&mut self) {
    self.finished = true;
  }
}

impl Drop for ChatStreamTerminalGuard {
  fn drop(&mut self) {
    if !self.finished {
      let _ = self.tx.try_send(crate::tui::events::Event::ChatStream(
        ChatStreamMsg::Finished {
          finish_reason: None,
        },
      ));
    }
  }
}

/// Spawn a tokio task that streams an OpenAI-compatible chat
/// completion from `http://127.0.0.1:<port>/v1/chat/completions`
/// against the supplied prompt. Forwards each delta to the supplied
/// unified-event channel wrapped as [`crate::tui::events::Event::ChatStream`]
/// so the main loop wakes on every chunk and renders only when state
/// actually changes.
pub fn spawn_chat_stream(
  port: u16,
  model: String,
  prompt: String,
  events_tx: mpsc::Sender<crate::tui::events::Event>,
) {
  tokio::spawn(async move {
    let mut guard = ChatStreamTerminalGuard {
      tx: events_tx.clone(),
      finished: false,
    };
    let send = |msg: ChatStreamMsg| {
      let tx = events_tx.clone();
      async move { tx.send(crate::tui::events::Event::ChatStream(msg)).await }
    };
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let body = json!({
      "model": model,
      "stream": true,
      "messages": [{"role": "user", "content": prompt}],
    });
    let client = shared_oai_client();
    let resp = match client.post(&url).json(&body).send().await {
      Ok(r) => r,
      Err(e) => {
        guard.mark_finished();
        let _ = send(ChatStreamMsg::Error(format!(
          "connect: {}",
          format_chain(&e)
        )))
        .await;
        return;
      }
    };
    if !resp.status().is_success() {
      let code = resp.status().as_u16();
      let err_body = resp.text().await.unwrap_or_default();
      guard.mark_finished();
      let _ = send(ChatStreamMsg::Error(format!("HTTP {code}: {err_body}"))).await;
      return;
    }
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut finish_reason: Option<String> = None;
    // Tracks an open synthetic `<think>` block from a prior
    // `reasoning_content` delta. Newer llama.cpp builds split
    // chain-of-thought into its own delta field; we wrap those chunks
    // in `<think>...</think>` so the chat tab's `r` collapse
    // toggle works on them. Markers appear only at boundaries (first
    // reasoning chunk, first content chunk after reasoning) so
    // interleaved streams stay well-formed.
    let mut in_reasoning = false;
    use futures::StreamExt;
    while let Some(next_chunk) = stream.next().await {
      let bytes = match next_chunk {
        Ok(b) => b,
        Err(e) => {
          guard.mark_finished();
          let _ = send(ChatStreamMsg::Error(format!(
            "stream: {}",
            format_chain(&e)
          )))
          .await;
          return;
        }
      };
      buffer.push_str(&String::from_utf8_lossy(&bytes));
      while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        buffer.drain(..=idx + 1);
        for line in frame.lines() {
          let line = line.trim_start();
          let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
          };
          if payload == "[DONE]" {
            // Close any open reasoning block before reporting the
            // terminal frame. `return` makes the local flag dead, so
            // we don't bother resetting it.
            if in_reasoning {
              let _ = send(ChatStreamMsg::Delta("</think>".into())).await;
            }
            guard.mark_finished();
            let _ = send(ChatStreamMsg::Finished {
              finish_reason: finish_reason.clone(),
            })
            .await;
            return;
          }
          let parsed: Result<ChatChunk, _> = serde_json::from_str(payload);
          let decoded = match parsed {
            Ok(c) => c,
            Err(_) => continue, // tolerate keepalive/heartbeat lines
          };
          for choice in decoded.choices {
            if let Some(reason) = choice.finish_reason {
              finish_reason = Some(reason);
            }
            if let Some(reasoning) = choice.delta.reasoning_content {
              if !reasoning.is_empty() {
                let chunk = if in_reasoning {
                  reasoning
                } else {
                  in_reasoning = true;
                  format!("<think>{reasoning}")
                };
                let _ = send(ChatStreamMsg::Delta(chunk)).await;
              }
            }
            if let Some(content) = choice.delta.content {
              if !content.is_empty() {
                // `mem::take` reads the current flag and resets it to
                // `false` in one step — content closes the reasoning
                // channel whether or not we just transitioned out of
                // it. Splitting into `if in_reasoning { in_reasoning =
                // false; ... }` trips `clippy::unused_assignments`
                // because the next read lives in a different loop
                // iteration.
                let chunk = if std::mem::take(&mut in_reasoning) {
                  format!("</think>{content}")
                } else {
                  content
                };
                let _ = send(ChatStreamMsg::Delta(chunk)).await;
              }
            }
          }
        }
      }
    }
    if in_reasoning {
      let _ = send(ChatStreamMsg::Delta("</think>".into())).await;
    }
    guard.mark_finished();
    let _ = send(ChatStreamMsg::Finished { finish_reason }).await;
  });
}

#[derive(Deserialize)]
struct ChatChunk {
  #[serde(default)]
  choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
  #[serde(default)]
  delta: ChatDelta,
  #[serde(default)]
  finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChatDelta {
  #[serde(default)]
  content: Option<String>,
  /// Reasoning trace emitted by llama.cpp when `--reasoning-format`
  /// is set to a value other than `none` (default for DeepSeek-R1 /
  /// Qwen3 / GPT-OSS chat templates that surface a separate
  /// `<think>` channel). The wrapper task synthesises `<think>` /
  /// `</think>` markers around these chunks so the chat tab
  /// renderer handles the inline-tag and separate-field shapes
  /// through one code path.
  #[serde(default)]
  reasoning_content: Option<String>,
}

/// One-shot embeddings call. Returns the first vector's dimension
/// and the first eight values so the embed tab can render a thumb.
pub async fn embed(port: u16, model: &str, input: &str) -> Result<EmbedResult, String> {
  let url = format!("http://127.0.0.1:{port}/v1/embeddings");
  let request_body = json!({"model": model, "input": input});
  let resp = shared_oai_client()
    .post(&url)
    .json(&request_body)
    .send()
    .await
    .map_err(|e| format!("connect: {e}"))?;
  if !resp.status().is_success() {
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    return Err(format!("HTTP {code}: {text}"));
  }
  let response_body: Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
  let first = response_body
    .get("data")
    .and_then(Value::as_array)
    .and_then(|a| a.first())
    .ok_or_else(|| "empty data array".to_string())?;
  let vector = first
    .get("embedding")
    .and_then(Value::as_array)
    .ok_or_else(|| "missing embedding".to_string())?;
  let dim = vector.len();
  let preview: Vec<f64> = vector.iter().take(8).filter_map(Value::as_f64).collect();
  let norm = vector
    .iter()
    .filter_map(Value::as_f64)
    .map(|v| v * v)
    .sum::<f64>()
    .sqrt();
  Ok(EmbedResult { dim, preview, norm })
}

#[derive(Debug, Clone)]
pub struct EmbedResult {
  pub dim: usize,
  pub preview: Vec<f64>,
  pub norm: f64,
}

/// One-shot rerank call. Returns the ranked indices + scores.
pub async fn rerank(
  port: u16,
  model: &str,
  query: &str,
  candidates: &[String],
) -> Result<Vec<(usize, f64)>, String> {
  let url = format!("http://127.0.0.1:{port}/v1/rerank");
  let request_body = json!({"model": model, "query": query, "documents": candidates});
  let resp = shared_oai_client()
    .post(&url)
    .json(&request_body)
    .send()
    .await
    .map_err(|e| format!("connect: {e}"))?;
  if !resp.status().is_success() {
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    return Err(format!("HTTP {code}: {text}"));
  }
  let response_body: Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
  let arr = response_body
    .get("results")
    .and_then(Value::as_array)
    .ok_or_else(|| "missing results".to_string())?;
  let mut out: Vec<(usize, f64)> = arr
    .iter()
    .filter_map(|row| {
      let idx = row.get("index").and_then(Value::as_u64)? as usize;
      let score = row.get("relevance_score").and_then(Value::as_f64)?;
      Some((idx, score))
    })
    .collect();
  out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
  Ok(out)
}
