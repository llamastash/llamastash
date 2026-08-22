//! Continue.dev patcher — `~/.continue/config.yaml`.
//!
//! Continue's schema is `models: [ { name, provider, apiBase, model,
//! roles }, ... ]` — a TOP-LEVEL array of named entries. Wholesale
//! replacing it would clobber user-added OpenAI/Anthropic models, so
//! [`merge_with_current`](ToolPatcher::merge_with_current) is
//! overridden to merge by `name`: entries in our `llamastash-*`
//! namespace are ours to replace, everything else is left untouched.
//! Entries for models that are no longer registered are dropped from
//! that namespace, so a favorite removed upstream stops appearing here.
//!
//! `config.yaml` is the current format (per Continue's docs as of
//! 2025–2026 — `config.json` is deprecated; we never write the old
//! format).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::{merge, Format, PatchContext, ToolPatcher};

pub struct ContinueDev;

/// Prefix owning our entries in Continue's top-level `models[]`. Anything
/// under it is ours to rewrite or drop; anything else is the user's.
/// `llamastash` bare is the pre-0.3 single-entry name, swept for the same
/// reason.
const NAME_PREFIX: &str = "llamastash-";
const LEGACY_MODEL_NAME: &str = "llamastash";

fn entry_name(id: &str) -> String {
  format!("{NAME_PREFIX}{id}")
}

fn is_ours(entry: &Value) -> bool {
  entry
    .get("name")
    .and_then(|v| v.as_str())
    .is_some_and(|n| n == LEGACY_MODEL_NAME || n.starts_with(NAME_PREFIX))
}

impl ToolPatcher for ContinueDev {
  fn id(&self) -> &'static str {
    "continue"
  }
  fn display_name(&self) -> &'static str {
    "Continue.dev"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".continue").join("config.yaml"))
  }
  fn alt_paths(&self) -> Vec<PathBuf> {
    // Some users name their YAML `.yml`; check that variant before
    // creating a parallel `.yaml`. We deliberately do NOT detect the
    // deprecated `config.json` here — Continue is migrating off it,
    // and writing to it would silently keep users on the old format.
    crate::util::paths::home_dir()
      .map(|h| vec![h.join(".continue").join("config.yml")])
      .unwrap_or_default()
  }
  fn format(&self) -> Format {
    Format::Yaml
  }
  fn build_additions(&self, ctx: &PatchContext) -> Value {
    let models: Vec<Value> = ctx
      .models
      .iter()
      .map(|m| {
        // Continue's `roles` enum drives what the IDE attempts with the
        // model. Setting `chat`/`edit` on an embedder (nomic-embed-text
        // etc.) gives confusing errors because Continue tries to chat
        // with an encoder-only model. The `embed` role is the right
        // wire for embedding models.
        let roles = if m.is_embed {
          json!(["embed"])
        } else {
          json!(["chat", "edit"])
        };
        let mut entry = serde_json::Map::new();
        entry.insert("name".into(), json!(entry_name(&m.id)));
        entry.insert("provider".into(), json!("openai"));
        entry.insert("apiBase".into(), json!(ctx.proxy_base_url));
        entry.insert("model".into(), json!(m.id));
        entry.insert("apiKey".into(), json!(ctx.api_key));
        entry.insert("roles".into(), roles);
        Value::Object(entry)
      })
      .collect();
    json!({
      "name": "llamastash",
      "version": "1.0.0",
      "schema": "v1",
      "models": models,
    })
  }
  fn merge_with_current(&self, current: Value, ctx: &PatchContext) -> Value {
    let additions = self.build_additions(ctx);
    let Value::Object(mut additions_obj) = additions else {
      return merge::merge(current, additions);
    };
    // Pull our `models` array out of the additions first — we'll
    // splice it into the *existing* models[] by name rather than let
    // the recursive merge replace the whole array.
    let our_models = additions_obj
      .remove("models")
      .and_then(|v| match v {
        Value::Array(a) => Some(a),
        _ => None,
      })
      .unwrap_or_default();
    // Top-level metadata (`name`, `version`, `schema`) is "fill in
    // only if absent" — the user owns these. Default recursive merge
    // would override the user's `name: MyConfig` with our
    // `name: llamastash` placeholder, which is user-hostile.
    let current_obj = match current {
      Value::Object(m) => m,
      _ => serde_json::Map::new(),
    };
    for key in ["name", "version", "schema"] {
      if current_obj.contains_key(key) {
        additions_obj.remove(key);
      }
    }
    // Anything still in additions_obj is safe to merge — empty in
    // practice today (we've removed everything), but the recursion
    // is cheap and future-proofs against added fields.
    let mut merged = merge::merge(Value::Object(current_obj), Value::Object(additions_obj));
    if let Value::Object(ref mut m) = merged {
      let slot = m
        .entry("models")
        .or_insert_with(|| Value::Array(Vec::new()));
      if let Value::Array(arr) = slot {
        if !our_models.is_empty() {
          // Sweep our namespace so a model that is no longer registered
          // stops being offered, then splice the current set in. Skipped
          // when we resolved no models at all — that means we could not
          // read the catalog, not that the user has none, and wiping a
          // working config on a daemon hiccup is not a trade worth making.
          let incoming: std::collections::HashSet<&str> = our_models
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
            .collect();
          arr.retain(|e| {
            !is_ours(e)
              || e
                .get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| incoming.contains(n))
          });
        }
        splice_named(arr, our_models, "name");
      }
    }
    merged
  }
}

