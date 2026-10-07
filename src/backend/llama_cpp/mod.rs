//! llama.cpp reference implementation of the [`Backend`] contract.
//!
//! Currently the sole backend. Every method **delegates** to the existing
//! launch surface rather than reimplementing it, so the wire behavior is
//! provably unchanged:
//!
//! - argv ← `compose::compose` (llama.cpp's own emitter, the `compose` submodule)
//! - identity ← [`crate::gguf::identity::compute`]
//! - the env strip ← [`LLAMA_ENV_STRIP`] (moved here from the supervisor)
//!
//! The golden parity tests below pin `prepare_launch`'s argv to
//! `compose`'s output so a future reimplementation can't silently drift.

mod actuals;
pub mod caps;
mod compose;
mod effort;
pub mod knobs;
pub mod list_devices;
mod slot_cache;
mod telemetry;

use compose::compose;
pub use list_devices::{parse_list_devices, probe_devices, BinaryDevice};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::ModelIdentity;
use super::{
  Accelerator, AcceleratorSupport, Backend, LaunchPlan, Lifecycle, ProcessLaunchSpec, Readiness,
};
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::launch::params::LaunchParams;

/// Config-derived launch-knob keys llama.cpp carries in
/// [`LaunchParams::launch_config`](crate::launch::params::LaunchParams), the
/// [`LaunchParams::launch_config`](crate::launch::params::LaunchParams) — the
/// config-projection channel, deliberately not knobs. Seeded fresh each launch
/// by [`LlamaCppBackend::seed_launch_knobs`] from daemon config rather than
/// from user intent, so they surface no editor row and no CLI flag. `compose`
/// and the admission / readiness hooks read them straight out of the map.
pub const LLAMACPP_KNOB_JINJA: &str = "jinja";
pub const LLAMACPP_KNOB_STRICT_FIT: &str = "strict_fit";
pub const LLAMACPP_KNOB_FIT_CTX_FLOOR: &str = "fit_ctx_floor";
/// `launch_config` key carrying which spelling the resolved build takes for the
/// model-loading mode. Written by `seed_binary_caps`, read by `compose`.
pub const LLAMACPP_KNOB_LOAD_MODE_DIALECT: &str = "load_mode_dialect";
/// The same fact as a [`caps`] probe key on `Server::caps`.
pub const CAP_LOAD_MODE_DIALECT: &str = "load_mode_dialect";
/// `launch_config` key carrying whether the resolved build serves the
/// `--slot-save-path` KV API. Written by `seed_binary_caps`, read by `compose`
/// and by the eviction / readiness hooks.
pub const LLAMACPP_KNOB_SLOT_SAVE_CAP: &str = "slot_save_cap";
/// The same fact as a [`caps`] probe key on `Server::caps`.
pub const CAP_SLOT_SAVE: &str = "slot_save";
/// `launch_config` key carrying this launch's slot-save directory, seeded when
/// the feature is on and the binary can do it. Its presence is the feature's
/// per-launch switch: `compose` emits `--slot-save-path`, and the save and
/// restore hooks act only when it is set.
pub const LLAMACPP_KNOB_SLOT_SAVE_DIR: &str = "slot_save_dir";
/// `launch_config` key carrying the byte cap on all saved slots, projected from
/// `backend.llamacpp.slot_cache_max_mib` next to the dir so the save hook needs
/// nothing but the launch params.
pub const LLAMACPP_KNOB_SLOT_SAVE_MAX_MIB: &str = "slot_save_max_mib";
/// The engine flag itself, refused in user extras: last-occurrence semantics
/// would let a copy beat the one `compose` emits and move the saves to a
/// directory nothing fingerprints or prunes.
pub const SLOT_SAVE_PATH_FLAG: &str = "--slot-save-path";

/// The slot-save directory this launch was seeded with, when both the config
/// and the binary allow it. `None` leaves the KV persistence feature off for
/// this launch.
fn slot_save_dir(params: &LaunchParams) -> Option<std::path::PathBuf> {
  let cap = params.launch_config.get(LLAMACPP_KNOB_SLOT_SAVE_CAP)?;
  if cap != "true" {
    return None;
  }
  params
    .launch_config
    .get(LLAMACPP_KNOB_SLOT_SAVE_DIR)
    .map(std::path::PathBuf::from)
}

/// The `fit_ctx_floor` launch knob parsed to `u32`, or `None` when unseeded /
/// unparsable. Shared by the admission-floor and readiness-gate hooks.
fn fit_ctx_floor_knob(params: &LaunchParams) -> Option<u32> {
  params
    .launch_config
    .get(LLAMACPP_KNOB_FIT_CTX_FLOOR)
    .and_then(|s| s.parse::<u32>().ok())
}

