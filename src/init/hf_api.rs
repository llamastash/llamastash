//! HuggingFace Hub API client (Unit 3 / R104–R109).
//!
//! Two surfaces, both routed through [`FetchClient`] so the v2 fetch
//! contract (HTTPS-only, host allowlist, redirect cap, body cap,
//! offline branch) gates every metadata request:
//! - [`search`] hits `GET /api/models` with `search`, `filter=gguf`,
//!   `sort`, and `limit` query params. Results carry the sort-relevant
//!   metric, the `pipeline_tag`, tags, and the canonical repo id.
//!   Pagination is via the `Link` response header (`rel="next"`); we
//!   extract just the opaque `cursor` query parameter from the next URL
//!   so a server-supplied pagination URL can't redirect outside the
//!   fetch contract's host allowlist (defense in depth — the redirect
//!   policy already runs `check_url` on every hop).
//! - [`list_repo_files`] hits `GET /api/models/<repo>/tree/main`, which
//!   returns per-file `path` + `size` (unlike `hf-hub::Api::model(id).info()`,
//!   whose `Siblings` struct only carries the filename). Sizes feed the
//!   picker's hardware-fit indicator (R111) directly.
//!
//! Search + listing are deliberately unauthenticated; the v2 fetch
//! contract forbids opportunistic `Authorization` headers, and both
//! endpoints are public. Private repos may surface in search but fail
//! to pull from the existing `download_repo` path; the dialog renders
//! the error per R117 rather than gating browse behind auth.

use serde::Deserialize;

use crate::init::download;
use crate::init::fetch::{FetchClient, FetchError};
use crate::init::fetch_policy::{check_url, HostAllowlist};

/// Max bytes for a single search response. 1 MiB covers ≥ 20 model
/// objects with comfortable headroom; an upstream payload larger than
/// this is treated as a misbehaving endpoint, not a real result.
pub const SEARCH_BODY_CAP: u64 = 1024 * 1024;

/// Max bytes for the per-repo tree listing (`/api/models/<id>/tree/main`).
/// 256 KiB covers even sharded repos with dozens of siblings — the
/// largest GGUF mirrors (Llama 70B Q4_K_M, 30+ shards) come in under
/// 50 KiB.
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
  ///
  /// `Trending` maps to `trendingScore` — the HF Hub API renamed
  /// the parameter value in 2026 (the legacy `sort=trending` now
  /// returns `HTTP 400`). `trendingScore` accepts `search` and
  /// `filter=gguf` natively, so unlike the legacy carve-out we
  /// don't have to strip either param under trending.
  pub fn as_query_token(self) -> &'static str {
    match self {
      HfSortKey::Downloads => "downloads",
      HfSortKey::Likes => "likes",
      HfSortKey::RecentlyUpdated => "lastModified",
      HfSortKey::Trending => "trendingScore",
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
/// upstream tree response omits the field (rare — pre-LFS pointer
/// files in legacy repos). The dialog renders `?` and the hardware-fit
/// indicator falls back to `FileFit::Unknown` for those rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfRepoFile {
  pub filename: String,
  pub size_bytes: Option<u64>,
}

/// One entry in the `/api/models/<repo>/tree/<rev>` response. Mirrors
/// the public HF Hub tree schema — `type` is `"file"` / `"directory"`,
/// `size` is the resolved blob size (LFS pointer files report their
/// LFS-resolved size here, not the pointer size). `oid`, `lfs`, and
/// other fields exist in the payload but are not needed for the
/// picker — `serde` ignores unknown fields by default.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct HfTreeEntry {
  #[serde(rename = "type")]
  entry_type: String,
  path: String,
  #[serde(default)]
  size: Option<u64>,
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
  let url = build_search_url(&endpoint, query, sort, cursor)?;
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

/// Pure URL builder for the `/api/models` search endpoint. Carved out
/// of [`search`] so the encoding rules can be unit-tested without a
/// runtime / FetchClient. Every sort key — including `Trending` (now
/// `sort=trendingScore`) — accepts `search` and `filter=gguf`
/// natively, so the URL shape is uniform across sorts.
fn build_search_url(
  endpoint: &str,
  query: &str,
  sort: HfSortKey,
  cursor: Option<&str>,
) -> Result<reqwest::Url, FetchError> {
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
  Ok(url)
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
        p.split_once('=')
          .map(|(k, v)| (k.trim(), v.trim().trim_matches('"'))),
        Some(("rel", "next"))
      )
    });
    if is_next {
      return Some(url);
    }
  }
  None
}

