//! Cross-restart KV (prompt) cache for llama.cpp launches.
//!
//! When the idle sweep or make-room stops a launch mid-conversation, the next
//! request reprocesses the whole prompt. llama-server can write a slot's KV
//! state to disk (`--slot-save-path` plus
//! `POST /slots/{id_slot}?action=save|restore`), so llamastash saves every
//! non-empty slot right before an eviction stop and restores the files once a
//! matching launch is healthy again. Measured on llama.cpp b11457 with a 4B
//! model (`docs/spikes/2026-10-07-slot-kv-persistence.md`): a 24k-token slot
//! saves in 0.47 s (840 MB) and restores in 0.13 s against a 24.9 s
//! reprocess.
//!
//! Matching a restored slot to the returning conversation is the engine's job:
//! llama.cpp routes a new request to the slot whose cached prompt shares the
//! longest common prefix ("selected slot by LCP similarity" in the server
//! log), so restoring each slot id is sufficient — a conversation that lands
//! on a different slot simply misses the cache, never reuses a wrong one.
//!
//! The engine validates only the model arch when loading a state file, so the
//! sidecar written next to every save pins the full launch identity
//! ([`Sidecar`]); a pair whose fingerprint disagrees with the live launch is
//! left alone, because it belongs to a different launch that may still read
//! it. Files are consumed (deleted) after
//! a restore attempt, good or bad — a file that fails once will not get
//! better, and one that succeeds has been read into the server.
//!
//! Transport is a hand-rolled loopback request over raw TCP, the same
//! no-dep stance as [`super::actuals`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::launch::params::LaunchParams;

/// One slot save. The engine writes sequentially and a 100k-token slot takes
/// seconds; the budget guards a wedged child, not normal work.
const SAVE_TIMEOUT: Duration = Duration::from_secs(30);
/// One slot restore, same rationale as [`SAVE_TIMEOUT`] with headroom for
/// cold-disk reads of multi-GB state files.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(60);
const SLOT_LIST_TIMEOUT: Duration = Duration::from_secs(5);
/// Save pairs untouched for this long are dropped by [`prune`] at daemon
/// start: a conversation abandoned for a week is not worth reviving, and a
/// crash between save and restore would otherwise strand the pair forever.
const PAIR_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// How long a state file with no sidecar may sit in a directory. A restore
/// deletes the sidecar the moment it stages the file, so an unclaimed one is
/// either seconds from being read or the corpse of a crashed restore.
const STAGED_MAX_AGE: Duration = Duration::from_secs(600);

/// Sidecar naming the state file's owner inside a launch's save dir.
fn bin_name(slot: u32) -> String {
  format!("slot-{slot}.bin")
}

fn sidecar_name(slot: u32) -> String {
  format!("slot-{slot}.json")
}

fn slot_from_bin(name: &str) -> Option<u32> {
  name
    .strip_prefix("slot-")?
    .strip_suffix(".bin")?
    .parse()
    .ok()
}

fn slot_from_json(name: &str) -> Option<u32> {
  name
    .strip_prefix("slot-")?
    .strip_suffix(".json")?
    .parse()
    .ok()
}

/// A file's name as a `&str`, or `None` when it is not valid UTF-8 (never ours).
fn name_of(path: &Path) -> Option<&str> {
  path.file_name().and_then(|n| n.to_str())
}

/// What a saved state file was produced by. llama.cpp only checks the model
/// arch when loading, so every field here is checked against the relaunch
/// before a file is fed to the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
  /// The state file's slot, echoed for self-consistency with the file name.
  pub slot: u32,
  /// Canonical model path as the launching process saw it.
  pub model_path: String,
  /// Model file size and mtime — a re-download or shard swap changes the KV
  /// layout without changing the path.
  pub model_len: u64,
  pub model_mtime: u64,
  /// Serving mode label ([`crate::launch::mode::LaunchMode`]); an embedding
  /// launch must not inherit a chat conversation's state.
  pub mode: String,
  /// The context window the engine resolved, read from `/props` rather than
  /// from the request: a relaunch under different memory pressure resolves
  /// `--fit` differently, and the engine accepts state whose positions no
  /// longer fit in ways that only show up as a wrong answer later.
  pub ctx: Option<u32>,
  /// Unix seconds when the save returned. Prune orders by it.
  pub saved_at: u64,
}

