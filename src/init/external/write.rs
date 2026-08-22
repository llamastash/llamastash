//! Format-aware read / merge / write glue.
//!
//! Both JSON and YAML targets route through the same merge logic
//! ([`super::merge`]) — only the read-current and serialise-final
//! steps care about format. The atomic write itself is
//! [`crate::util::atomic_write::write_secure`], shared with the
//! daemon state store and llamastash's own config writer.

use std::path::Path;

use serde_json::Value;

use crate::util::atomic_write::write_secure;
use crate::util::config_patch::DiffEntry;

use super::{merge, Format, PatchContext, PatchError, ToolPatcher};

/// Read the file at `path` and parse it according to `format`.
/// Missing or empty files return an empty JSON object (so a fresh
/// install gets clean "Added" diff rows). YAML files are parsed
/// via `yaml_serde` then converted into JSON via `serde_json::to_value`
/// so the merge stays JSON-native.
pub fn read_current(
  tool_id: &'static str,
  path: &Path,
  format: Format,
) -> Result<Value, PatchError> {
  let raw = match std::fs::read_to_string(path) {
    Ok(s) if s.trim().is_empty() => return Ok(Value::Object(Default::default())),
    Ok(s) => s,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
      return Ok(Value::Object(Default::default()))
    }
    Err(e) => {
      return Err(PatchError::Read {
        tool_id,
        path: path.to_path_buf(),
        error: e.to_string(),
      })
    }
  };
  match format {
    Format::Json => {
      // Strip `//` / `/* */` comments AND trailing commas before
      // strict-JSON parse so a `.jsonc` file (OpenCode) or a `.json`
      // the user has annotated VSCode-style (Zed's `settings.json` is
      // JSON5-shape) parses cleanly. Both passes are string-safe —
      // comment markers and commas inside JSON string literals are
      // left alone.
      //
      // Writes ALWAYS emit strict JSON; comments in the source file
      // are not preserved across a merge.
      let cleaned = strip_trailing_commas(&strip_json_comments(&raw));
      serde_json::from_str(&cleaned).map_err(|e| PatchError::Parse {
        tool_id,
        path: path.to_path_buf(),
        format,
        error: e.to_string(),
      })
    }
    Format::Yaml => {
      let yaml: yaml_serde::Value = yaml_serde::from_str(&raw).map_err(|e| PatchError::Parse {
        tool_id,
        path: path.to_path_buf(),
        format,
        error: e.to_string(),
      })?;
      serde_json::to_value(&yaml).map_err(|e| PatchError::Parse {
        tool_id,
        path: path.to_path_buf(),
        format,
        error: format!("yaml→json: {e}"),
      })
    }
    Format::Raw => Ok(Value::Object(Default::default())),
  }
}

/// Serialise the merged JSON value back into the target format's
/// canonical text. YAML uses block style for readability; JSON uses
/// pretty-print so the user can re-read what we wrote.
fn serialise(tool_id: &'static str, merged: &Value, format: Format) -> Result<String, PatchError> {
  match format {
    Format::Json => {
      let mut s = serde_json::to_string_pretty(merged)
        .map_err(|e| PatchError::Serialise(format!("{tool_id}: {e}")))?;
      // serde_json::to_string_pretty drops trailing newline; add one
      // back so editors don't flag the file with an end-of-file warning.
      s.push('\n');
      Ok(s)
    }
    Format::Yaml => {
      yaml_serde::to_string(merged).map_err(|e| PatchError::Serialise(format!("{tool_id}: {e}")))
    }
    Format::Raw => Err(PatchError::Serialise(format!(
      "{tool_id}: Format::Raw bypasses merge — caller must use apply_raw_body"
    ))),
  }
}

/// Compute the structural diff that [`apply_merge`] would produce —
/// without writing the file. Used by [`super::dry_run`].
pub fn compute_diff(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  path: &Path,
  format: Format,
) -> Result<Vec<DiffEntry>, PatchError> {
  let current = read_current(patcher.id(), path, format)?;
  let merged = patcher.merge_with_current(current.clone(), ctx);
  Ok(merge::diff(&current, &merged))
}

