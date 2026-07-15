//! ds4 (antirez's DwarfStar) backend — a **direct, process-per-model** peer
//! that runs exclusively the DeepSeek V4 Flash/PRO GGUFs through the
//! self-contained `ds4-server` binary.
//!
//! ds4-server is OpenAI/Anthropic-compatible but is *not* `llama-server`: it
//! speaks a smaller flag set, exposes no `/health` and no web UI, and reports
//! a fixed model **alias** (`deepseek-v4-flash` / `deepseek-v4-pro`) rather
//! than the file path. Every one of those divergences is encoded here so the
//! rest of the daemon (supervisor, probe, orphan sweep, proxy) stays
//! backend-agnostic.
//!
//! Routing keys on [`ds4_compatible`] — the header-level quant contract read
//! from ds4's own loader (`ds4.c`) — not on arch alone, because a current
//! upstream llama.cpp (b9840+) also runs `deepseek4` GGUFs. ds4 is the
//! *preferred* engine for the files it can run; llama.cpp is the fallback,
//! never a refusal (an older llama.cpp than b9840 rejects the file with
//! `unknown model architecture: 'deepseek4'`).
//!
//! Facts verified against a real ds4 build (`ds4-server --help`, 2026-07-10):
//! - readiness: `ds4_engine_open` loads the model *before* `listen_on`, so a
//!   `GET /v1/models` 200 means the weights are resident (see [`readiness`]);
//! - flags: ds4-server has `--power`/`--tokens`/`--threads`/`--kv-disk-*`/
//!   `--ssd-streaming` plus the MTP trio `--mtp`/`--mtp-draft`/`--mtp-margin`
//!   (confirmed under `--help runtime`), so the native-knob table is 9 entries.
//!   `--quality` is ds4-CLI-only and excluded.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::identity::ModelIdentity;
use super::{
  Accelerator, AcceleratorSupport, Backend, KnobCapability, LaunchPlan, Lifecycle,
  NativeKnobResolution, ProcessLaunchSpec, Readiness, CREDENTIAL_ENV_STRIP,
};
use crate::config::Config;
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::gguf::header::{GgufHeader, GgufValue};
use crate::init::doctor::{Finding, Severity};
use crate::launch::flag_aliases::KnobField;
use crate::launch::mode::LaunchMode;
use crate::launch::native_knobs::{translate, NativeKnobDescriptor, NativeKnobKind};
use crate::launch::params::LaunchParams;

/// The backend id — the stable string used in `BackendChoice`, `status`, the
/// `list`/`show` badge, and adoption dispatch.
pub const DS4_BACKEND_ID: &str = "ds4";

/// ds4 (DwarfStar) direct backend config. **Default-on, gated by binary
/// detection** (D-enable): unlike Lemonade's opt-in `bool`, `enabled` is an
/// `Option<bool>` — `None` (unset) means "on when `ds4-server` resolves",
/// `Some(false)` forces off, `Some(true)` forces on. The `--ds4` flag /
/// `LLAMASTASH_DS4=1` force on regardless. Availability always additionally
/// requires the binary to resolve, so the footprint is zero when it is absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Ds4Config {
  #[serde(default)]
  pub enabled: Option<bool>,
  /// The `ds4-server` binary/binaries. ds4 is single-binary in practice, so the
  /// first entry is the server; unset falls back to a `ds4-server` on `$PATH`.
  #[serde(default)]
  pub servers: Vec<crate::backend::ServerConfig>,
}

impl Ds4Config {
  /// Whether the user *intends* ds4 enabled, given the force flag
  /// (`--ds4` / `LLAMASTASH_DS4`). Actual availability still requires the
  /// binary to resolve — this only encodes intent (default-on unless
  /// explicitly `enabled: false`, which the force flag overrides).
  pub fn intends_enabled(&self, force: bool) -> bool {
    force || self.enabled != Some(false)
  }

  /// The configured `ds4-server` path (first server), if any — the input to
  /// [`resolve_ds4_binary`].
  pub fn primary_binary(&self) -> Option<&Path> {
    self.servers.first().map(|s| s.binary.as_path())
  }
}

/// Executable name searched on `PATH` when `ds4.binary` is unset. The PATH
/// search (and this const) are compiled out under `test-fixtures` so tests
/// never auto-discover a host `ds4-server`.
#[cfg(not(feature = "test-fixtures"))]
const DS4_SERVER_BIN: &str = "ds4-server";

/// The fixed model aliases ds4-server advertises on `GET /v1/models`. Used
/// by both the readiness body check (D-ready) and orphan adoption (D-adopt).
/// Tolerant of future `deepseek-v4-*` ids via [`is_ds4_alias`].
pub const DS4_ALIAS_IDS: &[&str] = &["deepseek-v4-flash", "deepseek-v4-pro"];

/// The common prefix every ds4 `/v1/models` alias carries. Matching on this
/// (rather than the two exact ids) keeps readiness + adoption tolerant of a
/// future ds4-server build advertising e.g. `deepseek-v4-turbo`.
pub const DS4_ALIAS_PREFIX: &str = "deepseek-v4-";

/// Network-affecting flag heads ds4 adds on top of the base
/// loopback/credential denylist ([`crate::launch::params::FORBIDDEN_ADVANCED_PREFIXES`]).
/// `--cors` weakens the browser same-origin posture on the loopback child;
/// `--dist-` is forward-defense against ds4's distributed serving mode (the
/// current ds4-server build has no `--dist-*` flags, but the ds4 CLI does).
/// `--host`/`--listen` are already covered by the base set.
pub const DS4_FORBIDDEN_EXTRA_HEADS: &[&str] = &["--cors", "--dist-"];

/// `doctor` finding id: a ds4-compatible DeepSeek-V4 GGUF is present but
/// `ds4-server` is unavailable. Info-tier — the model still *runs* (llama.cpp
/// fallback), so the advice is "install the purpose-built engine", not an error.
const DS4_DOCTOR_UNAVAILABLE_ID: &str = "ds4_unavailable";

/// `doctor` finding id: `ds4-server` is installed but disabled via
/// `backend.ds4.enabled: false` (no force). Distinct from `ds4_unavailable` so
/// the fix hint is "re-enable it", not "build it".
const DS4_DOCTOR_DISABLED_ID: &str = "ds4_disabled";

/// `doctor` fix-hint for [`DS4_DOCTOR_UNAVAILABLE_ID`].
const DS4_DOCTOR_UNAVAILABLE_FIX: &str =
  "build ds4-server (`git clone https://github.com/antirez/ds4 && cd ds4 && make`) and set \
   `backend.ds4.binary` to the built path (or put `ds4-server` on PATH); see docs/usage.md#ds4-backend";

/// `doctor` fix-hint for [`DS4_DOCTOR_DISABLED_ID`].
const DS4_DOCTOR_DISABLED_FIX: &str =
  "remove `backend.ds4.enabled: false` from config.yaml (or set `LLAMASTASH_DS4=1`) to route \
   DeepSeek-V4 models to ds4-server; see docs/usage.md#ds4-backend";