impl Sidecar {
  /// The fingerprint of a live launch, to compare saves against. `None` when
  /// the model file cannot be stat'ed (deleted mid-flight): saves are then
  /// simply never matched.
  fn for_launch(params: &LaunchParams) -> Option<Self> {
    let meta = std::fs::metadata(&params.model_path).ok()?;
    Some(Self {
      slot: 0, // filled by the caller per saved slot
      model_path: crate::util::paths::canonicalize(&params.model_path)
        .unwrap_or_else(|_| params.model_path.clone())
        .display()
        .to_string(),
      model_len: meta.len(),
      model_mtime: meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0),
      mode: params.mode.label().to_string(),
      ctx: params.ctx,
      saved_at: 0, // filled by the caller at save time
    })
  }

  /// Whether `self` (a save) may be restored into the launch `base` describes.
  fn matches(&self, base: &Self) -> bool {
    self.model_path == base.model_path
      && self.model_len == base.model_len
      && self.model_mtime == base.model_mtime
      && self.mode == base.mode
      && self.ctx == base.ctx
  }
}

/// The slotsave root: `<cache_dir>/slots`. `None` only when platform dirs are
/// unavailable (bare test hosts).
pub fn root() -> Option<PathBuf> {
  crate::util::paths::cache_dir().map(|d| d.join("slots"))
}

/// A fresh, launch-unique save dir, created eagerly because llama-server
/// rejects `--slot-save-path` values that are not existing directories. The
/// name carries the daemon pid and a microsecond clock, so concurrent launches
/// and relaunches of the same model never share a directory.
pub fn create_save_dir() -> Option<PathBuf> {
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .ok()?
    .as_micros();
  let dir = root()?.join(format!("{}-{now}", std::process::id()));
  std::fs::create_dir_all(&dir).ok()?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
  }
  Some(dir)
}

/// One slot as `GET /slots` reports it. `n_ctx` is present for every slot on
/// current builds and unused here; the id is all the save/restore calls need.
async fn list_slots(port: u16) -> Result<Vec<u32>, String> {
  let (status, body) = request(port, "GET", "/slots", None, SLOT_LIST_TIMEOUT).await?;
  if status != 200 {
    return Err(format!("/slots returned {status}"));
  }
  let v: serde_json::Value =
    serde_json::from_str(&body).map_err(|e| format!("/slots unparseable: {e}"))?;
  let slots = v.as_array().ok_or("/slots not an array")?;
  Ok(
    slots
      .iter()
      .filter_map(|s| s.get("id").and_then(|i| i.as_u64()))
      .map(|i| i as u32)
      .collect(),
  )
}

/// POST one slot action. `filename` is resolved by the engine inside its own
/// `--slot-save-path`.
async fn slot_action(
  port: u16,
  slot: u32,
  action: &str,
  filename: &str,
  timeout: Duration,
) -> Result<serde_json::Value, String> {
  let body = serde_json::json!({ "filename": filename }).to_string();
  let path = format!("/slots/{slot}?action={action}");
  let (status, resp) = request(port, "POST", &path, Some(&body), timeout).await?;
  let v: serde_json::Value =
    serde_json::from_str(&resp).map_err(|e| format!("{path} unparseable: {e}"))?;
  if status != 200 {
    let msg = v
      .pointer("/error/message")
      .and_then(|m| m.as_str())
      .unwrap_or("unknown error");
    return Err(format!("{path} returned {status}: {msg}"));
  }
  Ok(v)
}

/// The fingerprint a save must match to be restorable into this launch, with
/// `ctx` set to the window the *engine* resolved (`/props`) rather than the one
/// requested: a `--fit` launch requests nothing, and state saved under a window
/// the relaunch cannot provide is rejected by the engine in ways better refused
/// here. `None` — and so a launch that touches no save files at all — when the
/// model file is gone or `/props` will not answer; a server that cannot report
/// its own context window is not in a state to have its slots rewritten.
async fn fingerprint(port: u16, params: &LaunchParams) -> Option<Sidecar> {
  let mut base = Sidecar::for_launch(params)?;
  let resolved = super::actuals::fetch_props_actuals(port, SLOT_LIST_TIMEOUT)
    .await
    .resolved_ctx;
  base.ctx = resolved.or(params.ctx);
  base.ctx.is_some().then_some(base)
}

/// Save every non-empty slot of the launch on `port` into `dir`.
///
/// Best-effort by design: this runs inline before an eviction stop (make-room
/// is holding a request), so any per-slot failure is logged and skipped. A
/// slot with no cached prompt answers `n_saved: 0` with a header-only file;
/// that stub is removed and the slot skipped.
pub async fn save(port: u16, params: &LaunchParams, dir: &Path, max_bytes: u64) {
  let Some(root) = root() else { return };
  save_at(&root, port, params, dir, max_bytes).await;
}

