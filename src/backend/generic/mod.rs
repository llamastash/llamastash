//! Generic backend: run any OpenAI-compatible server declared in `config.yaml`.
//!
//! Knows nothing about the engine. Each `backend.generic.servers[]` entry names
//! a binary, its args and env with placeholders, its own string knobs, and the
//! readiness path; llamastash supplies the port, the readiness poll, proxy
//! routing and the stop. Everything engine-specific lives in the entry or in a
//! wrapper script the user writes.
//!
//! Entries are read once per process at config load
//! ([`install`](GenericBackend::install_config)), like every other hand-edit
//! to `config.yaml`.

pub mod argv;
pub mod config;
pub mod knobs;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

mod rows;

pub use config::{GenericConfig, GenericServer, KnobDecl, KnobSpec};

use super::identity::{BackendModelId, ModelIdentity};
use super::{
  AcceleratorSupport, Backend, KnobResolution, LaunchPlan, Lifecycle, ProcessLaunchSpec, Readiness,
  CREDENTIAL_ENV_STRIP,
};
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::launch::knobs::KnobSet;
use crate::launch::params::LaunchParams;

pub const GENERIC_BACKEND_ID: &str = "generic";

/// Synthetic catalog path of an entry: `generic://<name>`.
pub const GENERIC_PATH_SCHEME: &str = "generic://";

/// `launch_config` key carrying this launch's published id (`name` or
/// `name@launch`), stamped in [`GenericBackend::start`] for the `{name}`
/// placeholder.
const PUBLISHED_NAME_KEY: &str = "published_name";

/// One installed entry and its interned knob scope.
#[derive(Debug)]
struct Entry {
  server: GenericServer,
  scope: &'static str,
}

fn entries() -> &'static RwLock<BTreeMap<String, Arc<Entry>>> {
  static ENTRIES: OnceLock<RwLock<BTreeMap<String, Arc<Entry>>>> = OnceLock::new();
  ENTRIES.get_or_init(Default::default)
}

fn entry_named(name: &str) -> Option<Arc<Entry>> {
  entries()
    .read()
    .unwrap_or_else(|e| e.into_inner())
    .get(name)
    .cloned()
}

/// The entry name in a `generic://<name>` path, or in a minted
/// `generic://<name>/<model>` path.
pub fn entry_name_from_path(path: &Path) -> Option<&str> {
  let tail = path.to_str()?.strip_prefix(GENERIC_PATH_SCHEME)?;
  tail.split('/').next()
}

/// The minted model name in a `generic://<name>/<model>` path, else `None`.
fn minted_name_from_path(path: &Path) -> Option<&str> {
  let tail = path.to_str()?.strip_prefix(GENERIC_PATH_SCHEME)?;
  tail.split_once('/').map(|(_, model)| model)
}

fn entry_for_path(path: &Path) -> Option<Arc<Entry>> {
  entry_named(entry_name_from_path(path)?)
}

/// The server-catalog id of an entry that sets `model`.
pub fn server_id(name: &str) -> String {
  format!("{GENERIC_BACKEND_ID}-{name}")
}

/// The entry a launch of `path` runs: its own `generic://` row's, else the
/// picked server's, else (no pick) the first entry whose `model` matches.
fn entry_for_launch(path: &Path, server: Option<&str>) -> Option<Arc<Entry>> {
  if let Some(e) = entry_for_path(path) {
    return Some(e);
  }
  let all = all_entries();
  match server {
    Some(id) => all.into_iter().find(|e| server_id(&e.server.name) == id),
    None => all.into_iter().find(|e| e.server.serves(path)),
  }
}

fn all_entries() -> Vec<Arc<Entry>> {
  entries()
    .read()
    .unwrap_or_else(|e| e.into_inner())
    .values()
    .cloned()
    .collect()
}

