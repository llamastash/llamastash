//! Keep a launch's prompt cache across an eviction.
//!
//! llama-server can write a slot's KV cache to a file and read it back
//! (`--slot-save-path` plus `POST /slots/{id}?action=save|restore`). The daemon
//! saves every non-empty slot right before the idle sweep or make-room stops a
//! launch, and restores them when the same model file is Ready again, so the
//! returning conversation does not reprocess its whole prompt. Measured numbers
//! are in `docs/spikes/2026-10-06-slot-save-restore.md`.
//!
//! Files live in `<cache_dir>/slot-cache/<model key>/slot-<id>.bin`. A restore
//! consumes them, so one model holds at most one save, and the total across
//! models is capped at `backend.llamacpp.slot_cache.max_gb`, oldest first.
//!
//! Matching a restored slot to the returning conversation is llama-server's own
//! job: each file goes back into the slot id it came from, and the server picks
//! the slot whose cached tokens share the longest prefix with the request.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

/// `launch_config` key carrying the directory the launch saves its slots to.
/// Present only when the feature applies to the launch.
pub(super) const KNOB_SAVE_PATH: &str = "slot_save_path";
/// `launch_config` key carrying the total size cap in bytes.
const KNOB_MAX_BYTES: &str = "slot_cache_max_bytes";

const SAVE_FLAG: &str = "--slot-save-path";
/// One slot's save or restore. A save that runs out of time is dropped and the
/// stop goes ahead; a restore that does leaves the launch to start cold.
const ACTION_TIMEOUT: Duration = Duration::from_secs(120);
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// `backend.llamacpp.slot_cache` in `config.yaml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct SlotCacheConfig {
  /// Save slots before an eviction and restore them on the next load. Factory
  /// `false`: a save file is the whole KV cache, which runs to tens of GB for a
  /// large model at a long context.
  pub enabled: bool,
  /// Cap on the total size of all save files, in GiB. `0` means no cap.
  pub max_gb: u32,
}

impl Default for SlotCacheConfig {
  fn default() -> Self {
    Self {
      enabled: false,
      max_gb: 20,
    }
  }
}

fn root() -> Option<PathBuf> {
  crate::util::paths::cache_dir().map(|d| d.join("slot-cache"))
}

/// The save directory for one model file. The key covers the file's size and
/// mtime as well as its path, so a different model written to the same path
/// never gets the old model's cache restored into it.
fn dir_for(root: &Path, model: &Path) -> PathBuf {
  let mut hasher = blake3::Hasher::new();
  hasher.update(model.as_os_str().as_encoded_bytes());
  if let Ok(meta) = std::fs::metadata(model) {
    hasher.update(&meta.len().to_le_bytes());
    let mtime = meta
      .modified()
      .ok()
      .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
      .map_or(0, |d| d.as_nanos());
    hasher.update(&mtime.to_le_bytes());
  }
  root.join(&hasher.finalize().to_hex()[..16])
}

/// Project the config onto this launch: set the save directory when the
/// feature applies, clear it otherwise. Skipped for embedding and rerank
/// launches (no conversation to keep), under the bench parity switch, and when
/// the user already passes their own `--slot-save-path`.
pub(super) fn seed(cfg: &SlotCacheConfig, params: &mut LaunchParams) {
  params.launch_config.remove(KNOB_SAVE_PATH);
  params.launch_config.remove(KNOB_MAX_BYTES);
  let applies = cfg.enabled
    && params.mode == LaunchMode::Chat
    && !crate::launch::params::bench_disable_defaults_from_env()
    && !params.extras.iter().any(|e| {
      let e = e.to_string_lossy();
      e == SAVE_FLAG || e.starts_with("--slot-save-path=")
    });
  if !applies {
    return;
  }
  let Some(root) = root() else {
    return;
  };
  let dir = dir_for(&root, &params.model_path);
  // llama-server refuses to start when the directory is missing.
  if let Err(e) = std::fs::create_dir_all(&dir) {
    log::warn!("slot cache: cannot create {}: {e}", dir.display());
    return;
  }
  params.launch_config.insert(
    KNOB_SAVE_PATH.to_string(),
    dir.to_string_lossy().into_owned(),
  );
  let max_bytes = u64::from(cfg.max_gb) << 30;
  params
    .launch_config
    .insert(KNOB_MAX_BYTES.to_string(), max_bytes.to_string());
}

