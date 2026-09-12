//! SGLang — direct, process-per-model serving of safetensors HF repos.
//!
//! The same shape as the other Python safetensors engine: SGLang takes a model
//! *directory* (a resolved HF snapshot), exposes its own OpenAI-compatible
//! HTTP server, and rides the generic supervisor and the format-agnostic proxy
//! forward with no lifecycle plumbing of its own. What differs is memory: it
//! has no byte-level KV cap, so the unified-memory guard lives in [`guard`]
//! and writes a token cap instead.
//!
//! Flag surface verified against SGLang 0.5.18 (`sglang serve --help` and a
//! live server's `/get_server_info`).

pub mod guard;
pub mod knobs;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::{BackendModelId, ModelIdentity};
use super::{
  Accelerator, AcceleratorSupport, Backend, LaunchPlan, Lifecycle, ProcessLaunchSpec, Readiness,
  CREDENTIAL_ENV_STRIP,
};
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::launch::admission::{human_gib, MIN_KV_CACHE_BYTES, UNIFIED_HOST_RESERVE_BYTES};
use crate::launch::params::LaunchParams;

/// Stable backend id. The only place this string is authored.
pub const SGLANG_BACKEND_ID: &str = "sglang";

/// SGLang config. **Default-on, gated by binary detection**, same tri-state as
/// the other detected backends: `None` (unset) means "on when the binary
/// resolves", `Some(false)` forces off, `Some(true)` forces on. `--sglang` /
/// `LLAMASTASH_SGLANG=1` force on regardless.
///
/// No CORS switch: SGLang's HTTP server allows every origin unconditionally
/// and exposes no flag to narrow it (0.5.18), so there is nothing to project.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct SglangConfig {
  pub enabled: Option<bool>,
  /// The `sglang` launcher binary/binaries. Unset falls back to a `sglang`
  /// on `$PATH`. Where SGLang ships only as a container image, point this at
  /// a wrapper script — see `docs/sglang-setup.md`.
  pub servers: Vec<crate::backend::ServerConfig>,
}

impl SglangConfig {
  /// Whether the user *intends* SGLang enabled, given the force flag. Actual
  /// availability still requires the binary to resolve.
  pub fn intends_enabled(&self, force: bool) -> bool {
    force || self.enabled != Some(false)
  }

  /// The configured launcher path (first server), if any.
  pub fn primary_binary(&self) -> Option<&Path> {
    self.servers.first().map(|s| s.binary.as_path())
  }
}

/// Executable name searched on `PATH` when no server is configured.
const SGLANG_BIN: &str = "sglang";

/// Extras heads refused on top of the shared loopback/credential denylist.
///
/// The key heads would undo the loopback-only, same-UID posture. The
/// distributed and parallel heads reach for multi-node and multi-process
/// execution the supervisor cannot own or reap. The gRPC and sidecar heads
/// replace or bypass the HTTP server the proxy forwards to. `--file-storage-path`
/// opens an arbitrary filesystem write surface to any proxy client through the
/// files API. `--config` reads further flags out of a YAML file and splices
/// them in ahead of ours, so every head here would be settable through it.
pub const SGLANG_FORBIDDEN_EXTRA_HEADS: &[&str] = &[
  "--api-key",
  "--admin-api-key",
  "--enable-ssl-refresh",
  "--dist-init-addr",
  "--nccl-init-addr",
  "--nnodes",
  "--node-rank",
  "--dp-size",
  "--data-parallel-size",
  "--pp-size",
  // Prefill/decode disaggregation: a bootstrap server plus peers over the
  // network. The family is `--disaggregation-mode` / `-bootstrap-port` / ….
  "--disaggregation-",
  "--grpc-mode",
  "--smg-grpc-mode",
  "--grpc-port",
  "--sidecar",
  "--sidecar-args",
  // Would move every route under a prefix the proxy does not know.
  "--fastapi-root-path",
  "--file-storage-path",
  "--config",
  // Ours: readiness and the chat path match the name we pass, so a second
  // one (last wins in argparse) would leave the launch waiting out its probe
  // budget for a name the server never advertises.
  "--served-model-name",
];

/// Resolve the launcher, by existence only — see
/// [`super::resolve_launcher_by_existence`] for why it is never executed.
pub fn resolve_sglang_binary(configured: Option<&Path>) -> Option<PathBuf> {
  super::resolve_launcher_by_existence(configured, SGLANG_BIN)
}

