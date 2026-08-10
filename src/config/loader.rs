use std::{
  collections::BTreeMap,
  env,
  ffi::OsString,
  fs,
  io::ErrorKind,
  net::{IpAddr, Ipv4Addr},
  path::{Path, PathBuf},
  time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::launch::flag_aliases::{knob_specs, KnobField};
use crate::launch::mode::LaunchMode;
use crate::theme::{CustomThemeConfig, ThemeName};
use crate::util::paths::user_config_file;

/// Hard cap on config-file size. The YAML parser expands anchors and aliases
/// without depth limits — a hostile file could mushroom in memory. 1 MiB is
/// far more than any plausible hand-written config and small enough that even
/// pathological YAML can't OOM the process.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Hard ceiling on any context-window value (`-c`, `knobs.ctx`, the
/// `fit_ctx_floor` config). 2^20 tokens — far above any real model's
/// trained window, low enough that a fat-fingered value is caught
/// before it reaches `llama-server`. Centralised here so the CLI,
/// daemon admission, and config validation share one bound.
pub const MAX_CTX_TOKENS: u32 = 1_048_576;

/// Factory `fit_ctx_floor`: the `--fit-ctx` floor llamastash passes so
/// `--fit` never collapses the window below a usable size on the
/// unified-memory hosts where its free reading mis-reports (the 4096
/// upstream floor is too small for real chat sessions).
pub const DEFAULT_FIT_CTX_FLOOR: u32 = 16384;

/// User-authored YAML config, with sensible defaults via `#[serde(default)]`.
///
/// Every field is optional in the file; missing fields use the built-in
/// defaults. Unknown fields are accepted silently so old files keep working
/// when new fields are added (forward-compat). Unknown values within a known
/// field (e.g. a non-existent theme name) still error, which is intentional —
/// silent typo tolerance for theme names would mask a real user problem.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Config {
  pub theme: ThemeName,
  /// Optional user-defined palette. When present it becomes the
  /// `Custom` theme target — selectable via the config `theme:
  /// custom` setting, and joined to the `t:theme` cycle. Absent
  /// (the default) means `Custom` is not selectable and the cycle
  /// stays on the five built-ins. See
  /// [`crate::theme::custom::CustomThemeConfig`] for the slot list.
  pub custom_theme: Option<CustomThemeConfig>,
  pub model_paths: Vec<PathBuf>,
  pub disable_default_cache_paths: CachePathsConfig,
  pub keybindings: BTreeMap<String, String>,
  /// GPU probe configuration, grouped under `gpu:`. Controls which
  /// vendor tools the daemon spawns during initial and periodic
  /// hardware detection.
  #[serde(default)]
  pub gpu: GpuConfig,
  /// Daemon lifecycle configuration, grouped under `daemon:` — launch
  /// port range, health-probe timeout, idle shutdown, and the
  /// host-metrics sampler cadence.
  #[serde(default)]
  pub daemon: DaemonConfig,
  pub disable_scan: bool,
  /// Opt into terminal mouse capture so a left-click can switch pane
  /// focus and pick a right-pane tab. Off by default: capturing the
  /// mouse pre-empts the terminal's native click-and-drag text
  /// selection, so users who copy paths / logs out of the dashboard
  /// keep the cleaner default. When enabled, most terminals still
  /// expose a bypass modifier (Shift on iTerm2/Alacritty/foot/wezterm,
  /// Option on Apple Terminal) for ad-hoc selections.
  pub mouse_focus: bool,
  /// Per-architecture launch defaults — user escape hatch over the
  /// built-in `(arch, gpu_backend) → TypedKnobs` table. Map keys are
  /// GGUF `general.architecture` strings (`llama`, `qwen2`, `mistral`,
  /// `gemma`, `phi`, `qwen3`, …). At launch time the daemon merges
  /// these layers in precedence order — preset > last_params >
  /// `arch_defaults` (this map) > built-in table > llama-server. The
  /// wizard no longer writes this field; it remains as a hand-edited
  /// escape hatch for users overriding a built-in row.
  pub arch_defaults: BTreeMap<String, TypedKnobs>,
  /// OpenAI-compat proxy router. Enabled by default so agent clients
  /// (OpenCode, Pi) can attach to one stable URL and route by
  /// `body.model`. In normal mode the listener prefers
  /// `127.0.0.1:11435`; in Ollama-compat mode it prefers
  /// `127.0.0.1:11434`. See
  /// docs/plans/2026-05-21-001-feat-proxy-router-plan.md for the
  /// rationale. Unknown keys inside `[proxy]` are rejected loudly so
  /// a typo never silently falls back to defaults — separate posture
  /// from the top-level config which tolerates unknown keys for
  /// forward-compat.
  pub proxy: ProxyConfig,
  /// All backend configuration, grouped under `backend:`. Holds the always-on
  /// llama.cpp settings (`binary`, `additional_binaries`, `jinja`,
  /// `strict_fit`, `fit_ctx_floor`) plus the optional Lemonade / ds4 engines
  /// (each default-on when its binary resolves). Each backend owns its own
  /// typed struct in its own module; see [`crate::backend::BackendConfig`].
  #[serde(default)]
  pub backend: crate::backend::BackendConfig,
  /// How a knob no layer supplied a value for is seeded at launch
  /// `auto` (factory) delegates layer-less knobs to `--fit`;
  /// `inherited` leaves them unset (pre-Auto behavior). Env override:
  /// `LLAMASTASH_DEFAULT_LAUNCH_MODE=auto|inherited`.
  #[serde(default)]
  pub default_launch_mode: DefaultLaunchMode,
  /// Render the TUI with the `7`-bit ASCII glyph fallback instead of
  /// the default Unicode house style (geometric status dots, severity
  /// triangles, box-drawing borders). For terminals / fonts that show
  /// the Unicode set as tofu. Factory `false`. The `LLAMASTASH_ASCII=1`
  /// env var overrides this and forces ASCII on regardless.
  #[serde(default)]
  pub ascii_glyphs: bool,
  /// Left (Models list) pane width percentages the TUI `Alt+L` shortcut
  /// cycles through, in wide mode. Slot 0 is the startup default; each press
  /// advances to the next, wrapping. `100` hides the right pane, `0` hides the
  /// list. Session-only — the pick resets to slot 0 on restart. At most 5 slots
  /// are honored (extras ignored); each is clamped to `0..=100`; an empty /
  /// all-invalid list falls back to the factory `[65, 100, 50, 35, 0]`.
  #[serde(default = "default_left_pane_ratios")]
  pub left_pane_ratios: Vec<u16>,
  /// Named launch presets, the single writable home for presets. Map
  /// keys are classified per-resolution against the live model catalog
  /// (see [`crate::launch::presets::classify_preset_key`]): a key that
  /// names a discovered model (by basename, path fallback) is **per
  /// model**; otherwise it is read as a GGUF `general.architecture` id
  /// and applies to **every model of that arch**. Model wins on a name
  /// collision. The CLI `presets save/delete` and the TUI `Ctrl+P` write
  /// per-model keys here (comment-safe, via
  /// [`crate::config::presets_writer`]); arch keys are hand-authored.
  #[serde(default)]
  pub presets: BTreeMap<String, ConfigPresetBlock>,
}

/// Factory left-pane width cycle: current (65/35), list-full, even, right-heavy,
/// right-full. Also the fallback when a user override sanitizes to empty.
pub fn default_left_pane_ratios() -> Vec<u16> {
  vec![65, 100, 50, 35, 0]
}

/// Bounds on [`DaemonConfig::metrics_interval_secs`]. Below 1 s the
/// sampler spends more time spawning `nvidia-smi` than sleeping; above
/// 60 s the host pane is stale enough to look broken.
const METRICS_INTERVAL_SECS_RANGE: std::ops::RangeInclusive<u64> = 1..=60;

/// Daemon lifecycle configuration, grouped under `daemon:`.
///
/// The `*_secs` fields are the raw user-authored values; read them
/// through the accessors ([`Self::metrics_interval`],
/// [`Self::idle_timeout`]) so the clamp and the "0 disables" rule are
/// applied identically everywhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct DaemonConfig {
  /// Inclusive TCP port range the supervisor picks from when launching
  /// a backend server.
  pub port_range: PortRange,
  /// Per-launch health-probe timeout in seconds. 120 s is enough for
  /// the typical 7B–13B model on local NVMe but can be tight for 70B+
  /// on slow disks. Raise to e.g. 600 if you hit `health probe timeout
  /// (last status 503)` for legitimate loads.
  pub probe_timeout_secs: u64,
  /// Seconds of inactivity (no running models AND no attached clients)
  /// before the daemon auto-shuts down. `0` (factory) disables the idle
  /// timer — the daemon runs until explicitly stopped.
  pub idle_timeout_secs: u64,
  /// Host-metrics sampler cadence in seconds (CPU%, RAM, GPU
  /// util/temp/VRAM). Factory `1` (1 Hz). Raising it reduces how often
  /// `nvidia-smi` / `rocm-smi` are spawned, at the cost of a less
  /// responsive host pane.
  pub metrics_interval_secs: u64,
}

impl DaemonConfig {
  /// Sampler cadence, clamped into `METRICS_INTERVAL_SECS_RANGE`. A
  /// `0` would busy-loop the sampler, so it resolves to the 1 s floor
  /// rather than disabling anything.
  pub fn metrics_interval(&self) -> Duration {
    Duration::from_secs(self.metrics_interval_secs.clamp(
      *METRICS_INTERVAL_SECS_RANGE.start(),
      *METRICS_INTERVAL_SECS_RANGE.end(),
    ))
  }

  /// Idle-shutdown deadline, or `None` when the timer is disabled.
  pub fn idle_timeout(&self) -> Option<Duration> {
    (self.idle_timeout_secs > 0).then(|| Duration::from_secs(self.idle_timeout_secs))
  }
}

impl Default for DaemonConfig {
  fn default() -> Self {
    Self {
      port_range: PortRange::default(),
      probe_timeout_secs: 120,
      idle_timeout_secs: 0,
      metrics_interval_secs: 1,
    }
  }
}

/// Sanitize a configured `left_pane_ratios` list: keep at most the first
/// [`MAX_LEFT_PANE_RATIO_SLOTS`] slots, clamp each to `0..=100`, and fall back
/// to [`default_left_pane_ratios`] when the result is empty (unset / all
/// dropped). Applied once when the TUI resolves its options.
pub fn sanitize_left_pane_ratios(raw: &[u16]) -> Vec<u16> {
  let slots: Vec<u16> = raw
    .iter()
    .take(MAX_LEFT_PANE_RATIO_SLOTS)
    .map(|p| (*p).min(100))
    .collect();
  if slots.is_empty() {
    default_left_pane_ratios()
  } else {
    slots
  }
}

/// Upper bound on `left_pane_ratios` slots the `Alt+L` cycle honors.
pub const MAX_LEFT_PANE_RATIO_SLOTS: usize = 5;

