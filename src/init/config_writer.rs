//! Init wizard's config.yaml write half.
//!
//! Thin user-facing wrapper around the
//! [`crate::config::writer::merge_and_write`] primitive that adds diff
//! rendering, secret-key redaction, and the managed_keys list the wizard
//! stamps into `_init_snapshot.json`.
//!
//! The actual atomic rename + 0600 mode + symlink/parent-mode refusal
//! all live in that primitive — this module never touches the
//! filesystem itself.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::writer::{
  diff, merge, merge_and_write, read_or_default, DiffEntry, WriteError, WriteOutcome,
};

pub use crate::util::config_patch::{
  path_is_secret, redact_diff, render_human, RedactedDiffEntry, SECRET_PATH_TOKENS,
};

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

/// How the wrapper handles the diff preview. There is currently no
/// blocking dialoguer prompt — the actual confirm prompt lives in
/// the wizard's interactive flow and is plumbed past this wrapper.
///
/// - `show_diff_preview = true` renders the diff to stderr before the
///   write so a human can audit it. The wizard's prompt is layered on
///   top in interactive mode; `--recommended` / `--json` runs pass
///   `false`.
/// - `verbose = true` always renders the diff to stderr (legacy
///   `--verbose` semantics). Mutually compatible with the preview
///   flag: setting both is the same as either one.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
  pub show_diff_preview: bool,
  pub verbose: bool,
}

/// The dotted paths the wizard *actually* wrote (per the diff outcome),
/// so a user-edited key never falsely appears as wizard-owned.
pub fn managed_keys_from_diff(diff: &[DiffEntry]) -> Vec<String> {
  diff
    .iter()
    .map(|d| d.path.clone())
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .collect()
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
pub fn dry_run_diff(path: &Path, additions: yaml_serde::Value) -> Result<DryRunDiff, WriteError> {
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

/// Wizard-facing wrapper. Writes via the config-writer primitive, applies
/// redaction, renders both forms of the diff, returns the
/// [`WriteResult`] the wizard stitches into its summary.
pub fn write_with_diff(
  path: &Path,
  additions: yaml_serde::Value,
  options: WriteOptions,
) -> Result<WriteResult, WriteError> {
  let outcome: WriteOutcome = merge_and_write(path, additions)?;
  let diff_json = redact_diff(&outcome.diff);
  let diff_human = render_human(&diff_json);
  if options.verbose {
    eprintln!("init config diff:\n{diff_human}");
  } else if options.show_diff_preview {
    // Interactive caller renders its own confirm prompt
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

    let additions: yaml_serde::Value = yaml_serde::from_str(
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