/// The SGLang backend.
#[derive(Debug, Clone, Default)]
pub struct SglangBackend {}

impl SglangBackend {
  pub fn new() -> Self {
    Self {}
  }
}

impl Backend for SglangBackend {
  fn knobs(&self) -> &'static [crate::launch::knobs::KnobDef] {
    knobs::KNOBS
  }

  fn id(&self) -> &'static str {
    SGLANG_BACKEND_ID
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  fn enabled_in_config(
    &self,
    config: &super::BackendConfig,
    force: &std::collections::BTreeMap<String, bool>,
  ) -> bool {
    config
      .sglang
      .intends_enabled(force.get(SGLANG_BACKEND_ID).copied().unwrap_or(false))
      && resolve_sglang_binary(config.sglang.primary_binary()).is_some()
  }

  fn projects_hf_repos(&self) -> bool {
    true
  }

  fn synthetic_identity(&self, path: &Path) -> Option<ModelIdentity> {
    // Claims the launch path before the orchestrator tries to read a GGUF
    // header — a safetensors snapshot is a directory, and the header read
    // would fail with EISDIR.
    crate::discovery::hf_repos::is_safetensors_snapshot(path).then(|| self.identify(path, &[]))
  }

  fn project_hf_repos(
    &self,
    candidates: &[crate::discovery::hf_repos::HfRepoCandidate],
  ) -> Vec<crate::discovery::DiscoveredModel> {
    candidates
      .iter()
      .filter(|c| crate::discovery::hf_repos::serves_safetensors_only(c))
      .map(|c| crate::discovery::hf_repos::project_safetensors_row(c, SGLANG_BACKEND_ID))
      .collect()
  }

  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    SGLANG_FORBIDDEN_EXTRA_HEADS
  }

  fn accelerators(&self) -> AcceleratorSupport {
    // GPU-first: CUDA and ROCm are the shipped serving targets. The CPU path
    // exists but is a build variant we cannot detect from here, so it stays in
    // the list as the floor rather than being claimed as fast.
    AcceleratorSupport::from_list([Accelerator::Cuda, Accelerator::Rocm, Accelerator::Cpu])
  }

  fn identify(&self, path: &Path, _header_bytes: &[u8]) -> ModelIdentity {
    // A safetensors snapshot has no GGUF header to hash. The repo id is the
    // stable name — a re-pull moves the snapshot revision directory but keeps
    // the repo — and it is exactly the name the launcher advertises, so the
    // chat path, which sends the identity, names a model the server has.
    ModelIdentity::Backend(BackendModelId {
      backend: SGLANG_BACKEND_ID.to_string(),
      name: served_model_name(path),
    })
  }

  fn available(&self, ctx: &MethodContext) -> bool {
    let force = ctx
      .backend_force
      .get(SGLANG_BACKEND_ID)
      .copied()
      .unwrap_or(false);
    ctx.backend.sglang.intends_enabled(force)
      && resolve_sglang_binary(ctx.backend.sglang.primary_binary()).is_some()
  }

  fn installed(&self, ctx: &MethodContext) -> bool {
    resolve_sglang_binary(ctx.backend.sglang.primary_binary()).is_some()
  }

  fn status_enabled(&self, ctx: &MethodContext) -> Option<bool> {
    Some(self.available(ctx))
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    resolve_sglang_binary(ctx.backend.sglang.primary_binary()).map(|b| b.display().to_string())
  }

  fn configured_servers(&self, ctx: &MethodContext) -> Vec<super::ServerSpec> {
    if !self.available(ctx) {
      return Vec::new();
    }
    resolve_sglang_binary(ctx.backend.sglang.primary_binary())
      .map(|binary| {
        vec![super::ServerSpec {
          binary,
          name: ctx
            .backend
            .sglang
            .servers
            .first()
            .and_then(|s| s.name.clone()),
        }]
      })
      .unwrap_or_default()
  }

  fn config_servers(&self, config: &crate::config::Config) -> Vec<crate::backend::ServerConfig> {
    config.backend.sglang.servers.clone()
  }

  fn launch_priority(&self) -> i32 {
    // Below the other safetensors engine: on a host with both installed the
    // established one stays the auto-route default and SGLang is a `--backend`
    // choice. Never competes for a GGUF.
    4
  }

  fn process_markers(&self) -> &'static [&'static str] {
    &[SGLANG_BACKEND_ID]
  }

  async fn fetch_actuals(
    &self,
    port: u16,
    timeout: std::time::Duration,
  ) -> crate::daemon::actuals::Actuals {
    // `/get_server_info` echoes every resolved server argument. The resolved
    // window is `context_length` (what `--context-length` lands as, or the
    // model's own when unset); `/v1/models` carries no such field here.
    let Some(info) = fetch_server_info(port, timeout).await else {
      return Default::default();
    };
    crate::daemon::actuals::Actuals {
      resolved_ctx: info
        .get("context_length")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok()),
      ..Default::default()
    }
  }

  fn projected_cache_bytes(&self, params: &LaunchParams, free_bytes: u64) -> Option<u64> {
    // A token cap is exact once the per-token cost is known: it is the figure
    // the launcher is handed, times what one token costs this model.
    if let Some(tokens) = knob_u64(params, "max-total-tokens") {
      if let Some(per_token) = guard::kv_bytes_per_token(&params.model_path) {
        return Some(tokens.saturating_mul(per_token));
      }
    }
    // A user-set fraction is projected against what is *free*, not the whole
    // pool SGLang actually takes it of, and the gate adds the weights on top
    // even though `mem_fraction_static` already covers them (0.5.18
    // `ServerArgs`: "model weights and KV cache memory pool"). Both errors
    // over-refuse, the safe direction. The other safetensors engine carries
    // the same projection for its utilization fraction; correct both at once.
    if let Some(frac) = knob_f64(params, "mem-fraction-static") {
      let frac = frac.clamp(0.0, 1.0);
      return Some((free_bytes as f64 * frac) as u64);
    }
    None
  }

  async fn adoption_matches(
    &self,
    recorded_path: &Path,
    argv: &[String],
    port: u16,
    probe_timeout: std::time::Duration,
  ) -> bool {
    super::served_name_adoption_matches(
      &served_model_name(recorded_path),
      recorded_path,
      argv,
      port,
      probe_timeout,
    )
    .await
  }

  fn resolve_launch_binary(
    &self,
    ctx: &MethodContext,
    _default_binary: PathBuf,
    port: u16,
  ) -> Result<(PathBuf, u16), String> {
    // The default binary is the device-owning llama.cpp server; SGLang has to
    // spawn its own launcher on the reserved pool port.
    match resolve_sglang_binary(ctx.backend.sglang.primary_binary()) {
      Some(bin) => Ok((bin, port)),
      None => Err(
        "SGLang backend selected but no `sglang` launcher found; set \
         `backend.sglang.servers[0].binary` or put `sglang` on PATH \
         (see docs/sglang-setup.md)"
          .to_string(),
      ),
    }
  }

  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan {
    LaunchPlan::SpawnProcess(ProcessLaunchSpec {
      binary,
      argv: sglang_argv(params, port),
      env_remove: CREDENTIAL_ENV_STRIP.to_vec(),
      readiness: readiness(&served_model_name(&params.model_path)),
      probe,
    })
  }

  async fn resolve_knobs(
    &self,
    ctx: &MethodContext,
    params: &mut LaunchParams,
    weights_bytes: u64,
  ) -> super::KnobResolution {
    let mut out = super::KnobResolution::default();
    // Either explicit bound opts out. A user-set fraction is honoured (we
    // insert nothing) and `projected_cache_bytes` turns it into a demand the
    // admission gate can evaluate.
    if user_set(params, "max-total-tokens") || user_set(params, "mem-fraction-static") {
      return out;
    }
    // Fail **safe**, not open: an unknown host is treated as unified, because
    // that is the assumption whose failure mode is survivable. The cost of
    // capping a discrete GPU is a smaller pool the user can raise; the cost of
    // not capping a UMA host is the machine.
    let snapshot = match ctx.host_metrics.as_ref() {
      Some(metrics) => Some(metrics.read().await.clone()),
      None => None,
    };
    let sampled = snapshot
      .as_ref()
      .is_some_and(crate::launch::admission::is_sampled);
    if sampled && !snapshot.as_ref().is_some_and(|s| s.unified) {
      // A sampled, definitely-discrete host: the fraction applies to real
      // VRAM there, so SGLang's own default is right and we stay out of it.
      return out;
    }
    // The byte budget has to be spent in tokens, which needs what one token
    // costs this model. No geometry means no safe figure, and a guess in the
    // wrong direction is the freeze — so refuse, with the override named.
    let Some(per_token) = guard::kv_bytes_per_token(&params.model_path) else {
      out.refusal = Some(format!(
        "cannot size the KV pool: no attention geometry readable from {}; \
         set max-total-tokens (or mem-fraction-static) explicitly to launch",
        params.model_path.join("config.json").display()
      ));
      return out;
    };
    let cap = match snapshot.as_ref().filter(|_| sampled) {
      Some(s) => {
        let free = crate::launch::admission::effective_free_bytes(s);
        match guard::max_total_tokens_cap(free, weights_bytes, per_token) {
          Some(cap) => cap,
          None => {
            out.refusal = Some(format!(
              "not enough memory: {} of weights leaves under {} (or under {} \
               tokens at {} bytes per token) for the KV pool once the {} host \
               reserve is kept free (host has {} available). Lower --ctx, pick \
               a smaller model, or set max-total-tokens to override.",
              human_gib(weights_bytes),
              human_gib(MIN_KV_CACHE_BYTES),
              guard::MIN_POOL_TOKENS,
              per_token,
              human_gib(UNIFIED_HOST_RESERVE_BYTES),
              human_gib(free),
            ));
            return out;
          }
        }
      }
      None => {
        log::warn!(
          "sglang: no host memory reading yet — capping the KV pool at the default \
           budget rather than letting the launcher size it against the whole pool"
        );
        // The default budget stands in for the (unknown) free figure, but the
        // token floor still applies: a pool no request fits in is a refusal
        // here exactly as it is on the sampled path.
        let budget = crate::launch::admission::DEFAULT_KV_CACHE_BYTES;
        match guard::tokens_for_budget(budget, per_token) {
          Some(cap) => cap,
          None => {
            out.refusal = Some(format!(
              "cannot size the KV pool: no host memory reading yet, and the default \
               {} budget holds under {} tokens at {} bytes per token. Set \
               max-total-tokens to override.",
              human_gib(budget),
              guard::MIN_POOL_TOKENS,
              per_token,
            ));
            return out;
          }
        }
      }
    };
    if let Some(ctx_len) = params.ctx.filter(|c| *c > cap) {
      out.warnings.push(format!(
        "KV pool capped at {cap} tokens, below the requested {ctx_len}-token \
         context; requests longer than the pool will be rejected"
      ));
    }
    log::info!("sglang: capping the KV pool at {cap} tokens");
    // A rejected value would leave the pool uncapped on the host the guard
    // exists for, and the knob is declared `U32 { max: None }` precisely so
    // this cannot happen — say so loudly rather than launching wide open if a
    // later bound ever makes it fail.
    if !params
      .knobs
      .set_by_name_for(SGLANG_BACKEND_ID, "max-total-tokens", cap.to_string())
    {
      out.refusal = Some(format!(
        "internal: the KV pool cap of {cap} tokens was rejected by the \
         max-total-tokens knob; set it explicitly to launch"
      ));
      return out;
    }
    out.auto_set.insert("max-total-tokens".to_string());
    out
  }
}

