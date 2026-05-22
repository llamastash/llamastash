//! HuggingFace pull (Unit 9, R65).
//!
//! Backs `llamastash pull <repo>` standalone and the init wizard's
//! model step. v2 uses the `hf-hub` crate (0.5 line) for the HF
//! listing + download path. hf-hub resolves the same `reqwest 0.12`
//! we pin elsewhere, so adopting it does not introduce a duplicate
//! reqwest in the dep tree.
//!
//! Fetch-contract carve-out: hf-hub builds its own `reqwest::Client`,
//! which means HF traffic does not flow through [`FetchClient`]'s
//! redirect-cap / body-cap / host-allowlist gates. We accept that —
//! hf-hub talks only to `huggingface.co` (and its LFS CDN via the
//! resolve-endpoint redirect chain), so the host scope is already
//! constrained. GH Releases install and benchmark snapshot fetches
//! continue to go through `FetchClient`.
//!
//! Cache layout matches `huggingface_hub` (Python) and what
//! `discovery::known_caches` scans:
//! `~/.cache/huggingface/hub/models--<owner>--<repo>/snapshots/<rev>/<file>`,
//! with blobs stored under `blobs/<etag>` and snapshot paths as
//! symlinks. hf-hub writes exactly that layout, so subsequent
//! `llamastash list` rescans dedupe via the existing discovery path
//! (R62).

use std::path::{Path, PathBuf};

use hf_hub::{
  api::tokio::{Api, ApiBuilder},
  Repo, RepoType,
};

use crate::cli::cli_args::{Cli, PullArgs};
use crate::cli::exit_codes::{CliExit, CliResult, INIT_DOWNLOAD_FAILED, PULL_FAILED};
use crate::config::Config;
use crate::init::fetch::{FetchClient, FetchError};

/// Disk-space headroom required *on top of* the estimated download
/// size (R64). 1 GiB matches the brainstorm spec.
pub const DISK_HEADROOM_BYTES: u64 = 1024 * 1024 * 1024;

/// Max bytes per per-file download. 64 GiB accommodates frontier
/// single-file GGUFs (e.g. 27B Q8_0 ≈ 29 GB, 70B Q4_K_M ≈ 43 GB when
/// not sharded). Enforced via hf-hub's `Api::metadata` HEAD before
/// each download; the cap is a safety net against runaway metadata,
/// not a model-size policy.
pub const PER_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Default HF endpoint root. Overridable via `HF_ENDPOINT` env to
/// match `huggingface_hub`'s convention, but only to hosts on
/// [`HF_HOST_ALLOWLIST`] — the bearer-token-carrying client must not
/// be tricked into talking to attacker-controlled origins.
pub const DEFAULT_HF_ENDPOINT: &str = "https://huggingface.co";

/// Hosts an explicit `HF_ENDPOINT` override may resolve to. A
/// non-allowlisted value is treated as misconfiguration, not a silent
/// downgrade, so the wizard surfaces it as an actionable error before
/// the `HF_TOKEN` ever leaves the process. Subdomains of
/// `huggingface.co` are accepted (CDN endpoints under that suffix).
pub const HF_HOST_ALLOWLIST: &[&str] = &["huggingface.co"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSpec {
  pub repo_id: String,
  pub pinned_filename: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
  #[error("`{0}` is not an `owner/repo` HuggingFace id")]
  BadRepoSpec(String),
  #[error("HF_TOKEN file `{}` is mode {mode:o}; refuse to use (run `chmod 600 {}`)", path.display(), path.display())]
  TokenFileTooOpen { path: PathBuf, mode: u32 },
  #[error("offline mode (LLAMASTASH_OFFLINE / --offline); cannot pull from HuggingFace")]
  Offline,
  #[error(
    "free disk space {available_bytes} bytes < estimated {needed_bytes} \
     (download + 1 GiB headroom)"
  )]
  InsufficientDisk {
    available_bytes: u64,
    needed_bytes: u64,
  },
  #[error("HF tree listing for `{repo}` returned zero matching files")]
  NoMatchingFiles { repo: String },
  #[error("HF file `{file}` exceeds per-file cap of {cap} bytes (size {size})")]
  FileTooLarge { file: String, size: u64, cap: u64 },
  #[error("hf-hub: {0}")]
  Hub(#[from] hf_hub::api::tokio::ApiError),
  #[error("fetch contract: {0}")]
  Fetch(#[from] FetchError),
  #[error("I/O: {0}")]
  Io(String),
  #[error("could not resolve a HuggingFace cache directory")]
  NoCacheDir,
  #[error(
    "HF_ENDPOINT=`{value}` is not allowed; expected `https://` on an allowlisted host \
     ({allowlist}) — unset HF_ENDPOINT to use the default ({default})"
  )]
  EndpointNotAllowed {
    value: String,
    allowlist: String,
    default: &'static str,
  },
}

/// Outcome of a successful download.
#[derive(Debug, Clone, Default)]
pub struct DownloadResult {
  pub paths: Vec<PathBuf>,
  pub total_bytes: u64,
  pub revision: String,
  /// HuggingFace repo id the files actually came from. Equal to the
  /// caller's `spec.repo_id` for normal downloads; differs when a
  /// synthetic-row fallback resolved to `bartowski/...` (etc.) instead.
  pub resolved_repo_id: String,
}

