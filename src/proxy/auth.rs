//! Bearer-token auth for the LAN-exposed proxy data plane.
//!
//! Mirrors the control-plane's `IpcToken` story (`src/daemon/auth.rs`):
//! a high-entropy key, constant-time comparison, and a redacted
//! `Debug` so the secret never lands in a log line. The two differ in
//! lifetime — the control-plane token is regenerated every daemon
//! start and lives in `runtime.json`, whereas the proxy key is durable
//! (clients hard-code it) and lives in `config.yaml`.
//!
//! Auth is enforced only when a key is configured. The default
//! loopback, keyless posture skips the check before touching the
//! request headers ([`ProxyAuth::enforced`] is a single bool), so the
//! benchmarked hot path is unchanged.

use base64::{
  engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
  Engine,
};
use hyper::header::{HeaderMap, AUTHORIZATION};
use rand::TryRngCore;

use crate::util::http_auth::{constant_time_eq, extract_bearer};

/// Raw entropy bytes behind a generated key (before encoding). 32
/// bytes is ~256 bits — the same bar as the control-plane token.
const KEY_BYTES: usize = 32;

/// Prefix on generated keys so they read as OpenAI-style `sk-` keys in
/// client configs and are greppable in a user's shell history.
const KEY_PREFIX: &str = "sk-llamastash-";

/// A proxy bearer key. Wraps the encoded string so equality is
/// constant-time and an accidental `Debug` never prints the secret.
#[derive(Clone)]
pub struct ProxyApiKey(String);

impl ProxyApiKey {
  /// Generate a fresh key: `sk-llamastash-<43-char base64url>`.
  /// Panics only if the OS randomness source is unavailable — the same
  /// non-recoverable state `IpcToken::generate` treats as fatal.
  pub fn generate() -> Self {
    let mut bytes = [0u8; KEY_BYTES];
    rand::rngs::OsRng
      .try_fill_bytes(&mut bytes)
      .expect("OsRng must succeed for proxy key generation");
    Self(format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
  }

  /// Wrap an existing key string (config / env path).
  pub fn from_string(raw: String) -> Self {
    Self(raw)
  }

  /// Borrow the full secret for persistence / one-time printing. The
  /// returned slice is the secret; callers must not log it.
  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Constant-time comparison against a candidate token.
  pub fn verify(&self, candidate: &str) -> bool {
    constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
  }
}

impl std::fmt::Debug for ProxyApiKey {
  // Suppress the secret in any Debug output.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ProxyApiKey")
      .field("len", &self.0.len())
      .finish()
  }
}

/// Per-listener auth policy. `None` key = auth disabled (loopback,
/// same-UID posture). `Some` = a `Bearer` token is required on the
/// data routes.
#[derive(Clone, Debug, Default)]
pub struct ProxyAuth {
  key: Option<ProxyApiKey>,
}

impl ProxyAuth {
  /// Build from the resolved config key. `None` or a blank string
  /// disables auth.
  pub fn new(key: Option<String>) -> Self {
    Self {
      key: key
        .filter(|k| !k.trim().is_empty())
        .map(ProxyApiKey::from_string),
    }
  }

  /// Whether auth is enforced (a key is configured). Cheap — a single
  /// `Option::is_some`. Callers gate on this before doing any header
  /// work so the keyless path stays free.
  pub fn enforced(&self) -> bool {
    self.key.is_some()
  }

  /// Whether the request is authorized: `true` when auth is disabled
  /// or a recognised header presents the configured key. Three forms are
  /// accepted, all compared in constant time:
  ///
  /// - `x-api-key: <key>` — the Anthropic path. Claude Code and the
  ///   Anthropic SDK send the key in this header (not `Authorization`)
  ///   when pointed at the proxy via `ANTHROPIC_BASE_URL`. The whole
  ///   header value is the key.
  /// - `Authorization: Bearer <key>` — the API path (OpenAI SDKs, agent
  ///   harnesses).
  /// - `Authorization: Basic base64(user:<key>)` — the browser path. A
  ///   browser can't send a `Bearer` header by navigating, but it speaks
  ///   Basic natively: on a `WWW-Authenticate: Basic` challenge it
  ///   prompts, the user pastes the proxy key as the **password**, and
  ///   the browser resends it per-origin. The username is ignored. This
  ///   is what makes the `/ui` surface reachable over LAN with the same
  ///   auto-provisioned key (plan §"LAN /ui + browser auth").
  ///
  /// `false` on a missing / malformed / wrong credential.
  pub fn check(&self, headers: &HeaderMap) -> bool {
    let Some(key) = &self.key else {
      return true;
    };
    // Anthropic clients authenticate with `x-api-key`; the header value
    // is the bare key. Check it first, then fall through to the
    // `Authorization` schemes so a request carrying both still works.
    if let Some(api_key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
      if key.verify(api_key.trim()) {
        return true;
      }
    }
    let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
      return false;
    };
    if let Some(tok) = extract_bearer(value) {
      return key.verify(tok);
    }
    if let Some(pass) = extract_basic_password(value) {
      return key.verify(&pass);
    }
    false
  }
}