/// [`save`] with the tree to prune after saving passed in, so a test drives the
/// whole save/prune pair against a fixture directory instead of the real cache.
async fn save_at(root: &Path, port: u16, params: &LaunchParams, dir: &Path, max_bytes: u64) {
  let Some(base) = fingerprint(port, params).await else {
    log::debug!(
      "slot cache: no fingerprint for {}; nothing to save",
      params.model_path.display()
    );
    return;
  };
  let slots = match list_slots(port).await {
    Ok(s) => s,
    Err(e) => {
      log::warn!("slot cache: cannot list slots on :{port}: {e}");
      return;
    }
  };
  let mut saved = 0usize;
  let mut bytes = 0u64;
  for slot in slots {
    let tmp = format!("save-{slot}-{}.tmp", unix_secs());
    match slot_action(port, slot, "save", &tmp, SAVE_TIMEOUT).await {
      Err(e) => log::warn!("slot cache: save of slot {slot} on :{port} failed: {e}"),
      Ok(resp) => {
        let n = resp.get("n_saved").and_then(|v| v.as_u64()).unwrap_or(0);
        if n == 0 {
          // Header-only stub of an idle slot; nothing was cached.
          let _ = std::fs::remove_file(dir.join(&tmp));
          continue;
        }
        let size = std::fs::metadata(dir.join(&tmp))
          .map(|m| m.len())
          .unwrap_or(0);
        let sidecar = Sidecar {
          slot,
          saved_at: unix_secs(),
          ..base.clone()
        };
        if rename_pair(dir, &tmp, slot).is_err()
          || std::fs::write(
            dir.join(sidecar_name(slot)),
            serde_json::to_string(&sidecar).unwrap(),
          )
          .is_err()
        {
          log::warn!(
            "slot cache: could not persist save of slot {slot} in {}",
            dir.display()
          );
          let _ = std::fs::remove_file(dir.join(&tmp));
          continue;
        }
        saved += 1;
        bytes += size;
      }
    }
  }
  if saved > 0 {
    log::info!(
      "slot cache: saved {saved} slot(s) ({}) from :{port} for {}",
      crate::launch::admission::human_gib(bytes),
      crate::util::paths::model_display_name(&params.model_path)
    );
  }
  prune_at(root, Some(dir), max_bytes, Duration::ZERO);
}

/// Restore any saves matching this launch into its slots, before the launch is
/// declared Ready. Consumes every pair it attempts (see the module docs).
pub async fn restore(port: u16, params: &LaunchParams, dir: &Path) {
  let Some(root) = root() else { return };
  restore_at(&root, port, params, dir).await;
}

/// [`restore`] with the tree to scan passed in, so a test drives it against a
/// fixture directory instead of the real cache dir.
async fn restore_at(root: &Path, port: u16, params: &LaunchParams, dir: &Path) {
  let Some(base) = fingerprint(port, params).await else {
    return;
  };
  let slots = match list_slots(port).await {
    Ok(s) => s,
    Err(e) => {
      log::warn!("slot cache: cannot list slots on :{port}: {e}");
      return;
    }
  };
  // Newest pair per slot across every older save dir; a launch that was
  // evicted twice keeps only its most recent state.
  let mut newest: std::collections::BTreeMap<u32, (u64, PathBuf)> = Default::default();
  let Ok(dirs) = std::fs::read_dir(root) else {
    return;
  };
  for entry in dirs.flatten() {
    let od = entry.path();
    if od == dir || !od.is_dir() {
      continue;
    }
    let Ok(files) = std::fs::read_dir(&od) else {
      continue;
    };
    for f in files.flatten() {
      let path = f.path();
      let Some(slot) = name_of(&path).and_then(slot_from_json) else {
        continue;
      };
      let Ok(raw) = std::fs::read_to_string(&path) else {
        continue;
      };
      let Ok(sidecar) = serde_json::from_str::<Sidecar>(&raw) else {
        let _ = std::fs::remove_file(&path);
        continue;
      };
      if !od.join(bin_name(slot)).is_file() {
        continue;
      }
      // Left in place when it is not ours: this sweep walks every launch's
      // directory, and the save belongs to whichever launch *can* read it.
      // Deleting a foreign pair here would mean any other model's relaunch
      // wipes the conversation that is waiting for its own.
      if !sidecar.matches(&base) {
        continue;
      }
      // The relaunch has fewer slots than this launch had (a smaller `-np`):
      // the conversation has nowhere to land, and only now is it certain the
      // pair is ours to discard.
      if !slots.contains(&slot) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(od.join(bin_name(slot)));
        continue;
      }
      match newest.get(&slot) {
        Some((saved_at, _)) if *saved_at >= sidecar.saved_at => {}
        _ => {
          newest.insert(slot, (sidecar.saved_at, od.join(bin_name(slot))));
        }
      }
    }
  }
  if newest.is_empty() {
    return;
  }
  let t0 = std::time::Instant::now();
  let mut restored = 0usize;
  for (slot, (_, src)) in newest {
    // The engine resolves filenames inside this launch's own save dir, so
    // the pair travels there first (same filesystem: a rename, not a copy).
    let dest = dir.join(bin_name(slot));
    if std::fs::rename(&src, &dest).is_err() {
      if copy_file(&src, &dest).is_err() {
        log::warn!("slot cache: could not stage {} for restore", src.display());
        continue;
      }
      let _ = std::fs::remove_file(&src);
    }
    let _ = std::fs::remove_file(src.with_extension("json"));
    match slot_action(port, slot, "restore", &bin_name(slot), RESTORE_TIMEOUT).await {
      Ok(resp) => {
        let n = resp.get("n_restored").and_then(|v| v.as_u64()).unwrap_or(0);
        log::debug!("slot cache: restored slot {slot} ({n} tokens) on :{port}");
        restored += 1;
      }
      Err(e) => log::warn!("slot cache: restore of slot {slot} on :{port} failed: {e}"),
    }
    // Consumed either way: a good file is now in the server, a bad one will
    // not improve.
    let _ = std::fs::remove_file(&dest);
  }
  if restored > 0 {
    log::info!(
      "slot cache: restored {restored} slot(s) into :{port} for {} in {:.1}s",
      crate::util::paths::model_display_name(&params.model_path),
      t0.elapsed().as_secs_f64()
    );
  }
}

