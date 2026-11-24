//! Shared building blocks for "merge an additions object into a
//! config file" workflows.
//!
//! The llamastash config writer and the init wizard's external-tool
//! patchers produce the same [`DiffEntry`] rows, run them through
//! the same redaction allowlist ([`SECRET_PATH_TOKENS`]), and
//! render through the same human formatter so a token can't leak
//! through one surface that the other would have masked. A new
//! patcher inherits the redaction policy by construction.
//!
//! The field name `value_yaml` is historical (predates JSON
//! patchers); treat it as "value's serialised textual form in the
//! patcher's native format" (YAML for the YAML writer, JSON-inline
//! for the JSON patchers).

use serde::Serialize;

/// One row of a config-merge structural diff. `kind` distinguishes
/// "added the key entirely" from "value changed". `value_yaml` is
/// the textual serialisation of the *new* value in the target
/// format (treated as opaque by the redaction + render layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
  pub path: String,
  pub kind: DiffKind,
  pub value_yaml: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
  Added,
  Changed,
}

/// Substrings that mark a dotted config path as secret-bearing.
/// Case-insensitive match — `HF_Token`, `user.Credential`,
/// `api_secret_key` all redact. Kept narrow on purpose; a path
/// like `model` won't false-positive.
pub const SECRET_PATH_TOKENS: &[&str] = &["token", "secret", "password", "key", "credential"];

/// Redacted view of a [`DiffEntry`] — what we print to stderr / emit
/// to JSON consumers.
#[derive(Debug, Clone, Serialize)]
pub struct RedactedDiffEntry {
  pub path: String,
  pub kind: &'static str,
  pub value_yaml: String,
}

/// Apply the secret-path allowlist to a diff. Returns the JSON-
/// emission shape; the human renderer ([`render_human`]) consumes
/// the same vec so a token can't leak through a divergent code
/// path.
pub fn redact_diff(diff: &[DiffEntry]) -> Vec<RedactedDiffEntry> {
  diff
    .iter()
    .map(|d| RedactedDiffEntry {
      path: d.path.clone(),
      kind: match d.kind {
        DiffKind::Added => "added",
        DiffKind::Changed => "changed",
      },
      value_yaml: if path_is_secret(&d.path) {
        "<redacted>".to_string()
      } else {
        d.value_yaml.clone()
      },
    })
    .collect()
}

/// Case-insensitive substring match against [`SECRET_PATH_TOKENS`].
pub fn path_is_secret(path: &str) -> bool {
  let lower = path.to_ascii_lowercase();
  SECRET_PATH_TOKENS.iter().any(|t| lower.contains(t))
}

/// Render a redacted diff as a `+ key: value` text block suitable
/// for stderr preview. Stable shape — the wizard's `--verbose`
/// output, the interactive confirm preview, and the external-tool
/// dry-run all route through here.
pub fn render_human(diff: &[RedactedDiffEntry]) -> String {
  if diff.is_empty() {
    return "  (no changes)\n".to_string();
  }
  let mut out = String::new();
  for row in diff {
    let marker = match row.kind {
      "added" => "+",
      "changed" => "~",
      _ => " ",
    };
    out.push_str(&format!("  {marker} {}: {}\n", row.path, row.value_yaml));
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(path: &str, kind: DiffKind, value: &str) -> DiffEntry {
    DiffEntry {
      path: path.to_string(),
      kind,
      value_yaml: value.to_string(),
    }
  }

  #[test]
  fn path_is_secret_is_case_insensitive() {
    assert!(path_is_secret("HF_Token"));
    assert!(path_is_secret("user.Credential"));
    assert!(path_is_secret("api_secret"));
    assert!(path_is_secret("user.password"));
    assert!(path_is_secret("openAiApiKey"));
    assert!(!path_is_secret("port_range.start"));
    assert!(!path_is_secret("model"));
  }

  #[test]
  fn secret_paths_get_redacted_value_only() {
    let diff = vec![
      entry("hf_token", DiffKind::Added, "hf_xxxxxxxxxxxxxxxxxx"),
      entry("port_range.start", DiffKind::Changed, "41100"),
      entry("api_secret", DiffKind::Added, "shhh"),
      entry("user.password", DiffKind::Added, "letmein"),
      entry("custom_credential_token", DiffKind::Added, "abc"),
    ];
    let redacted = redact_diff(&diff);
    let by_path = |p: &str| {
      redacted
        .iter()
        .find(|r| r.path == p)
        .expect("path present")
        .clone()
    };
    assert_eq!(by_path("hf_token").value_yaml, "<redacted>");
    assert_eq!(by_path("api_secret").value_yaml, "<redacted>");
    assert_eq!(by_path("user.password").value_yaml, "<redacted>");
    assert_eq!(by_path("custom_credential_token").value_yaml, "<redacted>");
    assert_eq!(by_path("port_range.start").value_yaml, "41100");
  }

  #[test]
  fn render_human_uses_added_and_changed_markers() {
    let diff = vec![
      RedactedDiffEntry {
        path: "llama_server_path".into(),
        kind: "added",
        value_yaml: "/opt/llama-server".into(),
      },
      RedactedDiffEntry {
        path: "port_range.start".into(),
        kind: "changed",
        value_yaml: "50000".into(),
      },
    ];
    let s = render_human(&diff);
    assert!(s.contains("+ llama_server_path"));
    assert!(s.contains("~ port_range.start"));
  }

  #[test]
  fn render_human_handles_empty_diff() {
    let s = render_human(&[]);
    assert!(s.contains("(no changes)"));
  }
}