impl RepoSpec {
  pub fn parse(raw: &str) -> Result<Self, DownloadError> {
    let (repo_part, filename_part) = match raw.split_once(':') {
      Some((r, f)) => (r, Some(f.to_string())),
      None => (raw, None),
    };
    if !repo_part.contains('/') || repo_part.starts_with('/') || repo_part.ends_with('/') {
      return Err(DownloadError::BadRepoSpec(raw.to_string()));
    }
    let (owner, name) = repo_part
      .split_once('/')
      .ok_or_else(|| DownloadError::BadRepoSpec(raw.to_string()))?;
    if owner.is_empty() || name.is_empty() {
      return Err(DownloadError::BadRepoSpec(raw.to_string()));
    }
    if let Some(f) = filename_part.as_deref() {
      if f.is_empty() || f.contains('/') || f.contains('\\') || f.contains("..") {
        return Err(DownloadError::BadRepoSpec(raw.to_string()));
      }
    }
    Ok(RepoSpec {
      repo_id: repo_part.to_string(),
      pinned_filename: filename_part,
    })
  }

  pub fn owner_name(&self) -> Option<(&str, &str)> {
    self.repo_id.split_once('/')
  }
}

/// Resolve the HuggingFace token. Priority: `HF_TOKEN` env →
/// `~/.cache/huggingface/token` (refused if mode lets group or world
/// read it on Unix).
pub fn resolve_hf_token() -> Result<Option<String>, DownloadError> {
  if let Ok(v) = std::env::var("HF_TOKEN") {
    let trimmed = v.trim().to_string();
    if !trimmed.is_empty() {
      return Ok(Some(trimmed));
    }
  }
  let home = crate::util::paths::home_dir();
  let token_path = match home {
    Some(h) => h.join(".cache/huggingface/token"),
    None => return Ok(None),
  };
  let meta = match std::fs::metadata(&token_path) {
    Ok(m) => m,
    Err(_) => return Ok(None),
  };
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
      return Err(DownloadError::TokenFileTooOpen {
        path: token_path.clone(),
        mode,
      });
    }
  }
  let _ = meta;
  let body = std::fs::read_to_string(&token_path).map_err(|e| DownloadError::Io(e.to_string()))?;
  let trimmed = body.trim().to_string();
  if trimmed.is_empty() {
    Ok(None)
  } else {
    Ok(Some(trimmed))
  }
}

/// Options the wizard / standalone pull both feed in.
#[derive(Clone, Default)]
pub struct DownloadOptions {
  /// `.gguf` by default; the wizard's "skip non-weights" rule.
  pub extension_filter: Option<String>,
  /// Estimated size for the R64 precheck. `None` skips precheck.
  pub estimated_bytes: Option<u64>,
  /// Optional callback fired after the repo listing + HEAD probes
  /// resolve, and again at the start/end of each per-file download.
  /// `None` is the default — `download_repo` runs silently. The
  /// wizard wires this to its `StepProgress` so users see "Downloading
  /// foo.gguf (425 MiB) → Downloaded foo.gguf" instead of one long
  /// silent await.
  pub progress: Option<std::sync::Arc<dyn DownloadProgress>>,
  /// Pin the HuggingFace revision (commit SHA, branch, or tag) used
  /// when resolving files. `None` resolves the default branch (`main`),
  /// preserving the pre-`--revision` behavior byte-for-byte. Threaded
  /// through to `hf_hub::Repo::with_revision` so the downloaded file
  /// set matches the supplied identifier exactly.
  pub revision: Option<String>,
  /// Trusted-converter repos to try in order if the primary spec's
  /// repo has no GGUF (typical for "synthetic" snapshot rows that
  /// point at an official org repo shipping only safetensors). Each
  /// fallback is probed with `quant_hint` to pick the matching file
  /// from a multi-quant repo. Empty list (the default) disables the
  /// fallback path — `download_repo` errors as it did before.
  pub fallback_repos: Vec<String>,
  /// Quant tag (e.g. `"Q4_K_M"`) used to select a file from a fallback
  /// repo's listing when the primary `pinned_filename` doesn't match
  /// the fallback's naming convention (bartowski / unsloth /
  /// lmstudio-community each pick different stems). Case-insensitive
  /// substring match against `.gguf` siblings.
  pub quant_hint: Option<String>,
}

impl std::fmt::Debug for DownloadOptions {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DownloadOptions")
      .field("extension_filter", &self.extension_filter)
      .field("estimated_bytes", &self.estimated_bytes)
      .field("progress", &self.progress.as_ref().map(|_| "<callback>"))
      .field("revision", &self.revision)
      .field("fallback_repos", &self.fallback_repos)
      .field("quant_hint", &self.quant_hint)
      .finish()
  }
}

/// Per-file progress hook the wizard can plug into a download so the
/// user sees what's happening file-by-file. All methods take `&self`
/// so the implementation can hold a cliclack spinner internally
/// (cliclack's `ProgressBar` is `Send + Sync` and updates via
/// interior mutability).
pub trait DownloadProgress: Send + Sync {
  /// Fired once after the repo listing + HEAD probes resolve, before
  /// any byte is downloaded. `files` is `(filename, size_bytes)`.
  fn on_files_resolved(&self, files: &[(String, u64)]);
  /// Fired right before each file's download starts.
  fn on_file_started(&self, filename: &str, size: u64, index: usize, total: usize);
  /// Fired right after each file's download completes (cached files
  /// also fire — hf-hub returns the cached path without re-downloading).
  fn on_file_finished(&self, filename: &str, index: usize, total: usize);
  /// Fired by the hf-hub progress adapter on every byte chunk landed
  /// during a non-cached download. `bytes_in_file` is the running
  /// total for the current file (not cumulative across the pull).
  /// Default no-op so existing implementations (the wizard's
  /// cliclack spinner) don't have to opt in.
  fn on_bytes_progress(&self, _filename: &str, _bytes_in_file: u64) {}
}

