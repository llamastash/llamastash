//! llama.cpp reference implementation of the [`Backend`] contract.
//!
//! Currently the sole backend. Every method **delegates** to the existing
//! launch surface rather than reimplementing it, so the wire behavior is
//! provably unchanged:
//!
//! - argv ← `compose::compose` (llama.cpp's own emitter, the `compose` submodule)
//! - identity ← [`crate::gguf::identity::compute`]
//! - capabilities ← every [`crate::launch::flag_aliases`] knob
//! - the env strip ← [`LLAMA_ENV_STRIP`] (moved here from the supervisor)
//!
//! The golden parity tests below pin `prepare_launch`'s argv to
//! `compose`'s output so a future reimplementation can't silently drift.

mod actuals;
mod compose;
pub mod list_devices;
mod telemetry;

use compose::compose;
pub use list_devices::{parse_list_devices, probe_devices, BinaryDevice};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::ModelIdentity;
use super::{
  Accelerator, AcceleratorSupport, Backend, KnobCapability, LaunchPlan, Lifecycle,
  ProcessLaunchSpec, Readiness,
};
use crate::config::KnobValue;
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::launch::params::LaunchParams;

/// Config-derived launch-knob keys llama.cpp carries in
/// [`LaunchParams::backend_knobs`] (string-encoded, like ds4's native knobs).
/// These are config projections seeded fresh each launch by
/// [`LlamaCppBackend::seed_launch_knobs`], **not** [`Backend::native_knobs`]
/// descriptors — they surface no TUI picker row. `compose` and the admission /
/// readiness hooks read them straight out of the map.
pub const LLAMACPP_KNOB_JINJA: &str = "jinja";
pub const LLAMACPP_KNOB_STRICT_FIT: &str = "strict_fit";
pub const LLAMACPP_KNOB_FIT_CTX_FLOOR: &str = "fit_ctx_floor";

/// The `fit_ctx_floor` launch knob parsed to `u32`, or `None` when unseeded /
/// unparsable. Shared by the admission-floor and readiness-gate hooks.
fn fit_ctx_floor_knob(params: &LaunchParams) -> Option<u32> {
  params
    .backend_knobs
    .get(LLAMACPP_KNOB_FIT_CTX_FLOOR)
    .and_then(|kv| kv.as_set())
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
/// <https://llama.app> puts on `$PATH` — so a `winget` / `irm` install has no
/// `llama-server` at all.
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
}

fn default_true() -> bool {
  true
}

fn default_fit_ctx_floor() -> u32 {
  crate::config::DEFAULT_FIT_CTX_FLOOR
}

