//! Network egress policy for the init wizard / doctor / pull.
//!
//! Three guards travel together:
//! - **Host allowlist** — only hosts on the curated list are reachable.
//!   Blocks SSRF-via-redirect and stops `LLAMASTASH_*` env confusion
//!   from quietly redirecting traffic to an attacker-controlled host.
//! - **HTTPS only** — `http://` is refused before any DNS lookup. The
//!   redirect chain inherits this constraint per hop.
//! - **Redirect cap** — three hops max. Long chains are the dominant
//!   signal of a hostile mirror, and llama.cpp + HF have one-hop CDN
//!   redirects in production.
//!
//! No IP-class filter in v2: every public-internet host on the
//! allowlist is operated by a known third party (GitHub, HuggingFace);
//! a DNS-rebinding redirect to `127.0.0.1` would still need the
//! resolved hostname to be on the allowlist. Add IP-class checking
//! post-v2 if a real threat materialises (see plan §"Future
//! Considerations").

use std::collections::BTreeSet;

/// Default allowlist hosts. Curated from Unit 1 spike outputs:
/// - `api.github.com` for the llama.cpp Releases REST API.
/// - `github.com` for browser-download asset URLs (302-redirected to
///   the next entry).
/// - `objects.githubusercontent.com` is the legacy GH Releases asset
///   CDN; some older releases still resolve there.
/// - `release-assets.githubusercontent.com` is the current GH Releases
///   asset CDN (GitHub migrated browser-download redirects to this
///   host in 2024).
/// - `huggingface.co` for the HF Hub metadata + non-LFS files.
/// - `cdn-lfs.huggingface.co` is the LFS CDN HF redirects to.
/// - `cas-bridge.xethub.hf.co` is the XET-hash CDN (large LFS files)
///   HuggingFace redirects to.
pub const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
  "api.github.com",
  "github.com",
  "objects.githubusercontent.com",
  "release-assets.githubusercontent.com",
  "huggingface.co",
  "cdn-lfs.huggingface.co",
  "cdn-lfs-us-1.huggingface.co",
  "cdn-lfs-eu-1.huggingface.co",
  "cas-bridge.xethub.hf.co",
];

/// Maximum redirect chain length. Three covers the GitHub Releases
/// pattern (api → browser → CDN) with one slack hop; HF's CDN chain
/// fits in two.
pub const MAX_REDIRECTS: usize = 3;

/// Maximum bytes per single fetch unless the caller passes a higher
/// cap explicitly. 1 GiB is enough for any one llama.cpp asset
/// (heaviest is the Windows CUDA + HIP zip at ~320 MB); HF model
/// downloads override this cap with a per-shard value derived from
/// the disk-space precheck (R64).
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// The fetch contract's URL scheme requirement.
pub const REQUIRED_SCHEME: &str = "https";

/// Effective allowlist resolved at `FetchClient` build time. Combines
/// `DEFAULT_ALLOWED_HOSTS` with any caller-supplied additions (e.g.
/// the snapshot host the CI workflow publishes to).
///
/// `subdomain_match` controls whether a listed host like
/// `huggingface.co` should also accept `cdn-lfs.huggingface.co`,
/// `*.huggingface.co`, etc. Off by default — the v2 FetchClient
/// allowlist enumerates each CDN host explicitly so DNS-rebinding
/// surprises can't sneak through. Turned on for HF endpoint
/// validation where any HF subdomain is acceptable.
#[derive(Debug, Clone, Default)]
pub struct HostAllowlist {
  hosts: BTreeSet<String>,
  subdomain_match: bool,
}

impl HostAllowlist {
  /// Default allowlist for v2.
  pub fn default_v2() -> Self {
    let mut hosts = BTreeSet::new();
    for h in DEFAULT_ALLOWED_HOSTS {
      hosts.insert((*h).to_string());
    }
    Self {
      hosts,
      subdomain_match: false,
    }
  }

  /// Build an allowlist from a fixed list of hosts. Used by callers
  /// that don't want the v2 default set (e.g. HF endpoint validation,
  /// which has its own narrow allowlist + subdomain matching).
  pub fn from_hosts<I, S>(hosts: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    Self {
      hosts: hosts.into_iter().map(|h| h.into()).collect(),
      subdomain_match: false,
    }
  }

  /// Extend with additional hosts. Used by the snapshot fetcher to
  /// reach the rolling-release host without bloating the default.
  pub fn with_additional(mut self, extra: impl IntoIterator<Item = String>) -> Self {
    self.hosts.extend(extra);
    self
  }

  /// Accept subdomains of any listed host (`example.com` matches
  /// `a.example.com` and `a.b.example.com`).
  pub fn with_subdomain_matching(mut self, on: bool) -> Self {
    self.subdomain_match = on;
    self
  }