/// Environment variables removed before spawning `llama-server`.
///
/// `LLAMA_ARG_*` would let an inherited env var override the loopback /
/// auth argv contract `FORBIDDEN_ADVANCED_PREFIXES` enforces (llama.cpp
/// reads `LLAMA_ARG_HOST` etc. for every flag). `HF_*` are llamastash's
/// own pull credentials, which `llama-server` has no reason to see —
/// stripping them keeps the credential blast radius small.
///
/// This is the canonical home for the list (moved out of
/// [`crate::daemon::supervisor::spawn`]); it rides on the
/// [`ProcessLaunchSpec::env_remove`] field so the supervisor stays
/// backend-agnostic.
pub const LLAMA_ENV_STRIP: &[&str] = &[
  "LLAMA_ARG_HOST",
  "LLAMA_ARG_PORT",
  "LLAMA_ARG_BIND",
  "LLAMA_ARG_LISTEN",
  "LLAMA_ARG_API_KEY",
  "LLAMA_ARG_SSL_KEY_FILE",
  "LLAMA_ARG_SSL_CERT_FILE",
  "HF_TOKEN",
  "HUGGING_FACE_HUB_TOKEN",
  "HF_HOME",
  "HF_ENDPOINT",
];

/// Basenames llama.cpp's server ships under, primary first.
///
/// `llama-server` is the standalone server every release tarball still
/// carries. `llama` is the unified app upstream added in May 2026: one binary
/// that dispatches on a subcommand, and the only one the official installer at
/// <https://llama.app> puts on `$PATH` — so a host set up through that installer
/// has no `llama-server` at all.
pub const SERVER_BINARIES: &[&str] = &["llama-server", "llama"];

/// The subcommand argv `binary` needs ahead of the server flags: `["serve"]`
/// for the unified app, empty for the standalone server.
///
/// The two take the *same* flags — `llama serve --help` is byte-identical to
/// `llama-server --help` (verified against build 10610) — so the subcommand is the
/// whole difference, and one prefix covers both the launch argv and the
/// `--list-devices` probe. Dispatch is on the name of the binary about to be
/// spawned, not on its version: both binaries ship in the same release and
/// report the same version, so the version cannot tell them apart. A path
/// configured explicitly (flag / env / `servers`) is canonicalized before it
/// gets here, so a symlink is read as its target; a `$PATH` hit keeps the name
/// it was found under.
pub fn serve_prefix(binary: &Path) -> &'static [&'static str] {
  let unified = binary
    .file_stem()
    .and_then(|s| s.to_str())
    .is_some_and(|s| s.eq_ignore_ascii_case("llama"));
  if unified {
    &["serve"]
  } else {
    &[]
  }
}

/// llama.cpp backend configuration — the always-on default backend, so it has
/// no `enabled` field. `servers` are the `llama-server` build/binary variants
/// (the first is the *default* binary for auto / no-device launches, and the
/// back-compat target of the `--llama-server` flag / `LLAMASTASH_LLAMA_SERVER`
/// env); `jinja` / `strict_fit` / `fit_ctx_floor` are launch-behaviour knobs
/// surfaced under `backend.llamacpp` in `config.yaml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct LlamaCppConfig {
  /// The `llama-server` build/binary variants. The first entry is the default
  /// binary (auto / no-device launches); each is probed with `--list-devices`
  /// at daemon start to build the launch server/device catalog. One install can
  /// offer CUDA / ROCm / Vulkan launches by listing the matching single-backend
  /// builds — every entry is its own selectable server (no dedup across
  /// builds). Empty falls back to a `llama-server` on `$PATH`.
  #[serde(default)]
  pub servers: Vec<crate::backend::ServerConfig>,
  /// Pass `--jinja` on every launch (factory `true`) — what enables tool
  /// calling on both the OpenAI `/v1/chat/completions` and Anthropic
  /// `/v1/messages` surfaces. The reasoning toggle still forces `--jinja` on
  /// regardless.
  #[serde(default = "default_true")]
  pub jinja: bool,
  /// Refuse (rather than degrade) a launch `--fit` could not place as
  /// requested. Factory `false`.
  #[serde(default)]
  pub strict_fit: bool,
  /// `--fit-ctx` floor so `--fit` never collapses the window below a usable
  /// size. Factory [`crate::config::DEFAULT_FIT_CTX_FLOOR`].
  #[serde(default = "default_fit_ctx_floor")]
  pub fit_ctx_floor: u32,
  /// Map the Anthropic `output_config.effort` field onto
  /// `chat_template_kwargs.reasoning_effort` when the proxy forwards
  /// `/v1/messages` (factory `true`). llama.cpp's own Anthropic translation
  /// drops that field, so without the mapping a client's effort control does
  /// nothing on a local model.
  ///
  /// Set `false` when the effort should come from the launch or the engine
  /// default instead: a per-request kwarg overrides both, and a client that
  /// sends an effort value on every request would otherwise always win.
  #[serde(default = "default_true")]
  pub map_anthropic_effort: bool,
  /// Save each slot's KV cache to disk before an eviction stops a launch and
  /// restore it when a matching launch comes back, so a conversation that
  /// straddles an unload does not reprocess its whole prompt. Factory `false`:
  /// llama.cpp restore transfers no KV on hybrid/recurrent or SWA models
  /// (upstream ggml-org/llama.cpp#28194), which includes the Qwen3.5 family, so
  /// the save would write gigabytes and buy nothing. Builds without the engine's
  /// `--slot-save-path` API are skipped individually. See the `slot_cache` module.
  #[serde(default)]
  pub slot_cache: bool,
  /// Ceiling for the whole slot-save tree under the cache dir, in MiB. The
  /// oldest saves are dropped first when it is exceeded. Factory 8 GiB,
  /// roughly a 100k-token prompt on a mid-size model.
  #[serde(default = "default_slot_cache_max_mib")]
  pub slot_cache_max_mib: u32,
}