// GGML tensor type ids (subset ds4's contract references). These are the
// on-disk `ggml_type` values in the GGUF tensor-info block.
const GGML_F32: u32 = 0;
const GGML_F16: u32 = 1;
const GGML_Q8_0: u32 = 8;
const GGML_Q2_K: u32 = 10;
const GGML_Q4_K: u32 = 12;
const GGML_IQ2_XXS: u32 = 16;
const GGML_I32: u32 = 26;

/// Whether `id` names a ds4 model alias (the fixed `/v1/models` id). Tolerant
/// of future `deepseek-v4-*` variants beyond the two shipped today.
pub fn is_ds4_alias(id: &str) -> bool {
  id.starts_with(DS4_ALIAS_PREFIX)
}

/// The **ds4-compatibility predicate** — the single routing signal (D-compat).
///
/// A GGUF is ds4-compatible when its arch is `deepseek4` **and** its
/// per-tensor-role quant recipe matches ds4's loader contract (read from
/// `ds4.c`): routed-expert tensors (`ffn_{gate,up,down}_exps`) are one of
/// `IQ2_XXS` / `Q2_K` / `Q4_K` (`tensor_is_routed_expert_type`), and every
/// other weight tensor is `F32` / `F16` / `Q8_0` (`matvec_any`) plus `I32`
/// index tables. Both published Flash/PRO variants pass; a generic
/// third-party `deepseek4` K-quant (Q4_K/Q6_K on attention tensors, Q6_K
/// experts) fails and stays an ordinary llama.cpp model.
///
/// Header-only: it reads tensor-info types, never weight bytes. Requires at
/// least one routed-expert tensor to be present, so a truncated or
/// metadata-only header can't false-positive.
///
/// Re-audit per ds4 release: the type sets are anchored to named `ds4.c`
/// functions and can drift if ds4 adds quant kernels.
pub fn ds4_compatible(header: &GgufHeader) -> bool {
  let arch = header.get("general.architecture").and_then(|v| v.as_str());
  if arch != Some("deepseek4") {
    return false;
  }
  let mut saw_expert = false;
  for t in &header.tensors {
    if is_routed_expert_tensor(&t.name) {
      saw_expert = true;
      if !matches!(t.ggml_type, GGML_IQ2_XXS | GGML_Q2_K | GGML_Q4_K) {
        return false;
      }
    } else if !matches!(t.ggml_type, GGML_F32 | GGML_F16 | GGML_Q8_0 | GGML_I32) {
      return false;
    }
  }
  // A real ds4 model has 43+ routed-expert tensors; requiring one guards
  // against a header whose tensor list was truncated or absent.
  saw_expert
}

/// Whether `filename` names one **half** of ds4's PRO distributed/split GGUF
/// pair (D-guard) — files that are unloadable *alone* by either engine:
/// `DeepSeek-V4-Pro-Q4K-Layers00-30.gguf` (a bare layer-range shard) and
/// `DeepSeek-V4-Pro-Q4K-Layers-31-output.gguf` (the output half).
///
/// Deliberately precise, **not** a bare `-Layers` substring: the single-file
/// Flash `…-Layers37-42Q4KExperts-…-imatrix-fixed.gguf` carries a layer range
/// *followed by quant descriptors* and is a fully launchable model — it must
/// not be refused. Pass the file name (or stem); matching is case-insensitive.
pub fn is_ds4_split_half(filename: &str) -> bool {
  // Strip a trailing `.gguf` so both the stem and the full name work.
  let s = filename
    .to_ascii_lowercase()
    .trim_end_matches(".gguf")
    .to_string();
  // Output half: `…-layers-31-output`.
  if s.contains("layers") && s.ends_with("-output") {
    return true;
  }
  // Layer-range half: the name ends with `layers<digits>-<digits>` and nothing
  // else. The single-file Flash has quant text after the range, so its `split_once`
  // right half is not all-digits and it is not matched.
  if let Some(idx) = s.rfind("layers") {
    let tail = &s[idx + "layers".len()..];
    let tail = tail.strip_prefix('-').unwrap_or(tail);
    if let Some((a, b)) = tail.split_once('-') {
      let all_digits = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
      if all_digits(a) && all_digits(b) {
        return true;
      }
    }
  }
  false
}

/// ds4's KV-cache byte model, computed from the header. Lives with the backend
/// (moved out of `gguf::memory`) so the ds4-specific cache geometry is
/// self-contained; `gguf::memory::kv_bytes` reaches it through the
/// [`Backend::kv_bytes`] hook, which gates it on the `deepseek4` arch — a
/// deepseek4 GGUF gets this figure even on the llama.cpp fallback, matching the
/// pre-seam behavior. This function itself is ungated (the caller decides), so
/// it stays byte-identical to the estimator code it replaced.
///
/// ds4 keeps, per layer, a small uncompressed recent window plus a compressed
/// cache of `ctx / compress_ratio[layer]` rows; every row is
/// `attention.key_length` F32 latents (mirrors ds4.c's KV policy). Reading the
/// per-layer `attention.compress_ratios` + `key_length` sizes both Flash and
/// PRO from their own headers (~0.5 GiB at 8k ctx, ~11 GiB at 1M for Flash)
/// instead of the naive GQA figure (`head_count_kv=1 × key_length × full ctx`),
/// which ignores the sequence compression and over-counts ~8x at long context.
pub fn ds4_kv_bytes(header: &GgufHeader, ctx: u64) -> u64 {
  let key_length = header.u64(&["deepseek4.attention.key_length"]).unwrap_or(0);
  // ds4 caches F32 latents (`sizeof(float)` in ds4.c).
  const BYTES_PER_ELEM: u64 = 4;
  // Per-layer uncompressed recent window; ds4 sizes it from the prefill chunk
  // (~4k rows). A fixed conservative floor, capped at the context length.
  const RAW_CAP_ROWS: u64 = 4096;
  let raw_rows = RAW_CAP_ROWS.min(ctx.max(1));
  let ratios: Vec<u64> = match header.get("deepseek4.attention.compress_ratios") {
    Some(GgufValue::Array(a)) => a.iter().filter_map(GgufValue::as_u64).collect(),
    _ => Vec::new(),
  };
  let mut rows: u64 = 0;
  if ratios.is_empty() {
    // No per-layer ratios (unexpected for a real ds4 GGUF): fall back to the
    // arch's densest ratio (4) across every layer.
    let n_layers = header.u64(&["deepseek4.block_count"]).unwrap_or(0);
    rows = n_layers.saturating_mul(raw_rows.saturating_add(ctx / 4));
  } else {
    for r in &ratios {
      rows = rows.saturating_add(raw_rows); // uncompressed recent window
      if *r != 0 {
        rows = rows.saturating_add(ctx / r); // compressed rows
      }
    }
  }
  rows
    .saturating_mul(key_length)
    .saturating_mul(BYTES_PER_ELEM)
}

/// Routed-expert tensor marker: `ffn_gate_exps` / `ffn_up_exps` /
/// `ffn_down_exps` all carry the `_exps` component. The shared expert
/// (`ffn_*_shexp`) and the router gate (`ffn_gate_inp`) do **not**, so they
/// correctly fall in the dense/other bucket.
fn is_routed_expert_tensor(name: &str) -> bool {
  name.contains("_exps")
}

