//! Keep a launch's prompt cache across an unload.
//!
//! `llama-server` writes one slot's KV cache to disk (`--slot-save-path` plus
//! `POST /slots/{id}?action=save`) and reads it back into a *fresh* process
//! (`action=restore`), so a launch that the idle sweep or `make_room` stops does
//! not make the returning conversation reprocess its prompt. Measured at 102k
//! tokens: 78.6 s of reprocessing against 0.31 s of restore.
//! `docs/spikes/2026-10-07-slot-kv-save-restore.md` holds the numbers and every
//! engine failure mode this module is written around.
//!
//! Two entry points, both reached only through the `Backend` hooks this backend
//! implements, so the eviction sweep and the supervisor stay backend-neutral. A
//! hook needs nothing but `launch_config` (seeded by `seed_launch_knobs` /
//! `seed_binary_caps`) plus the model file's own metadata — no `MethodContext`,
//! matching `fetch_actuals`.
//!
//! The engine matches a returning conversation to a slot itself (prompt-prefix
//! similarity over whatever the restored slots hold), so the launcher keys a
//! cache on the model, restores every slot it saved, and never looks at a prompt.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::launch::params::LaunchParams;

use super::{
  LLAMACPP_KNOB_SLOT_CACHE_DIR, LLAMACPP_KNOB_SLOT_CACHE_MAX_AGE_HOURS,
  LLAMACPP_KNOB_SLOT_CACHE_MAX_BYTES,
};

/// Below this a slot is not worth a file. At 32 KB/token (the spike's measured
/// rate for a small model) a 256-token prompt is ~8 MB and reprocesses far
/// quicker than a save round trip costs.
const MIN_SAVE_TOKENS: u64 = 256;
/// Whole-phase budgets, ~30x the measured worst case (2.5 s to write 3.3 GB,
/// 0.31 s to read it back). The save phase delays an eviction that is waiting to
/// free memory; the restore phase holds a launch below Ready. Neither may be at
/// the mercy of a wedged engine.
const SAVE_BUDGET: Duration = Duration::from_secs(90);
const RESTORE_BUDGET: Duration = Duration::from_secs(30);
/// One engine call. Listing is a local in-memory answer; save and restore move
/// gigabytes.
const LIST_TIMEOUT: Duration = Duration::from_secs(3);
const SAVE_CALL_TIMEOUT: Duration = Duration::from_secs(60);
const RESTORE_CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// A file that fails to restore this many times is deleted rather than retried on
/// every launch: a prompt that no longer fits the launch's context, or a file the
/// engine rejects, would otherwise cost a doomed restore each time.
const DROP_AFTER_FAILURES: u32 = 2;
/// Response cap for the hand-rolled transport. `/slots` is a few KB per slot.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Serializes read-modify-write of `manifest.json`: two launches of one model can
/// be evicted at the same time and the manifest is one file.
static MANIFEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The one directory this feature owns: `<cache dir>/slot-cache`. Nothing else
/// writes there and deleting it costs only reprocessed prompts, which is why it
/// lives under the cache dir and not the state dir.
pub(super) fn default_dir() -> Option<PathBuf> {
  crate::util::paths::cache_dir().map(|dir| dir.join("slot-cache"))
}

/// What was decided for one launch, read back off `launch_config`. Absent keys
/// mean the feature is off for this launch (config off, or a `llama-server`
/// build that does not advertise `--slot-save-path`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SlotCache {
  /// The directory the engine was given as `--slot-save-path`, and where our own
  /// files and manifest live.
  dir: PathBuf,
  max_bytes: u64,
  max_age: Duration,
}

impl SlotCache {
  fn from_params(params: &LaunchParams) -> Option<Self> {
    let dir = params
      .launch_config
      .get(LLAMACPP_KNOB_SLOT_CACHE_DIR)?
      .clone();
    let defaults = super::SlotCacheConfig::default();
    let max_bytes =
      parse_u64(params, LLAMACPP_KNOB_SLOT_CACHE_MAX_BYTES).unwrap_or(defaults.max_bytes);
    let max_age_hours =
      parse_u64(params, LLAMACPP_KNOB_SLOT_CACHE_MAX_AGE_HOURS).unwrap_or(defaults.max_age_hours);
    if dir.is_empty() {
      return None;
    }
    Some(Self {
      dir: PathBuf::from(dir),
      max_bytes,
      max_age: Duration::from_secs(max_age_hours.saturating_mul(3600)),
    })
  }
}