/// List the files of a single repo with their resolved sizes. Hits
/// `/api/models/<repo>/tree/main` via [`FetchClient`] so the v2 fetch
/// contract guards the request (cap, allowlist, offline branch) and
/// the picker's hardware-fit indicator (R111) can read real sizes for
/// every row. Directories are filtered out — the picker only renders
/// flat files.
///
/// Falls back to `/tree/master` when `/tree/main` returns 404 — the
/// HF Hub doesn't enforce a default-branch name and a small but
/// non-zero slice of legacy repos still ship from `master`. Other
/// failures (auth, transport) surface immediately without retry.
///
/// `repo_id` is appended as path segments (`owner` / `name`), so the
/// `url` crate percent-encodes any path-special characters and the
/// server can't be tricked into hitting a non-`/api/models/...` path.
pub async fn list_repo_files(
  fetch: &FetchClient,
  repo_id: &str,
) -> Result<Vec<HfRepoFile>, ListRepoFilesError> {
  if fetch.is_offline() {
    return Err(ListRepoFilesError::Offline);
  }
  let (owner, name) = parse_repo_id(repo_id)?;
  match list_repo_files_for_branch(fetch, owner, name, "main").await {
    Err(ListRepoFilesError::Fetch(FetchError::RemoteStatus { status: 404 })) => {
      list_repo_files_for_branch(fetch, owner, name, "master").await
    }
    other => other,
  }
}

async fn list_repo_files_for_branch(
  fetch: &FetchClient,
  owner: &str,
  name: &str,
  branch: &str,
) -> Result<Vec<HfRepoFile>, ListRepoFilesError> {
  let endpoint = endpoint_or_default();
  let url = build_tree_url(&endpoint, owner, name, branch)?;
  let entries: Vec<HfTreeEntry> = fetch
    .get_json(url.as_str(), REPO_LIST_BODY_CAP)
    .await
    .map_err(ListRepoFilesError::Fetch)?;
  Ok(
    entries
      .into_iter()
      .filter(|e| e.entry_type == "file")
      .map(|e| HfRepoFile {
        filename: e.path,
        size_bytes: e.size,
      })
      .collect(),
  )
}

fn build_tree_url(
  endpoint: &str,
  owner: &str,
  name: &str,
  branch: &str,
) -> Result<reqwest::Url, ListRepoFilesError> {
  let mut url = reqwest::Url::parse(endpoint)
    .map_err(|e| ListRepoFilesError::Fetch(FetchError::Transport(format!("URL parse: {e}"))))?;
  {
    let mut segs = url.path_segments_mut().map_err(|_| {
      ListRepoFilesError::Fetch(FetchError::Transport(
        "HF endpoint is not a base URL".to_string(),
      ))
    })?;
    segs.extend(["api", "models", owner, name, "tree", branch]);
  }
  Ok(url)
}

fn parse_repo_id(repo_id: &str) -> Result<(&str, &str), ListRepoFilesError> {
  let (owner, name) = repo_id
    .split_once('/')
    .ok_or_else(|| ListRepoFilesError::BadRepoId(repo_id.to_string()))?;
  if owner.is_empty() || name.is_empty() || name.contains('/') {
    return Err(ListRepoFilesError::BadRepoId(repo_id.to_string()));
  }
  Ok((owner, name))
}

/// Why a `list_repo_files` call failed. Mirrors the variants the
/// dialog branches on.
#[derive(Debug, thiserror::Error)]
pub enum ListRepoFilesError {
  #[error("network egress is disabled (LLAMASTASH_OFFLINE / --offline)")]
  Offline,
  #[error("`{0}` is not an `owner/repo` HuggingFace id")]
  BadRepoId(String),
  #[error("fetch: {0}")]
  Fetch(#[from] FetchError),
}

impl ListRepoFilesError {
  /// Convenience for branches in the dialog that branch on offline /
  /// transient-vs-permanent without re-matching the inner `FetchError`.
  pub fn is_offline(&self) -> bool {
    matches!(
      self,
      ListRepoFilesError::Offline | ListRepoFilesError::Fetch(FetchError::Offline)
    )
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
    // Regression: HF Hub renamed `sort=trending` → `sort=trendingScore`
    // (the legacy token now returns HTTP 400). The 2026-05-21 round-1
    // fix tried to compensate by dropping `search=` server-side; the
    // round-3 follow-up is the actual rename. Confirmed via curl
    // against the live Hub at fix time.
    assert_eq!(HfSortKey::Trending.as_query_token(), "trendingScore");
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
    let header = "<https://api-inference.huggingface.co/api/models?cursor=sub>; rel=\"next\"";
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
    assert!(r.unwrap_err().is_offline());
  }

  #[tokio::test]
  async fn list_repo_files_rejects_bad_repo_ids() {
    let fetch = FetchClient::new(crate::init::fetch::FetchClientConfig::default()).unwrap();
    for bad in ["", "no-slash", "/", "owner/", "/repo", "a/b/c"] {
      let r = list_repo_files(&fetch, bad).await;
      assert!(
        matches!(r, Err(ListRepoFilesError::BadRepoId(_))),
        "`{bad}` should be rejected, got {r:?}"
      );
    }
  }

