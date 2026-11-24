//! `llamastash doctor` diagnostic.
//!
//! Re-runs hardware + binary detection, loads `_init_snapshot.json`,
//! compares the two, emits 0-N findings. Every finding carries a
//! stable `id` agent consumers can branch on plus a
//! `fix_hint = "llamastash init --only X"` that maps to the wizard
//! step that resolves it.
//!
//! Read-only unless asked: `--fix` applies the mechanical repair a finding
//! carries (its `fix` id) and `--dry-run` previews that list. Neither
//! stops a daemon nor deletes a model.
//!
//! Output is always safe to paste into a public issue. `safe_to_log` is
//! unconditionally `true`. A future finding that legitimately needs
//! differentiated redaction lands the per-finding flag then, not preemptively.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::backend::Backend;
use crate::cli::cli_args::{Cli, DoctorArgs};
use crate::cli::exit_codes::CliResult;
use crate::config::Config;
use crate::gpu::{ClassSource, GpuInfo};
use crate::init::detection::{detect_hardware, HardwareSnapshot, OsFamily};
use crate::init::external::stale_url;
use crate::init::snapshot::{self, InitSnapshot, InstallMethod};
use crate::util::datetime::{current_yyyymmdd, days_between, parse_yyyymmdd};

/// Memory-drift change threshold: a pool-size change below the
/// larger of this fraction or [`DRIFT_MIN_DELTA_BYTES`] is noise and
/// fires no finding (guards against Windows DXGI flapping).
const DRIFT_MIN_FRACTION: f64 = 0.05;
/// Absolute floor for the drift threshold — 512 MiB.
const DRIFT_MIN_DELTA_BYTES: u64 = 512 * 1024 * 1024;
/// GTT-hint band: a GTT pool sized between these fractions of
/// system RAM is the amdgpu kernel default (~half) and signals the user
/// has not raised the ceiling. Outside the band → no hint.
const GTT_HINT_RATIO_LO: f64 = 0.40;
const GTT_HINT_RATIO_HI: f64 = 0.60;

/// Schema version for `doctor --json`. Bumped on breaking shape
/// changes; current readers refuse a snapshot whose `schema_version`
/// exceeds their max.
///
/// Finding and fix ids stay additive within a version — a reader that does not
/// know an id still parses the report. Only a shape change (new section,
/// renamed or retyped field) earns a bump.
pub const DOCTOR_JSON_SCHEMA_VERSION: u32 = 2;

/// `SnapshotStale` finding fires when the bundled snapshot is older
/// than this many days vs today.
pub const STALE_SNAPSHOT_THRESHOLD_DAYS: u64 = 14;

/// `RemoteSnapshotUnreachable` finding fires after this many
/// consecutive remote-fetch failures.
pub const REMOTE_UNREACHABLE_THRESHOLD: u32 = 3;

/// The hint every finding with an automatic repair carries.
const FIX_WITH_DOCTOR: &str = "llamastash doctor --fix";

/// The hint for a tool config no repair can reach because it cannot be read.
/// Naming `--fix` here would advertise a repair that only answers `skipped`.
const FIX_TOOL_CONFIG_BY_HAND: &str =
  "read the file yourself and point it at the address in `llamastash status --json | jq .proxy.listen`";

/// A mechanical repair `doctor --fix` knows how to apply, serialized as
/// the id on the finding (`fix`) and on its `fixes[]` entry, so an agent
/// can tell which findings are auto-repairable without a table of its own.
/// `apply_fixes` matches it exhaustively: a variant added here without a
/// repair is a compile error, not a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FixId {
  #[serde(rename = "config_chmod_0600")]
  ConfigMode,
  #[serde(rename = "remove_stale_daemon_files")]
  StaleDaemonFiles,
  #[serde(rename = "tool_config_proxy_url")]
  ToolConfigProxyUrl,
}

impl FixId {
  /// The repair's verb, for the human ledger line.
  fn action(self) -> &'static str {
    match self {
      Self::ConfigMode => "chmod 0600",
      Self::StaleDaemonFiles => "remove",
      Self::ToolConfigProxyUrl => "rewrite url",
    }
  }
}

/// Stable finding ids. Agent consumers branch on these — never change
/// a string here without bumping `DOCTOR_JSON_SCHEMA_VERSION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingId {
  BinaryMissing,
  BinaryDigestDrift,
  HardwareDrift,
  MemoryDrift,
  GttHint,
  SnapshotStale,
  ConfigModeDrift,
  StaleDaemonFiles,
  RemoteSnapshotUnreachable,
  ToolConfigStaleUrl,
}

