//! HuggingFace Hub API client (Unit 3 / R104–R109).
//!
//! Two surfaces:
//! - [`search`] hits `GET /api/models` with `search`, `filter=gguf`,
//!   `sort`, and `limit` query params. Results carry the sort-relevant
//!   metric, the `pipeline_tag`, tags, and the canonical repo id.
//!   Pagination is via the `Link` response header (`rel="next"`); we
//!   extract just the opaque `cursor` query parameter from the next URL
//!   so a server-supplied pagination URL can't redirect outside the
//!   fetch contract's host allowlist (defense in depth — the redirect
//!   policy already runs `check_url` on every hop).
//! - [`list_repo_files`] returns the sibling list for a single repo via
//!   `hf_hub::Api::model(id).info()` — same path `download_repo` uses,
//!   so the bearer-token + endpoint plumbing is shared.
//!
//! Both surfaces route through [`FetchClient`] (search) and `hf-hub`
//! (per-repo listing). Search calls are deliberately unauthenticated;
//! the v2 fetch contract forbids opportunistic `Authorization`
//! headers, and the `/api/models` search endpoint is public anyway.
//! Private repos may surface in search results but fail to pull from
//! the existing `download_repo` path; the dialog renders the error
//! per R117 rather than gating search behind auth.

use serde::Deserialize;

use crate::init::download;
use crate::init::fetch::{FetchClient, FetchError};
use crate::init::fetch_policy::{check_url, HostAllowlist};

/// Max bytes for a single search response. 1 MiB covers ≥ 20 model
/// objects with comfortable headroom; an upstream payload larger than
/// this is treated as a misbehaving endpoint, not a real result.
pub const SEARCH_BODY_CAP: u64 = 1024 * 1024;

/// Max bytes for the per-repo file listing (`/api/models/<id>`). 256
/// KiB covers even sharded repos with dozens of siblings.
#[allow(dead_code)]
pub const REPO_LIST_BODY_CAP: u64 = 256 * 1024;

/// Default page size; matches R108's target of 20 rows per page.
pub const SEARCH_LIMIT: u32 = 20;

/// One row in a HuggingFace search response. Fields that the API may
/// omit are `Option<_>` so the deserialiser doesn't reject a partial
/// payload — repos newly indexed often miss `lastModified` or
/// `pipeline_tag` for a few hours.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HfSearchResult {
  #[serde(rename = "id")]
  pub repo_id: String,
  #[serde(default)]
  pub downloads: Option<u64>,
  #[serde(default)]
  pub likes: Option<u64>,
  #[serde(default, rename = "lastModified")]
  pub last_modified: Option<String>,
  #[serde(default, rename = "pipeline_tag")]
  pub pipeline_tag: Option<String>,
  #[serde(default)]
  pub tags: Vec<String>,
}

/// Sort key for the search endpoint. Maps to HF Hub's API query
/// tokens — `Trending` and `RecentlyUpdated` are the conventional
/// labels verified during planning; if the API surprises us, the
/// mapping moves here without touching the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HfSortKey {
  Downloads,
  Likes,
  RecentlyUpdated,
  Trending,
}

impl HfSortKey {
  /// Wire token the `sort=` query parameter takes.
  pub fn as_query_token(self) -> &'static str {
    match self {
      HfSortKey::Downloads => "downloads",
      HfSortKey::Likes => "likes",
      HfSortKey::RecentlyUpdated => "lastModified",
      HfSortKey::Trending => "trending",
    }
  }

  /// Cycle order (R107): Downloads → Likes → RecentlyUpdated → Trending → Downloads.
  pub fn cycle_next(self) -> Self {
    match self {
      HfSortKey::Downloads => HfSortKey::Likes,
      HfSortKey::Likes => HfSortKey::RecentlyUpdated,
      HfSortKey::RecentlyUpdated => HfSortKey::Trending,
      HfSortKey::Trending => HfSortKey::Downloads,
    }
  }
}

