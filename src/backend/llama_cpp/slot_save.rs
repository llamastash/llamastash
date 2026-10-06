//! Keep a launch's prompt cache across an eviction.
//!
//! `llama-server` can write a slot's KV cache to a file and read it back:
//! `--slot-save-path DIR` plus `POST /slots/{id}?action=save|restore`. When the
//! idle sweep or make-room is about to stop a launch, [`before_evict`] saves the
//! slots that hold a prompt. The next launch of the same model with the same
//! settings gets them back in [`after_ready`], before it is reported `Ready`.
//! The engine then matches a returning conversation to the restored slot by
//! longest common token prefix, the way it does for a live slot.
//!
//! A slot file leaves out the engine's context checkpoints. Without them the
//! engine reuses a restored slot only for models whose cache can be rolled back
//! to a shorter prefix: full-attention models, and not hybrid or sliding-window
//! ones (measured on b11457). So each launch is probed once ([`probe`]) and
//! nothing is saved where the restore would be thrown away.
//!
//! Measurements and the reasons for the limits:
//! `docs/spikes/2026-10-06-slot-save-restore.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::daemon::supervisor::{LaunchOrigin, ManagedModel};
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

const GIB: u64 = 1024 * 1024 * 1024;

/// `launch_config` keys. [`seed`] writes them, `compose` reads the path, and the
/// two hooks read the rest off the running launch, so a launch keeps the limits
/// it started with.
pub(super) const KEY_PATH: &str = "slot_save_path";
const KEY_MAX_BYTES: &str = "slot_save_max_bytes";
const KEY_MAX_AGE_SECS: &str = "slot_save_max_age_secs";
const KEY_MIN_TOKENS: &str = "slot_save_min_tokens";

/// One save or restore call. A 16 GiB file at 150 MB/s is about 110 s.
const SLOT_ACTION_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_STEP_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
/// A `.tmp` this old belongs to a save that never finished.
const STALE_TMP: Duration = Duration::from_secs(15 * 60);

/// The probe's two prompts share this many leading numbers.
const PROBE_SHARED_NUMBERS: u32 = 24;
const PROBE_LONG_NUMBERS: u32 = 40;
/// Every tokenizer spends at least one token per number, so a reused prefix is
/// well above this and a discarded one is 0.
const PROBE_MIN_CACHED: u64 = 8;

/// `backend.llamacpp.slot_save` in `config.yaml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct SlotSaveConfig {
  /// Save a launch's prompt cache before the idle sweep or make-room stops it
  /// and restore it into the next launch of the same model. Factory `false`:
  /// a save writes about 33 KiB per cached token on a 1B model and more on
  /// larger ones.
  pub enabled: bool,
  /// Total size of the saved files, in GiB. The oldest are deleted first, and a
  /// slot that would not fit is not saved. `0` saves nothing.
  pub max_gib: u64,
  /// A saved file is deleted this many seconds after it was written. `0`
  /// means no age limit.
  pub max_age_secs: u64,
  /// A slot holding fewer cached tokens than this is not saved.
  pub min_tokens: u64,
}

impl Default for SlotSaveConfig {
  fn default() -> Self {
    Self {
      enabled: false,
      max_gib: 16,
      max_age_secs: 24 * 60 * 60,
      min_tokens: 2048,
    }
  }
}

/// Project the config onto one launch. Leaves the keys unset, which turns both
/// hooks into no-ops, unless this launch can use slot files.
pub(super) fn seed(cfg: &SlotSaveConfig, log_dir: Option<&Path>, params: &mut LaunchParams) {
  for key in [KEY_PATH, KEY_MAX_BYTES, KEY_MAX_AGE_SECS, KEY_MIN_TOKENS] {
    params.launch_config.remove(key);
  }
  // A user who passes the flag in extras manages their own slot files.
  if !cfg.enabled
    || params.mode != LaunchMode::Chat
    || crate::launch::params::bench_disable_defaults_from_env()
    || crate::launch::params::extras_have_flag(&params.extras, "--slot-save-path")
  {
    return;
  }
  // The files are reclaimable, so they sit beside the logs in the cache dir.
  let Some(root) = log_dir.and_then(Path::parent).map(|c| c.join("slots")) else {
    return;
  };
  let Some(root_str) = root.to_str() else {
    return;
  };
  // llama-server exits at startup when the directory is missing.
  if let Err(e) = create_private_dir(&root) {
    log::warn!(
      "slot save: cannot create {}: {e}; off for this launch",
      root.display()
    );
    return;
  }
  let config = &mut params.launch_config;
  config.insert(KEY_PATH.to_string(), root_str.to_string());
  config.insert(
    KEY_MAX_BYTES.to_string(),
    cfg.max_gib.saturating_mul(GIB).to_string(),
  );
  config.insert(KEY_MAX_AGE_SECS.to_string(), cfg.max_age_secs.to_string());
  config.insert(KEY_MIN_TOKENS.to_string(), cfg.min_tokens.to_string());
}