/// Compute a diff for [`Format::Raw`] patchers: the entire body is
/// either Added (file missing or empty) or Changed (existing file
/// differs), with `path: "<file>"` as a single synthetic row. Lets
/// the same dry-run / apply rendering work for the env.sh writer.
pub fn compute_raw_diff(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  path: &Path,
) -> Result<Vec<DiffEntry>, PatchError> {
  let body = patcher.raw_body(ctx).ok_or_else(|| {
    PatchError::Serialise(format!(
      "{}: Format::Raw patcher must implement raw_body()",
      patcher.id()
    ))
  })?;
  let current = match std::fs::read_to_string(path) {
    Ok(s) => Some(s),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
    Err(e) => {
      return Err(PatchError::Read {
        tool_id: patcher.id(),
        path: path.to_path_buf(),
        error: e.to_string(),
      })
    }
  };
  use crate::util::config_patch::DiffKind;
  match current {
    Some(ref s) if s == &body => Ok(Vec::new()),
    Some(_) => Ok(vec![DiffEntry {
      path: file_label(path),
      kind: DiffKind::Changed,
      value_yaml: body.lines().count().to_string() + " line(s)",
    }]),
    None => Ok(vec![DiffEntry {
      path: file_label(path),
      kind: DiffKind::Added,
      value_yaml: body.lines().count().to_string() + " line(s)",
    }]),
  }
}

fn file_label(path: &Path) -> String {
  path
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("<file>")
    .to_string()
}

/// Strip `//` line comments and `/* … */` block comments while
/// leaving content inside JSON string literals alone. Idempotent on
/// strict JSON (no comment markers to strip). Used as a pre-parse
/// pass for both `.jsonc` files (OpenCode) and `.json` files the
/// user has edited VSCode-style (Zed's settings.json).
///
/// Trailing-comma stripping lives in [`strip_trailing_commas`] —
/// always paired with this pass on JSON reads.
fn strip_json_comments(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  let mut chars = input.chars().peekable();
  let mut in_string = false;
  let mut escape = false;
  while let Some(c) = chars.next() {
    if in_string {
      out.push(c);
      if escape {
        escape = false;
      } else if c == '\\' {
        escape = true;
      } else if c == '"' {
        in_string = false;
      }
      continue;
    }
    if c == '"' {
      in_string = true;
      out.push(c);
      continue;
    }
    if c == '/' {
      match chars.peek() {
        Some('/') => {
          // Line comment — drop until newline (keep newline so line
          // numbers in parse errors still line up roughly).
          chars.next();
          for nc in chars.by_ref() {
            if nc == '\n' {
              out.push('\n');
              break;
            }
          }
        }
        Some('*') => {
          // Block comment — drop until `*/`.
          chars.next();
          let mut prev = '\0';
          for nc in chars.by_ref() {
            if prev == '*' && nc == '/' {
              break;
            }
            prev = nc;
          }
        }
        _ => out.push(c),
      }
      continue;
    }
    out.push(c);
  }
  out
}