  #[test]
  fn parse_repo_id_accepts_owner_slash_repo() {
    assert_eq!(
      parse_repo_id("Qwen/Qwen2.5-7B-Instruct-GGUF").unwrap(),
      ("Qwen", "Qwen2.5-7B-Instruct-GGUF")
    );
  }

  #[test]
  fn tree_entry_deserialises_files_and_directories() {
    // Recorded sample of `/api/models/Qwen/Qwen2.5-7B-Instruct-GGUF/tree/main`.
    let json = r#"[
      { "type": "file", "path": "qwen2.5-7b-instruct-q4_k_m.gguf", "size": 4683074336, "oid": "abc", "lfs": { "size": 4683074336, "sha256": "deadbeef", "pointerSize": 134 } },
      { "type": "file", "path": "config.json", "size": 612, "oid": "def" },
      { "type": "directory", "path": "subdir" },
      { "type": "file", "path": "README.md", "size": 8421, "oid": "ghi" }
    ]"#;
    let entries: Vec<HfTreeEntry> = serde_json::from_str(json).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].entry_type, "file");
    assert_eq!(entries[0].size, Some(4683074336));
    assert_eq!(entries[2].entry_type, "directory");
    assert_eq!(entries[2].size, None);
  }

  #[test]
  fn tree_entry_tolerates_missing_size() {
    // Defensive: legacy pre-LFS pointer rows occasionally omit `size`.
    let json = r#"[{ "type": "file", "path": "pointer.gguf", "oid": "x" }]"#;
    let entries: Vec<HfTreeEntry> = serde_json::from_str(json).unwrap();
    assert!(entries[0].size.is_none());
  }

  #[test]
  fn build_tree_url_targets_requested_branch() {
    // Regression: the `/tree/<branch>` segment must reflect the
    // branch argument so the `main` → `master` fallback actually
    // hits the second branch rather than re-issuing /main twice.
    let main = build_tree_url(
      "https://huggingface.co",
      "Qwen",
      "Qwen2.5-7B-Instruct-GGUF",
      "main",
    )
    .unwrap();
    assert_eq!(
      main.path(),
      "/api/models/Qwen/Qwen2.5-7B-Instruct-GGUF/tree/main"
    );
    let master =
      build_tree_url("https://huggingface.co", "owner", "legacy-repo", "master").unwrap();
    assert_eq!(master.path(), "/api/models/owner/legacy-repo/tree/master");
  }

  #[test]
  fn build_tree_url_percent_encodes_owner_and_name() {
    // Even though parse_repo_id rejects `/` in name, owners and
    // names can legally contain `.` and other allowed-but-special
    // characters; the path-segment builder must encode anything that
    // would collide with the URL grammar. `?` would otherwise be
    // parsed as the start of the query string and the
    // `/tree/<branch>` token would silently disappear.
    let url = build_tree_url("https://huggingface.co", "owner", "weird?name", "main").unwrap();
    assert_eq!(
      url.path(),
      "/api/models/owner/weird%3Fname/tree/main",
      "path-special characters must be percent-encoded into the segment"
    );
  }

  #[test]
  fn build_search_url_includes_search_for_non_trending_sorts() {
    for sort in [
      HfSortKey::Downloads,
      HfSortKey::Likes,
      HfSortKey::RecentlyUpdated,
    ] {
      let url = build_search_url("https://huggingface.co", "qwen", sort, None).expect("build url");
      let q = url.query().unwrap_or("");
      assert!(
        q.contains("search=qwen"),
        "{sort:?} must carry the search param: {q}"
      );
      assert!(q.contains(&format!("sort={}", sort.as_query_token())));
    }
  }

  #[test]
  fn build_search_url_carries_search_and_filter_for_every_sort_including_trending() {
    // Regression: with `sort=trendingScore` (the renamed trending
    // token), the HF API accepts `search` and `filter=gguf` natively
    // — the URL shape is uniform across every sort. The previous
    // round of carve-outs (drop both params under trending) was
    // chasing the 400 symptom rather than the rename root cause.
    for sort in [
      HfSortKey::Downloads,
      HfSortKey::Likes,
      HfSortKey::RecentlyUpdated,
      HfSortKey::Trending,
    ] {
      let url = build_search_url("https://huggingface.co", "qwen", sort, None).expect("build url");
      let q = url.query().unwrap_or("");
      assert!(q.contains("search=qwen"), "{sort:?} must carry search: {q}");
      assert!(q.contains("filter=gguf"), "{sort:?} must carry filter: {q}");
      assert!(q.contains(&format!("sort={}", sort.as_query_token())));
    }
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