fn gib_to_bytes(gib: f64) -> u64 {
  (gib * 1024.0 * 1024.0 * 1024.0) as u64
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GenericBackend;

impl GenericBackend {
  pub fn new() -> Self {
    Self
  }

  fn catalog_row(server: &GenericServer) -> crate::discovery::DiscoveredModel {
    use crate::discovery::{DiscoveredModel, ModelSource};
    use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
    use crate::launch::mode::LaunchMode;
    DiscoveredModel {
      path: PathBuf::from(format!("{GENERIC_PATH_SCHEME}{}", server.name)),
      parent: PathBuf::from(GENERIC_PATH_SCHEME),
      source: ModelSource::Config,
      metadata: Some(ModelMetadata {
        arch: server.arch.clone(),
        total_parameters: None,
        parameter_label: server.params.clone(),
        quant: Quant::Unknown(0),
        quant_label: server.quant.clone(),
        native_ctx: server.row_ctx(),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: !server.reasoning_effort.is_empty(),
        mode_hint: match server.modes.first() {
          Some(LaunchMode::Embedding) => ModeHint::Embedding,
          Some(LaunchMode::Rerank) => ModeHint::Rerank,
          _ => ModeHint::Chat,
        },
        // The declared figure is the launch's whole resident demand: it feeds
        // the size column, the probe budget and the admission gate alike.
        weights_bytes: server.memory_gib.map(gib_to_bytes),
        lazy_tensor_bytes: Vec::new(),
        mtp: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: Some(server.name.clone()),
      multimodal: server.vision.then_some(crate::discovery::Multimodal {
        vision: true,
        audio: false,
      }),
      supported_backends: vec![GENERIC_BACKEND_ID.to_string()],
      mtp_head: None,
    }
  }

  fn process_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ProcessLaunchSpec {
    let entry = entry_for_launch(&params.model_path, params.server.as_deref());
    let (argv, env, ready, probe, grace) = match &entry {
      Some(e) => {
        let s = &e.server;
        // A minted row always launches with its bare tag: the published id is
        // qualified (`tag@coder`) or escalated (`entry/tag`) exactly when the
        // engine would reject it, and the engine picks the model from the tag.
        let published = match minted_name_from_path(&params.model_path) {
          Some(tag) => tag.to_string(),
          None => params
            .launch_config
            .get(PUBLISHED_NAME_KEY)
            .cloned()
            .unwrap_or_else(|| s.name.clone()),
        };
        // `resolve_knobs` already refused a placeholder knob with no value, so
        // an error here means the params changed under us; spawn nothing
        // useful rather than panic.
        let composed = argv::compose(
          s,
          &params.knobs,
          &params.extras,
          port,
          &published,
          &params.model_path,
        )
        .unwrap_or_else(|msg| {
          log::error!("{msg}");
          argv::Composed {
            argv: Vec::new(),
            env: Vec::new(),
          }
        });
        let probe = match s.ready_timeout_secs {
          Some(secs) => ProbeOptions {
            timeout: Duration::from_secs(secs),
            ..probe
          },
          None => probe,
        };
        (
          composed.argv,
          composed.env,
          s.ready.clone().unwrap_or_else(|| "/v1/models".into()),
          probe,
          Duration::from_secs(s.stop_grace_secs.unwrap_or(0)),
        )
      }
      None => (
        Vec::new(),
        Vec::new(),
        "/v1/models".into(),
        probe,
        Duration::ZERO,
      ),
    };
    ProcessLaunchSpec {
      binary,
      argv,
      env,
      env_remove: CREDENTIAL_ENV_STRIP.to_vec(),
      readiness: Readiness::HttpPoll {
        path: ready,
        ready_status: 200,
      },
      probe,
      min_stop_grace: grace,
    }
  }
}

impl Backend for GenericBackend {
  fn id(&self) -> &'static str {
    GENERIC_BACKEND_ID
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  /// None compiled in: every knob is declared per entry, in a runtime table
  /// scoped to that entry ([`Backend::knob_scope`]).
  fn knobs(&self) -> &'static [crate::launch::knobs::KnobDef] {
    &[]
  }

  fn accelerators(&self) -> AcceleratorSupport {
    AcceleratorSupport::default()
  }

  async fn status_accelerators(
    &self,
    _ctx: &MethodContext,
    _device: &[super::Accelerator],
  ) -> Vec<String> {
    // Each entry runs whatever its own binary was built for, so the GPUs the
    // llama.cpp probe found on this host say nothing about it.
    Vec::new()
  }

  fn status_server_labels(&self) -> Vec<String> {
    // A `model:` entry is a catalog server and is labelled there; one without
    // is its own row and never reaches the catalog.
    let mut names: Vec<String> = all_entries()
      .iter()
      .filter(|e| e.server.model.is_none())
      .map(|e| e.server.name.clone())
      .collect();
    names.sort();
    names
  }

  fn identify(&self, path: &Path, _header_bytes: &[u8]) -> ModelIdentity {
    self.synthetic_identity(path).unwrap_or_else(|| {
      ModelIdentity::Backend(BackendModelId {
        backend: GENERIC_BACKEND_ID.to_string(),
        name: path.to_string_lossy().into_owned(),
      })
    })
  }

  fn install_config(&self, config: &super::BackendConfig) -> Result<(), String> {
    let generic = &config.generic;
    generic.validate(&|id| crate::launch::knobs::registry::resolve_static_id(id).is_some())?;
    let mut table = entries().write().unwrap_or_else(|e| e.into_inner());
    // Merge rather than replace: a process loads config once, and replacing
    // would let an unrelated load (a test, a migration re-read) drop entries
    // another caller is using.
    for server in &generic.servers {
      let defs = server
        .knobs
        .iter()
        .map(|d| knobs::def_for(&d.spec()))
        .collect();
      let scope = crate::launch::knobs::registry::install_scoped(
        &crate::launch::knobs::registry::scope_key(GENERIC_BACKEND_ID, &server.name),
        defs,
      );
      table.insert(
        server.name.clone(),
        Arc::new(Entry {
          server: server.clone(),
          scope,
        }),
      );
    }
    Ok(())
  }

  fn knob_scope(&self, path: &Path, server: Option<&str>) -> Option<&'static str> {
    entry_for_launch(path, server).map(|e| e.scope)
  }

  fn serves_mode(
    &self,
    path: &Path,
    server: Option<&str>,
    mode: crate::launch::mode::LaunchMode,
  ) -> bool {
    entry_for_launch(path, server)
      .is_none_or(|e| e.server.modes.is_empty() || e.server.modes.contains(&mode))
  }

  fn config_effort(&self, path: &Path) -> super::DeclaredEffort {
    entry_for_path(path)
      .map(|e| super::DeclaredEffort {
        levels: e.server.reasoning_effort.clone(),
        default: e.server.reasoning_effort_default.clone(),
      })
      .unwrap_or_default()
  }

  fn config_default_knobs(&self, path: &Path, server: Option<&str>) -> KnobSet {
    let mut out = KnobSet::new();
    let Some(entry) = entry_for_launch(path, server) else {
      return out;
    };
    for def in crate::launch::knobs::registry::for_backend(entry.scope) {
      let default = entry
        .server
        .knobs
        .iter()
        .map(|d| d.spec())
        .find(|s| s.knob_id() == def.id)
        .and_then(|s| s.default);
      if let Some(v) = default {
        if let Ok(value) = crate::launch::knobs::parse_value(def, &v) {
          out.set(def.knob_id(), value);
        }
      }
    }
    out
  }

  fn enabled_in_config(
    &self,
    config: &super::BackendConfig,
    _force: &BTreeMap<String, bool>,
  ) -> bool {
    config
      .generic
      .servers
      .iter()
      .any(|s| s.binary_path().is_file())
  }

  async fn config_catalog_rows(
    &self,
    config: &super::BackendConfig,
    _force: &BTreeMap<String, bool>,
  ) -> Vec<crate::discovery::DiscoveredModel> {
    // An entry with `model` is a server option on catalog rows, not a row.
    let mut out = Vec::new();
    for server in config.generic.servers.iter().filter(|s| s.model.is_none()) {
      if server.rows_from.is_some() {
        out.extend(rows::mint(server).await);
      } else {
        out.push(Self::catalog_row(server));
      }
    }
    out
  }

  fn serves_path(&self, path: &Path) -> bool {
    all_entries().iter().any(|e| e.server.serves(path))
  }

  fn server_serves(&self, server_id: &str, path: &Path) -> bool {
    all_entries()
      .iter()
      .find(|e| self::server_id(&e.server.name) == server_id)
      .is_some_and(|e| e.server.serves(path))
  }

  fn configured_servers(&self, _ctx: &MethodContext) -> Vec<super::ServerSpec> {
    all_entries()
      .iter()
      .filter(|e| e.server.model.is_some())
      .map(|e| super::ServerSpec {
        binary: e.server.binary_path(),
        name: Some(e.server.name.clone()),
      })
      .collect()
  }

  fn config_servers(&self, config: &crate::config::Config) -> Vec<super::ServerConfig> {
    config
      .backend
      .generic
      .servers
      .iter()
      .filter(|s| s.model.is_some())
      .map(|s| super::ServerConfig {
        binary: s.binary_path(),
        name: Some(s.name.clone()),
      })
      .collect()
  }

  fn available(&self, _ctx: &MethodContext) -> bool {
    !all_entries().is_empty()
  }

  fn synthetic_identity(&self, path: &Path) -> Option<ModelIdentity> {
    let entry = entry_for_path(path)?;
    let name = match minted_name_from_path(path) {
      Some(model) => format!("{}/{model}", entry.server.name),
      None => entry.server.name.clone(),
    };
    Some(ModelIdentity::Backend(BackendModelId {
      backend: GENERIC_BACKEND_ID.to_string(),
      name,
    }))
  }

  /// No orphan adoption: an arbitrary binary can't be recognised from its argv
  /// after a daemon crash, so the wrapper cleans up its own leftovers instead.
  async fn adoption_matches(
    &self,
    _recorded_path: &Path,
    _argv: &[String],
    _port: u16,
    _probe_timeout: Duration,
  ) -> bool {
    false
  }

  async fn resolve_knobs(
    &self,
    _ctx: &MethodContext,
    params: &mut LaunchParams,
    _weights_bytes: u64,
  ) -> KnobResolution {
    let mut out = KnobResolution::default();
    match entry_for_launch(&params.model_path, params.server.as_deref()) {
      Some(e) => {
        if let Err(msg) = argv::compose(
          &e.server,
          &params.knobs,
          &params.extras,
          0,
          "",
          &params.model_path,
        ) {
          out.refusal = Some(msg);
        }
      }
      None => {
        out.refusal = Some(format!(
          "no backend.generic entry serves {}; pick one with --server",
          params.model_path.display()
        ));
      }
    }
    out
  }

  /// The declared `memory_gib` is the whole demand and already rides in as
  /// the weights figure, so nothing more. Unset means no gating.
  fn projected_cache_bytes(
    &self,
    params: &LaunchParams,
    _host: &crate::launch::admission::DemandInputs,
  ) -> Option<u64> {
    entry_for_path(&params.model_path)
      .and_then(|e| e.server.memory_gib)
      .map(|_| 0)
  }

  async fn doctor_findings(
    &self,
    config: &crate::config::Config,
  ) -> Vec<crate::init::doctor::Finding> {
    use crate::init::doctor::{Finding, Severity};
    config
      .backend
      .generic
      .servers
      .iter()
      .map(|s| {
        let path = s.binary_path();
        if path.is_file() {
          Finding::from_parts(
            "generic_binary_found",
            Severity::Info,
            format!("generic `{}`: {}", s.name, path.display()),
            "",
          )
        } else {
          Finding::from_parts(
            "generic_binary_missing",
            Severity::Warning,
            format!("generic `{}`: binary {} not found", s.name, path.display()),
            "fix `binary` under backend.generic.servers in config.yaml",
          )
        }
      })
      .collect()
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

  /// The default process-per-model start, plus the one input it lacks: the
  /// launch's published id for `{name}`, which only `exec` carries.
  async fn start(
    &self,
    ctx: &MethodContext,
    mut exec: crate::daemon::launch_service::LaunchExec,
  ) -> Result<crate::daemon::launch_service::StartedLaunch, crate::ipc::protocol::ErrorObject> {
    let refuse = |msg: String| {
      crate::ipc::protocol::ErrorObject::new(crate::ipc::protocol::ErrorCode::InvalidParams, msg)
    };
    let Some(entry) = entry_for_launch(&exec.params.model_path, exec.params.server.as_deref())
    else {
      ctx
        .supervisors
        .release_reserved_port(exec.reserved_port)
        .await;
      return Err(refuse(format!(
        "no backend.generic entry for {}; restart the daemon after editing config.yaml",
        exec.params.model_path.display()
      )));
    };
    let binary = entry.server.binary_path();
    if !binary.is_file() {
      ctx
        .supervisors
        .release_reserved_port(exec.reserved_port)
        .await;
      return Err(refuse(format!(
        "backend.generic entry `{}`: binary {} not found",
        entry.server.name,
        binary.display()
      )));
    }
    // The proxy forwards `body.model` unchanged, so `{name}` must be the id
    // `/v1/models` publishes, which qualifies a stem another row shares.
    let snap = ctx.catalog.snapshot().await;
    let ids = crate::proxy::router::published_ids(&snap);
    let id_for = |p: &Path| ids.get(p.to_string_lossy().as_ref()).cloned();
    // The catalog keys canonical paths; a caller may send a symlinked one
    // (`/tmp` is `/private/tmp` on macOS).
    let model_id = id_for(&exec.params.model_path)
      .or_else(|| {
        crate::util::paths::canonicalize(&exec.params.model_path)
          .ok()
          .and_then(|c| id_for(&c))
      })
      .unwrap_or_else(|| match entry.server.model {
        Some(_) => crate::util::paths::model_display_name(&exec.params.model_path),
        None => entry.server.name.clone(),
      });
    let published = match exec.name.as_deref() {
      Some(n) => crate::launch::resolve::join_named_reference(&model_id, n),
      None => model_id,
    };
    if entry.server.rewrite_model {
      exec.params.launch_config.insert(
        crate::backend::REQUEST_MODEL_KEY.to_string(),
        published.clone(),
      );
    }
    exec
      .params
      .launch_config
      .insert(PUBLISHED_NAME_KEY.to_string(), published);
    let spec = self.process_spec(&exec.params, exec.reserved_port, binary, exec.probe);
    crate::daemon::launch_service::spawn_supervised(ctx, exec, spec, None, None).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn install(yaml: &str) {
    let generic: GenericConfig = yaml_serde::from_str(yaml).unwrap();
    let config = super::super::BackendConfig {
      generic,
      ..Default::default()
    };
    GenericBackend::new().install_config(&config).unwrap();
  }

  /// `status` names an entry without `model:` (it never reaches the server
  /// catalog) and leaves a `model:` entry to the catalog; the host's GPUs are
  /// never the backend's accelerators.
  #[tokio::test]
  async fn status_names_row_entries_and_claims_no_host_accelerators() {
    use crate::backend::{Accelerator, Backend};
    install(
      "servers:\n  \
       - {name: st-row, binary: /b, ready: /h}\n  \
       - {name: st-server, model: \"x*\", binary: /b, ready: /h, args: [\"{model}\"]}\n",
    );
    let labels = GenericBackend::new().status_server_labels();
    assert!(labels.contains(&"st-row".to_string()), "{labels:?}");
    assert!(!labels.contains(&"st-server".to_string()), "{labels:?}");
    let ctx =
      crate::daemon::context::MethodContext::new(crate::daemon::shutdown::ShutdownToken::new());
    let accels = GenericBackend::new()
      .status_accelerators(&ctx, &[Accelerator::Rocm, Accelerator::Vulkan])
      .await;
    assert!(accels.is_empty(), "{accels:?}");
  }

  /// `modes` limits what an entry serves; without it every mode passes and the
  /// engine answers or refuses. The row's mode is the first one declared.
  #[test]
  fn modes_limit_what_an_entry_serves() {
    use crate::backend::Backend;
    use crate::launch::mode::LaunchMode;
    install(
      "servers:\n  \
       - {name: md-chat, binary: /b, ready: /h, modes: [chat]}\n  \
       - {name: md-embed, binary: /b, ready: /h, modes: [embedding]}\n  \
       - {name: md-any, binary: /b, ready: /h}\n",
    );
    let serves = |name: &str, mode| {
      GenericBackend::new().serves_mode(Path::new(&format!("generic://{name}")), None, mode)
    };
    assert!(serves("md-chat", LaunchMode::Chat));
    assert!(!serves("md-chat", LaunchMode::Embedding));
    assert!(!serves("md-chat", LaunchMode::Rerank));
    assert!(serves("md-embed", LaunchMode::Embedding));
    assert!(serves("md-any", LaunchMode::Rerank));
    let embed = entry_named("md-embed").unwrap();
    let row = GenericBackend::catalog_row(&embed.server);
    assert_eq!(
      row.metadata.unwrap().mode_hint,
      crate::gguf::metadata::ModeHint::Embedding
    );
  }

  /// Declared row info fills the list columns verbatim; Ctx falls back to the
  /// `ctx: true` knob default.
  #[test]
  fn declared_row_info_fills_the_catalog_row() {
    install(
      "servers:\n  \
       - {name: info-full, binary: /b, ready: /h, arch: qwen3next, params: 80B, quant: Q4_K_M, ctx: 262144, knobs: [{flag: --if-ctx, id: if-ctx, ctx: true, default: \"65536\"}]}\n  \
       - {name: info-knob, binary: /b, ready: /h, knobs: [{flag: --ik-ctx, id: ik-ctx, ctx: true, default: \"131072\"}]}\n  \
       - {name: info-none, binary: /b, ready: /h}\n",
    );
    let md = |name: &str| {
      GenericBackend::catalog_row(&entry_named(name).unwrap().server)
        .metadata
        .unwrap()
    };
    let full = md("info-full");
    assert_eq!(full.arch.as_deref(), Some("qwen3next"));
    assert_eq!(full.parameter_label.as_deref(), Some("80B"));
    assert_eq!(full.quant_display(), "Q4_K_M");
    assert_eq!(full.native_ctx, Some(262144), "declared ctx wins");
    assert_eq!(md("info-knob").native_ctx, Some(131072));
    let none = md("info-none");
    assert_eq!(none.arch, None);
    assert_eq!(none.native_ctx, None);
  }

  /// `vision` and `reasoning_effort` mark the row the way a GGUF's projector
  /// and chat template would, and `integrations` gets the declared levels.
  /// `[none]` alone marks a reasoning model with no levels to pick.
  #[test]
  fn declared_vision_and_effort_reach_integrations() {
    install(
      "servers:\n  \
       - {name: ve-both, binary: /b, ready: /h, vision: true, reasoning_effort: [xhigh, none, low, medium], reasoning_effort_default: medium}\n  \
       - {name: ve-top, binary: /b, ready: /h, reasoning_effort: [low, xhigh]}\n  \
       - {name: ve-off, binary: /b, ready: /h, reasoning_effort: [none]}\n  \
       - {name: ve-none, binary: /b, ready: /h}\n",
    );
    let patch = |name: &str| {
      let row = GenericBackend::catalog_row(&entry_named(name).unwrap().server);
      let row = crate::discovery::catalog::catalog_row(&row);
      crate::init::external::PatchModel::from_catalog_row(&row, name.to_string())
    };
    let both = patch("ve-both");
    assert!(both.reasoning && both.vision);
    let effort = both.effort.unwrap();
    assert_eq!(effort.levels, ["low", "medium", "xhigh"]);
    assert!(effort.can_disable);
    assert_eq!(effort.preferred(), Some("medium"));
    assert_eq!(patch("ve-top").effort.unwrap().preferred(), Some("xhigh"));
    let off = patch("ve-off");
    assert!(off.reasoning && off.effort.is_none());
    let none = patch("ve-none");
    assert!(!none.reasoning && !none.vision && none.effort.is_none());
    assert_eq!(
      crate::backend::config_effort(Path::new("/m/Some-Q4.gguf")),
      super::super::DeclaredEffort::default()
    );
  }

  /// `integrations` declares this as a favorite's context when no default
  /// preset sets one: the entry's own row, and a GGUF on a picked entry. A
  /// GGUF with no pick runs on the default backend, so the entry's default
  /// does not apply to it.
  #[test]
  fn config_default_ctx_reads_the_entry_s_ctx_knob_default() {
    install(
      "servers:\n  \
       - {name: cdc-row, binary: /b, ready: /h, knobs: [{flag: --cr-ctx, id: cr-ctx, ctx: true, default: \"65536\"}]}\n  \
       - {name: cdc-srv, model: \"Cdc-*\", binary: /b, args: [\"{model}\"], ready: /h, knobs: [{flag: --cs-ctx, id: cs-ctx, ctx: true, default: \"98304\"}]}\n  \
       - {name: cdc-none, binary: /b, ready: /h, knobs: [--cn-seed]}\n",
    );
    let ctx = crate::backend::config_default_ctx;
    assert_eq!(ctx(Path::new("generic://cdc-row"), None), Some(65536));
    assert_eq!(ctx(Path::new("generic://cdc-none"), None), None);
    let gguf = Path::new("/m/Cdc-Q4.gguf");
    assert_eq!(ctx(gguf, Some(&server_id("cdc-srv"))), Some(98304));
    assert_eq!(ctx(gguf, None), None);
  }

  #[test]
  fn the_tui_editor_shows_an_unset_knob_s_entry_default() {
    use crate::tui::launch_picker::{LaunchPickerState, INHERITED_LABEL};
    install(
      "servers:\n  - {name: picker-dflt, binary: /a, ready: /h, knobs: [{flag: --pd-ctx, id: pd-ctx, default: \"4096\"}, --pd-seed]}\n",
    );
    let mut s = LaunchPickerState::for_model("picker-dflt");
    s.model_path = Some(PathBuf::from("generic://picker-dflt"));
    s.model_backend = crate::launch::params::BackendChoice::from_id(GENERIC_BACKEND_ID);
    let scope = s.knob_scope();
    let id = |name: &str| {
      crate::launch::knobs::registry::for_backend(scope)
        .iter()
        .find(|d| d.id == name)
        .unwrap()
        .knob_id()
    };
    assert_eq!(s.value_label(id("pd-ctx")), "4096");
    assert!(s.shows_config_default(id("pd-ctx")));
    assert_eq!(
      s.buffer_seed(id("pd-ctx")),
      "4096",
      "`e` edits from the default"
    );
    s.commit_text(id("pd-ctx"), "4096").unwrap();
    assert!(
      s.shows_config_default(id("pd-ctx")),
      "accepting the unchanged default keeps the row unset"
    );
    assert_eq!(s.value_label(id("pd-seed")), INHERITED_LABEL, "no default");
    assert!(!s.shows_config_default(id("pd-seed")));
    s.commit_text(id("pd-ctx"), "8192").unwrap();
    assert!(
      !s.shows_config_default(id("pd-ctx")),
      "a set value is not muted"
    );
  }

  #[test]
  fn each_entry_gets_its_own_scope_with_only_its_knobs() {
    install(
      r#"
servers:
  - {name: scope-a, binary: /a, ready: /h, knobs: [--scope-a-only, {flag: --c, id: scope-ctx, ctx: true, default: "4096"}]}
  - {name: scope-b, binary: /b, ready: /h, knobs: [--scope-b-only]}
"#,
    );
    let b = GenericBackend::new();
    let scope_a = b.knob_scope(Path::new("generic://scope-a"), None).unwrap();
    let ids: Vec<&str> = crate::launch::knobs::registry::for_backend(scope_a)
      .iter()
      .map(|d| d.id)
      .collect();
    assert_eq!(ids, ["scope-a-only", "scope-ctx"]);
    let scope_b = b.knob_scope(Path::new("generic://scope-b"), None).unwrap();
    assert!(crate::launch::knobs::registry::def_for_backend(
      scope_b,
      crate::launch::knobs::KnobId("scope-a-only")
    )
    .is_none());

    // `--ctx` lands on the `ctx: true` knob as a token count.
    let mut set = KnobSet::new();
    assert!(set.set_by_concept(
      scope_a,
      crate::launch::knobs::Concept::ContextLength,
      crate::launch::knobs::KnobValue::Set(crate::launch::knobs::Scalar::U32(65536)),
    ));
    assert_eq!(
      set.u32(crate::launch::knobs::KnobId("scope-ctx")),
      Some(65536)
    );

    let defaults = b.config_default_knobs(Path::new("generic://scope-a"), None);
    assert_eq!(
      defaults.u32(crate::launch::knobs::KnobId("scope-ctx")),
      Some(4096)
    );
  }

  #[test]
  fn a_knobset_with_an_entry_knob_round_trips_once_installed() {
    install("servers:\n  - {name: serde-a, binary: /a, ready: /h, knobs: [--serde-spec]}\n");
    let set: KnobSet = yaml_serde::from_str("serde-spec: mtp\n").unwrap();
    assert_eq!(
      set.str(crate::launch::knobs::KnobId("serde-spec")),
      Some("mtp")
    );
    let back = yaml_serde::to_string(&set).unwrap();
    assert!(back.contains("serde-spec: mtp"), "{back}");
  }

  #[test]
  fn identity_is_the_entry_name() {
    install("servers:\n  - {name: ident-a, binary: /a, ready: /h}\n");
    let id = GenericBackend::new()
      .synthetic_identity(Path::new("generic://ident-a"))
      .unwrap();
    assert_eq!(id.as_backend().unwrap().name, "ident-a");
    assert!(GenericBackend::new()
      .synthetic_identity(Path::new("generic://not-configured"))
      .is_none());
  }
}