impl FindingId {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::BinaryMissing => "binary_missing",
      Self::BinaryDigestDrift => "binary_digest_drift",
      Self::HardwareDrift => "hardware_drift",
      Self::MemoryDrift => "memory_drift",
      Self::GttHint => "gtt_hint",
      Self::SnapshotStale => "snapshot_stale",
      Self::ConfigModeDrift => "config_mode_drift",
      Self::StaleDaemonFiles => "stale_daemon_files",
      Self::RemoteSnapshotUnreachable => "remote_snapshot_unreachable",
      Self::ToolConfigStaleUrl => "tool_config_stale_url",
    }
  }

  pub fn fix_hint(self) -> &'static str {
    match self {
      Self::BinaryMissing | Self::BinaryDigestDrift | Self::HardwareDrift => {
        "llamastash init --only server"
      }
      Self::MemoryDrift => "(no action — the baseline has been refreshed to the new size)",
      Self::GttHint => {
        "(optional — raise `amdgpu.gttsize` / `ttm.pages_limit` to let llama-server use more system RAM; see docs/troubleshooting.md)"
      }
      Self::SnapshotStale => {
        "run `llamastash recommend` (or `init`) to pull the latest snapshot — the recommender prefers it over the bundled one; upgrade for a fresher bundled snapshot"
      }
      Self::RemoteSnapshotUnreachable => {
        "the remote snapshot fetch keeps failing — check network / egress; the recommender falls back to the bundled snapshot until it recovers"
      }
      // Repairable ids never reach a manual hint while the repair exists —
      // `Finding::new` points them at `doctor --fix`. These arms are what
      // they say if a repair is ever withheld or gated out, so no id can be
      // left without advice, and they name the hand step rather than
      // repeating the `--fix` line.
      Self::ConfigModeDrift => {
        "run `chmod 600` on the config file (or `llamastash init --only config`)"
      }
      Self::StaleDaemonFiles => "remove `runtime.json` and `daemon.pid` from the state dir",
      Self::ToolConfigStaleUrl => {
        "re-run `llamastash init --integrations <ids>` (which also rewrites the model list)"
      }
    }
  }

  /// The repair `doctor --fix` applies for this finding, or `None` when
  /// nothing mechanical fixes it.
  pub fn fix_action(self) -> Option<FixId> {
    match self {
      // The finding itself only exists on unix (POSIX file modes), so its
      // repair is gated the same way.
      #[cfg(unix)]
      Self::ConfigModeDrift => Some(FixId::ConfigMode),
      Self::StaleDaemonFiles => Some(FixId::StaleDaemonFiles),
      Self::ToolConfigStaleUrl => Some(FixId::ToolConfigProxyUrl),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
  Info,
  Warning,
  Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
  pub id: &'static str,
  pub severity: Severity,
  pub message: String,
  pub fix_hint: &'static str,
  pub safe_to_log: bool,
  /// Set when `doctor --fix` repairs this finding mechanically. Absent
  /// when it does not, so the pre-`--fix` shape of a finding is unchanged.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub fix: Option<FixId>,
  /// The repair this finding wanted but `--fix` withheld, with the path it
  /// would have acted on and why. Internal: it drives the ledger line for
  /// the blocked repair, while `fix` stays absent so a pre-`--fix` consumer
  /// sees the shape it already knows.
  #[serde(skip)]
  withheld: Option<(FixId, String, String)>,
}

impl Finding {
  fn new(id: FindingId, severity: Severity, message: impl Into<String>) -> Self {
    let fix = id.fix_action();
    // The hint follows the repair: a finding that `--fix` will not act on
    // never tells the reader to run `--fix`.
    let fix_hint = if fix.is_some() {
      FIX_WITH_DOCTOR
    } else {
      id.fix_hint()
    };
    Self {
      fix,
      ..Self::from_parts(id.as_str(), severity, message, fix_hint)
    }
  }

  /// A finding about something `--fix` cannot fix at all, whose hint is the
  /// hand step the reader takes instead. Promises no repair and withholds
  /// none.
  fn manual(
    id: FindingId,
    severity: Severity,
    message: impl Into<String>,
    hint: &'static str,
  ) -> Self {
    Self::from_parts(id.as_str(), severity, message, hint)
  }

  /// The id's repair exists but could not run on `target` for `why`: nothing
  /// is advertised on the finding, and `--fix` ledges it as skipped instead
  /// of staying quiet.
  #[cfg(unix)]
  fn blocked(
    id: FindingId,
    severity: Severity,
    message: impl Into<String>,
    target: &Path,
    why: String,
  ) -> Self {
    Self {
      withheld: id
        .fix_action()
        .map(|fix| (fix, target.display().to_string(), why)),
      ..Self::from_parts(id.as_str(), severity, message, id.fix_hint())
    }
  }

  /// Construct a finding from a stable string `id` + verbatim `fix_hint` — the
  /// path a backend uses to contribute a finding through [`crate::backend::Backend::doctor_findings`]
  /// without a [`FindingId`] variant. `safe_to_log` is unconditionally `true`,
  /// matching the v2 invariant that every finding is safe to paste publicly.
  pub fn from_parts(
    id: &'static str,
    severity: Severity,
    message: impl Into<String>,
    fix_hint: &'static str,
  ) -> Self {
    Self {
      id,
      severity,
      message: message.into(),
      fix_hint,
      safe_to_log: true,
      fix: None,
      withheld: None,
    }
  }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Baseline {
  pub snapshot_bundle_date: Option<String>,
  pub init_date: Option<String>,
}

/// Live hardware section of the doctor report. Built from the
/// same [`HardwareSnapshot`] the init banner renders, with the
/// label conventions (`MEM`/`MEM*`, `VRAM (shared)`).
#[derive(Debug, Clone, Serialize)]
pub struct HardwareSection {
  pub cpu_brand: String,
  pub cpu_cores: u32,
  /// Inference-relevant CPU instruction sets (AVX2, AVX-512, FMA, …).
  /// Shown so `doctor` is the superset of what `init` reports. Empty on
  /// archs without a meaningful surface.
  #[serde(default)]
  pub cpu_features: Vec<String>,
  /// OS family + CPU arch (`linux/x86_64`), mirroring the init banner's
  /// `sys:` line so the two surfaces agree.
  #[serde(default)]
  pub os: String,
  /// System RAM total in bytes — rendered `MEM` (discrete) or `MEM*`
  /// (unified, where the GPU draws from this same pool).
  pub mem_total_bytes: u64,
  pub disk_free_bytes: u64,
  pub gpu_backend: String,
  /// Whether the GPU shares the system memory pool (Apple, AMD/Intel
  /// UMA APU).
  pub unified: bool,
  /// How the unified-vs-discrete verdict was reached. `None` on
  /// Apple Metal (unified by construction) and non-classifying backends.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub uma_class_source: Option<ClassSource>,
  /// Raw GPU memory ceiling — the aggregated pool total the recommender
  /// sizes against. For a UMA APU this is carve-out + GTT. `None` on
  /// CPU-only / unknown hosts.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub gpu_pool_total_bytes: Option<u64>,
  /// UMA composition: the small BIOS-dedicated VRAM carve-out.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub uma_carve_bytes: Option<u64>,
  /// UMA composition: the system-RAM-backed shared pool, rendered
  /// `VRAM (shared)` as a breakdown *of* `MEM*` (not a separate pool).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub uma_shared_bytes: Option<u64>,
}

impl HardwareSection {
  fn from_hardware(hw: &HardwareSnapshot) -> Self {
    let (uma_carve_bytes, uma_shared_bytes) = uma_composition(&hw.gpu);
    Self {
      cpu_brand: hw.cpu_brand.clone(),
      cpu_cores: hw.cpu_cores,
      cpu_features: hw.cpu_features.clone(),
      os: format!(
        "{}/{}",
        crate::init::prompts::os_short(hw.os),
        crate::init::prompts::arch_short(hw.cpu_arch)
      ),
      mem_total_bytes: hw.ram_total_bytes,
      disk_free_bytes: hw.disk_free_bytes,
      gpu_backend: hw.gpu.label().to_string(),
      unified: hw.gpu.is_unified(),
      uma_class_source: hw.gpu.uma_class_source(),
      gpu_pool_total_bytes: hw.vram_bytes,
      uma_carve_bytes,
      uma_shared_bytes,
    }
  }
}

/// Pull the UMA pool composition `(carve, shared_gtt)` from the first
/// unified device, where `total = carve + shared`. `None` for discrete
/// hosts and for Apple Metal (genuinely unified, no carve split).
fn uma_composition(gpu: &GpuInfo) -> (Option<u64>, Option<u64>) {
  let devices = match gpu {
    GpuInfo::Nvidia { devices }
    | GpuInfo::Amd { devices }
    | GpuInfo::Unknown { devices }
    | GpuInfo::Multi { devices } => devices,
    GpuInfo::AppleMetal { .. } | GpuInfo::CpuOnly => return (None, None),
  };
  devices
    .iter()
    .find_map(|d| {
      d.uma_shared_total_bytes.map(|shared| {
        let carve = d.total_memory_bytes.saturating_sub(shared);
        (Some(carve), Some(shared))
      })
    })
    .unwrap_or((None, None))
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
  pub schema_version: u32,
  pub findings: Vec<Finding>,
  pub baseline: Baseline,
  pub hardware: HardwareSection,
  /// Repairs applied (or, under `--dry-run`, proposed) for the findings
  /// that carry a `fix`. Empty — and therefore an empty JSON array —
  /// whenever `doctor` ran read-only.
  pub fixes: Vec<FixReport>,
}

/// How one `--fix` repair ended. `WouldApply` is the `--dry-run` answer
/// for the same repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixOutcome {
  Applied,
  WouldApply,
  Skipped,
  Failed,
}

/// One line of the `--fix` ledger: what was targeted, and what happened.
#[derive(Debug, Clone, Serialize)]
pub struct FixReport {
  pub fix: FixId,
  /// Verb of the repair, for the human line (`chmod 0600`, `remove`).
  pub action: &'static str,
  pub target: String,
  pub outcome: FixOutcome,
  pub detail: String,
}

impl FixReport {
  fn new(
    fix: FixId,
    target: impl Into<String>,
    outcome: FixOutcome,
    detail: impl Into<String>,
  ) -> Self {
    Self {
      fix,
      action: fix.action(),
      target: target.into(),
      outcome,
      detail: detail.into(),
    }
  }

  fn skipped(fix: FixId, target: impl Into<String>, detail: impl Into<String>) -> Self {
    Self::new(fix, target, FixOutcome::Skipped, detail)
  }
}

/// Build the report. Pure-ish: reads the on-disk snapshot + re-detects
/// hardware/binary but never mutates anything.
pub fn build_report(snapshot: Option<&InitSnapshot>, hardware: &HardwareSnapshot) -> DoctorReport {
  let mut findings: Vec<Finding> = Vec::new();
  let baseline = Baseline {
    snapshot_bundle_date: snapshot.and_then(|s| s.snapshot_bundle_date.clone()),
    init_date: snapshot.and_then(|s| s.init_date.clone()),
  };
  let hardware_section = HardwareSection::from_hardware(hardware);

  // The GTT hint is hardware-only — it fires even before init has run.
  if let Some(finding) = check_gtt_hint(hardware) {
    findings.push(finding);
  }

  let Some(snapshot) = snapshot else {
    return DoctorReport {
      schema_version: DOCTOR_JSON_SCHEMA_VERSION,
      findings,
      baseline,
      hardware: hardware_section,
      fixes: Vec::new(),
    };
  };

  if let Some(finding) = check_binary_missing(snapshot) {
    findings.push(finding);
  }
  if let Some(finding) = check_binary_digest_drift(snapshot) {
    findings.push(finding);
  }
  if let Some(finding) = check_hardware_drift(snapshot, hardware) {
    findings.push(finding);
  }
  if let Some(finding) = check_memory_drift(snapshot, hardware) {
    findings.push(finding);
  }
  if let Some(finding) = check_snapshot_stale(snapshot) {
    findings.push(finding);
  }
  findings.extend(check_config_mode_drift());
  if let Some(finding) = check_remote_snapshot_unreachable(snapshot) {
    findings.push(finding);
  }
  DoctorReport {
    schema_version: DOCTOR_JSON_SCHEMA_VERSION,
    findings,
    baseline,
    hardware: hardware_section,
    fixes: Vec::new(),
  }
}

fn check_binary_missing(snapshot: &InitSnapshot) -> Option<Finding> {
  let path = snapshot.llama_server_path.as_ref()?;
  if path.is_file() && is_readable(path) {
    return None;
  }
  Some(Finding::new(
    FindingId::BinaryMissing,
    Severity::Error,
    format!(
      "`{}` is missing or unreadable — reinstall `llama-server`",
      path.display()
    ),
  ))
}

fn check_binary_digest_drift(snapshot: &InitSnapshot) -> Option<Finding> {
  // Brew carve-out: digest drift after `brew upgrade` is normal; we
  // don't surface it.
  let install_method = snapshot.install_method?;
  if install_method != InstallMethod::GhReleases {
    return None;
  }
  let path = snapshot.llama_server_path.as_ref()?;
  let expected = snapshot.llama_server_digest.as_ref()?;
  let actual = match crate::init::install::sha256_file(path) {
    Ok(d) => d,
    Err(_) => return None, // BinaryMissing already covers this path
  };
  if &actual == expected {
    return None;
  }
  Some(Finding::new(
    FindingId::BinaryDigestDrift,
    Severity::Warning,
    format!(
      "SHA-256 of `{}` ({}) differs from the recorded digest ({}); \
       binary may have been replaced or corrupted",
      path.display(),
      short_hex(&actual),
      short_hex(expected),
    ),
  ))
}

fn check_hardware_drift(snapshot: &InitSnapshot, hardware: &HardwareSnapshot) -> Option<Finding> {
  let prior_vendor = snapshot.gpu_vendor.as_deref()?;
  if prior_vendor == hardware.gpu.label() {
    return None;
  }
  Some(Finding::new(
    FindingId::HardwareDrift,
    Severity::Warning,
    format!(
      "GPU vendor changed from `{prior_vendor}` to `{}` since init — \
       reinstall to pick the right `llama-server` variant",
      hardware.gpu.label()
    ),
  ))
}

/// Memory-drift finding. Compares the freshly-detected GPU pool
/// ceiling against the recorded baseline. `None` when there is no
/// baseline yet (the stamp happens in [`run`]), when the host is
/// CPU-only, or when the change is within the noise threshold. Growth
/// is informational; shrinkage is a warning (a model that fit may no
/// longer). The baseline is re-stamped by [`run`] after this fires.
fn check_memory_drift(snapshot: &InitSnapshot, hardware: &HardwareSnapshot) -> Option<Finding> {
  let baseline = snapshot.gpu_pool_total_bytes?;
  let current = hardware.vram_bytes?;
  let delta = baseline.abs_diff(current);
  let threshold = ((baseline as f64 * DRIFT_MIN_FRACTION) as u64).max(DRIFT_MIN_DELTA_BYTES);
  if delta < threshold {
    return None;
  }
  let (severity, verb) = if current > baseline {
    (Severity::Info, "grew")
  } else {
    (Severity::Warning, "shrank")
  };
  Some(Finding::new(
    FindingId::MemoryDrift,
    severity,
    format!(
      "GPU memory pool {verb} from {} to {} since the last baseline",
      fmt_gib(baseline),
      fmt_gib(current)
    ),
  ))
}

/// GTT-cap hint. Fires on Linux unified hosts whose GTT pool is
/// sized at the amdgpu kernel default (~half of system RAM), signalling
/// the user has headroom to raise the ceiling. Never suggests
/// `amd_iommu=off` (it breaks Thunderbolt docks).
fn check_gtt_hint(hardware: &HardwareSnapshot) -> Option<Finding> {
  if hardware.os != OsFamily::Linux || !hardware.gpu.is_unified() {
    return None;
  }
  let (_carve, shared) = uma_composition(&hardware.gpu);
  let gtt = shared?;
  let ram = hardware.ram_total_bytes;
  if ram == 0 {
    return None;
  }
  let ratio = gtt as f64 / ram as f64;
  if !(GTT_HINT_RATIO_LO..=GTT_HINT_RATIO_HI).contains(&ratio) {
    return None;
  }
  Some(Finding::new(
    FindingId::GttHint,
    Severity::Info,
    format!(
      "GPU shared pool ({}) is about half of system RAM ({}) — the amdgpu default; \
       raising the GTT ceiling lets llama-server use more system RAM as GPU memory",
      fmt_gib(gtt),
      fmt_gib(ram)
    ),
  ))
}

use crate::init::detection::fmt_gib;

/// Info fix hint for the configured-servers summary — nothing to act on.
const SERVERS_CONFIGURED_FIX: &str = "(no action — informational)";
/// Warning fix hint when a configured server binary path no longer exists.
const SERVER_MISSING_FIX: &str =
  "fix or remove the `backend.<id>.servers[].binary` path that no longer exists (see docs/usage.md#servers)";

/// Server-catalog advisory (config-only, no daemon): warn on a configured
/// `servers:` binary that no longer resolves to a file, and — when any server
/// is configured — summarize the resolvable servers with their probed device
/// counts. Stays silent on a default install that configures no `servers:` (the
/// primary is PATH-resolved and covered by `BinaryMissing`). Additive string
/// finding ids, so the doctor schema stays 2.
fn check_servers(config: &Config) -> Vec<Finding> {
  let mut out = Vec::new();
  let missing = crate::backend::missing_configured_servers(config);
  for (backend_id, path) in &missing {
    out.push(Finding::from_parts(
      "server_binary_missing",
      Severity::Warning,
      format!(
        "configured {backend_id} server binary not found: {}",
        path.display()
      ),
      SERVER_MISSING_FIX,
    ));
  }
  let catalog = crate::backend::config_server_catalog(config);
  if catalog.is_empty() && missing.is_empty() {
    return out;
  }
  let summary = catalog
    .iter()
    .map(|s| {
      // Selectors and GPUs differ when one build reports a card under two
      // compute APIs; say so rather than claiming a second GPU.
      let gpus = s.physical_device_count();
      match (gpus, s.devices.len()) {
        (0, _) => s.id.clone(),
        (n, sel) if sel > n => {
          let unit = if n == 1 { "GPU" } else { "GPUs" };
          format!("{} ({n} {unit}, {sel} selectors)", s.id)
        }
        (1, _) => format!("{} (1 GPU)", s.id),
        (n, _) => format!("{} ({n} GPUs)", s.id),
      }
    })
    .collect::<Vec<_>>()
    .join(", ");
  let msg = if summary.is_empty() {
    format!(
      "{} configured server binary/binaries not found",
      missing.len()
    )
  } else {
    format!("{} configured server(s): {summary}", catalog.len())
  };
  out.push(Finding::from_parts(
    "servers_configured",
    Severity::Info,
    msg,
    SERVERS_CONFIGURED_FIX,
  ));
  out
}

/// Freshen the persisted snapshot's `bundle_date` against the latest
/// available remote so the staleness check reflects what the recommender
/// would actually use, not the binary's bundled date. The recommender
/// already prefers a verified-fresher remote (`benchmark::load_remote`);
/// doctor must too or it cries wolf about a snapshot the picks don't even
/// use. In-memory only — doctor's sole persisted write stays the memory
/// baseline. Skips the network entirely when the local snapshot is
/// already fresh, or when offline (`LLAMASTASH_OFFLINE`).
async fn freshen_snapshot_date(snapshot: Option<InitSnapshot>) -> Option<InitSnapshot> {
  let mut snap = snapshot?;
  let bundled = crate::init::benchmark::load_bundled();
  // Local freshest = max(date init recorded, this binary's bundled date).
  let mut freshest = snap.snapshot_bundle_date.clone().unwrap_or_default();
  if bundled.bundle_date > freshest {
    freshest = bundled.bundle_date.clone();
  }
  // Only pay a network round-trip when the local snapshot already looks
  // stale — a fresh install never reaches out.
  if date_is_stale(&freshest) {
    if let Ok(fetch) = crate::init::fetch::build_with_offline_check(
      false,
      crate::init::fetch::FetchClientConfig::default(),
    ) {
      if !fetch.is_offline() {
        if let Ok(Some(remote)) = crate::init::benchmark::load_remote(&fetch, &bundled).await {
          if remote.bundle_date > freshest {
            freshest = remote.bundle_date;
          }
        }
      }
    }
  }
  if !freshest.is_empty() {
    snap.snapshot_bundle_date = Some(freshest);
  }
  Some(snap)
}

/// `true` when a `YYYY-MM-DD` date is older than the stale threshold.
/// Mirrors `check_snapshot_stale`'s arithmetic so the "should I probe the
/// remote?" gate and the finding agree.
fn date_is_stale(date: &str) -> bool {
  let Some(now) = current_yyyymmdd() else {
    return false;
  };
  let (Some(then), Some(parsed_now)) = (parse_yyyymmdd(date), parse_yyyymmdd(&now)) else {
    return false;
  };
  days_between(then, parsed_now)
    .map(|d| d > STALE_SNAPSHOT_THRESHOLD_DAYS)
    .unwrap_or(false)
}

fn check_snapshot_stale(snapshot: &InitSnapshot) -> Option<Finding> {
  let bundle_date = snapshot.snapshot_bundle_date.as_deref()?;
  let now = current_yyyymmdd()?;
  let bundled = parse_yyyymmdd(bundle_date)?;
  let then = parse_yyyymmdd(&now)?;
  let delta_days = days_between(bundled, then)?;
  if delta_days <= STALE_SNAPSHOT_THRESHOLD_DAYS {
    return None;
  }
  Some(Finding::new(
    FindingId::SnapshotStale,
    Severity::Info,
    format!(
      "benchmark snapshot in use is {delta_days} days old and no fresher \
       one was reachable — recommender picks may be stale"
    ),
  ))
}

/// Config hardening: the file's own mode, and whether a `chmod` can be run
/// on it safely at all. Both are reported; the mode drift only advertises a
/// repair when [`chmod_plan`] says the repair would act.
fn check_config_mode_drift() -> Vec<Finding> {
  let Some(path) = crate::util::paths::user_config_file() else {
    return Vec::new();
  };
  if !path.exists() {
    return Vec::new();
  }
  #[cfg(unix)]
  {
    let Some(plan) = chmod_plan(&path) else {
      return Vec::new();
    };
    let mut findings = Vec::new();
    if let Some((dir, why)) = &plan.blocked {
      // No repair here: replacing or re-owning a directory another user
      // could swap into is not a mechanical repair.
      findings.push(Finding::manual(
        FindingId::ConfigModeDrift,
        Severity::Warning,
        format!("parent dir `{}` {why}", dir.display()),
        "run `chmod go-w` on that dir, or move the config out of it",
      ));
    }
    if plan.mode != 0o600 {
      let message = format!(
        "`{}` is mode {:#o} (expected 0600) — \
         re-run init or `chmod 600` to restore the hardening",
        path.display(),
        plan.mode
      );
      findings.push(match &plan.blocked {
        None => Finding::new(FindingId::ConfigModeDrift, Severity::Warning, message),
        Some((_, why)) => Finding::blocked(
          FindingId::ConfigModeDrift,
          Severity::Warning,
          message,
          &plan.real,
          why.clone(),
        ),
      });
    }
    findings
  }
  #[cfg(not(unix))]
  {
    let _ = path;
    Vec::new()
  }
}

/// What an automated `chmod` on the config file would touch, and whether it
/// may run. `set_permissions` follows a symlink, so a writable directory at
/// either end — the config dir, or the dir the link resolves into — lets that
/// directory's writer choose what the repair hits. The report and the repair
/// both ask this, so a finding cannot advertise a repair the repair would
/// then refuse.
#[cfg(unix)]
struct ChmodPlan {
  /// The resolved file the `chmod` would write.
  real: PathBuf,
  mode: u32,
  regular: bool,
  /// The directory that blocks the repair, with the reason.
  blocked: Option<(PathBuf, String)>,
}

#[cfg(unix)]
fn chmod_plan(path: &Path) -> Option<ChmodPlan> {
  use std::os::unix::fs::PermissionsExt;
  let our_uid = unsafe { libc::geteuid() };
  let real = crate::util::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
  let meta = std::fs::metadata(&real).ok()?;
  let blocked = [
    path.parent().unwrap_or(path.as_ref()),
    real.parent().unwrap_or(real.as_ref()),
  ]
  .into_iter()
  .find_map(|dir| {
    crate::util::file_security::dir_swap_surface(dir, our_uid)
      .map(|surface| (dir.to_path_buf(), surface.describe(our_uid)))
  });
  Some(ChmodPlan {
    mode: meta.permissions().mode() & 0o777,
    regular: meta.is_file(),
    real,
    blocked,
  })
}

/// `daemon.pid` with no flock holder is left over from a dead daemon by
/// construction (see [`crate::daemon::lockfile`]), and the `runtime.json`
/// beside it is a handshake pointing at a control-plane URL nothing
/// listens on. `daemon start` rebinds both, so this is untidiness rather
/// than breakage — but a client that reads `runtime.json` first aims at
/// the dead URL until it does.
fn check_stale_daemon_files(state_dir: &Path) -> Option<Finding> {
  if crate::daemon::existing_daemon_pid(state_dir).is_some() {
    return None;
  }
  let leftovers = stale_daemon_files(state_dir);
  if leftovers.is_empty() {
    return None;
  }
  let names = leftovers
    .iter()
    .filter_map(|p| p.file_name())
    .map(|n| n.to_string_lossy())
    .collect::<Vec<_>>()
    .join(", ");
  Some(Finding::new(
    FindingId::StaleDaemonFiles,
    Severity::Warning,
    format!(
      "`{}` still holds {names} from a daemon that is not running — no process owns the lock",
      state_dir.display()
    ),
  ))
}

fn stale_daemon_files(state_dir: &Path) -> Vec<PathBuf> {
  [
    crate::daemon::runtime_file::path(state_dir),
    state_dir.join("daemon.pid"),
  ]
  .into_iter()
  .filter(|p| p.exists())
  .collect()
}

/// One warning per tool config naming a proxy address the live proxy is not
/// on. The caller only asks when a proxy is answering — `doctor` never spawns
/// one to compare against — and one finding per file, because the repair is
/// per file too. `listen` is the address the daemon reports, the same one the
/// repair rewrites to.
fn check_tool_config_urls(live: &stale_url::LiveProxy, listen: &str) -> Vec<Finding> {
  stale_url::scan(live)
    .into_iter()
    .map(|scan| url_finding(scan, listen))
    .collect()
}

/// The message of a stale-address finding. Both addresses appear in words as
/// well as inside the `stale → live` list: a rewrite aimed at the wrong
/// port has to read wrong on its own, not only when the arrow is read back.
fn stale_url_message(path: &Path, found: &[String], listen: &str, changes: &str) -> String {
  let head = match (found.first(), found.len()) {
    (Some(first), 1) => format!("`{}` points at {first}", path.display()),
    (Some(first), n) => format!("`{}` points at {first} and {} more", path.display(), n - 1),
    (None, _) => format!("`{}` points at a proxy address", path.display()),
  };
  // One address gets no list after it, or the same pair prints twice; several
  // get the count once, in the head, and the capped list after it.
  if found.len() > 1 {
    format!("{head}, which nothing listens on; the proxy listens on {listen} — {changes}")
  } else {
    format!("{head}, which nothing listens on; the proxy listens on {listen}")
  }
}

/// The finding for one scanned config. A stale address carries the repair;
/// an unreadable file carries no repair at all, because the only thing `--fix`
/// could do with it is refuse — and its hint is the hand step instead.
fn url_finding(scan: stale_url::Scan, listen: &str) -> Finding {
  match scan {
    stale_url::Scan::Stale { path, urls } => Finding::new(
      FindingId::ToolConfigStaleUrl,
      Severity::Warning,
      stale_url_message(
        &path,
        &urls.iter().map(|u| u.found.clone()).collect::<Vec<_>>(),
        listen,
        &stale_url::changes(&urls),
      ),
    ),
    // Not a repair that failed — an address that could not be read is one
    // nothing checked, which is not the same as nothing found.
    stale_url::Scan::Unreadable { path, reason } => Finding::manual(
      FindingId::ToolConfigStaleUrl,
      Severity::Warning,
      format!(
        "`{}` could not be read, so its proxy address was not checked: {reason}",
        path.display()
      ),
      FIX_TOOL_CONFIG_BY_HAND,
    ),
  }
}

/// Apply every repair the report marks fixable, in finding order. Under
/// `dry_run` nothing is touched and every entry comes back `would_apply`.
/// The set is bounded on purpose: no daemon is signalled, no model is
/// deleted, and daemon state is only removed once nothing holds the lock.
/// A repair a finding wanted but could not get is ledged against the path it
/// wanted, so `--fix` is never silent about what it left alone. Short of
/// deleting the stale daemon files, a stale proxy address is the only file
/// content any of it rewrites.
fn apply_fixes(
  findings: &[Finding],
  dry_run: bool,
  live: Option<&stale_url::LiveProxy>,
) -> Vec<FixReport> {
  let mut fixes: Vec<FixReport> = Vec::new();
  // The URL rewrite covers every stale config in one pass, so it runs on the
  // first finding that wants it rather than once per file.
  let mut proxy_urls_done = false;
  for f in findings {
    if let Some((fix, target, why)) = &f.withheld {
      // The finding already names the path the repair wanted, so the ledger
      // points there rather than at whatever the repair id implies.
      fixes.push(FixReport::skipped(
        *fix,
        target.clone(),
        format!("withheld: {why}; manual step: {}", f.fix_hint),
      ));
      continue;
    }
    let Some(fix) = f.fix else { continue };
    match fix {
      FixId::ConfigMode => {
        // The finding only exists on unix, so neither does the repair.
        #[cfg(unix)]
        if let Some(path) = crate::util::paths::user_config_file() {
          fixes.push(fix_config_mode(&path, dry_run));
        }
      }
      FixId::StaleDaemonFiles => {
        if let Some(dir) = crate::util::paths::state_dir() {
          fixes.extend(fix_stale_daemon_files(&dir, dry_run));
        }
      }
      FixId::ToolConfigProxyUrl => {
        if !proxy_urls_done {
          proxy_urls_done = true;
          match live {
            Some(live) => {
              for scan in stale_url::scan(live) {
                if let stale_url::Scan::Stale { path, .. } = scan {
                  fixes.push(fix_tool_config_url(&path, live, dry_run))
                }
              }
            }
            // The finding only fires while a proxy answers, so reaching here
            // without one means the proxy went away after the report was
            // built. There is then no address to rewrite to, and `--fix`
            // promised never to be silent about what it left alone.
            None => {
              // `target` names a path for every other repair, so it stays a
              // path here even though this repair spans several files.
              let target = crate::util::paths::config_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| "the tool configs `init` writes".to_string());
              fixes.push(FixReport::skipped(
                FixId::ToolConfigProxyUrl,
                target,
                "no proxy is answering, so there is no address to rewrite to; \
                 start the daemon and run --fix again",
              ))
            }
          }
        }
      }
    }
  }
  fixes
}