/// ds4's native-knob descriptor table — 9 tunables that have no llama.cpp IR
/// slot. Ids are stable persistence keys; label/description drive the
/// launch-picker rows. Only flags the real `ds4-server` binary accepts
/// (verified via `--help runtime`, 2026-07-10 build): the MTP trio
/// (`--mtp`/`--mtp-draft`/`--mtp-margin`) is included; `--quality` is
/// ds4-CLI-only and deliberately excluded.
pub const DS4_NATIVE_KNOBS: &[NativeKnobDescriptor] = &[
  NativeKnobDescriptor {
    id: "power",
    label: "GPU power %",
    description: "GPU duty-cycle target, 1-100 (ds4 default 100)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "tokens",
    label: "Default tokens",
    description: "default max output tokens when a client omits a limit",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "threads",
    label: "CPU threads",
    description: "CPU helper thread count for host-side work",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "kv_disk_dir",
    label: "KV disk dir",
    description: "directory for ds4's persistent disk KV cache (user-owned, never cleaned)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "kv_disk_space_mb",
    label: "KV disk cap",
    description: "disk KV cache budget in MB (ds4 default 4096 when enabled)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "ssd_streaming",
    label: "SSD streaming",
    description: "stream weights from disk (below-RAM-floor mode; skips the admission gate)",
    kind: NativeKnobKind::Bool,
  },
  NativeKnobDescriptor {
    id: "mtp",
    label: "MTP sidecar",
    description: "path to the MTP draft-head GGUF (auto-paired from a sibling when unset)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "mtp_draft",
    label: "MTP draft tokens",
    description: "max autoregressive MTP draft tokens (ds4 default 1)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "mtp_margin",
    label: "MTP margin",
    description: "verifier confidence margin for fast MTP acceptance (ds4 default 3)",
    kind: NativeKnobKind::FreeText,
  },
];

/// `id → ds4-server flag head` mapping consumed by
/// [`crate::launch::native_knobs::translate`]. One row per descriptor.
const DS4_FLAG_MAP: &[(&str, &str)] = &[
  ("power", "--power"),
  ("tokens", "--tokens"),
  ("threads", "--threads"),
  ("kv_disk_dir", "--kv-disk-dir"),
  ("kv_disk_space_mb", "--kv-disk-space-mb"),
  ("ssd_streaming", "--ssd-streaming"),
  ("mtp", "--mtp"),
  ("mtp_draft", "--mtp-draft"),
  ("mtp_margin", "--mtp-margin"),
];

/// Resolve the `ds4-server` binary: an explicit `ds4.binary` config path
/// (must exist), else the first `ds4-server` on `PATH`. Canonicalized.
/// Mirrors [`crate::backend::lemonade::resolve_lemond_binary`].
pub fn resolve_ds4_binary(configured: Option<&Path>) -> Option<PathBuf> {
  fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
  }
  if let Some(explicit) = configured {
    return explicit.is_file().then(|| canonical(explicit));
  }
  // Never auto-discover a host `ds4-server` on `PATH` under the test-fixtures
  // build — same reason as `resolve_lemond_binary`: a real daemon subprocess
  // (with ds4 default-on) must not pick up + leak the developer's system
  // binary. Tests point at an explicit fake `ds4.binary`.
  #[cfg(feature = "test-fixtures")]
  {
    None
  }
  #[cfg(not(feature = "test-fixtures"))]
  {
    let exe = if cfg!(windows) {
      format!("{DS4_SERVER_BIN}.exe")
    } else {
      DS4_SERVER_BIN.to_string()
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
      let candidate = dir.join(&exe);
      if candidate.is_file() {
        return Some(canonical(&candidate));
      }
    }
    None
  }
}

/// The ds4 backend: direct, process-per-model, DeepSeek-V4-only.
#[derive(Debug, Clone)]
pub struct Ds4Backend {
  capabilities: KnobCapability,
}

impl Ds4Backend {
  pub fn new() -> Self {
    // ds4 honors exactly one IR knob — `Ctx` (→ `--ctx`). Everything else
    // llama.cpp-shaped is dropped per R6; ds4's own tunables ride the
    // native-knob channel.
    Self {
      capabilities: KnobCapability::of(&[KnobField::Ctx]),
    }
  }

  /// Build the process-per-model launch spec directly (tests and the
  /// orchestrator's spawn arm both consume this).
  pub fn process_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ProcessLaunchSpec {
    ProcessLaunchSpec {
      binary,
      argv: ds4_argv(params, port),
      // ds4 reads no env config, but strip the HF pull credentials it has no
      // reason to see — least-privilege applies at least as strongly to a
      // young third-party binary as to llama-server.
      env_remove: CREDENTIAL_ENV_STRIP.to_vec(),
      readiness: readiness(),
      probe,
    }
  }
}

impl Default for Ds4Backend {
  fn default() -> Self {
    Self::new()
  }
}

/// The ds4 readiness contract (D-ready): `GET /v1/models` → 200 **and** a
/// body whose model id is a ds4 alias. The alias check matters because ds4
/// leaves its reserved port *unbound* for the entire multi-minute load
/// (unlike llama-server's immediate bind), so a status-only 200 could come
/// from any process that grabbed the port meanwhile.
pub fn readiness() -> Readiness {
  Readiness::HttpPollModelId {
    path: "/v1/models".to_string(),
    ready_status: 200,
    // Match the `deepseek-v4-` prefix, not the two exact ids: the probe does a
    // substring check against the response body, so the prefix accepts every
    // shipped alias (`-flash` / `-pro`) plus a future `-turbo`, staying in
    // lockstep with `is_ds4_alias` / adoption. Scanning the whole body is
    // consistent with the existing foreign-process-on-port threat model.
    expect_model_ids: vec![DS4_ALIAS_PREFIX.to_string()],
  }
}

/// Build the ds4-server argv. Never `compose` (that emits llama-server
/// flags): `-m <path> --host 127.0.0.1 --port <port>`, `--ctx <n>` when the
/// resolved `Ctx` knob is set, the translated native knobs, then extras —
/// all under the ds4-extended loopback/credential strip. Never `--jinja` /
/// `--reasoning-format` (ds4-server has no such flags).
pub fn ds4_argv(params: &LaunchParams, port: u16) -> Vec<std::ffi::OsString> {
  use std::ffi::OsString;
  let mut argv: Vec<OsString> = vec![
    OsString::from("-m"),
    params.model_path.clone().into_os_string(),
    OsString::from("--host"),
    OsString::from("127.0.0.1"),
    OsString::from("--port"),
    OsString::from(port.to_string()),
  ];
  if let Some(ctx) = params.ctx {
    argv.push(OsString::from("--ctx"));
    argv.push(OsString::from(ctx.to_string()));
  }
  // Native knobs, strip-checked against base ∪ ds4 forbidden heads.
  argv.extend(translate(
    DS4_NATIVE_KNOBS,
    DS4_FLAG_MAP,
    &params.backend_knobs,
    DS4_FORBIDDEN_EXTRA_HEADS,
  ));
  // Free-form extras tail. The fail-fast `forbidden_in_extras_ext` in
  // `compose_and_spawn` already refused a banned head with a clear error;
  // this defensive strip is the belt-and-suspenders that guarantees no
  // banned flag reaches ds4-server even if a path skipped the fail-fast.
  for e in &params.extras {
    let lossy = e.to_string_lossy();
    let head = lossy.split('=').next().unwrap_or(&lossy);
    if crate::launch::params::is_forbidden_head_ext(head, DS4_FORBIDDEN_EXTRA_HEADS) {
      log::warn!("ds4_argv: stripping forbidden extra {head:?}");
      continue;
    }
    argv.push(e.clone());
  }
  argv
}