fn default_true() -> bool {
  true
}

fn default_fit_ctx_floor() -> u32 {
  crate::config::DEFAULT_FIT_CTX_FLOOR
}

fn default_slot_cache_max_mib() -> u32 {
  8192
}

impl Default for LlamaCppConfig {
  fn default() -> Self {
    Self {
      servers: Vec::new(),
      jinja: true,
      strict_fit: false,
      fit_ctx_floor: crate::config::DEFAULT_FIT_CTX_FLOOR,
      map_anthropic_effort: true,
      slot_cache: false,
      slot_cache_max_mib: default_slot_cache_max_mib(),
    }
  }
}

impl LlamaCppConfig {
  /// The configured default (first) server binary, if any — the `config_path`
  /// input to the daemon's `llama-server` locator.
  pub fn primary_binary(&self) -> Option<PathBuf> {
    self.servers.first().map(|s| s.binary.clone())
  }

  /// The additional server binaries (everything past the first) — probed for
  /// launch devices alongside the primary.
  pub fn extra_binaries(&self) -> Vec<PathBuf> {
    self
      .servers
      .iter()
      .skip(1)
      .map(|s| s.binary.clone())
      .collect()
  }
}

/// llama.cpp backend: direct, zero-overhead, fully-tuned. The product's
/// reason to exist; never routed through a wrapper.
#[derive(Debug, Clone)]
pub struct LlamaCppBackend {}

impl LlamaCppBackend {
  pub fn new() -> Self {
    // llama.cpp honors the full typed-knob vocabulary.
    Self {}
  }
}

impl Default for LlamaCppBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl LlamaCppBackend {
  /// Build the process-per-model launch spec directly.
  ///
  /// [`Backend::prepare_launch`] wraps this in a [`LaunchPlan`] so the
  /// orchestrator can branch on lifecycle shape. Call sites that have
  /// already committed to a process spawn (and tests) can skip the enum
  /// and build the spec straight away.
  pub fn process_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ProcessLaunchSpec {
    ProcessLaunchSpec {
      // Delegate to the canonical argv emitter — pinned by parity tests —
      // behind the subcommand the unified app needs (empty for `llama-server`).
      argv: serve_prefix(&binary)
        .iter()
        .map(std::ffi::OsString::from)
        .chain(compose(params, port))
        .collect(),
      binary,
      env_remove: LLAMA_ENV_STRIP.to_vec(),
      env: Vec::new(),
      min_stop_grace: std::time::Duration::ZERO,
      readiness: Readiness::HttpPoll {
        path: "/health".to_string(),
        ready_status: 200,
      },
      probe,
    }
  }
}

