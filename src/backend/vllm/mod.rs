//! vLLM — direct, process-per-model serving of safetensors HF repos.
//!
//! vLLM takes a model *directory* (a resolved HF snapshot) rather than a
//! single weight file, and exposes its own OpenAI-compatible HTTP server, so
//! it rides the generic supervisor and the format-agnostic proxy forward with
//! no lifecycle plumbing of its own.
//!
//! Plan: `docs/plans/2026-08-10-001-feat-vllm-backend-plan.md`.

pub mod discovery;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::{BackendModelId, ModelIdentity};
use super::{
  Accelerator, AcceleratorSupport, Backend, KnobCapability, LaunchPlan, Lifecycle,
  ProcessLaunchSpec, Readiness, CREDENTIAL_ENV_STRIP,
};
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::launch::flag_aliases::KnobField;
use crate::launch::native_knobs::{translate, NativeKnobDescriptor, NativeKnobKind};
use crate::launch::params::LaunchParams;

/// Stable backend id. The only place this string is authored.
pub const VLLM_BACKEND_ID: &str = "vllm";

/// vLLM config. **Default-on, gated by binary detection**, same tri-state as
/// the other detected backends: `None` (unset) means "on when the binary
/// resolves", `Some(false)` forces off, `Some(true)` forces on. `--vllm` /
/// `LLAMASTASH_VLLM=1` force on regardless.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct VllmConfig {
  #[serde(default)]
  pub enabled: Option<bool>,
  /// The `vllm` launcher binary/binaries. Unset falls back to a `vllm` on
  /// `$PATH`. Where vLLM ships only as a container image, point this at a
  /// wrapper script — see `docs/vllm-setup.md`.
  #[serde(default)]
  pub servers: Vec<crate::backend::ServerConfig>,
}

impl VllmConfig {
  /// Whether the user *intends* vLLM enabled, given the force flag. Actual
  /// availability still requires the binary to resolve.
  pub fn intends_enabled(&self, force: bool) -> bool {
    force || self.enabled != Some(false)
  }

  /// The configured launcher path (first server), if any.
  pub fn primary_binary(&self) -> Option<&Path> {
    self.servers.first().map(|s| s.binary.as_path())
  }
}

/// Executable name searched on `PATH` when no server is configured. Compiled
/// out under `test-fixtures` so tests never auto-discover a host `vllm`.
#[cfg(not(feature = "test-fixtures"))]
const VLLM_BIN: &str = "vllm";

/// Extras heads refused on top of the shared loopback/credential denylist.
///
/// `--host` / `--api-key` / `--allowed-origins` / `--ssl-*` would undo the
/// loopback-only, same-UID posture. `--allowed-local-media-path` opens an
/// arbitrary filesystem read surface to any proxy client. The parallel and
/// distributed heads reach for multi-process and Ray execution, which is
/// outside what the supervisor can own or reap.
pub const VLLM_FORBIDDEN_EXTRA_HEADS: &[&str] = &[
  "--api-key",
  "--allowed-origins",
  "--allowed-local-media-path",
  "--pipeline-parallel-size",
  "--data-parallel",
  "--distributed-executor-backend",
  "--ray",
];

