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
//!
//! Per model, verified against opencode `dev` `e9f8a210b` (release
//! 1.18.33):
//! - `limit`: a config model without one gets `context: 0`
//!   (`provider/provider.ts`), and `context: 0` turns compaction off
//!   (`session/overflow.ts`), so a long session overflows the server. The
//!   config schema requires `output` beside `context`.
//! - `reasoning` + `variants`: `ProviderTransform.variants()` builds no
//!   effort variants for an id containing `qwen` (and several other
//!   families), so they are written explicitly, one per level the chat
//!   template accepts. `@ai-sdk/openai-compatible` sends `reasoningEffort`
//!   as `reasoning_effort`.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::init::external::{Format, PatchContext, PatchModel, SessionLaunch, ToolPatcher};

pub struct OpenCode;

/// The provider id our block registers under, and the prefix of a model
/// address (`llamastash/<id>`) in `model` / `small_model`.
pub const PROVIDER: &str = "llamastash";

fn model_entry(m: &PatchModel) -> serde_json::Value {
  // opencode compacts at `context - output`.
  let mut entry = json!({
    "name": m.id,
    "limit": { "context": m.declared_context(), "output": m.declared_output() },
  });
  // opencode replaces an image part with an error text unless the model
  // lists `image` input (`provider/transform.ts`).
  if m.vision {
    entry["modalities"] = json!({ "input": ["text", "image"], "output": ["text"] });
  }
  if let Some(effort) = &m.effort {
    // Lowest first, matching the order of the levels.
    let variants: serde_json::Map<String, serde_json::Value> = effort
      .can_disable
      .then_some("none")
      .into_iter()
      .chain(effort.levels.iter().map(String::as_str))
      .map(|l| (l.to_string(), json!({ "reasoningEffort": l })))
      .collect();
    entry["reasoning"] = json!(true);
    entry["variants"] = serde_json::Value::Object(variants);
  }
  entry
}