/// ds4 adoption id contract (D-adopt): `/v1/models` → 200 with a `data[].id`
/// in ds4's fixed alias set (`deepseek-v4-*`). ds4 never echoes the path, so
/// [`Ds4Backend::adoption_matches`] pairs this with an argv `-m` cross-check for
/// per-file discrimination. The `/v1/models` fetch is the generic OpenAI-compat
/// probe both backends serve ([`crate::daemon::orphans::fetch_models_body`]).
async fn models_endpoint_reports_ds4_alias(port: u16, timeout: std::time::Duration) -> bool {
  let Ok((200, body)) = crate::daemon::orphans::fetch_models_body(port, timeout).await else {
    return false;
  };
  let Ok(text) = std::str::from_utf8(&body) else {
    return false;
  };
  let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
    return false;
  };
  parsed
    .get("data")
    .and_then(|v| v.as_array())
    .map(|arr| {
      arr.iter().any(|row| {
        row
          .get("id")
          .and_then(|v| v.as_str())
          .map(is_ds4_alias)
          .unwrap_or(false)
      })
    })
    .unwrap_or(false)
}

impl Backend for Ds4Backend {
  fn id(&self) -> &'static str {
    DS4_BACKEND_ID
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  fn capabilities(&self) -> &KnobCapability {
    &self.capabilities
  }