  /// Whether `host` is on the allowlist (case-insensitive match —
  /// reqwest gives us host strings normalised to lowercase, but we
  /// match defensively).
  pub fn contains(&self, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if self.subdomain_match {
      self
        .hosts
        .iter()
        .any(|a| host == *a || host.ends_with(&format!(".{a}")))
    } else {
      self.hosts.contains(&host)
    }
  }
}

/// Classify a redirect/request URL against the policy. Returns an
/// actionable `FetchError`-shaped reason on refusal.
pub fn check_url(url: &reqwest::Url, allowlist: &HostAllowlist) -> Result<(), UrlRefusal> {
  if url.scheme() != REQUIRED_SCHEME {
    return Err(UrlRefusal::Scheme {
      got: url.scheme().to_string(),
    });
  }
  let host = url
    .host_str()
    .ok_or(UrlRefusal::MissingHost)?
    .to_ascii_lowercase();
  if !allowlist.contains(&host) {
    return Err(UrlRefusal::HostNotAllowed { host });
  }
  Ok(())
}

/// Reasons `check_url` may refuse. `FetchClient::get_bytes` maps these
/// to typed `FetchError` variants so callers branch on cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlRefusal {
  Scheme { got: String },
  MissingHost,
  HostNotAllowed { host: String },
}

#[cfg(test)]
mod tests {
  use super::*;

  fn allow(extra: &[&str]) -> HostAllowlist {
    HostAllowlist::default_v2().with_additional(extra.iter().map(|s| (*s).to_string()))
  }

  #[test]
  fn https_against_allowlisted_host_is_accepted() {
    let url =
      reqwest::Url::parse("https://api.github.com/repos/ggml-org/llama.cpp/releases").unwrap();
    assert!(check_url(&url, &allow(&[])).is_ok());
  }

  #[test]
  fn http_scheme_is_refused() {
    let url = reqwest::Url::parse("http://api.github.com/").unwrap();
    let err = check_url(&url, &allow(&[])).unwrap_err();
    assert_eq!(err, UrlRefusal::Scheme { got: "http".into() });
  }

  #[test]
  fn unknown_host_is_refused() {
    let url = reqwest::Url::parse("https://evil.example.com/").unwrap();
    let err = check_url(&url, &allow(&[])).unwrap_err();
    assert!(matches!(err, UrlRefusal::HostNotAllowed { .. }));
  }

  #[test]
  fn user_supplied_allowlist_extension_is_honoured() {
    let url =
      reqwest::Url::parse("https://snapshot.llamastash.dev/benchmark-snapshot.json").unwrap();
    let allowlist = allow(&["snapshot.llamastash.dev"]);
    assert!(check_url(&url, &allowlist).is_ok());
  }

  #[test]
  fn loopback_literal_host_is_refused_via_allowlist() {
    let url = reqwest::Url::parse("https://127.0.0.1:8080/").unwrap();
    let err = check_url(&url, &allow(&[])).unwrap_err();
    assert!(matches!(err, UrlRefusal::HostNotAllowed { .. }));
  }

  #[test]
  fn subdomain_match_accepts_listed_root_and_subdomains() {
    let al = HostAllowlist::from_hosts(["huggingface.co"]).with_subdomain_matching(true);
    let root = reqwest::Url::parse("https://huggingface.co/").unwrap();
    let cdn = reqwest::Url::parse("https://cdn-lfs.huggingface.co/x").unwrap();
    let nested = reqwest::Url::parse("https://us1.cdn-lfs.huggingface.co/x").unwrap();
    assert!(check_url(&root, &al).is_ok());
    assert!(check_url(&cdn, &al).is_ok());
    assert!(check_url(&nested, &al).is_ok());
  }

  #[test]
  fn subdomain_match_refuses_lookalike_suffix() {
    let al = HostAllowlist::from_hosts(["huggingface.co"]).with_subdomain_matching(true);
    let evil = reqwest::Url::parse("https://huggingface.co.attacker.com/").unwrap();
    let err = check_url(&evil, &al).unwrap_err();
    assert!(matches!(err, UrlRefusal::HostNotAllowed { .. }));
  }

  #[test]
  fn default_allowlist_does_not_match_subdomains() {
    // Sanity: turning subdomain_match off (the FetchClient default)
    // refuses anything not on the explicit list — even a subdomain
    // of a listed host.
    let al = HostAllowlist::from_hosts(["huggingface.co"]);
    let cdn = reqwest::Url::parse("https://cdn-lfs.huggingface.co/x").unwrap();
    let err = check_url(&cdn, &al).unwrap_err();
    assert!(matches!(err, UrlRefusal::HostNotAllowed { .. }));
  }
}