impl Backend for LlamaCppBackend {
  fn knobs(&self) -> &'static [crate::launch::knobs::KnobDef] {
    knobs::KNOBS
  }
  fn id(&self) -> &'static str {
    "llamacpp"
  }

  fn rewrite_request_body(
    &self,
    ctx: &MethodContext,
    endpoint: &str,
    body: &[u8],
  ) -> Option<Vec<u8>> {
    if !ctx.backend.llamacpp.map_anthropic_effort {
      return None;
    }
    effort::rewrite_request_body(endpoint, body)
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  fn accelerators(&self) -> AcceleratorSupport {
    // CPU is the always-available floor; which GPU backend a given build
    // can drive is host-/variant-specific and surfaced via the live device
    // catalog (`status` unions that in), not asserted statically here.
    AcceleratorSupport::from_list([Accelerator::Cpu])
  }

  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity {
    // Delegate — do not reimplement the `(path, BLAKE3)` identity. Wrap it
    // in the generalized seam type; the GGUF `ModelId` is unchanged.
    ModelIdentity::Gguf(crate::gguf::identity::compute(path, header_bytes))
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

  fn available(&self, ctx: &MethodContext) -> bool {
    // Installed = the resolved `llama-server` binary exists. GPU capability is
    // surfaced separately (the live device catalog); this is just presence.
    ctx
      .launch
      .as_ref()
      .and_then(|e| e.binary.as_ref())
      .is_some_and(|b| b.exists())
  }

  fn resolve_launch_binary(
    &self,
    _ctx: &MethodContext,
    default_binary: Option<PathBuf>,
    port: u16,
  ) -> Result<(PathBuf, u16), String> {
    default_binary.map(|b| (b, port)).ok_or_else(|| {
      "llama-server binary not found — point `--llama-server` / `LLAMASTASH_LLAMA_SERVER` at it \
       or run `llamastash init` to install one"
        .to_string()
    })
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    // The daemon-resolved server path, surfaced verbatim (present even when the
    // file is missing, so `status` can show *what* it looked for vs `installed`).
    ctx
      .launch
      .as_ref()
      .and_then(|e| e.binary.as_ref())
      .map(|b| b.display().to_string())
  }

  fn configured_servers(&self, ctx: &MethodContext) -> Vec<super::ServerSpec> {
    let cfg = &ctx.backend.llamacpp;
    let mut out = Vec::new();
    // Primary server = the daemon-resolved binary (CLI flag > env > config >
    // PATH); its name hint comes from the first configured `servers` entry.
    if let Some(binary) = ctx.launch.as_ref().and_then(|e| e.binary.clone()) {
      out.push(super::ServerSpec {
        binary,
        name: cfg.servers.first().and_then(|s| s.name.clone()),
      });
    }
    // Additional builds: `servers[1..]`, each canonicalized + existence-checked
    // (a missing entry contributes nothing rather than failing the probe).
    for extra in cfg.servers.iter().skip(1) {
      let resolved =
        crate::util::paths::canonicalize(&extra.binary).unwrap_or_else(|_| extra.binary.clone());
      if resolved.is_file() {
        out.push(super::ServerSpec {
          binary: resolved,
          name: extra.name.clone(),
        });
      } else {
        log::warn!(
          "extra llama-server {} not found; skipping",
          extra.binary.display()
        );
      }
    }
    out
  }

  fn config_servers(&self, config: &crate::config::Config) -> Vec<super::ServerConfig> {
    config.backend.llamacpp.servers.clone()
  }

  fn probe_devices(&self, binary: &Path) -> Vec<super::Device> {
    list_devices::probe_devices(binary)
  }

  fn probe_caps(&self, binary: &Path) -> std::collections::BTreeMap<String, String> {
    let probed = caps::BuildCaps::probe(binary);
    let mut caps = std::collections::BTreeMap::new();
    caps.insert(
      CAP_LOAD_MODE_DIALECT.to_string(),
      probed.load_mode.label().to_string(),
    );
    caps.insert(CAP_SLOT_SAVE.to_string(), probed.slot_save.to_string());
    caps
  }

  fn seed_binary_caps(&self, binary: &Path, servers: &[super::Server], params: &mut LaunchParams) {
    let probed = match servers.iter().find(|s| s.binary == binary) {
      Some(server) => caps::BuildCaps {
        load_mode: caps::LoadModeDialect::from_label(
          server.caps.get(CAP_LOAD_MODE_DIALECT).map(String::as_str),
        ),
        slot_save: server.caps.get(CAP_SLOT_SAVE).is_some_and(|s| s == "true"),
      },
      // The binary should always be a catalog row (`configured_servers` adds the
      // daemon's resolved default), so a miss means the catalog and the launch
      // disagree. Probe rather than assume: guessing wrong here emits a flag the
      // engine rejects, which costs a whole failed load to discover.
      None => {
        log::warn!(
          "{} is not in the server catalog; probing its flags directly",
          binary.display()
        );
        caps::BuildCaps::probe(binary)
      }
    };
    params.launch_config.insert(
      LLAMACPP_KNOB_LOAD_MODE_DIALECT.to_string(),
      probed.load_mode.label().to_string(),
    );
    params.launch_config.insert(
      LLAMACPP_KNOB_SLOT_SAVE_CAP.to_string(),
      probed.slot_save.to_string(),
    );
  }

  fn launch_priority(&self) -> i32 {
    // The stable default engine.
    10
  }

  fn process_markers(&self) -> &'static [&'static str] {
    SERVER_BINARIES
  }

  /// The unified app is one binary for every mode, so a matched basename is not
  /// yet a server: `llama cli` and `llama download` carry the same name. Require
  /// the subcommand [`serve_prefix`] would have launched with, which leaves the
  /// standalone binary (empty prefix) matching on its basename alone. An argv
  /// the OS won't hand over falls back to the basename verdict rather than
  /// dropping a process that may well be a server.
  fn argv_is_server(&self, argv: &[String]) -> bool {
    let Some(exe) = argv.first() else {
      return true;
    };
    let expected = serve_prefix(Path::new(exe));
    argv.len() > expected.len()
      && expected
        .iter()
        .zip(argv[1..].iter())
        .all(|(want, got)| got == want)
  }

  fn serves_web_ui(&self) -> bool {
    // llama-server ships a stock browser web UI the proxy's `/ui` reverse-proxies.
    // The one backend that opts into the default-off `serves_web_ui`.
    true
  }

  fn seed_launch_knobs(&self, ctx: &MethodContext, params: &mut LaunchParams) {
    // Project the daemon's config-derived launch knobs onto `backend_knobs`,
    // fresh each launch (config, not user intent) so an inherited last_params
    // value can never stick them. `--jinja` is a llamastash default, so the
    // bench parity escape hatch suppresses it (keeps `start` byte-identical to
    // raw `llama-server` for Suite-A overhead); `compose` ORs reasoning in.
    let cfg = &ctx.backend.llamacpp;
    let jinja = cfg.jinja && !crate::launch::params::bench_disable_defaults_from_env();
    if jinja {
      params
        .launch_config
        .insert(LLAMACPP_KNOB_JINJA.to_string(), "true".into());
    } else {
      // Jinja off (config `jinja: false` or bench parity): drop the key so
      // `compose` emits no `--jinja`.
      params.launch_config.remove(LLAMACPP_KNOB_JINJA);
    }
    params.launch_config.insert(
      LLAMACPP_KNOB_STRICT_FIT.to_string(),
      cfg.strict_fit.to_string(),
    );
    params.launch_config.insert(
      LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(),
      cfg.fit_ctx_floor.to_string(),
    );
    // The KV save dir, created here because the child rejects a
    // `--slot-save-path` that is not an existing directory and this is the last
    // hook before it spawns. Whether the *binary* can take the flag is a
    // separate verdict (`seed_binary_caps`, which runs later); a launch on a
    // build without it leaves the directory empty and the next save's prune
    // pass removes it. Removed when off so an inherited `last_params` copy
    // can't point a later launch at a dead directory.
    params.launch_config.remove(LLAMACPP_KNOB_SLOT_SAVE_DIR);
    params.launch_config.remove(LLAMACPP_KNOB_SLOT_SAVE_MAX_MIB);
    if cfg.slot_cache && !crate::launch::params::bench_disable_defaults_from_env() {
      if let Some(dir) = slot_cache::create_save_dir() {
        params.launch_config.insert(
          LLAMACPP_KNOB_SLOT_SAVE_DIR.to_string(),
          dir.display().to_string(),
        );
        params.launch_config.insert(
          LLAMACPP_KNOB_SLOT_SAVE_MAX_MIB.to_string(),
          cfg.slot_cache_max_mib.to_string(),
        );
      }
    }
  }

  fn admission_ctx_floor(&self, params: &LaunchParams) -> Option<u32> {
    // The `--fit-ctx` floor the launch will pass, projected as the admission
    // demand's ctx when `ctx` is unpinned.
    fit_ctx_floor_knob(params)
  }

  fn readiness_fit_gate(
    &self,
    params: &LaunchParams,
    native_ctx: Option<u32>,
  ) -> Option<crate::daemon::supervisor::FitGate> {
    // The strict-fit ctx-clamp gate is meaningful only when ctx is delegated to
    // `--fit` (a pinned ctx suppresses it — fit honors the pin) and the trained
    // window is known to compare against.
    if params.ctx.is_some() {
      return None;
    }
    let floor = fit_ctx_floor_knob(params).unwrap_or(crate::config::DEFAULT_FIT_CTX_FLOOR);
    let strict = params
      .launch_config
      .get(LLAMACPP_KNOB_STRICT_FIT)
      .is_some_and(|s| s == "true");
    native_ctx.map(|native| crate::daemon::supervisor::FitGate {
      floor,
      native,
      strict,
    })
  }

  async fn fetch_actuals(
    &self,
    port: u16,
    timeout: std::time::Duration,
  ) -> crate::daemon::actuals::Actuals {
    // The llama-server-specific `/props` fetch + `n_ctx` parse.
    actuals::fetch_props_actuals(port, timeout).await
  }

  fn prune_cache(&self, cfg: &super::BackendConfig) {
    slot_cache::prune_at_boot(u64::from(cfg.llamacpp.slot_cache_max_mib) * 1024 * 1024);
  }

  async fn on_evict(&self, port: u16, params: &LaunchParams) {
    let Some(dir) = slot_save_dir(params) else {
      return;
    };
    let mib = params
      .launch_config
      .get(LLAMACPP_KNOB_SLOT_SAVE_MAX_MIB)
      .and_then(|s| s.parse::<u32>().ok())
      .unwrap_or_else(default_slot_cache_max_mib);
    slot_cache::save(port, params, &dir, u64::from(mib) * 1024 * 1024).await;
  }

  async fn on_ready(&self, port: u16, params: &LaunchParams) {
    let Some(dir) = slot_save_dir(params) else {
      return;
    };
    slot_cache::restore(port, params, &dir).await;
  }

  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    &[SLOT_SAVE_PATH_FLAG]
  }

  fn gpu_resident(&self, params: &LaunchParams, layer_count: Option<u64>) -> bool {
    use crate::launch::knobs::{kid, KnobValue};
    let k = &params.knobs;
    if k
      .str(kid("device"))
      .is_some_and(|d| d.eq_ignore_ascii_case("none"))
      || k.u32(kid("n-cpu-moe")).is_some_and(|n| n > 0)
    {
      return false;
    }
    // Unset or `auto` is `--fit`, which offloads every layer when they fit.
    let all_layers = match k.get(kid("n-gpu-layers")) {
      None | Some(KnobValue::Auto) => true,
      Some(_) => k
        .u32(kid("n-gpu-layers"))
        .zip(layer_count)
        .is_some_and(|(n, layers)| u64::from(n) >= layers),
    };
    // A hand-passed placement flag can move work to the CPU; don't parse it.
    const PLACEMENT_FLAGS: &[&str] = &[
      "-ngl",
      "--gpu-layers",
      "--n-gpu-layers",
      "-dev",
      "--device",
      "-cmoe",
      "--cpu-moe",
      "-ncmoe",
      "--n-cpu-moe",
      "-ot",
      "--override-tensor",
    ];
    all_layers
      && !PLACEMENT_FLAGS
        .iter()
        .any(|f| crate::launch::params::extras_have_flag(&params.extras, f))
  }

  fn speculation_set_in_extras(&self, extras: &[std::ffi::OsString]) -> bool {
    // llama-server *appends* spec types rather than replacing, so emitting ours
    // on top of a hand-passed `--spec-type` would leave two configured.
    crate::launch::params::extras_have_flag(extras, "--spec-type")
  }

  fn mtp_active(&self, params: &LaunchParams) -> bool {
    // Live exactly when `compose` emitted `--spec-type draft-mtp`, which it
    // keys on the resolved directive.
    params.mtp_directive.is_some()
  }

  fn draft_acceptance(&self, log_lines: &[String]) -> Option<crate::backend::DraftAcceptance> {
    // Printed on the slot timing line, so it stays `None` until the model has
    // served enough tokens for one.
    telemetry::latest_draft_acceptance(log_lines)
  }
}