/// Whether a `Host` header value (with an optional `:port`) names loopback:
/// `localhost`, any `*.localhost` name, or an IP literal inside
/// `127.0.0.0/8` / `::1`. `*.localhost` counts because RFC 6761 makes
/// browsers resolve the whole suffix to loopback, so a DNS-rebinding page
/// cannot own such a name. Case-insensitive; everything else is refused.
pub(crate) fn host_is_loopback(host: &str) -> bool {
  let host = host.trim();
  // An IPv6 literal keeps its brackets (`[::1]:11435`); a bare host may
  // carry a `:port` made of digits (a bracket-less IPv6 is invalid in a
  // Host header, so dropping only a numeric port cannot mangle a real one).
  let bare = match host.split_once(']') {
    Some((inner, _)) => inner.strip_prefix('[').unwrap_or(inner),
    None => match host.rsplit_once(':') {
      Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
      _ => host,
    },
  };
  let name = bare.to_ascii_lowercase();
  if name == "localhost" || name.ends_with(".localhost") {
    return true;
  }
  bare
    .parse::<std::net::IpAddr>()
    .is_ok_and(|ip| ip.is_loopback())
}

/// Whether an `Origin` header value may drive the proxy: the page's own host
/// over `http` or `https` (`https` is a TLS reverse proxy in front of it), or
/// an extension / app scheme, which a web page cannot send. `null` is refused
/// because a sandboxed iframe or a `file://` page sends it.
pub(crate) fn origin_is_allowed(origin: &str, host: Option<&str>) -> bool {
  let origin = origin.trim();
  if origin.eq_ignore_ascii_case("null") {
    return false;
  }
  let Some(scheme_end) = origin.find("://") else {
    return false;
  };
  let scheme = &origin[..scheme_end];
  if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
    return true;
  }
  host.is_some_and(|h| origin[scheme_end + 3..].eq_ignore_ascii_case(h))
}

