//! `llamastash doctor` read-only diagnostic.
//!
//! Re-runs hardware + binary detection, loads `_init_snapshot.json`,
//! compares the two, emits 0-N findings. Every finding carries a
//! stable `id` agent consumers can branch on plus a
//! `fix_hint = "llamastash init --only X"` that maps to the wizard
//! step that resolves it.
//!
//! Output is always safe to paste into a public issue — see the
//! Security Contract addendum's redaction rule in the v2 plan.
//! `safe_to_log` is unconditionally `true` for v2 findings; a future
//! finding that legitimately needs differentiated redaction lands
//! the per-finding flag *then*, not preemptively.

use std::path::Path;

use serde::Serialize;

use crate::backend::Backend;
use crate::cli::cli_args::{Cli, DoctorArgs};
use crate::cli::exit_codes::CliResult;
use crate::config::Config;
use crate::gpu::{ClassSource, GpuInfo};
use crate::init::detection::{detect_hardware, HardwareSnapshot, OsFamily};
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
/// v2: added the `hardware` section and the `memory_drift` / `gtt_hint`
/// finding ids (R12-R14).
pub const DOCTOR_JSON_SCHEMA_VERSION: u32 = 2;

/// `SnapshotStale` finding fires when the bundled snapshot is older
/// than this many days vs today.
pub const STALE_SNAPSHOT_THRESHOLD_DAYS: u64 = 14;

/// `RemoteSnapshotUnreachable` finding fires after this many
/// consecutive remote-fetch failures.
pub const REMOTE_UNREACHABLE_THRESHOLD: u32 = 3;

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
  RemoteSnapshotUnreachable,
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
      Self::RemoteSnapshotUnreachable => "remote_snapshot_unreachable",
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
      Self::ConfigModeDrift => "llamastash init --only config",
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
}

impl Finding {
  fn new(id: FindingId, severity: Severity, message: impl Into<String>) -> Self {
    Self::from_parts(id.as_str(), severity, message, id.fix_hint())
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
    }
  }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Baseline {
  pub snapshot_bundle_date: Option<String>,
  pub init_date: Option<String>,
}

/// Live hardware section of the doctor report. Built from the
/// same [`HardwareSnapshot`] the init banner renders, with the R15
/// label conventions (`MEM`/`MEM*`, `VRAM (shared)`) from day one.
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
  if let Some(finding) = check_config_mode_drift() {
    findings.push(finding);
  }
  if let Some(finding) = check_remote_snapshot_unreachable(snapshot) {
    findings.push(finding);
  }
  DoctorReport {
    schema_version: DOCTOR_JSON_SCHEMA_VERSION,
    findings,
    baseline,
    hardware: hardware_section,
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

/// R13 memory-drift finding. Compares the freshly-detected GPU pool
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

/// R14 GTT-cap hint. Fires on Linux unified hosts whose GTT pool is
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

fn check_config_mode_drift() -> Option<Finding> {
  let path = crate::util::paths::user_config_file()?;
  if !path.exists() {
    return None;
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let file_meta = std::fs::metadata(&path).ok()?;
    let file_mode = file_meta.permissions().mode() & 0o777;
    if file_mode != 0o600 {
      return Some(Finding::new(
        FindingId::ConfigModeDrift,
        Severity::Warning,
        format!(
          "`{}` is mode {file_mode:#o} (expected 0600) — \
           re-run init or `chmod 600` to restore the hardening",
          path.display()
        ),
      ));
    }
    if let Some(parent) = path.parent() {
      if let Ok(pmeta) = std::fs::metadata(parent) {
        let pmode = pmeta.permissions().mode() & 0o777;
        if pmode & 0o022 != 0 {
          return Some(Finding::new(
            FindingId::ConfigModeDrift,
            Severity::Warning,
            format!(
              "parent dir `{}` is group/world-writable (mode {pmode:#o}) — \
               `chmod 700` recommended",
              parent.display()
            ),
          ));
        }
      }
    }
  }
  let _ = path;
  None
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

/// Render the live hardware section with the R15 label
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

  // R13 baseline stamp/refresh — doctor's single documented write,
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

  // Backend-contributed advisories (D-doctor): each backend adds its own
  // findings via the `doctor_findings` hook (ds4's "compatible model present but
  // engine unavailable", say). Collected generically over the registry so this
  // path names no backend; every id stays additive (schema stays 2).
  for backend in crate::backend::Backends::all() {
    report
      .findings
      .extend(backend.doctor_findings(config).await);
  }

  // Server-catalog advisory: configured `servers:` health across backends.
  report.findings.extend(check_servers(config));

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
      FindingId::RemoteSnapshotUnreachable,
    ];
    for id in ids {
      assert!(!id.fix_hint().is_empty(), "{id:?} must have a fix_hint");
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
}