/// Put the config file back to the `0600` it ships with.
///
/// [`chmod_plan`] decides whether that is allowed and which file it means, so
/// the repair cannot act on something the report did not already refuse.
#[cfg(unix)]
fn fix_config_mode(path: &Path, dry_run: bool) -> FixReport {
  use std::os::unix::fs::PermissionsExt;
  let Some(plan) = chmod_plan(path) else {
    return FixReport::new(
      FixId::ConfigMode,
      path.display().to_string(),
      FixOutcome::Failed,
      "could not read the config file",
    );
  };
  let target = plan.real.display().to_string();
  if let Some((_, why)) = &plan.blocked {
    return FixReport::skipped(FixId::ConfigMode, target, why.clone());
  }
  if !plan.regular {
    return FixReport::skipped(FixId::ConfigMode, target, "not a regular file");
  }
  let mode = plan.mode;
  if mode == 0o600 {
    return FixReport::skipped(FixId::ConfigMode, target, "already 0600");
  }
  let detail = format!("mode {mode:#o} \u{2192} 0600");
  if dry_run {
    return FixReport::new(FixId::ConfigMode, target, FixOutcome::WouldApply, detail);
  }
  match std::fs::set_permissions(&plan.real, std::fs::Permissions::from_mode(0o600)) {
    Ok(()) => FixReport::new(FixId::ConfigMode, target, FixOutcome::Applied, detail),
    Err(e) => FixReport::new(FixId::ConfigMode, target, FixOutcome::Failed, e.to_string()),
  }
}