impl Default for LlamaCppConfig {
  fn default() -> Self {
    Self {
      servers: Vec::new(),
      jinja: true,
      strict_fit: false,
      fit_ctx_floor: crate::config::DEFAULT_FIT_CTX_FLOOR,
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
pub struct LlamaCppBackend {
  capabilities: KnobCapability,
}

impl LlamaCppBackend {
  pub fn new() -> Self {
    // llama.cpp honors the full typed-knob vocabulary.
    Self {
      capabilities: KnobCapability::all(),
    }
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
      readiness: Readiness::HttpPoll {
        path: "/health".to_string(),
        ready_status: 200,
      },
      probe,
    }
  }
}

impl Backend for LlamaCppBackend {
  fn id(&self) -> &'static str {
    "llamacpp"
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  fn capabilities(&self) -> &KnobCapability {
    &self.capabilities
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
      .map(|e| e.binary.exists())
      .unwrap_or(false)
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    // The daemon-resolved server path, surfaced verbatim (present even when the
    // file is missing, so `status` can show *what* it looked for vs `installed`).
    ctx.launch.as_ref().map(|e| e.binary.display().to_string())
  }

  fn configured_servers(&self, ctx: &MethodContext) -> Vec<super::ServerSpec> {
    let cfg = &ctx.backend.llamacpp;
    let mut out = Vec::new();
    // Primary server = the daemon-resolved binary (CLI flag > env > config >
    // PATH); its name hint comes from the first configured `servers` entry.
    if let Some(env) = ctx.launch.as_ref() {
      out.push(super::ServerSpec {
        binary: env.binary.clone(),
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

  fn launch_priority(&self) -> i32 {
    // The stable default engine; ds4 outranks it for a compatible DeepSeek-V4.
    10
  }

  fn process_markers(&self) -> &'static [&'static str] {
    SERVER_BINARIES
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
      params.backend_knobs.insert(
        LLAMACPP_KNOB_JINJA.to_string(),
        KnobValue::Set("true".into()),
      );
    } else {
      // Jinja off (config `jinja: false` or bench parity): drop the key so
      // `compose` emits no `--jinja`.
      params.backend_knobs.remove(LLAMACPP_KNOB_JINJA);
    }
    params.backend_knobs.insert(
      LLAMACPP_KNOB_STRICT_FIT.to_string(),
      KnobValue::Set(cfg.strict_fit.to_string()),
    );
    params.backend_knobs.insert(
      LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(),
      KnobValue::Set(cfg.fit_ctx_floor.to_string()),
    );
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
      .backend_knobs
      .get(LLAMACPP_KNOB_STRICT_FIT)
      .and_then(|kv| kv.as_set())
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
  use crate::config::{KnobValue, TypedKnobs};
  use crate::launch::flag_aliases::knob_specs;
  use crate::launch::mode::LaunchMode;
  use std::ffi::OsString;

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

  fn spec_of(plan: LaunchPlan) -> ProcessLaunchSpec {
    match plan {
      LaunchPlan::SpawnProcess(s) => s,
      LaunchPlan::DelegateToManager(_) => panic!("llama.cpp must produce a SpawnProcess plan"),
    }
  }

  fn full_knobs() -> TypedKnobs {
    // Mirror the canonical-order fixture in params.rs so the parity
    // assertion exercises every emitted flag.
    TypedKnobs {
      ctx: Some(KnobValue::Set(32768)),
      reasoning: Some(KnobValue::Set(true)),
      n_gpu_layers: Some(KnobValue::Set(99)),
      n_cpu_moe: Some(KnobValue::Set(12)),
      threads: Some(KnobValue::Set(8)),
      cache_type_k: Some(KnobValue::Set("q8_0".into())),
      cache_type_v: Some(KnobValue::Set("q8_0".into())),
      flash_attn: Some(KnobValue::Set(true)),
      mlock: Some(KnobValue::Set(true)),
      no_mmap: Some(KnobValue::Set(true)),
      parallel: Some(KnobValue::Set(4)),
      batch_size: Some(KnobValue::Set(2048)),
      ubatch_size: Some(KnobValue::Set(512)),
      rope_freq_scale: Some(KnobValue::Set(1.0)),
      keep: Some(KnobValue::Set(128)),
      device: None,
      tensor_split: Some(KnobValue::Set("3,1".into())),
      main_gpu: Some(KnobValue::Set(0)),
      split_mode: Some(KnobValue::Set("layer".into())),
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
  fn capabilities_cover_every_knob() {
    let b = LlamaCppBackend::new();
    for spec in knob_specs() {
      assert!(b.capabilities().supports(spec.field));
    }
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

  // Cross-backend `Backends` enum-dispatch forwarding is tested in the
  // parent module (`crate::backend`), where the enum now lives.

  // ---- config-derived launch knobs (jinja / strict_fit / fit_ctx_floor) ----

  fn ctx_with_env(jinja: bool, strict_fit: bool, fit_ctx_floor: u32) -> MethodContext {
    use crate::backend::BackendConfig;
    use crate::daemon::shutdown::ShutdownToken;
    // `seed_launch_knobs` reads its config from `ctx.backend.llamacpp`, so the
    // launch-behaviour knobs ride the backend config, not a `LaunchEnv`.
    let backend = BackendConfig {
      llamacpp: LlamaCppConfig {
        jinja,
        strict_fit,
        fit_ctx_floor,
        ..Default::default()
      },
      ..Default::default()
    };
    MethodContext::new(ShutdownToken::new())
      .with_backend(backend, std::collections::BTreeMap::new())
  }

  #[test]
  fn seed_launch_knobs_projects_config_into_backend_knobs() {
    let ctx = ctx_with_env(true, true, 8192);
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    LlamaCppBackend::new().seed_launch_knobs(&ctx, &mut p);
    assert_eq!(
      p.backend_knobs.get(LLAMACPP_KNOB_JINJA),
      Some(&KnobValue::Set("true".into()))
    );
    assert_eq!(
      p.backend_knobs.get(LLAMACPP_KNOB_STRICT_FIT),
      Some(&KnobValue::Set("true".into()))
    );
    assert_eq!(
      p.backend_knobs.get(LLAMACPP_KNOB_FIT_CTX_FLOOR),
      Some(&KnobValue::Set("8192".into()))
    );
  }

  #[test]
  fn seed_launch_knobs_drops_jinja_when_config_off() {
    let ctx = ctx_with_env(false, false, 16384);
    // A stale inherited jinja knob must be removed, not left set.
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    p.backend_knobs.insert(
      LLAMACPP_KNOB_JINJA.to_string(),
      KnobValue::Set("true".into()),
    );
    LlamaCppBackend::new().seed_launch_knobs(&ctx, &mut p);
    assert!(
      !p.backend_knobs.contains_key(LLAMACPP_KNOB_JINJA),
      "jinja off => no key seeded, so compose emits no --jinja"
    );
    assert_eq!(
      p.backend_knobs.get(LLAMACPP_KNOB_STRICT_FIT),
      Some(&KnobValue::Set("false".into()))
    );
  }

  #[test]
  fn seed_launch_knobs_overwrites_inherited_values() {
    let ctx = ctx_with_env(true, false, 16384);
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    // Inherited (e.g. from last_params) values that must be re-projected.
    p.backend_knobs.insert(
      LLAMACPP_KNOB_STRICT_FIT.to_string(),
      KnobValue::Set("true".into()),
    );
    p.backend_knobs.insert(
      LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(),
      KnobValue::Set("999".into()),
    );
    LlamaCppBackend::new().seed_launch_knobs(&ctx, &mut p);
    assert_eq!(
      p.backend_knobs.get(LLAMACPP_KNOB_STRICT_FIT),
      Some(&KnobValue::Set("false".into())),
      "config wins over an inherited strict_fit"
    );
    assert_eq!(
      p.backend_knobs.get(LLAMACPP_KNOB_FIT_CTX_FLOOR),
      Some(&KnobValue::Set("16384".into())),
      "config wins over an inherited fit_ctx_floor"
    );
  }

  #[test]
  fn admission_ctx_floor_reads_the_seeded_knob() {
    let b = LlamaCppBackend::new();
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    assert_eq!(b.admission_ctx_floor(&p), None, "no knob => no floor");
    p.backend_knobs.insert(
      LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(),
      KnobValue::Set("16384".into()),
    );
    assert_eq!(b.admission_ctx_floor(&p), Some(16384));
  }

  #[test]
  fn readiness_fit_gate_reproduces_the_launch_condition() {
    let b = LlamaCppBackend::new();
    let seed = |p: &mut LaunchParams, floor: u32, strict: bool| {
      p.backend_knobs.insert(
        LLAMACPP_KNOB_FIT_CTX_FLOOR.to_string(),
        KnobValue::Set(floor.to_string()),
      );
      p.backend_knobs.insert(
        LLAMACPP_KNOB_STRICT_FIT.to_string(),
        KnobValue::Set(strict.to_string()),
      );
    };

    // A pinned ctx suppresses the gate entirely (fit honors the pin).
    let mut pinned = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    pinned.ctx = Some(32768);
    seed(&mut pinned, 16384, true);
    assert!(b.readiness_fit_gate(&pinned, Some(65536)).is_none());

    // Fit-delegated ctx but no trained window known => no gate.
    let mut unpinned = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    seed(&mut unpinned, 16384, true);
    assert!(b.readiness_fit_gate(&unpinned, None).is_none());

    // Fit-delegated ctx + known window => gate with the seeded floor/strict.
    let gate = b
      .readiness_fit_gate(&unpinned, Some(65536))
      .expect("gate present");
    assert_eq!(gate.floor, 16384);
    assert_eq!(gate.native, 65536);
    assert!(gate.strict);

    // strict_fit knob off flips only the strict flag.
    let mut soft = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    seed(&mut soft, 16384, false);
    let soft_gate = b.readiness_fit_gate(&soft, Some(65536)).expect("gate");
    assert!(!soft_gate.strict);
  }
}
