//! The `Backend` seam: everything currently hardwired to `llama-server`
//! on the **launch / supervise / identify** side, expressed as a
//! contract so other inference engines can plug in.
//!
//! This module + [`llama_cpp`] provide the contract with
//! llama.cpp as the reference implementation. This module is the
//! backend-agnostic foundation: a
//! second engine plugs in by implementing [`Backend`], adding a variant to
//! [`Backends`] (+ its match arms), and registering its
//! [`identity`]/selection arms — no change to the supervisor, proxy, or
//! resolver. The generic process-supervision machinery in
//! [`crate::daemon::supervisor`] (state machine, log rotation, ring buffer,
//! resource sampler, exit watcher, signal handling) is shared by every
//! backend — only the backend-specific spots live behind this seam:
//! argv/launch translation, the env strip, the readiness endpoint, and
//! identity.
//!
//! See `docs/plans/2026-06-08-001-refactor-backend-trait-abstraction-plan.md`
//! and the origin brainstorm
//! `docs/brainstorms/2026-06-08-multi-backend-abstraction-requirements.md`.
//!
//! # Two lifecycle shapes
//!
//! The contract does not assume **one process per model**. Two shapes
//! exist, one per [`Lifecycle`]:
//!
//! - **Process-per-model** ([`llama_cpp`]): llamastash spawns one
//!   `llama-server` per model and owns its full lifecycle. The launch
//!   produces a [`LaunchPlan::SpawnProcess`].
//! - **Managed-multiplexer**: a backend supervises one long-lived umbrella
//!   process and delegates per-model start/list to its API. The launch
//!   produces a [`LaunchPlan::DelegateToManager`] carrying the umbrella
//!   [`ProcessLaunchSpec`] + the model to serve. [`Backend::prepare_launch`]
//!   stays synchronous for both shapes — the async API call happens when the
//!   plan is *executed*. Lemonade ([`lemonade`]) is the first
//!   managed-multiplexer backend.
//!
//! # Generalized identity
//!
//! [`Backend::identify`] returns the seam-level [`identity::ModelIdentity`]
//! (GGUF or backend-registry), wrapping the unchanged GGUF
//! [`crate::gguf::identity::ModelId`] so `state.json` is preserved. A
//! file-less backend-registry model rides the same persisted maps as GGUF
//! rows — reusable by any future backend.

pub mod ds4;
pub mod identity;
pub mod lemonade;
pub mod llama_cpp;
pub mod server;
pub mod vllm;

pub use server::{
  build_server_catalog, config_server_catalog, missing_configured_servers, Device, Server,
  ServerConfig, ServerSpec,
};

/// The default backend id — what a plain GGUF binds to (`backend_for_identity`)
/// and the fallback for an unknown identity / persisted tag. Consumers that
/// need to recognise "the default backend" (the TUI's multi-backend column
/// gate, the `state.json` resolved-backend default, the disk-source badge) use
/// this instead of the literal, so they name no specific backend.
pub const DEFAULT_BACKEND_ID: &str = "llamacpp";

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::ds4::Ds4Backend;
use crate::backend::identity::ModelIdentity;
use crate::backend::lemonade::LemonadeBackend;
use crate::backend::llama_cpp::LlamaCppBackend;
use crate::backend::vllm::VllmBackend;
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::gguf::header::GgufHeader;
use crate::launch::flag_aliases::{knob_specs, KnobField};
use crate::launch::mode::LaunchMode;
use crate::launch::native_knobs::NativeKnobDescriptor;
use crate::launch::params::{BackendChoice, LaunchParams};

/// A speculative-decoding acceptance figure a backend reported for a launch —
/// the source for `status` `params.mtp.acceptance` / `draft_accepted` /
/// `draft_generated`.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DraftAcceptance {
  /// Fraction of drafted tokens the target model accepted (0.0..=1.0).
  pub rate: f32,
  pub accepted: u64,
  pub generated: u64,
}

/// How a backend manages the lifecycle of the models it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
  /// One supervised child process per model; llamastash owns the full
  /// lifecycle (spawn, probe, evict-by-kill). llama.cpp.
  ProcessPerModel,
  /// One long-lived supervised umbrella process; per-model start/stop
  /// /list delegated to the backend's own API. Used by managed-multiplexer
  /// backends (e.g. an NPU server).
  ManagedMultiplexer,
}

impl Lifecycle {
  /// Stable lowercase label for logs / future JSON projection.
  pub fn label(self) -> &'static str {
    match self {
      Lifecycle::ProcessPerModel => "process_per_model",
      Lifecycle::ManagedMultiplexer => "managed_multiplexer",
    }
  }
}

/// How to tell that a launched model is ready to serve.
///
/// Currently only the HTTP-poll shape (llama.cpp's `/health`). The
/// poll semantics live in [`crate::daemon::probe`]; this declares the
/// endpoint + the status that means "ready" so the probe is no longer
/// hardwired to `/health`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
  /// Poll an HTTP path until it returns `ready_status`. Any other
  /// status (including the conventional `503` "still loading") keeps
  /// the probe waiting until its timeout — matching today's behavior.
  HttpPoll { path: String, ready_status: u16 },
  /// Poll `path` until it returns `ready_status` **and** the JSON body
  /// advertises a model id in `expect_model_ids`. ds4 needs this because it
  /// leaves its reserved port *unbound* for the entire multi-minute load, so
  /// a status-only 200 could come from any process that grabbed the port
  /// meanwhile — matching the advertised alias confirms the real backend
  /// bound. Falls back to the timeout if the id never matches.
  HttpPollModelId {
    path: String,
    ready_status: u16,
    expect_model_ids: Vec<String>,
  },
}

/// The HF-credential subset stripped from a backend child's environment.
/// `HF_*` are llamastash's own pull tokens/config, which a launched inference
/// server has no reason to see — stripping them keeps the credential blast
/// radius small. This is the whole strip set ds4 needs (it reads no env
/// config); llama.cpp's [`crate::backend::llama_cpp::LLAMA_ENV_STRIP`] carries
/// the same four vars plus its `LLAMA_ARG_*` argv-override guards.
pub const CREDENTIAL_ENV_STRIP: &[&str] = &[
  "HF_TOKEN",
  "HUGGING_FACE_HUB_TOKEN",
  "HF_HOME",
  "HF_ENDPOINT",
];

/// A fully-resolved instruction for starting one model on a
/// **process-per-model** backend. Everything
/// [`crate::daemon::supervisor::spawn`] needs to launch + probe a child,
/// with no llama.cpp specifics left in the supervisor.
#[derive(Debug, Clone)]
pub struct ProcessLaunchSpec {
  /// The executable to spawn (the device-owning binary, already chosen
  /// by the orchestrator).
  pub binary: PathBuf,
  /// The full argv (everything after the program name). For llama.cpp
  /// this is exactly [`crate::backend::llama_cpp`]'s `compose` output —
  /// pinned by golden parity tests.
  pub argv: Vec<OsString>,
  /// Environment variables to remove before spawn (the loopback /
  /// credential contract: `LLAMA_ARG_*`, `HF_*`). Declared by the
  /// backend rather than hardcoded in the supervisor.
  pub env_remove: Vec<&'static str>,
  /// How to detect readiness once spawned.
  pub readiness: Readiness,
  /// Probe budget (the caller has already applied `scale_for_model`).
  pub probe: ProbeOptions,
}

/// The result of translating the resolved knob IR into "how to start
/// this model" for a given backend.
///
/// The two arms mirror the two lifecycle shapes: process-per-model
/// (llama.cpp) and managed-multiplexer. Adding the second arm
/// is additive — it does not change [`Backend::prepare_launch`]'s
/// signature, which stays synchronous and infallible (the async work
/// happens when the plan is *executed*).
#[derive(Debug, Clone)]
pub enum LaunchPlan {
  /// Spawn and supervise a child process (process-per-model shape).
  SpawnProcess(ProcessLaunchSpec),
  /// Ensure a long-lived umbrella process is up, then delegate the
  /// per-model start to its API (managed-multiplexer shape).
  DelegateToManager(ManagerLaunchSpec),
}

/// How to start one model on a **managed-multiplexer** backend: make sure
/// the umbrella process is running, then ask it (via its API) to serve a
/// named model. The umbrella is supervised by the same generic
/// [`crate::daemon::supervisor`] that runs process-per-model children — it
/// is just one long-lived child whose readiness is the backend's liveness
/// endpoint (e.g. a `/live` probe).
#[derive(Debug, Clone)]
pub struct ManagerLaunchSpec {
  /// How to ensure the umbrella process is up (spawn it once if not).
  pub umbrella: ProcessLaunchSpec,
  /// The model the umbrella should serve.
  pub model: ManagerModelRef,
}

/// A reference to a model the umbrella backend serves from its own
/// registry. Kept minimal (just the registry name) for now;
/// room to grow as the API surface is wired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerModelRef {
  /// The model name as the backend's API knows it.
  pub name: String,
}

/// A hardware accelerator class a backend can run models on.
///
/// Distinct from [`KnobCapability`] (which knob *fields* a backend honors):
/// this is which *compute targets* it can use. Surfaced by `status` so a
/// user can see, per backend, what their host can actually run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Accelerator {
  Cpu,
  Cuda,
  Rocm,
  Vulkan,
  Metal,
  Npu,
}

impl Accelerator {
  /// Stable lowercase label for JSON / status rendering.
  pub fn label(self) -> &'static str {
    match self {
      Accelerator::Cpu => "cpu",
      Accelerator::Cuda => "cuda",
      Accelerator::Rocm => "rocm",
      Accelerator::Vulkan => "vulkan",
      Accelerator::Metal => "metal",
      Accelerator::Npu => "npu",
    }
  }
}

