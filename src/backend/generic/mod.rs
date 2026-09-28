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

/// The entry name in a `generic://<name>` path.
pub fn entry_name_from_path(path: &Path) -> Option<&str> {
  path.to_str()?.strip_prefix(GENERIC_PATH_SCHEME)
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
    DiscoveredModel {
      path: PathBuf::from(format!("{GENERIC_PATH_SCHEME}{}", server.name)),
      parent: PathBuf::from(GENERIC_PATH_SCHEME),
      source: ModelSource::Config,
      metadata: Some(ModelMetadata {
        arch: None,
        total_parameters: None,
        parameter_label: None,
        quant: Quant::Unknown(0),
        quant_label: None,
        native_ctx: None,
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        // The declared figure is the launch's whole resident demand: it feeds
        // the size column, the probe budget and the admission gate alike.
        weights_bytes: server.memory_gib.map(gib_to_bytes),
        lazy_tensor_bytes: Vec::new(),
        mtp: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: Some(server.name.clone()),
      multimodal: None,
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
        let published = params
          .launch_config
          .get(PUBLISHED_NAME_KEY)
          .cloned()
          .unwrap_or_else(|| s.name.clone());
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
    config
      .generic
      .servers
      .iter()
      .filter(|s| s.model.is_none())
      .map(Self::catalog_row)
      .collect()
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
    entry_for_path(path).map(|e| {
      ModelIdentity::Backend(BackendModelId {
        backend: GENERIC_BACKEND_ID.to_string(),
        name: e.server.name.clone(),
      })
    })
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