/// OpenAI-compat proxy router configuration.
///
/// `enabled: true` (the default) starts a hyper listener on
/// `127.0.0.1:<port>` inside the daemon process. Two operating modes:
///
/// - **Default** (`ollama_compat: false`): identifies as `LlamaStash is
///   running` on `GET /` and prefers port `11435` so an existing
///   Ollama install on `11434` keeps working. Co-existence by design.
/// - **Ollama-compat** (`ollama_compat: true`): identifies as `Ollama
///   is running`, prefers port `11434`, and serves as a drop-in for
///   Ollama-shape clients (the `ollama` CLI, Ollama-Go libraries,
///   etc.) that probe `HEAD /` before any `/api/*` call.
///
/// Both modes scan up to port `11440` for a free slot; both speak the
/// same OpenAI compat + Ollama-discovery surfaces. The listener binds
/// loopback (`127.0.0.1`) by default; `host` opts the *proxy data
/// plane* into LAN exposure, gated behind the `api_key` bearer token
/// (the control plane and `llama-server` children always stay
/// loopback). TLS is not yet implemented — LAN mode is plaintext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ProxyConfig {
  /// Whether the daemon binds the proxy listener at startup. When
  /// `false`, the daemon still runs; `status.proxy.status` reports
  /// `"disabled"`. Default `true`.
  #[serde(default = "ProxyConfig::default_enabled")]
  pub enabled: bool,
  /// Base TCP port for the loopback listener on `127.0.0.1`.
  /// `None` (the YAML default) is resolved by [`Self::effective_port`]
  /// from `ollama_compat`: `11434` when true, `11435` otherwise.
  /// `Some(N)` pins the base port regardless of mode. The listener
  /// then walks `base..=base+5` looking for a free slot (six
  /// attempts) — see [`crate::proxy::server::DEFAULT_PORT_SCAN_MAX_OFFSET`]
  /// and the `--proxy-port` CLI override.
  #[serde(default)]
  pub port: Option<u16>,
  /// Enable Ollama drop-in mode. Default `false`.
  ///
  /// When `true`: `GET /` returns `"Ollama is running"` (Ollama-CLI
  /// handshake), and `effective_port()` defaults to `11434`. When
  /// `false`: `GET /` returns `"LlamaStash is running"` and the
  /// default port is `11435` so the listener coexists with a running
  /// Ollama without colliding.
  ///
  /// CLI override: `--ollama-compat`. Env override:
  /// `LLAMASTASH_OLLAMA_COMPAT=1`. The three sources are OR-ed; any
  /// one of them enables the mode for that daemon process.
  #[serde(default)]
  pub ollama_compat: bool,
  /// Family-MRU fallback behaviour when a requested model fails to
  /// auto-start. Default `true`: the proxy picks another Ready
  /// supervisor (same arch first, then any) and serves the request
  /// with `x-llamastash-fallback-reason`. Set to `false` to make the
  /// proxy return a 503 `launch_failed` envelope instead — useful
  /// when a client must not silently receive a response from a
  /// different model (e.g. an embedding client that would
  /// mis-interpret a chat-completion payload).
  ///
  /// CLI override: `--no-proxy-fallback` (only disables; cannot
  /// re-enable from the CLI). Env override:
  /// `LLAMASTASH_NO_PROXY_FALLBACK=1`. Any of the three "disable"
  /// signals turns it off — re-enabling requires unsetting all of
  /// them and setting `fallback_enabled: true` in config (the
  /// default).
  #[serde(default = "ProxyConfig::default_fallback_enabled")]
  pub fallback_enabled: bool,
  /// How long hyper waits for a client to finish sending request
  /// headers, in seconds. Default `30`. Bounds partial-request clients
  /// (crashed agents leaving sockets half-open, slow-loris-style
  /// mistakes) so they don't pin a serve_connection task forever.
  /// Raise to e.g. `120` if an agent legitimately streams headers
  /// across a slow link.
  #[serde(default = "ProxyConfig::default_header_read_timeout_secs")]
  pub header_read_timeout_secs: u64,
  /// Idle-TTL eviction for proxy-auto-started supervisors. After
  /// `idle_ttl_secs` of no inbound request *and* no in-flight stream,
  /// the daemon's eviction sweeper calls `model.stop(5s grace)` so a
  /// long-running daemon doesn't pin VRAM on models nobody is using.
  /// Default `1800` (30 min). `0` disables eviction entirely;
  /// supervisors stay resident until explicit `stop_model`.
  ///
  /// Only auto-start supervisors (`LaunchOrigin::AutoStart`) are
  /// evictable — explicit `llamastash start` / TUI launches are
  /// treated as durable user intent and stay resident regardless.
  #[serde(default = "ProxyConfig::default_idle_ttl_secs")]
  pub idle_ttl_secs: u64,
  /// Address the proxy listener binds. `None` (the default) keeps the
  /// listener on `127.0.0.1` — same loopback-only posture as before.
  /// Set to a routable address (`0.0.0.0`, a specific NIC IP, or an
  /// IPv6 address like `::`) to expose the proxy on the LAN. Non-
  /// loopback binding requires `api_key` unless `insecure_no_auth` is
  /// set; otherwise the daemon refuses to bind the proxy (the daemon
  /// itself still runs). Only the proxy moves — the control plane and
  /// `llama-server` children stay loopback regardless.
  ///
  /// CLI override: `--proxy-host <IP>`. Env override:
  /// `LLAMASTASH_PROXY_HOST`. Precedence: CLI > env > config.
  #[serde(default)]
  pub host: Option<IpAddr>,
  /// Bearer token required on the proxy's data routes (`/v1/*`,
  /// `/api/*`) when set. `None` (the default) means no auth — the
  /// loopback-only, same-UID posture. Auto-provisioned and persisted
  /// here the first time LAN binding is enabled without an existing
  /// key. Enforced whenever it is `Some`, regardless of bind host.
  ///
  /// Env override `LLAMASTASH_PROXY_API_KEY` takes precedence and is
  /// never written back to disk (containers / secret managers). The
  /// value is a secret: never log it; status surfaces report only
  /// whether auth is enforced, never the key.
  #[serde(default)]
  pub api_key: Option<String>,
  /// Allow binding a non-loopback `host` with no `api_key` (no auth on
  /// the LAN-exposed proxy). Default `false` — the daemon refuses such
  /// a bind. Set to `true` (or pass `--insecure-no-auth`) only when you
  /// deliberately want an unauthenticated LAN proxy. A loud warning
  /// prints either way when the proxy binds a non-loopback address.
  #[serde(default)]
  pub insecure_no_auth: bool,
}

impl ProxyConfig {
  fn default_enabled() -> bool {
    true
  }

  /// Port the listener tries first. Falls through to the
  /// `ollama_compat`-derived default when `port` is unset.
  pub fn effective_port(&self) -> u16 {
    self
      .port
      .unwrap_or(if self.ollama_compat { 11434 } else { 11435 })
  }

  /// Address the listener binds. Falls back to loopback
  /// (`127.0.0.1`) when `host` is unset — the historical default.
  pub fn effective_host(&self) -> IpAddr {
    self.host.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
  }

  /// Whether bearer auth is enforced on the proxy's data routes. True
  /// iff an `api_key` is configured; enforcement is independent of the
  /// bind host (a configured key is honored even on loopback).
  pub fn auth_enforced(&self) -> bool {
    self.api_key.is_some()
  }

  /// The bearer key actually in force for this process, and the single
  /// resolver for it: the `LLAMASTASH_PROXY_API_KEY` env override
  /// (trimmed, non-empty) wins over the configured `api_key`; a
  /// blank/whitespace value on either side reads as "no key" (`None` —
  /// the keyless loopback default). The daemon folds this into
  /// `opts.proxy.api_key` at bind time so `auth_enforced()`,
  /// `ProxyAuth`, and the fail-closed backstop all see one value; the
  /// init wizard's external-tool writers read it directly so their
  /// generated configs carry exactly what the proxy enforces.
  pub fn effective_api_key(&self) -> Option<String> {
    if let Some(raw) = std::env::var_os("LLAMASTASH_PROXY_API_KEY") {
      let key = raw.to_string_lossy().trim().to_string();
      if !key.is_empty() {
        return Some(key);
      }
    }
    self
      .api_key
      .as_deref()
      .map(str::trim)
      .filter(|k| !k.is_empty())
      .map(str::to_string)
  }

  fn default_header_read_timeout_secs() -> u64 {
    30
  }

  fn default_fallback_enabled() -> bool {
    true
  }

  fn default_idle_ttl_secs() -> u64 {
    30 * 60
  }
}

impl Default for ProxyConfig {
  fn default() -> Self {
    Self {
      enabled: Self::default_enabled(),
      port: None,
      ollama_compat: false,
      fallback_enabled: Self::default_fallback_enabled(),
      header_read_timeout_secs: Self::default_header_read_timeout_secs(),
      idle_ttl_secs: Self::default_idle_ttl_secs(),
      host: None,
      api_key: None,
      insecure_no_auth: false,
    }
  }
}

/// A single typed-knob slot's *state*. A knob is either pinned to an
/// explicit value (`Set`) or delegated to llama-server's `--fit`
/// placement (`Auto`). The third state — *Inherited* — is the
/// absence of a `KnobValue`: `None` on the `Option<KnobValue<T>>`
/// field, which the layered resolver fills from the next layer down,
/// or which falls through to llama-server's own default.
///
/// **Serde shape:** `Set(v)` serialises as the bare scalar `v` exactly
/// as the pre-tri-state `Option<T>` field did, so existing bare values
/// load unchanged. `Auto` serialises as the bare token `auto` (e.g.
/// `ctx: auto`) — readable and idiomatic in both `config.yaml` and the
/// JSON wire.
///
/// **The `auto` collision, and its escape hatch:** `"auto"` *could* be a
/// legal value for a string knob, so the bare token is reserved for the
/// Auto state. To set the literal string `"auto"` on a knob, wrap it in
/// the explicit `{ value: auto }` escape, which always round-trips as
/// `Set("auto")`. No string/number/bool knob value is ever a map, so the
/// `{ value: … }` escape (and the still-read legacy `{ auto: true }`
/// sentinel) can never collide with a real scalar value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KnobValue<T> {
  /// Explicitly pinned to a concrete value; emits the flag verbatim.
  Set(T),
  /// Delegated to `--fit`; emits no flag (fit governs placement).
  Auto,
}

/// The bare YAML/JSON token that denotes the [`KnobValue::Auto`] state.
const AUTO_TOKEN: &str = "auto";

/// True when `v` would itself serialise to the bare `auto` token (i.e. the
/// string `"auto"`) — the one value that needs the `{ value: … }` escape so
/// it round-trips as `Set`, not the Auto sentinel. Only a string can collide.
fn serialises_as_auto_token<T: Serialize>(v: &T) -> bool {
  matches!(
    serde_json::to_value(v),
    Ok(serde_json::Value::String(s)) if s == AUTO_TOKEN
  )
}

impl<T> KnobValue<T> {
  /// True when this knob is delegated to `--fit`.
  pub fn is_auto(&self) -> bool {
    matches!(self, KnobValue::Auto)
  }

  /// Borrow the concrete value when `Set`; `None` when `Auto`.
  pub fn as_set(&self) -> Option<&T> {
    match self {
      KnobValue::Set(v) => Some(v),
      KnobValue::Auto => None,
    }
  }

  /// Take the concrete value when `Set`; `None` when `Auto`.
  pub fn into_set(self) -> Option<T> {
    match self {
      KnobValue::Set(v) => Some(v),
      KnobValue::Auto => None,
    }
  }
}

