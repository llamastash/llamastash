//! Aider patcher — `~/.aider.conf.yml`.
//!
//! Flat top-level YAML keys: `openai-api-base`, `openai-api-key`,
//! optionally `model` (Aider prefixes OpenAI-compat custom models
//! with `openai/` in `--model`). Aider's docs list `~/.aider.conf.yml`
//! as the home-dir search location; the wizard writes there.
//!
//! Per Aider's docs, only OpenAI/Anthropic keys are allowed in the
//! YAML — that's fine for our purposes (we use the openai-api-key
//! key with the stub `llamastash` value).

use std::path::PathBuf;

use serde_json::json;

use crate::init::external::{Format, PatchContext, ToolPatcher};

pub struct Aider;

impl ToolPatcher for Aider {
  fn id(&self) -> &'static str {
    "aider"
  }
  fn display_name(&self) -> &'static str {
    "Aider"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".aider.conf.yml"))
  }
  fn alt_paths(&self) -> Vec<PathBuf> {
    // `.yaml` is sometimes used instead of `.yml` — Aider reads both
    // when searching the home dir per its docs. Detect-and-patch
    // beats creating a parallel `.yml`.
    crate::util::paths::home_dir()
      .map(|h| vec![h.join(".aider.conf.yaml")])
      .unwrap_or_default()
  }
  fn format(&self) -> Format {
    Format::Yaml
  }
  fn build_additions(&self, ctx: &PatchContext) -> serde_json::Value {
    let mut obj = json!({
      "openai-api-base": ctx.proxy_base_url,
      "openai-api-key": ctx.api_key,
    });
    // Aider's `model:` is a single slot — the primary chat model.
    if let Some(m) = ctx.primary() {
      obj
        .as_object_mut()
        .unwrap()
        .insert("model".into(), json!(format!("openai/{}", m.id)));
    }
    obj
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
  fn the_single_model_slot_takes_the_first_chat_model() {
    // Aider has one `model:` key, and an embedder in it would be useless.
    let ctx = PatchContext::fixture(&["nomic-embed-text-v1.5", "qwen3-coder-30b"]);
    let v = Aider.build_additions(&ctx);
    assert_eq!(v["model"], "openai/qwen3-coder-30b");
  }

  #[test]
  fn writes_openai_api_base_into_empty_yaml() {
    let dir = crate::util::test_temp::unique_temp_dir("aider-empty");
    let path = dir.join(".aider.conf.yml");
    apply(&Aider, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("openai-api-base: http://127.0.0.1:11435/v1"));
    assert!(body.contains("openai-api-key: llamastash"));
    assert!(body.contains("model: openai/qwen3-coder-30b"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn preserves_user_yaml_keys() {
    let dir = crate::util::test_temp::unique_temp_dir("aider-coexist");
    let path = dir.join(".aider.conf.yml");
    std::fs::write(&path, "auto-commits: false\nstream: true\n").unwrap();
    apply(&Aider, &ctx(), Some(path.clone())).expect("apply");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("auto-commits: false"));
    assert!(body.contains("stream: true"));
    assert!(body.contains("openai-api-base"));
    std::fs::remove_dir_all(&dir).ok();
  }
}