/// The set of accelerators a backend supports on this host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceleratorSupport {
  set: BTreeSet<Accelerator>,
}

impl AcceleratorSupport {
  /// Build from an accelerator list (deduped + ordered).
  pub fn from_list<I: IntoIterator<Item = Accelerator>>(items: I) -> Self {
    Self {
      set: items.into_iter().collect(),
    }
  }

  /// Add an accelerator (idempotent).
  pub fn insert(&mut self, a: Accelerator) {
    self.set.insert(a);
  }

  pub fn contains(&self, a: Accelerator) -> bool {
    self.set.contains(&a)
  }

  /// Ordered lowercase labels (`["cpu", "npu"]`) for JSON / status.
  pub fn labels(&self) -> Vec<&'static str> {
    self.set.iter().map(|a| a.label()).collect()
  }
}

/// The set of knob IR fields a backend can honor.
///
/// llama.cpp supports every [`KnobField`]. Other backends declare a
/// subset; fields outside the set are dropped from that backend's launch
/// and surfaced as "not supported by `<backend>`" in Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnobCapability {
  supported: BTreeSet<KnobField>,
}

impl KnobCapability {
  /// Every knob the typed-knob surface defines — llama.cpp's full
  /// vocabulary, derived from the canonical [`knob_specs`] table so it
  /// can never drift from the flags `compose` actually emits.
  pub fn all() -> Self {
    Self {
      supported: knob_specs().iter().map(|s| s.field).collect(),
    }
  }

  /// No knobs honored. A managed-multiplexer backend takes a model name,
  /// not llama.cpp launch flags, so the typed-knob IR mostly doesn't apply —
  /// set knobs drop and surface as unsupported. Widen only with evidence
  /// that the backend honors a specific field.
  pub fn none() -> Self {
    Self {
      supported: BTreeSet::new(),
    }
  }

  /// Exactly `fields` honored — for backends with a narrow, evidenced
  /// surface (Lemonade honors `ctx` via `/api/v1/load`'s `ctx_size`).
  pub fn of(fields: &[KnobField]) -> Self {
    Self {
      supported: fields.iter().copied().collect(),
    }
  }

  /// Whether this backend honors `field`. A backend that honors
  /// only a subset of the IR will construct a narrower set here; the
  /// subset constructor lands with that first real consumer.
  pub fn supports(&self, field: KnobField) -> bool {
    self.supported.contains(&field)
  }
}

/// One inference backend. All behavior currently hardwired to
/// `llama-server` is expressed here so each backend owns its own
/// translation from the neutral knob IR.
///
/// The outcome of a backend resolving its **Auto** native knobs for a launch
/// with live host context. Default is no resolution.
#[derive(Debug, Default, Clone)]
pub struct NativeKnobResolution {
  /// Native-knob keys the backend auto-resolved this launch (Auto → a concrete
  /// value). Stripped from the persisted `last_params` so they re-resolve from
  /// live conditions next launch — they are not user intent.
  pub auto_set: BTreeSet<String>,
  /// Advisories to surface (e.g. "enabled disk streaming: residency won't fit").
  pub warnings: Vec<String>,
  /// Set when the resolved knob combination is one the engine rejects at
  /// startup. The launch is refused pre-spawn instead of paying a multi-minute
  /// model load for a process that exits — a backend can only detect this once
  /// its Auto knobs have resolved, so the check rides here rather than in
  /// [`Backend::refuses`], which sees only the model path.
  pub refusal: Option<String>,
}

/// Dispatch is via the [`Backends`] enum (zero-cost, exhaustive) rather than
/// `dyn Backend` — the backend set is small and closed. Because the trait is
/// only ever reached through the enum (never as a generic bound or trait
/// object), native `async fn` methods are safe here: the `async_fn_in_trait`
/// lint's auto-trait-bound concern doesn't apply.
///
/// Most methods are synchronous (pure translation / cheap config reads). The
/// few `async` ones do I/O a live backend needs at query time — a managed
/// multiplexer probing its umbrella for `status`, say — and the heavy async
/// work (spawning a process, calling a multiplexer's API) still happens when a
/// [`LaunchPlan`] is *executed*, not when it is built.
#[allow(async_fn_in_trait)]
pub trait Backend {
  /// Stable backend identifier (`"llamacpp"`). Used by the registry and
  /// any backend-aware surface.
  fn id(&self) -> &'static str;

  /// The lifecycle shape this backend uses.
  fn lifecycle(&self) -> Lifecycle;

  /// Which knob IR fields this backend honors.
  fn capabilities(&self) -> &KnobCapability;