/// vLLM's tunables, on the per-backend native-knob channel.
///
/// Every flag here was checked against a live `vllm serve --help=all`
/// (0.19.1, 238 flags) rather than the docs. The long tail stays on `extras`
/// — this set is the part worth a picker row. Note vLLM has **no**
/// `--swap-space`; CPU offload moved to its own config group.
pub const VLLM_NATIVE_KNOBS: &[NativeKnobDescriptor] = &[
  NativeKnobDescriptor {
    id: "kv_cache_memory_bytes",
    label: "KV cache size",
    description: "hard cap on KV cache bytes (e.g. 8G); overrides the GPU memory fraction",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "gpu_memory_utilization",
    label: "GPU memory frac",
    description: "fraction of GPU memory vLLM may claim, 0.0-1.0 (vLLM default 0.92)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "max_num_seqs",
    label: "Max sequences",
    description: "ceiling on concurrently batched sequences",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "tensor_parallel_size",
    label: "Tensor parallel",
    description: "GPUs to shard the model across on this host",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "dtype",
    label: "Weight dtype",
    description: "weight/activation dtype",
    kind: NativeKnobKind::Cycle {
      presets: &["auto", "half", "bfloat16", "float16", "float32"],
    },
  },
  NativeKnobDescriptor {
    id: "kv_cache_dtype",
    label: "KV cache dtype",
    description: "KV cache dtype; the fp8 stops trade accuracy for cache headroom",
    kind: NativeKnobKind::Cycle {
      presets: &["auto", "fp8", "fp8_e5m2", "fp8_e4m3"],
    },
  },
  NativeKnobDescriptor {
    id: "quantization",
    label: "Quantization",
    description: "quantization method; leave unset to read it from the repo config",
    kind: NativeKnobKind::Cycle {
      presets: &["awq", "gptq", "fp8", "bitsandbytes"],
    },
  },
  NativeKnobDescriptor {
    id: "enforce_eager",
    label: "Eager mode",
    description: "skip graph capture — faster startup, lower steady-state throughput",
    kind: NativeKnobKind::Bool,
  },
  NativeKnobDescriptor {
    id: "trust_remote_code",
    label: "Trust remote code",
    description: "execute custom model code shipped in the repo (only for repos you trust)",
    kind: NativeKnobKind::Bool,
  },
];

/// Native-knob id → `vllm serve` flag.
const VLLM_KNOB_FLAGS: &[(&str, &str)] = &[
  ("kv_cache_memory_bytes", "--kv-cache-memory-bytes"),
  ("gpu_memory_utilization", "--gpu-memory-utilization"),
  ("max_num_seqs", "--max-num-seqs"),
  ("tensor_parallel_size", "--tensor-parallel-size"),
  ("dtype", "--dtype"),
  ("kv_cache_dtype", "--kv-cache-dtype"),
  ("quantization", "--quantization"),
  ("enforce_eager", "--enforce-eager"),
  ("trust_remote_code", "--trust-remote-code"),
];

/// Resolve the launcher **by filesystem existence only — never by running it.**
///
/// vLLM builds its argument parser through a device probe: on a host with no
/// usable accelerator, even `vllm --version` dies with
/// `RuntimeError: Failed to infer device type`. An exec-based probe would
/// therefore report "not installed" on exactly the machines where a user is
/// configuring the binary by hand. Verified against
/// `vllm 0.19.1+rocm7.13.0rc2` on 2026-08-10.
pub fn resolve_vllm_binary(configured: Option<&Path>) -> Option<PathBuf> {
  if let Some(path) = configured {
    return path.is_file().then(|| path.to_path_buf());
  }
  #[cfg(not(feature = "test-fixtures"))]
  {
    return which::which(VLLM_BIN).ok();
  }
  #[cfg(feature = "test-fixtures")]
  None
}

/// The vLLM backend.
#[derive(Debug, Clone)]
pub struct VllmBackend {
  capabilities: KnobCapability,
}

impl VllmBackend {
  pub fn new() -> Self {
    Self {
      // vLLM honours exactly one shared-IR knob: context length, which it
      // spells `--max-model-len`. Everything else it tunes lives on the
      // per-backend native-knob channel, so the llama.cpp-shaped rows stay
      // filtered out of the picker for a vLLM model.
      capabilities: KnobCapability::of(&[KnobField::Ctx]),
    }
  }
}

impl Default for VllmBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl Backend for VllmBackend {
  fn id(&self) -> &'static str {
    VLLM_BACKEND_ID
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  fn capabilities(&self) -> &KnobCapability {
    &self.capabilities
  }