/// This backend's knob accessors, bound to its own vocabulary.
fn knob_u64(params: &LaunchParams, id: &str) -> Option<u64> {
  crate::launch::params::knob_u64(params, SGLANG_BACKEND_ID, id)
}

fn knob_f64(params: &LaunchParams, id: &str) -> Option<f64> {
  crate::launch::params::knob_f64(params, SGLANG_BACKEND_ID, id)
}

fn user_set(params: &LaunchParams, id: &str) -> bool {
  crate::launch::params::knob_is_user_set(params, SGLANG_BACKEND_ID, id)
}

/// Ceiling on a `/get_server_info` body. Larger than the `/v1/models` cap
/// the orphan probe uses because the body is every server argument (479 keys
/// on 0.5.18, well under 64 KiB); a body past it is not this server's.
const SERVER_INFO_MAX_BODY: usize = 256 * 1024;

/// GET `/get_server_info` on the loopback port, parsed. The body is read in
/// chunks and **rejected** once it passes [`SERVER_INFO_MAX_BODY`] — never
/// buffered whole and then cut, which would bound nothing and leave a
/// truncated body that no longer parses.
async fn fetch_server_info(port: u16, timeout: std::time::Duration) -> Option<serde_json::Value> {
  let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
  let mut resp = client
    .get(format!("http://127.0.0.1:{port}/get_server_info"))
    .send()
    .await
    .ok()?;
  if resp.status().as_u16() != 200 {
    return None;
  }
  let mut body = Vec::new();
  while let Some(chunk) = resp.chunk().await.ok()? {
    if body.len() + chunk.len() > SERVER_INFO_MAX_BODY {
      return None;
    }
    body.extend_from_slice(&chunk);
  }
  serde_json::from_slice(&body).ok()
}