  /// The backend's own tunables, declared **outside** the llama.cpp
  /// [`KnobField`] IR (R4) — rendered by the launch picker as native cycle
  /// /edit rows and translated to flags in [`Self::prepare_launch`] via
  /// [`crate::launch::native_knobs::translate`]. Orthogonal to
  /// [`Self::capabilities`]: a backend can honor no `KnobField` and still
  /// declare a native-knob set.
  ///
  /// Defaults to empty: llama.cpp and Lemonade surface no native knobs, so
  /// the picker + persistence are byte-identical for them. A backend opts in
  /// by overriding this with a `&'static` descriptor slice (ds4 does).
  fn native_knobs(&self) -> &'static [NativeKnobDescriptor] {
    &[]
  }

  /// Network-affecting flag heads this backend refuses in `extras` /
  /// native-knob values **on top of** the base loopback/credential denylist
  /// ([`crate::launch::params::FORBIDDEN_ADVANCED_PREFIXES`]). Default empty:
  /// llama.cpp and Lemonade add nothing. ds4 adds `--cors` / `--dist-`.
  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    &[]
  }

  /// Whether this backend serves a browser web UI the proxy's `/ui` surface
  /// can reverse-proxy. Default `false` — serving a web UI is the exception, so a
  /// new backend opts in only if it genuinely exposes one. llama.cpp overrides to
  /// `true` (its stock web UI); every other backend keeps `false`, so `/ui` never
  /// auto-pins them.
  fn serves_web_ui(&self) -> bool {
    false
  }

  /// Seed daemon-config-derived launch knobs into `params.backend_knobs`, fresh
  /// each launch (config projection, not user intent). Default: no-op. llama.cpp
  /// projects its `jinja` / `strict_fit` / `fit_ctx_floor` config here so the
  /// generic launch path carries no llama.cpp-specific launch scalars.
  fn seed_launch_knobs(&self, _ctx: &MethodContext, _params: &mut LaunchParams) {}

  /// Native knobs that must **not** be replayed from `last_params`.
  ///
  /// For a knob whose value is a judgement about *this host, right now* rather
  /// than a lasting preference. Persisting one makes a single experiment
  /// permanent: the user passes it once through a preset, and every later bare
  /// launch silently inherits it — including, for a memory knob, the launch
  /// where the auto guard would have stepped in. The knob still applies to the
  /// launch that asked for it, and still applies whenever its preset is named
  /// again; it just is not remembered on the user's behalf.
  ///
  /// Distinct from [`NativeKnobResolution::auto_set`], which drops what the
  /// *daemon* resolved. This drops what the *user* set. Default: none.
  fn volatile_native_knobs(&self) -> &'static [&'static str] {
    &[]
  }

  /// Ctx floor to project admission memory demand against when `ctx` is unpinned,
  /// or `None` to use a neutral default. Default: `None`.
  fn admission_ctx_floor(&self, _params: &LaunchParams) -> Option<u32> {
    None
  }

  /// The strict-fit ctx-clamp readiness gate for this launch, or `None` when the
  /// backend has no such gate (a pinned ctx, an unknown trained window, or a
  /// backend that doesn't delegate ctx to `--fit`). Default `None`.
  fn readiness_fit_gate(
    &self,
    _params: &LaunchParams,
    _native_ctx: Option<u32>,
  ) -> Option<crate::daemon::supervisor::FitGate> {
    None
  }

  /// Fetch this backend's post-launch actuals (what placement it resolved) from
  /// the running child, called once on the Loading → Ready transition. Default:
  /// empty — a backend that exposes no such endpoint reports nothing, and the
  /// running surfaces render "unavailable". llama.cpp overrides to read its
  /// `/props` `n_ctx`; keeping the endpoint in the backend leaves the generic
  /// supervisor / last-params paths backend-agnostic.
  async fn fetch_actuals(
    &self,
    _port: u16,
    _timeout: std::time::Duration,
  ) -> crate::daemon::actuals::Actuals {
    crate::daemon::actuals::Actuals::default()
  }

  /// Whether `extras` already configures speculative decoding by hand, in which
  /// case llamastash defers entirely and adds none of its own (KD3).
  ///
  /// Default `false`. Only the backend knows its own flag spelling — and
  /// whether a second spec flag would stack rather than replace — so the
  /// generic MTP resolution asks instead of matching an argv literal.
  fn speculation_set_in_extras(&self, _extras: &[OsString]) -> bool {
    false
  }

  /// Whether this launch actually dispatched with MTP (speculative decoding)
  /// enabled — `status` `params.mtp.active`.
  ///
  /// Default `false`: a backend with no MTP path reports honestly rather than
  /// the generic `status` path inferring "on" from the resolved directive.
  /// Backends fold that directive into their own dispatch rule differently, so
  /// only the owner knows what it launched with.
  fn mtp_active(&self, _params: &LaunchParams) -> bool {
    false
  }

  /// The latest draft-acceptance figure in `log_lines` (the recent server-log
  /// tail), or `None` when this backend publishes no such telemetry or hasn't
  /// printed one yet. Only called for a launch this backend reports as
  /// [`Backend::mtp_active`], so the log tail is never read otherwise.
  ///
  /// Default `None` — acceptance is printed in a backend's own log format, so
  /// the parse belongs with the backend.
  fn draft_acceptance(&self, _log_lines: &[String]) -> Option<DraftAcceptance> {
    None
  }

  /// The model ids a backend's `/v1/models` may advertise for one of its
  /// launches — the adoption/readiness id contract (D-adopt / D-ready).
  /// Empty (the default) means "match by the recorded file path/basename"
  /// (llama.cpp's rule, applied by the orphan sweep). ds4 returns its fixed
  /// alias set, since it never echoes the path.
  fn adoption_model_ids(&self) -> &'static [&'static str] {
    &[]
  }

  /// Whether the live process at `port` (with command line `argv`) is a
  /// re-adoptable instance of the launch recorded for `recorded_path` — the
  /// identity factor of the orphan sweep's three-factor confirmation (D-adopt).
  ///
  /// Default (llama.cpp): the `/v1/models` id matches the recorded path or its
  /// basename (`crate::daemon::orphans::models_endpoint_matches`); `argv` is
  /// unused. ds4 overrides — it echoes a fixed alias, never the path, so it
  /// cross-checks `argv`'s `-m` against `recorded_path` **and** confirms the
  /// endpoint advertises a ds4 alias. Names no backend at the call site; the
  /// sweep resolves the recorded backend tag and calls this.
  async fn adoption_matches(
    &self,
    recorded_path: &Path,
    _argv: &[String],
    port: u16,
    probe_timeout: std::time::Duration,
  ) -> bool {
    crate::daemon::orphans::models_endpoint_matches(port, recorded_path, probe_timeout).await
  }

  /// Whether this backend **auto-claims** `header` beyond the default identity
  /// rule — the header-level routing predicate (ds4's arch + quant contract).
  ///
  /// Default `false`: llama.cpp (runs every GGUF) and a registry backend
  /// (Lemonade) claim nothing *specially* here. Discovery records the first
  /// claiming backend's id as the model's `routed_backend`, and the `list`
  /// badge / launch routing read that — so a new special-routing backend needs
  /// only override this, with no discovery edit.
  /// Config-only enablement, for boot decisions taken before the full
  /// [`MethodContext`] exists (the discovery task is spawned first). Same
  /// predicate as [`Backend::available`], reading only what `DaemonOptions`
  /// already has. Default `false` — a backend opts in alongside
  /// [`Backend::projects_hf_repos`].
  fn enabled_in_config(
    &self,
    _config: &BackendConfig,
    _force: &std::collections::BTreeMap<String, bool>,
  ) -> bool {
    false
  }

  /// Whether this backend projects rows from the shared safetensors / HF-repo
  /// enumerator. Read *before* the walk so an install with no such backend
  /// enabled skips it entirely rather than walking the cache and discarding
  /// the result.
  fn projects_hf_repos(&self) -> bool {
    false
  }

  /// Catalog rows this backend contributes from the shared safetensors /
  /// HF-repo enumerator, given every non-GGUF candidate the walk found.
  ///
  /// Default: none. A backend that serves safetensors repos overrides this
  /// with its eligibility predicate plus a projection — that pair is the whole
  /// leaf, and it keeps the discovery task free of any backend name. The
  /// enumerator runs once per rescan and the candidates are shared, so a
  /// second safetensors engine costs one more override, not another walk.
  fn project_hf_repos(
    &self,
    _candidates: &[crate::discovery::hf_repos::HfRepoCandidate],
  ) -> Vec<crate::discovery::DiscoveredModel> {
    Vec::new()
  }

  fn auto_routes(&self, _header: &GgufHeader) -> bool {
    false
  }

  /// Whether this backend serves `mode`. Default `true` — serves chat /
  /// embedding / rerank alike. A backend that serves only some modes overrides
  /// this; an `Auto` launch in an unserved mode falls back to the identity
  /// default (so e.g. embeddings route to the generic backend), a routing
  /// input, not an error.
  fn serves_mode(&self, _mode: LaunchMode) -> bool {
    true
  }

  /// A pre-spawn refusal for a model this backend recognizes but cannot launch,
  /// or `None` to proceed.
  ///
  /// Checked on an `Auto` launch across every backend (see
  /// [`refusal_for_auto_launch`]) so a header a backend understands but can't
  /// load is refused before the expensive spawn; an explicit `--backend`
  /// override skips the check so the engine can surface its own error. Default
  /// `None`. `arch` is the resolved `general.architecture`.
  fn refuses(&self, _arch: Option<&str>, _path: &Path) -> Option<String> {
    None
  }

  /// Whether this backend is **available** (installed + enabled) on this host.
  ///
  /// Read by launch selection (an auto-routing backend must be available to
  /// win over the identity default), `status` (the `installed` signal), and the
  /// routing guards. Default `true` — a backend with no external binary /
  /// enablement gate. Real backends override to resolve their binary + config
  /// enablement from `ctx`; keeping that logic in the backend's own file is what
  /// stops the daemon from hard-coding per-backend availability checks.
  fn available(&self, _ctx: &MethodContext) -> bool {
    true
  }

  /// Enumerate this backend's configured **servers** (build/binary variants),
  /// resolved to absolute paths, from its own `servers:` config. The boot
  /// [`build_server_catalog`] probes each for devices and derives an id/name.
  /// Default: no servers — a backend contributes to the catalog only if it
  /// resolves at least one binary.
  fn configured_servers(&self, _ctx: &MethodContext) -> Vec<ServerSpec> {
    Vec::new()
  }

  /// This backend's configured servers read from [`crate::config::Config`]
  /// **alone** (no daemon context) — the view read-only `doctor` needs. Returns
  /// the raw
  /// `backend.<id>.servers` entries; unlike [`Self::configured_servers`] it adds
  /// no daemon-resolved PATH primary, reporting exactly what config declares.
  /// Default: none.
  fn config_servers(&self, _config: &crate::config::Config) -> Vec<ServerConfig> {
    Vec::new()
  }

  /// Probe one server binary for the GPU **devices** it can target (the exact
  /// `--device` selectors it accepts). Default empty — a backend with no
  /// device-selection surface (ds4 / lemonade). llama.cpp overrides with its
  /// `--list-devices` probe.
  fn probe_devices(&self, _binary: &Path) -> Vec<Device> {
    Vec::new()
  }

  /// Default-ordering weight among the servers a model supports (higher first).
  /// Orders both the launch **server** knob and `supported_backends`, and picks
  /// the no-selection default. Default `0`; a purpose-built backend that should
  /// win the auto-route (ds4 over llama.cpp) returns a higher value.
  fn launch_priority(&self) -> i32 {
    0
  }

  /// Resolve the executable + port this launch spawns on, given the
  /// orchestrator's default (device-owning) binary and the reserved pool port.
  ///
  /// Default returns them unchanged — a process-per-model backend that uses the
  /// device binary on the pool port. A backend with its own server binary (or a
  /// managed multiplexer whose umbrella binds a configured port) overrides.
  /// `Err(msg)` is a user-facing "binary not found" message; the launch is
  /// refused and the reserved port released. Keeps the orchestrator's spawn arm
  /// backend-agnostic.
  fn resolve_launch_binary(
    &self,
    _ctx: &MethodContext,
    default_binary: PathBuf,
    port: u16,
  ) -> Result<(PathBuf, u16), String> {
    Ok((default_binary, port))
  }

  /// Resolve this backend's **Auto** native knobs for a launch given live host
  /// context, mutating `params.backend_knobs` in place (`weights_bytes` is the
  /// shard-aware model size). Default: no-op — an Auto native knob stays unset.
  ///
  /// This is the uniform auto-behavior for native knobs: a knob the user left
  /// unset/`Auto` resolves here from live conditions (a backend that disk-streams
  /// when residency won't fit enables that knob), while an explicit user value is
  /// left untouched. Returns the auto-resolved keys (stripped from persistence)
  /// and any advisory. Keeps per-backend admission/knob logic out of the generic
  /// launch path.
  async fn resolve_native_knobs(
    &self,
    _ctx: &MethodContext,
    _params: &mut LaunchParams,
    _weights_bytes: u64,
  ) -> NativeKnobResolution {
    NativeKnobResolution::default()
  }

  /// Bytes this launch will hold beyond its weights, for a model with no GGUF
  /// header to project demand from.
  ///
  /// Only the backend can answer: the figure lives in its own native knobs,
  /// and the knobs are its own vocabulary. Called with post-headroom free
  /// bytes so a backend expressing its budget as a *fraction of the pool* can
  /// turn that into bytes. `None` means "no projection possible", which leaves
  /// the admission gate disengaged — return a figure wherever one can be
  /// derived, because a silent skip is how an oversized launch reaches spawn.
  ///
  /// Default `None`: a GGUF backend projects from its header instead and never
  /// reaches this path.
  fn projected_cache_bytes(&self, _params: &LaunchParams, _free_bytes: u64) -> Option<u64> {
    None
  }

  /// Whether this launch bypasses the pre-spawn memory admission gate — a
  /// backend that streams weights from disk (bounded residency) skips the hard
  /// OOM refusal. Default `false`. Read on the *resolved* params (after
  /// [`Self::resolve_native_knobs`]).
  fn bypasses_admission(&self, _params: &LaunchParams) -> bool {
    false
  }

  /// Whether the backend's executable is present on this host (the `status`
  /// `installed` signal), independent of the enablement toggle. Default
  /// [`Self::available`]; a backend with a separate enablement config overrides
  /// to report presence alone.
  fn installed(&self, ctx: &MethodContext) -> bool {
    self.available(ctx)
  }

  /// The `status` `enabled` flag: `Some(_)` for a backend with an enablement
  /// toggle, `None` for one with none (so `status` omits the field). Default
  /// `None`.
  fn status_enabled(&self, _ctx: &MethodContext) -> Option<bool> {
    None
  }

  /// The resolved executable path to surface in `status`, or `None` when none
  /// resolves. Default `None`; a backend with a binary overrides.
  fn binary_path(&self, _ctx: &MethodContext) -> Option<String> {
    None
  }

  /// The accelerator-class labels for `status`, unioning the backend's static
  /// floor with the host's live device classes. Default does exactly that; a
  /// backend that can probe its *installed* accelerators live overrides.
  async fn status_accelerators(&self, _ctx: &MethodContext, device: &[Accelerator]) -> Vec<String> {
    let mut acc = self.accelerators();
    for a in device {
      acc.insert(*a);
    }
    acc.labels().into_iter().map(str::to_string).collect()
  }

  /// Extra `status` row fields beyond id / lifecycle / installed / enabled /
  /// accelerators / binary — e.g. a managed-multiplexer's umbrella state.
  /// Default none.
  async fn status_extra(&self, _ctx: &MethodContext) -> Vec<(String, serde_json::Value)> {
    Vec::new()
  }

  /// The `doctor` advisories this backend contributes, given the resolved
  /// [`Config`](crate::config::Config). Default: none. `doctor` collects across
  /// [`Backends::all`] so its check flow names no backend; each finding carries
  /// a stable string id (kept additive, so `schema_version` never bumps for a
  /// new backend). A backend with host-specific diagnostics (ds4's "compatible
  /// model present but the engine is unavailable") overrides this and builds its
  /// findings via [`Finding::from_parts`](crate::init::doctor::Finding::from_parts).
  /// `config`-only (not `ctx`) because `doctor` runs CLI-side with no
  /// [`MethodContext`]; a backend reads its own sub-config + does its own scan.
  async fn doctor_findings(
    &self,
    _config: &crate::config::Config,
  ) -> Vec<crate::init::doctor::Finding> {
    Vec::new()
  }

  /// The [`LaunchId`](crate::daemon::registry::LaunchId) of this backend's
  /// long-lived infrastructure process (a managed-multiplexer umbrella), or
  /// `None` for a process-per-model backend. Generic walkers that iterate
  /// running launches (proxy routing, `/ui`, eviction, `status`) skip infra
  /// launches via [`is_infra_launch`] rather than hard-coding the umbrella id.
  fn umbrella_launch_id(&self) -> Option<crate::daemon::registry::LaunchId> {
    None
  }

  /// The OpenAI path prefix a managed-multiplexer umbrella serves its `/v1/...`
  /// surface behind (e.g. `Some("/api")` → the umbrella answers `/api/v1/...`),
  /// or `None` for a backend that serves `/v1/...` directly. Read by the proxy's
  /// umbrella-routing arm so it forwards under the right prefix without naming a
  /// backend. Default `None`.
  fn umbrella_openai_prefix(&self) -> Option<&'static str> {
    None
  }

  /// Bring up any always-on infrastructure this backend supervises at daemon
  /// **boot** — a managed multiplexer's shared umbrella process, so discovery
  /// and proxy routing work before the user issues an explicit `start`. Called
  /// once per registered backend after the dispatch context is wired, before
  /// the proxy listener binds. Default: nothing (a process-per-model backend
  /// supervises only per `start_model`).
  ///
  /// The backend **self-gates** on its own [`available`](Self::available)
  /// predicate and must return promptly: bring the process up in a detached
  /// background task rather than blocking boot on a readiness probe (the
  /// detached-start parent only waits a few seconds for `runtime.json`).
  /// `log_dir` is the daemon's per-launch log directory; `probe_timeout` is the
  /// configured readiness deadline (`None` = the backend's default).
  fn supervise_at_boot(
    &self,
    _ctx: &MethodContext,
    _log_dir: &Path,
    _probe_timeout: Option<std::time::Duration>,
  ) {
  }

  /// Mint this backend's synthetic [`ModelIdentity`] for a **file-less** catalog
  /// `path` — a backend that names models from a remote registry rather than a
  /// local GGUF — or `None` when `path` is not one of its synthetic paths.
  /// Default `None`: a process-per-model / GGUF backend has no synthetic
  /// identity. Lets the generic launch / status path mint a file-less row's
  /// identity from just a path, without naming a backend (see
  /// [`synthetic_identity_for_path`]).
  fn synthetic_identity(&self, _path: &Path) -> Option<ModelIdentity> {
    None
  }

  /// Every basename this backend's server binary can carry — the names the
  /// `$PATH` locator searches for and the orphan sweep recognises an
  /// *unmanaged* instance by. The **first** entry is the primary marker (the
  /// name used in messages and for an adopted child); the rest are alternate
  /// distributions of the same server. Empty for a backend with no standalone
  /// per-model server process (a managed multiplexer's umbrella is supervised,
  /// not swept). Default empty.
  fn process_markers(&self) -> &'static [&'static str] {
    &[]
  }

  /// A backend-specific KV-cache byte model for `header`, or `None` to use the
  /// generic GQA/MLA estimate.
  ///
  /// Keyed on the **header** (arch + shape), not on which backend actually runs
  /// the model: KV geometry is a property of the weights, so
  /// [`crate::gguf::memory::kv_bytes`] consults every backend's override and a
  /// `deepseek4` GGUF gets ds4's compressed-cache figure even when it falls
  /// back to llama.cpp. Default `None` — llama.cpp / Lemonade use the generic
  /// path. `arch` is the resolved `general.architecture` the estimator keys on
  /// (passed alongside the header so the gate matches the pre-seam behavior
  /// exactly, independent of what the header's own arch key says).
  fn kv_bytes(&self, _header: &GgufHeader, _arch: Option<&str>, _ctx_len: u64) -> Option<u64> {
    None
  }

  /// The accelerator classes this backend can run models on.
  ///
  /// A *static, backend-intrinsic* floor — llama.cpp always runs CPU (GPU
  /// targets are build-/host-specific and surfaced separately via the live
  /// device catalog); a managed-multiplexer backend might declare CPU + NPU.
  /// The `status` backends view unions this with host-derived signals (e.g.
  /// the llama.cpp device catalog) for the full per-host picture.
  fn accelerators(&self) -> AcceleratorSupport;

  /// Compute the stable identity for a model handled by this backend.
  ///
  /// Returns the generalized [`ModelIdentity`]: llama.cpp wraps the
  /// concrete `(path, BLAKE3)` GGUF identity in
  /// [`ModelIdentity::Gguf`]; a managed-registry backend returns
  /// [`ModelIdentity::Backend`]. The `(path, header_bytes)` inputs are
  /// the GGUF-discovery shape — registry backends ignore them for now and
  /// will derive identity from their API when such a backend lands (see
  /// [`identity`]).
  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity;

  /// Translate a fully-resolved [`LaunchParams`] into a [`LaunchPlan`]
  /// Pure and infallible for llama.cpp — `compose` cannot fail.
  ///
  /// `binary` is the device-owning executable the orchestrator already
  /// selected; `probe` carries the size-scaled budget.
  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan;

  /// Execute a launch, returning the observable `StartedLaunch` handle (or a
  /// JSON-RPC error). **The backend owns the lifecycle**: the default spawns a
  /// supervised child process (admission-gated) via `spawn_supervised`; a
  /// managed-multiplexer backend overrides this to ensure its umbrella +
  /// delegate the model. The caller (`compose_and_spawn`) hands over the prepared
  /// [`LaunchExec`](crate::daemon::launch_service::LaunchExec) and never branches
  /// on lifecycle.
  async fn start(
    &self,
    ctx: &MethodContext,
    exec: crate::daemon::launch_service::LaunchExec,
  ) -> Result<crate::daemon::launch_service::StartedLaunch, crate::ipc::protocol::ErrorObject> {
    // Default: a supervised child process. Resolve the executable + port (the
    // pool port for a process-per-model backend), compose the argv, then run the
    // generic admission-gated spawn.
    let (binary, port) =
      match self.resolve_launch_binary(ctx, exec.default_binary.clone(), exec.reserved_port) {
        Ok(bp) => bp,
        Err(msg) => {
          ctx
            .supervisors
            .release_reserved_port(exec.reserved_port)
            .await;
          return Err(crate::ipc::protocol::ErrorObject::new(
            crate::ipc::protocol::ErrorCode::InvalidParams,
            msg,
          ));
        }
      };
    let spec = match self.prepare_launch(&exec.params, port, binary, exec.probe) {
      LaunchPlan::SpawnProcess(s) => s,
      LaunchPlan::DelegateToManager(_) => {
        unreachable!("a managed-multiplexer backend must override start()")
      }
    };
    // Backend-owned launch inputs the generic supervised spawn needs but must
    // not derive itself: the admission ctx floor and the strict-fit readiness
    // gate. Both default to `None` (a backend with no such concept), so the
    // spawn path names no backend.
    let admission_floor = self.admission_ctx_floor(&exec.params);
    let fit_gate = self.readiness_fit_gate(&exec.params, exec.native_ctx);
    crate::daemon::launch_service::spawn_supervised(ctx, exec, spec, admission_floor, fit_gate)
      .await
  }

  /// Stop a launch this backend owns, returning the `{launch_id, state}` response
  /// (or a JSON-RPC error). **The backend owns the lifecycle**, mirroring
  /// [`start`](Backend::start): the default SIGTERMs the supervised child
  /// (`stop_supervised`, `spawn_supervised`'s counterpart); a managed multiplexer
  /// overrides this to unload the model from its umbrella (or tear the umbrella
  /// down). The caller resolves the owning backend (`backend_for_launch`) and
  /// calls `stop`; it never branches on process-vs-umbrella.
  async fn stop(
    &self,
    ctx: &MethodContext,
    launch_id: &crate::daemon::registry::LaunchId,
    grace_secs: u64,
  ) -> Result<serde_json::Value, crate::ipc::protocol::ErrorObject> {
    crate::daemon::launch_service::stop_supervised(ctx, launch_id, grace_secs).await
  }
}