/// Bridge between hf-hub's chunk-level `Progress` trait and our
/// [`DownloadProgress::on_bytes_progress`] callback. Holds a running
/// byte counter (Arc'd so hf-hub's parallel chunk workers all add to
/// the same total) and forwards every `update` into the caller's
/// progress hook.
#[derive(Clone)]
struct HfHubProgressAdapter {
  filename: String,
  bytes_in_file: std::sync::Arc<std::sync::atomic::AtomicU64>,
  inner: Option<std::sync::Arc<dyn DownloadProgress>>,
}

impl HfHubProgressAdapter {
  fn new(filename: String, inner: Option<std::sync::Arc<dyn DownloadProgress>>) -> Self {
    Self {
      filename,
      bytes_in_file: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
      inner,
    }
  }
}

impl hf_hub::api::tokio::Progress for HfHubProgressAdapter {
  async fn init(&mut self, _size: usize, _filename: &str) {
    self
      .bytes_in_file
      .store(0, std::sync::atomic::Ordering::Relaxed);
  }

  async fn update(&mut self, size: usize) {
    let prev = self
      .bytes_in_file
      .fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
    let cumulative = prev.saturating_add(size as u64);
    if let Some(inner) = &self.inner {
      inner.on_bytes_progress(&self.filename, cumulative);
    }
  }

  async fn finish(&mut self) {}
}

/// Disk-space precheck (R64). Refuses when free < needed + headroom.
pub fn precheck_disk(target_dir: &Path, needed_bytes: u64) -> Result<(), DownloadError> {
  let available = available_bytes(target_dir).unwrap_or(u64::MAX);
  let required = needed_bytes.saturating_add(DISK_HEADROOM_BYTES);
  if available < required {
    return Err(DownloadError::InsufficientDisk {
      available_bytes: available,
      needed_bytes: required,
    });
  }
  Ok(())
}

#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
  use std::ffi::CString;
  use std::os::unix::ffi::OsStrExt;
  let cstr = CString::new(path.as_os_str().as_bytes()).ok()?;
  let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
  let rc = unsafe { libc::statvfs(cstr.as_ptr(), &mut stat) };
  if rc != 0 {
    return None;
  }
  Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> Option<u64> {
  None
}

/// Resolve the HF cache root (`$HF_HOME/hub` → `~/.cache/huggingface/hub`).
/// Matches the path hf-hub's `Cache::default` produces — we pass this
/// explicitly to `ApiBuilder::with_cache_dir` so the dependency on
/// hf-hub's defaults is explicit, not implicit.
pub fn hf_cache_dir() -> Result<PathBuf, DownloadError> {
  if let Some(home) = std::env::var_os("HF_HOME") {
    return Ok(PathBuf::from(home).join("hub"));
  }
  let home = crate::util::paths::home_dir().ok_or(DownloadError::NoCacheDir)?;
  Ok(home.join(".cache/huggingface/hub"))
}

/// Compute the cache folder name for a repo (matches `huggingface_hub`
/// and hf-hub's `Repo::folder_name`).
pub fn repo_folder_name(repo_id: &str) -> String {
  format!("models--{}", repo_id.replace('/', "--"))
}

/// Resolve the HF endpoint, refusing any `HF_ENDPOINT` override that
/// is not HTTPS on an allowlisted host. Returning `Err` here aborts
/// `build_api` before the bearer token is handed to hf-hub's
/// uncontrolled `reqwest::Client`.
pub fn endpoint() -> Result<String, DownloadError> {
  let Ok(raw) = std::env::var("HF_ENDPOINT") else {
    return Ok(DEFAULT_HF_ENDPOINT.to_string());
  };
  validate_endpoint(&raw)?;
  Ok(raw)
}

fn validate_endpoint(raw: &str) -> Result<(), DownloadError> {
  let endpoint_err = || DownloadError::EndpointNotAllowed {
    value: raw.to_string(),
    allowlist: HF_HOST_ALLOWLIST.join(", "),
    default: DEFAULT_HF_ENDPOINT,
  };
  // Route through the shared fetch-policy `check_url` so HF endpoint
  // validation uses the same scheme + host check as `FetchClient`
  // does for every other request. Subdomain matching is enabled
  // because HF redirects to per-region CDN hosts (`cdn-lfs.*`,
  // `us1.cdn-lfs.*`) under the same root.
  let parsed = reqwest::Url::parse(raw).map_err(|_| endpoint_err())?;
  let allowlist = crate::init::fetch_policy::HostAllowlist::from_hosts(HF_HOST_ALLOWLIST.to_vec())
    .with_subdomain_matching(true);
  crate::init::fetch_policy::check_url(&parsed, &allowlist).map_err(|_| endpoint_err())
}

/// Build the per-file URL hf-hub will GET. Kept here so callers
/// reasoning about traffic (tests, logging) don't have to reach into
/// hf-hub's internals.
pub fn file_url(endpoint: &str, repo_id: &str, path: &str) -> String {
  format!("{endpoint}/{repo_id}/resolve/main/{path}")
}

fn build_api(cache_dir: PathBuf) -> Result<Api, DownloadError> {
  let token = resolve_hf_token()?;
  let endpoint = endpoint()?;
  ApiBuilder::new()
    .with_endpoint(endpoint)
    .with_cache_dir(cache_dir)
    .with_token(token)
    .with_user_agent("llamastash", env!("CARGO_PKG_VERSION"))
    .with_progress(false)
    .build()
    .map_err(DownloadError::from)
}