/// A slot file holds a conversation's tokens and llama-server writes it with
/// the default umask, so the directory is what keeps other users out.
fn create_private_dir(root: &Path) -> std::io::Result<()> {
  std::fs::create_dir_all(root)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
  }
  Ok(())
}

fn human_size(bytes: u64) -> String {
  const MIB: u64 = 1024 * 1024;
  if bytes >= GIB {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
  } else {
    format!("{} MiB", bytes.div_ceil(MIB))
  }
}

/// The limits a running launch was started with.
struct Limits {
  root: PathBuf,
  max_bytes: u64,
  max_age: Duration,
  min_tokens: u64,
}

/// `None` when slot files are off for this launch.
fn limits(params: &LaunchParams) -> Option<Limits> {
  let config = &params.launch_config;
  let number = |key: &str| config.get(key).and_then(|v| v.parse::<u64>().ok());
  Some(Limits {
    root: PathBuf::from(config.get(KEY_PATH)?),
    max_bytes: number(KEY_MAX_BYTES)?,
    max_age: Duration::from_secs(number(KEY_MAX_AGE_SECS)?),
    min_tokens: number(KEY_MIN_TOKENS)?,
  })
}

/// File-name prefix shared by launches that can exchange slot files: the same
/// model file, server build and argv apart from the port. A changed knob is a
/// different prefix, so a file is never restored into a launch it was not
/// written for.
fn file_prefix(model: &ManagedModel) -> String {
  let mut hasher = blake3::Hasher::new();
  hasher.update(model.id().path.as_os_str().as_encoded_bytes());
  hasher.update(&model.id().header_blake3);
  hasher.update(model.params().server.as_deref().unwrap_or("").as_bytes());
  for arg in super::compose::compose(model.params(), 0) {
    hasher.update(&[0]);
    hasher.update(arg.as_encoded_bytes());
  }
  let hex = hasher.finalize().to_hex();
  format!("{}.{}", file_stem(&model.id().path), &hex[..16])
}

/// The model's file stem, reduced to characters llama-server accepts in a slot
/// file name. It carries no dot, so the prefix's own dot is unambiguous.
fn file_stem(path: &Path) -> String {
  let stem: String = path
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("model")
    .chars()
    .map(|c| {
      if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
        c
      } else {
        '-'
      }
    })
    .take(80)
    .collect();
  if stem.is_empty() {
    "model".to_string()
  } else {
    stem
  }
}

/// The saved slot files for `prefix`, as `(slot id, path)`.
fn saved_slots(root: &Path, prefix: &str) -> Vec<(u32, PathBuf)> {
  let Ok(entries) = std::fs::read_dir(root) else {
    return Vec::new();
  };
  let mut out: Vec<(u32, PathBuf)> = entries
    .flatten()
    .filter_map(|entry| {
      let name = entry.file_name();
      let slot = name
        .to_str()?
        .strip_prefix(prefix)?
        .strip_prefix(".slot")?
        .strip_suffix(".bin")?
        .parse::<u32>()
        .ok()?;
      Some((slot, entry.path()))
    })
    .collect();
  out.sort();
  out
}

/// What the probe (or a restore) learned about slot files for one prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reuse {
  /// A restored slot serves a prompt that shares only a prefix with it.
  reusable: bool,
  /// A slot file is about `fixed_bytes + bytes_per_token * tokens`.
  fixed_bytes: u64,
  bytes_per_token: u64,
}

impl Reuse {
  const NOT_REUSABLE: Reuse = Reuse {
    reusable: false,
    fixed_bytes: 0,
    bytes_per_token: 0,
  };