// ============================================================================
// BACKEND REGISTRY — the only place (besides a backend's own module) that names
// a specific backend.
// ============================================================================

/// All backend configuration, grouped under the `backend:` map in
/// `config.yaml`. Each backend owns its own typed config struct in its own
/// module; this is the single aggregation point the top-level [`crate::config::Config`]
/// carries. llama.cpp is the always-on default backend, so it has no `enabled`
/// field — only the optional engines do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct BackendConfig {
  pub llamacpp: crate::backend::llama_cpp::LlamaCppConfig,
  pub lemonade: crate::backend::lemonade::LemonadeConfig,
  pub ds4: crate::backend::ds4::Ds4Config,
  pub vllm: crate::backend::vllm::VllmConfig,
}

/// Zero-cost, exhaustive dispatch over the available backends.
///
/// `dyn Backend` is deliberately avoided — the backend set is small and
/// closed, so an enum gives static dispatch and forces every new backend
/// to be handled at every call site. The compiler flags every `match` that
/// needs a newly-added variant.
#[derive(Debug, Clone)]
pub enum Backends {
  /// Direct, zero-overhead llama.cpp (process-per-model).
  LlamaCpp(LlamaCppBackend),
  /// Lemonade (`lemond`) managed-multiplexer — one umbrella, many models.
  Lemonade(LemonadeBackend),
  /// ds4 (DwarfStar) — direct process-per-model for DeepSeek V4 GGUFs.
  Ds4(Ds4Backend),
  /// vLLM — direct process-per-model for safetensors HF repos.
  Vllm(VllmBackend),
}