/// Pick which repo files to download given the pinned filename and the
/// `.gguf` extension filter. When a pinned filename has no exact match,
/// expand to the GGUF shard convention `<stem>-NNNNN-of-NNNNN.<ext>`
/// (e.g. `Qwen/Qwen2.5-7B-Instruct-GGUF` only hosts
/// `qwen2.5-7b-instruct-q4_k_m-00001-of-00002.gguf`, never the
/// unsharded basename that the benchmark snapshot records).
/// llama.cpp loads sharded GGUFs natively from the first shard, so the
/// smoke step and config-write step keep working unchanged.
pub(crate) fn select_files(
  all_files: &[String],
  pinned: Option<&str>,
  ext: Option<&str>,
) -> Vec<String> {
  match (pinned, ext) {
    (Some(name), _) => {
      if all_files.iter().any(|f| f == name) {
        return vec![name.to_string()];
      }
      let mut shards = expand_shards(name, all_files);
      shards.sort();
      shards
    }
    (None, Some(ext)) => all_files
      .iter()
      .filter(|p| p.ends_with(ext))
      .cloned()
      .collect(),
    (None, None) => all_files.to_vec(),
  }
}

fn expand_shards(name: &str, all_files: &[String]) -> Vec<String> {
  let (stem, ext) = match name.rfind('.') {
    Some(i) => (&name[..i], &name[i..]),
    None => (name, ""),
  };
  let prefix = format!("{stem}-");
  all_files
    .iter()
    .filter(|f| {
      f.starts_with(&prefix) && f.ends_with(ext) && {
        let mid = &f[prefix.len()..f.len() - ext.len()];
        is_shard_index(mid)
      }
    })
    .cloned()
    .collect()
}

fn is_shard_index(s: &str) -> bool {
  let Some((a, b)) = s.split_once("-of-") else {
    return false;
  };
  !a.is_empty()
    && !b.is_empty()
    && a.chars().all(|c| c.is_ascii_digit())
    && b.chars().all(|c| c.is_ascii_digit())
}

/// Build an `hf-hub` repo handle for `repo_id` at the given revision
/// (or the default branch when `revision` is `None`). Empty revision
/// strings collapse to the default branch — the CLI parser already
/// rejects empty `--revision`, this is defense in depth for direct
/// library callers.
fn build_repo_handle(
  api: &hf_hub::api::tokio::Api,
  repo_id: &str,
  revision: Option<&str>,
) -> hf_hub::api::tokio::ApiRepo {
  match revision {
    Some(sha) if !sha.is_empty() => api.repo(Repo::with_revision(
      repo_id.to_string(),
      RepoType::Model,
      sha.to_string(),
    )),
    _ => api.model(repo_id.to_string()),
  }
}

/// Outcome of repo resolution: the spec to actually download (primary
/// or a fallback substitute), the revision to use (always `None` for
/// fallbacks since their revisions are unrelated to the user-supplied
/// one), the `RepoInfo` from the listing call (so `download_repo` can
/// reuse `info.sha`), and the filtered file list.
type ResolvedRepo = (RepoSpec, Option<String>, hf_hub::api::RepoInfo, Vec<String>);

/// Probe the primary spec; on `NoMatchingFiles` and a non-empty
/// `fallback_repos`, try each fallback in order with a quant-substring
/// match against its `.gguf` siblings. The first fallback that yields
/// at least one match wins. If every probe fails, return the original
/// `NoMatchingFiles` error so callers see the primary repo's name.
async fn resolve_repo(
  spec: &RepoSpec,
  api: &hf_hub::api::tokio::Api,
  options: &DownloadOptions,
) -> Result<ResolvedRepo, DownloadError> {
  let primary_handle = build_repo_handle(api, &spec.repo_id, options.revision.as_deref());
  match probe_repo(
    &primary_handle,
    spec.pinned_filename.as_deref(),
    options.extension_filter.as_deref(),
    None,
  )
  .await
  {
    Ok((info, filtered)) => Ok((spec.clone(), options.revision.clone(), info, filtered)),
    Err(primary_err) => {
      // Only fall back on the explicit "repo listing came up empty"
      // signal — transient API failures (rate limits, network blips)
      // must surface to the caller, not silently swap repos.
      let is_no_match = matches!(&primary_err, DownloadError::NoMatchingFiles { .. });
      if !is_no_match || options.fallback_repos.is_empty() {
        return Err(primary_err);
      }
      log::info!(
        "init download: `{}` has no matching files; trying {} fallback(s)",
        spec.repo_id,
        options.fallback_repos.len(),
      );
      for fallback_id in &options.fallback_repos {
        // Fallbacks always use the default branch — the user's
        // `--revision` is meaningful for the primary spec only.
        let handle = build_repo_handle(api, fallback_id, None);
        match probe_repo(
          &handle,
          None,
          options.extension_filter.as_deref(),
          options.quant_hint.as_deref(),
        )
        .await
        {
          Ok((info, filtered)) => {
            log::info!(
              "init download: resolved synthetic row to fallback `{fallback_id}` ({} file)",
              filtered.len(),
            );
            let pinned = filtered.first().cloned();
            return Ok((
              RepoSpec {
                repo_id: fallback_id.clone(),
                pinned_filename: pinned,
              },
              None,
              info,
              filtered,
            ));
          }
          Err(e) => {
            log::debug!("init download: fallback `{fallback_id}` rejected: {e}");
            continue;
          }
        }
      }
      Err(primary_err)
    }
  }
}