  /// Fit the size model through two saves of `(tokens, bytes)`. A hybrid
  /// model's file carries a fixed recurrent state next to the per-token part,
  /// so one point would overprice every token.
  fn fit(reusable: bool, long: (u64, u64), short: (u64, u64)) -> Self {
    let (long_tokens, long_bytes) = long;
    let (short_tokens, short_bytes) = short;
    if long_tokens > short_tokens && long_bytes >= short_bytes {
      let bytes_per_token = (long_bytes - short_bytes) / (long_tokens - short_tokens);
      Reuse {
        reusable,
        fixed_bytes: long_bytes.saturating_sub(bytes_per_token.saturating_mul(long_tokens)),
        bytes_per_token,
      }
    } else {
      Reuse {
        reusable,
        fixed_bytes: 0,
        bytes_per_token: long_bytes / long_tokens.max(1),
      }
    }
  }

  fn estimate(&self, tokens: u64) -> u64 {
    self
      .fixed_bytes
      .saturating_add(self.bytes_per_token.saturating_mul(tokens))
  }
}

/// Probe results by file prefix, kept for the daemon's lifetime so a model is
/// probed once however often it is reloaded.
fn verdicts() -> std::sync::MutexGuard<'static, HashMap<String, Reuse>> {
  static VERDICTS: OnceLock<Mutex<HashMap<String, Reuse>>> = OnceLock::new();
  VERDICTS
    .get_or_init(Default::default)
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn http() -> &'static reqwest::Client {
  static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
  CLIENT.get_or_init(|| {
    reqwest::Client::builder()
      .no_proxy()
      .build()
      .unwrap_or_default()
  })
}

/// Why a call to the server failed.
#[derive(Debug)]
enum CallError {
  /// The server answered with an error, or took too long.
  Refused(String),
  /// The server could not be reached, as when it is being stopped.
  Unreachable(String),
}

impl std::fmt::Display for CallError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      CallError::Refused(why) | CallError::Unreachable(why) => f.write_str(why),
    }
  }
}

impl From<reqwest::Error> for CallError {
  fn from(e: reqwest::Error) -> Self {
    if e.is_timeout() || e.is_decode() {
      CallError::Refused(e.to_string())
    } else {
      CallError::Unreachable(e.to_string())
    }
  }
}

async fn send(request: reqwest::RequestBuilder, timeout: Duration) -> Result<Value, CallError> {
  let response = request.timeout(timeout).send().await?;
  let status = response.status();
  let body: Value = response.json().await?;
  if !status.is_success() {
    let message = body
      .pointer("/error/message")
      .and_then(Value::as_str)
      .unwrap_or("no error message");
    return Err(CallError::Refused(format!(
      "HTTP {}: {message}",
      status.as_u16()
    )));
  }
  Ok(body)
}

/// `POST /slots/{slot}?action=save|restore`, returning `(tokens, bytes)`.
async fn slot_file_action(
  port: u16,
  slot: u32,
  action: &str,
  filename: &str,
  timeout: Duration,
) -> Result<(u64, u64), CallError> {
  let url = format!("http://127.0.0.1:{port}/slots/{slot}?action={action}");
  let body = send(
    http().post(url).json(&json!({ "filename": filename })),
    timeout,
  )
  .await?;
  let (tokens_key, bytes_key) = if action == "save" {
    ("n_saved", "n_written")
  } else {
    ("n_restored", "n_read")
  };
  let field = |key: &str| {
    body
      .get(key)
      .and_then(Value::as_u64)
      .ok_or_else(|| CallError::Refused(format!("no `{key}` in the {action} response")))
  };
  Ok((field(tokens_key)?, field(bytes_key)?))
}

async fn erase_slot(port: u16, slot: u32) -> Result<(), CallError> {
  let url = format!("http://127.0.0.1:{port}/slots/{slot}?action=erase");
  send(http().post(url).json(&json!({})), PROBE_STEP_TIMEOUT)
    .await
    .map(|_| ())
}