/// Forward a [`Backend`] call to whichever [`Backends`] variant is active.
///
/// The variant list lives here **once**. Every `Backend` method on `Backends`
/// delegates through this macro, so adding a backend is: a new variant on the
/// enum, one arm here, and one line in [`Backends::all`] — the ~dozen method
/// bodies never change. `$body` may `.await`: the enum is native static
/// dispatch, so async backend methods forward with no boxing (the reason the
/// contract stays an enum rather than `dyn Backend`).
macro_rules! for_each_backend {
  ($self:expr, $b:ident => $body:expr) => {
    match $self {
      Backends::LlamaCpp($b) => $body,
      Backends::Lemonade($b) => $body,
      Backends::Ds4($b) => $body,
      Backends::Vllm($b) => $body,
    }
  };
}

impl Backends {
  /// Every backend llamastash knows about, freshly constructed — the single
  /// enumeration point (R3).
  ///
  /// Consumers that need to survey all backends (the `status` backend rows,
  /// `doctor` advisories, the `--backend` value set, discovery routing) walk
  /// this instead of hand-listing variants, so a new backend surfaces
  /// everywhere from one registration.
  pub fn all() -> Vec<Backends> {
    vec![
      Backends::LlamaCpp(LlamaCppBackend::new()),
      Backends::Lemonade(LemonadeBackend::new()),
      Backends::Ds4(Ds4Backend::new()),
      Backends::Vllm(VllmBackend::new()),
    ]
  }
}

/// The id of the backend that **auto-claims** `header` (the first registry
/// entry whose [`Backend::auto_routes`] matches), or `None` when no backend
/// does. The single generic routing-predicate entry point: launch routing reads
/// it, so a new special-routing backend surfaces from its `auto_routes` override
/// alone — no call site names a backend.
pub fn routed_backend_for(header: &GgufHeader) -> Option<String> {
  Backends::all()
    .into_iter()
    .find(|b| b.auto_routes(header))
    .map(|b| b.id().to_string())
}

/// Every backend that can serve a disk GGUF with `header`, **priority-ordered**
/// (highest [`Backend::launch_priority`] first, ties broken by registration
/// order). The first entry is the auto-route default. A backend is included when
/// it `auto_routes` the header (special routing, e.g. ds4 for a compatible
/// DeepSeek-V4) **or** it is the identity-default backend for a plain GGUF
/// ([`DEFAULT_BACKEND_ID`], always able to run a local file). Discovery records
/// this per model; the `list` badge / right-pane badges show all of them, and
/// launch routing prefers the first available one. Names no backend beyond the
/// identity default.
pub fn supported_backends_for(header: &GgufHeader) -> Vec<String> {
  let mut backends: Vec<Backends> = Backends::all()
    .into_iter()
    .filter(|b| b.auto_routes(header) || b.id() == DEFAULT_BACKEND_ID)
    .collect();
  // Stable sort by priority descending — ds4 (20) before llamacpp (10).
  backends.sort_by_key(|b| std::cmp::Reverse(b.launch_priority()));
  backends.into_iter().map(|b| b.id().to_string()).collect()
}