  fn native_knobs(&self) -> &'static [NativeKnobDescriptor] {
    DS4_NATIVE_KNOBS
  }

  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    DS4_FORBIDDEN_EXTRA_HEADS
  }

  // serves_web_ui: keeps the trait default (`false`) — ds4-server has no web UI,
  // so `/ui` never auto-pins a ds4 model.

  fn accelerators(&self) -> AcceleratorSupport {
    // CPU is the always-available floor; whether a given ds4-server build
    // drives Metal/CUDA/ROCm is a build variant invisible to us (mirrors
    // llama.cpp's conservative floor).
    AcceleratorSupport::from_list([Accelerator::Cpu])
  }

  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity {
    // ds4 models stay plain GGUFs — same `(path, BLAKE3)` identity as
    // llama.cpp, so discovery / favorites / renames are unchanged.
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

  fn adoption_model_ids(&self) -> &'static [&'static str] {
    DS4_ALIAS_IDS
  }

  async fn adoption_matches(
    &self,
    recorded_path: &Path,
    argv: &[String],
    port: u16,
    probe_timeout: std::time::Duration,
  ) -> bool {
    // ds4-server reports a fixed alias, never the path, so the alias alone can't
    // discriminate two ds4 instances: argv `-m` must equal the recorded
    // canonical path (restores per-file discrimination) AND the endpoint must
    // advertise a ds4 alias. Either failing rejects the adoption (the PID-reuse
    // guard on the ds4 path).
    let argv_ok = crate::daemon::orphans::extract_model_path(argv)
      .as_deref()
      .map(|mp| crate::daemon::orphans::paths_equal(mp, recorded_path))
      .unwrap_or(false);
    argv_ok && models_endpoint_reports_ds4_alias(port, probe_timeout).await
  }

  fn kv_bytes(&self, header: &GgufHeader, arch: Option<&str>, ctx_len: u64) -> Option<u64> {
    // The compressed-cache model, gated on the `deepseek4` arch exactly as the
    // pre-seam `if a == "deepseek4"` estimator branch was.
    (arch == Some("deepseek4")).then(|| ds4_kv_bytes(header, ctx_len))
  }

  fn auto_routes(&self, header: &GgufHeader) -> bool {
    // The header-level routing predicate: arch `deepseek4` + the per-tensor
    // quant contract. A compatible GGUF prefers ds4 (available + chat mode);
    // otherwise it falls back to llama.cpp — never a refusal.
    ds4_compatible(header)
  }

  fn serves_mode(&self, mode: LaunchMode) -> bool {
    // ds4-server serves chat/completions, not embeddings/rerank — a mode
    // mismatch routes to the fallback engine (a routing input, not an error).
    matches!(mode, LaunchMode::Chat)
  }

  fn refuses(&self, arch: Option<&str>, path: &Path) -> Option<String> {
    // Each half of the distributed/split PRO GGUF pair is unloadable *alone* by
    // any engine, and attempting it wastes a 100 GB+ load. Gated on the
    // `deepseek4` arch so an unrelated GGUF that merely matches the
    // `…-Layers00-30` filename pattern is never wrongly refused.
    if arch != Some("deepseek4") {
      return None;
    }
    let name = path.file_name().and_then(|n| n.to_str())?;
    is_ds4_split_half(name).then(|| {
      format!(
        "`{name}` is one half of ds4's distributed/split PRO GGUF — unloadable on its own. \
         ds4 distributed mode is unsupported; use a single-file DeepSeek-V4 GGUF, or pass \
         `--backend ds4` to attempt it anyway (ds4-server will surface its own error)."
      )
    })
  }

  fn available(&self, ctx: &MethodContext) -> bool {
    // Intent (default-on unless `ds4.enabled: false`, `--ds4`/env force) AND the
    // `ds4-server` binary resolves. The single availability predicate selection,
    // the split-file guard, and `status` all consult.
    let force = ctx
      .backend_force
      .get(DS4_BACKEND_ID)
      .copied()
      .unwrap_or(false);
    ctx.backend.ds4.intends_enabled(force)
      && resolve_ds4_binary(ctx.backend.ds4.primary_binary()).is_some()
  }

  fn installed(&self, ctx: &MethodContext) -> bool {
    // Presence of the binary, independent of the enablement toggle.
    resolve_ds4_binary(ctx.backend.ds4.primary_binary()).is_some()
  }

  fn status_enabled(&self, ctx: &MethodContext) -> Option<bool> {
    Some(self.available(ctx))
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    resolve_ds4_binary(ctx.backend.ds4.primary_binary()).map(|b| b.display().to_string())
  }

  fn configured_servers(&self, ctx: &MethodContext) -> Vec<super::ServerSpec> {
    // ds4 is single-binary and device-probe-less: one server when available.
    if !self.available(ctx) {
      return Vec::new();
    }
    resolve_ds4_binary(ctx.backend.ds4.primary_binary())
      .map(|binary| {
        vec![super::ServerSpec {
          binary,
          name: ctx.backend.ds4.servers.first().and_then(|s| s.name.clone()),
        }]
      })
      .unwrap_or_default()
  }

  fn config_servers(&self, config: &crate::config::Config) -> Vec<super::ServerConfig> {
    config.backend.ds4.servers.clone()
  }

  fn launch_priority(&self) -> i32 {
    // Purpose-built for DeepSeek-V4 — outranks llama.cpp for a compatible file.
    20
  }

  fn process_marker(&self) -> Option<&'static str> {
    Some("ds4-server")
  }

  fn resolve_launch_binary(
    &self,
    ctx: &MethodContext,
    _default_binary: PathBuf,
    port: u16,
  ) -> Result<(PathBuf, u16), String> {
    // ds4 spawns `ds4-server` (not the device-owning `llama-server`) on the
    // reserved pool port.
    match resolve_ds4_binary(ctx.backend.ds4.primary_binary()) {
      Some(bin) => Ok((bin, port)),
      None => Err(
        "ds4 backend selected but no `ds4-server` binary found; set `ds4.binary` \
         or put `ds4-server` on PATH (see docs/usage.md)"
          .to_string(),
      ),
    }
  }

  async fn resolve_native_knobs(
    &self,
    ctx: &MethodContext,
    params: &mut LaunchParams,
    weights_bytes: u64,
  ) -> NativeKnobResolution {
    let mut out = NativeKnobResolution::default();
    // MTP auto-pair (KD7): an unset `mtp` knob resolves to a separate MTP
    // draft-head sidecar found next to the model (the same sibling detection
    // discovery uses on disk), paired as `--mtp <path>`. A user-set value wins.
    // Honors the backend-agnostic MTP intent — `mtp: off` (from `--mtp off` or
    // the picker's MTP row) skips auto-pairing entirely, so the one MTP switch
    // governs every backend. Independent of host metrics, so it runs before the
    // streaming gate; the auto-set key is stripped from `last_params` so it
    // re-resolves each launch.
    if !matches!(params.mtp, crate::launch::params::MtpEnable::Off)
      && !matches!(
        params.backend_knobs.get("mtp"),
        Some(crate::config::KnobValue::Set(_))
      )
    {
      if let Some(head) = crate::discovery::scanner::find_mtp_head(&params.model_path) {
        log::info!("ds4: auto-paired MTP sidecar {}", head.display());
        params.backend_knobs.insert(
          "mtp".to_string(),
          crate::config::KnobValue::Set(head.display().to_string()),
        );
        out.auto_set.insert("mtp".to_string());
      }
    }
    // `ssd_streaming` Auto → on when residency won't fit. ds4 holds the full
    // model plus a cached-expert/KV working set the deepseek4 demand model can't
    // see (~1.25× weights), so a full-residency spawn OOM-kills mid-load
    // (ds4-server sets its own oom_score_adj=1000) — disk-stream instead. A user
    // on/off wins; only the unset/Auto knob resolves here.
    if matches!(
      params.backend_knobs.get("ssd_streaming"),
      Some(crate::config::KnobValue::Set(_))
    ) {
      return out;
    }
    let Some(host_slot) = ctx.host_metrics.as_ref() else {
      return out;
    };
    let snapshot = host_slot.read().await.clone();
    if !crate::launch::admission::is_sampled(&snapshot) {
      return out;
    }
    let free = crate::launch::admission::effective_free_bytes(&snapshot);
    if ds4_should_auto_stream(weights_bytes, free) {
      params.backend_knobs.insert(
        "ssd_streaming".to_string(),
        crate::config::KnobValue::Set("true".to_string()),
      );
      out.auto_set.insert("ssd_streaming".to_string());
      let gib = crate::init::detection::fmt_gib;
      out.warnings.push(format!(
        "ds4 needs ~{} resident but only {} is free — enabled SSD streaming to launch \
         from disk (slower). Set `ssd_streaming: false` to force full residency.",
        gib(ds4_resident_estimate(weights_bytes)),
        gib(free)
      ));
    }
    out
  }

  fn bypasses_admission(&self, params: &LaunchParams) -> bool {
    // Streaming weights from disk skips the hard OOM refusal (on-disk bytes ≠
    // memory demand). Reads the resolved `ssd_streaming` knob.
    matches!(
      params.backend_knobs.get("ssd_streaming"),
      Some(crate::config::KnobValue::Set(v)) if v == "true"
    )
  }

  /// ds4's `doctor` advisories (D-doctor). Three cases, keyed on the same
  /// availability logic a launch uses (`intends_enabled` + binary resolve,
  /// honoring the `LLAMASTASH_DS4` force):
  /// - **available** (intended + installed): nothing to advise, no scan.
  /// - **installed but disabled** (`backend.ds4.enabled: false`, no force): a
  ///   config choice, not a missing engine — advise how to re-enable, no scan
  ///   and no wrong "build it" hint.
  /// - **binary absent**: scan for a ds4-compatible model and, if one is
  ///   present, advise installing ds4. Only this case pays the scan (bounded by
  ///   the arch pre-check), and it's the only one where the install advice fits.
  async fn doctor_findings(&self, config: &Config) -> Vec<Finding> {
    use crate::discovery::known_caches::{default_set, RootResolution};
    use crate::discovery::scanner::{scan, ScanOptions};
    use crate::gguf::header::{read_path, HeaderReadOptions};

    let force = ds4_env_forced();
    let intends = config.backend.ds4.intends_enabled(force);
    let binary = resolve_ds4_binary(config.backend.ds4.primary_binary());
    // Available: intended and installed. Nothing to advise, no scan.
    if intends && binary.is_some() {
      return Vec::new();
    }
    // Installed but explicitly disabled: a deliberate config choice, not a
    // missing engine. Advise how to re-enable (no scan, no "build it" hint).
    if binary.is_some() && !intends {
      return vec![Finding::from_parts(
        DS4_DOCTOR_DISABLED_ID,
        Severity::Info,
        "ds4-server is installed but disabled (`backend.ds4.enabled: false`); remove that key (or set \
         `LLAMASTASH_DS4=1`) to route DeepSeek-V4 models to the purpose-built engine",
        DS4_DOCTOR_DISABLED_FIX,
      )];
    }
    // Discovery resolves model paths from CLI + env + config; doctor is
    // config-only, so fold in `LLAMASTASH_MODEL_PATHS` / `LLAMASTASH_NO_SCAN`
    // here too — otherwise a user who supplies model paths *only* via the env
    // override gets no `ds4_unavailable` advisory even with a compatible model
    // present.
    let mut user_paths = config.model_paths.clone();
    user_paths.extend(crate::cli::daemon::env_model_paths());
    let roots = default_set(RootResolution {
      user_paths: &user_paths,
      disable: &config.disable_default_cache_paths,
      no_scan: config.disable_scan || crate::cli::daemon::env_no_scan(),
      home: crate::util::paths::home_dir().as_deref(),
    });
    let mut rx = scan(roots, ScanOptions::default());
    while let Some(m) = rx.recv().await {
      // Arch pre-check spares a header read for every non-deepseek4 model.
      if m.metadata.as_ref().and_then(|md| md.arch.as_deref()) != Some("deepseek4") {
        continue;
      }
      let is_ds4 = read_path(&m.path, HeaderReadOptions::default())
        .map(|r| ds4_compatible(&r.header))
        .unwrap_or(false);
      if is_ds4 {
        let name = m
          .path
          .file_name()
          .and_then(|n| n.to_str())
          .unwrap_or("a DeepSeek-V4 GGUF");
        return vec![Finding::from_parts(
          DS4_DOCTOR_UNAVAILABLE_ID,
          Severity::Info,
          format!(
            "ds4-compatible model present ({name}) but `ds4-server` is unavailable — it runs on \
             llama.cpp; install ds4 for the purpose-built engine (disk KV cache, SSD streaming)"
          ),
          DS4_DOCTOR_UNAVAILABLE_FIX,
        )];
      }
    }
    Vec::new()
  }
}