impl<T: Serialize> Serialize for KnobValue<T> {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    match self {
      KnobValue::Auto => serializer.serialize_str(AUTO_TOKEN),
      // A value that would itself render as the bare `auto` token must be
      // wrapped in the `{ value: … }` escape so it reads back as `Set`.
      KnobValue::Set(v) if serialises_as_auto_token(v) => {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("value", v)?;
        map.end()
      }
      KnobValue::Set(v) => v.serialize(serializer),
    }
  }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for KnobValue<T> {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    // Untagged probe (self-describing formats buffer and retry, so this is
    // format-agnostic). Order matters; no scalar knob value is ever a map,
    // so the two map forms can never shadow a legitimate `Set`:
    //   1. `{ value: X }`   -> Set(X)  — explicit escape (forces Set even
    //                                    when X is the literal "auto").
    //   2. `{ auto: true }` -> Auto    — legacy sentinel, still read.
    //   3. the bare token `auto`       -> Auto.
    //   4. any other bare scalar       -> Set.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr<T> {
      Escape { value: T },
      Sentinel { auto: bool },
      Auto(AutoToken),
      Set(T),
    }
    match Repr::<T>::deserialize(deserializer)? {
      Repr::Escape { value } => Ok(KnobValue::Set(value)),
      Repr::Sentinel { auto } => {
        // Any `auto`-keyed map is the Auto state; the bool value is irrelevant
        // (binding it keeps the field live so serde still matches the shape).
        let _ = auto;
        Ok(KnobValue::Auto)
      }
      Repr::Auto(_) => Ok(KnobValue::Auto),
      Repr::Set(v) => Ok(KnobValue::Set(v)),
    }
  }
}

/// Deserializes only from the exact bare token `auto`; any other scalar
/// errors so the untagged probe falls through to `Set`. (A unit type rather
/// than a `()` so the untagged enum gives it a distinct variant.)
struct AutoToken;

impl<'de> Deserialize<'de> for AutoToken {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    let s = String::deserialize(deserializer)?;
    if s == AUTO_TOKEN {
      Ok(AutoToken)
    } else {
      Err(serde::de::Error::custom("not the `auto` token"))
    }
  }
}

/// Ergonomic accessors over an `Option<KnobValue<T>>` knob slot, so the
/// many sites that read the old two-state `Option<T>` value keep their
/// shape. `None` (Inherited) and `Auto` both collapse to "no concrete
/// value" — the correct view for argv emission and value display, where
/// Auto emits/shows nothing just like an unset field.
pub trait KnobValueOpt<T> {
  /// Borrow the concrete value when the knob is `Set`; `None` when
  /// unset (Inherited) or `Auto`.
  fn set_value(&self) -> Option<&T>;
  /// True when the knob is explicitly delegated to `--fit`.
  fn is_auto(&self) -> bool;
}

impl<T> KnobValueOpt<T> for Option<KnobValue<T>> {
  fn set_value(&self) -> Option<&T> {
    match self {
      Some(KnobValue::Set(v)) => Some(v),
      _ => None,
    }
  }
  fn is_auto(&self) -> bool {
    matches!(self, Some(KnobValue::Auto))
  }
}

/// Typed launch knobs the supervisor argvifies into `llama-server`
/// flags. Used everywhere a structured per-launch tuning surface is
/// needed: persistence (`LaunchParams.knobs`), IPC wire shape, the
/// built-in `(arch, gpu_backend)` defaults table, the YAML
/// `arch_defaults` escape hatch, and the Settings-tab typed editor.
///
/// Every field is `Option<KnobValue<T>>` — a tri-state per knob:
/// `None` means "inherit from the next layer down" in the layered
/// resolver, `Some(KnobValue::Set(v))` pins an explicit value, and
/// `Some(KnobValue::Auto)` delegates the knob to llama-server's
/// `--fit`. Field names mirror llama-server's flag names (snake-cased)
/// so they're grep-able directly against the binary's log output.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct TypedKnobs {
  /// Context window length. Maps to `-c` (`--ctx-size`). `None` means
  /// no flag is sent — llama-server reads `context_length` from the
  /// GGUF header.
  pub ctx: Option<KnobValue<u32>>,
  /// Reasoning toggle. `Some(true)` bundles `--jinja --reasoning-format
  /// deepseek` at argv time; `Some(false)` / `None` send nothing and
  /// let the model's chat template decide.
  pub reasoning: Option<KnobValue<bool>>,
  /// Layers offloaded to the GPU. Maps to `--n-gpu-layers`. Use 99
  /// for "all" (llama-server caps internally).
  pub n_gpu_layers: Option<KnobValue<u32>>,
  /// MoE expert layers kept on CPU. Maps to `--n-cpu-moe`. Keeps the
  /// MoE weights of the first N layers in system RAM while the rest
  /// offload to GPU — the counterpart to `n_gpu_layers` for MoE
  /// models that don't fit VRAM.
  pub n_cpu_moe: Option<KnobValue<u32>>,
  /// CPU threads. Maps to `--threads`.
  pub threads: Option<KnobValue<u32>>,
  /// K-cache quantisation tag (e.g. `q8_0`). Maps to `--cache-type-k`.
  pub cache_type_k: Option<KnobValue<String>>,
  /// V-cache quantisation tag. Maps to `--cache-type-v`.
  pub cache_type_v: Option<KnobValue<String>>,
  /// Flash-attention. Maps to `--flash-attn` (boolean flag).
  pub flash_attn: Option<KnobValue<bool>>,
  /// Lock model in RAM. Maps to `--mlock`.
  pub mlock: Option<KnobValue<bool>>,
  /// Disable mmap (forces full load into RAM). Maps to `--no-mmap`.
  pub no_mmap: Option<KnobValue<bool>>,
  /// Concurrent request slots. Maps to `--parallel`.
  pub parallel: Option<KnobValue<u32>>,
  /// Prompt batch size. Maps to `--batch-size`.
  pub batch_size: Option<KnobValue<u32>>,
  /// Physical (ubatch) batch size. Maps to `--ubatch-size`.
  pub ubatch_size: Option<KnobValue<u32>>,
  /// RoPE frequency scaling factor. Maps to `--rope-freq-scale`.
  pub rope_freq_scale: Option<KnobValue<f32>>,
  /// Tokens to retain on context shift. Maps to `--keep`.
  pub keep: Option<KnobValue<u32>>,
  /// GPU device to target (`--device`). `None` lets llama-server
  /// auto-select (the default, which may split across all GPUs on
  /// Vulkan). When set, the value is a real `llama-server` device
  /// selector exactly as that binary's `--list-devices` reports it
  /// (`"Vulkan0"`, `"CUDA0"`, `"ROCm0"`) — sourced from the launch
  /// device catalog, never a bare index. The daemon spawns the binary
  /// that owns the selector (see [`crate::backend::llama_cpp::list_devices`]).
  pub device: Option<KnobValue<String>>,
  /// Proportional split of the model across multiple GPUs. Maps to
  /// `--tensor-split` (e.g. `"3,1"` puts 75% on GPU 0 and 25% on
  /// GPU 1). Forwarded verbatim; one comma-separated value per GPU.
  /// Only meaningful on multi-GPU hosts.
  pub tensor_split: Option<KnobValue<String>>,
  /// Primary GPU index that holds non-split tensors (and the KV cache
  /// under `split_mode = row`). Maps to `--main-gpu`. Only meaningful
  /// on multi-GPU hosts.
  pub main_gpu: Option<KnobValue<u32>>,
  /// How llama-server splits the model across GPUs. Maps to
  /// `--split-mode` (`none` = single GPU, `layer` = llama-server's
  /// default by-layer split, `row` = by-row split). Only meaningful
  /// on multi-GPU hosts.
  pub split_mode: Option<KnobValue<String>>,
}