/// One page of search results, with the opaque cursor token the next
/// `search()` call passes back if pagination is available.
#[derive(Debug, Clone)]
pub struct HfSearchPage {
  pub results: Vec<HfSearchResult>,
  pub next_cursor: Option<String>,
}

/// One sibling file in an HF repo. `size_bytes` is `None` when the
/// upstream `RepoInfo` doesn't carry the field — some shards are
/// missing it; the dialog falls back to a HEAD probe on Confirm to
/// surface the real total before dispatching the pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfRepoFile {
  pub filename: String,
  pub size_bytes: Option<u64>,
}

/// Issue an HF Hub search. Cursor-based pagination — pass
/// `Some(prev_page.next_cursor)` to advance.
pub async fn search(
  fetch: &FetchClient,
  query: &str,
  sort: HfSortKey,
  cursor: Option<&str>,
) -> Result<HfSearchPage, FetchError> {
  if fetch.is_offline() {
    return Err(FetchError::Offline);
  }
  let endpoint = endpoint_or_default();
  let mut url = reqwest::Url::parse(&format!("{endpoint}/api/models"))
    .map_err(|e| FetchError::Transport(format!("URL parse: {e}")))?;
  {
    let mut pairs = url.query_pairs_mut();
    pairs.append_pair("search", query);
    pairs.append_pair("filter", "gguf");
    pairs.append_pair("sort", sort.as_query_token());
    pairs.append_pair("limit", &SEARCH_LIMIT.to_string());
    if let Some(c) = cursor {
      pairs.append_pair("cursor", c);
    }
  }
  let (results, headers) = fetch
    .get_json_with_headers::<Vec<HfSearchResult>>(url.as_str(), SEARCH_BODY_CAP)
    .await?;
  let next_cursor = headers
    .get(reqwest::header::LINK)
    .and_then(|v| v.to_str().ok())
    .and_then(extract_next_cursor);
  Ok(HfSearchPage {
    results,
    next_cursor,
  })
}

/// Resolve the HF endpoint; on any error (env var validation failure)
/// fall back to the default. Search is best-effort: an
/// allowlist-violating `HF_ENDPOINT` aborts the download path
/// (via `download::endpoint()`); for search we surface the same
/// refusal by routing back through `FetchClient`, which re-checks the
/// host on every request, so the override never has a chance to leak.
fn endpoint_or_default() -> String {
  download::endpoint().unwrap_or_else(|_| download::DEFAULT_HF_ENDPOINT.to_string())
}

/// Extract the opaque `cursor` query parameter from a Link header's
/// `rel="next"` URL. Re-validates the host against the HF allowlist
/// (with subdomain matching) so a server-supplied pagination URL
/// pointing outside `*.huggingface.co` returns `None` rather than
/// being silently followed on the next call.
fn extract_next_cursor(link_header: &str) -> Option<String> {
  let next_url = parse_next_link(link_header)?;
  let parsed = reqwest::Url::parse(&next_url).ok()?;
  let allowlist = HostAllowlist::from_hosts(download::HF_HOST_ALLOWLIST.iter().copied())
    .with_subdomain_matching(true);
  check_url(&parsed, &allowlist).ok()?;
  parsed
    .query_pairs()
    .find_map(|(k, v)| (k == "cursor").then(|| v.into_owned()))
}

/// Parse RFC 5988 Link headers and return the URL labelled with
/// `rel="next"`. Tolerant of whitespace and quoted params; the HF
/// API emits a single-segment Link header with the next URL.
fn parse_next_link(header: &str) -> Option<String> {
  for raw_segment in header.split(',') {
    let segment = raw_segment.trim();
    let (raw_url, params) = segment.split_once(';')?;
    let url = raw_url
      .trim()
      .strip_prefix('<')
      .and_then(|s| s.strip_suffix('>'))?
      .to_string();
    let is_next = params.split(';').any(|p| {
      let p = p.trim();
      // Accept both `rel=next` and `rel="next"`.
      matches!(
        p.split_once('=').map(|(k, v)| (k.trim(), v.trim().trim_matches('"'))),
        Some(("rel", "next"))
      )
    });
    if is_next {
      return Some(url);
    }
  }
  None
}

