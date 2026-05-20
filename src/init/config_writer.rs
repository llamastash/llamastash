//! Init wizard's config.yaml write half (R66 / R67 / R68 / R72).
//!
//! Thin user-facing wrapper around Unit 2's
//! [`crate::config::writer::merge_and_write`] primitive that adds:
//! - human-friendly + JSON diff rendering,
//! - secret-key redaction (path matches `token` / `secret` /
//!   `password` substring → value rendered as `<redacted>`),
//! - --yes / interactive confirm hook,
//! - managed_keys list build that Unit 10 then stamps into
//!   `_init_snapshot.json`.
//!
//! The actual atomic rename + 0600 mode + symlink/parent-mode refusal
//! all live in Unit 2's primitive — this module never touches the
//! filesystem itself.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::writer::{
  diff, merge, merge_and_write, read_or_default, DiffEntry, DiffKind, WriteError, WriteOutcome,
};

/// Substrings that mark a YAML path as secret-bearing. Case-insensitive
/// match against the dotted path; a hit redacts the rendered value.
pub const SECRET_PATH_TOKENS: &[&str] = &["token", "secret", "password", "key", "credential"];

/// Render result returned to the wizard. `diff_human` is what the
/// interactive flow prints; `diff_json` is what `init --json` emits
/// under `config.diff` (always the same redaction pass applied so
/// downstream agents see exactly what the user did).
#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
  pub path: PathBuf,
  pub written_bytes: u64,
  pub managed_keys: Vec<String>,
  pub diff_human: String,
  pub diff_json: Vec<RedactedDiffEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedactedDiffEntry {
  pub path: String,
  pub kind: &'static str,
  pub value_yaml: String,
}

/// How the wrapper handles the diff preview. There is currently no
/// blocking dialoguer prompt — the actual confirm prompt lives in
/// Unit 10's interactive flow and is plumbed past this wrapper.
///
/// - `show_diff_preview = true` renders the diff to stderr before the
///   write so a human can audit it. Unit 10's prompt is layered on
///   top in interactive mode; `--yes` / `--json` runs pass `false`.
/// - `verbose = true` always renders the diff to stderr (legacy
///   `--verbose` semantics). Mutually compatible with the preview
///   flag: setting both is the same as either one.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
  pub show_diff_preview: bool,
  pub verbose: bool,
}

/// Stamp a managed key's path into the on-disk
/// `_init_snapshot.json.managed_keys` list. Filtered for the dotted
/// paths the wizard *actually* wrote (per the diff outcome) so a
/// user-edited key never falsely appears as wizard-owned.
pub fn managed_keys_from_diff(diff: &[DiffEntry]) -> Vec<String> {
  diff
    .iter()
    .map(|d| d.path.clone())
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .collect()
}

/// Apply the redaction allowlist to a diff. Returns the JSON-emission
/// shape; the human renderer goes through the same path so a token
/// can't leak through one channel.
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

fn path_is_secret(path: &str) -> bool {
  let lower = path.to_ascii_lowercase();
  SECRET_PATH_TOKENS.iter().any(|t| lower.contains(t))
}

/// Render the redacted diff as a `+ key: value` text block suitable
/// for stderr preview. Stable shape — Unit 10's verbose output and the
/// future `init --diff-only` use this directly.
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

/// Diff render produced by [`dry_run_diff`]. No bytes touched on
/// disk; the wizard can present this to the user, take a confirm
/// answer, and then call [`write_with_diff`] only when accepted.
#[derive(Debug, Clone, Serialize)]
pub struct DryRunDiff {
  pub diff_human: String,
  pub diff_json: Vec<RedactedDiffEntry>,
}

/// Compute the same redacted diff [`write_with_diff`] would emit, but
/// without writing the file. Used by the interactive wizard's confirm
/// flow so the diff is visible before the user commits to the write.
pub fn dry_run_diff(path: &Path, additions: serde_yaml::Value) -> Result<DryRunDiff, WriteError> {
  let current = read_or_default(path)?;
  let merged = merge(current.clone(), additions);
  let raw_diff = diff(&current, &merged);
  let diff_json = redact_diff(&raw_diff);
  let diff_human = render_human(&diff_json);
  Ok(DryRunDiff {
    diff_human,
    diff_json,
  })
}

/// Wizard-facing wrapper. Writes via Unit 2's primitive, applies
/// redaction, renders both forms of the diff, returns the
/// [`WriteResult`] Unit 10 stitches into its summary.
pub fn write_with_diff(
  path: &Path,
  additions: serde_yaml::Value,
  options: WriteOptions,
) -> Result<WriteResult, WriteError> {
  let outcome: WriteOutcome = merge_and_write(path, additions)?;
  let diff_json = redact_diff(&outcome.diff);
  let diff_human = render_human(&diff_json);
  if options.verbose {
    eprintln!("init config diff:\n{diff_human}");
  } else if options.show_diff_preview {
    // Interactive caller (Unit 10) renders its own confirm prompt
    // around this — we just surface the diff so the user sees
    // what would be written.
    eprintln!("config diff (preview):\n{diff_human}");
  }
  let managed_keys = managed_keys_from_diff(&outcome.diff);
  Ok(WriteResult {
    path: path.to_path_buf(),
    written_bytes: outcome.written_bytes,
    managed_keys,
    diff_human,
    diff_json,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::writer::DiffKind;

  fn entry(path: &str, kind: DiffKind, value: &str) -> DiffEntry {
    DiffEntry {
      path: path.to_string(),
      kind,
      value_yaml: value.to_string(),
    }
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
    // Non-secret path keeps its value.
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

  #[test]
  fn managed_keys_from_diff_deduplicates() {
    let diff = vec![
      entry("a", DiffKind::Added, "x"),
      entry("b", DiffKind::Changed, "y"),
      entry("a", DiffKind::Changed, "z"),
    ];
    let keys = managed_keys_from_diff(&diff);
    assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
  }

  #[test]
  fn path_is_secret_is_case_insensitive() {
    assert!(path_is_secret("HF_Token"));
    assert!(path_is_secret("user.Credential"));
    assert!(!path_is_secret("port_range.start"));
  }

  #[test]
  fn write_with_diff_round_trips_via_unit2_primitive() {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let dir = std::env::temp_dir().join(format!(
      "llamastash-config-writer-it-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let path = dir.join("config.yaml");

    let additions: serde_yaml::Value = serde_yaml::from_str(
      "theme: latte\nllama_server_path: /opt/llama-server\nhf_token: hf_xxx\n",
    )
    .unwrap();
    let result = write_with_diff(&path, additions, WriteOptions::default()).expect("write");
    assert!(result.written_bytes > 0);
    // hf_token is in the diff but the redacted form replaces the value.
    let redacted_hf = result
      .diff_json
      .iter()
      .find(|r| r.path == "hf_token")
      .expect("hf_token diff row");
    assert_eq!(redacted_hf.value_yaml, "<redacted>");

    fs::remove_dir_all(&dir).ok();
  }
}