fn parse_u64(params: &LaunchParams, key: &str) -> Option<u64> {
  params
    .launch_config
    .get(key)
    .and_then(|raw| raw.trim().parse::<u64>().ok())
}

/// One saved slot. `fingerprint` is the cache key (see [`fingerprint`]); the rest
/// is what a restore needs plus what a human reading `manifest.json` wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SlotEntry {
  fingerprint: String,
  /// Not part of the key — provenance for the manifest and the logs.
  model: String,
  slot: u32,
  filename: String,
  tokens: u64,
  bytes: u64,
  /// Unix seconds the engine finished writing the file.
  saved_at: u64,
  /// Restore attempts that failed; see [`DROP_AFTER_FAILURES`].
  #[serde(default)]
  failures: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
  #[serde(default)]
  entries: Vec<SlotEntry>,
}

/// Save every non-idle slot of the launch on `port`, before it is stopped.
///
/// Called on the eviction path only. Best-effort by construction: every failure
/// is a log line, because the caller is about to free this launch's memory
/// whether or not the cache was written.
pub(super) async fn save_before_stop(port: u16, params: &LaunchParams) {
  let Some(cache) = SlotCache::from_params(params) else {
    return;
  };
  match tokio::time::timeout(SAVE_BUDGET, save_all(port, params, &cache)).await {
    Ok(()) => {}
    Err(_) => log::warn!(
      "slot cache: save gave up after {}s; the launch is stopped with whatever was written",
      SAVE_BUDGET.as_secs()
    ),
  }
}

/// Read back what [`save_before_stop`] wrote for this model, once the engine
/// answers its readiness probe.
pub(super) async fn restore_after_ready(port: u16, params: &LaunchParams) {
  let Some(cache) = SlotCache::from_params(params) else {
    return;
  };
  match tokio::time::timeout(
    RESTORE_BUDGET,
    restore_all(port, &cache, &params.model_path),
  )
  .await
  {
    Ok(()) => {}
    Err(_) => log::warn!(
      "slot cache: restore gave up after {}s; the launch goes Ready with an empty cache",
      RESTORE_BUDGET.as_secs()
    ),
  }
}

async fn save_all(port: u16, params: &LaunchParams, cache: &SlotCache) {
  let Some(fp) = fingerprint(&params.model_path) else {
    return;
  };
  let slots = match list_slots(port).await {
    Ok(slots) => slots,
    Err(err) => {
      log::debug!("slot cache: could not list slots on port {port}: {err}");
      return;
    }
  };
  let mut saved = 0usize;
  for (slot, tokens) in slots {
    if tokens < MIN_SAVE_TOKENS {
      continue;
    }
    // A file projected past the cap could never be kept, so the write itself is
    // what the cap has to prevent: the spike measured 32 KB/token on a 1B model,
    // which is ~10x that on a 70B at the same context.
    if let Some(projected) = projected_bytes(params, tokens) {
      if projected > cache.max_bytes {
        log::info!(
          "slot cache: skipped slot {slot} ({} prompt tokens, ~{} MB of KV) over the {} MB cap",
          tokens,
          projected / 1_000_000,
          cache.max_bytes / 1_000_000
        );
        continue;
      }
    }
    let filename = filename_for(&fp, slot);
    match slot_action(port, "save", slot, &filename, SAVE_CALL_TIMEOUT).await {
      Err(err) => {
        log::debug!("slot cache: save of slot {slot} failed: {err}");
      }
      Ok(reply) => {
        let saved_tokens = number(&reply, "n_saved");
        let bytes = number(&reply, "n_written");
        if saved_tokens == 0 {
          // An idle slot answers 200 with a header-only file. Not a cache.
          let _ = tokio::fs::remove_file(cache.dir.join(&filename)).await;
          continue;
        }
        let entry = SlotEntry {
          fingerprint: fp.clone(),
          model: params.model_path.display().to_string(),
          slot,
          filename,
          tokens,
          bytes,
          saved_at: now_secs(),
          failures: 0,
        };
        match record(cache, entry).await {
          Ok(()) => saved += 1,
          Err(err) => log::warn!("slot cache: could not update the manifest: {err}"),
        }
      }
    }
  }
  if saved > 0 {
    log::info!(
      "slot cache: wrote {saved} slot cache(s) to {}",
      cache.dir.display()
    );
  }
}