/// Drop save pairs older than `max_age` (disabled when `max_age` is zero),
/// then trim the whole slots tree to `max_bytes` by dropping the oldest pairs
/// first. The pairs in `keep` (the launch that just saved) are never dropped;
/// empty dirs go with whatever is removed.
pub fn prune(keep: Option<&Path>, max_bytes: u64, max_age: Duration) {
  let Some(root) = root() else { return };
  prune_at(&root, keep, max_bytes, max_age);
}

fn prune_at(root: &Path, keep: Option<&Path>, max_bytes: u64, max_age: Duration) {
  let cutoff = unix_secs().saturating_sub(max_age.as_secs());
  // (saved_at, bytes, dir, slot) — one entry per complete pair.
  let mut pairs: Vec<(u64, u64, PathBuf, u32)> = Vec::new();
  for dir in dirs_of(root) {
    let Ok(files) = std::fs::read_dir(&dir) else {
      continue;
    };
    let mut bins: BTreeMap<u32, u64> = Default::default();
    let mut jsons: BTreeMap<u32, u64> = Default::default();
    for f in files.flatten() {
      let path = f.path();
      let Some(name) = name_of(&path) else {
        continue;
      };
      let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
      if let Some(slot) = slot_from_bin(name) {
        bins.insert(slot, len);
        continue;
      }
      if let Some(slot) = slot_from_json(name) {
        jsons.insert(slot, len);
        continue;
      }
      // A temp the engine wrote and the rename never claimed (a crash, or a
      // slot that answered `n_saved: 0` and was skipped).
      if name.starts_with("save-") {
        let _ = std::fs::remove_file(&path);
      }
    }
    for (slot, len) in &bins {
      match jsons.get(slot).and_then(|_| read_sidecar(&dir, *slot)) {
        Some(saved_at) => pairs.push((saved_at, *len, dir.clone(), *slot)),
        // No usable sidecar: either mid-restore in a live launch's dir (the
        // sidecar goes first, the state file follows within seconds) or junk.
        // Age decides.
        None if file_age_secs(&dir.join(bin_name(*slot))) > STAGED_MAX_AGE.as_secs() => {
          let _ = std::fs::remove_file(dir.join(bin_name(*slot)));
        }
        None => {}
      }
    }
    for slot in jsons.keys().filter(|s| !bins.contains_key(s)) {
      let _ = std::fs::remove_file(dir.join(sidecar_name(*slot)));
    }
  }
  pairs.sort_by_key(|(saved_at, _, _, _)| *saved_at);
  let mut total: u64 = pairs.iter().map(|(_, len, _, _)| *len).sum();
  for (saved_at, len, dir, slot) in pairs {
    if keep.is_some_and(|k| k == dir) {
      continue;
    }
    let stale = max_age != Duration::ZERO && saved_at < cutoff;
    if !stale && total <= max_bytes {
      continue;
    }
    let _ = std::fs::remove_file(dir.join(bin_name(slot)));
    let _ = std::fs::remove_file(dir.join(sidecar_name(slot)));
    total -= len;
  }
  for dir in dirs_of(root) {
    if !keep.is_some_and(|k| k == dir)
      && std::fs::read_dir(&dir).map_or(true, |mut r| r.next().is_none())
    {
      let _ = std::fs::remove_dir(&dir);
    }
  }
}