fn save_dir(params: &LaunchParams) -> Option<PathBuf> {
  params.launch_config.get(KNOB_SAVE_PATH).map(PathBuf::from)
}

fn file_name(id: u32) -> String {
  format!("slot-{id}.bin")
}

fn slot_id(name: &str) -> Option<u32> {
  name
    .strip_prefix("slot-")?
    .strip_suffix(".bin")?
    .parse()
    .ok()
}

/// The `slot-<id>.bin` files in `dir`, as `(id, path)`.
fn slot_files(dir: &Path) -> Vec<(u32, PathBuf)> {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return Vec::new();
  };
  let mut files: Vec<(u32, PathBuf)> = entries
    .flatten()
    .filter_map(|e| Some((slot_id(e.file_name().to_str()?)?, e.path())))
    .collect();
  files.sort();
  files
}

/// Save every non-empty slot of the launch on `port`. Best-effort: a failure
/// leaves no file behind and never blocks the stop that follows.
pub(super) async fn save(params: &LaunchParams, port: u16) {
  let Some(dir) = save_dir(params) else {
    return;
  };
  // A file left by an earlier save belongs to a prompt this process never saw.
  for (_, stale) in slot_files(&dir) {
    let _ = std::fs::remove_file(stale);
  }
  let _ = std::fs::create_dir_all(&dir);
  // The ids come from the server's own list: it wraps an out-of-range id back
  // onto a real slot instead of refusing it, so counting up until an error
  // would save the same slot many times.
  let ids = match request(port, "GET", "/slots", None, LIST_TIMEOUT).await {
    Ok((200, body)) => slots_with_a_prompt(&body),
    Ok((status, _)) => {
      log::info!("slot cache: port {port} does not list its slots (HTTP {status}), nothing saved");
      return;
    }
    Err(e) => {
      log::warn!("slot cache: listing slots on port {port} failed: {e}");
      return;
    }
  };
  for id in ids {
    let name = file_name(id);
    let path = dir.join(&name);
    match post(port, id, "save", &name).await {
      Ok((200, body)) => log::info!(
        "slot cache: saved slot {id} on port {port} ({} tokens, {} bytes)",
        body.get("n_saved").and_then(|v| v.as_u64()).unwrap_or(0),
        body.get("n_written").and_then(|v| v.as_u64()).unwrap_or(0),
      ),
      Ok((status, body)) => {
        log::info!("slot cache: slot {id} on port {port} not saved (HTTP {status}): {body}");
        let _ = std::fs::remove_file(&path);
      }
      Err(e) => {
        log::warn!("slot cache: saving slot {id} on port {port} failed: {e}");
        let _ = std::fs::remove_file(&path);
        break;
      }
    }
  }
  let max_bytes = params
    .launch_config
    .get(KNOB_MAX_BYTES)
    .and_then(|s| s.parse::<u64>().ok())
    .unwrap_or(0);
  if let Some(root) = dir.parent() {
    prune(root, max_bytes);
  }
}

/// Restore the files saved for this launch's model, then delete them. A file
/// the server refuses (a smaller context, fewer slots) is deleted the same way.
pub(super) async fn restore(params: &LaunchParams, port: u16) {
  let Some(dir) = save_dir(params) else {
    return;
  };
  for (id, path) in slot_files(&dir) {
    match post(port, id, "restore", &file_name(id)).await {
      Ok((200, body)) => log::info!(
        "slot cache: restored slot {id} on port {port} ({} tokens)",
        body.get("n_restored").and_then(|v| v.as_u64()).unwrap_or(0),
      ),
      Ok((status, body)) => {
        log::info!("slot cache: slot {id} on port {port} not restored (HTTP {status}): {body}");
      }
      Err(e) => log::warn!("slot cache: restoring slot {id} on port {port} failed: {e}"),
    }
    let _ = std::fs::remove_file(path);
  }
}

