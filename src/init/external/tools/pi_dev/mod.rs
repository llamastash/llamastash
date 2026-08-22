//! pi.dev patcher — `~/.pi/agent/models.json` plus the `enabledModels`
//! scope in [`settings`], so a registered model is also *reachable* in
//! pi's model switcher without the user widening the scope by hand.
//!
//! Schema: `providers.<id>` with `baseUrl`, `api: "openai-completions"`,
//! `apiKey`, and a `models[]` array.
//!
//! The `apiKey` field takes a literal, a `$ENV_VAR` reference, or a
//! `!command` that pi runs and reads stdout from. We use the command form
//! (`!llamastash api-key`): the literal token never lands on disk, and
//! unlike the env reference it works in a terminal that has not sourced
//! anything — `$LLAMASTASH_API_KEY` is unset in a fresh shell, which left
//! the provider configured but unusable with "No API key found".
//!
//! The `models[]` array is inside our own `llamastash` provider
//! block, so a wholesale replace only touches our entries — the
//! default object-recursive merge is fine.
//!
//! **Chat models only.** `api` is provider-level and pi's api registry
//! has exactly one OpenAI-shaped entry, `openai-completions` — there is no
//! embeddings api (verified against pi 0.84.2, `BUILTIN_APIS` in
//! `packages/ai/dist/compat.js`). pi is a coding agent and never calls
//! `/v1/embeddings`, so an embedder registered here would only fail at
//! stream time. They are left out.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::{Format, PatchContext, ToolPatcher};

pub mod settings;

pub struct PiDev;

pub const PROVIDER: &str = "llamastash";

/// pi runs this and uses stdout as the credential. Resolved per pi
/// process, so a rotated key is picked up on the next start without
/// re-patching anything.
const API_KEY_COMMAND: &str = "!llamastash api-key";

impl ToolPatcher for PiDev {
  fn id(&self) -> &'static str {
    "pi"
  }
  fn display_name(&self) -> &'static str {
    "pi.dev"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".pi").join("agent").join("models.json"))
  }
  fn format(&self) -> Format {
    Format::Json
  }
  fn build_additions(&self, ctx: &PatchContext) -> Value {
    let models: Vec<Value> = ctx
      .models
      .iter()
      .filter(|m| !m.is_embed)
      .map(|m| {
        json!({
          "id": m.id,
          "name": m.id,
          "contextWindow": m.declared_context(),
          "maxTokens": 8192,
        })
      })
      .collect();
    json!({
      "providers": {
        PROVIDER: {
          "name": "LlamaStash",
          "baseUrl": ctx.proxy_base_url,
          "api": "openai-completions",
          "apiKey": API_KEY_COMMAND,
          "models": models,
        }
      }
    })
  }
  fn companions(&self) -> Vec<Box<dyn ToolPatcher>> {
    vec![Box::new(settings::PiSettings)]
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
  fn writes_provider_block_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-empty");
    let path = dir.join("models.json");
    apply(&PiDev, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["providers"]["llamastash"]["baseUrl"],
      "http://127.0.0.1:11435/v1"
    );
    assert_eq!(body["providers"]["llamastash"]["api"], "openai-completions");
    assert_eq!(
      body["providers"]["llamastash"]["models"][0]["id"],
      "qwen3-coder-30b"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn every_chat_model_lands_in_the_models_array() {
    let ctx = PatchContext::fixture(&["qwen3-coder-30b", "Qwen/Qwen3-0.6B"]);
    let v = PiDev.build_additions(&ctx);
    let ids: Vec<&str> = v["providers"]["llamastash"]["models"]
      .as_array()
      .expect("array")
      .iter()
      .filter_map(|m| m["id"].as_str())
      .collect();
    assert_eq!(ids, vec!["qwen3-coder-30b", "Qwen/Qwen3-0.6B"]);
  }

  #[test]
  fn preserves_user_providers_alongside_llamastash() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-coexist");
    let path = dir.join("models.json");
    std::fs::write(
      &path,
      r#"{"providers":{"openai":{"baseUrl":"https://api.openai.com/v1","api":"openai-completions"}}}"#,
    )
    .unwrap();
    apply(&PiDev, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["providers"]["openai"]["baseUrl"],
      "https://api.openai.com/v1"
    );
    assert!(body["providers"]["llamastash"].is_object());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn the_api_key_is_resolved_by_shelling_out_never_written_literally() {
    let v = PiDev.build_additions(&ctx());
    // An env reference would leave a fresh terminal with no key at all,
    // and a literal would put the secret in a file people commit.
    assert_eq!(
      v["providers"]["llamastash"]["apiKey"],
      "!llamastash api-key"
    );
    assert!(
      !serde_json::to_string(&v)
        .unwrap()
        .contains("llamastash-secret"),
      "the resolved key never appears in the file"
    );
  }

  #[test]
  fn embedders_are_left_out_entirely() {
    // pi 0.84.2 has no embeddings api — registering one would only fail at
    // stream time, and pi never calls `/v1/embeddings` anyway.
    let v = PiDev.build_additions(&PatchContext::fixture(&[
      "nomic-embed-text-v1.5",
      "qwen3-coder-30b",
    ]));
    let ids: Vec<&str> = v["providers"]["llamastash"]["models"]
      .as_array()
      .expect("array")
      .iter()
      .filter_map(|m| m["id"].as_str())
      .collect();
    assert_eq!(ids, vec!["qwen3-coder-30b"]);
    assert_eq!(v["providers"].as_object().expect("providers").len(), 1);
  }

  #[test]
  fn the_model_scope_is_patched_as_a_companion() {
    // The provider block alone leaves the models out of pi's switcher
    // scope, so the second file travels with the first.
    let ids: Vec<&str> = PiDev.companions().iter().map(|c| c.id()).collect();
    assert_eq!(ids, vec!["pi-settings"]);
  }

  #[test]
  fn chat_model_keeps_openai_completions() {
    let v = PiDev.build_additions(&ctx());
    assert_eq!(v["providers"]["llamastash"]["api"], "openai-completions");
  }
}