/// A mutable reference to one [`TypedKnobs`] slot, tagged by its storage
/// kind. [`TypedKnobs::slot_mut`] is the single point where a
/// [`KnobField`] fans out to the heterogeneous fields, so every per-knob
/// writer routes through here and a new `KnobField` whose slot is left
/// unwired fails to compile (the `match` has no wildcard).
pub enum KnobSlotMut<'a> {
  U32(&'a mut Option<KnobValue<u32>>),
  F32(&'a mut Option<KnobValue<f32>>),
  Bool(&'a mut Option<KnobValue<bool>>),
  Str(&'a mut Option<KnobValue<String>>),
}

/// Shared read view of one knob slot; counterpart to [`KnobSlotMut`].
#[derive(Clone, Copy)]
pub enum KnobSlotRef<'a> {
  U32(&'a Option<KnobValue<u32>>),
  F32(&'a Option<KnobValue<f32>>),
  Bool(&'a Option<KnobValue<bool>>),
  Str(&'a Option<KnobValue<String>>),
}

impl<'a> KnobSlotRef<'a> {
  /// Any value present (Set or Auto) — the inverse of "inherited".
  pub fn is_some(self) -> bool {
    match self {
      Self::U32(s) => s.is_some(),
      Self::F32(s) => s.is_some(),
      Self::Bool(s) => s.is_some(),
      Self::Str(s) => s.is_some(),
    }
  }

  /// Slot explicitly delegated to `--fit`.
  pub fn is_auto(self) -> bool {
    match self {
      Self::U32(s) => s.is_auto(),
      Self::F32(s) => s.is_auto(),
      Self::Bool(s) => s.is_auto(),
      Self::Str(s) => s.is_auto(),
    }
  }

  /// Concrete `Set` value when the slot is the matching kind, else
  /// `None` (covers wrong-kind, Inherited, and Auto alike).
  pub fn as_u32(self) -> Option<u32> {
    match self {
      Self::U32(s) => s.set_value().copied(),
      _ => None,
    }
  }

  pub fn as_f32(self) -> Option<f32> {
    match self {
      Self::F32(s) => s.set_value().copied(),
      _ => None,
    }
  }

  pub fn as_bool(self) -> Option<bool> {
    match self {
      Self::Bool(s) => s.set_value().copied(),
      _ => None,
    }
  }

  pub fn as_str(self) -> Option<&'a str> {
    match self {
      Self::Str(s) => s.set_value().map(String::as_str),
      _ => None,
    }
  }
}

impl KnobSlotMut<'_> {
  /// Pin the slot to [`KnobValue::Auto`].
  pub fn set_auto(self) {
    match self {
      Self::U32(s) => *s = Some(KnobValue::Auto),
      Self::F32(s) => *s = Some(KnobValue::Auto),
      Self::Bool(s) => *s = Some(KnobValue::Auto),
      Self::Str(s) => *s = Some(KnobValue::Auto),
    }
  }

  /// Drop the slot back to Inherited (`None`).
  pub fn clear(self) {
    match self {
      Self::U32(s) => *s = None,
      Self::F32(s) => *s = None,
      Self::Bool(s) => *s = None,
      Self::Str(s) => *s = None,
    }
  }

  /// Write a concrete value (`Some` → `Set`, `None` → Inherited) when
  /// the slot is the matching kind; a no-op otherwise. The Auto state is
  /// set via [`Self::set_auto`], never here.
  pub fn set_u32(self, v: Option<u32>) {
    if let Self::U32(s) = self {
      *s = v.map(KnobValue::Set);
    }
  }

  pub fn set_f32(self, v: Option<f32>) {
    if let Self::F32(s) = self {
      *s = v.map(KnobValue::Set);
    }
  }

  pub fn set_bool(self, v: Option<bool>) {
    if let Self::Bool(s) = self {
      *s = v.map(KnobValue::Set);
    }
  }

  pub fn set_str(self, v: Option<String>) {
    if let Self::Str(s) = self {
      *s = v.map(KnobValue::Set);
    }
  }
}

impl TypedKnobs {
  /// Read view of the slot backing `field`. The sole `&self` fan-out
  /// from `KnobField` to the heterogeneous fields.
  pub fn slot(&self, field: KnobField) -> KnobSlotRef<'_> {
    use KnobField as F;
    match field {
      F::Ctx => KnobSlotRef::U32(&self.ctx),
      F::Reasoning => KnobSlotRef::Bool(&self.reasoning),
      F::NGpuLayers => KnobSlotRef::U32(&self.n_gpu_layers),
      F::NCpuMoe => KnobSlotRef::U32(&self.n_cpu_moe),
      F::Threads => KnobSlotRef::U32(&self.threads),
      F::CacheTypeK => KnobSlotRef::Str(&self.cache_type_k),
      F::CacheTypeV => KnobSlotRef::Str(&self.cache_type_v),
      F::FlashAttn => KnobSlotRef::Bool(&self.flash_attn),
      F::Mlock => KnobSlotRef::Bool(&self.mlock),
      F::NoMmap => KnobSlotRef::Bool(&self.no_mmap),
      F::Parallel => KnobSlotRef::U32(&self.parallel),
      F::BatchSize => KnobSlotRef::U32(&self.batch_size),
      F::UbatchSize => KnobSlotRef::U32(&self.ubatch_size),
      F::RopeFreqScale => KnobSlotRef::F32(&self.rope_freq_scale),
      F::Keep => KnobSlotRef::U32(&self.keep),
      F::Device => KnobSlotRef::Str(&self.device),
      F::TensorSplit => KnobSlotRef::Str(&self.tensor_split),
      F::MainGpu => KnobSlotRef::U32(&self.main_gpu),
      F::SplitMode => KnobSlotRef::Str(&self.split_mode),
    }
  }

  /// Mutable view of the slot backing `field`. The sole `&mut self`
  /// fan-out from `KnobField` to the heterogeneous fields.
  pub fn slot_mut(&mut self, field: KnobField) -> KnobSlotMut<'_> {
    use KnobField as F;
    match field {
      F::Ctx => KnobSlotMut::U32(&mut self.ctx),
      F::Reasoning => KnobSlotMut::Bool(&mut self.reasoning),
      F::NGpuLayers => KnobSlotMut::U32(&mut self.n_gpu_layers),
      F::NCpuMoe => KnobSlotMut::U32(&mut self.n_cpu_moe),
      F::Threads => KnobSlotMut::U32(&mut self.threads),
      F::CacheTypeK => KnobSlotMut::Str(&mut self.cache_type_k),
      F::CacheTypeV => KnobSlotMut::Str(&mut self.cache_type_v),
      F::FlashAttn => KnobSlotMut::Bool(&mut self.flash_attn),
      F::Mlock => KnobSlotMut::Bool(&mut self.mlock),
      F::NoMmap => KnobSlotMut::Bool(&mut self.no_mmap),
      F::Parallel => KnobSlotMut::U32(&mut self.parallel),
      F::BatchSize => KnobSlotMut::U32(&mut self.batch_size),
      F::UbatchSize => KnobSlotMut::U32(&mut self.ubatch_size),
      F::RopeFreqScale => KnobSlotMut::F32(&mut self.rope_freq_scale),
      F::Keep => KnobSlotMut::U32(&mut self.keep),
      F::Device => KnobSlotMut::Str(&mut self.device),
      F::TensorSplit => KnobSlotMut::Str(&mut self.tensor_split),
      F::MainGpu => KnobSlotMut::U32(&mut self.main_gpu),
      F::SplitMode => KnobSlotMut::Str(&mut self.split_mode),
    }
  }

  /// Layer `over` on top of `self`: every `Some` field in `over` wins,
  /// untouched fields keep `self`'s value. Used to apply per-invocation
  /// CLI overrides onto a preset baseline without wiping the preset's
  /// other knobs. The same `.or()` layering `crate::launch::defaults_table`
  /// builds on.
  pub fn overlay(&mut self, mut over: TypedKnobs) {
    for field in knob_specs().iter().map(|s| s.field) {
      overlay_slot(self.slot_mut(field), over.slot_mut(field));
    }
  }
}

/// `over` wins when present; otherwise `dst` keeps its value. Both slots
/// address the same `KnobField`, so the kinds always match.
fn overlay_slot(dst: KnobSlotMut<'_>, over: KnobSlotMut<'_>) {
  use KnobSlotMut::*;
  match (dst, over) {
    (U32(d), U32(o)) => {
      if o.is_some() {
        *d = o.take();
      }
    }
    (F32(d), F32(o)) => {
      if o.is_some() {
        *d = o.take();
      }
    }
    (Bool(d), Bool(o)) => {
      if o.is_some() {
        *d = o.take();
      }
    }
    (Str(d), Str(o)) => {
      if o.is_some() {
        *d = o.take();
      }
    }
    _ => unreachable!("a KnobField maps to exactly one slot kind"),
  }
}

/// One model-or-arch key's preset block in the config `presets:` map.
///
/// `entries` is keyed by preset **name** (a map, not a sequence) so the
/// comment-safe writer can `Add`/`Replace`/`Remove` one entry without
/// touching siblings. `default` names the entry the TUI cycle opens on;
/// it is hand-edited only (no CLI/TUI set-default op) and is ignored when
/// it names an absent entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ConfigPresetBlock {
  pub default: Option<String>,
  pub entries: BTreeMap<String, PresetBody>,
}

/// A single named preset's launch settings, as authored in `config.yaml`.
///
/// The typed knobs are flattened so `ctx: 65536` / `flash_attn: true` read
/// flat under the entry. `ctx` and `reasoning` are part of [`TypedKnobs`]
/// already, so they ride in `knobs` here (a `ctx: 65536` is
/// `knobs.ctx = Set(65536)`); materialisation pulls them into the
/// [`crate::launch::params::LaunchParams`] sibling fields so the IPC/CLI
/// wire shape is unchanged. `mode` (launch mode) and `extras` (the
/// free-form llama-server argv tail) are the only non-knob settings an
/// entry carries. Every field is optional — an entry only stores what it
/// pins.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PresetBody {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub mode: Option<LaunchMode>,
  #[serde(flatten)]
  pub knobs: TypedKnobs,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub extras: Option<Vec<String>>,
  /// Per-backend native knobs (see [`crate::launch::native_knobs`]), keyed by
  /// descriptor id. Parallel to `knobs`; empty for llama.cpp / Lemonade, so
  /// `skip_serializing_if` keeps a preset without native knobs byte-stable in
  /// `config.yaml`.
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub backend_knobs: BTreeMap<String, KnobValue<String>>,
  /// MTP intent (KD2's tri-state, not a `KnobValue`). Skipped when `Auto` so a
  /// preset that never touched MTP stays byte-stable in `config.yaml`.
  #[serde(
    default,
    skip_serializing_if = "crate::launch::params::MtpEnable::is_auto"
  )]
  pub mtp: crate::launch::params::MtpEnable,
  /// Draft tokens per speculation step; `None` leaves the backend on its own
  /// default. Rides with `mtp` because the two are only meaningful together.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub mtp_draft_n: Option<u32>,
}

/// How a knob *no layer supplied a value for* is seeded at launch
/// composition (R1 seeding rule). Selects only the seed for layer-less
/// knobs — knobs any layer set (user / last-used / arch / preset) keep
/// that value ("remembered values win").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultLaunchMode {
  /// Layer-less knobs seed to [`KnobValue::Auto`] — delegate placement
  /// to llama-server's `--fit`. Factory default.
  #[default]
  Auto,
  /// Layer-less knobs stay Inherited (`None`) and fall through to
  /// llama-server's own default — the pre-Auto behavior.
  Inherited,
}

/// GPU probe configuration — which vendor tools the daemon spawns
/// during initial and periodic hardware detection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct GpuConfig {
  /// When `true` (the default), the Vulkan fallback probe
  /// (`vulkaninfo -j` / `--summary`) runs at startup and on periodic
  /// re-probes. Set to `false` to skip it entirely — useful when a
  /// native NVIDIA/AMD/Metal probe already covers all GPUs and you
  /// don't want `vulkaninfo` spawning subprocesses.
  pub enable_vulkan_probe: bool,
  /// How often (in seconds) the daemon re-runs the full vendor probe
  /// chain to catch GPU hotplug, a late driver load, or a CpuOnly →
  /// detected transition. `0` disables periodic re-probes; the initial
  /// probe at daemon start always runs. Factory `60`.
  pub reprobe_interval_secs: u64,
}

impl GpuConfig {
  /// Full re-probe period, or `None` when periodic re-probes are off.
  pub fn reprobe_interval(&self) -> Option<Duration> {
    (self.reprobe_interval_secs > 0).then(|| Duration::from_secs(self.reprobe_interval_secs))
  }
}

impl Default for GpuConfig {
  fn default() -> Self {
    Self {
      enable_vulkan_probe: true,
      reprobe_interval_secs: 60,
    }
  }
}