impl Backend for Backends {
  fn id(&self) -> &'static str {
    for_each_backend!(self, b => b.id())
  }

  fn lifecycle(&self) -> Lifecycle {
    for_each_backend!(self, b => b.lifecycle())
  }

  fn capabilities(&self) -> &KnobCapability {
    for_each_backend!(self, b => b.capabilities())
  }

  fn native_knobs(&self) -> &'static [NativeKnobDescriptor] {
    for_each_backend!(self, b => b.native_knobs())
  }

  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    for_each_backend!(self, b => b.forbidden_extra_heads())
  }

  fn volatile_native_knobs(&self) -> &'static [&'static str] {
    for_each_backend!(self, b => b.volatile_native_knobs())
  }

  fn serves_web_ui(&self) -> bool {
    for_each_backend!(self, b => b.serves_web_ui())
  }

  fn seed_launch_knobs(&self, ctx: &MethodContext, params: &mut LaunchParams) {
    for_each_backend!(self, b => b.seed_launch_knobs(ctx, params))
  }

  fn admission_ctx_floor(&self, params: &LaunchParams) -> Option<u32> {
    for_each_backend!(self, b => b.admission_ctx_floor(params))
  }

  fn readiness_fit_gate(
    &self,
    params: &LaunchParams,
    native_ctx: Option<u32>,
  ) -> Option<crate::daemon::supervisor::FitGate> {
    for_each_backend!(self, b => b.readiness_fit_gate(params, native_ctx))
  }

  async fn fetch_actuals(
    &self,
    port: u16,
    timeout: std::time::Duration,
  ) -> crate::daemon::actuals::Actuals {
    for_each_backend!(self, b => b.fetch_actuals(port, timeout).await)
  }

  fn speculation_set_in_extras(&self, extras: &[OsString]) -> bool {
    for_each_backend!(self, b => b.speculation_set_in_extras(extras))
  }

  fn mtp_active(&self, params: &LaunchParams) -> bool {
    for_each_backend!(self, b => b.mtp_active(params))
  }

  fn draft_acceptance(&self, log_lines: &[String]) -> Option<DraftAcceptance> {
    for_each_backend!(self, b => b.draft_acceptance(log_lines))
  }

  fn adoption_model_ids(&self) -> &'static [&'static str] {
    for_each_backend!(self, b => b.adoption_model_ids())
  }

  async fn adoption_matches(
    &self,
    recorded_path: &Path,
    argv: &[String],
    port: u16,
    probe_timeout: std::time::Duration,
  ) -> bool {
    for_each_backend!(self, b => b.adoption_matches(recorded_path, argv, port, probe_timeout).await)
  }

  fn kv_bytes(&self, header: &GgufHeader, arch: Option<&str>, ctx_len: u64) -> Option<u64> {
    for_each_backend!(self, b => b.kv_bytes(header, arch, ctx_len))
  }

  fn auto_routes(&self, header: &GgufHeader) -> bool {
    for_each_backend!(self, b => b.auto_routes(header))
  }

  fn enabled_in_config(
    &self,
    config: &BackendConfig,
    force: &std::collections::BTreeMap<String, bool>,
  ) -> bool {
    for_each_backend!(self, b => b.enabled_in_config(config, force))
  }

  fn projects_hf_repos(&self) -> bool {
    for_each_backend!(self, b => b.projects_hf_repos())
  }

  fn project_hf_repos(
    &self,
    candidates: &[crate::discovery::hf_repos::HfRepoCandidate],
  ) -> Vec<crate::discovery::DiscoveredModel> {
    for_each_backend!(self, b => b.project_hf_repos(candidates))
  }

  fn serves_mode(&self, mode: LaunchMode) -> bool {
    for_each_backend!(self, b => b.serves_mode(mode))
  }

  fn refuses(&self, arch: Option<&str>, path: &Path) -> Option<String> {
    for_each_backend!(self, b => b.refuses(arch, path))
  }

  fn available(&self, ctx: &MethodContext) -> bool {
    for_each_backend!(self, b => b.available(ctx))
  }

  fn configured_servers(&self, ctx: &MethodContext) -> Vec<ServerSpec> {
    for_each_backend!(self, b => b.configured_servers(ctx))
  }

  fn config_servers(&self, config: &crate::config::Config) -> Vec<ServerConfig> {
    for_each_backend!(self, b => b.config_servers(config))
  }

  fn probe_devices(&self, binary: &Path) -> Vec<Device> {
    for_each_backend!(self, b => b.probe_devices(binary))
  }

  fn launch_priority(&self) -> i32 {
    for_each_backend!(self, b => b.launch_priority())
  }

  fn resolve_launch_binary(
    &self,
    ctx: &MethodContext,
    default_binary: PathBuf,
    port: u16,
  ) -> Result<(PathBuf, u16), String> {
    for_each_backend!(self, b => b.resolve_launch_binary(ctx, default_binary, port))
  }

  async fn resolve_native_knobs(
    &self,
    ctx: &MethodContext,
    params: &mut LaunchParams,
    weights_bytes: u64,
  ) -> NativeKnobResolution {
    for_each_backend!(self, b => b.resolve_native_knobs(ctx, params, weights_bytes).await)
  }

  fn bypasses_admission(&self, params: &LaunchParams) -> bool {
    for_each_backend!(self, b => b.bypasses_admission(params))
  }

  fn projected_cache_bytes(&self, params: &LaunchParams, free_bytes: u64) -> Option<u64> {
    for_each_backend!(self, b => b.projected_cache_bytes(params, free_bytes))
  }

  fn installed(&self, ctx: &MethodContext) -> bool {
    for_each_backend!(self, b => b.installed(ctx))
  }

  fn status_enabled(&self, ctx: &MethodContext) -> Option<bool> {
    for_each_backend!(self, b => b.status_enabled(ctx))
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    for_each_backend!(self, b => b.binary_path(ctx))
  }

  async fn status_accelerators(&self, ctx: &MethodContext, device: &[Accelerator]) -> Vec<String> {
    for_each_backend!(self, b => b.status_accelerators(ctx, device).await)
  }

  async fn status_extra(&self, ctx: &MethodContext) -> Vec<(String, serde_json::Value)> {
    for_each_backend!(self, b => b.status_extra(ctx).await)
  }

  async fn doctor_findings(
    &self,
    config: &crate::config::Config,
  ) -> Vec<crate::init::doctor::Finding> {
    for_each_backend!(self, b => b.doctor_findings(config).await)
  }

  fn umbrella_launch_id(&self) -> Option<crate::daemon::registry::LaunchId> {
    for_each_backend!(self, b => b.umbrella_launch_id())
  }

  fn umbrella_openai_prefix(&self) -> Option<&'static str> {
    for_each_backend!(self, b => b.umbrella_openai_prefix())
  }

  fn supervise_at_boot(
    &self,
    ctx: &MethodContext,
    log_dir: &Path,
    probe_timeout: Option<std::time::Duration>,
  ) {
    for_each_backend!(self, b => b.supervise_at_boot(ctx, log_dir, probe_timeout))
  }

  fn synthetic_identity(&self, path: &Path) -> Option<ModelIdentity> {
    for_each_backend!(self, b => b.synthetic_identity(path))
  }

  fn process_markers(&self) -> &'static [&'static str] {
    for_each_backend!(self, b => b.process_markers())
  }

  fn accelerators(&self) -> AcceleratorSupport {
    for_each_backend!(self, b => b.accelerators())
  }

  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity {
    for_each_backend!(self, b => b.identify(path, header_bytes))
  }

  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan {
    for_each_backend!(self, b => b.prepare_launch(params, port, binary, probe))
  }

  async fn start(
    &self,
    ctx: &MethodContext,
    exec: crate::daemon::launch_service::LaunchExec,
  ) -> Result<crate::daemon::launch_service::StartedLaunch, crate::ipc::protocol::ErrorObject> {
    for_each_backend!(self, b => b.start(ctx, exec).await)
  }

  async fn stop(
    &self,
    ctx: &MethodContext,
    launch_id: &crate::daemon::registry::LaunchId,
    grace_secs: u64,
  ) -> Result<serde_json::Value, crate::ipc::protocol::ErrorObject> {
    for_each_backend!(self, b => b.stop(ctx, launch_id, grace_secs).await)
  }
}

/// Map a model's [`ModelIdentity`] to the backend that runs it.
///
/// The identity-keyed rule (the **auto** half of R17): a GGUF identity binds to
/// the direct llama.cpp backend; a backend-registry identity binds to the
/// registry backend whose [`Backend::id`] matches (found generically over
/// [`Backends::all`], so no backend is named here). An unknown registry id
/// falls back to the safe direct path.
pub fn backend_for_identity(identity: &ModelIdentity) -> Backends {
  match identity {
    ModelIdentity::Gguf(_) => Backends::LlamaCpp(LlamaCppBackend::new()),
    ModelIdentity::Backend(id) => Backends::all()
      .into_iter()
      .find(|b| b.id() == id.backend)
      .unwrap_or_else(|| Backends::LlamaCpp(LlamaCppBackend::new())),
  }
}

/// Resolve the backend for a launch, honoring a per-model override.
///
/// An explicit [`BackendChoice`] wins; [`BackendChoice::Auto`] defers to the
/// [`backend_for_identity`] rule. The single entry point the live launch path
/// uses, so override and auto rule can never diverge across surfaces.
pub fn resolve_backend(identity: &ModelIdentity, choice: BackendChoice) -> Backends {
  match choice {
    BackendChoice::Auto => backend_for_identity(identity),
    // Force the named backend from the registry; an unknown id (shouldn't reach
    // here — the CLI/IPC boundary validates) falls back to the identity rule.
    BackendChoice::Explicit(id) => Backends::all()
      .into_iter()
      .find(|b| b.id() == id)
      .unwrap_or_else(|| backend_for_identity(identity)),
  }
}

/// The first backend refusal for an `Auto` launch of `path` (architecture
/// `arch`), or `None` to proceed.
///
/// Iterates the registry so any backend can decline a header it recognizes but
/// cannot load (e.g. an unloadable split file) *before* the expensive spawn.
/// The launch path calls this only on `Auto`; an explicit `--backend` override
/// skips it so the chosen engine can surface its own error. Names no backend.
pub fn refusal_for_auto_launch(arch: Option<&str>, path: &Path) -> Option<String> {
  Backends::all()
    .into_iter()
    .find_map(|b| b.refuses(arch, path))
}

/// Whether `id` is a backend's long-lived **infrastructure** launch (a
/// managed-multiplexer umbrella), not a servable model.
///
/// The generic replacement for the hard-coded `if launch_id == umbrella_id`
/// skip that every running-launch walker (proxy routing, `/ui`, eviction,
/// `status`) applies: an umbrella process is supervised like any child but
/// isn't itself a model a client can attach to. Names no backend.
pub fn is_infra_launch(id: &crate::daemon::registry::LaunchId) -> bool {
  umbrella_owner(id).is_some()
}

/// The managed-multiplexer backend that owns the umbrella at `id`, or `None`
/// when `id` isn't an umbrella. Idle eviction resolves the owner this way to
/// `stop` the delegated models it serves (which unloads them from the umbrella),
/// without naming a backend.
pub fn umbrella_owner(id: &crate::daemon::registry::LaunchId) -> Option<Backends> {
  Backends::all()
    .into_iter()
    .find(|b| b.umbrella_launch_id().as_ref() == Some(id))
}

/// Mint the synthetic [`ModelIdentity`] for a **file-less** catalog `path`, plus
/// the id of the backend that owns it. The generic replacement for hand-minting
/// a backend-registry identity in the launch / status path: returns the first
/// backend whose [`Backend::synthetic_identity`] claims `path` (paired with that
/// backend's [`Backend::id`], which callers stamp as `resolved_backend`), or
/// `None` for a local-file (GGUF) path no backend synthesizes. Names no backend.
pub fn synthetic_identity_for_path(path: &Path) -> Option<(ModelIdentity, String)> {
  Backends::all().into_iter().find_map(|b| {
    b.synthetic_identity(path)
      .map(|id| (id, b.id().to_string()))
  })
}

/// [`synthetic_identity_for_path`] restricted to backends that are actually
/// available on this host.
///
/// The ungated form has no `ctx` and so cannot consult config: a disabled
/// backend still claimed a path shaped like one of its models, and the launch
/// then failed with an error naming a backend the user had switched off. That
/// contradicts the "an absent or disabled backend changes nothing" property
/// the daemon and discovery task both document. Callers holding a context use
/// this; the ungated form remains for clients that have none.
pub fn synthetic_identity_for_path_available(
  path: &Path,
  ctx: &MethodContext,
) -> Option<(ModelIdentity, String)> {
  Backends::all()
    .into_iter()
    .filter(|b| b.available(ctx))
    .find_map(|b| {
      b.synthetic_identity(path)
        .map(|id| (id, b.id().to_string()))
    })
}

/// Everything a surface needs to identify a model path, whichever shape the
/// model is: a local GGUF file, or a directory / registry entry a backend
/// claims. Produced by [`resolve_identity_for_path`].
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
  /// The file-keyed handle: last-params lookups, log-file names, the proxy's
  /// coalesce + failure-tracker keys, MRU, running-snapshot retention. A
  /// backend-claimed path carries the sentinel zero hash — this is a key, not
  /// a digest, and the zero marks it as not-a-GGUF.
  pub id: crate::gguf::identity::ModelId,
  /// The identity that gets **persisted**: `Gguf` for a local file, `Backend`
  /// for a claimed path.
  pub identity: ModelIdentity,
  /// The backend that claimed the path, when one did. Callers stamp it as
  /// `resolved_backend`.
  pub claimed_by: Option<String>,
  /// Header-derived, and therefore `None` for a claimed path — the backend
  /// owns that recipe, not us.
  pub arch: Option<String>,
  pub native_ctx: Option<u32>,
  pub supported_backends: Vec<String>,
  pub mtp_embedded: Option<u32>,
}