/// Pull the password out of a `Basic base64(user:pass)` header value.
/// The username half is discarded — browsers send the proxy key as the
/// password. Returns `None` when the scheme isn't `Basic`, the base64
/// is malformed, the decoded bytes aren't UTF-8, or there's no `:`
/// separator. Browsers encode Basic credentials with standard (padded)
/// base64, so we decode with the `STANDARD` engine, not the URL-safe
/// one used for key generation.
fn extract_basic_password(header: &str) -> Option<String> {
  let encoded = header
    .strip_prefix("Basic ")
    .or_else(|| header.strip_prefix("basic "))?;
  let decoded = STANDARD.decode(encoded.trim()).ok()?;
  let text = String::from_utf8(decoded).ok()?;
  // RFC 7617: split on the first colon — the password may itself
  // contain colons, the userid may not.
  let (_user, pass) = text.split_once(':')?;
  Some(pass.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use hyper::header::HeaderValue;

  fn headers_with(auth: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(AUTHORIZATION, HeaderValue::from_str(auth).unwrap());
    h
  }

  #[test]
  fn generated_key_has_prefix_and_is_unique() {
    let a = ProxyApiKey::generate();
    let b = ProxyApiKey::generate();
    assert!(a.as_str().starts_with(KEY_PREFIX));
    // prefix + 43 base64url chars (ceil(32 * 8 / 6) = 43).
    assert_eq!(a.as_str().len(), KEY_PREFIX.len() + 43);
    assert_ne!(a.as_str(), b.as_str(), "two fresh keys collided");
  }

  #[test]
  fn key_verifies_self_and_rejects_others() {
    let k = ProxyApiKey::generate();
    assert!(k.verify(k.as_str()));
    assert!(!k.verify("sk-llamastash-wrong"));
    assert!(!k.verify(""));
  }

  #[test]
  fn host_is_loopback_accepts_loopback_names_and_literals() {
    for ok in [
      "127.0.0.1:11435",
      "localhost",
      "LOCALHOST:1",
      "foo.localhost:11435",
      "[::1]:11435",
      "127.1.2.3",
    ] {
      assert!(host_is_loopback(ok), "{ok} must pass");
    }
    for bad in [
      "evil.example",
      "localhost.evil.example",
      "192.168.1.5:11435",
      "[::ffff:8.8.8.8]",
      "",
    ] {
      assert!(!host_is_loopback(bad), "{bad} must be refused");
    }
  }

  #[test]
  fn origin_is_allowed_pins_same_origin_and_extension_schemes() {
    assert!(origin_is_allowed(
      "http://127.0.0.1:11435",
      Some("127.0.0.1:11435")
    ));
    assert!(origin_is_allowed("chrome-extension://abc", None));
    assert!(!origin_is_allowed("null", Some("127.0.0.1:11435")));
    assert!(!origin_is_allowed(
      "http://evil.example",
      Some("127.0.0.1:11435")
    ));
    // A TLS reverse proxy in front keeps the host and changes the scheme.
    assert!(origin_is_allowed(
      "https://llm.example.com",
      Some("llm.example.com")
    ));
    assert!(!origin_is_allowed(
      "https://evil.example",
      Some("llm.example.com")
    ));
    // A different spelling of the same machine is a different origin here.
    assert!(!origin_is_allowed(
      "http://localhost:11435",
      Some("127.0.0.1:11435")
    ));
  }

  #[test]
  fn debug_redacts_the_secret() {
    let k = ProxyApiKey::from_string("sk-llamastash-supersecret".into());
    let shown = format!("{k:?}");
    assert!(
      !shown.contains("supersecret"),
      "Debug leaked the key: {shown}"
    );
  }

  #[test]
  fn disabled_auth_allows_everything() {
    let auth = ProxyAuth::new(None);
    assert!(!auth.enforced());
    assert!(auth.check(&HeaderMap::new()));
    // A blank key string is treated as no key.
    assert!(!ProxyAuth::new(Some("   ".into())).enforced());
  }

  #[test]
  fn enabled_auth_requires_matching_bearer() {
    let auth = ProxyAuth::new(Some("sk-llamastash-k3y".into()));
    assert!(auth.enforced());
    // Correct token passes.
    assert!(auth.check(&headers_with("Bearer sk-llamastash-k3y")));
    // Wrong token and missing header reject.
    assert!(!auth.check(&headers_with("Bearer nope")));
    assert!(!auth.check(&HeaderMap::new()));
    // A non-base64 `Basic` value is malformed → reject (no panic).
    assert!(!auth.check(&headers_with("Basic sk-llamastash-k3y")));
  }

  #[test]
  fn enabled_auth_accepts_anthropic_x_api_key() {
    // Anthropic path: Claude Code / the Anthropic SDK send the bare key
    // in `x-api-key`, not `Authorization`.
    let auth = ProxyAuth::new(Some("sk-llamastash-k3y".into()));
    let mut ok = HeaderMap::new();
    ok.insert("x-api-key", HeaderValue::from_static("sk-llamastash-k3y"));
    assert!(auth.check(&ok));
    // Surrounding whitespace is tolerated.
    let mut padded = HeaderMap::new();
    padded.insert(
      "x-api-key",
      HeaderValue::from_static("  sk-llamastash-k3y  "),
    );
    assert!(auth.check(&padded));
    // Wrong key rejects.
    let mut bad = HeaderMap::new();
    bad.insert("x-api-key", HeaderValue::from_static("nope"));
    assert!(!auth.check(&bad));
    // A wrong `x-api-key` still lets a correct `Authorization` pass
    // (both-headers request).
    let mut both = HeaderMap::new();
    both.insert("x-api-key", HeaderValue::from_static("nope"));
    both.insert(
      AUTHORIZATION,
      HeaderValue::from_static("Bearer sk-llamastash-k3y"),
    );
    assert!(auth.check(&both));
  }

  #[test]
  fn enabled_auth_accepts_basic_with_key_as_password() {
    // Browser path: the key rides in as the Basic *password*; the
    // username half is ignored. Same key, different scheme.
    let auth = ProxyAuth::new(Some("sk-llamastash-k3y".into()));
    let ok = format!("Basic {}", STANDARD.encode("x:sk-llamastash-k3y"));
    assert!(auth.check(&headers_with(&ok)));
    // An empty username is fine too (RFC 7617 allows it).
    let ok_empty_user = format!("Basic {}", STANDARD.encode(":sk-llamastash-k3y"));
    assert!(auth.check(&headers_with(&ok_empty_user)));
    // Wrong password rejects.
    let bad = format!("Basic {}", STANDARD.encode("x:wrong"));
    assert!(!auth.check(&headers_with(&bad)));
    // No colon separator → malformed → reject.
    let no_colon = format!("Basic {}", STANDARD.encode("sk-llamastash-k3y"));
    assert!(!auth.check(&headers_with(&no_colon)));
  }
}