/// The idle slots that hold a prompt, as `(slot id, cached tokens)`, largest
/// first. A slot that never served a request reports no token count.
fn parse_slots(body: &Value) -> Vec<(u32, u64)> {
  let mut out: Vec<(u32, u64)> = body
    .as_array()
    .map(Vec::as_slice)
    .unwrap_or_default()
    .iter()
    .filter(|slot| slot.get("is_processing").and_then(Value::as_bool) != Some(true))
    .filter_map(|slot| {
      let id = u32::try_from(slot.get("id")?.as_u64()?).ok()?;
      let tokens = slot.get("n_prompt_tokens")?.as_u64()?;
      (tokens > 0).then_some((id, tokens))
    })
    .collect();
  out.sort_by_key(|&(_, tokens)| std::cmp::Reverse(tokens));
  out
}

fn probe_prompt(numbers: u32, tail: &str) -> String {
  let mut prompt = (1..=numbers)
    .map(|n| n.to_string())
    .collect::<Vec<_>>()
    .join(" ");
  prompt.push_str(tail);
  prompt
}

/// A one-token completion on slot 0, returning how many prompt tokens came from
/// the cache.
async fn probe_completion(port: u16, prompt: &str) -> Result<u64, CallError> {
  let body = json!({
    "prompt": prompt,
    "n_predict": 1,
    "cache_prompt": true,
    "id_slot": 0,
    "temperature": 0,
  });
  let url = format!("http://127.0.0.1:{port}/completion");
  let out = send(http().post(url).json(&body), PROBE_STEP_TIMEOUT).await?;
  out
    .pointer("/timings/cache_n")
    .and_then(Value::as_u64)
    .ok_or_else(|| CallError::Refused("no `timings.cache_n` in the completion response".into()))
}

/// Ask the running server whether a restored slot is reused: fill slot 0, save
/// it, erase it, restore it, then send a prompt that shares only a prefix. The
/// erase drops the in-memory context checkpoints, which is the state a restore
/// into a new process starts from. Must run before the launch serves anything,
/// because it overwrites slot 0.
async fn probe(port: u16, root: &Path, prefix: &str) -> Reuse {
  let file = format!("{prefix}.probe.{port}.tmp");
  let outcome = probe_steps(port, &file).await;
  let _ = erase_slot(port, 0).await;
  let _ = tokio::fs::remove_file(root.join(&file)).await;
  match outcome {
    Ok(reuse) => {
      log::info!(
        "slot save: {prefix} on port {port}: a restored slot {} reused (about {} bytes per token)",
        if reuse.reusable { "is" } else { "is not" },
        reuse.bytes_per_token,
      );
      reuse
    }
    Err(e) => {
      log::info!("slot save: probe failed for {prefix} on port {port}, not saving its slots: {e}");
      Reuse::NOT_REUSABLE
    }
  }
}

async fn probe_steps(port: u16, file: &str) -> Result<Reuse, CallError> {
  probe_completion(port, &probe_prompt(PROBE_LONG_NUMBERS, "")).await?;
  let long = slot_file_action(port, 0, "save", file, PROBE_STEP_TIMEOUT).await?;
  erase_slot(port, 0).await?;
  slot_file_action(port, 0, "restore", file, PROBE_STEP_TIMEOUT).await?;
  let cached = probe_completion(port, &probe_prompt(PROBE_SHARED_NUMBERS, " stop")).await?;
  let short = slot_file_action(port, 0, "save", file, PROBE_STEP_TIMEOUT).await?;
  Ok(Reuse::fit(cached >= PROBE_MIN_CACHED, long, short))
}

/// Restore what an eviction saved for this launch, or probe it when there is
/// nothing to restore and it can be evicted later. Runs before `Ready`, so the
/// request that started the launch finds its slot already filled.
pub(super) async fn after_ready(model: &ManagedModel) {
  let Some(limits) = limits(model.params()) else {
    return;
  };
  let prefix = file_prefix(model);
  let port = model.port();
  prune(&limits).await;
  let mut restored = None;
  for (slot, path) in saved_slots(&limits.root, &prefix) {
    let name = path
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or_default();
    match slot_file_action(port, slot, "restore", name, SLOT_ACTION_TIMEOUT).await {
      Ok((tokens, bytes)) => {
        log::info!(
          "slot save: restored slot {slot} of {prefix} on port {port} ({tokens} tokens, {})",
          human_size(bytes)
        );
        restored = Some(Reuse {
          reusable: true,
          fixed_bytes: 0,
          bytes_per_token: bytes / tokens.max(1),
        });
      }
      // The launch is going away mid-load. The file is still good for the
      // next one.
      Err(CallError::Unreachable(e)) => {
        log::warn!("slot save: {name} not restored, the server is gone: {e}");
        return;
      }
      Err(CallError::Refused(e)) => log::warn!("slot save: restore of {name} failed: {e}"),
    }
    // The slot holds it now, or the server cannot use the file.
    let _ = tokio::fs::remove_file(&path).await;
  }
  if let Some(reuse) = restored {
    verdicts().entry(prefix).or_insert(reuse);
    return;
  }
  // Only an auto-started launch is ever evicted, so only it needs the answer.
  if model.origin() != LaunchOrigin::AutoStart || verdicts().contains_key(&prefix) {
    return;
  }
  let reuse = probe(port, &limits.root, &prefix).await;
  verdicts().insert(prefix, reuse);
}