async fn restore_all(port: u16, cache: &SlotCache, model_path: &Path) {
  let Some(fp) = fingerprint(model_path) else {
    return;
  };
  let mut entries = load_manifest(&cache.dir).await;
  if !entries.iter().any(|e| e.fingerprint == fp) {
    return;
  }
  // Slot ids are per-process and the engine picks which slot a conversation
  // lands in, so a launch with fewer slots simply has nowhere to put some
  // entries. Their files stay: a later launch with room can still use them.
  let live: BTreeSet<u32> = match list_slots(port).await {
    Ok(slots) => slots.iter().map(|(id, _)| *id).collect(),
    Err(err) => {
      log::debug!("slot cache: could not list slots on port {port}: {err}");
      return;
    }
  };
  let mut restored = 0usize;
  for entry in entries.iter_mut().filter(|e| e.fingerprint == fp) {
    if !live.contains(&entry.slot) {
      log::debug!(
        "slot cache: slot {} is not in this launch; kept for a later one",
        entry.slot
      );
      continue;
    }
    if !tokio::fs::try_exists(cache.dir.join(&entry.filename))
      .await
      .unwrap_or(false)
    {
      entry.failures = DROP_AFTER_FAILURES;
      continue;
    }
    match slot_action(
      port,
      "restore",
      entry.slot,
      &entry.filename,
      RESTORE_CALL_TIMEOUT,
    )
    .await
    {
      Ok(reply) => {
        log::info!(
          "slot cache: restored {} prompt tokens into slot {} ({})",
          number(&reply, "n_restored"),
          entry.slot,
          entry.filename
        );
        restored += 1;
      }
      Err(err) => {
        // The engine's own answer is the same 400 for a prompt that no longer
        // fits, a foreign model, a truncated file and a changed KV cache type.
        // In every case the slot is left usable, so this only costs time.
        entry.failures += 1;
        log::info!(
          "slot cache: restore of slot {} from {} failed: {err}",
          entry.slot,
          entry.filename
        );
      }
    }
  }
  let doomed: Vec<SlotEntry> = entries
    .iter()
    .filter(|e| e.fingerprint == fp && e.failures >= DROP_AFTER_FAILURES)
    .cloned()
    .collect();
  if !doomed.is_empty() {
    for entry in &doomed {
      let _ = tokio::fs::remove_file(cache.dir.join(&entry.filename)).await;
    }
    entries.retain(|e| !doomed.contains(e));
    if let Err(err) = write_manifest(&cache.dir, &entries).await {
      log::warn!("slot cache: could not update the manifest: {err}");
    }
  }
  if restored > 0 {
    log::info!(
      "slot cache: {restored} cache(s) restored on port {port}; returning requests skip the prompt"
    );
  }
}

/// Insert or replace one entry, then apply the cap and the TTL.
async fn record(cache: &SlotCache, entry: SlotEntry) -> io::Result<()> {
  let _guard = MANIFEST_LOCK.lock().await;
  let mut entries = load_manifest(&cache.dir).await;
  entries.retain(|e| !(e.fingerprint == entry.fingerprint && e.slot == entry.slot));
  entries.push(entry);
  let dropped = prune(&mut entries, now_secs(), cache.max_age, cache.max_bytes);
  for gone in dropped {
    let _ = tokio::fs::remove_file(cache.dir.join(&gone.filename)).await;
  }
  write_manifest(&cache.dir, &entries).await
}