  fn native_knobs(&self) -> &'static [NativeKnobDescriptor] {
    VLLM_NATIVE_KNOBS
  }

  fn enabled_in_config(
    &self,
    config: &super::BackendConfig,
    force: &std::collections::BTreeMap<String, bool>,
  ) -> bool {
    config
      .vllm
      .intends_enabled(force.get(VLLM_BACKEND_ID).copied().unwrap_or(false))
      && resolve_vllm_binary(config.vllm.primary_binary()).is_some()
  }

  fn projects_hf_repos(&self) -> bool {
    true
  }

  fn synthetic_identity(&self, path: &Path) -> Option<ModelIdentity> {
    // Claims the launch path before the orchestrator tries to read a GGUF
    // header — a safetensors snapshot is a directory, and the header read
    // would fail with EISDIR. The same hook a registry backend uses for its
    // file-less `<scheme>://<name>` paths; here the path is real, it just
    // isn't a single weight file.
    discovery::is_safetensors_snapshot(path).then(|| self.identify(path, &[]))
  }

  fn project_hf_repos(
    &self,
    candidates: &[crate::discovery::hf_repos::HfRepoCandidate],
  ) -> Vec<crate::discovery::DiscoveredModel> {
    candidates
      .iter()
      .filter(|c| discovery::eligible(c))
      .map(|c| discovery::project(c, VLLM_BACKEND_ID))
      .collect()
  }

  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    VLLM_FORBIDDEN_EXTRA_HEADS
  }

  fn accelerators(&self) -> AcceleratorSupport {
    // vLLM is GPU-first: CUDA and ROCm are the shipped serving targets. The
    // CPU path exists but is a build variant we cannot detect from here, so
    // it stays in the list as the floor rather than being claimed as fast.
    AcceleratorSupport::from_list([Accelerator::Cuda, Accelerator::Rocm, Accelerator::Cpu])
  }

  fn identify(&self, path: &Path, _header_bytes: &[u8]) -> ModelIdentity {
    // A safetensors snapshot has no GGUF header to hash, so the `Gguf`
    // identity does not apply. The repo id is the stable name: a re-pull
    // moves the snapshot revision directory but keeps the repo.
    ModelIdentity::Backend(BackendModelId {
      backend: VLLM_BACKEND_ID.to_string(),
      name: discovery::repo_id_for_snapshot(path).unwrap_or_else(|| path.display().to_string()),
    })
  }

  fn available(&self, ctx: &MethodContext) -> bool {
    let force = ctx
      .backend_force
      .get(VLLM_BACKEND_ID)
      .copied()
      .unwrap_or(false);
    ctx.backend.vllm.intends_enabled(force)
      && resolve_vllm_binary(ctx.backend.vllm.primary_binary()).is_some()
  }

  fn installed(&self, ctx: &MethodContext) -> bool {
    resolve_vllm_binary(ctx.backend.vllm.primary_binary()).is_some()
  }

  fn status_enabled(&self, ctx: &MethodContext) -> Option<bool> {
    Some(self.available(ctx))
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    resolve_vllm_binary(ctx.backend.vllm.primary_binary()).map(|b| b.display().to_string())
  }

  fn configured_servers(&self, ctx: &MethodContext) -> Vec<super::ServerSpec> {
    if !self.available(ctx) {
      return Vec::new();
    }
    resolve_vllm_binary(ctx.backend.vllm.primary_binary())
      .map(|binary| {
        vec![super::ServerSpec {
          binary,
          name: ctx
            .backend
            .vllm
            .servers
            .first()
            .and_then(|s| s.name.clone()),
        }]
      })
      .unwrap_or_default()
  }

  fn config_servers(&self, config: &crate::config::Config) -> Vec<crate::backend::ServerConfig> {
    config.backend.vllm.servers.clone()
  }

  fn launch_priority(&self) -> i32 {
    // Below llama.cpp: vLLM never competes for a GGUF, and on the safetensors
    // rows it claims it is currently the only candidate.
    5
  }

  fn process_marker(&self) -> Option<&'static str> {
    Some(VLLM_BACKEND_ID)
  }

  async fn adoption_matches(
    &self,
    recorded_path: &Path,
    argv: &[String],
    port: u16,
    probe_timeout: std::time::Duration,
  ) -> bool {
    // The default rule looks for the recorded *path* in `/v1/models`, but this
    // backend advertises the served name instead — the whole point of passing
    // `--served-model-name`. Confirm on that, cross-checked against the path
    // still being the argv's model argument so a recycled PID serving a
    // different model of ours cannot pass.
    let expected = served_model_name(recorded_path);
    let path_str = recorded_path.to_string_lossy();
    let argv_agrees = argv.is_empty() || argv.iter().any(|a| *a == path_str);
    argv_agrees
      && crate::daemon::orphans::models_endpoint_serves_id(port, &expected, probe_timeout).await
  }

  fn resolve_launch_binary(
    &self,
    ctx: &MethodContext,
    _default_binary: PathBuf,
    port: u16,
  ) -> Result<(PathBuf, u16), String> {
    // The default binary is the device-owning llama.cpp server; vLLM has to
    // spawn its own launcher on the reserved pool port.
    match resolve_vllm_binary(ctx.backend.vllm.primary_binary()) {
      Some(bin) => Ok((bin, port)),
      None => Err(
        "vLLM backend selected but no `vllm` launcher found; set \
         `backend.vllm.servers[0].binary` or put `vllm` on PATH \
         (see docs/vllm-setup.md)"
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
    LaunchPlan::SpawnProcess(self.process_spec(params, port, binary, probe))
  }

  async fn resolve_native_knobs(
    &self,
    ctx: &MethodContext,
    params: &mut LaunchParams,
    weights_bytes: u64,
  ) -> super::NativeKnobResolution {
    let mut out = super::NativeKnobResolution::default();
    if user_set(params, "kv_cache_memory_bytes") || user_set(params, "gpu_memory_utilization") {
      return out;
    }
    let Some(metrics) = ctx.host_metrics.as_ref() else {
      return out;
    };
    let snapshot = metrics.read().await.clone();
    if !crate::launch::admission::is_sampled(&snapshot) || !snapshot.unified {
      return out;
    }
    let free = crate::launch::admission::effective_free_bytes(&snapshot);
    let Some(cap) = kv_cache_cap_bytes(free, weights_bytes) else {
      // Nothing safe left to give. Say nothing and let the admission gate
      // produce the refusal, rather than launching with a token cache.
      return out;
    };
    log::info!("vllm: capping KV cache at {cap} bytes on a unified-memory host");
    params.backend_knobs.insert(
      "kv_cache_memory_bytes".to_string(),
      crate::config::KnobValue::Set(cap.to_string()),
    );
    out.auto_set.insert("kv_cache_memory_bytes".to_string());
    out
  }
}

/// Whether the user pinned this knob (as opposed to leaving it to us).
fn user_set(params: &LaunchParams, id: &str) -> bool {
  matches!(
    params.backend_knobs.get(id),
    Some(crate::config::KnobValue::Set(_))
  )
}

/// Default KV cache budget when nothing else bounds it. Generous for a single
/// user (~85x concurrency at 2k context on a 0.5B) and small enough that the
/// launch cannot take the host down.
const DEFAULT_KV_CACHE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Reserve left free for the OS and everything else after weights + cache.
const UNIFIED_HOST_RESERVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// The KV cache cap for a unified-memory host, or `None` when the weights
/// alone leave no room.
///
/// On an APU, GPU memory *is* system RAM, and vLLM sizes its KV cache to fill
/// whatever `gpu_memory_utilization` allows — a fraction of the **pool**, not
/// of the model. Measured on a 121 GB Strix Halo: `0.15` on a 0.5B model
/// reserved 15.1 GiB of KV cache (1.3M tokens, 644x concurrency for a
/// 2048-token model) and cost 21.2 GB of RAM; the 0.92 default projects to
/// ~106 GB and has frozen the machine outright. Clamping the *fraction* does
/// not help, because the arithmetic is against the wrong number. Capping the
/// cache in bytes does: vLLM then skips memory profiling entirely and honours
/// the figure.
fn kv_cache_cap_bytes(free_bytes: u64, weights_bytes: u64) -> Option<u64> {
  let headroom = free_bytes
    .checked_sub(weights_bytes)?
    .checked_sub(UNIFIED_HOST_RESERVE_BYTES)?;
  (headroom > 0).then(|| headroom.min(DEFAULT_KV_CACHE_BYTES))
}

impl VllmBackend {
  pub fn process_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ProcessLaunchSpec {
    ProcessLaunchSpec {
      binary,
      argv: vllm_argv(params, port),
      env_remove: CREDENTIAL_ENV_STRIP.to_vec(),
      readiness: readiness(&served_model_name(&params.model_path)),
      probe,
    }
  }
}

/// The name vLLM advertises this model under.
///
/// Always passed explicitly: left to itself vLLM advertises the raw model
/// argument, which for us is a snapshot directory — that would leak a cache
/// path into `/v1/models` and force clients to name it in requests. The repo
/// id is what the catalog shows, so the proxy, the catalog and vLLM all agree.
pub fn served_model_name(model_path: &Path) -> String {
  discovery::repo_id_for_snapshot(model_path)
    .or_else(|| {
      model_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    })
    .unwrap_or_else(|| model_path.display().to_string())
}

/// Every name vLLM should answer to, primary first.
///
/// vLLM is the first backend behind our proxy that *validates* the request's
/// `model` field — llama.cpp ignores it, ds4 echoes it back. The proxy forwards
/// the client's bytes unchanged by design, so a name our own resolver accepted
/// (it matches case-insensitive substrings) would reach vLLM verbatim and 404
/// **after** paying a full cold start. `--served-model-name` takes a list, so
/// registering the aliases is cheaper than teaching the proxy to rewrite bodies.
///
/// Not every substring the resolver would accept, which is unbounded — the
/// forms clients actually send: the repo id, the bare model name, and the
/// lowercase of each. Verified against vLLM 0.27.1: both entries appear in
/// `/v1/models` and both route, while an unregistered name still 404s.
pub fn served_model_aliases(model_path: &Path) -> Vec<String> {
  let primary = served_model_name(model_path);
  let mut out = vec![primary.clone()];
  let bare = primary.rsplit('/').next().unwrap_or(&primary).to_string();
  for alias in [bare.clone(), primary.to_lowercase(), bare.to_lowercase()] {
    if !alias.is_empty() && !out.contains(&alias) {
      out.push(alias);
    }
  }
  out
}

/// The vLLM readiness contract.
///
/// `/v1/models` returning 200 **with the served name in the body**, not a
/// bare status check. Two reasons, both observed on a real 0.19.1 server:
/// the unready window is long (engine init — profiling plus KV-cache build —
/// measured at 10-27 s on a 0.5B and longer on real models), and the reserved
/// port sits idle across it, so a status-only probe could be answered by
/// whatever else grabbed the port meanwhile. Matching the served name is what
/// makes the 200 ours.
pub fn readiness(served_name: &str) -> Readiness {
  Readiness::HttpPollModelId {
    path: "/v1/models".to_string(),
    ready_status: 200,
    expect_model_ids: vec![served_name.to_string()],
  }
}

/// Build the `vllm serve` argv.
fn vllm_argv(params: &LaunchParams, port: u16) -> Vec<std::ffi::OsString> {
  let mut argv: Vec<std::ffi::OsString> = vec!["serve".into(), params.model_path.clone().into()];
  argv.push("--served-model-name".into());
  for alias in served_model_aliases(&params.model_path) {
    argv.push(alias.into());
  }
  // Loopback only, like every other backend we spawn.
  argv.push("--host".into());
  argv.push("127.0.0.1".into());
  argv.push("--port".into());
  argv.push(port.to_string().into());
  if let Some(ctx) = params.ctx {
    argv.push("--max-model-len".into());
    argv.push(ctx.to_string().into());
  }
  argv.extend(translate(
    VLLM_NATIVE_KNOBS,
    VLLM_KNOB_FLAGS,
    &params.backend_knobs,
    VLLM_FORBIDDEN_EXTRA_HEADS,
  ));
  // The `-- <extras>` tail carries the ~230 flags that have no typed knob.
  // `compose_and_spawn` already refused a banned head with a clear error;
  // this strip is the belt-and-suspenders that guarantees none reaches the
  // launcher even if some path skipped the fail-fast.
  let mut skip_value = false;
  for e in &params.extras {
    let lossy = e.to_string_lossy();
    // Drop the value token that belonged to a flag we just stripped. Without
    // this the space-separated form leaves `0.0.0.0` dangling in argv, which
    // vLLM reads as a stray positional and refuses the launch over.
    if skip_value {
      skip_value = false;
      if !lossy.starts_with('-') {
        continue;
      }
    }
    let head = lossy.split('=').next().unwrap_or(&lossy);
    if crate::launch::params::is_forbidden_head_ext(head, VLLM_FORBIDDEN_EXTRA_HEADS) {
      log::warn!("vllm_argv: stripping forbidden extra {head:?}");
      skip_value = !lossy.contains('=');
      continue;
    }
    argv.push(e.clone());
  }
  argv
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn capabilities_cover_exactly_ctx() {
    let b = VllmBackend::new();
    assert!(b.capabilities().supports(KnobField::Ctx));
    for f in [
      KnobField::NGpuLayers,
      KnobField::FlashAttn,
      KnobField::Threads,
      KnobField::SplitMode,
    ] {
      assert!(
        !b.capabilities().supports(f),
        "{f:?} is a llama.cpp knob and must not be claimed"
      );
    }
  }

  #[test]
  fn id_and_lifecycle() {
    let b = VllmBackend::new();
    assert_eq!(b.id(), "vllm");
    assert_eq!(b.lifecycle(), Lifecycle::ProcessPerModel);
  }

  #[test]
  fn resolve_binary_requires_an_existing_file() {
    let dir = crate::util::test_temp::unique_temp_dir("vllm-resolve");
    let missing = dir.join("vllm");
    assert_eq!(resolve_vllm_binary(Some(&missing)), None);

    std::fs::write(&missing, b"#!/bin/sh\n").unwrap();
    assert_eq!(resolve_vllm_binary(Some(&missing)), Some(missing.clone()));

    // A directory is not a launcher.
    assert_eq!(resolve_vllm_binary(Some(&dir)), None);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// Pins the D6 decision. vLLM's parser construction probes for a device, so
  /// `vllm --version` fails outright on a GPU-less host; resolving must never
  /// execute the candidate. This binary would exit non-zero and print nothing
  /// if run — resolution still has to succeed on its existence alone.
  #[test]
  fn resolve_binary_never_executes_the_candidate() {
    let dir = crate::util::test_temp::unique_temp_dir("vllm-noexec");
    let marker = dir.join("was-executed");
    let bin = dir.join("vllm");
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

    assert_eq!(resolve_vllm_binary(Some(&bin)), Some(bin.clone()));
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
    vllm_argv(p, port)
      .into_iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  fn set(p: &mut LaunchParams, id: &str, value: &str) {
    p.backend_knobs.insert(
      id.to_string(),
      crate::config::KnobValue::Set(value.to_string()),
    );
  }

  #[test]
  fn minimal_argv_is_path_served_names_loopback_and_port() {
    let p = params("/c/models--o--n/snapshots/rev");
    assert_eq!(
      argv_strings(&p, 41100),
      vec![
        "serve",
        "/c/models--o--n/snapshots/rev",
        // `--served-model-name` takes a list; `--host` terminates it.
        "--served-model-name",
        "o/n",
        "n",
        "--host",
        "127.0.0.1",
        "--port",
        "41100",
      ]
    );
  }

  /// A name our resolver accepts must be one vLLM accepts, or the client eats
  /// a full cold start and then a 404. The primary stays first so `/v1/models`
  /// and the catalog still agree on the canonical id.
  #[test]
  fn aliases_cover_the_bare_name_and_lowercase_forms() {
    let aliases = served_model_aliases(Path::new(
      "/c/models--Qwen--Qwen2.5-0.5B-Instruct/snapshots/rev",
    ));
    assert_eq!(
      aliases[0], "Qwen/Qwen2.5-0.5B-Instruct",
      "primary comes first"
    );
    for expected in [
      "Qwen2.5-0.5B-Instruct",
      "qwen/qwen2.5-0.5b-instruct",
      "qwen2.5-0.5b-instruct",
    ] {
      assert!(
        aliases.iter().any(|a| a == expected),
        "{expected} missing from {aliases:?}"
      );
    }
    let mut deduped = aliases.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), aliases.len(), "aliases must be unique");
  }

  #[test]
  fn ctx_renders_max_model_len_and_no_knob_duplicates_it() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    p.ctx = Some(8192);
    let argv = argv_strings(&p, 1);
    assert_eq!(argv.iter().filter(|a| *a == "--max-model-len").count(), 1);
    let i = argv.iter().position(|a| a == "--max-model-len").unwrap();
    assert_eq!(argv[i + 1], "8192");
  }

  #[test]
  fn every_native_knob_renders_its_verified_flag() {
    for (id, flag) in VLLM_KNOB_FLAGS {
      let mut p = params("/c/models--o--n/snapshots/rev");
      let is_bool = VLLM_NATIVE_KNOBS
        .iter()
        .find(|d| &d.id == id)
        .expect("every flag-mapped id has a descriptor")
        .is_bool();
      set(&mut p, id, if is_bool { "true" } else { "7" });
      let argv = argv_strings(&p, 1);
      assert!(argv.contains(&flag.to_string()), "{id} did not emit {flag}");
      if is_bool {
        assert!(
          !argv.contains(&"true".to_string()),
          "{id} is a bool and must emit a bare flag"
        );
      }
    }
  }

  #[test]
  fn every_descriptor_has_a_flag_mapping() {
    // A descriptor with no mapping renders a picker row that silently does
    // nothing — the failure mode `translate` cannot report.
    for d in VLLM_NATIVE_KNOBS {
      assert!(
        VLLM_KNOB_FLAGS.iter().any(|(id, _)| *id == d.id),
        "native knob `{}` has no flag mapping",
        d.id
      );
    }
  }

  #[test]
  fn unset_knobs_emit_nothing() {
    let p = params("/c/models--o--n/snapshots/rev");
    let argv = argv_strings(&p, 1);
    for (_, flag) in VLLM_KNOB_FLAGS {
      assert!(
        !argv.contains(&flag.to_string()),
        "{flag} leaked when unset"
      );
    }
  }

  #[test]
  fn a_false_bool_emits_nothing() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    set(&mut p, "enforce_eager", "false");
    assert!(!argv_strings(&p, 1).contains(&"--enforce-eager".to_string()));
  }

  /// The knob channel must not become a way to smuggle a LAN bind or a
  /// credential in through a value.
  #[test]
  fn knob_values_cannot_smuggle_a_forbidden_head() {
    for smuggle in ["--host 0.0.0.0", "--host=0.0.0.0", "--api-key=hunter2"] {
      let mut p = params("/c/models--o--n/snapshots/rev");
      set(&mut p, "dtype", smuggle);
      let argv = argv_strings(&p, 1).join(" ");
      assert!(
        !argv.contains("0.0.0.0") && !argv.contains("hunter2"),
        "`{smuggle}` survived into argv: {argv}"
      );
    }
  }

  #[test]
  fn extras_reach_argv_after_the_knobs() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    p.extras = vec!["--max-num-batched-tokens".into(), "8192".into()];
    let argv = argv_strings(&p, 1);
    assert!(
      argv
        .windows(2)
        .any(|w| w[0] == "--max-num-batched-tokens" && w[1] == "8192"),
      "the documented extras tail must reach argv: {argv:?}"
    );
  }

  #[test]
  fn a_forbidden_extra_is_stripped_from_argv_in_both_spellings() {
    for smuggle in [
      vec!["--host".to_string(), "0.0.0.0".to_string()],
      vec!["--host=0.0.0.0".to_string()],
      vec!["--api-key=hunter2".to_string()],
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

  /// The freeze guard. vLLM's default sizes the KV cache against the pool,
  /// which on a UMA host is system RAM — measured at ~106 GB of a 121 GB box.
  #[test]
  fn kv_cap_leaves_the_host_a_reserve() {
    const GB: u64 = 1024 * 1024 * 1024;
    // Plenty free: the default budget applies, not "everything that fits".
    assert_eq!(kv_cache_cap_bytes(113 * GB, GB), Some(8 * GB));
    // Tight: the cap shrinks to what is left after weights + reserve.
    assert_eq!(kv_cache_cap_bytes(20 * GB, 8 * GB), Some(4 * GB));
    // No room at all — no knob, so the admission gate refuses instead of us
    // launching with a token cache.
    assert_eq!(kv_cache_cap_bytes(10 * GB, 8 * GB), None);
    assert_eq!(kv_cache_cap_bytes(4 * GB, 8 * GB), None);
  }

  #[test]
  fn kv_cap_renders_the_verified_flag() {
    let mut p = params("/c/models--o--n/snapshots/rev");
    set(&mut p, "kv_cache_memory_bytes", "2147483648");
    let argv = argv_strings(&p, 1);
    let i = argv
      .iter()
      .position(|a| a == "--kv-cache-memory-bytes")
      .expect("the cap must reach argv");
    assert_eq!(argv[i + 1], "2147483648");
  }

  #[test]
  fn intends_enabled_tri_state() {
    let mut c = VllmConfig::default();
    assert!(c.intends_enabled(false), "unset means on-when-found");
    c.enabled = Some(false);
    assert!(!c.intends_enabled(false));
    assert!(c.intends_enabled(true), "the force flag overrides off");
    c.enabled = Some(true);
    assert!(c.intends_enabled(false));
  }
}