/// A pair's save stamp, or `None` when the sidecar is unreadable: an
/// unidentifiable save is junk, not a candidate.
fn read_sidecar(dir: &Path, slot: u32) -> Option<u64> {
  let raw = std::fs::read_to_string(dir.join(sidecar_name(slot))).ok()?;
  serde_json::from_str::<Sidecar>(&raw)
    .ok()
    .map(|s| s.saved_at)
}

fn file_age_secs(path: &Path) -> u64 {
  std::fs::metadata(path)
    .and_then(|m| m.modified())
    .ok()
    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    .map(|d| unix_secs().saturating_sub(d.as_secs()))
    .unwrap_or(u64::MAX)
}

/// Drop stale pairs and empty dirs at daemon start, ignoring the byte cap
/// (`max_bytes` acts only as a ceiling above the age cut).
pub fn prune_at_boot(max_bytes: u64) {
  prune(None, max_bytes, PAIR_MAX_AGE);
}

fn dirs_of(root: &Path) -> Vec<PathBuf> {
  std::fs::read_dir(root)
    .map(|entries| {
      entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
    })
    .unwrap_or_default()
}

fn rename_pair(dir: &Path, tmp: &str, slot: u32) -> std::io::Result<()> {
  std::fs::rename(dir.join(tmp), dir.join(bin_name(slot)))
}

fn copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
  std::fs::copy(from, to).map(|_| ())
}

fn unix_secs() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0)
}