/// Remove a leftover `runtime.json` / `daemon.pid`. `lockfile::acquire` is
/// what makes this safe to automate: it succeeds only when no daemon holds
/// the flock, so a daemon that started after the check answers
/// `AlreadyRunning` instead of losing a live pidfile. Acquiring also claims
/// the pidfile, and the guard's `Drop` unlinks it.
fn fix_stale_daemon_files(state_dir: &Path, dry_run: bool) -> Vec<FixReport> {
  use crate::daemon::lockfile::{acquire, AcquireOutcome};
  let leftovers = stale_daemon_files(state_dir);
  let dir = state_dir.display().to_string();
  if leftovers.is_empty() {
    return vec![FixReport::skipped(
      FixId::StaleDaemonFiles,
      dir,
      "nothing left over",
    )];
  }
  if dry_run {
    return leftovers
      .iter()
      .map(|p| {
        FixReport::new(
          FixId::StaleDaemonFiles,
          p.display().to_string(),
          FixOutcome::WouldApply,
          "no process owns the lock",
        )
      })
      .collect();
  }
  match acquire(state_dir) {
    Ok(AcquireOutcome::Acquired(mut guard)) => {
      let pidfile = guard.path().to_path_buf();
      // Everything destructive happens while the lock is held, so no daemon
      // that starts afterwards can lose a file it just wrote. The pidfile is
      // unlinked here rather than by the guard's `Drop` so the verdict comes
      // from our own `remove_file` instead of a later `exists()` that a
      // fresh daemon could have made true again, and `disarm` keeps that
      // `Drop` from unlinking a pidfile a new daemon has meanwhile created
      // at the same name.
      crate::daemon::runtime_file::remove(state_dir);
      let runtime_gone = !crate::daemon::runtime_file::path(state_dir).exists();
      let pidfile_gone = std::fs::remove_file(&pidfile).is_ok() || !pidfile.exists();
      guard.disarm();
      drop(guard);
      leftovers
        .iter()
        .map(|p| {
          let target = p.display().to_string();
          let gone = if *p == pidfile {
            pidfile_gone
          } else {
            runtime_gone
          };
          if gone {
            FixReport::new(
              FixId::StaleDaemonFiles,
              target,
              FixOutcome::Applied,
              "handshake with no lock holder",
            )
          } else {
            FixReport::new(
              FixId::StaleDaemonFiles,
              target,
              FixOutcome::Failed,
              "file is still there",
            )
          }
        })
        .collect()
    }
    Ok(AcquireOutcome::AlreadyRunning { pid, .. }) => vec![FixReport::skipped(
      FixId::StaleDaemonFiles,
      dir,
      format!("a daemon (pid {pid}) holds the lock; nothing was removed"),
    )],
    Err(e) => vec![FixReport::new(
      FixId::StaleDaemonFiles,
      dir,
      FixOutcome::Failed,
      e.to_string(),
    )],
  }
}

/// Point one tool config at the address the proxy actually listens on. Only
/// each stale URL's `host:port` moves: the path the file already carries
/// (`/v1` for the OpenAI-shape tools, none for Claude Code) and every other
/// byte stay where they are, and a config whose URL names another host is not
/// ours to rewrite.
fn fix_tool_config_url(path: &Path, live: &stale_url::LiveProxy, dry_run: bool) -> FixReport {
  let target = path.display().to_string();
  let before = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(e) => {
      return FixReport::new(
        FixId::ToolConfigProxyUrl,
        target,
        FixOutcome::Failed,
        e.to_string(),
      )
    }
  };
  let stale = stale_url::stale_urls(&before, live);
  if stale.is_empty() {
    return FixReport::skipped(FixId::ToolConfigProxyUrl, target, "no stale proxy url");
  }
  let changes = stale_url::changes(&stale);
  if dry_run {
    return FixReport::new(
      FixId::ToolConfigProxyUrl,
      target,
      FixOutcome::WouldApply,
      changes,
    );
  }
  let Some(body) = stale_url::rewrite(&before, live) else {
    return FixReport::skipped(FixId::ToolConfigProxyUrl, target, "no stale proxy url");
  };
  // Rename-a-tempfile-in, so the mode has to be re-declared: the file's own
  // bits, because repairing an address is not a licence to harden or loosen a
  // config. The path arrives already resolved through any symlink, so the
  // rename lands on the real file and leaves the link alone.
  let mode = file_mode(path);
  let dir = path.parent().unwrap_or_else(|| Path::new("."));
  match crate::util::atomic_write::write_secure(
    dir,
    "llamastash-tool-config.tmp.",
    path,
    body.as_bytes(),
    mode,
  ) {
    Ok(_) => FixReport::new(
      FixId::ToolConfigProxyUrl,
      target,
      FixOutcome::Applied,
      changes,
    ),
    Err(e) => FixReport::new(
      FixId::ToolConfigProxyUrl,
      target,
      FixOutcome::Failed,
      e.to_string(),
    ),
  }
}

/// A file's own permission bits, to carry them across an atomic rewrite.
fn file_mode(path: &Path) -> Option<u32> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
      .ok()
      .map(|m| m.permissions().mode() & 0o777)
  }
  #[cfg(not(unix))]
  {
    let _ = path;
    None
  }
}

fn check_remote_snapshot_unreachable(snapshot: &InitSnapshot) -> Option<Finding> {
  if snapshot.remote_fetch_failures < REMOTE_UNREACHABLE_THRESHOLD {
    return None;
  }
  Some(Finding::new(
    FindingId::RemoteSnapshotUnreachable,
    Severity::Info,
    format!(
      "remote benchmark snapshot has been unreachable for \
       {} consecutive verified-fetch attempts; bundled fallback in use",
      snapshot.remote_fetch_failures
    ),
  ))
}

fn is_readable(path: &Path) -> bool {
  std::fs::File::open(path).is_ok()
}

/// Render the live hardware section with the label
/// conventions. `MEM`/`MEM*` mark discrete vs unified memory; the
/// `VRAM (shared)` row is the UMA pool's system-RAM portion — a
/// breakdown of `MEM*`, not a separate pool.
fn format_hardware_section(hw: &HardwareSection) -> String {
  use crate::cli::format;
  use std::fmt::Write as _;
  let mut out = String::new();
  out.push_str(&format::section_header("hardware", None));
  let cpu = if hw.cpu_brand.is_empty() {
    "unknown CPU"
  } else {
    hw.cpu_brand.as_str()
  };
  let cpu_features = if hw.cpu_features.is_empty() {
    String::new()
  } else {
    format!(" · {}", hw.cpu_features.join(" "))
  };
  let _ = writeln!(
    out,
    "  {:<5} {cpu} · {} cores{cpu_features}",
    "CPU", hw.cpu_cores
  );
  let mem_label = if hw.unified { "MEM*" } else { "MEM" };
  let _ = writeln!(out, "  {:<5} {}", mem_label, fmt_gib(hw.mem_total_bytes));
  // Shared one-line GPU summary so `doctor`, `status`, and `init` name
  // the vendor + pool + classification identically.
  let gpu_detail = crate::init::detection::gpu_summary_line(
    &hw.gpu_backend,
    hw.gpu_pool_total_bytes,
    hw.uma_class_source,
  );
  let _ = writeln!(out, "  {:<5} {gpu_detail}", "GPU");
  if let Some(shared) = hw.uma_shared_bytes {
    let _ = writeln!(out, "  VRAM (shared) {}", fmt_gib(shared));
  }
  if hw.disk_free_bytes > 0 {
    let _ = writeln!(out, "  {:<5} {} free", "DISK", fmt_gib(hw.disk_free_bytes));
  }
  if !hw.os.is_empty() {
    let _ = writeln!(out, "  {:<5} {}", "OS", hw.os);
  }
  out
}