/// Strip JSONC-style trailing commas: a `,` that's followed (after
/// optional whitespace) by `}` or `]` is silently dropped. String-
/// safe — commas inside JSON string literals are left alone.
/// Idempotent on strict JSON (no trailing commas to strip).
///
/// Runs *after* [`strip_json_comments`] so a comment between the
/// comma and the closing brace doesn't hide the trailing comma:
///
/// ```text
///   "foo": 1,  // trailing comma + comment
///   }
/// ```
fn strip_trailing_commas(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  let mut in_string = false;
  let mut escape = false;
  // Pending: a comma we haven't decided about yet. Holds the comma
  // plus any whitespace seen since — flushed verbatim if we hit
  // non-whitespace non-closer; if we hit `}` or `]`, drop the comma
  // and keep just the whitespace.
  let mut pending: Option<String> = None;
  for c in input.chars() {
    if in_string {
      // Strings can't span a pending state, but defensively flush
      // before pushing the string content.
      if let Some(p) = pending.take() {
        out.push_str(&p);
      }
      out.push(c);
      if escape {
        escape = false;
      } else if c == '\\' {
        escape = true;
      } else if c == '"' {
        in_string = false;
      }
      continue;
    }
    if let Some(ref mut p) = pending {
      if c.is_whitespace() {
        p.push(c);
        continue;
      }
      if c == '}' || c == ']' {
        // Trailing comma confirmed — drop it; keep the whitespace
        // (preserves line numbers in any downstream parse error).
        let ws: String = p.chars().skip(1).collect();
        out.push_str(&ws);
        out.push(c);
        pending = None;
        continue;
      }
      // Comma turned out to be legitimate — flush as-is.
      let p_take = std::mem::take(p);
      pending = None;
      out.push_str(&p_take);
      // Fall through to process `c` normally.
    }
    if c == '"' {
      in_string = true;
      out.push(c);
      continue;
    }
    if c == ',' {
      pending = Some(",".to_string());
      continue;
    }
    out.push(c);
  }
  if let Some(p) = pending {
    out.push_str(&p);
  }
  out
}

/// Read current, merge additions, atomic-write. Returns the diff
/// and `written_bytes`. Production hot path called by [`super::apply`].
pub fn apply_merge(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  path: &Path,
  format: Format,
) -> Result<(Vec<DiffEntry>, u64), PatchError> {
  let current = read_current(patcher.id(), path, format)?;
  let merged = patcher.merge_with_current(current.clone(), ctx);
  let diff_rows = merge::diff(&current, &merged);
  let body = serialise(patcher.id(), &merged, format)?;
  let written = atomic_write_body(patcher, path, body.as_bytes())?;
  Ok((diff_rows, written))
}

/// Whole-file write for [`Format::Raw`] patchers (env.sh writer).
pub fn apply_raw(
  patcher: &dyn ToolPatcher,
  ctx: &PatchContext,
  path: &Path,
) -> Result<(Vec<DiffEntry>, u64), PatchError> {
  let body = patcher.raw_body(ctx).ok_or_else(|| {
    PatchError::Serialise(format!(
      "{}: Format::Raw patcher must implement raw_body()",
      patcher.id()
    ))
  })?;
  let diff_rows = compute_raw_diff(patcher, ctx, path)?;
  let written = atomic_write_body(patcher, path, body.as_bytes())?;
  Ok((diff_rows, written))
}