/// Save the slots that hold a prompt, ahead of the stop that evicts this launch.
pub(super) async fn before_evict(model: &ManagedModel) {
  let Some(limits) = limits(model.params()) else {
    return;
  };
  let prefix = file_prefix(model);
  let known = verdicts().get(&prefix).copied();
  let Some(reuse) = known.filter(|r| r.reusable) else {
    return;
  };
  let port = model.port();
  let url = format!("http://127.0.0.1:{port}/slots");
  let slots = match send(http().get(url), LIST_TIMEOUT).await {
    Ok(body) => parse_slots(&body),
    Err(e) => {
      log::info!("slot save: cannot list the slots of {prefix} on port {port}, nothing saved: {e}");
      return;
    }
  };
  // Files from an eviction this launch survived are older than its live slots.
  for (_, stale) in saved_slots(&limits.root, &prefix) {
    let _ = tokio::fs::remove_file(stale).await;
  }
  let mut budget = limits.max_bytes;
  for (slot, tokens) in slots {
    if tokens < limits.min_tokens {
      continue;
    }
    let estimate = reuse.estimate(tokens);
    if estimate > budget {
      log::info!(
        "slot save: slot {slot} of {prefix} ({tokens} tokens, about {}) is over the size cap, not saved",
        human_size(estimate)
      );
      continue;
    }
    if crate::init::download::precheck_disk(&limits.root, estimate).is_err() {
      log::warn!(
        "slot save: not enough free disk under {} for slot {slot} of {prefix} (about {}), not saved",
        limits.root.display(),
        human_size(estimate)
      );
      continue;
    }
    // Written under a per-launch name and renamed, so a restore never reads a
    // partial file and two launches with one prefix never share a write.
    let tmp = format!("{prefix}.slot{slot}.{port}.tmp");
    let tmp_path = limits.root.join(&tmp);
    let saved = slot_file_action(port, slot, "save", &tmp, SLOT_ACTION_TIMEOUT).await;
    let renamed = match saved {
      Ok(size) => tokio::fs::rename(
        &tmp_path,
        limits.root.join(format!("{prefix}.slot{slot}.bin")),
      )
      .await
      .map(|()| size)
      .map_err(|e| e.to_string()),
      Err(e) => Err(e.to_string()),
    };
    match renamed {
      Ok((saved_tokens, bytes)) => {
        budget = budget.saturating_sub(bytes);
        log::info!(
          "slot save: saved slot {slot} of {prefix} on port {port} ({saved_tokens} tokens, {})",
          human_size(bytes)
        );
      }
      Err(e) => {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        log::warn!("slot save: saving slot {slot} of {prefix} failed: {e}");
      }
    }
  }
  prune(&limits).await;
}

async fn prune(limits: &Limits) {
  let (root, max_age, max_bytes) = (limits.root.clone(), limits.max_age, limits.max_bytes);
  let deleted =
    tokio::task::spawn_blocking(move || prune_dir(&root, max_age, max_bytes, SystemTime::now()))
      .await
      .unwrap_or_default();
  for path in deleted {
    log::info!("slot save: deleted {}", path.display());
  }
}