/// ds4's resident working set estimate: ~1.25× raw weights (the cached-expert
/// pool + KV + runtime the `deepseek4` demand model can't see). Its own auto
/// budget targets ~99 GiB for an 80 GiB Flash quant, which this tracks.
/// Saturating so a pathological weight total can't overflow.
fn ds4_resident_estimate(weights_total: u64) -> u64 {
  weights_total.saturating_add(weights_total / 4)
}

/// Whether a ds4 launch should auto-enable SSD streaming: its resident estimate
/// exceeds the effective free memory, so a full-residency spawn would OOM-kill
/// mid-load. Pure so the memory decision is unit-testable without a live host
/// sampler.
fn ds4_should_auto_stream(weights_total: u64, free: u64) -> bool {
  ds4_resident_estimate(weights_total) > free
}

/// Whether `LLAMASTASH_DS4` is set to a truthy value — the env force that
/// enables ds4 over `backend.ds4.enabled: false`. Read directly (not from
/// `MethodContext::backend_force`) because `doctor` runs CLI-side with no
/// context; mirrors the daemon's `env_flag_truthy` so the diagnostic agrees
/// with what a launch would actually do.
fn ds4_env_forced() -> bool {
  std::env::var("LLAMASTASH_DS4")
    .ok()
    .map(|v| {
      matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
      )
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::KnobValue;
  use crate::gguf::header::{GgufHeader, GgufValue, TensorInfo};
  use crate::launch::mode::LaunchMode;
  use std::collections::HashMap;
  use std::ffi::OsString;

  fn header(arch: &str, tensors: &[(&str, u32)]) -> GgufHeader {
    let mut metadata = HashMap::new();
    metadata.insert(
      "general.architecture".to_string(),
      GgufValue::String(arch.to_string()),
    );
    GgufHeader {
      version: 3,
      tensor_count: tensors.len() as u64,
      metadata,
      tensors: tensors
        .iter()
        .map(|(name, ty)| TensorInfo {
          name: name.to_string(),
          dims: vec![4096, 4096],
          ggml_type: *ty,
        })
        .collect(),
    }
  }

  fn params_with(ctx: Option<u32>, knobs: &[(&str, &str)]) -> LaunchParams {
    let mut p = LaunchParams::new(PathBuf::from("/m/ds4flash.gguf"), LaunchMode::Chat);
    p.ctx = ctx;
    for (k, v) in knobs {
      p.backend_knobs
        .insert((*k).to_string(), KnobValue::Set((*v).to_string()));
    }
    p
  }

  fn argv_strings(p: &LaunchParams, port: u16) -> Vec<String> {
    ds4_argv(p, port)
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  // ---- compat predicate ----

  #[test]
  fn compat_accepts_both_published_recipes() {
    // q2 Flash: IQ2_XXS gate/up experts, Q2_K down experts, Q8_0/F16/F32/I32
    // elsewhere (matches the real published header).
    let q2 = header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS),
        ("blk.0.ffn_down_exps.weight", GGML_Q2_K),
        ("blk.0.ffn_gate_tid2eid.weight", GGML_I32),
        ("blk.0.attn_q.weight", GGML_Q8_0),
        ("token_embd.weight", GGML_F16),
        ("output_norm.weight", GGML_F32),
      ],
    );
    assert!(ds4_compatible(&q2), "q2 IQ2_XXS/Q2_K recipe");
    // q4 Flash: Q4_K experts.
    let q4 = header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_Q4_K),
        ("blk.0.ffn_up_exps.weight", GGML_Q4_K),
        ("blk.0.attn_q.weight", GGML_Q8_0),
        ("token_embd.weight", GGML_F16),
      ],
    );
    assert!(ds4_compatible(&q4), "q4 Q4_K expert recipe");
  }

  #[test]
  fn compat_rejects_for_the_right_reason() {
    // Wrong arch.
    assert!(!ds4_compatible(&header(
      "deepseek2",
      &[("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS)]
    )));
    // Q4_K on an attention (non-expert) projection — a generic K-quant.
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS),
        ("blk.0.attn_q.weight", GGML_Q4_K),
      ]
    )));
    // Q6_K expert (14) — outside the routed-expert set.
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[("blk.0.ffn_gate_exps.weight", 14)]
    )));
    // BF16 tensor (30) anywhere.
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS),
        ("token_embd.weight", 30),
      ]
    )));
    // deepseek4 arch but no expert tensors (metadata-only / truncated).
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[("token_embd.weight", GGML_F16)]
    )));
  }

  // ---- argv builder ----

  #[test]
  fn argv_defaults_only_is_loopback_pinned() {
    let p = params_with(None, &[]);
    assert_eq!(
      argv_strings(&p, 41100),
      vec![
        "-m",
        "/m/ds4flash.gguf",
        "--host",
        "127.0.0.1",
        "--port",
        "41100"
      ]
    );
  }

  #[test]
  fn argv_emits_ctx_and_native_knobs() {
    let p = params_with(
      Some(32768),
      &[
        ("power", "60"),
        ("kv_disk_dir", "/tmp/kv"),
        ("ssd_streaming", "true"),
      ],
    );
    let a = argv_strings(&p, 8000);
    assert!(a.windows(2).any(|w| w == ["--ctx", "32768"]));
    assert!(a.windows(2).any(|w| w == ["--power", "60"]));
    assert!(a.windows(2).any(|w| w == ["--kv-disk-dir", "/tmp/kv"]));
    // Bool knob emits a bare flag.
    assert!(a.contains(&"--ssd-streaming".to_string()));
    // Never a reasoning/jinja flag.
    assert!(!a.iter().any(|s| s == "--jinja" || s.contains("reasoning")));
  }

  #[test]
  fn argv_never_emits_jinja_even_when_params_ask() {
    // ds4's argv builder ignores the llama.cpp-only `jinja` launch knob and
    // the reasoning toggle — neither surfaces on the ds4 command line.
    let mut p = params_with(None, &[("jinja", "true")]);
    p.reasoning = true;
    let a = argv_strings(&p, 8000);
    assert!(!a.iter().any(|s| s == "--jinja"));
    assert!(!a.iter().any(|s| s.contains("reasoning")));
  }

  #[test]
  fn argv_strips_loopback_and_ds4_forbidden_extras() {
    let mut p = params_with(None, &[]);
    p.extras = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--cors"),
      OsString::from("--dist-worker"),
      OsString::from("--power"),
      OsString::from("70"),
    ];
    let a = argv_strings(&p, 8000);
    // Forbidden heads gone; the benign `--power 70` survives.
    assert!(!a.contains(&"--cors".to_string()));
    assert!(!a.contains(&"--dist-worker".to_string()));
    // The security property: no rebind head survives — the only `--host` in
    // argv is our own loopback one. (An orphaned `0.0.0.0` value token is
    // inert without its `--host` flag; the real guard is the fail-fast
    // `forbidden_in_extras_ext` refusal in `compose_and_spawn`, which never
    // lets such extras reach here in production.)
    assert_eq!(a.iter().filter(|s| *s == "--host").count(), 1);
    assert!(a.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
    assert!(a.windows(2).any(|w| w == ["--power", "70"]));
  }

  #[test]
  fn argv_native_value_smuggling_forbidden_head_is_stripped() {
    // A free-text native value that tries to smuggle `--cors` is dropped.
    let p = params_with(None, &[("kv_disk_dir", "/x --cors")]);
    let a = argv_strings(&p, 8000);
    assert!(!a.iter().any(|s| s.contains("--cors")));
  }

  // ---- descriptor table + capabilities ----

  #[test]
  fn native_knob_ids_are_unique_and_documented() {
    let b = Ds4Backend::new();
    let descs = b.native_knobs();
    assert_eq!(
      descs.len(),
      9,
      "ds4 exposes 9 native knobs (6 base + the MTP trio; no quality)"
    );
    let mut ids: Vec<&str> = descs.iter().map(|d| d.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 9, "ids are unique persistence keys");
    for d in descs {
      assert!(!d.label.is_empty(), "{} has a label", d.id);
      assert!(!d.description.is_empty(), "{} has a description", d.id);
      // Every descriptor id has a flag mapping.
      assert!(
        DS4_FLAG_MAP.iter().any(|(id, _)| *id == d.id),
        "{} has a flag mapping",
        d.id
      );
    }
  }

  #[test]
  fn argv_emits_mtp_native_knobs() {
    // The MTP trio rides the native-knob channel → ds4-server flags.
    let p = params_with(
      None,
      &[
        ("mtp", "/m/DeepSeek-V4-Flash-mtp.gguf"),
        ("mtp_draft", "3"),
        ("mtp_margin", "2.5"),
      ],
    );
    let a = argv_strings(&p, 41100);
    assert!(a
      .windows(2)
      .any(|w| w == ["--mtp", "/m/DeepSeek-V4-Flash-mtp.gguf"]));
    assert!(a.windows(2).any(|w| w == ["--mtp-draft", "3"]));
    assert!(a.windows(2).any(|w| w == ["--mtp-margin", "2.5"]));
  }

  #[test]
  fn capabilities_honor_only_ctx() {
    let b = Ds4Backend::new();
    assert!(b.capabilities().supports(KnobField::Ctx));
    assert!(!b.capabilities().supports(KnobField::NGpuLayers));
    assert!(!b.capabilities().supports(KnobField::FlashAttn));
  }

  #[test]
  fn readiness_is_models_endpoint_with_alias_check() {
    match readiness() {
      Readiness::HttpPollModelId {
        path,
        ready_status,
        expect_model_ids,
      } => {
        assert_eq!(path, "/v1/models");
        assert_eq!(ready_status, 200);
        // The prefix (a substring of every ds4 alias body) is what the probe
        // matches on, so `-flash` / `-pro` / a future `-turbo` all pass.
        assert_eq!(expect_model_ids, vec![DS4_ALIAS_PREFIX.to_string()]);
        assert!("deepseek-v4-flash".starts_with(&expect_model_ids[0]));
        assert!("deepseek-v4-turbo".starts_with(&expect_model_ids[0]));
      }
      _ => panic!("ds4 readiness must be HttpPollModelId"),
    }
  }

  #[test]
  fn split_half_guard_is_precise() {
    // The two genuinely-unlaunchable PRO split halves.
    assert!(is_ds4_split_half("DeepSeek-V4-Pro-Q4K-Layers00-30.gguf"));
    assert!(is_ds4_split_half(
      "DeepSeek-V4-Pro-Q4K-Layers-31-output.gguf"
    ));
    // The single-file Flash with a layer range in its name is launchable —
    // MUST NOT be refused.
    assert!(!is_ds4_split_half(
      "DeepSeek-V4-Flash-Layers37-42Q4KExperts-OtherExpertLayersIQ2XXSGateUp-Q2KDown-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix-fixed.gguf"
    ));
    // The standard single-file Flash/PRO quants are not halves.
    assert!(!is_ds4_split_half(
      "DeepSeek-V4-Flash-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix.gguf"
    ));
    assert!(!is_ds4_split_half(
      "DeepSeek-V4-Pro-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-Instruct.gguf"
    ));
    // An unrelated model with no layer split.
    assert!(!is_ds4_split_half("qwen2.5-7b-instruct-q4_k_m.gguf"));
  }

  #[test]
  fn alias_matcher_tolerates_future_variants() {
    assert!(is_ds4_alias("deepseek-v4-flash"));
    assert!(is_ds4_alias("deepseek-v4-pro"));
    assert!(is_ds4_alias("deepseek-v4-turbo")); // future variant
    assert!(!is_ds4_alias("llama"));
    assert!(!is_ds4_alias("deepseek-v3"));
  }

  #[test]
  fn typed_knobs_outside_ctx_never_reach_argv() {
    let mut p = params_with(None, &[]);
    p.knobs.n_gpu_layers = Some(KnobValue::Set(99));
    p.knobs.flash_attn = Some(KnobValue::Set(true));
    let a = argv_strings(&p, 8000);
    assert!(!a.iter().any(|s| s == "-ngl" || s == "--n-gpu-layers"));
    // Precise flag check (the model path itself contains "flash").
    assert!(!a.iter().any(|s| s == "--flash-attn" || s == "-fa"));
  }

  /// Optional real-file check: when the published q2 Flash header is present
  /// in the local HF cache, the predicate must accept it. Ignored by default
  /// (needs the 86 GB download); run with `--ignored` on a UAT box.
  #[test]
  #[ignore]
  fn compat_accepts_real_published_flash_header() {
    let blob = PathBuf::from(
      "/mnt/work/huggingface/hub/models--antirez--deepseek-v4-gguf/blobs/\
       efc7ed607ff27076e3e501fc3fefefa33c0ed8cf1eff483a2b7fdc0c2e616668",
    );
    if !blob.is_file() {
      return;
    }
    let read =
      crate::gguf::header::read_path(&blob, crate::gguf::header::HeaderReadOptions::default())
        .expect("read real ds4 header");
    assert!(
      ds4_compatible(&read.header),
      "real published Flash must be ds4-compatible"
    );
  }

  #[test]
  fn auto_stream_triggers_only_when_residency_exceeds_free() {
    let gib = |g: u64| g * 1024 * 1024 * 1024;
    // 80 GiB Flash weights → ~100 GiB resident estimate.
    assert_eq!(ds4_resident_estimate(gib(80)), gib(100));
    // Won't fit: 100 GiB resident > 95 GiB free → stream (the Strix Halo case).
    assert!(ds4_should_auto_stream(gib(80), gib(95)));
    // Fits with headroom: 100 GiB resident < 200 GiB free → full residency.
    assert!(!ds4_should_auto_stream(gib(80), gib(200)));
    // Exact boundary is not a shortfall (estimate == free → no stream).
    assert!(!ds4_should_auto_stream(gib(80), gib(100)));
    // A pathological weight total saturates instead of overflowing.
    assert!(ds4_should_auto_stream(u64::MAX, gib(100)));
  }

  /// Spin up a tiny single-shot HTTP responder on an OS-assigned loopback port,
  /// returning `(task, port)` — so the alias endpoint check runs against a real
  /// socket without the full `fake_ds4_server` binary.
  async fn spawn_one_shot(status: u16, body: String) -> (tokio::task::JoinHandle<()>, u16) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind(("127.0.0.1", 0))
      .await
      .expect("bind probe responder");
    let port = listener
      .local_addr()
      .expect("probe responder local_addr")
      .port();
    let task = tokio::spawn(async move {
      if let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let header = format!(
          "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        let _ = sock.write_all(header.as_bytes()).await;
        let _ = sock.write_all(body.as_bytes()).await;
        let _ = sock.shutdown().await;
      }
    });
    (task, port)
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn ds4_alias_endpoint_check_is_prefix_tolerant_and_rejects_foreign() {
    use std::time::Duration;
    // ds4 adoption (D-adopt) matches the `/v1/models` id by the `deepseek-v4-`
    // prefix, so a future `-turbo` build adopts; a foreign id on the reused
    // port does not (the PID-reuse guard on the ds4 path).
    let ds4_body = serde_json::json!({
      "object": "list",
      "data": [{"id": "deepseek-v4-turbo", "object": "model"}],
    })
    .to_string();
    let (_resp, port) = spawn_one_shot(200, ds4_body).await;
    assert!(
      models_endpoint_reports_ds4_alias(port, Duration::from_secs(1)).await,
      "a deepseek-v4-* alias must be accepted"
    );

    let foreign_body = serde_json::json!({
      "object": "list",
      "data": [{"id": "llama-3-8b", "object": "model"}],
    })
    .to_string();
    let (_resp2, port2) = spawn_one_shot(200, foreign_body).await;
    assert!(
      !models_endpoint_reports_ds4_alias(port2, Duration::from_secs(1)).await,
      "a foreign id must not be read as a ds4 alias"
    );
  }

  // ---- doctor advisory (D-doctor) ----

  /// Build a config that scans only `dir` (default caches suppressed) with ds4
  /// unavailable — the isolated setup the doctor-advisory tests need.
  fn ds4_scan_config(dir: &std::path::Path) -> Config {
    Config {
      model_paths: vec![dir.to_path_buf()],
      disable_scan: true, // `default_set` then returns only `user_paths`
      ..Config::default()
    }
  }

  #[tokio::test]
  async fn doctor_advisory_fires_for_compatible_model_when_ds4_unavailable() {
    use crate::gguf::test_fixtures::FixtureBuilder;
    let dir = crate::test_support::unique_temp_dir("doctor-ds4", "compat");
    let bytes = FixtureBuilder::new()
      .with_arch("deepseek4")
      .with_tensor("blk.0.ffn_gate_exps.weight", &[512, 512], 16) // IQ2_XXS
      .with_tensor("token_embd.weight", &[512, 512], 1) // F16
      .build();
    std::fs::write(dir.join("deepseek-v4-flash.gguf"), bytes).unwrap();

    let config = ds4_scan_config(&dir);
    let findings = Ds4Backend::new().doctor_findings(&config).await;
    let finding = findings.first().expect("advisory fires");
    assert_eq!(finding.id, "ds4_unavailable");
    assert_eq!(finding.severity, Severity::Info);
    assert!(finding.safe_to_log);
    assert_eq!(finding.fix_hint, DS4_DOCTOR_UNAVAILABLE_FIX);
    std::fs::remove_dir_all(&dir).ok();
  }

  // Sync (not `#[tokio::test]`) so the env-lock guard is held across a plain
  // `block_on`: the advisory folds `LLAMASTASH_MODEL_PATHS` into its scan, so a
  // parallel test setting it must not leak a ds4 model in. `disable_scan` in the
  // config already bounds the scan to `dir` — no need to touch `NO_SCAN` (which
  // would race the `build_options` tests that read it).
  #[test]
  fn doctor_advisory_absent_for_non_ds4_models() {
    use crate::gguf::test_fixtures::build_minimal_gguf;
    let _env = crate::cli::test_lock::serialize();
    let dir = crate::test_support::unique_temp_dir("doctor-ds4", "plain");
    std::fs::write(dir.join("qwen.gguf"), build_minimal_gguf("qwen2")).unwrap();
    // Drop any inherited env path so the scan sees only `dir`.
    let prev_paths = std::env::var_os("LLAMASTASH_MODEL_PATHS");
    std::env::remove_var("LLAMASTASH_MODEL_PATHS");
    let config = ds4_scan_config(&dir);
    let findings = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap()
      .block_on(Ds4Backend::new().doctor_findings(&config));
    match prev_paths {
      Some(v) => std::env::set_var("LLAMASTASH_MODEL_PATHS", v),
      None => std::env::remove_var("LLAMASTASH_MODEL_PATHS"),
    }
    assert!(
      findings.is_empty(),
      "no advisory when no ds4-compatible model is present"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  /// Doctor is config-only, but discovery also honors `LLAMASTASH_MODEL_PATHS`
  /// / `LLAMASTASH_NO_SCAN`. A user who supplies model paths *only* via the env
  /// override must still get the `ds4_unavailable` advisory — regression for
  /// the advisory silently under-scanning when `config.model_paths` is empty.
  // Sync (not `#[tokio::test]`) so the env-lock guard is held across a plain
  // `block_on` rather than an `.await` — else clippy's `await_holding_lock`
  // fires. The env vars must stay set for the whole scan.
  #[test]
  fn doctor_advisory_scans_env_model_paths_when_config_has_none() {
    use crate::gguf::test_fixtures::FixtureBuilder;
    // Serialize against other `LLAMASTASH_MODEL_PATHS` / `LLAMASTASH_NO_SCAN`
    // tests — process-global env vars race across parallel test threads.
    let _env = crate::cli::test_lock::serialize();
    let dir = crate::test_support::unique_temp_dir("doctor-ds4", "envpath");
    let bytes = FixtureBuilder::new()
      .with_arch("deepseek4")
      .with_tensor("blk.0.ffn_gate_exps.weight", &[512, 512], 16) // IQ2_XXS
      .with_tensor("token_embd.weight", &[512, 512], 1) // F16
      .build();
    std::fs::write(dir.join("deepseek-v4-flash.gguf"), bytes).unwrap();

    // Paths come ONLY from the env override; the config carries none. NO_SCAN
    // keeps `default_set` to the env path so the test never touches real caches.
    let prev_paths = std::env::var_os("LLAMASTASH_MODEL_PATHS");
    let prev_noscan = std::env::var_os("LLAMASTASH_NO_SCAN");
    std::env::set_var("LLAMASTASH_MODEL_PATHS", &dir);
    std::env::set_var("LLAMASTASH_NO_SCAN", "1");

    let findings = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap()
      .block_on(Ds4Backend::new().doctor_findings(&Config::default()));

    match prev_paths {
      Some(v) => std::env::set_var("LLAMASTASH_MODEL_PATHS", v),
      None => std::env::remove_var("LLAMASTASH_MODEL_PATHS"),
    }
    match prev_noscan {
      Some(v) => std::env::set_var("LLAMASTASH_NO_SCAN", v),
      None => std::env::remove_var("LLAMASTASH_NO_SCAN"),
    }

    assert_eq!(
      findings
        .first()
        .expect("advisory fires from env-supplied model path")
        .id,
      "ds4_unavailable"
    );
    std::fs::remove_dir_all(&dir).ok();
  }
}