/// List `repo`'s siblings, apply the standard file filter, and (when
/// `quant_hint` is set) narrow to a single `.gguf` whose name carries
/// the quant tag. Returns `NoMatchingFiles` when no file survives,
/// propagates hf-hub errors for any other failure.
async fn probe_repo(
  repo: &hf_hub::api::tokio::ApiRepo,
  pinned: Option<&str>,
  extension: Option<&str>,
  quant_hint: Option<&str>,
) -> Result<(hf_hub::api::RepoInfo, Vec<String>), DownloadError> {
  let info = repo.info().await?;
  let all_files: Vec<String> = info.siblings.iter().map(|s| s.rfilename.clone()).collect();
  let mut filtered = select_files(&all_files, pinned, extension);
  if let Some(quant) = quant_hint {
    filtered = pick_quant_match(&filtered, quant)
      .map(|f| vec![f])
      .unwrap_or_default();
  }
  if filtered.is_empty() {
    return Err(DownloadError::NoMatchingFiles {
      repo: repo.url("").trim_end_matches("/resolve//").to_string(),
    });
  }
  Ok((info, filtered))
}

/// Pick a single `.gguf` file from `candidates` whose name contains
/// `quant` (case-insensitive). When several match, prefer the shortest
/// stem — bartowski-style repos often ship variants like
/// `Model-Q4_K_M.gguf` and `Model-Q4_K_M-imatrix.gguf`; the plain form
/// is the one we want.
fn pick_quant_match(candidates: &[String], quant: &str) -> Option<String> {
  let q_lower = quant.to_lowercase();
  let mut matches: Vec<&String> = candidates
    .iter()
    .filter(|f| f.to_lowercase().contains(&q_lower))
    .collect();
  matches.sort_by_key(|f| f.len());
  matches.first().map(|s| (*s).clone())
}

/// Trusted converter repo candidates for a `synthetic`-publisher snapshot
/// row. The source repo `Owner/Name` ships safetensors only; these
/// repos commonly host community GGUF conversions of the same weights.
/// Order matters — bartowski is checked first because it's the most
/// frequent host today.
///
/// Two bartowski conventions exist in the wild:
/// `bartowski/{Owner}_{Name}-GGUF` (newer Qwen, etc.) and
/// `bartowski/{Name}-GGUF` (older meta-llama, gemma). Both are
/// probed; whichever resolves first wins.
pub fn synthetic_publisher_fallbacks(source_repo_id: &str) -> Vec<String> {
  let Some((owner, name)) = source_repo_id.split_once('/') else {
    return Vec::new();
  };
  vec![
    format!("bartowski/{owner}_{name}-GGUF"),
    format!("bartowski/{name}-GGUF"),
    format!("unsloth/{name}-GGUF"),
    format!("lmstudio-community/{name}-GGUF"),
  ]
}

/// Orchestrator. Lists, filters, prechecks disk, downloads via hf-hub.
pub async fn download_repo(
  spec: &RepoSpec,
  fetch: &FetchClient,
  options: &DownloadOptions,
) -> Result<DownloadResult, DownloadError> {
  if fetch.is_offline() {
    return Err(DownloadError::Offline);
  }
  let cache_root = hf_cache_dir()?;
  let api = build_api(cache_root.clone())?;

  // Resolve the actual repo + file set. Tries the primary spec first;
  // on `NoMatchingFiles` (typical for synthetic snapshot rows pointing
  // at safetensors-only org repos), probes each fallback for a
  // quant-matching `.gguf` and uses the first that resolves. The
  // resolved spec replaces `spec` for the rest of the function.
  let (resolved_spec, resolved_revision, info, filtered) =
    resolve_repo(spec, &api, options).await?;
  let repo = build_repo_handle(&api, &resolved_spec.repo_id, resolved_revision.as_deref());

  // hf-hub's RepoInfo doesn't expose per-file size, so we HEAD each
  // file's resolve URL to (a) enforce PER_FILE_MAX_BYTES and (b) feed
  // the R64 disk precheck with a real total. One HEAD per file is
  // cheap relative to the actual GGUF download that follows.
  let mut sizes: Vec<(String, u64)> = Vec::with_capacity(filtered.len());
  let mut total_size: u64 = 0;
  for filename in &filtered {
    let url = repo.url(filename);
    let md = api.metadata(&url).await?;
    let size = md.size() as u64;
    if size > PER_FILE_MAX_BYTES {
      return Err(DownloadError::FileTooLarge {
        file: filename.clone(),
        size,
        cap: PER_FILE_MAX_BYTES,
      });
    }
    total_size = total_size.saturating_add(size);
    sizes.push((filename.clone(), size));
  }

  precheck_disk(&cache_root, total_size)?;

  if let Some(p) = &options.progress {
    p.on_files_resolved(&sizes);
  }

  let total_files = sizes.len();
  let mut paths = Vec::with_capacity(total_files);
  let cache_handle = hf_hub::Cache::new(cache_root.clone());
  for (idx, (filename, size)) in sizes.iter().enumerate() {
    if let Some(p) = &options.progress {
      p.on_file_started(filename, *size, idx, total_files);
    }
    // Cache short-circuit: if the file already lives in the HF
    // snapshot, return the cached path without hitting the network.
    // Otherwise route through `download_with_progress` so the
    // chunk-level callback drives byte-accurate progress.
    let cache_repo = cache_handle.repo(hf_hub::Repo::with_revision(
      resolved_spec.repo_id.clone(),
      hf_hub::RepoType::Model,
      resolved_revision
        .clone()
        .unwrap_or_else(|| "main".to_string()),
    ));
    let cached = if resolved_revision.is_some() {
      cache_repo.get(filename)
    } else {
      // For the default branch hf-hub stores the resolved ref under
      // `refs/main`; the with-revision lookup misses that. Probe the
      // model handle's default cache repo too.
      cache_handle
        .model(resolved_spec.repo_id.clone())
        .get(filename)
        .or_else(|| cache_repo.get(filename))
    };
    let path = if let Some(p) = cached {
      if let Some(cb) = &options.progress {
        cb.on_bytes_progress(filename, *size);
      }
      p
    } else {
      let adapter = HfHubProgressAdapter::new(filename.clone(), options.progress.clone());
      repo.download_with_progress(filename, adapter).await?
    };
    if let Some(p) = &options.progress {
      p.on_file_finished(filename, idx, total_files);
    }
    paths.push(path);
  }

  Ok(DownloadResult {
    paths,
    total_bytes: total_size,
    revision: info.sha,
    resolved_repo_id: resolved_spec.repo_id.clone(),
  })
}