/// Delete the oldest save files until the total under `root` fits `max_bytes`
/// (`0` = no cap), then drop the directories left empty.
fn prune(root: &Path, max_bytes: u64) {
  let Ok(dirs) = std::fs::read_dir(root) else {
    return;
  };
  let dirs: Vec<PathBuf> = dirs.flatten().map(|e| e.path()).collect();
  let mut files: Vec<(SystemTime, u64, PathBuf)> = dirs
    .iter()
    .flat_map(|d| slot_files(d))
    .filter_map(|(_, path)| {
      let meta = std::fs::metadata(&path).ok()?;
      Some((meta.modified().ok()?, meta.len(), path))
    })
    .collect();
  files.sort_by_key(|f| std::cmp::Reverse(f.0));
  let mut total = 0u64;
  for (_, len, path) in files {
    total = total.saturating_add(len);
    if max_bytes != 0 && total > max_bytes {
      log::info!(
        "slot cache: over the {max_bytes} byte cap, deleting {}",
        path.display()
      );
      let _ = std::fs::remove_file(path);
    }
  }
  for dir in dirs {
    // Fails on a directory that still holds a file, which is the check.
    let _ = std::fs::remove_dir(dir);
  }
}

/// Ids of the idle slots that hold a prompt, read from a `GET /slots` reply.
fn slots_with_a_prompt(slots: &serde_json::Value) -> Vec<u32> {
  let has = |slot: &serde_json::Value, key: &str| slot.get(key).and_then(|v| v.as_u64());
  slots
    .as_array()
    .into_iter()
    .flatten()
    .filter(|s| has(s, "n_prompt_tokens").is_some_and(|n| n > 0))
    .filter(|s| s.get("is_processing").and_then(|v| v.as_bool()) != Some(true))
    .filter_map(|s| u32::try_from(has(s, "id")?).ok())
    .collect()
}

/// `POST /slots/{id}?action=<action>` with the filename body.
async fn post(
  port: u16,
  id: u32,
  action: &str,
  filename: &str,
) -> std::io::Result<(u16, serde_json::Value)> {
  let body = serde_json::json!({ "filename": filename }).to_string();
  let path = format!("/slots/{id}?action={action}");
  request(port, "POST", &path, Some(&body), ACTION_TIMEOUT).await
}

/// One HTTP exchange with the child on loopback, returning the status and the
/// parsed JSON reply. Raw TCP with `Connection: close`, like the `/props` fetch.
async fn request(
  port: u16,
  method: &str,
  path: &str,
  body: Option<&str>,
  timeout: Duration,
) -> std::io::Result<(u16, serde_json::Value)> {
  let body = body.unwrap_or_default();
  let request = format!(
    "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
     Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
    body.len()
  );
  let fut = async {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await?;
    sock.write_all(request.as_bytes()).await?;
    let mut buf = Vec::with_capacity(1024);
    // The cap only guards a misbehaving peer; a slot list is a few KB per slot.
    sock.take(4 << 20).read_to_end(&mut buf).await?;
    Ok::<_, std::io::Error>(buf)
  };
  let raw = tokio::time::timeout(timeout, fut)
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "slot request timeout"))??;
  parse_reply(&raw)
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed slot reply"))
}