/// Delete saved files older than `max_age`, unfinished saves, and then the
/// oldest files until the rest fit `max_bytes`. Returns what was deleted.
fn prune_dir(root: &Path, max_age: Duration, max_bytes: u64, now: SystemTime) -> Vec<PathBuf> {
  let Ok(entries) = std::fs::read_dir(root) else {
    return Vec::new();
  };
  let mut deleted = Vec::new();
  let mut kept: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
  for entry in entries.flatten() {
    let path = entry.path();
    let extension = path.extension().and_then(|e| e.to_str());
    let is_tmp = extension == Some("tmp");
    if !is_tmp && extension != Some("bin") {
      continue;
    }
    let Ok(meta) = entry.metadata() else {
      continue;
    };
    let modified = meta.modified().unwrap_or(now);
    let age = now.duration_since(modified).unwrap_or_default();
    let expired = if is_tmp {
      age > STALE_TMP
    } else {
      !max_age.is_zero() && age > max_age
    };
    if expired {
      deleted.push(path);
    } else if !is_tmp {
      kept.push((modified, meta.len(), path));
    }
  }
  kept.sort();
  let mut total: u64 = kept.iter().map(|(_, len, _)| len).sum();
  for (_, len, path) in kept {
    if total <= max_bytes {
      break;
    }
    total -= len;
    deleted.push(path);
  }
  deleted.retain(|path| std::fs::remove_file(path).is_ok());
  deleted
}

#[cfg(test)]
mod tests {
  use super::*;

  fn chat_params() -> LaunchParams {
    LaunchParams::new(PathBuf::from("/models/a.gguf"), LaunchMode::Chat)
  }

  fn enabled() -> SlotSaveConfig {
    SlotSaveConfig {
      enabled: true,
      ..SlotSaveConfig::default()
    }
  }

  fn touch(path: &Path, len: usize, age: Duration, now: SystemTime) {
    std::fs::write(path, vec![0u8; len]).unwrap();
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(now - age).unwrap();
  }

  #[test]
  fn seed_is_off_by_default_and_for_launches_that_cannot_use_slot_files() {
    let dir = crate::test_support::unique_temp_dir("ls-slot", "seed-off");
    let log_dir = dir.join("logs");

    let mut off = chat_params();
    seed(&SlotSaveConfig::default(), Some(&log_dir), &mut off);
    assert!(limits(&off).is_none());
    assert!(!dir.join("slots").exists());

    let mut embedding = LaunchParams::new(PathBuf::from("/models/e.gguf"), LaunchMode::Embedding);
    seed(&enabled(), Some(&log_dir), &mut embedding);
    assert!(limits(&embedding).is_none());

    let mut own_flag = chat_params();
    own_flag.extras = vec!["--slot-save-path".into(), "/elsewhere".into()];
    seed(&enabled(), Some(&log_dir), &mut own_flag);
    assert!(limits(&own_flag).is_none());

    // A stale value from an inherited launch must not survive the config flip.
    let mut inherited = chat_params();
    seed(&enabled(), Some(&log_dir), &mut inherited);
    assert!(limits(&inherited).is_some());
    seed(&SlotSaveConfig::default(), Some(&log_dir), &mut inherited);
    assert!(limits(&inherited).is_none());
  }