/// The program `llamastash session opencode` execs.
const PROGRAM: &str = "opencode";
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
  /// The `llamastash` provider's `models` map is LlamaStash's own, so it is
  /// replaced, not merged, like pi's array: a model no longer registered
  /// drops out, and each entry is written fresh, fields and order alike.
  fn merge_with_current(
    &self,
    mut current: serde_json::Value,
    ctx: &PatchContext,
  ) -> serde_json::Value {
    // Null in place keeps the key where it was; the merge then replaces it.
    if let Some(models) = current.pointer_mut(&format!("/provider/{PROVIDER}/models")) {
      *models = serde_json::Value::Null;
    }
    crate::init::external::merge::merge(current, self.build_additions(ctx))
  }
  fn build_additions(&self, ctx: &PatchContext) -> serde_json::Value {
    let models: serde_json::Map<String, serde_json::Value> = ctx
      .models
      .iter()
      .map(|m| (m.id.clone(), model_entry(m)))
      .collect();
    json!({
      "$schema": "https://opencode.ai/config.json",
      "provider": {
        (PROVIDER): {
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
  fn session_program(&self) -> Option<&'static str> {
    Some(PROGRAM)
  }
  /// OpenCode reads `OPENCODE_CONFIG_CONTENT` after its global and project
  /// config, so the whole run's configuration is one env var and nothing of
  /// the user's `opencode.json` is touched.
  fn session(&self, ctx: &PatchContext, _files_dir: &Path) -> Option<SessionLaunch> {
    let m = ctx.primary().filter(|m| !m.is_embed)?;
    let mut config = self.build_additions(ctx);
    let address = format!("{PROVIDER}/{}", m.id);
    config["model"] = json!(&address);
    config["small_model"] = json!(&address);
    Some(SessionLaunch {
      program: PROGRAM,
      args: Vec::new(),
      env: vec![
        ("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string()),
        // The provider block resolves its key as `{env:LLAMASTASH_API_KEY}`.
        (super::env_sh::API_KEY_VAR.to_string(), ctx.api_key.clone()),
      ],
      env_remove: Vec::new(),
      files: Vec::new(),
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
  fn every_model_declares_its_context_and_output_limit() {
    let mut ctx = PatchContext::fixture(&["big", "small"]);
    ctx.models[0].context_window = Some(262_144);
    ctx.models[1].context_window = Some(8192);
    let v = OpenCode.build_additions(&ctx);
    let models = &v["provider"]["llamastash"]["models"];
    assert_eq!(
      models["big"]["limit"],
      json!({"context": 262_144, "output": 32_000})
    );
    assert_eq!(
      models["small"]["limit"],
      json!({"context": 8192, "output": 4096})
    );
  }

  #[test]
  fn a_vision_model_lists_image_input() {
    let mut ctx = PatchContext::fixture(&["small-vl", "plain"]);
    ctx.models[0].vision = true;
    let v = OpenCode.build_additions(&ctx);
    let models = &v["provider"]["llamastash"]["models"];
    assert_eq!(
      models["small-vl"]["modalities"],
      json!({"input": ["text", "image"], "output": ["text"]})
    );
    assert!(models["plain"].get("modalities").is_none());
  }

  #[test]
  fn a_model_with_effort_levels_gets_reasoning_and_variants() {
    let mut ctx = PatchContext::fixture(&["Qwen3.8-27B-UD-Q6_K", "plain"]);
    ctx.models[0].effort = Some(crate::init::external::effort::EffortLevels {
      levels: vec!["low".into(), "medium".into(), "xhigh".into()],
      default: Some("xhigh".into()),
      can_disable: true,
      ..Default::default()
    });
    let v = OpenCode.build_additions(&ctx);
    let models = &v["provider"]["llamastash"]["models"];
    let qwen = &models["Qwen3.8-27B-UD-Q6_K"];
    assert_eq!(qwen["reasoning"], true);
    assert_eq!(
      qwen["variants"],
      json!({
        "low": {"reasoningEffort": "low"},
        "medium": {"reasoningEffort": "medium"},
        "xhigh": {"reasoningEffort": "xhigh"},
        "none": {"reasoningEffort": "none"},
      })
    );
    let order: Vec<&String> = qwen["variants"].as_object().unwrap().keys().collect();
    assert_eq!(order, ["none", "low", "medium", "xhigh"], "lowest first");
    assert!(models["plain"].get("reasoning").is_none());
    assert!(models["plain"].get("variants").is_none());
  }

  /// Our `models` map is replaced, so a model no longer registered drops out
  /// and old variant order is rewritten. The user's own keys stay, in order.
  #[test]
  fn rerun_replaces_our_models_and_keeps_the_rest() {
    let dir = crate::util::test_temp::unique_temp_dir("opencode-rerun");
    let path = dir.join("opencode.json");
    std::fs::write(
      &path,
      r#"{"theme": "x", "provider": {"mine": {"npm": "m"}, "llamastash": {"name": "old",
        "models": {"gone@preset": {"name": "gone@preset"},
          "Qwen3.8-27B-UD-Q6_K": {"variants": {"low": {}, "medium": {}, "none": {}, "xhigh": {}}}}}},
        "agent": {}}"#,
    )
    .unwrap();
    let mut ctx = PatchContext::fixture(&["Qwen3.8-27B-UD-Q6_K"]);
    ctx.models[0].effort = Some(crate::init::external::effort::EffortLevels {
      levels: vec!["low".into(), "medium".into(), "xhigh".into()],
      can_disable: true,
      ..Default::default()
    });
    apply(&OpenCode, &ctx, Some(path.clone())).expect("apply");
    let body: serde_json::Value =
      serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let keys = |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&body), ["theme", "provider", "agent", "$schema"]);
    assert_eq!(keys(&body["provider"]), ["mine", "llamastash"]);
    let models = &body["provider"]["llamastash"]["models"];
    assert_eq!(keys(models), ["Qwen3.8-27B-UD-Q6_K"], "stale entry dropped");
    assert_eq!(
      keys(&models["Qwen3.8-27B-UD-Q6_K"]["variants"]),
      ["none", "low", "medium", "xhigh"]
    );
    std::fs::remove_dir_all(&dir).ok();
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
    assert_eq!(
      body["provider"]["llamastash"]["options"]["baseURL"],
      "http://127.0.0.1:11435/v1"
    );
    assert!(!dir.join("opencode.json").exists());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn dry_run_reports_baseurl_change_for_existing_install() {
    let dir = crate::util::test_temp::unique_temp_dir("opencode-dry");
    let path = dir.join("opencode.json");
    std::fs::write(
      &path,
      r#"{"provider":{"llamastash":{"npm":"@ai-sdk/openai-compatible","name":"LlamaStash","options":{"baseURL":"http://127.0.0.1:99999/v1","apiKey":"{env:LLAMASTASH_API_KEY}"},"models":{"qwen3-coder-30b":{"name":"qwen3-coder-30b","limit":{"context":32768,"output":16384}}}}}}"#,
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

  #[test]
  fn session_content_is_the_patcher_block_plus_the_model_choice() {
    let mut ctx = ctx();
    ctx.models[0].context_window = Some(131_072);
    let launch = OpenCode.session(&ctx, Path::new(".")).expect("launch");
    assert_eq!(launch.program, "opencode");
    assert!(launch.args.is_empty());
    assert!(launch.files.is_empty());
    let content = launch
      .env
      .iter()
      .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
      .expect("config content")
      .1
      .clone();
    let cfg: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
    // Byte-for-byte the provider the patcher would write to disk for this model.
    assert_eq!(
      cfg["provider"]["llamastash"],
      OpenCode.build_additions(&ctx)["provider"]["llamastash"]
    );
    assert_eq!(
      cfg["provider"]["llamastash"]["models"]["qwen3-coder-30b"]["limit"],
      json!({ "context": 131_072, "output": 32_000 })
    );
    assert_eq!(cfg["model"], "llamastash/qwen3-coder-30b");
    assert_eq!(cfg["small_model"], "llamastash/qwen3-coder-30b");
    // The block resolves its key from the env, so the launch has to set it.
    assert_eq!(
      cfg["provider"]["llamastash"]["options"]["apiKey"],
      "{env:LLAMASTASH_API_KEY}"
    );
    assert!(launch
      .env
      .iter()
      .any(|(k, v)| k == "LLAMASTASH_API_KEY" && v == "llamastash"));
  }
}