fn short_hex(digest: &str) -> String {
  if digest.len() <= 12 {
    digest.to_string()
  } else {
    format!("{}…", &digest[..12])
  }
}

/// CLI handler entry-point. Always exits 0 — findings are informative,
/// not a failure signal. (Agents can branch on a non-empty `findings`
/// array to escalate.)
pub async fn run(args: DoctorArgs, _cli: &Cli, config: &Config) -> CliResult {
  let hardware = detect_hardware();
  // Distinguish three snapshot states:
  //   * `Some(snap)` — read cleanly; full diff against baseline.
  //   * `None` after a parse-fail Err — file existed but was corrupt
  //     or unreadable. `snapshot::load` already quarantined it to
  //     `.broken-<ts>`; we proceed without a baseline but log so the
  //     user sees what happened.
  //   * `None` after Ok(None) — first run, no snapshot yet. Silent.
  let state_dir = crate::util::paths::state_dir();
  let snapshot = match state_dir.as_ref() {
    Some(dir) => match snapshot::load(dir) {
      Ok(snap) => snap,
      Err(e) => {
        log::warn!("doctor: failed to read init_snapshot.json (quarantined to .broken-<ts>): {e}");
        None
      }
    },
    None => None,
  };
  // Reflect the snapshot the recommender would actually use: freshen the
  // recorded bundle_date against the latest available remote before the
  // staleness check runs. In-memory only — the original `snapshot` is
  // left intact for the memory-baseline write below.
  let report_snapshot = freshen_snapshot_date(snapshot.clone()).await;
  let mut report = build_report(report_snapshot.as_ref(), &hardware);

  // Baseline stamp/refresh — doctor's single documented write,
  // amending the otherwise read-only contract. Persist the current GPU
  // pool ceiling when a snapshot exists and either has no baseline yet
  // (stamp silently, no finding) or the pool drifted (re-stamp so the
  // drift finding is one-shot). A write failure degrades to a finding,
  // never an error exit.
  if let (Some(dir), Some(snap)) = (state_dir.as_ref(), snapshot.as_ref()) {
    let drift_fired = report
      .findings
      .iter()
      .any(|f| f.id == FindingId::MemoryDrift.as_str());
    let baseline_missing = snap.gpu_pool_total_bytes.is_none();
    if (drift_fired || baseline_missing) && hardware.vram_bytes.is_some() {
      let mut refreshed = snap.clone();
      refreshed.gpu_pool_total_bytes = hardware.vram_bytes;
      if let Err(e) = snapshot::save(dir, &refreshed) {
        log::warn!("doctor: failed to refresh memory baseline: {e}");
        report.findings.push(Finding::new(
          FindingId::MemoryDrift,
          Severity::Warning,
          format!("could not refresh the memory baseline ({e}); the drift finding may repeat"),
        ));
      }
    }
  }

  // Backend-contributed advisories: each backend adds its own
  // findings via the `doctor_findings` hook ("compatible model present but
  // engine unavailable", say). Collected generically over the registry so this
  // path names no backend; every id stays additive (schema stays 2).
  for backend in crate::backend::Backends::all() {
    report
      .findings
      .extend(backend.doctor_findings(config).await);
  }

  // Server-catalog advisory: configured `servers:` health across backends.
  report.findings.extend(check_servers(config));

  if let Some(dir) = state_dir.as_ref() {
    if let Some(finding) = check_stale_daemon_files(dir) {
      report.findings.push(finding);
    }
  }

  // A tool config whose proxy URL went stale when the proxy moved port. What
  // the live proxy reports is also what the repair below rewrites to, so the
  // finding and the fix can never disagree about the target address.
  let live_base = crate::init::external::proxy_url::from_running_daemon().await;
  let live_proxy = live_base
    .as_deref()
    .and_then(|base| stale_url::LiveProxy::from_base_url(base, &stale_url::bind_bases(config)));
  if let (Some(base), Some(live)) = (live_base.as_deref(), live_proxy.as_ref()) {
    report.findings.extend(check_tool_config_urls(live, base));
  }

  // `--fix` acts on what the report found; `--dry-run` previews the same
  // list without touching anything. Either way `doctor` still exits 0: a
  // repair that failed is a reported outcome, not a failed diagnostic.
  if args.fix || args.dry_run {
    report.fixes = apply_fixes(&report.findings, args.dry_run, live_proxy.as_ref());
  }

  if args.json {
    println!(
      "{}",
      serde_json::to_string_pretty(&report).unwrap_or_default()
    );
  } else {
    render_human(&report);
  }
  Ok(())
}

fn render_human(report: &DoctorReport) {
  print!("{}", format_human(report));
}

