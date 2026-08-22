//! pi.dev's model scope — `~/.pi/agent/settings.json`.
//!
//! `enabledModels` is a list of minimatch globs matched against
//! `provider/id` (or the bare id) that bounds pi's model switcher: with it
//! set, only matching models are in scope and anything else needs the user
//! to widen the scope by hand. A llamastash provider that isn't covered by
//! a pattern is invisible in the switcher even though it is configured.
//! Adding `llamastash/*` puts every model we registered in scope.
//!
//! **Only when the key is already there.** pi treats an absent or empty
//! `enabledModels` as "no scoping — every model is available", so writing
//! the key into a file that lacked it would *narrow* the user's switcher to
//! llamastash alone. Absent means the goal is already met; leave it be.
//!
//! Applied as a companion of [`super::PiDev`] rather than a picker entry:
//! it is the same integration, just a second file.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::{Format, PatchContext, ToolPatcher};

pub struct PiSettings;

/// Covers every model under our provider id, whatever they are called.
///
/// `**`, not `*`: pi matches with minimatch, where `*` stops at a path
/// separator, and a safetensors repo's id *is* `owner/repo`. Verified
/// against pi 0.84.2's bundled minimatch — `llamastash/*` misses
/// `llamastash/Qwen/Qwen2.5-0.5B-Instruct`, `llamastash/**` catches it,
/// and neither pulls in another provider.
fn scope_pattern() -> String {
  format!("{}/**", super::PROVIDER)
}

impl ToolPatcher for PiSettings {
  fn id(&self) -> &'static str {
    "pi-settings"
  }
  fn display_name(&self) -> &'static str {
    "pi.dev model scope"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".pi").join("agent").join("settings.json"))
  }
  fn format(&self) -> Format {
    Format::Json
  }
  fn build_additions(&self, _ctx: &PatchContext) -> Value {
    json!({ "enabledModels": [scope_pattern()] })
  }
  fn merge_with_current(&self, current: Value, _ctx: &PatchContext) -> Value {
    let Value::Object(mut obj) = current else {
      return Value::Object(serde_json::Map::new());
    };
    let existing = obj.get("enabledModels").and_then(Value::as_array).cloned();
    // Absent or empty means "no scope filter" — every model is already in
    // reach, and adding ours would take the others out of it.
    let Some(existing) = existing.filter(|a| !a.is_empty()) else {
      return Value::Object(obj);
    };
    let pattern = scope_pattern();
    if existing
      .iter()
      .any(|v| v.as_str().is_some_and(|p| p == pattern))
    {
      return Value::Object(obj);
    }
    let mut patterns = existing;
    patterns.push(json!(pattern));
    obj.insert("enabledModels".into(), Value::Array(patterns));
    Value::Object(obj)
  }
  fn unix_mode(&self) -> u32 {
    // Unlike the rest, this file never holds a credential and the user's
    // own settings live in it — keep whatever a normal config file gets.
    0o644
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::init::external::apply;

  fn ctx() -> PatchContext {
    PatchContext::fixture(&["qwen3-coder-30b"])
  }

  #[test]
  fn appends_our_scope_to_an_existing_filter() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-settings-append");
    let path = dir.join("settings.json");
    std::fs::write(
      &path,
      r#"{"theme":"catppuccin","enabledModels":["claude-*","glm-*"]}"#,
    )
    .unwrap();
    apply(&PiSettings, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["enabledModels"],
      json!(["claude-*", "glm-*", "llamastash/**"])
    );
    assert_eq!(body["theme"], "catppuccin", "user settings untouched");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn leaves_an_unscoped_config_unscoped() {
    // No `enabledModels` means every model is in reach. Writing ours would
    // hide every other provider behind a scope widen.
    let dir = crate::util::test_temp::unique_temp_dir("pi-settings-unscoped");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"theme":"catppuccin"}"#).unwrap();
    apply(&PiSettings, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(body.get("enabledModels").is_none());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn an_empty_filter_is_treated_as_unscoped() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-settings-empty");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"enabledModels":[]}"#).unwrap();
    apply(&PiSettings, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(body["enabledModels"], json!([]));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn re_applying_does_not_duplicate_the_pattern() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-settings-idem");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"enabledModels":["claude-*"]}"#).unwrap();
    apply(&PiSettings, &ctx(), Some(path.clone())).expect("first");
    let second = apply(&PiSettings, &ctx(), Some(path.clone())).expect("second");
    assert!(second.diff_json.is_empty(), "idempotent");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(body["enabledModels"], json!(["claude-*", "llamastash/**"]));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn a_missing_settings_file_stays_missing() {
    // Nothing to scope into: creating the file with only our pattern would
    // scope a fresh pi install down to llamastash.
    let dir = crate::util::test_temp::unique_temp_dir("pi-settings-absent");
    let path = dir.join("settings.json");
    apply(&PiSettings, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(body.get("enabledModels").is_none());
    std::fs::remove_dir_all(&dir).ok();
  }
}