/// Drop what the TTL expires and then the oldest files until the total fits
/// `max_bytes`, returning the entries that went (so their files can be deleted).
///
/// Entries are left newest-first, which is the order a restore wants.
fn prune(
  entries: &mut Vec<SlotEntry>,
  now: u64,
  max_age: Duration,
  max_bytes: u64,
) -> Vec<SlotEntry> {
  let floor = now.saturating_sub(max_age.as_secs());
  let mut dropped: Vec<SlotEntry> = entries
    .iter()
    .filter(|e| e.saved_at < floor)
    .cloned()
    .collect();
  entries.retain(|e| e.saved_at >= floor);
  entries.sort_by(|a, b| {
    b.saved_at
      .cmp(&a.saved_at)
      .then_with(|| b.slot.cmp(&a.slot))
  });
  let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
  while total > max_bytes {
    match entries.pop() {
      Some(oldest) => {
        total = total.saturating_sub(oldest.bytes);
        dropped.push(oldest);
      }
      None => break,
    }
  }
  dropped
}

fn manifest_path(dir: &Path) -> PathBuf {
  dir.join("manifest.json")
}

async fn load_manifest(dir: &Path) -> Vec<SlotEntry> {
  let bytes = match tokio::fs::read(manifest_path(dir)).await {
    Ok(bytes) => bytes,
    Err(_) => return Vec::new(),
  };
  match serde_json::from_slice::<Manifest>(&bytes) {
    Ok(manifest) => manifest.entries,
    Err(err) => {
      // A manifest we cannot read is treated as empty rather than trusted: the
      // files it named get replaced by the next save, and an orphan file is
      // unrecoverable either way.
      log::warn!(
        "slot cache: ignoring unreadable manifest in {}: {err}",
        dir.display()
      );
      Vec::new()
    }
  }
}

async fn write_manifest(dir: &Path, entries: &[SlotEntry]) -> io::Result<()> {
  let body = serde_json::to_vec_pretty(&Manifest {
    entries: entries.to_vec(),
  })
  .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
  let path = manifest_path(dir);
  let tmp = path.with_extension("json.tmp");
  tokio::fs::write(&tmp, body).await?;
  tokio::fs::rename(&tmp, &path).await
}

/// The engine's filename for one slot of one model. Flat, and short enough for
/// `fs_validate_filename`, which rejects anything path-shaped.
fn filename_for(fingerprint: &str, slot: u32) -> String {
  format!("{fingerprint}-{slot}.bin")
}

/// Cache key for a model: its path plus the two metadata fields that make those
/// bytes what they are. A rewritten or re-downloaded file changes it, so a stale
/// cache can only ever be a missed hit, never a wrong one.
///
/// Deliberately *not* in the key: offload depth and KV cache type. A different
/// `-ngl` restores fine (measured) and a different `--cache-type-*` is refused by
/// the engine, so both are handled without splitting the cache.
fn fingerprint(model_path: &Path) -> Option<String> {
  let meta = std::fs::metadata(model_path).ok()?;
  let mtime = meta.modified().ok()?;
  let mtime = mtime.duration_since(UNIX_EPOCH).ok()?.as_secs();
  let mut hash = Sha256::new();
  hash.update(model_path.as_os_str().as_encoded_bytes());
  hash.update(meta.len().to_le_bytes());
  hash.update(mtime.to_le_bytes());
  Some(
    hash
      .finalize()
      .iter()
      .take(8)
      .map(|b| format!("{b:02x}"))
      .collect(),
  )
}

