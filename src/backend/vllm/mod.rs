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
    Some("vllm")
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

/// The vLLM readiness contract.
///
/// `/v1/models` returning 200 **with the served name in the body**. A bare
/// health check flips too early: vLLM binds its port before the engine has
/// finished profiling and building the KV cache, a window measured at 10-27 s
/// on a 0.5B and far longer on real models.
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
  argv.push(served_model_name(&params.model_path).into());
  // Loopback only, like every other backend we spawn.
  argv.push("--host".into());
  argv.push("127.0.0.1".into());
  argv.push("--port".into());
  argv.push(port.to_string().into());
  if let Some(ctx) = params.ctx {
    argv.push("--max-model-len".into());
    argv.push(ctx.to_string().into());
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