  #[test]
  fn seed_creates_the_directory_beside_the_logs_and_projects_the_limits() {
    let dir = crate::test_support::unique_temp_dir("ls-slot", "seed-on");
    let mut params = chat_params();
    let cfg = SlotSaveConfig {
      enabled: true,
      max_gib: 2,
      max_age_secs: 60,
      min_tokens: 10,
    };
    seed(&cfg, Some(&dir.join("logs")), &mut params);
    let got = limits(&params).expect("slot save is on");
    assert_eq!(got.root, dir.join("slots"));
    assert!(got.root.is_dir());
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let mode = std::fs::metadata(&got.root).unwrap().permissions().mode();
      assert_eq!(mode & 0o777, 0o700, "slot files hold conversation tokens");
    }
    assert_eq!(got.max_bytes, 2 * GIB);
    assert_eq!(got.max_age, Duration::from_secs(60));
    assert_eq!(got.min_tokens, 10);
  }

  #[test]
  fn fit_separates_the_fixed_part_from_the_per_token_part() {
    // Llama-3.2-1B on b11457: 80 tokens -> 2_623_156 bytes, 32_784 per token.
    let flat = Reuse::fit(true, (80, 2_623_156), (49, 1_606_852));
    assert_eq!(flat.bytes_per_token, 32_784);
    assert!(flat.fixed_bytes < 1024);
    assert_eq!(flat.estimate(100_000) / 1_000_000, 3278);

    // A file with a 50 MB fixed part must not charge it to every token.
    let hybrid = Reuse::fit(
      true,
      (110, 50_000_000 + 110 * 40_000),
      (63, 50_000_000 + 63 * 40_000),
    );
    assert_eq!(hybrid.bytes_per_token, 40_000);
    assert_eq!(hybrid.fixed_bytes, 50_000_000);

    // Two equal points fall back to one-point pricing instead of dividing by 0.
    assert_eq!(
      Reuse::fit(true, (10, 1000), (10, 1000)).bytes_per_token,
      100
    );
  }

  #[test]
  fn parse_slots_keeps_idle_slots_with_a_prompt_largest_first() {
    let body = json!([
      { "id": 0, "is_processing": false },
      { "id": 1, "is_processing": false, "n_prompt_tokens": 0 },
      { "id": 2, "is_processing": false, "n_prompt_tokens": 5003 },
      { "id": 3, "is_processing": true, "n_prompt_tokens": 90000 },
      { "id": 4, "is_processing": false, "n_prompt_tokens": 70000 },
    ]);
    assert_eq!(parse_slots(&body), vec![(4, 70000), (2, 5003)]);
    assert!(parse_slots(&json!({ "error": "disabled" })).is_empty());
  }

  #[test]
  fn file_stem_keeps_only_characters_the_engine_accepts() {
    assert_eq!(
      file_stem(Path::new("/m/Llama-3.2-1B Instruct..Q4_K_M.gguf")),
      "Llama-3-2-1B-Instruct--Q4_K_M"
    );
    assert_eq!(file_stem(Path::new("/")), "model");
  }

  #[test]
  fn saved_slots_matches_one_prefix_only() {
    let dir = crate::test_support::unique_temp_dir("ls-slot", "saved");
    for name in [
      "a.0123456789abcdef.slot0.bin",
      "a.0123456789abcdef.slot3.bin",
      "a.0123456789abcdef.slot1.41000.tmp",
      "a.0123456789abcdef.probe.41000.tmp",
      "a.fedcba9876543210.slot0.bin",
      "ab.0123456789abcdef.slot0.bin",
    ] {
      std::fs::write(dir.join(name), b"x").unwrap();
    }
    let slots: Vec<u32> = saved_slots(&dir, "a.0123456789abcdef")
      .into_iter()
      .map(|(slot, _)| slot)
      .collect();
    assert_eq!(slots, vec![0, 3]);
  }

  #[test]
  fn prune_drops_expired_and_unfinished_files_then_the_oldest_over_the_cap() {
    let dir = crate::test_support::unique_temp_dir("ls-slot", "prune");
    let now = SystemTime::now();
    let minutes = |n: u64| Duration::from_secs(n * 60);
    touch(&dir.join("old.bin"), 10, minutes(120), now);
    touch(&dir.join("first.bin"), 400, minutes(30), now);
    touch(&dir.join("second.bin"), 400, minutes(20), now);
    touch(&dir.join("third.bin"), 400, minutes(10), now);
    touch(&dir.join("dead.tmp"), 10, minutes(20), now);
    touch(&dir.join("writing.tmp"), 5000, minutes(1), now);
    touch(&dir.join("notes.txt"), 5000, minutes(500), now);

    let mut deleted: Vec<String> = prune_dir(&dir, minutes(60), 900, now)
      .into_iter()
      .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
      .collect();
    deleted.sort();
    assert_eq!(deleted, vec!["dead.tmp", "first.bin", "old.bin"]);
    for kept in ["second.bin", "third.bin", "writing.tmp", "notes.txt"] {
      assert!(dir.join(kept).exists(), "{kept} should be kept");
    }
  }

  #[test]
  fn prune_with_no_age_limit_only_enforces_the_cap() {
    let dir = crate::test_support::unique_temp_dir("ls-slot", "prune-noage");
    let now = SystemTime::now();
    touch(
      &dir.join("ancient.bin"),
      10,
      Duration::from_secs(90 * 86_400),
      now,
    );
    assert!(prune_dir(&dir, Duration::ZERO, 100, now).is_empty());
    assert_eq!(prune_dir(&dir, Duration::ZERO, 0, now).len(), 1);
  }
}
