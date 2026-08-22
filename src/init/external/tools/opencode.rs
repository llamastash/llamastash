//! OpenCode patcher — `~/.config/opencode/opencode.json`.
//!
//! Registers a `llamastash` provider via the
//! `@ai-sdk/openai-compatible` SDK package and points it at the
//! local llamastash proxy. Per OpenCode's docs, custom providers
//! live under `provider.<id>` with `npm`, `name`, `options.baseURL`,
//! and a `models` map.
//!
//! API key: goes **inside `options`** — that's the object opencode
//! hands to the `@ai-sdk/openai-compatible` SDK constructor, so a
//! top-level `apiKey` is silently ignored and the SDK throws "missing
//! or invalid API key" against an auth-enforced proxy. Rendered as the
//! `{env:LLAMASTASH_API_KEY}` reference so the literal token never
//! lands on disk; that var is set by the sibling `env.sh` integration
//! (which carries the real bearer key when the proxy enforces auth), so
//! the opencode integration needs `env.sh` sourced — or
//! `LLAMASTASH_API_KEY` otherwise exported — to authenticate.

use std::path::PathBuf;

use serde_json::json;

use crate::init::external::{Format, PatchContext, ToolPatcher};

pub struct OpenCode;

impl ToolPatcher for OpenCode {
  fn id(&self) -> &'static str {
    "opencode"
  }
  fn display_name(&self) -> &'static str {
    "OpenCode"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".config").join("opencode").join("opencode.json"))
  }
  fn alt_paths(&self) -> Vec<PathBuf> {
    // OpenCode also reads `opencode.jsonc` — users who want inline
    // `//` comments use it. We check `.jsonc` first so re-running
    // `init` patches the existing file rather than creating a
    // parallel `opencode.json`. Writes always emit strict JSON;
    // existing comments in `.jsonc` are stripped (the wizard
    // outro warns when the alt path is the chosen target).
    crate::util::paths::home_dir()
      .map(|h| vec![h.join(".config").join("opencode").join("opencode.jsonc")])
      .unwrap_or_default()
  }
  fn format(&self) -> Format {
    Format::Json
  }
  fn required_env_var(&self) -> Option<&'static str> {
    // OpenCode resolves config values through exactly two substitutions,
    // `{env:VAR}` and `{file:PATH}` (verified against 1.18.21). With no
    // command form, the environment is the only way in that does not put
    // the key on disk.
    Some(super::env_sh::API_KEY_VAR)
  }
  fn build_additions(&self, ctx: &PatchContext) -> serde_json::Value {
    let models: serde_json::Map<String, serde_json::Value> = ctx
      .models
      .iter()
      .map(|m| (m.id.clone(), json!({ "name": m.id })))
      .collect();
    json!({
      "$schema": "https://opencode.ai/config.json",
      "provider": {
        "llamastash": {
          "npm": "@ai-sdk/openai-compatible",
          "name": "LlamaStash",
          "options": {
            "baseURL": ctx.proxy_base_url,
            "apiKey": "{env:LLAMASTASH_API_KEY}",
          },
          "models": models,
        }
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::init::external::{apply, dry_run};

  fn ctx() -> PatchContext {
    PatchContext::fixture(&["qwen3-coder-30b"])
  }

  #[test]
  fn every_model_is_registered_under_the_one_provider() {
    let ctx = PatchContext::fixture(&["qwen3-coder-30b", "Qwen/Qwen3-0.6B"]);
    let v = OpenCode.build_additions(&ctx);
    let models = &v["provider"]["llamastash"]["models"];
    // A safetensors repo id carries a slash — it is still just a key.
    assert_eq!(models["qwen3-coder-30b"]["name"], "qwen3-coder-30b");
    assert_eq!(models["Qwen/Qwen3-0.6B"]["name"], "Qwen/Qwen3-0.6B");
  }

  #[test]
  fn writes_provider_block_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("opencode-empty");
    let path = dir.join("opencode.json");
    let out = apply(&OpenCode, &ctx(), Some(path.clone())).expect("apply");
    assert!(out.written_bytes > 0);
    let body: serde_json::Value =
      serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["provider"]["llamastash"]["npm"],
      "@ai-sdk/openai-compatible"
    );
    assert_eq!(
      body["provider"]["llamastash"]["options"]["baseURL"],
      "http://127.0.0.1:11435/v1"
    );
    assert_eq!(
      body["provider"]["llamastash"]["models"]["qwen3-coder-30b"]["name"],
      "qwen3-coder-30b"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn preserves_user_providers_alongside_llamastash() {
    let dir = crate::util::test_temp::unique_temp_dir("opencode-coexist");
    let path = dir.join("opencode.json");
    std::fs::write(&path, r#"{"provider":{"anthropic":{"name":"Anthropic"}}}"#).unwrap();
    apply(&OpenCode, &ctx(), Some(path.clone())).expect("apply");
    let body: serde_json::Value =
      serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(body["provider"]["anthropic"]["name"], "Anthropic");
    assert!(body["provider"]["llamastash"].is_object());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn idempotent_apply_produces_no_second_diff() {
    let dir = crate::util::test_temp::unique_temp_dir("opencode-idem");
    let path = dir.join("opencode.json");
    apply(&OpenCode, &ctx(), Some(path.clone())).expect("first");
    let second = apply(&OpenCode, &ctx(), Some(path.clone())).expect("second");
    assert!(second.diff_json.is_empty());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn api_key_renders_as_env_reference_inside_options() {
    // Regression: opencode passes `options` verbatim to the
    // `@ai-sdk/openai-compatible` constructor, so the key MUST live
    // there. A top-level `apiKey` is ignored and the SDK throws
    // "missing or invalid API key" against an auth-enforced proxy.
    let ctx = ctx();
    let v = OpenCode.build_additions(&ctx);
    assert_eq!(
      v["provider"]["llamastash"]["options"]["apiKey"],
      "{env:LLAMASTASH_API_KEY}"
    );
    assert!(
      v["provider"]["llamastash"]["apiKey"].is_null(),
      "apiKey must not sit at the provider top level"
    );
  }

  #[test]
  fn existing_jsonc_is_patched_in_place_not_parallel_json() {
    // Repro the user's report: ~/.config/opencode/opencode.jsonc
    // exists with `//` / `/* */` comments AND trailing commas (very
    // common in JSONC). We should patch the .jsonc and never create
    // the .json sibling. The user's prior real-world failure mode
    // was "trailing comma at line 10 column 7"; this covers it.
    let dir = crate::util::test_temp::unique_temp_dir("opencode-jsonc");
    let jsonc = dir.join("opencode.jsonc");
    std::fs::write(
      &jsonc,
      "{\n  // user's settings\n  \"theme\": \"opencode\",\n  /* default model */\n  \"model\": \"anthropic/claude\",\n  \"provider\": {\n    \"anthropic\": {\n      \"name\": \"Anthropic\",\n    },\n  },\n}\n",
    )
    .unwrap();
    let out = apply(&OpenCode, &ctx(), Some(jsonc.clone())).expect("apply");
    assert_eq!(out.path, jsonc);
    let body: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&jsonc).unwrap())
      .expect("output is strict JSON");
    // User's keys preserved across the comment-stripped, trailing-
    // comma-stripped read + strict-JSON write round trip.
    assert_eq!(body["theme"], "opencode");
    assert_eq!(body["model"], "anthropic/claude");
    assert_eq!(body["provider"]["anthropic"]["name"], "Anthropic");
    // Our provider landed.
    assert_eq!(
      body["provider"]["llamastash"]["options"]["baseURL"],
      "http://127.0.0.1:11435/v1"
    );
    // No parallel .json sibling created.
    assert!(!dir.join("opencode.json").exists());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn dry_run_reports_baseurl_change_for_existing_install() {
    let dir = crate::util::test_temp::unique_temp_dir("opencode-dry");
    let path = dir.join("opencode.json");
    std::fs::write(
      &path,
      r#"{"provider":{"llamastash":{"npm":"@ai-sdk/openai-compatible","name":"LlamaStash","options":{"baseURL":"http://127.0.0.1:99999/v1","apiKey":"{env:LLAMASTASH_API_KEY}"},"models":{"qwen3-coder-30b":{"name":"qwen3-coder-30b"}}}}}"#,
    )
    .unwrap();
    let out = dry_run(&OpenCode, &ctx(), Some(path)).expect("dry_run");
    let leaf = out
      .diff_json
      .iter()
      .find(|d| d.path == "provider.llamastash.options.baseURL")
      .expect("baseURL leaf");
    assert_eq!(leaf.kind, "changed");
    std::fs::remove_dir_all(&dir).ok();
  }
}