fn parse_reply(raw: &[u8]) -> Option<(u16, serde_json::Value)> {
  let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
  let head = std::str::from_utf8(&raw[..split]).ok()?;
  let status = head.split_whitespace().nth(1)?.parse().ok()?;
  let body = serde_json::from_slice(&raw[split + 4..]).unwrap_or(serde_json::Value::Null);
  Some((status, body))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::unique_temp_dir;

  fn write(path: &Path, bytes: usize, age_secs: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    file.set_len(bytes as u64).unwrap();
    file
      .set_modified(SystemTime::now() - Duration::from_secs(age_secs))
      .unwrap();
  }

  #[test]
  fn the_model_key_changes_when_the_file_at_the_path_changes() {
    let tmp = unique_temp_dir("slot-cache", "key");
    let model = tmp.join("m.gguf");
    write(&model, 10, 100);
    let first = dir_for(&tmp, &model);
    assert_eq!(first, dir_for(&tmp, &model));
    write(&model, 11, 100);
    assert_ne!(first, dir_for(&tmp, &model));
    assert_ne!(first, dir_for(&tmp, &tmp.join("other.gguf")));
  }

  #[test]
  fn prune_deletes_oldest_first_and_drops_empty_dirs() {
    let root = unique_temp_dir("slot-cache", "prune");
    write(&root.join("a/slot-0.bin"), 600, 300);
    write(&root.join("b/slot-0.bin"), 600, 200);
    write(&root.join("b/slot-1.bin"), 600, 100);
    write(&root.join("b/notes.txt"), 600, 400);
    prune(&root, 1300);
    assert!(!root.join("a").exists(), "oldest file and its dir go");
    assert!(root.join("b/slot-0.bin").exists());
    assert!(root.join("b/slot-1.bin").exists());
    assert!(root.join("b/notes.txt").exists(), "only slot files count");
    prune(&root, 0);
    assert!(root.join("b/slot-0.bin").exists(), "0 means no cap");
  }

  #[test]
  fn slot_files_reads_only_slot_names() {
    let dir = unique_temp_dir("slot-cache", "names");
    write(&dir.join("slot-10.bin"), 1, 0);
    write(&dir.join("slot-2.bin"), 1, 0);
    write(&dir.join("slot-x.bin"), 1, 0);
    write(&dir.join("server.log"), 1, 0);
    let ids: Vec<u32> = slot_files(&dir).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, vec![2, 10]);
  }

  #[test]
  fn seed_sets_the_path_only_when_the_feature_applies() {
    let on = SlotCacheConfig {
      enabled: true,
      max_gb: 2,
    };
    let mut chat = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    chat
      .launch_config
      .insert(KNOB_SAVE_PATH.to_string(), "/stale".into());
    seed(&SlotCacheConfig::default(), &mut chat);
    assert!(
      save_dir(&chat).is_none(),
      "off by default, stale key cleared"
    );

    let mut embed = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Embedding);
    seed(&on, &mut embed);
    assert!(save_dir(&embed).is_none());

    let mut own = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    own.extras = vec![SAVE_FLAG.into(), "/mine".into()];
    seed(&on, &mut own);
    assert!(save_dir(&own).is_none(), "a hand-passed flag wins");
  }

  #[test]
  fn only_idle_slots_holding_a_prompt_are_saved() {
    let slots = serde_json::json!([
      {"id": 0, "is_processing": false},
      {"id": 1, "is_processing": false, "n_prompt_tokens": 0},
      {"id": 2, "is_processing": true, "n_prompt_tokens": 40},
      {"id": 3, "is_processing": false, "n_prompt_tokens": 9952},
    ]);
    assert_eq!(slots_with_a_prompt(&slots), vec![3]);
    assert!(slots_with_a_prompt(&serde_json::json!({"error": "x"})).is_empty());
  }

  #[test]
  fn parse_reply_reads_status_and_json() {
    let ok = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"n_saved\":7}";
    let (status, body) = parse_reply(ok).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body["n_saved"], 7);
    let (bad_status, bad_body) = parse_reply(b"HTTP/1.1 400 Bad Request\r\n\r\nnope").unwrap();
    assert_eq!(bad_status, 400);
    assert!(bad_body.is_null());
    assert!(parse_reply(b"garbage").is_none());
  }
}