fn atomic_write_body(
  patcher: &dyn ToolPatcher,
  path: &Path,
  body: &[u8],
) -> Result<u64, PatchError> {
  let dir = path
    .parent()
    .unwrap_or_else(|| Path::new("."))
    .to_path_buf();
  let prefix = format!("{}.tmp.", patcher.id());
  write_secure(&dir, &prefix, path, body, Some(patcher.unix_mode())).map_err(|e| {
    PatchError::Write {
      tool_id: patcher.id(),
      path: path.to_path_buf(),
      error: e.to_string(),
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::init::external::ToolPatcher;
  use std::path::PathBuf;

  struct YamlStub;

  impl ToolPatcher for YamlStub {
    fn id(&self) -> &'static str {
      "yaml-stub"
    }
    fn display_name(&self) -> &'static str {
      "YAML Stub"
    }
    fn default_path(&self) -> Option<PathBuf> {
      None
    }
    fn format(&self) -> Format {
      Format::Yaml
    }
    fn build_additions(&self, ctx: &PatchContext) -> Value {
      serde_json::json!({ "openai-api-base": ctx.proxy_base_url })
    }
  }

  fn ctx() -> PatchContext {
    PatchContext::fixture(&[])
  }

  #[test]
  fn yaml_round_trip_preserves_user_keys() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-write-yaml");
    let path = dir.join("conf.yaml");
    std::fs::write(&path, "model: gpt-4o\nfoo: bar\n").unwrap();
    let (diff_rows, written) = apply_merge(&YamlStub, &ctx(), &path, Format::Yaml).expect("apply");
    assert!(written > 0);
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("openai-api-base: http://127.0.0.1:11435/v1"));
    assert!(body.contains("model: gpt-4o"));
    assert!(body.contains("foo: bar"));
    assert!(diff_rows.iter().any(|d| d.path == "openai-api-base"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn json_pretty_print_ends_with_newline() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-write-json");
    let path = dir.join("conf.json");
    let s = serialise("t", &serde_json::json!({"a":1}), Format::Json).unwrap();
    assert!(s.ends_with('\n'));
    let _ = path;
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn strip_json_comments_handles_line_block_and_strings() {
    let src = r#"{
      // line comment
      "a": 1, /* inline block */
      "b": "// not a comment",
      "c": "/* still not */",
      /* multi
         line */
      "d": 2
    }"#;
    let cleaned = strip_json_comments(src);
    let v: Value = serde_json::from_str(&cleaned).expect("parses after strip");
    assert_eq!(v["a"], 1);
    assert_eq!(v["b"], "// not a comment");
    assert_eq!(v["c"], "/* still not */");
    assert_eq!(v["d"], 2);
  }

  #[test]
  fn strip_json_comments_is_idempotent_on_clean_json() {
    let src = r#"{"a":1,"b":"foo","c":[1,2,3]}"#;
    assert_eq!(strip_json_comments(src), src);
  }

  #[test]
  fn strip_trailing_commas_handles_object_and_array() {
    let src = r#"{"a":1,"b":[1,2,3,],"c":{"x":1,},}"#;
    let cleaned = strip_trailing_commas(src);
    let v: Value = serde_json::from_str(&cleaned).expect("parses after strip");
    assert_eq!(v["a"], 1);
    assert_eq!(v["b"], serde_json::json!([1, 2, 3]));
    assert_eq!(v["c"]["x"], 1);
  }

  #[test]
  fn strip_trailing_commas_preserves_commas_in_strings() {
    let src = r#"{"x":"a,b,","y":1,}"#;
    let cleaned = strip_trailing_commas(src);
    let v: Value = serde_json::from_str(&cleaned).unwrap();
    assert_eq!(v["x"], "a,b,");
    assert_eq!(v["y"], 1);
  }

  #[test]
  fn strip_trailing_commas_idempotent_on_clean_json() {
    let src = r#"{"a":1,"b":2}"#;
    assert_eq!(strip_trailing_commas(src), src);
  }

  #[test]
  fn jsonc_with_comments_and_trailing_commas_round_trips() {
    // The exact failure mode the user hit: parser said
    // "trailing comma at line 10 column 7".
    let src = r#"{
  "theme": "opencode",
  "model": "anthropic/claude",
  // multi-line provider block
  "provider": {
    "anthropic": {
      "name": "Anthropic",
    },
  },
}"#;
    let cleaned = strip_trailing_commas(&strip_json_comments(src));
    let v: Value = serde_json::from_str(&cleaned).expect("parses after both passes");
    assert_eq!(v["theme"], "opencode");
    assert_eq!(v["provider"]["anthropic"]["name"], "Anthropic");
  }

  #[test]
  fn strip_json_comments_handles_escaped_quote_in_string() {
    let src = r#"{"x":"he said \"// hi\"","y":1}"#;
    let cleaned = strip_json_comments(src);
    let v: Value = serde_json::from_str(&cleaned).unwrap();
    assert_eq!(v["x"], r#"he said "// hi""#);
    assert_eq!(v["y"], 1);
  }

  #[test]
  fn read_current_treats_missing_file_as_empty_object() {
    let dir = crate::util::test_temp::unique_temp_dir("ext-write-missing");
    let path = dir.join("absent.json");
    let v = read_current("t", &path, Format::Json).unwrap();
    assert_eq!(v, Value::Object(Default::default()));
    std::fs::remove_dir_all(&dir).ok();
  }
}
