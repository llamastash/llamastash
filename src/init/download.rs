//! HuggingFace pull (Unit 9, R65).
//!
//! Backs `llamadash pull <repo>` standalone and the init wizard's
//! model step. v2 uses the [`hf-hub`] crate (0.5 line) for the HF
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
//! `llamadash list` rescans dedupe via the existing discovery path
//! (R62).

use std::path::{Path, PathBuf};

use hf_hub::api::tokio::{Api, ApiBuilder};

use crate::cli::cli_args::{Cli, PullArgs};
use crate::cli::exit_codes::{CliExit, CliResult, INIT_DOWNLOAD_FAILED, PULL_FAILED};
use crate::config::Config;
use crate::init::fetch::{FetchClient, FetchError};

/// Disk-space headroom required *on top of* the estimated download
/// size (R64). 1 GiB matches the brainstorm spec.
pub const DISK_HEADROOM_BYTES: u64 = 1024 * 1024 * 1024;

/// Max bytes per per-file download. 16 GiB caps the largest plausible
/// GGUF shard (a 70B Q4_K_M is ~43 GB total but split across shards).
/// Enforced via hf-hub's `Api::metadata` HEAD before each download.
pub const PER_FILE_MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;

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
  #[error("offline mode (LLAMADASH_OFFLINE / --offline); cannot pull from HuggingFace")]
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
  Hub(String),
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

impl From<hf_hub::api::tokio::ApiError> for DownloadError {
  fn from(e: hf_hub::api::tokio::ApiError) -> Self {
    DownloadError::Hub(e.to_string())
  }
}

/// Outcome of a successful download.
#[derive(Debug, Clone, Default)]
pub struct DownloadResult {
  pub paths: Vec<PathBuf>,
  pub total_bytes: u64,
  pub revision: String,
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
#[derive(Debug, Clone)]
pub struct DownloadOptions {
  /// `.gguf` by default; the wizard's "skip non-weights" rule.
  pub extension_filter: Option<String>,
  /// Estimated size for the R64 precheck. `None` skips precheck.
  pub estimated_bytes: Option<u64>,
}

impl Default for DownloadOptions {
  fn default() -> Self {
    Self {
      extension_filter: Some(".gguf".to_string()),
      estimated_bytes: None,
    }
  }
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
    .with_user_agent("llamadash", env!("CARGO_PKG_VERSION"))
    .with_progress(false)
    .build()
    .map_err(DownloadError::from)
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
  let repo = api.model(spec.repo_id.clone());

  let info = repo.info().await?;
  let filtered: Vec<String> = info
    .siblings
    .into_iter()
    .map(|s| s.rfilename)
    .filter(
      |path| match (&spec.pinned_filename, &options.extension_filter) {
        (Some(name), _) => path == name,
        (None, Some(ext)) => path.ends_with(ext.as_str()),
        (None, None) => true,
      },
    )
    .collect();
  if filtered.is_empty() {
    return Err(DownloadError::NoMatchingFiles {
      repo: spec.repo_id.clone(),
    });
  }

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

  let mut paths = Vec::with_capacity(sizes.len());
  for (filename, _) in &sizes {
    let path = repo.get(filename).await?;
    paths.push(path);
  }

  Ok(DownloadResult {
    paths,
    total_bytes: total_size,
    revision: info.sha,
  })
}

/// `llamadash pull <repo>` handler entry-point. Unit 3 wires this in.
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

  #[tokio::test]
  async fn download_repo_propagates_offline() {
    let fetch = FetchClient::offline();
    let spec = RepoSpec::parse("owner/repo").unwrap();
    let err = download_repo(&spec, &fetch, &DownloadOptions::default())
      .await
      .unwrap_err();
    assert!(matches!(err, DownloadError::Offline));
  }
}