impl Default for Config {
  fn default() -> Self {
    Self {
      theme: ThemeName::default(),
      custom_theme: None,
      model_paths: Vec::new(),
      disable_default_cache_paths: CachePathsConfig::default(),
      keybindings: BTreeMap::new(),
      gpu: GpuConfig::default(),
      daemon: DaemonConfig::default(),
      disable_scan: false,
      mouse_focus: false,
      arch_defaults: BTreeMap::new(),
      proxy: ProxyConfig::default(),
      backend: Default::default(),
      default_launch_mode: DefaultLaunchMode::default(),
      ascii_glyphs: false,
      left_pane_ratios: default_left_pane_ratios(),
      presets: BTreeMap::new(),
    }
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct CachePathsConfig {
  pub huggingface: bool,
  pub ollama: bool,
  pub lm_studio: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PortRange {
  pub start: u16,
  pub end: u16,
}

impl Default for PortRange {
  fn default() -> Self {
    // High, unprivileged, rarely claimed by common dev servers. Resolved
    // during planning (see plan Open Questions).
    Self {
      start: 41100,
      end: 41300,
    }
  }
}

/// Returned by `load_config_from_path`. `warning` is non-`None` when the
/// loader gracefully fell back to defaults but the user should be told why
/// (e.g. malformed YAML).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadedConfig {
  pub config: Config,
  pub warning: Option<String>,
  /// Legacy top-level keys that have moved under a nested block, as
  /// `(old_path, new_path)`. Non-fatal — the config still loads, but the
  /// old key does nothing, so the CLI says so once at startup.
  pub relocated_keys: Vec<(&'static str, &'static str)>,
}

/// Resolve which config file to load, given an optional CLI override, an
/// optional env override, and the directory `directories` would pick. Pure
/// function for testability — mirrors `kdash::config::config_path_from`.
///
/// Precedence: `--config` flag > `LLAMASTASH_CONFIG` env > XDG default. The
/// CLI is highest because users explicitly typed it; env beats the default
/// for the same reason.
pub fn config_path_from(
  cli_override: Option<PathBuf>,
  env_override: Option<OsString>,
  config_file: Option<PathBuf>,
) -> Option<PathBuf> {
  cli_override
    .or_else(|| {
      env_override
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
    })
    .or(config_file)
}

/// Resolve the active config-file path. Caller passes the optional
/// `--config` value parsed from the CLI; if it's `Some`, that wins.
pub fn config_path(cli_override: Option<PathBuf>) -> Option<PathBuf> {
  config_path_from(
    cli_override,
    env::var_os("LLAMASTASH_CONFIG"),
    user_config_file(),
  )
}

/// Top-level keys that moved into a nested block, and where they went.
///
/// The top-level `Config` tolerates unknown keys on purpose (forward
/// compat), which means a moved key is silently ignored and the user
/// quietly loses the setting. Naming them keeps a breaking rename from
/// being invisible.
const RELOCATED_KEYS: &[(&str, &str)] = &[
  ("port_range", "daemon.port_range"),
  ("probe_timeout_secs", "daemon.probe_timeout_secs"),
];

/// Legacy top-level keys still present in `contents`, as `(old, new)`.
fn relocated_keys(contents: &str) -> Vec<(&'static str, &'static str)> {
  let Ok(doc) = yaml_serde::from_str::<yaml_serde::Value>(contents) else {
    return Vec::new();
  };
  let Some(map) = doc.as_mapping() else {
    return Vec::new();
  };
  RELOCATED_KEYS
    .iter()
    .filter(|(old, _)| map.contains_key(yaml_serde::Value::String((*old).to_string())))
    .copied()
    .collect()
}

fn parse_config(contents: &str, path: &Path) -> LoadedConfig {
  match yaml_serde::from_str::<Config>(contents) {
    Ok(config) => LoadedConfig {
      config,
      warning: None,
      relocated_keys: relocated_keys(contents),
    },
    Err(error) => LoadedConfig {
      config: Config::default(),
      warning: Some(format!(
        "failed to parse config file {}: {}",
        path.display(),
        error
      )),
      relocated_keys: Vec::new(),
    },
  }
}

/// Load a YAML config from `path`. Missing files yield defaults with no
/// warning. Read or parse errors yield defaults plus a warning describing the
/// problem; the caller decides whether to surface-and-proceed or reject (the
/// CLI dispatcher rejects a malformed config for all but `init` / `doctor`).
///
/// Two adversarial mitigations sit between the path and the YAML parser:
/// 1. `fs::metadata` rejects anything that isn't a regular file — a config
///    path pointed at a FIFO or `/dev/urandom` would otherwise hang the main
///    thread.
/// 2. A 1 MiB size cap (`MAX_CONFIG_BYTES`) prevents `yaml_serde`'s
///    unbounded anchor/alias expansion from being weaponised by a hostile
///    config file.
pub fn load_config_from_path(path: &Path) -> LoadedConfig {
  match fs::metadata(path) {
    Ok(meta) => {
      if !meta.is_file() {
        return LoadedConfig {
          config: Config::default(),
          warning: Some(format!(
            "config path {} is not a regular file (named pipe, device, or directory)",
            path.display()
          )),
          relocated_keys: Vec::new(),
        };
      }
      if meta.len() > MAX_CONFIG_BYTES {
        return LoadedConfig {
          config: Config::default(),
          warning: Some(format!(
            "config file {} is {} bytes; exceeds the {}-byte cap",
            path.display(),
            meta.len(),
            MAX_CONFIG_BYTES
          )),
          relocated_keys: Vec::new(),
        };
      }
    }
    Err(error) if error.kind() == ErrorKind::NotFound => {
      return LoadedConfig::default();
    }
    Err(error) => {
      return LoadedConfig {
        config: Config::default(),
        warning: Some(format!(
          "failed to stat config file {}: {}",
          path.display(),
          error
        )),
        relocated_keys: Vec::new(),
      };
    }
  }
  match fs::read_to_string(path) {
    Ok(contents) => parse_config(&contents, path),
    Err(error) if error.kind() == ErrorKind::NotFound => LoadedConfig::default(),
    Err(error) => LoadedConfig {
      config: Config::default(),
      warning: Some(format!(
        "failed to read config file {}: {}",
        path.display(),
        error
      )),
      relocated_keys: Vec::new(),
    },
  }
}

/// Load the user's config, honoring the `--config` CLI override if supplied.
/// A non-`None` `warning` describes a present-but-malformed file; the caller
/// (the CLI dispatcher) decides whether to reject or surface-and-proceed.
pub fn load_config(cli_override: Option<PathBuf>) -> LoadedConfig {
  config_path(cli_override)
    .map(|path| load_config_from_path(&path))
    .unwrap_or_default()
}

/// Validate that we have *some* place to look for models. If scanning is
/// disabled and no user-supplied paths exist, llamastash would start with an
/// empty list and no path forward — a confusing dead-end. Surface it
/// early.
pub fn validate_scan_settings(
  disable_scan: bool,
  cli_paths: &[PathBuf],
  env_paths: &[PathBuf],
  config_paths: &[PathBuf],
) -> Result<(), ScanSettingsError> {
  if disable_scan && cli_paths.is_empty() && env_paths.is_empty() && config_paths.is_empty() {
    Err(ScanSettingsError::NoScanWithoutPaths)
  } else {
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanSettingsError {
  NoScanWithoutPaths,
}

impl std::fmt::Display for ScanSettingsError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NoScanWithoutPaths => write!(
        f,
        "scanning is disabled but no model paths were supplied via --model-path, \
         LLAMASTASH_MODEL_PATHS, or the `model_paths` config key — llamastash has nothing to list. \
         Provide at least one path or re-enable scanning."
      ),
    }
  }
}

impl std::error::Error for ScanSettingsError {}

/// Validate that `daemon.port_range` can yield a port. An inverted or
/// zero-start range binds nothing, so every launch would fail — but the
/// allocator is the only thing that checks, and it doesn't run until the
/// first `start`. Without this the daemon comes up healthy and the typo
/// surfaces much later as a launch failure that names no config key.
pub fn validate_port_range(range: &PortRange) -> Result<(), PortRangeError> {
  if range.start == 0 {
    return Err(PortRangeError::ZeroStart);
  }
  if range.end < range.start {
    return Err(PortRangeError::Inverted {
      start: range.start,
      end: range.end,
    });
  }
  Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortRangeError {
  ZeroStart,
  Inverted { start: u16, end: u16 },
}

impl std::fmt::Display for PortRangeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::ZeroStart => write!(
        f,
        "`daemon.port_range.start` is 0, which is not a bindable port — \
         set it to the first port llamastash may launch a model on \
         (factory: 41100)."
      ),
      Self::Inverted { start, end } => write!(
        f,
        "`daemon.port_range` is inverted: start {start} is above end {end}, \
         so no port can be allocated. Swap them, or use the factory range \
         41100-41300."
      ),
    }
  }
}