/// Pure renderer for the doctor human-readable surface. Returns the
/// composed string so unit tests can assert byte shape without
/// capturing stdout. `render_human` is the thin wrapper that prints it.
fn format_human(report: &DoctorReport) -> String {
  use crate::cli::{colors, format};
  use std::fmt::Write as _;
  let mut out = String::new();
  out.push_str(&format_hardware_section(&report.hardware));
  if report.findings.is_empty() {
    // Empty-clean state reads as the same shape as a populated one:
    // bold section header, count suffix, then a single success line.
    out.push_str(&format::section_header(
      "llamastash doctor",
      Some((0, "findings")),
    ));
    let _ = writeln!(out, "{}", colors::success("everything looks healthy"));
    if let Some(date) = &report.baseline.init_date {
      let _ = writeln!(out, "  {}", colors::dim(&format!("last init: {date}")));
    }
    return out;
  }
  out.push_str(&format::section_header(
    "llamastash doctor",
    Some((report.findings.len(), "findings")),
  ));
  for f in &report.findings {
    // Per-finding block:
    //   • severity glyph (sentinel for byte-classifying parsers),
    //   • bold `[finding_id]` (stable scannable token),
    //   • severity-tinted message,
    //   • indented `→ fix with: <bold hint>` second line.
    let id_styled = console::style(format!("[{}]", f.id)).bold().to_string();
    let glyph = match f.severity {
      Severity::Error => console::style("✗").red().bold().to_string(),
      Severity::Warning => console::style("!").yellow().to_string(),
      Severity::Info => colors::dim("•"),
    };
    let message_styled = match f.severity {
      Severity::Error => console::style(&f.message).red().to_string(),
      Severity::Warning => console::style(&f.message).yellow().to_string(),
      Severity::Info => colors::dim(&f.message),
    };
    let _ = writeln!(out, "\n  {glyph} {id_styled} {message_styled}");
    let _ = writeln!(
      out,
      "    {} {}",
      colors::dim("→ fix with:"),
      console::style(f.fix_hint).bold(),
    );
  }
  if !report.fixes.is_empty() {
    out.push('\n');
    out.push_str(&format::section_header(
      "fixes",
      Some((report.fixes.len(), "actions")),
    ));
    for fix in &report.fixes {
      let line = match fix.outcome {
        FixOutcome::Applied => format!("{} {} ({})", fix.action, fix.target, fix.detail),
        FixOutcome::WouldApply => format!("would {} {} ({})", fix.action, fix.target, fix.detail),
        FixOutcome::Skipped => {
          format!("{} {} — skipped: {}", fix.action, fix.target, fix.detail)
        }
        FixOutcome::Failed => {
          format!("{} {} failed: {}", fix.action, fix.target, fix.detail)
        }
      };
      let line = match fix.outcome {
        FixOutcome::Applied => colors::success(&line),
        FixOutcome::Failed => colors::error(&line),
        _ => colors::dim(&line),
      };
      let _ = writeln!(out, "  {line}");
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::{ClassSource, GpuDevice, GpuInfo};
  use crate::init::detection::{CpuArch, OsFamily};

  /// Carve-signature UMA fixture: one AMD device whose total is
  /// `carve + gtt`, with the GTT marked shared.
  fn uma_hw(carve: u64, gtt: u64, ram: u64) -> HardwareSnapshot {
    let total = carve + gtt;
    let dev = GpuDevice {
      name: "card1".into(),
      backend: "amd".into(),
      total_memory_bytes: total,
      uma_shared_total_bytes: Some(gtt),
      uma_shared_used_bytes: Some(0),
      classification_source: Some(ClassSource::CarveSignature),
      ..Default::default()
    };
    HardwareSnapshot {
      gpu: GpuInfo::Amd { devices: vec![dev] },
      vram_bytes: Some(total),
      gpu_device_count: 1,
      ram_total_bytes: ram,
      disk_free_bytes: 0,
      cpu_brand: "AMD Test CPU".into(),
      cpu_cores: 16,
      cpu_features: Vec::new(),
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  const GIB: u64 = 1024 * 1024 * 1024;
  const CARVE: u64 = 512 * 1024 * 1024;

  fn cpu_hw() -> HardwareSnapshot {
    HardwareSnapshot {
      gpu: GpuInfo::CpuOnly,
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: 16 * 1024 * 1024 * 1024,
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
      os: OsFamily::Linux,
      cpu_arch: CpuArch::X86_64,
    }
  }

  #[test]
  fn report_with_no_snapshot_emits_no_findings() {
    let report = build_report(None, &cpu_hw());
    assert!(report.findings.is_empty());
    assert!(report.baseline.snapshot_bundle_date.is_none());
  }

  #[test]
  fn binary_missing_finding_fires_for_nonexistent_path() {
    let snap = InitSnapshot {
      llama_server_path: Some("/nonexistent/llama-server".into()),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(report.findings.iter().any(|f| f.id == "binary_missing"));
  }

  #[test]
  fn brew_digest_drift_is_carved_out() {
    // brew-installed binary with a missing/changed digest should
    // NOT produce a binary_digest_drift finding (only a possible
    // BinaryMissing finding if the path doesn't exist).
    let snap = InitSnapshot {
      install_method: Some(InstallMethod::Brew),
      llama_server_path: Some("/nonexistent/llama-server".into()),
      llama_server_digest: Some("a".repeat(64)),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(!report
      .findings
      .iter()
      .any(|f| f.id == "binary_digest_drift"));
  }

  #[test]
  fn hardware_drift_finding_fires_when_vendor_changes() {
    let snap = InitSnapshot {
      gpu_vendor: Some("nvidia".into()),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    let drift = report.findings.iter().find(|f| f.id == "hardware_drift");
    assert!(
      drift.is_some(),
      "hardware_drift should fire when vendor changed"
    );
    assert_eq!(drift.unwrap().fix_hint, "llamastash init --only server");
  }

  #[test]
  fn snapshot_stale_finding_fires_after_threshold_days() {
    let snap = InitSnapshot {
      snapshot_bundle_date: Some("2000-01-01".into()),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    let stale = report.findings.iter().find(|f| f.id == "snapshot_stale");
    assert!(
      stale.is_some(),
      "stale snapshot should fire for an ancient bundle_date"
    );
  }

  #[test]
  fn snapshot_stale_does_not_fire_for_fresh_bundle() {
    let today = current_yyyymmdd().expect("clock");
    let snap = InitSnapshot {
      snapshot_bundle_date: Some(today),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(!report.findings.iter().any(|f| f.id == "snapshot_stale"));
  }

  #[test]
  fn date_is_stale_flags_only_old_parseable_dates() {
    // The remote-probe gate in `freshen_snapshot_date` reuses this.
    assert!(date_is_stale("2000-01-01"), "ancient date must be stale");
    let today = current_yyyymmdd().expect("clock");
    assert!(!date_is_stale(&today), "today is never stale");
    assert!(
      !date_is_stale("not-a-date"),
      "an unparseable date must not be flagged (no spurious network probe)"
    );
  }

  #[tokio::test]
  async fn freshen_snapshot_date_noop_without_a_snapshot() {
    assert!(freshen_snapshot_date(None).await.is_none());
  }

  #[test]
  fn remote_unreachable_finding_fires_after_threshold() {
    let snap = InitSnapshot {
      remote_fetch_failures: REMOTE_UNREACHABLE_THRESHOLD,
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(report
      .findings
      .iter()
      .any(|f| f.id == "remote_snapshot_unreachable"));
  }

  #[test]
  fn remote_unreachable_does_not_fire_below_threshold() {
    let snap = InitSnapshot {
      remote_fetch_failures: REMOTE_UNREACHABLE_THRESHOLD - 1,
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(!report
      .findings
      .iter()
      .any(|f| f.id == "remote_snapshot_unreachable"));
  }

  #[test]
  fn every_finding_id_has_a_fix_hint_and_safe_to_log_true() {
    let ids = [
      FindingId::BinaryMissing,
      FindingId::BinaryDigestDrift,
      FindingId::HardwareDrift,
      FindingId::MemoryDrift,
      FindingId::GttHint,
      FindingId::SnapshotStale,
      FindingId::ConfigModeDrift,
      FindingId::StaleDaemonFiles,
      FindingId::RemoteSnapshotUnreachable,
      FindingId::ToolConfigStaleUrl,
    ];
    for id in ids {
      assert!(!id.fix_hint().is_empty(), "{id:?} must have a fix_hint");
      // The id names the hand step; pointing at the fixer is `Finding::new`'s
      // job, so naming it here too would be a second copy of that string.
      assert!(
        !id.fix_hint().contains("doctor --fix"),
        "{id:?} must name a hand step, not the fixer"
      );
      let f = Finding::new(id, Severity::Info, "test");
      assert!(f.safe_to_log, "v2 findings must all be safe_to_log");
    }
  }

  #[test]
  fn memory_drift_growth_is_info() {
    // Baseline 64 GiB, current 124.5 GiB (512 MiB carve + 124 GiB GTT)
    // — the kyuz0 GTT reconfig. Growth is informational.
    let snap = InitSnapshot {
      gpu_pool_total_bytes: Some(64 * GIB),
      ..Default::default()
    };
    let hw = uma_hw(CARVE, 124 * GIB, 128 * GIB);
    let f = check_memory_drift(&snap, &hw).expect("drift should fire");
    assert_eq!(f.id, "memory_drift");
    assert_eq!(f.severity, Severity::Info);
    assert!(f.message.contains("64.0 GiB"), "msg: {}", f.message);
    assert!(f.message.contains("124.5 GiB"), "msg: {}", f.message);
    assert!(f.message.contains("grew"), "msg: {}", f.message);
  }

  #[test]
  fn memory_drift_shrink_is_warning() {
    let snap = InitSnapshot {
      gpu_pool_total_bytes: Some(124 * GIB),
      ..Default::default()
    };
    let hw = uma_hw(CARVE, 60 * GIB, 128 * GIB);
    let f = check_memory_drift(&snap, &hw).expect("drift should fire");
    assert_eq!(f.severity, Severity::Warning);
    assert!(f.message.contains("shrank"), "msg: {}", f.message);
  }

  #[test]
  fn memory_drift_below_threshold_no_finding() {
    // 64 GiB baseline; current 64 GiB + 100 MiB is under max(5%, 512 MiB).
    let snap = InitSnapshot {
      gpu_pool_total_bytes: Some(64 * GIB),
      ..Default::default()
    };
    let hw = uma_hw(CARVE, 64 * GIB - CARVE + 100 * 1024 * 1024, 128 * GIB);
    assert!(check_memory_drift(&snap, &hw).is_none());
  }

  #[test]
  fn memory_drift_missing_baseline_no_finding() {
    // No baseline → no finding (run() stamps it silently instead).
    let snap = InitSnapshot::default();
    let hw = uma_hw(CARVE, 124 * GIB, 128 * GIB);
    assert!(check_memory_drift(&snap, &hw).is_none());
  }

  #[test]
  fn gtt_hint_fires_at_kernel_default_half_ram() {
    // GTT ~half of RAM (60/128 = 0.47) is the amdgpu default → hint.
    let hw = uma_hw(CARVE, 60 * GIB, 128 * GIB);
    let f = check_gtt_hint(&hw).expect("gtt hint should fire");
    assert_eq!(f.id, "gtt_hint");
    assert_eq!(f.severity, Severity::Info);
    assert!(
      !f.message.contains("amd_iommu"),
      "must never mention amd_iommu"
    );
  }

  #[test]
  fn gtt_hint_does_not_fire_when_ceiling_raised() {
    // GTT == RAM (already reconfigured) → ratio 1.0, outside the band.
    let hw = uma_hw(CARVE, 124 * GIB, 124 * GIB);
    assert!(check_gtt_hint(&hw).is_none());
  }

  #[test]
  fn gtt_hint_does_not_fire_on_discrete() {
    // CPU-only / non-unified host → no GTT hint.
    assert!(check_gtt_hint(&cpu_hw()).is_none());
  }

  #[test]
  fn hardware_section_carries_uma_composition() {
    let hw = uma_hw(CARVE, 124 * GIB, 128 * GIB);
    let report = build_report(None, &hw);
    let hs = &report.hardware;
    assert!(hs.unified);
    assert_eq!(hs.uma_class_source, Some(ClassSource::CarveSignature));
    assert_eq!(hs.gpu_pool_total_bytes, Some(CARVE + 124 * GIB));
    assert_eq!(hs.uma_carve_bytes, Some(CARVE));
    assert_eq!(hs.uma_shared_bytes, Some(124 * GIB));
    assert_eq!(report.schema_version, 2);
  }

  #[test]
  fn hardware_section_renders_mem_star_and_gpu_shared_for_uma() {
    let _g = crate::cli::test_lock::serialize();
    let prior_colors = console::colors_enabled();
    console::set_colors_enabled(false);
    let hw = uma_hw(CARVE, 124 * GIB, 128 * GIB);
    let out = format_hardware_section(&HardwareSection::from_hardware(&hw));
    assert!(out.contains("MEM*"), "unified host uses MEM*: {out:?}");
    assert!(
      out.contains("VRAM (shared)"),
      "UMA composition row: {out:?}"
    );
    assert!(
      out.contains("unified, inferred"),
      "classification source: {out:?}"
    );
    console::set_colors_enabled(prior_colors);
  }

  #[test]
  fn days_between_arithmetic_matches_civil_calendar() {
    let a = (2024, 1, 1);
    let b = (2024, 1, 31);
    assert_eq!(days_between(a, b), Some(30));
    let c = (2025, 1, 1);
    assert_eq!(days_between(a, c), Some(366)); // 2024 is leap
  }

  #[test]
  fn parse_yyyymmdd_rejects_bad_shapes() {
    assert!(parse_yyyymmdd("2024/01/01").is_none());
    assert!(parse_yyyymmdd("2024-13-01").is_none());
    assert!(parse_yyyymmdd("2024-01-32").is_none());
  }

  #[test]
  fn render_human_handles_empty_report() {
    // Smoke test: no panic on rendering, the function returns ().
    let report = build_report(None, &cpu_hw());
    render_human(&report);
  }

  #[test]
  fn format_human_empty_report_shape() {
    // The colors-disabled (piped) shape is byte-stable so an agent or
    // CI script parsing the human output sees the same string across
    // releases. The section header carries the (0 findings) suffix
    // even on the healthy branch so the surface stays uniform.
    let _g = crate::cli::test_lock::serialize();
    let prior_colors = console::colors_enabled();
    console::set_colors_enabled(false);
    let report = build_report(None, &cpu_hw());
    let out = format_human(&report);
    assert_eq!(
      out,
      "hardware\n  CPU   unknown CPU · 0 cores\n  MEM   16.0 GiB\n  GPU   CPU only\n  OS    linux/x86_64\n\
       llamastash doctor (0 findings)\n✓ everything looks healthy\n"
    );
    console::set_colors_enabled(prior_colors);
  }

  #[test]
  fn format_human_non_empty_report_renders_each_finding_block() {
    // Non-empty path: section header with the actual finding count,
    // then per-finding block with severity glyph, [bracketed id], and
    // an indented "→ fix with: <hint>" line. Plain-bytes assertions
    // catch silent shape drift on this critical visual surface.
    let _g = crate::cli::test_lock::serialize();
    let prior_colors = console::colors_enabled();
    console::set_colors_enabled(false);
    let snap = InitSnapshot {
      llama_server_path: Some("/nonexistent/llama-server".into()),
      gpu_vendor: Some("nvidia".into()),
      ..Default::default()
    };
    let report = build_report(Some(&snap), &cpu_hw());
    assert!(
      report.findings.len() >= 2,
      "expected at least 2 findings, got: {:?}",
      report.findings.iter().map(|f| f.id).collect::<Vec<_>>()
    );
    let out = format_human(&report);
    // The hardware section renders first; the findings section header
    // (with count suffix) follows it.
    assert!(
      out.starts_with("hardware\n"),
      "hardware section first: {out:?}"
    );
    assert!(
      out.contains(&format!(
        "llamastash doctor ({} findings)\n",
        report.findings.len()
      )),
      "section header drift: {out:?}"
    );
    // Every finding's id appears bracketed.
    for f in &report.findings {
      assert!(
        out.contains(&format!("[{}]", f.id)),
        "missing [{}] in: {out:?}",
        f.id
      );
    }
    // The "→ fix with:" arrow appears once per finding.
    let arrow_count = out.matches("→ fix with:").count();
    assert_eq!(
      arrow_count,
      report.findings.len(),
      "one fix-with arrow per finding; got {arrow_count} for {} findings",
      report.findings.len()
    );
    console::set_colors_enabled(prior_colors);
  }

  #[test]
  fn check_servers_silent_without_configured_servers() {
    // A default install configures no `servers:` (PATH-resolved primary) — the
    // advisory stays quiet rather than adding noise.
    assert!(check_servers(&Config::default()).is_empty());
  }

  #[test]
  fn check_servers_warns_on_a_missing_configured_binary() {
    let mut config = Config::default();
    config.backend.llamacpp.servers = vec![crate::backend::ServerConfig {
      binary: std::path::PathBuf::from("/nonexistent/build-xyz/bin/llama-server"),
      name: None,
    }];
    let findings = check_servers(&config);
    assert!(
      findings.iter().any(|f| f.id == "server_binary_missing"
        && f.severity == Severity::Warning
        && f.message.contains("llamacpp")
        && f.message.contains("llama-server")),
      "warning naming the backend + missing path"
    );
    // The summary still lists the count of configured binaries.
    assert!(findings.iter().any(|f| f.id == "servers_configured"));
  }

  #[test]
  fn check_servers_summarizes_a_present_binary() {
    // A present binary resolves into the config catalog (0 GPUs from the failed
    // probe of a non-llama-server file), so the info summary lists it and no
    // warning fires.
    let dir = crate::test_support::unique_temp_dir("doctor-servers", "present");
    let bin = dir.join("llama-server");
    std::fs::write(&bin, b"not a real server").unwrap();
    let mut config = Config::default();
    config.backend.llamacpp.servers = vec![crate::backend::ServerConfig {
      binary: bin,
      name: None,
    }];
    let findings = check_servers(&config);
    assert!(
      !findings.iter().any(|f| f.id == "server_binary_missing"),
      "a present binary must not warn"
    );
    let info = findings
      .iter()
      .find(|f| f.id == "servers_configured")
      .expect("info summary present");
    assert_eq!(info.severity, Severity::Info);
    assert!(info.message.contains("configured server"));
    assert!(info.safe_to_log);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  fn config_with_mode(dir: &Path, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("config.yaml");
    std::fs::write(&path, b"proxy:\n  port: 11435\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
  }

  #[cfg(unix)]
  fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
  }

  #[cfg(unix)]
  #[test]
  fn fix_config_mode_restores_0600_and_then_has_nothing_to_do() {
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "chmod");
    std::fs::create_dir_all(&dir).unwrap();
    let path = config_with_mode(&dir, 0o644);
    let fix = fix_config_mode(&path, false);
    assert_eq!(fix.outcome, FixOutcome::Applied);
    assert_eq!(fix.fix, FixId::ConfigMode);
    assert_eq!(mode_of(&path), 0o600);
    let again = fix_config_mode(&path, false);
    assert_eq!(again.outcome, FixOutcome::Skipped);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn dry_run_names_the_chmod_without_applying_it() {
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "chmod-dry");
    std::fs::create_dir_all(&dir).unwrap();
    let path = config_with_mode(&dir, 0o640);
    let fix = fix_config_mode(&path, true);
    assert_eq!(fix.outcome, FixOutcome::WouldApply);
    assert!(fix.detail.contains("640"), "detail names the mode: {fix:?}");
    assert_eq!(mode_of(&path), 0o640, "--dry-run must not chmod");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn fix_config_mode_chmods_the_link_target_and_names_it() {
    // A dotfiles-managed config: the link sits in the config dir and the
    // real file is elsewhere. The repair follows it, because that is what
    // `chmod` does, and names the file it actually wrote.
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "symlink");
    let real_dir = dir.join("real");
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    let real = real_dir.join("config.yaml");
    std::fs::write(&real, b"proxy:\n  port: 11435\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o666)).unwrap();
    let link = config_dir.join("config.yaml");
    symlink(&real, &link).unwrap();

    let fix = fix_config_mode(&link, false);
    assert_eq!(fix.outcome, FixOutcome::Applied, "{fix:?}");
    assert_eq!(
      fix.target,
      crate::util::paths::canonicalize(&real)
        .unwrap()
        .display()
        .to_string(),
      "the ledger must name the file chmodded, not the link"
    );
    assert_eq!(mode_of(&real), 0o600);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn fix_config_mode_refuses_a_swappable_parent() {
    // In a world-writable dir, whoever can place files there picks what an
    // automated chmod through a planted symlink would hit.
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "swappable");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = config_with_mode(&dir, 0o644);
    let fix = fix_config_mode(&path, false);
    assert_eq!(fix.outcome, FixOutcome::Skipped, "{fix:?}");
    assert!(fix.detail.contains("world-writable"), "{fix:?}");
    assert_eq!(mode_of(&path), 0o644, "nothing may be chmodded");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn fix_config_mode_refuses_a_link_that_lands_in_a_swap_surface() {
    // The config dir is ours and tight; the link points into a directory
    // anyone can write. That other directory's writer would be choosing what
    // the automated chmod hits, so the repair stays home.
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "link-out");
    let open = dir.join("open");
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&open).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let real = open.join("real.yaml");
    std::fs::write(&real, b"proxy:\n  port: 11435\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
    let link = config_dir.join("config.yaml");
    symlink(&real, &link).unwrap();

    // The plan reports the canonical chain it walked, and a temp root reached
    // through a symlink is not that string. macOS is always like this: its
    // `temp_dir()` is `/tmp`, which resolves to `/private/tmp`.
    let open_canonical = std::fs::canonicalize(&open).unwrap();
    assert_eq!(
      chmod_plan(&link).expect("plan").blocked.map(|(d, _)| d),
      Some(open_canonical),
      "the blocking dir is the one the link lands in"
    );
    let fix = fix_config_mode(&link, false);
    assert_eq!(fix.outcome, FixOutcome::Skipped, "{fix:?}");
    assert!(fix.detail.contains("world-writable"), "{fix:?}");
    assert_eq!(mode_of(&real), 0o644, "nothing may be chmodded");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn fix_config_mode_refuses_a_non_regular_target() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "device");
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("config.yaml");
    symlink("/dev/null", &link).unwrap();
    let fix = fix_config_mode(&link, false);
    assert_eq!(fix.outcome, FixOutcome::Skipped, "{fix:?}");
    assert!(fix.detail.contains("not a regular file"), "{fix:?}");
    assert_eq!(
      std::fs::metadata("/dev/null").unwrap().permissions().mode() & 0o777,
      0o666,
      "/dev/null must be untouched"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn leftover_daemon_files_are_found_then_removed() {
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "stale");
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("daemon.pid");
    let runtime = crate::daemon::runtime_file::path(&dir);
    std::fs::write(&pidfile, b"4242\n").unwrap();
    std::fs::write(&runtime, b"{}\n").unwrap();

    let finding = check_stale_daemon_files(&dir).expect("leftovers must be found");
    assert_eq!(finding.id, FindingId::StaleDaemonFiles.as_str());
    assert_eq!(finding.fix, Some(FixId::StaleDaemonFiles));

    let proposed = fix_stale_daemon_files(&dir, true);
    assert_eq!(proposed.len(), 2, "one entry per leftover: {proposed:?}");
    assert!(
      proposed.iter().all(|f| f.outcome == FixOutcome::WouldApply),
      "{proposed:?}"
    );
    assert!(
      pidfile.exists() && runtime.exists(),
      "--dry-run removes nothing"
    );

    for fix in fix_stale_daemon_files(&dir, false) {
      assert_eq!(fix.outcome, FixOutcome::Applied, "{fix:?}");
    }
    assert!(!pidfile.exists() && !runtime.exists());
    assert!(
      check_stale_daemon_files(&dir).is_none(),
      "a clean state dir has nothing to report"
    );
    assert_eq!(
      fix_stale_daemon_files(&dir, false)[0].outcome,
      FixOutcome::Skipped
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn a_daemon_that_holds_the_lock_is_left_alone() {
    // Our own flock answers the way a running daemon does: the probe uses a
    // separate open file description, so its non-blocking lock contends.
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "live");
    let guard = crate::daemon::lockfile::acquire(&dir).expect("acquire");
    let runtime = crate::daemon::runtime_file::path(&dir);
    std::fs::write(&runtime, b"{}\n").unwrap();

    assert!(
      check_stale_daemon_files(&dir).is_none(),
      "a live holder is not a leftover"
    );
    let fixes = fix_stale_daemon_files(&dir, false);
    assert_eq!(fixes[0].outcome, FixOutcome::Skipped, "{fixes:?}");
    assert!(
      runtime.exists(),
      "never delete a running daemon's handshake"
    );
    drop(guard);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn only_a_repairable_finding_carries_a_fix_id() {
    let stale = Finding::new(FindingId::StaleDaemonFiles, Severity::Warning, "leftover");
    assert_eq!(stale.fix, Some(FixId::StaleDaemonFiles));
    assert_eq!(stale.fix_hint, FIX_WITH_DOCTOR);
    assert!(serde_json::to_string(&stale)
      .unwrap()
      .contains("\"fix\":\"remove_stale_daemon_files\""));
    // Backend-contributed findings have no repair either, and their hint
    // stays the manual step the id carries.
    let contributed = Finding::from_parts("server_binary_missing", Severity::Warning, "m", "hint");
    assert_eq!(contributed.fix, None);
    assert!(
      !contributed.fix_hint.contains("doctor --fix"),
      "{}",
      contributed.fix_hint
    );
    assert!(
      !serde_json::to_string(&contributed)
        .unwrap()
        .contains("\"fix\":"),
      "an unrepairable finding must not gain the key"
    );
  }

  #[cfg(unix)]
  #[test]
  fn a_withheld_config_repair_keeps_the_id_but_loses_the_fix() {
    // One id, two cases: the mode drift `--fix` can chmod away, and the
    // swappable parent dir it must not touch.
    let mode_drift = Finding::new(FindingId::ConfigModeDrift, Severity::Warning, "mode 644");
    assert_eq!(mode_drift.fix, Some(FixId::ConfigMode));
    assert_eq!(mode_drift.fix_hint, FIX_WITH_DOCTOR);
    assert!(serde_json::to_string(&mode_drift)
      .unwrap()
      .contains("\"fix\":\"config_chmod_0600\""));
    let parent_drift = Finding::manual(
      FindingId::ConfigModeDrift,
      Severity::Warning,
      "parent dir is world-writable",
      "run `chmod go-w` on the parent dir",
    );
    assert_eq!(parent_drift.fix, None);
    assert!(
      !parent_drift.fix_hint.contains("doctor --fix"),
      "a finding --fix will not act on must not point at --fix: {}",
      parent_drift.fix_hint
    );
    assert!(
      !serde_json::to_string(&parent_drift)
        .unwrap()
        .contains("\"fix\":"),
      "an unrepairable finding must not gain the key"
    );
  }

  #[cfg(unix)]
  #[test]
  fn a_withheld_repair_is_ledged_against_its_own_path() {
    // The dir finding has no repair to withhold; the mode finding's chmod
    // was blocked, and the ledger line names the file it wanted.
    let dir_finding = Finding::manual(
      FindingId::ConfigModeDrift,
      Severity::Warning,
      "parent dir `/x` is world-writable",
      "run `chmod go-w` on that dir, or move the config out of it",
    );
    let mode_finding = Finding::blocked(
      FindingId::ConfigModeDrift,
      Severity::Warning,
      "`/x/config.yaml` is mode 0o666",
      Path::new("/x/config.yaml"),
      "is world-writable (mode 0o777)".to_string(),
    );
    let fixes = apply_fixes(&[dir_finding, mode_finding], false, None);
    assert_eq!(fixes.len(), 1, "{fixes:?}");
    assert_eq!(fixes[0].fix, FixId::ConfigMode);
    assert_eq!(fixes[0].outcome, FixOutcome::Skipped);
    assert_eq!(fixes[0].target, "/x/config.yaml", "{fixes:?}");
    assert!(
      fixes[0]
        .detail
        .starts_with("withheld: is world-writable (mode 0o777); manual step:"),
      "{fixes:?}"
    );
    // A dir finding on its own withholds nothing, so `--fix` stays empty.
    let only_dir = Finding::manual(
      FindingId::ConfigModeDrift,
      Severity::Warning,
      "parent dir `/x` is world-writable",
      "run `chmod go-w` on that dir, or move the config out of it",
    );
    assert!(apply_fixes(&[only_dir], false, None).is_empty());
  }

  #[test]
  fn a_stale_tool_config_is_rewritten_in_place() {
    let dir = crate::test_support::unique_temp_dir("doctor-fix", "tool-url");
    std::fs::create_dir_all(&dir).unwrap();
    let live = stale_url::LiveProxy::from_base_url(
      "http://127.0.0.1:11437/v1",
      &stale_url::bind_bases(&Config::default()),
    )
    .unwrap();
    let path = dir.join("env.sh");
    let body = "# llamastash env.sh (generated, do not edit by hand)\nexport AIDER_API_BASE=\"http://127.0.0.1:11435/v1\"\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:11435\"\n";
    std::fs::write(&path, body).unwrap();

    let proposed = fix_tool_config_url(&path, &live, true);
    assert_eq!(proposed.fix, FixId::ToolConfigProxyUrl);
    assert_eq!(proposed.outcome, FixOutcome::WouldApply, "{proposed:?}");
    assert_eq!(
      std::fs::read_to_string(&path).unwrap(),
      body,
      "--dry-run must write nothing"
    );

    let applied = fix_tool_config_url(&path, &live, false);
    assert_eq!(applied.outcome, FixOutcome::Applied, "{applied:?}");
    assert_eq!(
      applied.detail,
      "http://127.0.0.1:11435/v1 \u{2192} http://127.0.0.1:11437/v1, http://127.0.0.1:11435 \u{2192} http://127.0.0.1:11437"
    );
    assert_eq!(
      std::fs::read_to_string(&path).unwrap(),
      "# llamastash env.sh (generated, do not edit by hand)\nexport AIDER_API_BASE=\"http://127.0.0.1:11437/v1\"\nexport ANTHROPIC_BASE_URL=\"http://127.0.0.1:11437\"\n",
      "only the two stale addresses move"
    );

    // Nothing stale any more, so there is nothing to rewrite.
    assert_eq!(
      fix_tool_config_url(&path, &live, false).outcome,
      FixOutcome::Skipped,
      "a repaired file must not be rewritten again"
    );

    // A config whose URL names another host is not ours to rewrite, even
    // though the port is the stale one.
    let foreign = dir.join("codex.toml");
    let foreign_body = "model = \"qwen3-coder-30b\"\nbase_url = \"http://localhost:11435/v1\"\n";
    std::fs::write(&foreign, foreign_body).unwrap();
    assert_eq!(
      fix_tool_config_url(&foreign, &live, false).outcome,
      FixOutcome::Skipped
    );
    assert_eq!(std::fs::read_to_string(&foreign).unwrap(), foreign_body);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn an_unreadable_tool_config_promises_no_repair() {
    // A finding `--fix` can only refuse must not advertise a repair: an agent
    // that branches on `fix` would run a fix that answers `skipped`.
    let finding = url_finding(
      stale_url::Scan::Unreadable {
        path: std::path::PathBuf::from("/home/u/.aider.conf.yml"),
        reason: "stream did not contain valid UTF-8".to_string(),
      },
      "http://127.0.0.1:11437/v1",
    );
    assert_eq!(finding.id, "tool_config_stale_url");
    assert_eq!(finding.severity, Severity::Warning);
    assert_eq!(finding.fix, None, "no repair is advertised");
    assert_ne!(finding.fix_hint, FIX_WITH_DOCTOR);
    assert!(finding.safe_to_log);
    let json = serde_json::to_string(&finding).unwrap();
    assert!(!json.contains("\"fix\":"), "{json}");
    assert!(json.contains("could not be read"), "{json}");
  }

  #[test]
  fn a_stale_finding_counts_its_addresses_once() {
    // Past the list cap the head and the list used to count the same remainder
    // two different ways inside one sentence, and at one address the pair was
    // printed twice over.
    let text = "http://127.0.0.1:11435/v1 http://127.0.0.1:11436/v1 \
                http://127.0.0.1:11438/v1 http://127.0.0.1:11439/v1";
    let live = stale_url::LiveProxy::from_base_url("http://127.0.0.1:11500/v1", &[11435])
      .expect("live proxy");
    let urls = stale_url::stale_urls(text, &live);
    assert_eq!(urls.len(), 4);
    let found: Vec<String> = urls.iter().map(|u| u.found.clone()).collect();
    let msg = stale_url_message(
      Path::new("/home/u/.config/zed/settings.json"),
      &found,
      "http://127.0.0.1:11500/v1",
      &stale_url::changes(&urls),
    );
    assert!(msg.contains("and 3 more"), "{msg}");
    assert!(msg.contains("(+1 more)"), "{msg}");
    assert_eq!(
      msg.matches("http://127.0.0.1:11435/v1").count(),
      2,
      "the head names the first address once: {msg}"
    );
    assert!(!msg.contains("and 4 more"), "{msg}");
  }

  #[test]
  fn a_stale_finding_names_both_addresses_in_words() {
    // The live address and the file's own address have to be readable without
    // parsing the `old → new` list back: that list is where a wrong rewrite is
    // easiest to miss.
    let one = stale_url_message(
      Path::new("/home/u/.aider.conf.yml"),
      &["http://127.0.0.1:11435/v1".to_string()],
      "http://127.0.0.1:11437/v1",
      "http://127.0.0.1:11435/v1 \u{2192} http://127.0.0.1:11437/v1",
    );
    assert!(
      one.contains("points at http://127.0.0.1:11435/v1, which nothing listens on"),
      "{one}"
    );
    assert!(
      one.contains("the proxy listens on http://127.0.0.1:11437/v1"),
      "{one}"
    );
    assert!(one.ends_with("http://127.0.0.1:11437/v1"), "{one}");
    assert!(!one.contains("more"), "one address is not counted: {one}");

    // Several in one file: the first is named and the rest counted, the same
    // shape the pair list keeps.
    let many = stale_url_message(
      Path::new("/home/u/.config/zed/settings.json"),
      &[
        "http://127.0.0.1:11435/v1".to_string(),
        "http://127.0.0.1:11435".to_string(),
      ],
      "http://127.0.0.1:11437/v1",
      "http://127.0.0.1:11435/v1 \u{2192} http://127.0.0.1:11437/v1, http://127.0.0.1:11435 \u{2192} http://127.0.0.1:11437",
    );
    assert!(many.contains("and 1 more"), "{many}");
    assert!(
      many.contains("the proxy listens on http://127.0.0.1:11437/v1"),
      "{many}"
    );
  }

  #[test]
  fn a_stale_tool_config_finding_carries_the_rewrite_repair() {
    // What `--json` prints for the finding and for the ledger entry, so a
    // caller can pair the two up.
    let finding = Finding::new(
      FindingId::ToolConfigStaleUrl,
      Severity::Warning,
      "`/home/u/.config/aider/config.yaml` points at a proxy address nothing listens on",
    );
    assert_eq!(finding.fix, Some(FixId::ToolConfigProxyUrl));
    assert_eq!(finding.fix_hint, FIX_WITH_DOCTOR);
    let json = serde_json::to_string(&finding).unwrap();
    assert!(json.contains("\"fix\":\"tool_config_proxy_url\""), "{json}");
    let report = FixReport::new(
      FixId::ToolConfigProxyUrl,
      "/home/u/.config/aider/config.yaml",
      FixOutcome::Applied,
      "rewritten",
    );
    assert_eq!(report.action, "rewrite url");
    assert!(
      serde_json::to_string(&report)
        .unwrap()
        .contains("\"fix\":\"tool_config_proxy_url\""),
      "the ledger entry names the repair"
    );
  }

  #[test]
  fn a_rewrite_with_no_live_proxy_is_ledged_not_dropped() {
    // `--fix` is never silent about what it left alone, so a stale-config
    // finding whose repair could not run answers with the reason rather than
    // nothing at all — once, however many files the report flagged.
    let findings = [
      Finding::new(FindingId::ToolConfigStaleUrl, Severity::Warning, "a"),
      Finding::new(FindingId::ToolConfigStaleUrl, Severity::Warning, "b"),
    ];
    let fixes = apply_fixes(&findings, false, None);
    assert_eq!(fixes.len(), 1, "{fixes:?}");
    assert_eq!(fixes[0].fix, FixId::ToolConfigProxyUrl);
    assert_eq!(fixes[0].outcome, FixOutcome::Skipped, "{fixes:?}");
    assert!(
      fixes[0].detail.contains("no proxy is answering"),
      "{fixes:?}"
    );
    assert!(
      serde_json::to_string(&fixes[0])
        .unwrap()
        .contains("\"outcome\":\"skipped\""),
      "the ledger entry reaches --json"
    );
  }

  #[test]
  fn format_human_lists_every_fix_outcome() {
    let _g = crate::cli::test_lock::serialize();
    let prior_colors = console::colors_enabled();
    console::set_colors_enabled(false);
    let mut report = build_report(None, &cpu_hw());
    report.findings.push(Finding::new(
      FindingId::StaleDaemonFiles,
      Severity::Warning,
      "leftovers",
    ));
    report.fixes = vec![
      FixReport::new(
        FixId::StaleDaemonFiles,
        "/state/runtime.json",
        FixOutcome::Applied,
        "handshake with no lock holder",
      ),
      FixReport::new(
        FixId::ConfigMode,
        "/state/config.yaml",
        FixOutcome::WouldApply,
        "mode 0o644 → 0600",
      ),
      FixReport::skipped(
        FixId::ConfigMode,
        "/other/config.yaml",
        "is world-writable (mode 0o777)",
      ),
      FixReport::new(
        FixId::ConfigMode,
        "/ro/config.yaml",
        FixOutcome::Failed,
        "Read-only file system (os error 30)",
      ),
    ];
    let out = format_human(&report);
    console::set_colors_enabled(prior_colors);
    assert!(
      out.contains("fixes (4 actions)\n"),
      "fixes section header drift: {out:?}"
    );
    assert!(
      out.contains("✓ remove /state/runtime.json (handshake with no lock holder)"),
      "{out:?}"
    );
    assert!(
      out.contains("would chmod 0600 /state/config.yaml (mode 0o644 → 0600)"),
      "{out:?}"
    );
    assert!(
      out.contains("chmod 0600 /other/config.yaml — skipped: is world-writable (mode 0o777)"),
      "{out:?}"
    );
    assert!(
      out.contains("chmod 0600 /ro/config.yaml failed: Read-only file system (os error 30)"),
      "{out:?}"
    );
  }
}