/// KV bytes this launch's cache type would need for `tokens` prompt tokens, or
/// `None` when the geometry is unknown. Same closed form the admission estimator
/// uses ([`crate::gguf::memory::kv_bytes`]), which the spike confirmed against the
/// engine's own `n_written` to within a kilobyte.
///
/// Reads the GGUF header (a few KB at the head of the file) synchronously: this
/// runs at most once per eviction, not per request.
fn projected_bytes(params: &LaunchParams, tokens: u64) -> Option<u64> {
  let read = crate::gguf::read_path(
    &params.model_path,
    crate::gguf::HeaderReadOptions::default(),
  )
  .ok()?;
  let header = &read.header;
  let arch = header.string(&["general.architecture"]).map(str::to_string);
  let opts = crate::gguf::EstimateOptions {
    ctx_len: tokens,
    cache_type_k: crate::gguf::memory::parse_cache_type(
      params.knobs.text_by_name("cache-type-k").as_deref(),
    ),
    cache_type_v: crate::gguf::memory::parse_cache_type(
      params.knobs.text_by_name("cache-type-v").as_deref(),
    ),
    n_gpu_layers: None,
  };
  match crate::gguf::memory::kv_bytes(header, arch.as_deref(), opts) {
    0 => None,
    bytes => Some(bytes),
  }
}

/// `GET /slots` as `(id, prompt tokens)` pairs. A slot that never got a prompt
/// reports no token count and reads as zero.
async fn list_slots(port: u16) -> io::Result<Vec<(u32, u64)>> {
  let (status, body) = http(port, "GET", "/slots", None, LIST_TIMEOUT).await?;
  if status != 200 {
    return Err(err(format!("HTTP {status}: {}", clip(&body, 200))));
  }
  let parsed: serde_json::Value = serde_json::from_str(&body)
    .map_err(|parse_err| err(format!("unreadable /slots reply: {parse_err}")))?;
  let list = parsed
    .as_array()
    .ok_or_else(|| err("/slots reply was not a list".to_string()))?;
  let mut out = Vec::with_capacity(list.len());
  for slot in list {
    let Some(id) = slot.get("id").and_then(serde_json::Value::as_u64) else {
      continue;
    };
    out.push((id as u32, number(slot, "n_prompt_tokens")));
  }
  Ok(out)
}

/// `POST /slots/{slot}?action={action}` with `{"filename": ...}` and nothing
/// else: `fs_validate_filename` gates that field, and the engine writes to
/// `--slot-save-path` concatenated with it verbatim.
async fn slot_action(
  port: u16,
  action: &str,
  slot: u32,
  filename: &str,
  timeout: Duration,
) -> io::Result<serde_json::Value> {
  let path = format!("/slots/{slot}?action={action}");
  let body = serde_json::json!({ "filename": filename }).to_string();
  let (status, text) = http(port, "POST", &path, Some(&body), timeout).await?;
  if status != 200 {
    return Err(err(format!("HTTP {status}: {}", clip(&text, 200))));
  }
  serde_json::from_str(&text)
    .map_err(|parse_err| err(format!("unreadable {action} reply: {parse_err}")))
}

/// Minimal HTTP/1.1 over a plain socket, in the no-extra-deps stance of
/// [`super::actuals`]: `Connection: close` and read-to-EOF, no client crate.
async fn http(
  port: u16,
  method: &str,
  path: &str,
  body: Option<&str>,
  timeout: Duration,
) -> io::Result<(u16, String)> {
  let mut request =
    format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
  match body {
    Some(text) => {
      request.push_str("Content-Type: application/json\r\n");
      request.push_str(&format!("Content-Length: {}\r\n", text.len()));
      request.push_str("\r\n");
      request.push_str(text);
    }
    None => request.push_str("\r\n"),
  }
  let raw = tokio::time::timeout(timeout, exchange(port, request.as_bytes()))
    .await
    .map_err(|_| err(format!("{method} {path} timed out")))??;
  let split = raw
    .windows(4)
    .position(|w| w == b"\r\n\r\n")
    .ok_or_else(|| err(format!("{method} {path} returned no HTTP response")))?;
  let head = String::from_utf8_lossy(&raw[..split]);
  let status = head
    .split_whitespace()
    .nth(1)
    .and_then(|code| code.parse::<u16>().ok())
    .ok_or_else(|| err(format!("{method} {path} returned no status line")))?;
  let text = String::from_utf8_lossy(&raw[split + 4..]).into_owned();
  Ok((status, text))
}