/// Splice `incoming` entries into `current` by `name_field`: replace
/// matching entries, append new ones. Generic enough that
/// pi.dev / Zed could reuse it if their `available_models` ever
/// needs the same behaviour (today their schema means our entries
/// own the array, so wholesale replace is fine for those).
fn splice_named(current: &mut Vec<Value>, incoming: Vec<Value>, name_field: &str) {
  for new_entry in incoming {
    let key = new_entry
      .get(name_field)
      .and_then(|v| v.as_str())
      .map(String::from);
    let Some(key) = key else {
      current.push(new_entry);
      continue;
    };
    let pos = current
      .iter()
      .position(|c| c.get(name_field).and_then(|v| v.as_str()) == Some(key.as_str()));
    match pos {
      Some(i) => current[i] = new_entry,
      None => current.push(new_entry),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::init::external::apply;

  fn ctx() -> PatchContext {
    PatchContext::fixture(&["qwen3-coder-30b"])
  }

  fn embed_ctx() -> PatchContext {
    PatchContext::fixture(&["nomic-embed-text-v1.5"])
  }

  #[test]
  fn writes_models_array_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-empty");
    let path = dir.join("config.yaml");
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: llamastash-qwen3-coder-30b"));
    assert!(body.contains("apiBase: http://127.0.0.1:11435/v1"));
    assert!(body.contains("model: qwen3-coder-30b"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn preserves_user_models_in_array() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-user-models");
    let path = dir.join("config.yaml");
    std::fs::write(
      &path,
      "name: My Config\nversion: 1.0.0\nschema: v1\nmodels:\n  - name: GPT-4\n    provider: openai\n    model: gpt-4o\n",
    )
    .unwrap();
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: GPT-4"), "user model preserved");
    assert!(
      body.contains("name: llamastash-qwen3-coder-30b"),
      "our model added"
    );
    assert!(body.contains("name: My Config"), "top-level name preserved");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn re_applying_replaces_only_llamastash_entry() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-reapply");
    let path = dir.join("config.yaml");
    // User has GPT-4 + an older llamastash entry pointing at port 11434.
    std::fs::write(
      &path,
      "name: cfg\nversion: 1.0.0\nschema: v1\nmodels:\n  - name: GPT-4\n    provider: openai\n    model: gpt-4o\n  - name: llamastash\n    provider: openai\n    apiBase: http://127.0.0.1:11434/v1\n    model: old\n",
    )
    .unwrap();
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: GPT-4"));
    assert!(body.contains("apiBase: http://127.0.0.1:11435/v1"));
    assert!(!body.contains("11434"), "old llamastash entry replaced");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn embed_model_writes_embed_role_not_chat_edit() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-embed");
    let path = dir.join("config.yaml");
    apply(&ContinueDev, &embed_ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("- embed"), "embed role written");
    assert!(
      !body.contains("- chat"),
      "chat role NOT written for embedder"
    );
    assert!(
      !body.contains("- edit"),
      "edit role NOT written for embedder"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn every_model_gets_its_own_entry() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-multi");
    let path = dir.join("config.yaml");
    let ctx = PatchContext::fixture(&["qwen3-coder-30b", "nomic-embed-text-v1.5"]);
    apply(&ContinueDev, &ctx, Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: llamastash-qwen3-coder-30b"));
    assert!(body.contains("name: llamastash-nomic-embed-text-v1.5"));
    // Each carries the role its kind needs.
    assert!(body.contains("- chat"));
    assert!(body.contains("- embed"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn a_model_that_is_no_longer_registered_is_swept_from_our_namespace() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-sweep");
    let path = dir.join("config.yaml");
    let two = PatchContext::fixture(&["qwen3-coder-30b", "gemma-2-9b-it"]);
    apply(&ContinueDev, &two, Some(path.clone())).expect("first");
    // Second run registers only one of them — the other must not linger,
    // or Continue keeps offering a model the proxy no longer serves.
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("second");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: llamastash-qwen3-coder-30b"));
    assert!(!body.contains("gemma-2-9b-it"), "stale entry swept");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn an_empty_model_list_leaves_existing_entries_alone() {
    // No models resolved means the catalog was unreadable, not that the
    // user has none — wiping their working entries would be the wrong read.
    let dir = crate::util::test_temp::unique_temp_dir("continue-nomodels");
    let path = dir.join("config.yaml");
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("first");
    apply(
      &ContinueDev,
      &PatchContext::fixture(&[]),
      Some(path.clone()),
    )
    .expect("second");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: llamastash-qwen3-coder-30b"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn idempotent_second_apply_no_diff() {
    let dir = crate::util::test_temp::unique_temp_dir("continue-idem");
    let path = dir.join("config.yaml");
    apply(&ContinueDev, &ctx(), Some(path.clone())).expect("first");
    let second = apply(&ContinueDev, &ctx(), Some(path.clone())).expect("second");
    assert!(second.diff_json.is_empty(), "idempotent");
    std::fs::remove_dir_all(&dir).ok();
  }
}