/// Why [`resolve_identity_for_path`] could not identify a path.
#[derive(Debug, Clone)]
pub enum IdentityError {
  /// A backend recognizes this path, but it is disabled or its launcher is
  /// absent. Distinct from `Header` because the fix is different — the user
  /// enables a backend rather than repairing a file — and because falling
  /// through to the header reader hands a directory to the GGUF parser and
  /// surfaces a raw `EISDIR` instead of a reason.
  BackendUnavailable { backend: String },
  /// A local-file path whose GGUF header could not be read.
  Header(String),
}

impl std::fmt::Display for IdentityError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::BackendUnavailable { backend } => write!(
        f,
        "is served by the `{backend}` backend, which is not available — \
         enable it in config or install its launcher"
      ),
      Self::Header(msg) => write!(f, "{msg}"),
    }
  }
}

/// **The** path → identity resolution. Every surface that turns a model path
/// into an id goes through here: launch, proxy auto-start, favorites, presets.
///
/// Each of those used to do its own thing. Three read a GGUF header directly
/// and one special-cased a single backend by name, so support for a
/// directory-shaped model had to be added four times and was in fact added
/// twice — the proxy's auto-start died with `Is a directory (os error 21)` on
/// exactly the models the launch path had already learned to handle. A new
/// model shape is now one predicate ([`Backend::synthetic_identity`]) and every
/// surface follows.
///
/// `ctx` gates on backend availability when a caller has one; clients without
/// a context (and tests) pass `None` and get the ungated registry answer.
pub fn resolve_identity_for_path(
  path: &Path,
  ctx: Option<&MethodContext>,
) -> Result<ResolvedIdentity, IdentityError> {
  let claimed = match ctx {
    Some(c) => synthetic_identity_for_path_available(path, c),
    None => synthetic_identity_for_path(path),
  };
  if let Some((identity, backend_id)) = claimed {
    return Ok(ResolvedIdentity {
      id: crate::gguf::identity::ModelId {
        path: path.to_path_buf(),
        header_blake3: [0u8; 32],
      },
      identity,
      claimed_by: Some(backend_id),
      arch: None,
      native_ctx: None,
      supported_backends: Vec::new(),
      mtp_embedded: None,
    });
  }
  // Claimed, but by a backend that is off or absent. Only reachable with a
  // context — the ungated call above already accepted every claim.
  if ctx.is_some() {
    if let Some((_, backend)) = synthetic_identity_for_path(path) {
      return Err(IdentityError::BackendUnavailable { backend });
    }
  }
  // A local file. One header read yields the id and every header-derived
  // field, so the knob resolver never re-reads it.
  let header =
    crate::gguf::header::read_path(path, crate::gguf::header::HeaderReadOptions::default())
      .map_err(|e| {
        IdentityError::Header(format!(
          "could not read GGUF header at {}: {e}",
          path.display()
        ))
      })?;
  let summary = crate::gguf::metadata::summarise(&header.header);
  let id = crate::gguf::identity::compute(path, &header.raw);
  Ok(ResolvedIdentity {
    identity: ModelIdentity::Gguf(id.clone()),
    id,
    claimed_by: None,
    arch: summary.arch,
    // Trained context window (`<arch>.context_length`), clamped into u32.
    native_ctx: summary
      .native_ctx
      .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
    supported_backends: supported_backends_for(&header.header),
    mtp_embedded: summary.mtp,
  })
}

/// The external-process markers every backend contributes — the orphan sweep's
/// "is this an unmanaged instance of a backend server" list. Registry-driven,
/// so a new backend's server is swept from its `process_markers` override
/// alone, alternate binary names included.
pub fn external_process_markers() -> Vec<&'static str> {
  Backends::all()
    .iter()
    .flat_map(|b| b.process_markers().iter().copied())
    .collect()
}

/// The default backend instance — what a plain GGUF binds to and every
/// unknown-id / no-selection fallback resolves to. A client that must produce a
/// concrete backend without an identity in hand uses this instead of naming a
/// specific backend.
pub fn default_backend() -> Backends {
  Backends::LlamaCpp(LlamaCppBackend::new())
}

/// Whether the backend with `id` is a managed multiplexer (its models are
/// delegated to a shared umbrella, so they share the umbrella's port / RAM /
/// CPU). Lets a client key on the lifecycle shape from just an id, without
/// naming a backend. Unknown id → `false`.
pub fn is_managed_multiplexer(id: &str) -> bool {
  Backends::all()
    .iter()
    .any(|b| b.id() == id && b.lifecycle() == Lifecycle::ManagedMultiplexer)
}

/// The native-knob descriptors a backend (by id) declares, or an empty slice
/// for an unknown / knob-less backend. Lets a client (the TUI running-knob view)
/// render a backend's native knobs from just its id, without naming a backend.
pub fn native_knobs_for(id: &str) -> &'static [NativeKnobDescriptor] {
  Backends::all()
    .iter()
    .find(|b| b.id() == id)
    .map(|b| b.native_knobs())
    .unwrap_or(&[])
}

/// The process name for an adopted child recorded under `backend_id`, or the
/// default backend's own marker when the id is unknown / marker-less. Used by
/// the orphan sweep to label a re-adopted process. Names no backend — the marker
/// comes from the backend's own `process_markers`.
pub fn adopted_process_name(backend_id: &str) -> &'static str {
  Backends::all()
    .iter()
    .find(|b| b.id() == backend_id)
    .and_then(|b| b.process_markers().first().copied())
    // Unknown / marker-less id falls back to the default backend's own marker.
    .or_else(|| default_backend().process_markers().first().copied())
    .unwrap_or_default()
}