async fn exchange(port: u16, request: &[u8]) -> io::Result<Vec<u8>> {
  let mut sock = TcpStream::connect(("127.0.0.1", port)).await?;
  sock.write_all(request).await?;
  let mut buf = Vec::with_capacity(4096);
  let mut chunk = [0u8; 8192];
  loop {
    let read = sock.read(&mut chunk).await?;
    if read == 0 {
      break;
    }
    buf.extend_from_slice(&chunk[..read]);
    if buf.len() > MAX_RESPONSE_BYTES {
      break;
    }
  }
  Ok(buf)
}

fn number(value: &serde_json::Value, key: &str) -> u64 {
  value
    .get(key)
    .and_then(serde_json::Value::as_u64)
    .unwrap_or_default()
}

fn err(message: String) -> io::Error {
  io::Error::other(message)
}

fn clip(text: &str, max: usize) -> String {
  let trimmed = text.trim();
  if trimmed.len() <= max {
    return trimmed.to_string();
  }
  let mut end = max;
  while !trimmed.is_char_boundary(end) {
    end -= 1;
  }
  trimmed[..end].to_string()
}

fn now_secs() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::mode::LaunchMode;
  use std::path::PathBuf;

  fn entry(slot: u32, bytes: u64, saved_at: u64) -> SlotEntry {
    SlotEntry {
      fingerprint: "aa".into(),
      model: "/m/a.gguf".into(),
      slot,
      filename: format!("aa-{slot}.bin"),
      tokens: 1000,
      bytes,
      saved_at,
      failures: 0,
    }
  }

  fn params_with(dir: &str) -> LaunchParams {
    let mut p = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    p.launch_config
      .insert(LLAMACPP_KNOB_SLOT_CACHE_DIR.to_string(), dir.into());
    p
  }

  #[test]
  fn a_launch_without_the_key_is_not_cached() {
    let p = LaunchParams::new(PathBuf::from("/m/a.gguf"), LaunchMode::Chat);
    assert!(SlotCache::from_params(&p).is_none());
    assert!(SlotCache::from_params(&params_with("")).is_none());
  }

  #[test]
  fn the_key_carries_the_cap_and_ttl_along_with_the_dir() {
    let mut p = params_with("/state/slot-cache");
    p.launch_config.insert(
      LLAMACPP_KNOB_SLOT_CACHE_MAX_BYTES.to_string(),
      "1024".into(),
    );
    p.launch_config.insert(
      LLAMACPP_KNOB_SLOT_CACHE_MAX_AGE_HOURS.to_string(),
      "3".into(),
    );
    let cache = SlotCache::from_params(&p).expect("seeded key");
    assert_eq!(cache.dir, PathBuf::from("/state/slot-cache"));
    assert_eq!(cache.max_bytes, 1024);
    assert_eq!(cache.max_age, Duration::from_secs(3 * 3600));
    // Unseeded limits fall back to the config defaults, not to zero.
    let plain = SlotCache::from_params(&params_with("/x")).unwrap();
    assert_eq!(
      plain.max_bytes,
      super::super::SlotCacheConfig::default().max_bytes
    );
  }

  #[test]
  fn the_filename_is_flat_and_short() {
    let name = filename_for("0123456789abcdef", 3);
    assert_eq!(name, "0123456789abcdef-3.bin");
    assert!(!name.contains('/') && !name.contains('\\') && !name.contains(".."));
  }

  /// The engine's `fs_validate_filename` rejects `..`, any separator and an
  /// empty name; the spike confirmed those three rejections live.
  #[test]
  fn fingerprint_follows_the_file_not_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("a.gguf");
    std::fs::write(&model, b"weights").unwrap();
    let a = fingerprint(&model).expect("metadata is readable for a real file");
    assert_eq!(a.len(), 16, "short hex key that survives a manifest read");
    assert_eq!(a, fingerprint(&model).unwrap());
    std::fs::write(&model, b"re-download").unwrap();
    assert_ne!(
      a,
      fingerprint(&model).unwrap(),
      "a rewritten file gets a new key"
    );
    assert!(filename_for(&a, 0).ends_with(".bin"));
  }

  #[test]
  fn a_model_that_cannot_be_stat_has_no_key() {
    assert!(fingerprint(Path::new("/nonexistent/model.gguf")).is_none());
  }

  /// An expired entry must come back in `dropped`, not just vanish: the caller
  /// deletes files from that list, so anything dropped silently is a file left on
  /// disk with nothing pointing at it.
  #[test]
  fn ttl_expires_entries_and_returns_them() {
    let now = 1_000_000;
    let mut entries = vec![
      entry(0, 100, now - 7200),
      entry(1, 100, now - 60),
      entry(2, 100, now - 30),
    ];
    let dropped = prune(&mut entries, now, Duration::from_secs(3600), 10_000);
    assert_eq!(
      dropped.iter().map(|e| e.slot).collect::<Vec<_>>(),
      vec![0],
      "the two-hour-old entry is past a one-hour TTL"
    );
    assert_eq!(
      entries.iter().map(|e| e.slot).collect::<Vec<_>>(),
      vec![2, 1],
      "kept entries come out newest-first"
    );
  }

  #[test]
  fn the_cap_drops_the_oldest_until_the_total_fits() {
    let now = 1_000_000;
    let mut entries = vec![
      entry(0, 100, now - 20),
      entry(1, 100, now - 10),
      entry(2, 100, now),
    ];
    let dropped = prune(&mut entries, now, Duration::from_secs(3600), 150);
    assert_eq!(
      dropped.iter().map(|e| e.slot).collect::<Vec<_>>(),
      vec![0, 1],
      "oldest first until the total fits"
    );
    assert_eq!(entries.len(), 1);
  }

  #[test]
  fn prune_clears_out_entirely_when_even_one_file_does_not_fit() {
    let now = 1000;
    let mut entries = vec![entry(0, 5000, now)];
    let dropped = prune(&mut entries, now, Duration::from_secs(1), 100);
    assert_eq!(dropped.len(), 1);
    assert!(entries.is_empty());
  }

  #[tokio::test]
  async fn manifest_round_trips_and_survives_a_bad_file() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![entry(0, 10, 5), entry(3, 20, 7)];
    write_manifest(dir.path(), &entries).await.unwrap();
    assert_eq!(load_manifest(dir.path()).await, entries);
    tokio::fs::write(manifest_path(dir.path()), b"{ not json")
      .await
      .unwrap();
    assert!(load_manifest(dir.path()).await.is_empty());
  }

  #[tokio::test]
  async fn record_replaces_the_entry_for_its_slot() {
    let dir = tempfile::tempdir().unwrap();
    let cache = SlotCache {
      dir: dir.path().to_path_buf(),
      max_bytes: u64::MAX,
      max_age: Duration::from_secs(3600),
    };
    let now = now_secs();
    record(&cache, entry(0, 10, now)).await.unwrap();
    record(&cache, entry(0, 999, now)).await.unwrap();
    record(
      &cache,
      SlotEntry {
        slot: 1,
        bytes: 5,
        ..entry(0, 10, now)
      },
    )
    .await
    .unwrap();
    let entries = load_manifest(dir.path()).await;
    assert_eq!(entries.len(), 2, "one entry per slot, the second save wins");
    assert_eq!(entries[0].bytes, 5);
    assert_eq!(entries[1].bytes, 999);
  }

  #[tokio::test]
  async fn record_deletes_the_files_the_cap_drops() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("aa-0.bin");
    std::fs::write(&old, b"kv").unwrap();
    let cache = SlotCache {
      dir: dir.path().to_path_buf(),
      max_bytes: 100,
      max_age: Duration::from_secs(3600),
    };
    let mut stale = entry(0, 100, now_secs() - 60);
    stale.tokens = 4096;
    record(&cache, stale).await.unwrap();
    record(&cache, entry(1, 100, now_secs())).await.unwrap();
    assert!(!old.exists(), "the older slot's file goes with its entry");
    assert_eq!(load_manifest(dir.path()).await.len(), 1);
  }

  #[test]
  fn clip_stays_on_a_char_boundary() {
    assert_eq!(clip("  plain  ", 10), "plain");
    assert_eq!(clip("héllo", 3), "hé", "the boundary before the limit wins");
  }
}