/// One loopback request/response with `Connection: close`, read to EOF.
/// Returns the status code and the body.
async fn request(
  port: u16,
  method: &str,
  path: &str,
  body: Option<&str>,
  timeout: Duration,
) -> Result<(u16, String), String> {
  let work = async {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
      .await
      .map_err(|e| format!("connect :{port}: {e}"))?;
    let mut head =
      format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(b) = body {
      head.push_str("Content-Type: application/json\r\n");
      head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream
      .write_all(head.as_bytes())
      .await
      .map_err(|e| format!("write :{port}: {e}"))?;
    if let Some(b) = body {
      stream
        .write_all(b.as_bytes())
        .await
        .map_err(|e| format!("write body :{port}: {e}"))?;
    }
    let mut raw = Vec::new();
    stream
      .read_to_end(&mut raw)
      .await
      .map_err(|e| format!("read :{port}: {e}"))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head_part, body_part) = text
      .split_once("\r\n\r\n")
      .ok_or_else(|| "malformed response".to_string())?;
    let status = head_part
      .split_whitespace()
      .nth(1)
      .and_then(|s| s.parse::<u16>().ok())
      .ok_or_else(|| "no status line".to_string())?;
    Ok((status, body_part.to_string()))
  };
  tokio::time::timeout(timeout, work)
    .await
    .map_err(|_| format!("timeout after {timeout:?}"))?
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::util::test_temp::unique_temp_dir;
  use std::sync::{Arc, Mutex};

  fn params(path: &Path) -> LaunchParams {
    LaunchParams::new(path.to_path_buf(), crate::launch::mode::LaunchMode::Chat)
  }

  fn sidecar(slot: u32, saved_at: u64) -> Sidecar {
    Sidecar {
      slot,
      model_path: "/m/model.gguf".into(),
      model_len: 1,
      model_mtime: 1,
      mode: "chat".into(),
      ctx: Some(4096),
      saved_at,
    }
  }

  /// The shape a live save leaves in a directory: a state file of `len` bytes
  /// plus its sidecar, stamped `saved_at`.
  fn write_pair(dir: &Path, slot: u32, saved_at: u64, len: usize) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(bin_name(slot)), vec![b'x'; len]).unwrap();
    std::fs::write(
      dir.join(sidecar_name(slot)),
      serde_json::to_vec(&sidecar(slot, saved_at)).unwrap(),
    )
    .unwrap();
  }

  fn names_in(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
      .map(|entries| {
        entries
          .flatten()
          .filter_map(|f| name_of(&f.path()).map(str::to_string))
          .collect()
      })
      .unwrap_or_default();
    out.sort();
    out
  }

  fn touch_ago(path: &Path, secs: u64) {
    let then = SystemTime::now() - Duration::from_secs(secs);
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(then).unwrap();
  }

  // --- the fake engine ------------------------------------------------------

  /// What a `llama-server` answers for the slot API: `/props`, `/slots`, and
  /// the two actions, each writing or reading inside its own save dir. Shapes
  /// captured from llama.cpp b11457 (spike doc).
  struct Fake {
    save_dir: PathBuf,
    n_ctx: u64,
    /// Reported slots with the token count each one saves (`0` for an idle
    /// slot, which still produces a header-only file).
    saved: Vec<(u32, u64)>,
    hits: Mutex<Vec<String>>,
  }

  impl Fake {
    fn new(save_dir: PathBuf, n_ctx: u64, saved: Vec<(u32, u64)>) -> Arc<Self> {
      Arc::new(Self {
        save_dir,
        n_ctx,
        saved,
        hits: Mutex::new(Vec::new()),
      })
    }
  }

  async fn fake_engine(fake: Arc<Fake>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
      loop {
        let Ok((sock, _)) = listener.accept().await else {
          return;
        };
        let fake = fake.clone();
        tokio::spawn(async move {
          let _ = serve(sock, fake).await;
        });
      }
    });
    port
  }

  async fn serve(mut sock: TcpStream, fake: Arc<Fake>) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    let head_end = loop {
      if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        break i;
      }
      let n = sock.read(&mut chunk).await?;
      if n == 0 {
        return Ok(());
      }
      buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let clen = head
      .lines()
      .find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
          .eq_ignore_ascii_case("content-length")
          .then(|| v.trim().parse::<usize>().ok())
          .flatten()
      })
      .unwrap_or(0);
    while buf.len() < head_end + 4 + clen {
      let n = sock.read(&mut chunk).await?;
      if n == 0 {
        break;
      }
      buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end + 4..head_end + 4 + clen]).into_owned();
    let line = head.lines().next().unwrap_or_default().to_string();
    let (status, out) = route(&fake, &line, &body);
    let resp = format!(
      "HTTP/1.1 {status} Resp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
      out.len()
    );
    sock.write_all(resp.as_bytes()).await
  }

  fn slot_of(path: &str) -> u32 {
    path
      .trim_start_matches("/slots/")
      .split('?')
      .next()
      .and_then(|s| s.parse().ok())
      .unwrap_or(u32::MAX)
  }

  fn route(fake: &Fake, line: &str, body: &str) -> (u16, String) {
    let mut words = line.split_whitespace();
    let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let filename = serde_json::from_str::<serde_json::Value>(body)
      .ok()
      .and_then(|v| {
        v.get("filename")
          .and_then(|f| f.as_str())
          .map(str::to_string)
      })
      .unwrap_or_default();
    match (method, path) {
      ("GET", "/props") => (
        200,
        format!(
          "{{\"default_generation_settings\":{{\"n_ctx\":{}}}}}",
          fake.n_ctx
        ),
      ),
      ("GET", "/slots") => (
        200,
        format!(
          "[{}]",
          fake
            .saved
            .iter()
            .map(|(id, _)| format!("{{\"id\":{id}}}"))
            .collect::<Vec<_>>()
            .join(",")
        ),
      ),
      ("POST", p) if p.starts_with("/slots/") && p.ends_with("?action=save") => {
        let slot = slot_of(p);
        let n = fake
          .saved
          .iter()
          .find(|(id, _)| *id == slot)
          .map(|(_, n)| *n)
          .unwrap_or(0);
        std::fs::write(fake.save_dir.join(&filename), vec![b'k'; n as usize]).unwrap();
        fake
          .hits
          .lock()
          .unwrap()
          .push(format!("save {slot} {filename}"));
        (
          200,
          format!("{{\"id_slot\":{slot},\"n_saved\":{n},\"n_written\":{n}}}"),
        )
      }
      ("POST", p) if p.starts_with("/slots/") && p.ends_with("?action=restore") => {
        let slot = slot_of(p);
        fake
          .hits
          .lock()
          .unwrap()
          .push(format!("restore {slot} {filename}"));
        if fake.save_dir.join(&filename).is_file() {
          (200, format!("{{\"id_slot\":{slot},\"n_restored\":128}}"))
        } else {
          (
            400,
            "{\"error\":{\"message\":\"No available space in KV cache or invalid slot save file\"}}"
              .into(),
          )
        }
      }
      _ => (404, "{}".into()),
    }
  }

  // --- fingerprint ----------------------------------------------------------

  #[test]
  fn file_names_round_trip() {
    assert_eq!(slot_from_bin("slot-12.bin"), Some(12));
    assert_eq!(slot_from_bin("slot-1.json"), None);
    assert_eq!(slot_from_json("slot-1.json"), Some(1));
    assert_eq!(slot_from_json("slot-1.bin"), None);
    assert_eq!(bin_name(3), "slot-3.bin");
    assert_eq!(sidecar_name(3), "slot-3.json");
  }

  #[test]
  fn a_match_needs_every_fingerprint_field() {
    let tmp = unique_temp_dir("slotcache-match");
    let model = tmp.join("m.gguf");
    std::fs::write(&model, b"weights").unwrap();
    let p = params(&model);
    let base = Sidecar::for_launch(&p).unwrap();

    let same = Sidecar {
      slot: 5,
      saved_at: 99,
      ..base.clone()
    };
    assert!(
      same.matches(&base),
      "slot and saved_at are not fingerprint fields"
    );

    let bigger_window = Sidecar {
      ctx: Some(8192),
      ..base.clone()
    };
    assert!(
      !bigger_window.matches(&base),
      "state saved in another context window must not be fed to the engine"
    );

    std::fs::write(&model, b"different weights").unwrap();
    assert!(
      !Sidecar::for_launch(&p).unwrap().matches(&base),
      "a replaced model file must not restore the old KV"
    );
    let _ = std::fs::remove_dir_all(&tmp);
  }

  // --- prune ----------------------------------------------------------------

  #[test]
  fn prune_drops_the_oldest_pairs_first_until_under_the_cap() {
    let tmp = unique_temp_dir("slotcache-prune");
    let (old, new) = (tmp.join("100"), tmp.join("200"));
    write_pair(&old, 0, 10, 100);
    write_pair(&new, 0, 20, 100);
    prune_at(&tmp, None, 150, Duration::ZERO);
    assert!(names_in(&old).is_empty(), "the older pair goes first");
    assert_eq!(
      names_in(&new).len(),
      2,
      "the newer pair is still under the cap"
    );
    assert!(
      !old.is_dir(),
      "a drained directory is removed with its pair"
    );
    let _ = std::fs::remove_dir_all(&tmp);
  }

  #[test]
  fn prune_never_touches_the_directory_that_just_saved() {
    let tmp = unique_temp_dir("slotcache-keep");
    let (old, fresh) = (tmp.join("100"), tmp.join("200"));
    write_pair(&old, 0, 10, 100);
    write_pair(&fresh, 0, 20, 100);
    // A cap nothing can fit under: everything but the just-saved dir must go.
    prune_at(&tmp, Some(&fresh), 1, Duration::ZERO);
    assert_eq!(names_in(&fresh).len(), 2);
    assert!(names_in(&old).is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
  }

  #[test]
  fn prune_at_boot_drops_a_week_old_state() {
    let tmp = unique_temp_dir("slotcache-age");
    let (stale, fresh) = (tmp.join("100"), tmp.join("200"));
    write_pair(&stale, 0, unix_secs() - PAIR_MAX_AGE.as_secs() - 1, 10);
    write_pair(&fresh, 0, unix_secs(), 10);
    prune_at(&tmp, None, u64::MAX, PAIR_MAX_AGE);
    assert!(names_in(&stale).is_empty());
    assert_eq!(names_in(&fresh).len(), 2);
    let _ = std::fs::remove_dir_all(&tmp);
  }

  #[test]
  fn prune_clears_temps_and_halves_of_broken_pairs() {
    let tmp = unique_temp_dir("slotcache-junk");
    let dir = tmp.join("100");
    std::fs::create_dir_all(&dir).unwrap();
    // A save interrupted between the engine writing and the rename.
    std::fs::write(dir.join("save-3-1.tmp"), b"partial").unwrap();
    // A sidecar whose state file is gone.
    std::fs::write(
      dir.join(sidecar_name(7)),
      serde_json::to_vec(&sidecar(7, unix_secs())).unwrap(),
    )
    .unwrap();
    // A state file staged for a restore that never finished.
    std::fs::write(dir.join(bin_name(9)), b"staged").unwrap();
    touch_ago(&dir.join(bin_name(9)), STAGED_MAX_AGE.as_secs() + 1);
    // A restore in flight: staged seconds ago, so it must be left alone.
    std::fs::write(dir.join(bin_name(8)), b"staged").unwrap();
    write_pair(&dir, 0, unix_secs(), 10);

    prune_at(&tmp, None, u64::MAX, Duration::ZERO);

    assert_eq!(
      names_in(&dir),
      ["slot-0.bin", "slot-0.json", "slot-8.bin"],
      "temps, orphan halves and a stale staged file go; a live restore stays"
    );
    let _ = std::fs::remove_dir_all(&tmp);
  }

  // --- save / restore -------------------------------------------------------

  #[tokio::test]
  async fn a_save_becomes_a_restore_on_the_next_launch() {
    let tmp = unique_temp_dir("slotcache-roundtrip");
    let model = tmp.join("m.gguf");
    std::fs::write(&model, b"weights").unwrap();
    let p = params(&model);
    let root = tmp.join("slots");
    let (evicted, relaunch) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&evicted).unwrap();

    // Slot 1 is idle: the engine writes a stub and answers `n_saved: 0`, and
    // nothing of it may be kept.
    let first = Fake::new(evicted.clone(), 4096, vec![(0, 64), (1, 0)]);
    let evicted_port = fake_engine(first.clone()).await;
    save_at(&root, evicted_port, &p, &evicted, u64::MAX).await;
    assert_eq!(
      names_in(&evicted),
      ["slot-0.bin", "slot-0.json"],
      "one pair per non-idle slot, named by slot id"
    );

    let second = Fake::new(relaunch.clone(), 4096, vec![(0, 0), (1, 0)]);
    let later_port = fake_engine(second.clone()).await;
    std::fs::create_dir_all(&relaunch).unwrap();
    restore_at(&root, later_port, &p, &relaunch).await;
    assert_eq!(
      second.hits.lock().unwrap().as_slice(),
      ["restore 0 slot-0.bin"],
      "the file travels into the live launch's own dir before the POST"
    );
    assert!(
      names_in(&relaunch).is_empty(),
      "a consumed save leaves nothing behind"
    );
    assert!(names_in(&evicted).is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
  }

  #[tokio::test]
  async fn a_pair_from_another_launch_is_left_for_its_own_model() {
    let tmp = unique_temp_dir("slotcache-mismatch");
    let model = tmp.join("m.gguf");
    std::fs::write(&model, b"weights").unwrap();
    let p = params(&model);
    let root = tmp.join("slots");
    let (evicted, relaunch) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&evicted).unwrap();

    let first = Fake::new(evicted.clone(), 4096, vec![(0, 64)]);
    let evicted_port = fake_engine(first).await;
    save_at(&root, evicted_port, &p, &evicted, u64::MAX).await;
    assert_eq!(names_in(&evicted).len(), 2);

    // The relaunch resolved a smaller window than the save was made under, so
    // the engine would reject the file. It belongs to a launch that could still
    // read it, so this one must not restore it and must not delete it either:
    // every other model's relaunch would otherwise wipe the saves waiting for
    // its own return.
    let second = Fake::new(relaunch.clone(), 2048, vec![(0, 0)]);
    let relaunch_port = fake_engine(second.clone()).await;
    std::fs::create_dir_all(&relaunch).unwrap();
    restore_at(&root, relaunch_port, &p, &relaunch).await;
    assert!(
      second.hits.lock().unwrap().is_empty(),
      "no restore is attempted for a pair that cannot fit"
    );
    assert_eq!(
      names_in(&evicted).len(),
      2,
      "another launch's pair survives this one untouched"
    );
    let _ = std::fs::remove_dir_all(&tmp);
  }

  #[tokio::test]
  async fn a_save_survives_until_a_launch_with_the_same_slots_arrives() {
    let tmp = unique_temp_dir("slotcache-fewerslots");
    let model = tmp.join("m.gguf");
    std::fs::write(&model, b"weights").unwrap();
    let p = params(&model);
    let root = tmp.join("slots");
    let (evicted, relaunch) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&evicted).unwrap();

    let first = Fake::new(evicted.clone(), 4096, vec![(0, 64), (1, 64)]);
    let evicted_port = fake_engine(first).await;
    save_at(&root, evicted_port, &p, &evicted, u64::MAX).await;
    assert_eq!(names_in(&evicted).len(), 4);

    // The relaunch came up with one slot (a smaller `-np`): slot 1's
    // conversation has nowhere to land, and its files must go.
    let second = Fake::new(relaunch.clone(), 4096, vec![(0, 0)]);
    let later_port = fake_engine(second.clone()).await;
    std::fs::create_dir_all(&relaunch).unwrap();
    restore_at(&root, later_port, &p, &relaunch).await;
    assert_eq!(
      second.hits.lock().unwrap().as_slice(),
      ["restore 0 slot-0.bin"],
      "the slot the relaunch has is still restored"
    );
    assert!(
      names_in(&evicted).is_empty(),
      "slot 1's pair goes during the scan: that conversation has nowhere to land"
    );
    let _ = std::fs::remove_dir_all(&tmp);
  }
}