/// `llamastash pull <repo>` handler entry-point. Unit 3 wires this in.
pub async fn run(args: PullArgs, _cli: &Cli, _config: &Config) -> CliResult {
  let spec = RepoSpec::parse(&args.repo).map_err(|e| CliExit::prefix(PULL_FAILED, "pull", e))?;
  let fetch = crate::init::fetch::build_with_offline_check(
    args.offline,
    crate::init::fetch::FetchClientConfig::default(),
  )
  .map_err(|e| CliExit::prefix(PULL_FAILED, "pull: fetch client", e))?;
  match download_repo(&spec, &fetch, &DownloadOptions::default()).await {
    Ok(result) => {
      if args.json {
        let body = serde_json::json!({
          "repo": spec.repo_id,
          "revision": result.revision,
          "files": result
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>(),
          "total_bytes": result.total_bytes,
        });
        println!("{}", crate::cli::output::pretty_json(&body));
      } else {
        let mib = result.total_bytes as f64 / (1024.0 * 1024.0);
        println!(
          "pulled {} file(s) ({mib:.1} MiB) for `{}`",
          result.paths.len(),
          spec.repo_id
        );
      }
      Ok(())
    }
    Err(e) => Err(CliExit::prefix(PULL_FAILED, "pull", e)),
  }
}