impl std::error::Error for PortRangeError {}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
  };

  use super::*;

  #[test]
  fn sanitize_left_pane_ratios_caps_clamps_and_defaults() {
    // Factory default when unset/empty.
    assert_eq!(sanitize_left_pane_ratios(&[]), default_left_pane_ratios());
    // A valid override passes through verbatim (order + dupes preserved).
    assert_eq!(sanitize_left_pane_ratios(&[40, 40, 80]), vec![40, 40, 80]);
    // At most 5 slots — the 6th+ are ignored.
    assert_eq!(
      sanitize_left_pane_ratios(&[10, 20, 30, 40, 50, 60, 70]),
      vec![10, 20, 30, 40, 50]
    );
    // Each slot clamps to 0..=100 (150 -> 100).
    assert_eq!(sanitize_left_pane_ratios(&[150, 0, 100]), vec![100, 0, 100]);
  }

  #[test]
  fn preset_body_deserialises_flattened_knobs() {
    // The serde-flatten + KnobValue (untagged) combination is a known
    // footgun; pin that ctx/reasoning/knobs flatten flat, integers stay
    // integers, the Auto sentinel round-trips, and `mode` stays a sibling.
    let body: PresetBody = yaml_serde::from_str(
      "ctx: 65536\nreasoning: true\nmode: embedding\nflash_attn: true\nn_gpu_layers: { auto: true }\nthreads: 8\nextras: [--rope-freq-base, \"10000\"]\n",
    )
    .unwrap();
    assert_eq!(body.mode, Some(LaunchMode::Embedding));
    assert_eq!(body.knobs.ctx, Some(KnobValue::Set(65536)));
    assert_eq!(body.knobs.reasoning, Some(KnobValue::Set(true)));
    assert_eq!(body.knobs.flash_attn, Some(KnobValue::Set(true)));
    assert_eq!(body.knobs.threads, Some(KnobValue::Set(8)));
    assert_eq!(body.knobs.n_gpu_layers, Some(KnobValue::Auto));
    assert_eq!(
      body.extras.as_deref(),
      Some(&["--rope-freq-base".to_string(), "10000".to_string()][..])
    );
  }

  #[test]
  fn preset_body_serialises_back_to_a_flat_mapping() {
    let body = PresetBody {
      mode: None,
      knobs: TypedKnobs {
        ctx: Some(KnobValue::Set(32768)),
        flash_attn: Some(KnobValue::Set(true)),
        n_gpu_layers: Some(KnobValue::Auto),
        ..TypedKnobs::default()
      },
      extras: None,
      backend_knobs: Default::default(),
      ..PresetBody::default()
    };
    let value = serde_json::to_value(&body).unwrap();
    let obj = value.as_object().unwrap();
    assert_eq!(
      obj.get("ctx").and_then(serde_json::Value::as_u64),
      Some(32768)
    );
    assert_eq!(
      obj.get("flash_attn").and_then(serde_json::Value::as_bool),
      Some(true)
    );
    assert_eq!(
      obj.get("n_gpu_layers").and_then(serde_json::Value::as_str),
      Some("auto"),
      "Auto serialises as the bare `auto` token through flatten"
    );
    assert!(obj.get("mode").is_none(), "None siblings are skipped");
    assert!(obj.get("extras").is_none());
    assert!(
      obj.get("backend_knobs").is_none(),
      "empty backend_knobs is skipped → byte-stable preset shape"
    );
  }

  #[test]
  fn preset_body_carries_backend_knobs_round_trip() {
    // A preset with native knobs round-trips through YAML; the bare value
    // is `Set`, the `auto` token is `Auto`.
    let body: PresetBody = yaml_serde::from_str(
      "ctx: 8192\nbackend_knobs:\n  kv_disk_dir: \"/tmp/kv\"\n  quality: auto\n",
    )
    .unwrap();
    assert_eq!(body.knobs.ctx, Some(KnobValue::Set(8192)));
    assert_eq!(
      body.backend_knobs.get("kv_disk_dir"),
      Some(&KnobValue::Set("/tmp/kv".to_string()))
    );
    assert_eq!(body.backend_knobs.get("quality"), Some(&KnobValue::Auto));
    // Re-serialise and read back: equal.
    let yaml = yaml_serde::to_string(&body).unwrap();
    assert!(yaml.contains("backend_knobs"));
    let back: PresetBody = yaml_serde::from_str(&yaml).unwrap();
    assert_eq!(back, body);
  }

  #[test]
  fn config_presets_block_round_trips_through_yaml() {
    let yaml = "\
presets:
  qwen-coder:
    default: long-ctx
    entries:
      short-ctx: { ctx: 8192 }
      long-ctx: { ctx: 65536, flash_attn: true }
  qwen2:
    entries:
      balanced: { ctx: 16384 }
";
    let cfg: Config = yaml_serde::from_str(yaml).unwrap();
    let block = cfg.presets.get("qwen-coder").unwrap();
    assert_eq!(block.default.as_deref(), Some("long-ctx"));
    assert_eq!(block.entries.len(), 2);
    let long = block.entries.get("long-ctx").unwrap();
    assert_eq!(long.knobs.ctx, Some(KnobValue::Set(65536)));
    assert_eq!(long.knobs.flash_attn, Some(KnobValue::Set(true)));
    let arch = cfg.presets.get("qwen2").unwrap();
    assert!(arch.default.is_none());
    assert_eq!(
      arch.entries.get("balanced").unwrap().knobs.ctx,
      Some(KnobValue::Set(16384))
    );
  }

  #[test]
  fn config_without_presets_key_defaults_to_empty() {
    let cfg: Config = yaml_serde::from_str("theme: latte\n").unwrap();
    assert!(cfg.presets.is_empty());
  }

  #[test]
  fn field_name_matches_the_serde_keys_exactly() {
    // The Settings label and any field-name display read `field_name()`;
    // persistence reads the serde key. They must be the same string set,
    // both directions — a renamed serde field or a stale `field_name()`
    // arm fails here rather than silently mislabelling a saved knob.
    use std::collections::BTreeSet;
    let value = serde_json::to_value(TypedKnobs::default()).unwrap();
    let serde_keys: BTreeSet<&str> = value
      .as_object()
      .unwrap()
      .keys()
      .map(String::as_str)
      .collect();
    let field_names: BTreeSet<&str> = knob_specs().iter().map(|s| s.field.field_name()).collect();
    assert_eq!(
      field_names, serde_keys,
      "KnobField::field_name() must match the TypedKnobs serde keys exactly"
    );
  }

  #[test]
  fn overlay_takes_each_present_field_from_over_else_keeps_self() {
    // Representative (over, under) pair across every slot kind, including
    // a String knob (the take-vs-copy hazard the slot accessor unifies).
    let mut base = TypedKnobs {
      ctx: Some(KnobValue::Set(2048)),  // both set → over wins
      threads: Some(KnobValue::Set(4)), // only base set → survives
      cache_type_k: Some(KnobValue::Set("q4_0".into())), // both set → over wins
      device: Some(KnobValue::Set("CUDA0".into())), // only base set → survives
      flash_attn: Some(KnobValue::Set(false)), // both set → over wins
      ..TypedKnobs::default()
    };
    let over = TypedKnobs {
      ctx: Some(KnobValue::Set(8192)),
      cache_type_k: Some(KnobValue::Set("q8_0".into())),
      flash_attn: Some(KnobValue::Set(true)),
      n_gpu_layers: Some(KnobValue::Auto), // only over set → applied
      ..TypedKnobs::default()
    };
    base.overlay(over);
    assert_eq!(base.ctx, Some(KnobValue::Set(8192)));
    assert_eq!(base.threads, Some(KnobValue::Set(4)));
    assert_eq!(base.cache_type_k, Some(KnobValue::Set("q8_0".into())));
    assert_eq!(base.device, Some(KnobValue::Set("CUDA0".into())));
    assert_eq!(base.flash_attn, Some(KnobValue::Set(true)));
    assert_eq!(base.n_gpu_layers, Some(KnobValue::Auto));
  }

  #[test]
  fn knob_value_set_serialises_as_bare_scalar() {
    // The bare-scalar shape is the back-compat contract: a `Set` must
    // serialise exactly as the old `Option<T>` value did.
    assert_eq!(
      serde_json::to_string(&KnobValue::Set(8192u32)).unwrap(),
      "8192"
    );
    assert_eq!(
      serde_json::to_string(&KnobValue::Set(true)).unwrap(),
      "true"
    );
    assert_eq!(
      serde_json::to_string(&KnobValue::Set("q8_0".to_string())).unwrap(),
      "\"q8_0\""
    );
  }

  #[test]
  fn knob_value_auto_serialises_as_bare_token() {
    assert_eq!(
      serde_json::to_string(&KnobValue::<u32>::Auto).unwrap(),
      "\"auto\""
    );
  }

  #[test]
  fn knob_value_round_trips_every_kind() {
    // Set value, Auto token, and the legacy sentinel (read-only).
    for json in ["8192", "\"auto\""] {
      let v: KnobValue<u32> = serde_json::from_str(json).unwrap();
      let back = serde_json::to_string(&v).unwrap();
      assert_eq!(back, json, "u32 round-trip for {json}");
    }
    let set: KnobValue<u32> = serde_json::from_str("99").unwrap();
    assert_eq!(set, KnobValue::Set(99));
    let auto: KnobValue<bool> = serde_json::from_str("\"auto\"").unwrap();
    assert_eq!(auto, KnobValue::Auto);
    // Legacy `{ auto: true }` still loads.
    let legacy: KnobValue<bool> = serde_json::from_str("{\"auto\":true}").unwrap();
    assert_eq!(legacy, KnobValue::Auto);
  }

  #[test]
  fn bare_auto_token_is_the_auto_state() {
    // The bare token `auto` denotes the Auto state, on any knob type.
    let v: KnobValue<u32> = serde_json::from_str("\"auto\"").unwrap();
    assert_eq!(v, KnobValue::Auto);
    assert_eq!(
      serde_json::to_string(&KnobValue::<u32>::Auto).unwrap(),
      "\"auto\""
    );
    // Legacy `{ auto: true }` is still read as Auto (lenient).
    let legacy: KnobValue<u32> = serde_json::from_str(r#"{"auto":true}"#).unwrap();
    assert_eq!(legacy, KnobValue::Auto);
  }

  #[test]
  fn literal_auto_value_uses_the_value_escape() {
    // To set a knob to the *literal* string "auto" (not the Auto state),
    // use the `{ value: auto }` escape — which is also how it serialises.
    let escaped: KnobValue<String> = serde_json::from_str(r#"{"value":"auto"}"#).unwrap();
    assert_eq!(escaped, KnobValue::Set("auto".to_string()));
    assert_eq!(
      serde_json::to_string(&KnobValue::Set("auto".to_string())).unwrap(),
      r#"{"value":"auto"}"#
    );

    // Full TypedKnobs round-trip: a string knob set to "auto" stays Set,
    // distinct from a sibling delegated to Auto.
    let knobs = TypedKnobs {
      split_mode: Some(KnobValue::Set("auto".to_string())),
      device: Some(KnobValue::Auto),
      ..TypedKnobs::default()
    };
    let s = serde_json::to_string(&knobs).unwrap();
    let back: TypedKnobs = serde_json::from_str(&s).unwrap();
    assert_eq!(back.split_mode, Some(KnobValue::Set("auto".to_string())));
    assert_eq!(back.device, Some(KnobValue::Auto));
    // A non-"auto" string value still serialises bare (no escape).
    assert_eq!(
      serde_json::to_string(&KnobValue::Set("q8_0".to_string())).unwrap(),
      r#""q8_0""#
    );
  }

  #[test]
  fn typed_knobs_tri_state_round_trips_through_json_and_yaml() {
    let knobs = TypedKnobs {
      ctx: Some(KnobValue::Set(16384)),
      n_gpu_layers: Some(KnobValue::Auto),
      flash_attn: None,
      cache_type_k: Some(KnobValue::Set("q8_0".to_string())),
      ..TypedKnobs::default()
    };
    let json = serde_json::to_string(&knobs).unwrap();
    assert_eq!(serde_json::from_str::<TypedKnobs>(&json).unwrap(), knobs);
    let yaml = yaml_serde::to_string(&knobs).unwrap();
    assert_eq!(yaml_serde::from_str::<TypedKnobs>(&yaml).unwrap(), knobs);
  }

  #[test]
  fn old_typed_knobs_file_with_bare_scalars_loads_as_set() {
    // A pre-tri-state state.json / config.yaml carries bare scalars and
    // omits unset fields. It must load unchanged: bare scalar → Set,
    // absent → None. (Relies on TypedKnobs not setting
    // `deny_unknown_fields`.)
    let old = r#"{"ctx": 8192, "n_gpu_layers": 99, "cache_type_k": "q8_0"}"#;
    let k: TypedKnobs = serde_json::from_str(old).unwrap();
    assert_eq!(k.ctx, Some(KnobValue::Set(8192)));
    assert_eq!(k.n_gpu_layers, Some(KnobValue::Set(99)));
    assert_eq!(k.cache_type_k, Some(KnobValue::Set("q8_0".to_string())));
    assert_eq!(k.flash_attn, None);
  }

  fn temp_test_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("system time should be after epoch")
      .as_nanos();
    let path = env::temp_dir().join(format!(
      "llamastash-config-tests-{}-{}-{}",
      name,
      std::process::id(),
      suffix
    ));
    fs::create_dir_all(&path).expect("temp test dir should be created");
    path
  }

  #[test]
  fn config_path_from_prefers_env_override() {
    let path = config_path_from(
      None,
      Some(OsString::from("/tmp/custom.yaml")),
      Some(PathBuf::from("/tmp/ignored.yaml")),
    );
    assert_eq!(path, Some(PathBuf::from("/tmp/custom.yaml")));
  }

  #[test]
  fn config_path_from_falls_back_to_xdg() {
    let path = config_path_from(
      None,
      None,
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml")),
    );
    assert_eq!(
      path,
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml"))
    );
  }

  #[test]
  fn config_path_from_ignores_empty_env_value() {
    let path = config_path_from(
      None,
      Some(OsString::new()),
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml")),
    );
    assert_eq!(
      path,
      Some(PathBuf::from("/home/u/.config/llamastash/config.yaml"))
    );
  }

  #[test]
  fn config_path_from_returns_none_when_all_sources_absent() {
    assert_eq!(config_path_from(None, None, None), None);
  }

  #[test]
  fn config_path_from_cli_override_beats_env_and_xdg() {
    let path = config_path_from(
      Some(PathBuf::from("/tmp/from-cli.yaml")),
      Some(OsString::from("/tmp/from-env.yaml")),
      Some(PathBuf::from("/tmp/from-xdg.yaml")),
    );
    assert_eq!(path, Some(PathBuf::from("/tmp/from-cli.yaml")));
  }

  #[test]
  fn config_path_from_env_beats_xdg_when_cli_absent() {
    let path = config_path_from(
      None,
      Some(OsString::from("/tmp/from-env.yaml")),
      Some(PathBuf::from("/tmp/from-xdg.yaml")),
    );
    assert_eq!(path, Some(PathBuf::from("/tmp/from-env.yaml")));
  }

  #[test]
  fn load_config_from_path_reads_valid_yaml() {
    let dir = temp_test_dir("valid");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
theme: latte
disable_scan: false
model_paths:
  - /home/u/models
  - /mnt/storage/gguf
disable_default_cache_paths:
  ollama: true
daemon:
  port_range:
    start: 50000
    end: 50100
keybindings:
  quit: ctrl+q
",
    )
    .expect("config fixture should be written");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    assert_eq!(loaded.config.theme, ThemeName::Latte);
    assert_eq!(
      loaded.config.model_paths,
      vec![
        PathBuf::from("/home/u/models"),
        PathBuf::from("/mnt/storage/gguf"),
      ]
    );
    assert!(loaded.config.disable_default_cache_paths.ollama);
    assert!(!loaded.config.disable_default_cache_paths.huggingface);
    assert!(!loaded.config.disable_default_cache_paths.lm_studio);
    assert_eq!(
      loaded.config.daemon.port_range,
      PortRange {
        start: 50000,
        end: 50100
      }
    );
    assert_eq!(
      loaded.config.keybindings.get("quit"),
      Some(&"ctrl+q".to_string())
    );

    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_missing_file_returns_defaults_silently() {
    let dir = temp_test_dir("missing");
    let path = dir.join("missing.yaml");
    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    assert!(loaded.warning.is_none());
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_malformed_yaml_uses_defaults_with_warning() {
    let dir = temp_test_dir("malformed");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: latte\ndaemon: not-a-mapping").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("malformed YAML must surface a warning");
    assert!(
      warning.contains("failed to parse config file"),
      "warning should name the failure: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_unknown_theme_surfaces_warning() {
    let dir = temp_test_dir("unknown_theme");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: dracula\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("unknown theme must surface a warning");
    assert!(
      warning.contains("dracula"),
      "warning should name the bad value: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn load_config_from_path_partial_config_uses_defaults_for_unset_fields() {
    let dir = temp_test_dir("partial");
    let path = dir.join("config.yaml");
    fs::write(&path, "theme: gruvbox-dark\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none());
    assert_eq!(loaded.config.theme, ThemeName::GruvboxDark);
    assert_eq!(loaded.config.daemon.port_range, PortRange::default());
    assert!(loaded.config.model_paths.is_empty());
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn validate_port_range_accepts_usable_ranges() {
    assert!(validate_port_range(&PortRange::default()).is_ok());
    // A one-port range is legitimate — pinning every launch to one port.
    assert!(validate_port_range(&PortRange {
      start: 41100,
      end: 41100
    })
    .is_ok());
    assert!(validate_port_range(&PortRange {
      start: 1,
      end: u16::MAX
    })
    .is_ok());
  }

  /// The allocator rejects these, but not until the first launch — by
  /// which point nothing points back at `config.yaml`.
  #[test]
  fn validate_port_range_rejects_an_inverted_range() {
    let err = validate_port_range(&PortRange {
      start: 46000,
      end: 45000,
    })
    .expect_err("an inverted range allocates nothing");
    assert_eq!(
      err,
      PortRangeError::Inverted {
        start: 46000,
        end: 45000
      }
    );
    let msg = err.to_string();
    assert!(
      msg.contains("daemon.port_range"),
      "the message must name the config key, got: {msg}"
    );
  }

  #[test]
  fn validate_port_range_rejects_a_zero_start() {
    let err = validate_port_range(&PortRange { start: 0, end: 0 })
      .expect_err("port 0 is not bindable as a range start");
    assert_eq!(err, PortRangeError::ZeroStart);
    assert!(err.to_string().contains("daemon.port_range.start"));
  }

  /// The top-level parse tolerates unknown keys, so a moved key would
  /// otherwise be dropped in silence and the user would just lose the
  /// setting.
  #[test]
  fn a_legacy_top_level_key_is_reported_as_relocated() {
    let dir = temp_test_dir("relocated-keys");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "theme: latte\nport_range:\n  start: 46000\n  end: 46100\nprobe_timeout_secs: 600\n",
    )
    .expect("write failed");

    let loaded = load_config_from_path(&path);
    assert!(loaded.warning.is_none(), "a moved key must not be fatal");
    assert_eq!(
      loaded.relocated_keys,
      vec![
        ("port_range", "daemon.port_range"),
        ("probe_timeout_secs", "daemon.probe_timeout_secs"),
      ]
    );
    // Still ignored, as the forward-compat contract says — the point is
    // that the user is told.
    assert_eq!(loaded.config.daemon.port_range, PortRange::default());
    assert_eq!(loaded.config.theme, ThemeName::Latte);
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn a_config_using_the_new_nesting_reports_nothing_relocated() {
    let dir = temp_test_dir("no-relocated-keys");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "daemon:\n  port_range:\n    start: 46000\n    end: 46100\n",
    )
    .expect("write failed");

    let loaded = load_config_from_path(&path);
    assert!(loaded.relocated_keys.is_empty());
    assert_eq!(loaded.config.daemon.port_range.start, 46000);
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  /// These blocks are brand-new key names, so a typo is otherwise an
  /// undetectable no-op. Matches the `proxy:` posture.
  #[test]
  fn a_typo_inside_the_gpu_or_daemon_block_is_rejected() {
    for body in [
      "gpu:\n  enable_vulcan_probe: false\n",
      "daemon:\n  idle_timout_secs: 60\n",
    ] {
      let dir = temp_test_dir("block-typo");
      let path = dir.join("config.yaml");
      fs::write(&path, body).expect("write failed");

      let loaded = load_config_from_path(&path);
      let warning = loaded
        .warning
        .unwrap_or_else(|| panic!("a typo in {body:?} must be rejected, not ignored"));
      assert!(
        warning.contains("unknown field"),
        "warning should name the unknown field, got: {warning}"
      );
      fs::remove_dir_all(dir).expect("temp test dir should be removed");
    }
  }

  #[test]
  fn gpu_and_daemon_blocks_deserialize_from_yaml() {
    let dir = temp_test_dir("gpu-daemon-blocks");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
gpu:
  enable_vulkan_probe: false
  reprobe_interval_secs: 300
daemon:
  port_range:
    start: 42000
    end: 42010
  probe_timeout_secs: 600
  idle_timeout_secs: 1800
  metrics_interval_secs: 10
",
    )
    .expect("config fixture should be written");

    let cfg = load_config_from_path(&path).config;
    assert!(!cfg.gpu.enable_vulkan_probe);
    assert_eq!(cfg.gpu.reprobe_interval(), Some(Duration::from_secs(300)));
    assert_eq!(cfg.daemon.port_range.start, 42000);
    assert_eq!(cfg.daemon.probe_timeout_secs, 600);
    assert_eq!(cfg.daemon.idle_timeout(), Some(Duration::from_secs(1800)));
    assert_eq!(cfg.daemon.metrics_interval(), Duration::from_secs(10));
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  /// A partially-specified block keeps the factory value for every key
  /// the user didn't write.
  #[test]
  fn a_partial_daemon_block_keeps_factory_values_for_the_rest() {
    let dir = temp_test_dir("partial-daemon-block");
    let path = dir.join("config.yaml");
    fs::write(&path, "daemon:\n  idle_timeout_secs: 60\n").expect("write failed");

    let cfg = load_config_from_path(&path).config;
    assert_eq!(cfg.daemon.idle_timeout(), Some(Duration::from_secs(60)));
    assert_eq!(cfg.daemon.port_range, PortRange::default());
    assert_eq!(cfg.daemon.probe_timeout_secs, 120);
    assert_eq!(cfg.daemon.metrics_interval(), Duration::from_secs(1));
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn absent_gpu_and_daemon_blocks_fall_back_to_factory() {
    let cfg = Config::default();
    assert!(cfg.gpu.enable_vulkan_probe);
    assert_eq!(cfg.gpu.reprobe_interval(), Some(Duration::from_secs(60)));
    assert_eq!(cfg.daemon.idle_timeout(), None);
    assert_eq!(cfg.daemon.metrics_interval(), Duration::from_secs(1));
  }

  #[test]
  fn a_zero_reprobe_or_idle_value_disables_that_timer() {
    let gpu = GpuConfig {
      reprobe_interval_secs: 0,
      ..Default::default()
    };
    assert_eq!(gpu.reprobe_interval(), None);
    let daemon = DaemonConfig {
      idle_timeout_secs: 0,
      ..Default::default()
    };
    assert_eq!(daemon.idle_timeout(), None);
  }

  /// Unlike the two timers, `0` here is a reset rather than an off
  /// switch — a zero-length tick would busy-loop the sampler.
  #[test]
  fn metrics_interval_clamps_into_range() {
    let at_zero = DaemonConfig {
      metrics_interval_secs: 0,
      ..Default::default()
    };
    assert_eq!(at_zero.metrics_interval(), Duration::from_secs(1));
    let too_slow = DaemonConfig {
      metrics_interval_secs: u64::MAX,
      ..Default::default()
    };
    assert_eq!(too_slow.metrics_interval(), Duration::from_secs(60));
  }

  #[test]
  fn default_config_uses_macchiato_and_default_port_range() {
    let cfg = Config::default();
    assert_eq!(cfg.theme, ThemeName::Macchiato);
    assert_eq!(
      cfg.daemon.port_range,
      PortRange {
        start: 41100,
        end: 41300
      }
    );
    assert!(!cfg.disable_scan);
  }

  #[test]
  fn validate_scan_settings_errors_when_disabled_with_no_paths() {
    let result = validate_scan_settings(true, &[], &[], &[]);
    assert_eq!(result, Err(ScanSettingsError::NoScanWithoutPaths));
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("scanning is disabled"), "{msg}");
    assert!(msg.contains("--model-path"), "{msg}");
  }

  #[test]
  fn validate_scan_settings_ok_when_paths_supplied_via_any_source() {
    assert!(validate_scan_settings(true, &[PathBuf::from("/a")], &[], &[]).is_ok());
    assert!(validate_scan_settings(true, &[], &[PathBuf::from("/b")], &[]).is_ok());
    assert!(validate_scan_settings(true, &[], &[], &[PathBuf::from("/c")]).is_ok());
  }

  #[test]
  fn validate_scan_settings_ok_when_scan_enabled() {
    assert!(validate_scan_settings(false, &[], &[], &[]).is_ok());
  }

  #[test]
  fn load_config_from_path_rejects_oversized_file_with_warning() {
    let dir = temp_test_dir("oversize");
    let path = dir.join("config.yaml");
    // Write 1 MiB + 1 byte of valid YAML so the size cap, not the YAML
    // parser, is what trips the warning.
    let mut content = String::from("theme: latte\nkeybindings:\n");
    while content.len() <= MAX_CONFIG_BYTES as usize {
      content.push_str("  filler_key_filler_key_filler_key: 'pad pad pad pad pad'\n");
    }
    fs::write(&path, &content).expect("oversize fixture should write");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("oversized config must surface a warning");
    assert!(
      warning.contains("exceeds") && warning.contains("cap"),
      "warning should name the cap, got: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn arch_defaults_round_trip_through_yaml() {
    let dir = temp_test_dir("arch-defaults");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
theme: latte
arch_defaults:
  qwen2:
    n_gpu_layers: 99
    flash_attn: true
    cache_type_k: q8_0
    cache_type_v: q8_0
  llama:
    threads: 8
    parallel: 4
",
    )
    .expect("config fixture should be written");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    let qwen2 = loaded
      .config
      .arch_defaults
      .get("qwen2")
      .expect("qwen2 entry present");
    assert_eq!(qwen2.n_gpu_layers, Some(KnobValue::Set(99)));
    assert_eq!(qwen2.flash_attn, Some(KnobValue::Set(true)));
    assert_eq!(
      qwen2.cache_type_k.set_value().map(String::as_str),
      Some("q8_0")
    );
    assert_eq!(
      qwen2.cache_type_v.set_value().map(String::as_str),
      Some("q8_0")
    );
    let llama = loaded
      .config
      .arch_defaults
      .get("llama")
      .expect("llama entry present");
    assert_eq!(llama.threads, Some(KnobValue::Set(8)));
    assert_eq!(llama.parallel, Some(KnobValue::Set(4)));
    assert!(
      llama.n_gpu_layers.is_none(),
      "partial entry leaves rest None"
    );

    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn arch_defaults_absent_defaults_to_empty_map() {
    let cfg = Config::default();
    assert!(cfg.arch_defaults.is_empty());
  }

  #[test]
  fn llama_server_paths_round_trip_through_yaml() {
    let dir = temp_test_dir("llama-server-paths");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
backend:
  llamacpp:
    servers:
      - binary: /opt/builds/vulkan/llama-server
      - binary: /opt/builds/cuda/llama-server
        name: cuda
      - binary: /opt/builds/rocm/llama-server
",
    )
    .expect("config fixture should be written");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    // First entry is the primary (default) binary.
    assert_eq!(
      loaded.config.backend.llamacpp.primary_binary(),
      Some(PathBuf::from("/opt/builds/vulkan/llama-server"))
    );
    assert_eq!(
      loaded.config.backend.llamacpp.extra_binaries(),
      vec![
        PathBuf::from("/opt/builds/cuda/llama-server"),
        PathBuf::from("/opt/builds/rocm/llama-server"),
      ]
    );
    // The optional per-server name round-trips.
    assert_eq!(
      loaded.config.backend.llamacpp.servers[1].name.as_deref(),
      Some("cuda")
    );
  }

  #[test]
  fn llama_server_paths_absent_defaults_to_empty_vec() {
    let cfg = Config::default();
    assert!(cfg.backend.llamacpp.servers.is_empty());
  }

  #[test]
  fn proxy_config_defaults_match_plan() {
    let cfg = Config::default();
    assert!(cfg.proxy.enabled);
    assert!(!cfg.proxy.ollama_compat);
    assert_eq!(cfg.proxy.port, None);
    // Resolved port follows the mode: 11435 in default mode, 11434
    // when ollama-compat is enabled.
    assert_eq!(cfg.proxy.effective_port(), 11435);
    let compat = ProxyConfig {
      ollama_compat: true,
      ..ProxyConfig::default()
    };
    assert_eq!(compat.effective_port(), 11434);
    // An explicit `port:` override wins over the mode default in
    // either mode.
    let pinned = ProxyConfig {
      port: Some(20000),
      ollama_compat: true,
      ..ProxyConfig::default()
    };
    assert_eq!(pinned.effective_port(), 20000);
  }

  #[test]
  fn proxy_config_round_trips_through_yaml() {
    let dir = temp_test_dir("proxy-config");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      r"
theme: latte
proxy:
  enabled: false
  port: 13579
",
    )
    .expect("config fixture should be written");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    assert!(!loaded.config.proxy.enabled);
    assert_eq!(loaded.config.proxy.port, Some(13579));
    assert_eq!(loaded.config.proxy.effective_port(), 13579);
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn proxy_config_partial_inherits_remaining_defaults() {
    let dir = temp_test_dir("proxy-partial");
    let path = dir.join("config.yaml");
    fs::write(&path, "proxy:\n  port: 22222\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none());
    // `enabled` and `ollama_compat` keep their defaults when only
    // `port` is supplied.
    assert!(loaded.config.proxy.enabled);
    assert!(!loaded.config.proxy.ollama_compat);
    assert_eq!(loaded.config.proxy.port, Some(22222));
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn proxy_host_and_auth_round_trip_through_yaml() {
    let dir = temp_test_dir("proxy-lan-auth");
    let path = dir.join("config.yaml");
    fs::write(
      &path,
      "proxy:\n  host: 0.0.0.0\n  api_key: sk-llamastash-testkey\n  insecure_no_auth: true\n",
    )
    .expect("write failed");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none(), "valid config should not warn");
    let p = &loaded.config.proxy;
    assert_eq!(p.host, Some("0.0.0.0".parse().unwrap()));
    assert_eq!(p.effective_host(), "0.0.0.0".parse::<IpAddr>().unwrap());
    assert!(!p.effective_host().is_loopback());
    assert_eq!(p.api_key.as_deref(), Some("sk-llamastash-testkey"));
    assert!(p.auth_enforced());
    assert!(p.insecure_no_auth);
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn proxy_host_accepts_ipv6() {
    let dir = temp_test_dir("proxy-ipv6");
    let path = dir.join("config.yaml");
    fs::write(&path, "proxy:\n  host: \"::\"\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none());
    assert_eq!(loaded.config.proxy.host, Some("::".parse().unwrap()));
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn proxy_host_and_auth_default_to_loopback_no_key() {
    // Absent host/api_key keep the historical loopback, keyless
    // posture — an old config (no new keys) is unchanged.
    let p = ProxyConfig::default();
    assert_eq!(p.host, None);
    assert_eq!(p.effective_host(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(p.effective_host().is_loopback());
    assert_eq!(p.api_key, None);
    assert!(!p.auth_enforced());
    assert!(!p.insecure_no_auth);
  }

  #[test]
  fn proxy_config_ollama_compat_flips_default_port() {
    let dir = temp_test_dir("proxy-ollama-compat");
    let path = dir.join("config.yaml");
    fs::write(&path, "proxy:\n  ollama_compat: true\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert!(loaded.warning.is_none());
    assert!(loaded.config.proxy.enabled);
    assert!(loaded.config.proxy.ollama_compat);
    // `port: None` resolves to 11434 in compat mode (`Ollama is
    // running` handshake target), not 11435 (the default-mode value).
    assert_eq!(loaded.config.proxy.port, None);
    assert_eq!(loaded.config.proxy.effective_port(), 11434);
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn lemonade_is_off_by_default_and_parses_when_enabled() {
    // Missing `backend.lemonade:` section → opt-in default (off), no warning.
    let dir = temp_test_dir("lemonade-default");
    let path = dir.join("config.yaml");
    fs::write(&path, "{}\n").expect("write failed");
    let loaded = load_config_from_path(&path);
    assert!(loaded.warning.is_none());
    assert_eq!(
      loaded.config.backend.lemonade.enabled, None,
      "lemonade `enabled` defaults to unset (on-when-found intent, like ds4)"
    );
    assert!(loaded.config.backend.lemonade.servers.is_empty());
    assert_eq!(loaded.config.backend.lemonade.port, 13305);
    fs::remove_dir_all(dir).expect("temp test dir should be removed");

    // Explicit enable + user-provided binary path round-trips.
    let on_dir = temp_test_dir("lemonade-on");
    let on_path = on_dir.join("config.yaml");
    fs::write(
      &on_path,
      "backend:\n  lemonade:\n    enabled: true\n    servers:\n      - binary: /opt/lemonade/lemond\n",
    )
    .expect("write failed");
    let on_loaded = load_config_from_path(&on_path);
    assert!(on_loaded.warning.is_none());
    assert_eq!(on_loaded.config.backend.lemonade.enabled, Some(true));
    assert_eq!(
      on_loaded.config.backend.lemonade.primary_binary(),
      Some(std::path::Path::new("/opt/lemonade/lemond"))
    );
    fs::remove_dir_all(on_dir).expect("temp test dir should be removed");
  }

  #[test]
  fn proxy_config_unknown_key_is_rejected() {
    let dir = temp_test_dir("proxy-unknown");
    let path = dir.join("config.yaml");
    // `foo` is not part of ProxyConfig; with #[serde(deny_unknown_fields)]
    // on ProxyConfig the parser must reject the file and the loader
    // falls back to defaults with a warning naming the offending key.
    fs::write(&path, "proxy:\n  foo: bar\n").expect("write failed");

    let loaded = load_config_from_path(&path);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("unknown proxy key must surface a warning");
    assert!(
      warning.contains("foo"),
      "warning should name the unknown key, got: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn shipped_example_config_parses_without_warning() {
    // The shipped `config.example.yaml` is the user-facing source of
    // truth for the config surface. Its active (uncommented) keys must
    // deserialize into `Config` with no warning — this guards against
    // the example drifting from the struct (a stale key under a
    // `deny_unknown_fields` block like `proxy` / `lemonade`, a renamed
    // field, or a malformed edit). Commented-out keys are inert here;
    // they're covered by the per-section round-trip tests above.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml");
    let loaded = load_config_from_path(&path);
    assert!(
      loaded.warning.is_none(),
      "config.example.yaml must parse cleanly, got: {:?}",
      loaded.warning
    );
    // Spot-check that the active keys actually took effect (not just
    // that an empty doc parsed): defaults the example pins explicitly.
    assert!(loaded.config.proxy.enabled);
    assert!(!loaded.config.proxy.insecure_no_auth);
    // The example pins `backend.lemonade.enabled: true` explicitly (active key,
    // per the "example keys are not commented out" convention).
    assert_eq!(loaded.config.backend.lemonade.enabled, Some(true));
    assert_eq!(loaded.config.backend.lemonade.port, 13305);
  }

  #[test]
  fn load_config_from_path_rejects_directory_target_with_warning() {
    let dir = temp_test_dir("dir-target");
    // Point load_config_from_path at the directory itself, not a file in it.
    let loaded = load_config_from_path(&dir);

    assert_eq!(loaded.config, Config::default());
    let warning = loaded
      .warning
      .expect("non-regular-file target must surface a warning");
    assert!(
      warning.contains("not a regular file"),
      "warning should mention non-regular file, got: {warning}"
    );
    fs::remove_dir_all(dir).expect("temp test dir should be removed");
  }

  #[test]
  fn effective_api_key_resolves_env_over_config_blank_as_none() {
    // Shares the crate env mutex with the daemon's
    // LLAMASTASH_PROXY_API_KEY tests so they don't race on the var.
    let _env = crate::cli::test_lock::serialize();
    let saved = std::env::var_os("LLAMASTASH_PROXY_API_KEY");
    std::env::remove_var("LLAMASTASH_PROXY_API_KEY");

    let mut proxy = ProxyConfig {
      api_key: Some("sk-llamastash-cfg".into()),
      ..ProxyConfig::default()
    };
    // Configured key, no env override → the config key.
    assert_eq!(
      proxy.effective_api_key().as_deref(),
      Some("sk-llamastash-cfg")
    );
    // Blank / absent config key → no auth.
    proxy.api_key = Some("   ".into());
    assert_eq!(proxy.effective_api_key(), None);
    proxy.api_key = None;
    assert_eq!(proxy.effective_api_key(), None);

    // Env override wins over config and is trimmed.
    proxy.api_key = Some("sk-llamastash-cfg".into());
    std::env::set_var("LLAMASTASH_PROXY_API_KEY", "  sk-llamastash-env  ");
    assert_eq!(
      proxy.effective_api_key().as_deref(),
      Some("sk-llamastash-env")
    );
    // A blank env override is ignored → falls back to config.
    std::env::set_var("LLAMASTASH_PROXY_API_KEY", "   ");
    assert_eq!(
      proxy.effective_api_key().as_deref(),
      Some("sk-llamastash-cfg")
    );

    match saved {
      Some(v) => std::env::set_var("LLAMASTASH_PROXY_API_KEY", v),
      None => std::env::remove_var("LLAMASTASH_PROXY_API_KEY"),
    }
  }
}