/// List the GGUF-relevant sibling files of a single repo. Routes
/// through the same `hf-hub::Api` build path `download_repo` uses so
/// the bearer-token / endpoint resolution stays in one place (R65 +
/// the existing HF carve-out).
pub async fn list_repo_files(
  fetch: &FetchClient,
  repo_id: &str,
) -> Result<Vec<HfRepoFile>, ListRepoFilesError> {
  if fetch.is_offline() {
    return Err(ListRepoFilesError::Offline);
  }
  let cache_root = download::hf_cache_dir().map_err(ListRepoFilesError::download)?;
  let api = download::build_api(cache_root).map_err(ListRepoFilesError::download)?;
  let info = api
    .model(repo_id.to_string())
    .info()
    .await
    .map_err(|e| ListRepoFilesError::HfHub(e.to_string()))?;
  Ok(
    info
      .siblings
      .into_iter()
      .map(|s| HfRepoFile {
        filename: s.rfilename,
        size_bytes: None,
      })
      .collect(),
  )
}

/// Why a `list_repo_files` call failed. Mirrors the variants the
/// dialog branches on.
#[derive(Debug, thiserror::Error)]
pub enum ListRepoFilesError {
  #[error("network egress is disabled (LLAMASTASH_OFFLINE / --offline)")]
  Offline,
  #[error("HF auth / cache resolution failed: {0}")]
  Download(String),
  #[error("hf-hub: {0}")]
  HfHub(String),
}