/// Init's model step wraps this so a wizard-internal failure maps to
/// `INIT_DOWNLOAD_FAILED` (73) instead of `PULL_FAILED` (69).
pub async fn run_for_init(
  spec: &RepoSpec,
  fetch: &FetchClient,
  options: &DownloadOptions,
) -> Result<DownloadResult, CliExit> {
  download_repo(spec, fetch, options)
    .await
    .map_err(|e| CliExit::prefix(INIT_DOWNLOAD_FAILED, "init download", e))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Capture-only progress sink used by the trait-default tests.
  #[derive(Default)]
  struct CaptureProgress {
    bytes_progress: std::sync::Mutex<Vec<(String, u64)>>,
  }

  impl DownloadProgress for CaptureProgress {
    fn on_files_resolved(&self, _files: &[(String, u64)]) {}
    fn on_file_started(&self, _filename: &str, _size: u64, _index: usize, _total: usize) {}
    fn on_file_finished(&self, _filename: &str, _index: usize, _total: usize) {}
    fn on_bytes_progress(&self, filename: &str, bytes_in_file: u64) {
      self
        .bytes_progress
        .lock()
        .unwrap()
        .push((filename.to_string(), bytes_in_file));
    }
  }

  #[tokio::test]
  async fn hf_hub_progress_adapter_accumulates_bytes_per_chunk() {
    // Adapter must convert hf-hub's delta-style `update(size)`
    // callbacks into a cumulative byte count and forward each one
    // into `on_bytes_progress`. Without this the dialog's strip
    // sits at 0% for the duration of the download.
    use hf_hub::api::tokio::Progress;
    let capture = std::sync::Arc::new(CaptureProgress::default());
    let inner: std::sync::Arc<dyn DownloadProgress> = capture.clone();
    let mut adapter = HfHubProgressAdapter::new("model.gguf".into(), Some(inner));
    adapter.init(0, "model.gguf").await;
    adapter.update(100).await;
    adapter.update(50).await;
    adapter.update(25).await;
    adapter.finish().await;
    let events = capture.bytes_progress.lock().unwrap().clone();
    assert_eq!(events.len(), 3, "one event per chunk: {events:?}");
    assert_eq!(events[0], ("model.gguf".to_string(), 100));
    assert_eq!(events[1], ("model.gguf".to_string(), 150));
    assert_eq!(events[2], ("model.gguf".to_string(), 175));
  }

  #[test]
  fn download_progress_trait_has_default_bytes_callback() {
    // The wizard's cliclack progress impl doesn't implement the new
    // byte callback — verify the default no-op exists so existing
    // call sites keep compiling and running.
    struct LegacyProgress;
    impl DownloadProgress for LegacyProgress {
      fn on_files_resolved(&self, _files: &[(String, u64)]) {}
      fn on_file_started(&self, _filename: &str, _size: u64, _index: usize, _total: usize) {}
      fn on_file_finished(&self, _filename: &str, _index: usize, _total: usize) {}
    }
    let p: Box<dyn DownloadProgress> = Box::new(LegacyProgress);
    // Default impl is a no-op — must not panic.
    p.on_bytes_progress("file", 123);
  }

  #[test]
  fn parse_owner_repo() {
    let s = RepoSpec::parse("Qwen/Qwen2.5-7B-Instruct-GGUF").unwrap();
    assert_eq!(s.repo_id, "Qwen/Qwen2.5-7B-Instruct-GGUF");
    assert!(s.pinned_filename.is_none());
  }

  #[test]
  fn parse_owner_repo_with_pinned_file() {
    let s = RepoSpec::parse("owner/repo:weights.gguf").unwrap();
    assert_eq!(s.repo_id, "owner/repo");
    assert_eq!(s.pinned_filename.as_deref(), Some("weights.gguf"));
  }

  #[test]
  fn parse_refuses_missing_slash() {
    assert!(RepoSpec::parse("just-a-name").is_err());
  }

  #[test]
  fn parse_refuses_empty_segments() {
    assert!(RepoSpec::parse("/repo").is_err());
    assert!(RepoSpec::parse("owner/").is_err());
    assert!(RepoSpec::parse("owner/repo:").is_err());
  }

  #[test]
  fn parse_refuses_path_traversal_in_filename() {
    assert!(RepoSpec::parse("owner/repo:../escape").is_err());
    assert!(RepoSpec::parse("owner/repo:nested/path.gguf").is_err());
  }

  #[test]
  fn repo_folder_name_matches_huggingface_hub_convention() {
    assert_eq!(
      repo_folder_name("Qwen/Qwen2.5-7B"),
      "models--Qwen--Qwen2.5-7B"
    );
    assert_eq!(
      repo_folder_name("bartowski/Llama-3.2-3B-Instruct-GGUF"),
      "models--bartowski--Llama-3.2-3B-Instruct-GGUF"
    );
  }

  #[test]
  fn file_url_format_matches_resolve_endpoint() {
    let url = file_url(
      DEFAULT_HF_ENDPOINT,
      "Qwen/Qwen2.5-7B-Instruct-GGUF",
      "weights.gguf",
    );
    assert_eq!(
      url,
      "https://huggingface.co/Qwen/Qwen2.5-7B-Instruct-GGUF/resolve/main/weights.gguf"
    );
  }

  #[test]
  fn validate_endpoint_accepts_default_and_hf_subdomains() {
    assert!(validate_endpoint("https://huggingface.co").is_ok());
    assert!(validate_endpoint("https://huggingface.co/").is_ok());
    assert!(validate_endpoint("https://cdn-lfs.huggingface.co").is_ok());
    assert!(validate_endpoint("https://huggingface.co:443").is_ok());
  }

  #[test]
  fn validate_endpoint_refuses_http() {
    let err = validate_endpoint("http://huggingface.co").unwrap_err();
    assert!(matches!(err, DownloadError::EndpointNotAllowed { .. }));
  }

  #[test]
  fn validate_endpoint_refuses_non_allowlisted_host() {
    let err = validate_endpoint("https://attacker.com").unwrap_err();
    assert!(matches!(err, DownloadError::EndpointNotAllowed { .. }));
  }

  #[test]
  fn validate_endpoint_refuses_lookalike_suffix() {
    // `huggingface.co.attacker.com` shares the `huggingface.co`
    // substring but is rooted on `attacker.com`; the allowlist
    // check is `host == a || host.ends_with(".a")` so this is
    // refused.
    let err = validate_endpoint("https://huggingface.co.attacker.com").unwrap_err();
    assert!(matches!(err, DownloadError::EndpointNotAllowed { .. }));
  }

  #[test]
  fn validate_endpoint_refuses_userinfo_authority_to_external_host() {
    // `https://huggingface.co@attacker.com` — the URL authority
    // is `attacker.com`, the `huggingface.co` substring is the
    // userinfo.
    let err = validate_endpoint("https://huggingface.co@attacker.com").unwrap_err();
    assert!(matches!(err, DownloadError::EndpointNotAllowed { .. }));
  }

  #[test]
  fn precheck_disk_passes_when_root_is_huge() {
    assert!(precheck_disk(Path::new("/"), 1024).is_ok());
  }

  #[test]
  fn precheck_disk_fails_when_needed_exceeds_available() {
    let err = precheck_disk(Path::new("/"), u64::MAX / 2).unwrap_err();
    assert!(matches!(err, DownloadError::InsufficientDisk { .. }));
  }

  #[test]
  fn select_files_exact_pinned_match() {
    let files = vec![
      "qwen2.5-7b-instruct-q4_k_m.gguf".to_string(),
      "qwen2.5-7b-instruct-q8_0.gguf".to_string(),
      "README.md".to_string(),
    ];
    let picked = select_files(
      &files,
      Some("qwen2.5-7b-instruct-q4_k_m.gguf"),
      Some(".gguf"),
    );
    assert_eq!(picked, vec!["qwen2.5-7b-instruct-q4_k_m.gguf"]);
  }

  #[test]
  fn select_files_expands_sharded_pinned_filename() {
    // Real layout of `Qwen/Qwen2.5-7B-Instruct-GGUF` — the snapshot
    // records the unsharded basename but the repo only hosts shards.
    let files = vec![
      "qwen2.5-7b-instruct-q4_0-00001-of-00002.gguf".to_string(),
      "qwen2.5-7b-instruct-q4_0-00002-of-00002.gguf".to_string(),
      "qwen2.5-7b-instruct-q4_k_m-00001-of-00002.gguf".to_string(),
      "qwen2.5-7b-instruct-q4_k_m-00002-of-00002.gguf".to_string(),
      "README.md".to_string(),
    ];
    let picked = select_files(
      &files,
      Some("qwen2.5-7b-instruct-q4_k_m.gguf"),
      Some(".gguf"),
    );
    assert_eq!(
      picked,
      vec![
        "qwen2.5-7b-instruct-q4_k_m-00001-of-00002.gguf",
        "qwen2.5-7b-instruct-q4_k_m-00002-of-00002.gguf",
      ]
    );
  }

  #[test]
  fn select_files_expands_5_shard_set() {
    let files = vec![
      "qwen2.5-32b-instruct-q4_k_m-00001-of-00005.gguf".to_string(),
      "qwen2.5-32b-instruct-q4_k_m-00002-of-00005.gguf".to_string(),
      "qwen2.5-32b-instruct-q4_k_m-00003-of-00005.gguf".to_string(),
      "qwen2.5-32b-instruct-q4_k_m-00004-of-00005.gguf".to_string(),
      "qwen2.5-32b-instruct-q4_k_m-00005-of-00005.gguf".to_string(),
    ];
    let picked = select_files(&files, Some("qwen2.5-32b-instruct-q4_k_m.gguf"), None);
    assert_eq!(picked.len(), 5);
    assert_eq!(
      picked.first().unwrap(),
      "qwen2.5-32b-instruct-q4_k_m-00001-of-00005.gguf"
    );
  }

  #[test]
  fn select_files_extension_filter_only() {
    let files = vec![
      "weights.gguf".to_string(),
      "tokenizer.json".to_string(),
      "README.md".to_string(),
    ];
    let picked = select_files(&files, None, Some(".gguf"));
    assert_eq!(picked, vec!["weights.gguf"]);
  }

  #[test]
  fn select_files_returns_empty_when_no_match_and_no_shards() {
    let files = vec!["completely-different.gguf".to_string()];
    let picked = select_files(&files, Some("missing.gguf"), Some(".gguf"));
    assert!(picked.is_empty());
  }

  #[test]
  fn is_shard_index_recognises_canonical_pattern() {
    assert!(is_shard_index("00001-of-00002"));
    assert!(is_shard_index("00005-of-00005"));
    assert!(!is_shard_index("00001of00002"));
    assert!(!is_shard_index("-of-"));
    assert!(!is_shard_index(""));
    assert!(!is_shard_index("foo-of-bar"));
  }

  #[tokio::test]
  async fn download_repo_propagates_offline() {
    let fetch = FetchClient::offline();
    let spec = RepoSpec::parse("owner/repo").unwrap();
    let err = download_repo(&spec, &fetch, &DownloadOptions::default())
      .await
      .unwrap_err();
    assert!(matches!(err, DownloadError::Offline));
  }

  #[test]
  fn download_options_default_carries_no_revision() {
    // Pre-`--revision` callers (the standalone `llamastash pull`
    // handler, every existing wizard integration test) must keep
    // resolving the default branch — verified by `Default` returning
    // `revision: None`.
    let opts = DownloadOptions::default();
    assert!(opts.revision.is_none());
  }

  #[tokio::test]
  async fn download_repo_propagates_offline_even_when_revision_set() {
    // Offline mode must short-circuit *before* hf-hub touches the
    // network, regardless of whether `--revision` was supplied. Without
    // this guarantee a pinned-SHA UAT in offline mode would still try
    // to resolve the repo against HF.
    let fetch = FetchClient::offline();
    let spec = RepoSpec::parse("owner/repo").unwrap();
    let opts = DownloadOptions {
      revision: Some("abc1234".to_string()),
      ..DownloadOptions::default()
    };
    let err = download_repo(&spec, &fetch, &opts).await.unwrap_err();
    assert!(matches!(err, DownloadError::Offline));
  }

  #[test]
  fn pick_quant_match_finds_substring_case_insensitive() {
    let files = vec![
      "Qwen_Qwen3.6-27B-Q4_K_M.gguf".to_string(),
      "Qwen_Qwen3.6-27B-Q8_0.gguf".to_string(),
    ];
    assert_eq!(
      pick_quant_match(&files, "Q4_K_M"),
      Some("Qwen_Qwen3.6-27B-Q4_K_M.gguf".to_string())
    );
    // Case-insensitive: lowercase hint still matches uppercase tag.
    assert_eq!(
      pick_quant_match(&files, "q8_0"),
      Some("Qwen_Qwen3.6-27B-Q8_0.gguf".to_string())
    );
  }

  #[test]
  fn pick_quant_match_prefers_shortest_when_multiple_match() {
    // bartowski sometimes ships `Model-Q4_K_M.gguf` alongside an
    // `-imatrix` variant. The plain form is what we want.
    let files = vec![
      "Model-Q4_K_M-imatrix.gguf".to_string(),
      "Model-Q4_K_M.gguf".to_string(),
    ];
    assert_eq!(
      pick_quant_match(&files, "Q4_K_M"),
      Some("Model-Q4_K_M.gguf".to_string())
    );
  }

  #[test]
  fn pick_quant_match_returns_none_when_no_file_carries_the_tag() {
    let files = vec!["Model-Q5_K_M.gguf".to_string()];
    assert!(pick_quant_match(&files, "Q4_K_M").is_none());
    assert!(pick_quant_match(&[], "Q4_K_M").is_none());
  }

  #[test]
  fn synthetic_publisher_fallbacks_generates_four_candidates_in_priority_order() {
    let fallbacks = synthetic_publisher_fallbacks("Qwen/Qwen3-Next-80B-A3B-Instruct");
    assert_eq!(
      fallbacks,
      vec![
        "bartowski/Qwen_Qwen3-Next-80B-A3B-Instruct-GGUF".to_string(),
        "bartowski/Qwen3-Next-80B-A3B-Instruct-GGUF".to_string(),
        "unsloth/Qwen3-Next-80B-A3B-Instruct-GGUF".to_string(),
        "lmstudio-community/Qwen3-Next-80B-A3B-Instruct-GGUF".to_string(),
      ]
    );
  }

  #[test]
  fn synthetic_publisher_fallbacks_is_empty_for_a_non_owner_repo_id() {
    // Defensive — the wizard only calls this with valid curated entry
    // repos, but a bare name should not panic or produce nonsense.
    assert!(synthetic_publisher_fallbacks("bare-name").is_empty());
    assert!(synthetic_publisher_fallbacks("").is_empty());
  }
}