/// The name SGLang advertises this model under.
///
/// Always passed explicitly: left to itself SGLang advertises the raw model
/// argument, which for us is a snapshot directory — that would leak a cache
/// path into `/v1/models` and force clients to name it in requests. The repo
/// id is what the catalog shows, so the proxy, the catalog and SGLang agree.
/// One name only: unlike its sibling engine's flag, `--served-model-name`
/// takes a single string (0.5.18), so no alias list is registered.
pub fn served_model_name(model_path: &Path) -> String {
  crate::discovery::hf_repos::repo_id_for_snapshot(model_path)
    .or_else(|| {
      model_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    })
    .unwrap_or_else(|| model_path.display().to_string())
}

/// The SGLang readiness contract.
///
/// `/v1/models` returning 200 **with the served name in the body**, not a
/// bare status check: the unready window (weight load, CUDA graph capture,
/// warmup) is long and the reserved port sits idle across it, so a
/// status-only probe could be answered by whatever else grabbed the port
/// meanwhile. Matching the served name is what makes the 200 ours.
pub fn readiness(served_name: &str) -> Readiness {
  Readiness::HttpPollModelId {
    path: "/v1/models".to_string(),
    ready_status: 200,
    expect_model_ids: vec![served_name.to_string()],
  }
}

/// Build the `sglang serve` argv.
fn sglang_argv(params: &LaunchParams, port: u16) -> Vec<std::ffi::OsString> {
  let mut argv: Vec<std::ffi::OsString> = vec![
    "serve".into(),
    "--model-path".into(),
    params.model_path.clone().into(),
    "--served-model-name".into(),
    served_model_name(&params.model_path).into(),
    // Loopback only, like every other backend we spawn.
    "--host".into(),
    "127.0.0.1".into(),
    "--port".into(),
    port.to_string().into(),
  ];
  let mut knobs = params.knobs.clone();
  if let Some(ctx) = params.ctx {
    knobs.set_by_name_for(SGLANG_BACKEND_ID, "context-length", ctx.to_string());
  }
  argv.extend(crate::launch::knobs::emit_argv(
    SGLANG_BACKEND_ID,
    &knobs,
    SGLANG_FORBIDDEN_EXTRA_HEADS,
  ));
  // The `-- <extras>` tail carries the long tail of flags with no typed knob.
  argv.extend(crate::launch::params::strip_forbidden_extras(
    &params.extras,
    SGLANG_FORBIDDEN_EXTRA_HEADS,
    "sglang_argv",
  ));
  argv
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A backend declares only what it actually honors: declaring a knob is
  /// claiming it, and a claimed knob renders an editor row and accepts a CLI
  /// flag whose value would then go nowhere.
  #[test]
  fn declares_a_context_knob_and_no_process_offload_knobs() {
    let b = SglangBackend::new();
    let ids: Vec<&str> = crate::backend::Backend::knobs(&b)
      .iter()
      .map(|d| d.id)
      .collect();
    assert!(crate::launch::knobs::def_for_backend_concept(
      SGLANG_BACKEND_ID,
      crate::launch::knobs::Concept::ContextLength
    )
    .is_some());
    for foreign in ["n-gpu-layers", "flash-attn", "split-mode"] {
      assert!(
        !ids.contains(&foreign),
        "`{foreign}` belongs to a process-spawning engine this backend is not"
      );
    }
  }

  #[test]
  fn id_and_lifecycle() {
    let b = SglangBackend::new();
    assert_eq!(b.id(), "sglang");
    assert_eq!(b.lifecycle(), Lifecycle::ProcessPerModel);
  }

  #[test]
  fn resolve_binary_requires_an_existing_file() {
    let dir = crate::util::test_temp::unique_temp_dir("sglang-resolve");
    let missing = dir.join("sglang");
    assert_eq!(resolve_sglang_binary(Some(&missing)), None);

    std::fs::write(&missing, b"#!/bin/sh\n").unwrap();
    assert_eq!(resolve_sglang_binary(Some(&missing)), Some(missing.clone()));

    // A directory is not a launcher.
    assert_eq!(resolve_sglang_binary(Some(&dir)), None);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// Resolving must never execute the candidate: this binary would exit
  /// non-zero and print nothing if run, and resolution still has to succeed
  /// on its existence alone.
  #[test]
  fn resolve_binary_never_executes_the_candidate() {
    let dir = crate::util::test_temp::unique_temp_dir("sglang-noexec");
    let marker = dir.join("was-executed");
    let bin = dir.join("sglang");
    std::fs::write(
      &bin,
      format!("#!/bin/sh\ntouch {}\nexit 1\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    assert_eq!(resolve_sglang_binary(Some(&bin)), Some(bin.clone()));
    assert!(
      !marker.exists(),
      "availability probing must not spawn the binary"
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  fn params(model: &str) -> LaunchParams {
    LaunchParams::new(PathBuf::from(model), crate::launch::mode::LaunchMode::Chat)
  }

  fn argv_strings(p: &LaunchParams, port: u16) -> Vec<String> {
    sglang_argv(p, port)
      .into_iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  fn set(p: &mut LaunchParams, id: &str, value: &str) {
    assert!(
      p.knobs.set_by_name_for(SGLANG_BACKEND_ID, id, value),
      "{id} is not a knob this backend declares"
    );
  }

  #[test]
  fn minimal_argv_is_model_path_served_name_loopback_and_port() {
    let p = params("/c/models--o--n/snapshots/rev");
    assert_eq!(
      argv_strings(&p, 41100),
      vec![
        "serve",
        "--model-path",
        "/c/models--o--n/snapshots/rev",
        "--served-model-name",
        "o/n",
        "--host",
        "127.0.0.1",
        "--port",
        "41100",
      ]
    );
  }

  #[test]
  fn ctx_renders_context_length_and_no_knob_duplicates_it() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    p.ctx = Some(8192);
    let argv = argv_strings(&p, 1);
    assert_eq!(argv.iter().filter(|a| *a == "--context-length").count(), 1);
    let i = argv.iter().position(|a| a == "--context-length").unwrap();
    assert_eq!(argv[i + 1], "8192");
  }

  /// Every declared knob emits the flag it declares, with a value its own
  /// kind accepts. Drives the declarations rather than a parallel flag table,
  /// so a knob added to the backend is covered without touching this test.
  #[test]
  fn every_declared_knob_renders_its_flag() {
    use crate::launch::knobs::KnobKind;
    for def in crate::launch::knobs::for_backend(SGLANG_BACKEND_ID) {
      if matches!(def.emit, crate::launch::knobs::Emit::Custom) {
        continue;
      }
      let mut p = params("/c/models--o--n/snapshots/rev");
      let sample = match def.kind {
        KnobKind::Bool => "true",
        KnobKind::F32 { .. } => "0.5",
        KnobKind::Enum { choices } => choices[0],
        KnobKind::OpenEnum { choices, .. } => choices.first().copied().unwrap_or("qwen3_coder"),
        KnobKind::Ratio => "1",
        _ => "7",
      };
      set(&mut p, def.id, sample);
      let argv = argv_strings(&p, 1);
      let flag = def.emit_flag();
      assert!(argv.contains(&flag), "{} did not emit {flag}", def.id);
      if matches!(def.kind, KnobKind::Bool) {
        // A bare flag carries no value of its own.
        let i = argv.iter().position(|a| *a == flag).unwrap();
        assert!(
          argv.get(i + 1).is_none_or(|n| n.starts_with('-')),
          "{} is a bare flag: {argv:?}",
          def.id
        );
      }
    }
  }

  #[test]
  fn unset_knobs_emit_nothing() {
    let p = params("/c/models--o--n/snapshots/rev");
    let argv = argv_strings(&p, 1);
    for flag in crate::launch::knobs::for_backend(SGLANG_BACKEND_ID)
      .iter()
      .map(|d| d.emit_flag())
    {
      assert!(!argv.contains(&flag), "{flag} leaked when unset");
    }
  }

  #[test]
  fn a_false_bool_emits_nothing() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    set(&mut p, "enable-unified-memory", "false");
    assert!(!argv_strings(&p, 1).contains(&"--enable-unified-memory".to_string()));
  }

  /// The knob channel must not become a way to smuggle a LAN bind or a
  /// credential in through a value. Every free-text knob here is
  /// identifier-shaped, so the value is refused at set time — earlier than
  /// the argv guard, and the guarantee this pins.
  #[test]
  fn no_knob_accepts_a_value_shaped_like_a_flag() {
    for smuggle in ["--host 0.0.0.0", "--host=0.0.0.0", "--api-key=hunter2"] {
      for def in crate::launch::knobs::for_backend(SGLANG_BACKEND_ID) {
        let mut p = params("/c/models--o--n/snapshots/rev");
        let accepted = p.knobs.set_by_name_for(SGLANG_BACKEND_ID, def.id, smuggle);
        let argv = argv_strings(&p, 1).join(" ");
        assert!(
          !accepted && !argv.contains("0.0.0.0") && !argv.contains("hunter2"),
          "`{smuggle}` accepted by {} or survived into argv: {argv}",
          def.id
        );
      }
    }
  }

  /// The launcher owns the served name; a second one in extras would leave
  /// readiness waiting for a name the server never advertises.
  #[test]
  fn a_served_name_in_extras_is_stripped() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    p.extras = vec!["--served-model-name".into(), "other".into()];
    let argv = argv_strings(&p, 1);
    assert_eq!(
      argv.iter().filter(|a| *a == "--served-model-name").count(),
      1
    );
    assert!(!argv.contains(&"other".to_string()), "{argv:?}");
  }

  #[test]
  fn extras_reach_argv_after_the_knobs() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    p.extras = vec!["--max-prefill-tokens".into(), "8192".into()];
    let argv = argv_strings(&p, 1);
    assert!(
      argv
        .windows(2)
        .any(|w| w[0] == "--max-prefill-tokens" && w[1] == "8192"),
      "the documented extras tail must reach argv: {argv:?}"
    );
  }

  #[test]
  fn a_forbidden_extra_is_stripped_from_argv_in_both_spellings() {
    for smuggle in [
      vec!["--host".to_string(), "0.0.0.0".to_string()],
      vec!["--host=0.0.0.0".to_string()],
      vec!["--api-key=hunter2".to_string()],
      vec!["--admin-api-key".to_string(), "hunter2".to_string()],
    ] {
      let mut p = params("/c/models--o--n/snapshots/rev");
      p.extras = smuggle.iter().map(Into::into).collect();
      let argv = argv_strings(&p, 1).join(" ");
      assert!(
        !argv.contains("0.0.0.0") && !argv.contains("hunter2"),
        "`{smuggle:?}` survived into argv: {argv}"
      );
      // The loopback host we set ourselves must still be there.
      assert!(argv.contains("--host 127.0.0.1"));
    }
  }

  /// Multi-node, multi-process and disaggregated execution are outside what
  /// the supervisor can own; the gRPC and sidecar heads bypass the HTTP server
  /// the proxy forwards to.
  #[test]
  fn the_distributed_and_transport_heads_are_refused() {
    for smuggle in [
      "--dist-init-addr",
      "--nccl-init-addr",
      "--nnodes",
      "--node-rank",
      "--dp-size",
      "--data-parallel-size",
      "--pp-size",
      "--disaggregation-mode",
      "--disaggregation-bootstrap-port",
      "--grpc-mode",
      "--grpc-port",
      "--sidecar",
      "--fastapi-root-path",
      "--file-storage-path",
    ] {
      let mut p = params("/c/models--o--n/snapshots/rev");
      p.extras = vec![smuggle.into(), "2".into()];
      let argv = argv_strings(&p, 1).join(" ");
      assert!(!argv.contains(smuggle), "`{smuggle}` survived: {argv}");
    }
  }

  /// `--config` is indirection rather than a spelling: the YAML's flags are
  /// spliced in ahead of ours, which would reinstate every head refused here.
  #[test]
  fn a_config_file_cannot_smuggle_the_denylist_back_in() {
    for smuggle in [
      vec!["--config".to_string(), "/tmp/sglang.yaml".to_string()],
      vec!["--config=/tmp/sglang.yaml".to_string()],
    ] {
      let mut p = params("/c/models--o--n/snapshots/rev");
      p.extras = smuggle.iter().map(Into::into).collect();
      let argv = argv_strings(&p, 1).join(" ");
      assert!(
        !argv.contains("--config") && !argv.contains("sglang.yaml"),
        "`{smuggle:?}` survived: {argv}"
      );
    }
  }

  #[test]
  fn served_name_falls_back_to_the_directory_when_not_in_a_cache_repo() {
    assert_eq!(served_model_name(Path::new("/models/my-model")), "my-model");
  }

  #[test]
  fn readiness_requires_the_served_name_not_just_a_200() {
    match readiness("o/n") {
      Readiness::HttpPollModelId {
        path,
        ready_status,
        expect_model_ids,
      } => {
        assert_eq!(path, "/v1/models");
        assert_eq!(ready_status, 200);
        assert_eq!(expect_model_ids, vec!["o/n".to_string()]);
      }
      other => panic!("expected a model-id poll, got {other:?}"),
    }
  }

  /// The token cap the guard writes must reach argv under the verified flag.
  #[test]
  fn token_cap_renders_the_verified_flag() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    set(&mut p, "max-total-tokens", "65536");
    let argv = argv_strings(&p, 1);
    let i = argv
      .iter()
      .position(|a| a == "--max-total-tokens")
      .expect("the cap must reach argv");
    assert_eq!(argv[i + 1], "65536");
  }

  /// A user-set fraction or token cap is a real configuration, not a reason
  /// to disengage the admission gate: both project a demand.
  #[test]
  fn a_user_bound_still_yields_a_demand_projection() {
    const GB: u64 = 1024 * 1024 * 1024;
    let b = SglangBackend::new();
    let dir = crate::util::test_temp::unique_temp_dir("sglang-projection");
    let mut p = params(dir.to_str().unwrap());

    assert_eq!(b.projected_cache_bytes(&p, 100 * GB), None, "nothing set");

    set(&mut p, "mem-fraction-static", "0.9");
    assert_eq!(
      b.projected_cache_bytes(&p, 100 * GB),
      Some(90 * GB),
      "a fraction of the pool is a projectable demand"
    );

    // A token cap is exact once the per-token cost is readable, and wins.
    set(&mut p, "max-total-tokens", "1000");
    assert_eq!(
      b.projected_cache_bytes(&p, 100 * GB),
      Some(90 * GB),
      "without geometry the cap cannot be priced; the fraction still projects"
    );
    std::fs::write(
      dir.join("config.json"),
      r#"{"num_hidden_layers":24,"num_attention_heads":14,"num_key_value_heads":2,"hidden_size":896,"torch_dtype":"bfloat16"}"#,
    )
    .unwrap();
    assert_eq!(
      b.projected_cache_bytes(&p, 100 * GB),
      Some(1000 * 2 * 2 * 64 * 2 * 24)
    );
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// One derivation, so the identity the chat path sends is the name the
  /// launcher registered.
  #[test]
  fn identity_name_matches_the_served_name_outside_the_cache_layout() {
    let b = SglangBackend::new();
    let path = Path::new("/models/my-model");
    let ModelIdentity::Backend(id) = b.identify(path, &[]) else {
      panic!("expected a backend identity");
    };
    assert_eq!(id.name, served_model_name(path));
    assert_eq!(id.name, "my-model");
  }

  #[test]
  fn intends_enabled_tri_state() {
    let mut c = SglangConfig::default();
    assert!(c.intends_enabled(false), "unset means on-when-found");
    c.enabled = Some(false);
    assert!(!c.intends_enabled(false));
    assert!(c.intends_enabled(true), "the force flag overrides off");
    c.enabled = Some(true);
    assert!(c.intends_enabled(false));
  }

  #[test]
  fn an_empty_config_block_is_the_default() {
    let parsed: SglangConfig = yaml_serde::from_str("{}").expect("empty sglang config");
    assert_eq!(parsed, SglangConfig::default());
  }

  /// One HTTP/1.1 exchange on a loopback port, answering 200 with `body`.
  async fn serve_once(body: Vec<u8>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
      .await
      .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
      use tokio::io::{AsyncReadExt, AsyncWriteExt};
      let (mut sock, _) = listener.accept().await.expect("accept");
      let mut req = [0u8; 4096];
      let _ = sock.read(&mut req).await;
      let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
      );
      sock.write_all(head.as_bytes()).await.expect("head");
      sock.write_all(&body).await.expect("body");
      sock.shutdown().await.expect("shutdown");
    });
    port
  }

  /// The body cap rejects, rather than cutting a body into JSON that no
  /// longer parses; a body under it still parses whole.
  #[tokio::test]
  async fn server_info_past_the_body_cap_is_rejected_not_truncated() {
    let timeout = std::time::Duration::from_secs(5);
    let small = br#"{"context_length":2048}"#.to_vec();
    let small_port = serve_once(small).await;
    let info = fetch_server_info(small_port, timeout)
      .await
      .expect("small body parses");
    assert_eq!(info["context_length"], 2048);

    let pad = "x".repeat(SERVER_INFO_MAX_BODY);
    let huge = format!(r#"{{"context_length":2048,"pad":"{pad}"}}"#).into_bytes();
    let huge_port = serve_once(huge).await;
    assert_eq!(fetch_server_info(huge_port, timeout).await, None);
  }
}