impl ListRepoFilesError {
  fn download(e: download::DownloadError) -> Self {
    Self::Download(e.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sort_key_query_tokens() {
    assert_eq!(HfSortKey::Downloads.as_query_token(), "downloads");
    assert_eq!(HfSortKey::Likes.as_query_token(), "likes");
    assert_eq!(HfSortKey::RecentlyUpdated.as_query_token(), "lastModified");
    assert_eq!(HfSortKey::Trending.as_query_token(), "trending");
  }

  #[test]
  fn sort_key_cycles_through_all_four() {
    let start = HfSortKey::Downloads;
    let mut cur = start;
    for _ in 0..4 {
      cur = cur.cycle_next();
    }
    assert_eq!(cur, start);
  }

  #[test]
  fn search_result_deserialises_from_recorded_fixture() {
    // Recorded sample of `?search=qwen&filter=gguf&sort=downloads&limit=2`.
    let json = r#"[
      {
        "id": "Qwen/Qwen2.5-7B-Instruct-GGUF",
        "downloads": 1234567,
        "likes": 4321,
        "lastModified": "2026-04-18T12:34:56.000Z",
        "pipeline_tag": "text-generation",
        "tags": ["gguf", "qwen", "coder"]
      },
      {
        "id": "TheBloke/Qwen-7B-Chat-GGUF",
        "downloads": 999,
        "likes": 42,
        "lastModified": "2026-03-01T00:00:00.000Z",
        "tags": ["gguf"]
      }
    ]"#;
    let results: Vec<HfSearchResult> = serde_json::from_str(json).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].repo_id, "Qwen/Qwen2.5-7B-Instruct-GGUF");
    assert_eq!(results[0].downloads, Some(1234567));
    assert_eq!(results[0].pipeline_tag.as_deref(), Some("text-generation"));
    assert!(results[1].pipeline_tag.is_none());
  }

  #[test]
  fn search_result_handles_missing_optional_fields() {
    // A freshly-indexed repo may omit `downloads` / `likes` /
    // `lastModified` / `pipeline_tag` / `tags`; only `id` is
    // guaranteed.
    let json = r#"[{ "id": "owner/new-repo" }]"#;
    let results: Vec<HfSearchResult> = serde_json::from_str(json).unwrap();
    assert_eq!(results[0].repo_id, "owner/new-repo");
    assert!(results[0].downloads.is_none());
    assert!(results[0].pipeline_tag.is_none());
    assert!(results[0].tags.is_empty());
  }

  #[test]
  fn extract_next_cursor_pulls_token_from_huggingface_link() {
    let header = "<https://huggingface.co/api/models?cursor=opaque-abc&limit=20>; rel=\"next\"";
    assert_eq!(extract_next_cursor(header), Some("opaque-abc".to_string()));
  }

  #[test]
  fn extract_next_cursor_returns_none_when_only_prev_rel() {
    let header = "<https://huggingface.co/api/models?cursor=prev>; rel=\"prev\"";
    assert!(extract_next_cursor(header).is_none());
  }

  #[test]
  fn extract_next_cursor_returns_none_for_non_allowlisted_host() {
    // Defense in depth: a server-supplied next URL pointing outside
    // huggingface.co must NOT yield a cursor — otherwise the next
    // call would re-issue with that cursor against the HF host, but
    // a smarter exfil attempt could try to embed credentials in the
    // path; refusing the cursor short-circuits the whole class.
    let header = "<https://evil.example.com/api/models?cursor=abc>; rel=\"next\"";
    assert!(extract_next_cursor(header).is_none());
  }

  #[test]
  fn extract_next_cursor_accepts_huggingface_cdn_subdomain() {
    // HF occasionally hosts pagination URLs on a subdomain;
    // subdomain matching against `huggingface.co` is the policy.
    let header =
      "<https://api-inference.huggingface.co/api/models?cursor=sub>; rel=\"next\"";
    assert_eq!(extract_next_cursor(header), Some("sub".to_string()));
  }

  #[test]
  fn extract_next_cursor_returns_none_when_link_header_missing_cursor() {
    let header = "<https://huggingface.co/api/models?foo=bar>; rel=\"next\"";
    assert!(extract_next_cursor(header).is_none());
  }

  #[test]
  fn extract_next_cursor_handles_multi_link_header() {
    // RFC 5988 allows comma-separated Link entries; the next-rel
    // segment must be discoverable regardless of order.
    let header = concat!(
      "<https://huggingface.co/api/models?cursor=prev>; rel=\"prev\", ",
      "<https://huggingface.co/api/models?cursor=after-here>; rel=\"next\""
    );
    assert_eq!(extract_next_cursor(header), Some("after-here".to_string()));
  }

  #[tokio::test]
  async fn search_returns_offline_when_fetch_client_is_offline() {
    let fetch = FetchClient::offline();
    let r = search(&fetch, "qwen", HfSortKey::Downloads, None).await;
    assert!(matches!(r, Err(FetchError::Offline)), "got {r:?}");
  }

  #[tokio::test]
  async fn list_repo_files_returns_offline_when_fetch_client_is_offline() {
    let fetch = FetchClient::offline();
    let r = list_repo_files(&fetch, "owner/repo").await;
    assert!(matches!(r, Err(ListRepoFilesError::Offline)), "got {r:?}");
  }

  #[test]
  fn search_url_escapes_special_characters_in_query() {
    // Encoded form must escape `&` / `=` / Unicode so the server
    // sees the original free-text query rather than parsing it as
    // additional query parameters. This exercises the
    // `query_pairs_mut().append_pair` path without making a network
    // call.
    let mut url = reqwest::Url::parse("https://huggingface.co/api/models").unwrap();
    url
      .query_pairs_mut()
      .append_pair("search", "qwen & coder = 7B 🦙");
    let q = url.query().unwrap_or("");
    assert!(q.contains("search="));
    // `&` becomes `%26`, `=` becomes `%3D`, spaces become `+`, the
    // llama glyph becomes `%F0%9F%A6%99`.
    assert!(
      q.contains("%26") && q.contains("%3D") && q.contains("%F0%9F%A6%99"),
      "expected percent-encoded special chars in query, got `{q}`"
    );
  }
}