// The cross-backend dispatch enum (`Backends`) lives in the parent module
// (`crate::backend`) now that there is more than one backend — see
// `resolve_backend` / `backend_for_identity` and `impl Backend for Backends`
// there.

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::mode::LaunchMode;
  use std::ffi::OsString;

  #[test]
  fn anthropic_effort_mapping_is_switchable_off() {
    // The mapping overrides whatever effort the launch itself set, so a user
    // who wants the engine default has to be able to turn it off.
    let mut ctx =
      crate::daemon::context::MethodContext::new(crate::daemon::shutdown::ShutdownToken::new());
    let body = r#"{"output_config":{"effort":"xhigh"}}"#;
    let backend = LlamaCppBackend::new();

    ctx.backend.llamacpp.map_anthropic_effort = true;
    let rewritten = backend
      .rewrite_request_body(&ctx, "/v1/messages", body.as_bytes())
      .expect("mapped by default");
    assert_eq!(
      String::from_utf8(rewritten).expect("utf-8 json"),
      r#"{"output_config":{"effort":"xhigh"},"chat_template_kwargs":{"reasoning_effort":"xhigh"}}"#,
    );

    ctx.backend.llamacpp.map_anthropic_effort = false;
    assert_eq!(
      backend.rewrite_request_body(&ctx, "/v1/messages", body.as_bytes()),
      None,
      "off means the client's bytes go through untouched"
    );
  }

  #[test]
  fn speculation_set_in_extras_defers_to_a_hand_passed_spec_type() {
    // KD3: llama-server appends spec types rather than replacing, so a user
    // driving `--spec-type` themselves must stop llamastash adding a second.
    let b = LlamaCppBackend::new();
    let space = vec![
      OsString::from("--spec-type"),
      OsString::from("draft-simple"),
    ];
    assert!(b.speculation_set_in_extras(&space));
    assert!(b.speculation_set_in_extras(&[OsString::from("--spec-type=eagle3")]));
    assert!(!b.speculation_set_in_extras(&[OsString::from("--flash-attn")]));
    assert!(!b.speculation_set_in_extras(&[]));
  }

  #[test]
  fn mtp_active_and_acceptance_report_what_this_backend_dispatched() {
    let b = LlamaCppBackend::new();
    let mut p = LaunchParams::new(PathBuf::from("/m/x.gguf"), LaunchMode::Chat);
    assert!(!b.mtp_active(&p), "no directive → not speculating");
    p.mtp_directive = Some(crate::launch::params::MtpDirective { draft_model: None });
    assert!(b.mtp_active(&p), "directive → speculating");

    let lines = vec![
      "slot print_timing: draft acceptance = 0.65217 ( 105 accepted / 161 generated )".to_string(),
    ];
    let got = b
      .draft_acceptance(&lines)
      .expect("parses its own log format");
    assert_eq!((got.accepted, got.generated), (105, 161));
    assert!(b.draft_acceptance(&[]).is_none());
  }

  #[test]
  fn gpu_resident_only_when_nothing_is_placed_on_the_cpu() {
    let b = LlamaCppBackend::new();
    let with = |knobs: crate::launch::knobs::KnobSet, extras: &[&str]| {
      let mut p = LaunchParams::new(PathBuf::from("/m/x.gguf"), LaunchMode::Chat);
      p.knobs = knobs;
      p.extras = extras.iter().map(std::ffi::OsString::from).collect();
      p
    };
    let layers = Some(64);
    assert!(b.gpu_resident(&with(crate::knobset! {}, &[]), layers));
    assert!(b.gpu_resident(&with(crate::knobset! { n_gpu_layers: auto }, &[]), layers));
    assert!(b.gpu_resident(&with(crate::knobset! { n_gpu_layers: 99 }, &[]), layers));
    assert!(!b.gpu_resident(&with(crate::knobset! { n_gpu_layers: 32 }, &[]), layers));
    assert!(!b.gpu_resident(&with(crate::knobset! { n_gpu_layers: 99 }, &[]), None));
    assert!(!b.gpu_resident(&with(crate::knobset! { n_gpu_layers: 0 }, &[]), layers));
    assert!(!b.gpu_resident(&with(crate::knobset! { n_cpu_moe: 8 }, &[]), layers));
    assert!(!b.gpu_resident(&with(crate::knobset! { device: "none" }, &[]), layers));
    assert!(!b.gpu_resident(&with(crate::knobset! {}, &["-ngl", "0"]), layers));
    assert!(!b.gpu_resident(&with(crate::knobset! {}, &["--device=none"]), layers));
  }

  fn spec_of(plan: LaunchPlan) -> ProcessLaunchSpec {
    match plan {
      LaunchPlan::SpawnProcess(s) => s,
      LaunchPlan::DelegateToManager(_) => panic!("llama.cpp must produce a SpawnProcess plan"),
    }
  }

  fn full_knobs() -> crate::launch::knobs::KnobSet {
    // Mirror the canonical-order fixture in params.rs so the parity
    // assertion exercises every emitted flag.
    crate::knobset! {
      ctx: 32768,
      reasoning: true,
      n_gpu_layers: 99,
      n_cpu_moe: 12,
      threads: 8,
      cache_type_k: "q8_0",
      cache_type_v: "q8_0",
      flash_attn: true,
      mlock: true,
      no_mmap: true,
      parallel: 4,
      batch_size: 2048,
      ubatch_size: 512,
      rope_freq_scale: 1.0,
      keep: 128,
      tensor_split: "3,1",
      main_gpu: 0,
      split_mode: "layer",
    }
  }

  // ---- Unified `llama` app: the `serve` subcommand ----

  #[test]
  fn serve_prefix_is_keyed_on_the_binary_name() {
    for (path, want) in [
      ("/bin/llama-server", &[][..]),
      ("/bin/llama-server.exe", &[][..]),
      ("/opt/builds/rocm/llama-server-cuda", &[][..]),
      ("/home/u/.local/bin/llama", &["serve"][..]),
      // Windows: what the official installer drops in `WindowsApps`.
      ("llama.exe", &["serve"][..]),
      ("/home/u/.llama-app/LLAMA.EXE", &["serve"][..]),
      // Not the app: a longer name that merely starts with it.
      ("/bin/llama-cli", &[][..]),
      ("/bin/llamastash", &[][..]),
    ] {
      assert_eq!(serve_prefix(Path::new(path)), want, "{path}");
    }
  }

  #[test]
  fn argv_prepends_serve_for_the_unified_binary_only() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let unified = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/home/u/.local/bin/llama"),
      ProbeOptions::default(),
    ));
    let expected: Vec<OsString> = std::iter::once(OsString::from("serve"))
      .chain(compose(&p, 41100))
      .collect();
    assert_eq!(unified.argv, expected);
    // Same flags either way — the subcommand is the whole difference.
    assert_eq!(&unified.argv[1..], &compose(&p, 41100)[..]);
  }

  #[test]
  fn only_the_unified_app_s_serve_mode_counts_as_a_server() {
    let b = LlamaCppBackend::new();
    let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    // The standalone binary has no other modes to confuse it with.
    assert!(b.argv_is_server(&argv(&["/bin/llama-server", "--port", "8080"])));
    assert!(b.argv_is_server(&argv(&["/bin/llama-server-vulkan", "-m", "a.gguf"])));

    assert!(b.argv_is_server(&argv(&[
      "/home/u/.local/bin/llama",
      "serve",
      "-m",
      "a.gguf"
    ])));
    // The same binary in one of its other modes is not a server the daemon
    // should list, adopt a port from, or accept as a `stop` target.
    for other in [
      vec!["/home/u/.local/bin/llama", "cli", "-m", "a.gguf"],
      vec![
        "/home/u/.local/bin/llama",
        "download",
        "ggml-org/gemma-3-1b",
      ],
      vec!["/home/u/.local/bin/llama", "--version"],
      vec!["/home/u/.local/bin/llama"],
    ] {
      assert!(!b.argv_is_server(&argv(&other)), "{other:?}");
    }

    // No argv to read (a protected process): keep the basename verdict rather
    // than dropping a real server.
    assert!(b.argv_is_server(&[]));
  }

  #[test]
  fn the_argv_gate_routes_through_the_registry_and_ignores_unknown_markers() {
    let serve = ["/home/u/.local/bin/llama", "serve", "-m", "a.gguf"].map(String::from);
    let cli = ["/home/u/.local/bin/llama", "cli"].map(String::from);
    assert!(crate::backend::external_argv_is_server("llama", &serve));
    assert!(!crate::backend::external_argv_is_server("llama", &cli));
    // A marker no backend claims (what the sweep's own tests inject) passes.
    assert!(crate::backend::external_argv_is_server(
      "not-a-backend-marker",
      &cli
    ));
  }

  #[test]
  fn the_unified_app_is_an_alternate_marker_not_the_primary() {
    // Order is the resolution order: an install carrying both binaries keeps
    // resolving to the standalone server.
    let markers = LlamaCppBackend::new().process_markers();
    assert_eq!(markers, &["llama-server", "llama"]);
  }

  // ---- Parity: prepare_launch argv == compose() byte-for-byte ----

  #[test]
  fn argv_matches_compose_for_minimal_chat_params() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(spec.argv, compose(&p, 41100));
  }

  #[test]
  fn argv_matches_compose_for_full_knobs_ctx_reasoning_and_extras() {
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    p.ctx = Some(32768);
    p.reasoning = true;
    p.knobs = full_knobs();
    p.extras = vec![OsString::from("--threads-batch"), OsString::from("16")];
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      55555,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    // The whole point: identical to the canonical emitter, not a reimpl.
    assert_eq!(spec.argv, compose(&p, 55555));
  }

  #[test]
  fn argv_matches_compose_for_embedding_and_rerank_modes() {
    for mode in [LaunchMode::Embedding, LaunchMode::Rerank] {
      let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), mode);
      let spec = spec_of(LlamaCppBackend::new().prepare_launch(
        &p,
        41100,
        PathBuf::from("/bin/llama-server"),
        ProbeOptions::default(),
      ));
      assert_eq!(
        spec.argv,
        compose(&p, 41100),
        "mode {mode:?} must match compose"
      );
    }
  }

  #[test]
  fn argv_strips_forbidden_extras_exactly_like_compose() {
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    // A loopback-bypass attempt in extras must be stripped identically
    // to compose — the security contract survives the seam.
    p.extras = vec![OsString::from("--host"), OsString::from("0.0.0.0")];
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(spec.argv, compose(&p, 41100));
    // And the dangerous binding is gone (compose already guarantees this;
    // assert it explicitly so the intent is legible).
    assert!(!spec.argv.iter().any(|a| a == "0.0.0.0"));
  }

  #[test]
  fn minimal_params_emit_no_default_knobs_at_the_backend_layer() {
    // Parity contract (LLAMASTASH_BENCH_DISABLE_DEFAULTS): defaults are a
    // resolver / seed concern; the backend must not inject any of its own.
    // Empty knobs in => only the host/port/-m skeleton out. `jinja` is one
    // such default (factory-on, suppressed under bench parity); with no
    // `jinja` key seeded in `backend_knobs`, `compose` emits no `--jinja`.
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    let argv: Vec<String> = spec
      .argv
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    assert_eq!(
      argv,
      vec![
        "--host",
        "127.0.0.1",
        "--port",
        "41100",
        "-m",
        "/m/model.gguf"
      ]
    );
  }

  // ---- env strip, readiness, binary passthrough ----

  #[test]
  fn env_remove_is_exactly_the_supervisors_strip_set() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(
      spec.env_remove,
      vec![
        "LLAMA_ARG_HOST",
        "LLAMA_ARG_PORT",
        "LLAMA_ARG_BIND",
        "LLAMA_ARG_LISTEN",
        "LLAMA_ARG_API_KEY",
        "LLAMA_ARG_SSL_KEY_FILE",
        "LLAMA_ARG_SSL_CERT_FILE",
        "HF_TOKEN",
        "HUGGING_FACE_HUB_TOKEN",
        "HF_HOME",
        "HF_ENDPOINT",
      ]
    );
  }

  #[test]
  fn readiness_is_health_two_hundred() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(
      spec.readiness,
      Readiness::HttpPoll {
        path: "/health".to_string(),
        ready_status: 200,
      }
    );
  }

  #[test]
  fn binary_is_passed_through_verbatim() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/opt/cuda/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(spec.binary, PathBuf::from("/opt/cuda/llama-server"));
  }

  // ---- id / lifecycle / capabilities / identify ----

  #[test]
  fn id_and_lifecycle_are_stable() {
    let b = LlamaCppBackend::new();
    assert_eq!(b.id(), "llamacpp");
    assert_eq!(b.lifecycle(), Lifecycle::ProcessPerModel);
  }

  #[test]
  fn identify_delegates_to_gguf_identity() {
    let b = LlamaCppBackend::new();
    let bytes = b"GGUF\x03\x00\x00\x00 header";
    let via_backend = b.identify(Path::new("/m/model.gguf"), bytes);
    let direct = crate::gguf::identity::compute(Path::new("/m/model.gguf"), bytes);
    // llama.cpp identity is the GGUF identity, wrapped in the seam type.
    assert_eq!(via_backend.as_gguf(), Some(&direct));
  }

  /// The KV save hooks, `compose`, and the eviction path all key off this one
  /// lookup, so a launch that should not touch the disk has to read as
  /// unseeded: no dir, or a binary the probe says cannot save.
  #[test]
  fn the_slot_dir_is_live_only_with_the_cap_and_a_dir() {
    let mut p = LaunchParams::new(PathBuf::from("/m/m.gguf"), LaunchMode::Chat);
    assert_eq!(slot_save_dir(&p), None);
    p.launch_config
      .insert(LLAMACPP_KNOB_SLOT_SAVE_DIR.to_string(), "/s/1".into());
    assert_eq!(slot_save_dir(&p), None, "no probe verdict means off");
    p.launch_config
      .insert(LLAMACPP_KNOB_SLOT_SAVE_CAP.to_string(), "false".into());
    assert_eq!(slot_save_dir(&p), None);
    p.launch_config
      .insert(LLAMACPP_KNOB_SLOT_SAVE_CAP.to_string(), "true".into());
    assert_eq!(slot_save_dir(&p), Some(PathBuf::from("/s/1")));
  }

  #[test]
  fn the_slot_save_head_is_refused_in_extras() {
    assert!(LlamaCppBackend::new()
      .forbidden_extra_heads()
      .contains(&SLOT_SAVE_PATH_FLAG));
  }

  // Cross-backend `Backends` enum-dispatch forwarding is tested in the
  // parent module (`crate::backend`), where the enum now lives.

  // ---- config-derived launch knobs (jinja / strict_fit / fit_ctx_floor) ----
}
