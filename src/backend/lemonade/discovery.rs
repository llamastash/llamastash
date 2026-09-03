//! Lemonade discovery source — **list-only**.
//!
//! Reads the model list from a running `lemond` umbrella's `/api/v1/models`
//! and projects each entry into a Lemonade-tagged [`DiscoveredModel`].
//!
//! **Acquisition is Lemonade's job** (see the plan's Scope Boundaries): this
//! source only *lists* what `lemond` already knows — it never downloads or
//! pulls. Users acquire models via `lemonade pull <model>` or the Lemonade
//! web UI. Best-effort: a transport error (umbrella not up) yields no rows
//! and never aborts the surrounding scan, so a disabled/absent Lemonade
//! backend degrades cleanly to "no Lemonade rows".

use std::path::PathBuf;

use crate::backend::lemonade::{LemonadeClient, ModelEntry};
use crate::discovery::{DiscoveredModel, ModelSource};
use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

/// Synthetic path for a Lemonade-registry model (no local file). Keeps the
/// catalog's path-keyed map unique per model; the user-facing name lives in
/// `display_label` and is what resolution / routing key on. The scheme is the
/// shared [`crate::backend::lemonade::LEMONADE_PATH_SCHEME`] so the launch path
/// can parse the name back off it.
fn synthetic_path(name: &str) -> PathBuf {
  PathBuf::from(format!(
    "{}{name}",
    crate::backend::lemonade::LEMONADE_PATH_SCHEME
  ))
}

/// Mode hint from `lemond`'s capability labels. Registry rows have no
/// GGUF header to read, but the labels say what the model *is*:
/// `embedding` / `reranking` map onto llamastash's modes;
/// `transcription` / `tts` have no llamastash surface yet, so they stay
/// `Unknown` (the TUI then offers no chat tab for a Whisper model);
/// everything else is an LLM → chat.
fn mode_hint_from_labels(labels: &[String]) -> ModeHint {
  let has = |want: &str| labels.iter().any(|l| l.eq_ignore_ascii_case(want));
  if has("embedding") {
    ModeHint::Embedding
  } else if has("reranking") || has("rerank") {
    ModeHint::Rerank
  } else if has("transcription") || has("tts") {
    ModeHint::Unknown
  } else {
    ModeHint::Chat
  }
}

/// Project one Lemonade registry row into a catalog row. The metadata
/// block is synthesized from what `lemond` reports (labels → mode hint,
/// `size` GB → weights bytes); GGUF-header fields stay empty.
fn row_for(entry: &ModelEntry) -> DiscoveredModel {
  let name = entry.id.as_str();
  DiscoveredModel {
    path: synthetic_path(name),
    parent: PathBuf::from(crate::backend::lemonade::LEMONADE_PATH_SCHEME),
    source: ModelSource::Lemonade,
    metadata: Some(ModelMetadata {
      arch: None,
      total_parameters: None,
      parameter_label: None,
      // No GGUF header to read a quant tag from.
      quant: Quant::Unknown(0),
      quant_label: None,
      native_ctx: None,
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: false,
      mode_hint: mode_hint_from_labels(&entry.labels),
      weights_bytes: entry.size.map(|gb| (gb * 1e9) as u64),
      lazy_tensor_bytes: Vec::new(),
      mtp: None,
    }),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: Some(name.to_string()),
    // Lemonade serves registry models by name, not local GGUFs — there's no
    // companion projector to detect, so no multimodal signal.
    multimodal: None,
    // Registry-served, not a local GGUF — never ds4-routable, and it runs on
    // Lemonade and nowhere else, so scope the launch picker's server row to
    // Lemonade's own server(s) instead of the whole host catalog.
    supported_backends: vec![crate::backend::lemonade::LEMONADE_BACKEND_ID.to_string()],
    mtp_head: None,
  }
}

/// Enumerate the models a `lemond` umbrella on `port` reports. Best-effort:
/// returns an empty vec (never errors) when the umbrella is unreachable.
pub async fn enumerate(port: u16) -> Vec<DiscoveredModel> {
  let client = match LemonadeClient::new(port) {
    Ok(c) => c,
    Err(e) => {
      log::debug!("lemonade discovery: client build failed: {e}");
      return Vec::new();
    }
  };
  match client.list_model_entries().await {
    Ok(entries) => entries.iter().map(row_for).collect(),
    Err(e) => {
      log::debug!("lemonade discovery: list_models failed (umbrella down?): {e}");
      Vec::new()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;

  /// Spawn a loopback fake serving `GET /api/v1/models` with the given
  /// OpenAI-list body; one connection per accept until dropped.
  async fn spawn_fake_models(body: &'static str) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
      loop {
        let Ok((mut sock, _)) = listener.accept().await else {
          break;
        };
        let mut buf = vec![0u8; 2048];
        let _ = sock.read(&mut buf).await;
        let resp = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
          body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
      }
    });
    port
  }

  #[tokio::test]
  async fn enumerate_projects_models_into_lemonade_rows() {
    let port = spawn_fake_models(
      r#"{"object":"list","data":[{"id":"Qwen2.5-0.5B-Instruct"},{"id":"Llama-3.1-8B"}]}"#,
    )
    .await;
    let rows = enumerate(port).await;
    let names: Vec<String> = rows
      .iter()
      .map(|r| r.display_label.clone().unwrap())
      .collect();
    assert_eq!(names, vec!["Qwen2.5-0.5B-Instruct", "Llama-3.1-8B"]);
    // Every row is tagged Lemonade with a synthetic (file-less) path.
    assert!(rows.iter().all(|r| r.source == ModelSource::Lemonade));
    assert_eq!(
      rows[0].path,
      PathBuf::from("lemonade://Qwen2.5-0.5B-Instruct")
    );
    // The backend tag derives from the source.
    assert_eq!(rows[0].source.backend_id(), "lemonade");
    // A registry model is Lemonade-only — the picker's server row must scope to
    // Lemonade, not fall back to the whole host catalog.
    assert_eq!(rows[0].supported_backends, vec!["lemonade".to_string()]);
    // No labels → an LLM → chat hint.
    assert_eq!(rows[0].metadata.as_ref().unwrap().mode_hint, ModeHint::Chat);
  }

  #[tokio::test]
  async fn enumerate_maps_labels_and_size_into_metadata() {
    // Mirrors a real lemond `/api/v1/models` row set: an LLM with
    // capability labels, a Whisper transcription model, an embedder.
    let port = spawn_fake_models(
      r#"{"object":"list","data":[
        {"id":"qwen3.5-4b-FLM","labels":["vision","reasoning","tool-calling"],"size":3.2},
        {"id":"Whisper-Tiny","labels":["transcription","realtime-transcription"]},
        {"id":"nomic-embed","labels":["embedding"]}
      ]}"#,
    )
    .await;
    let rows = enumerate(port).await;
    let hint = |i: usize| rows[i].metadata.as_ref().unwrap().mode_hint;
    assert_eq!(hint(0), ModeHint::Chat, "LLM labels → chat");
    assert_eq!(
      hint(1),
      ModeHint::Unknown,
      "transcription has no llamastash mode surface"
    );
    assert_eq!(hint(2), ModeHint::Embedding);
    assert_eq!(
      rows[0].metadata.as_ref().unwrap().weights_bytes,
      Some(3_200_000_000),
      "size GB → weights bytes for the SIZE column"
    );
  }

  #[tokio::test]
  async fn enumerate_returns_empty_when_umbrella_unreachable() {
    // Port 1 has nothing listening → transport error → no rows, no panic.
    let rows = enumerate(1).await;
    assert!(rows.is_empty());
  }
}