/// Resolve the backend for a launch, honoring the per-model override **and**
/// the header-level routing signal (D-route).
///
/// Precedence: an explicit [`BackendChoice`] wins verbatim. Otherwise `Auto`
/// prefers the backend that auto-claimed the header — `routed_backend` is that
/// backend's id (from [`routed_backend_for`]) — but only when it is
/// [`Backend::available`] and [`Backend::serves_mode`] for this launch;
/// otherwise it falls back to the [`backend_for_identity`] rule (a fallback,
/// never a refusal). Registry-driven, so this seam names no backend and a new
/// routing backend needs only its trait overrides.
pub fn resolve_backend_for_launch(
  identity: &ModelIdentity,
  choice: BackendChoice,
  supported_backends: &[String],
  mode: LaunchMode,
  ctx: &MethodContext,
) -> Backends {
  match choice {
    BackendChoice::Auto => {
      // Walk the priority-ordered supported list; take the first backend that
      // is available and serves this mode (so a compatible ds4 model falls back
      // to llama.cpp when ds4 is absent, or on an embedding/rerank launch).
      for id in supported_backends {
        if let Some(b) = Backends::all().into_iter().find(|b| b.id() == id.as_str()) {
          if b.available(ctx) && b.serves_mode(mode) {
            return b;
          }
        }
      }
      backend_for_identity(identity)
    }
    other => resolve_backend(identity, other),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::lemonade::LEMONADE_BACKEND_ID;

  #[test]
  fn capability_all_covers_every_knob_spec() {
    let all = KnobCapability::all();
    for spec in knob_specs() {
      assert!(
        all.supports(spec.field),
        "KnobCapability::all() must cover {:?}",
        spec.field
      );
    }
  }

  #[test]
  fn process_launch_spec_is_constructible_and_readable() {
    // Proves the process-per-model shape is usable end-to-end as a
    // value (the supervisor will consume exactly these fields).
    let spec = ProcessLaunchSpec {
      binary: PathBuf::from("/usr/bin/llama-server"),
      argv: vec![OsString::from("--port"), OsString::from("41100")],
      env_remove: vec!["LLAMA_ARG_HOST"],
      readiness: Readiness::HttpPoll {
        path: "/health".to_string(),
        ready_status: 200,
      },
      probe: ProbeOptions::default(),
    };
    match LaunchPlan::SpawnProcess(spec) {
      LaunchPlan::SpawnProcess(s) => {
        assert_eq!(s.binary, PathBuf::from("/usr/bin/llama-server"));
        assert_eq!(s.argv.len(), 2);
        assert_eq!(s.env_remove, vec!["LLAMA_ARG_HOST"]);
        assert!(matches!(
          s.readiness,
          Readiness::HttpPoll {
            ready_status: 200,
            ..
          }
        ));
      }
      LaunchPlan::DelegateToManager(_) => unreachable!("constructed a SpawnProcess"),
    }
  }

  #[test]
  fn resolve_backend_honors_override_then_auto_rule() {
    use crate::backend::identity::BackendModelId;
    use crate::gguf::identity::compute;

    let gguf = ModelIdentity::Gguf(compute("/m/model.gguf", b"hdr"));
    let lemon = ModelIdentity::Backend(BackendModelId {
      backend: LEMONADE_BACKEND_ID.into(),
      name: "Qwen2.5-7B-Instruct-GGUF".into(),
    });
    // A backend-registry identity for an *unknown* backend falls back to the
    // safe direct path.
    let unknown = ModelIdentity::Backend(BackendModelId {
      backend: "made-up".into(),
      name: "x".into(),
    });

    // Auto runs the R13 identity rule; GGUF + explicit llama.cpp both bind
    // the direct backend; a Lemonade identity binds Lemonade.
    assert_eq!(resolve_backend(&gguf, BackendChoice::Auto).id(), "llamacpp");
    assert_eq!(
      resolve_backend(&gguf, BackendChoice::Explicit("llamacpp".into())).id(),
      "llamacpp"
    );
    assert_eq!(
      resolve_backend(&lemon, BackendChoice::Auto).id(),
      "lemonade"
    );
    assert_eq!(
      resolve_backend(&unknown, BackendChoice::Auto).id(),
      "llamacpp",
      "no concrete backend for an unknown registry identity → safe direct fallback"
    );

    // An explicit override wins over the identity rule: force Lemonade
    // even for a GGUF identity.
    assert_eq!(
      resolve_backend(&gguf, BackendChoice::Explicit("lemonade".into())).id(),
      "lemonade"
    );

    // The default choice is Auto.
    assert_eq!(BackendChoice::default(), BackendChoice::Auto);
  }

  #[test]
  fn resolve_backend_auto_exposes_full_capability_set_for_gguf() {
    use crate::gguf::identity::compute;
    use crate::launch::flag_aliases::knob_specs;
    let gguf = ModelIdentity::Gguf(compute("/m/anything.gguf", b"hdr"));
    let b = resolve_backend(&gguf, BackendChoice::Auto);
    assert_eq!(b.id(), "llamacpp");
    assert_eq!(b.lifecycle(), Lifecycle::ProcessPerModel);
    // The selected backend exposes the full capability set (R6 data seam).
    for spec in knob_specs() {
      assert!(b.capabilities().supports(spec.field));
    }
  }

  #[test]
  fn llama_and_lemonade_declare_no_native_knobs() {
    // The native-knob channel is empty for llama.cpp and Lemonade, so the
    // picker + persistence stay byte-identical for them. ds4 is the first
    // backend to override `native_knobs()`.
    assert!(Backends::LlamaCpp(LlamaCppBackend::new())
      .native_knobs()
      .is_empty());
    assert!(Backends::Lemonade(LemonadeBackend::new())
      .native_knobs()
      .is_empty());
  }

  #[test]
  fn lifecycle_labels_are_stable() {
    assert_eq!(Lifecycle::ProcessPerModel.label(), "process_per_model");
    assert_eq!(Lifecycle::ManagedMultiplexer.label(), "managed_multiplexer");
  }

  #[test]
  fn backend_for_identity_routes_by_shape() {
    use crate::backend::identity::BackendModelId;
    use crate::gguf::identity::compute;

    // GGUF always binds to the direct llama.cpp backend.
    let gguf = ModelIdentity::Gguf(compute("/m/model.gguf", b"hdr"));
    assert_eq!(backend_for_identity(&gguf).id(), "llamacpp");
    assert_eq!(
      backend_for_identity(&gguf).lifecycle(),
      Lifecycle::ProcessPerModel
    );

    // A Lemonade-registry identity binds the managed-multiplexer backend.
    let lemon = ModelIdentity::Backend(BackendModelId {
      backend: LEMONADE_BACKEND_ID.into(),
      name: "Qwen2.5-7B-Instruct-GGUF".into(),
    });
    assert_eq!(backend_for_identity(&lemon).id(), "lemonade");
    assert_eq!(
      backend_for_identity(&lemon).lifecycle(),
      Lifecycle::ManagedMultiplexer
    );

    // A backend-registry identity for an unknown backend falls back to the
    // safe direct path.
    let unknown = ModelIdentity::Backend(BackendModelId {
      backend: "made-up".into(),
      name: "x".into(),
    });
    assert_eq!(backend_for_identity(&unknown).id(), "llamacpp");
  }

  #[test]
  fn backends_enum_forwards_to_each_variant() {
    let llama = Backends::LlamaCpp(LlamaCppBackend::new());
    assert_eq!(llama.id(), "llamacpp");
    assert_eq!(llama.lifecycle(), Lifecycle::ProcessPerModel);

    let lemon = Backends::Lemonade(LemonadeBackend::new());
    assert_eq!(lemon.id(), "lemonade");
    assert_eq!(lemon.lifecycle(), Lifecycle::ManagedMultiplexer);

    // The dispatch enum routes prepare_launch to the process-per-model plan.
    use crate::launch::mode::LaunchMode;
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    assert!(matches!(
      llama.prepare_launch(
        &p,
        41100,
        PathBuf::from("/bin/llama-server"),
        ProbeOptions::default()
      ),
      LaunchPlan::SpawnProcess(_)
    ));
  }

  #[test]
  fn delegate_to_manager_carries_umbrella_and_model() {
    // The managed-multiplexer arm: an umbrella ProcessLaunchSpec (probed
    // via a liveness endpoint) plus the model the umbrella should serve.
    let umbrella = ProcessLaunchSpec {
      binary: PathBuf::from("/opt/example/server"),
      argv: vec![
        OsString::from("--host"),
        OsString::from("127.0.0.1"),
        OsString::from("--port"),
        OsString::from("13305"),
      ],
      env_remove: vec![],
      readiness: Readiness::HttpPoll {
        path: "/live".to_string(),
        ready_status: 200,
      },
      probe: ProbeOptions::default(),
    };
    let plan = LaunchPlan::DelegateToManager(ManagerLaunchSpec {
      umbrella,
      model: ManagerModelRef {
        name: "Qwen2.5-7B-Instruct-GGUF".to_string(),
      },
    });
    match plan {
      LaunchPlan::DelegateToManager(spec) => {
        assert_eq!(spec.model.name, "Qwen2.5-7B-Instruct-GGUF");
        assert!(matches!(
          spec.umbrella.readiness,
          Readiness::HttpPoll {
            ready_status: 200,
            ..
          }
        ));
        // Readiness path is a probe target, not a launch arg.
        assert!(!spec.umbrella.argv.iter().any(|a| a == "/live"));
      }
      LaunchPlan::SpawnProcess(_) => panic!("expected DelegateToManager"),
    }
  }

  #[test]
  fn all_registry_lists_every_backend_once() {
    // The single enumeration point: every shipped backend, exactly once, with
    // the ids the rest of the tree keys off. A new backend appears here by
    // construction (one `all()` line), which is what makes it surface in
    // `status` / `doctor` / `--backend` without editing those sites.
    let ids: Vec<&str> = Backends::all().iter().map(|b| b.id()).collect();
    assert_eq!(ids, vec!["llamacpp", "lemonade", "ds4", "vllm"]);
    // Forwarding through the macro reaches each variant's real lifecycle.
    let by_id: std::collections::BTreeMap<&str, Lifecycle> = Backends::all()
      .iter()
      .map(|b| (b.id(), b.lifecycle()))
      .collect();
    assert_eq!(by_id["llamacpp"], Lifecycle::ProcessPerModel);
    assert_eq!(by_id["lemonade"], Lifecycle::ManagedMultiplexer);
    assert_eq!(by_id["ds4"], Lifecycle::ProcessPerModel);
    assert_eq!(by_id["vllm"], Lifecycle::ProcessPerModel);
  }

  fn ds4_header() -> GgufHeader {
    use crate::gguf::header::{GgufValue, TensorInfo};
    use std::collections::HashMap;
    let mut metadata = HashMap::new();
    metadata.insert(
      "general.architecture".to_string(),
      GgufValue::String("deepseek4".to_string()),
    );
    GgufHeader {
      version: 3,
      tensor_count: 2,
      metadata,
      tensors: vec![
        TensorInfo {
          name: "blk.0.ffn_gate_exps.weight".to_string(),
          dims: vec![4096, 4096],
          ggml_type: 16, // IQ2_XXS — a routed-expert quant ds4 accepts
        },
        TensorInfo {
          name: "token_embd.weight".to_string(),
          dims: vec![4096, 4096],
          ggml_type: 1, // F16
        },
      ],
    }
  }

  #[test]
  fn backends_forward_defaulted_methods_to_variants() {
    // Regression guard: `Backends` must forward every *defaulted* trait method
    // to the active variant, else it silently returns the trait default rather
    // than the override. Two cheap sentinels: serves_mode (a variant overrides
    // Embedding → false; the default is true) and auto_routes (drives routing,
    // reached through routed_backend_for).
    let ds4 = Backends::Ds4(Ds4Backend::new());
    assert!(
      !ds4.serves_mode(LaunchMode::Embedding),
      "Backends must forward serves_mode to the variant"
    );
    assert!(ds4.serves_mode(LaunchMode::Chat));
    assert!(Backends::LlamaCpp(LlamaCppBackend::new()).serves_mode(LaunchMode::Embedding));

    // routed_backend_for exercises Backends::auto_routes forwarding end to end:
    // a compatible header resolves to the claiming backend's id.
    let h = ds4_header();
    assert!(
      ds4.auto_routes(&h),
      "Backends must forward auto_routes to the variant"
    );
    assert_eq!(routed_backend_for(&h), Some("ds4".to_string()));

    // A plain header claims no special routing → falls back to identity.
    use crate::gguf::header::GgufValue;
    use std::collections::HashMap;
    let mut m = HashMap::new();
    m.insert(
      "general.architecture".to_string(),
      GgufValue::String("llama".to_string()),
    );
    let plain = GgufHeader {
      version: 3,
      tensor_count: 0,
      metadata: m,
      tensors: vec![],
    };
    assert_eq!(routed_backend_for(&plain), None);
  }
}
