//! Thin OpenAI-compatible HTTP client used by the right-pane tabs.
//!
//! v1 calls land in `tabs::chat`, `tabs::embed`, and `tabs::rerank`;
//! all three hit a `llama-server` over loopback on the daemon's
//! recorded port. Isolated here so the v2 MCP layer can reuse the
//! same primitives.
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
/// keeps successive sends to the same loopback port cheap. The
/// timeout matches the previous per-call default.
fn shared_oai_client() -> &'static reqwest::Client {
  static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
  CLIENT.get_or_init(|| {
    reqwest::Client::builder()
      .timeout(Duration::from_secs(60))
      .build()
      .expect("reqwest client should build with default features")
  })
}

/// Outcome of one `/v1/chat/completions` stream chunk.
#[derive(Debug, Clone)]
pub enum ChatStreamMsg {
  /// One incremental delta the renderer should append.
  Delta(String),
  /// Stream finished cleanly. `finish_reason` may be `None` if the
  /// server stopped reporting one (older llama.cpp builds).
  Finished { finish_reason: Option<String> },
  /// Transport or protocol error — the stream is dead.
  Error(String),
}

/// Spawn a tokio task that streams an OpenAI-compatible chat
/// completion from `http://127.0.0.1:<port>/v1/chat/completions`
/// against the supplied prompt. Forwards each delta over the
/// returned channel; the renderer drains it without blocking.
pub fn spawn_chat_stream(
  port: u16,
  model: String,
  prompt: String,
) -> mpsc::Receiver<ChatStreamMsg> {
  let (tx, rx) = mpsc::channel::<ChatStreamMsg>(32);
  tokio::spawn(async move {
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
        let _ = tx.send(ChatStreamMsg::Error(format!("connect: {e}"))).await;
        return;
      }
    };
    if !resp.status().is_success() {
      let code = resp.status().as_u16();
      let err_body = resp.text().await.unwrap_or_default();
      let _ = tx
        .send(ChatStreamMsg::Error(format!("HTTP {code}: {err_body}")))
        .await;
      return;
    }
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut finish_reason: Option<String> = None;
    use futures::StreamExt;
    while let Some(next_chunk) = stream.next().await {
      let bytes = match next_chunk {
        Ok(b) => b,
        Err(e) => {
          let _ = tx.send(ChatStreamMsg::Error(format!("stream: {e}"))).await;
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
            let _ = tx
              .send(ChatStreamMsg::Finished {
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
            if let Some(content) = choice.delta.content {
              if !content.is_empty() {
                let _ = tx.send(ChatStreamMsg::Delta(content)).await;
              }
            }
          }
        }
      }
    }
    let _ = tx.send(ChatStreamMsg::Finished { finish_reason }).await;
  });
  rx
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

/// Collapse `<think>...</think>` blocks in `text`, replacing each
/// matched block with `⏵ reasoning (N tokens)`. Approximates token
/// count via whitespace splitting — good enough for a TUI badge,
/// not a billing claim.
pub fn collapse_think_blocks(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(open_idx) = rest.find("<think>") {
    out.push_str(&rest[..open_idx]);
    let after_open = &rest[open_idx + "<think>".len()..];
    match after_open.find("</think>") {
      Some(close_idx) => {
        let block = &after_open[..close_idx];
        let toks = block.split_whitespace().count();
        out.push_str(&format!("⏵ reasoning ({toks} tokens)"));
        rest = &after_open[close_idx + "</think>".len()..];
      }
      None => {
        // Unterminated — fall back to pass-through so we don't
        // swallow content while the stream is in flight.
        out.push_str("<think>");
        out.push_str(after_open);
        return out;
      }
    }
  }
  out.push_str(rest);
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn collapse_think_blocks_replaces_complete_block() {
    let s = "hello <think>let me reason about this</think> world";
    let out = collapse_think_blocks(s);
    assert_eq!(out, "hello ⏵ reasoning (5 tokens) world");
  }

  #[test]
  fn collapse_think_blocks_leaves_open_block_alone() {
    let s = "hello <think>in progress";
    assert_eq!(collapse_think_blocks(s), s);
  }

  #[test]
  fn collapse_think_blocks_handles_multiple_blocks() {
    let s = "<think>one two</think>X<think>three</think>Y";
    let out = collapse_think_blocks(s);
    assert_eq!(out, "⏵ reasoning (2 tokens)X⏵ reasoning (1 tokens)Y");
  }

  #[test]
  fn collapse_think_blocks_passthrough_when_no_block() {
    assert_eq!(collapse_think_blocks("plain text"), "plain text");
  }
}
