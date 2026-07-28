//! Best-effort GPU detection at daemon start.
//!
//! v1 baseline strategy: shell out to vendor tools and parse their
//! human/JSON output rather than linking native SDKs (NVML, ROCm,
//! Metal). This keeps the build portable across CUDA / ROCm /
//! Apple Silicon machines without conditional native deps. Future
//! follow-up: replace the shell-out with `nvml-wrapper` on Linux
//! for accurate per-PID VRAM attribution.
//!
//! Detection order:
//! 1. NVIDIA via `nvidia-smi --query-gpu=...` (Linux + Windows) — wins
//!    when available because it surfaces live util%/temperature that
//!    DXGI can't.
//! 2. AMD via `rocm-smi --showmeminfo vram --json` (Linux). Windows
//!    AMD doesn't ship `rocm-smi.exe`, so the DXGI step below covers
//!    it.
//! 3. **Windows-only:** DXGI via `IDXGIFactory1::EnumAdapters1` —
//!    static adapter name + dedicated VRAM + shared system memory
//!    for AMD / Intel / and the rare NVIDIA-without-nvidia-smi.exe
//!    stripped-install case. No live metrics (DXGI doesn't expose
//!    them); host pane renders `—` for util/temp on this path.
//! 4. Apple Silicon Metal via `system_profiler SPDisplaysDataType
//!    -json` (macOS).
//! 5. Vulkan fallback (`vulkaninfo --summary`) — Linux Vulkan-only
//!    AMD or Intel Arc machines without rocm-smi. Reports adapter
//!    names only; surfaces under `Unknown`.
//! 6. Final fallback: `CpuOnly` — supervisor still runs.

pub mod amd;
#[cfg(windows)]
pub mod dxgi;
pub mod metal;
pub mod nvidia;
#[cfg(target_os = "linux")]
pub mod sysfs;
pub mod vulkan;

/// Dedicated-VRAM ceiling below which an AMD card is classified as an
/// integrated UMA APU by carve-out signature. No discrete GPU
/// ships with under 1 GiB of dedicated VRAM, so a tiny `vram_total`
/// paired with a large system-RAM-backed GTT pool is the APU marker.
///
/// This is the *fallback* heuristic. The authoritative signal is the
/// PCI class (`0x0380` = Display controller → integrated), read by the
/// sysfs probe and passed as `is_integrated` to [`classify_amd_memory`]
/// — necessary because amdgpu auto-detects a 4 GiB VRAM region on Strix
/// Halo regardless of the BIOS carve-out, sailing past this 1 GiB
/// ceiling. The size heuristic still covers the rocm-smi fallback path
/// (no PCI class) and APUs that report a small carve. The verdict's
/// source is surfaced in `doctor` so it stays inspectable.
pub const CARVE_VRAM_CEILING_BYTES: u64 = 1024 * 1024 * 1024;

/// Decide whether a card's dedicated-VRAM size matches the integrated
/// UMA carve-out signature. `true` → treat as unified (sum VRAM + GTT,
/// mark the GTT portion as the shared system-RAM pool).
pub fn is_carve_signature(vram_total: u64) -> bool {
  vram_total < CARVE_VRAM_CEILING_BYTES
}

/// Classify an AMD card from its VRAM / GTT sizes — the single
/// source of truth shared by the sysfs probe and the rocm-smi fallback.
/// Returns `(total_memory_bytes, used_memory_bytes, uma_shared_total,
/// uma_shared_used, source)`. An integrated/UMA APU sums VRAM + GTT and
/// marks the GTT portion as the shared system-RAM pool; a discrete card
/// reports VRAM only.
///
/// `is_integrated` is the authoritative signal (the sysfs probe reads it
/// from the PCI class — Strix Halo and friends report VRAM ≫ 1 GiB so
/// the size heuristic alone misses them). It ORs with the legacy
/// carve-out signature (tiny VRAM + large GTT) so an APU still
/// classifies as unified when the class is unavailable (rocm-smi path).
pub fn classify_amd_memory(
  vram_total: u64,
  vram_used: u64,
  gtt_total: Option<u64>,
  gtt_used: Option<u64>,
  is_integrated: bool,
) -> (u64, u64, Option<u64>, Option<u64>, ClassSource) {
  if is_integrated || is_carve_signature(vram_total) {
    let gt = gtt_total.unwrap_or(0);
    let gu = gtt_used.unwrap_or(0);
    (
      vram_total.saturating_add(gt),
      vram_used.saturating_add(gu),
      Some(gt),
      Some(gu),
      ClassSource::CarveSignature,
    )
  } else {
    (vram_total, vram_used, None, None, ClassSource::Discrete)
  }
}

use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// When `true`, the Vulkan fallback probe (`vulkaninfo -j` / `--summary`)
/// is skipped entirely. Set via [`set_disable_vulkan_probe`]; the CLI
/// daemon handler threads `config.gpu.disable_vulkan_probe` through.
static DISABLE_VULKAN_PROBE: AtomicBool = AtomicBool::new(false);

/// Set the global Vulkan probe disable flag. Safe to call once at daemon
/// startup; the value is read by [`probe`] on every call.
pub fn set_disable_vulkan_probe(v: bool) {
  DISABLE_VULKAN_PROBE.store(v, Ordering::Relaxed);
}

/// Wall-clock budget for a single vendor probe. A wedged GPU driver
/// (nvidia-smi hang, ROCm reset, locked Vulkan loader) would otherwise
/// pin the blocking pool thread indefinitely. Five seconds is well
/// above any normal vendor-tool invocation on healthy hardware.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Run an external probe with a wall-clock deadline. On expiry the
/// child is killed; the call returns `None` so the probe chain can
/// fall through to the next backend instead of stalling the daemon.
///
/// Delegates to [`crate::util::process::run_with_drain_and_timeout`]
/// so the spawn-poll-drain pattern is shared with smoke and brew.
pub(crate) fn run_with_timeout(cmd: Command) -> Option<Output> {
  let program = format!("{:?}", cmd.get_program());
  match crate::util::process::run_with_drain_and_timeout(cmd, PROBE_TIMEOUT) {
    Ok(out) => Some(out),
    Err(crate::util::process::RunError::Timeout { after }) => {
      log::warn!("gpu probe `{program}` exceeded {after:?}; killed and falling through");
      None
    }
    Err(_) => None,
  }
}

/// Normalize a PCI bus address to canonical `00000000:bb:dd.f`.
///
/// Handles variants:
/// - `00000000:0F:00.0` (NVIDIA nvml — 8-char, uppercase)
/// - `0000:0E:00.0` (rocm-smi — 4-char domain, uppercase)
/// - `0e:00.0` (lspci short — no domain)
/// - `00000000:0f:00.0` (vulkaninfo — already canonical)
pub(crate) fn normalize_pci(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  // `[domain:]bus:device.function`. nvml / rocm-smi / vulkaninfo all
  // emit the domain; lspci's short form omits it (domain 0 implied).
  let parts: Vec<&str> = trimmed.split(':').collect();
  let (domain, bus, dev_fn) = match parts.as_slice() {
    [bus, dev_fn] => (0u32, *bus, *dev_fn),
    [domain, bus, dev_fn] => (u32::from_str_radix(domain.trim(), 16).ok()?, *bus, *dev_fn),
    _ => return None,
  };
  let bus = u8::from_str_radix(bus.trim(), 16).ok()?;
  // Split `device.function`; a missing dot means function 0.
  let (dev, func) = match dev_fn.trim().split_once('.') {
    Some((d, f)) => (d.trim(), f.trim()),
    None => (dev_fn.trim(), "0"),
  };
  let dev = u8::from_str_radix(dev, 16).ok()?;
  let func = u8::from_str_radix(func, 16).ok()?;
  Some(format!("{domain:08x}:{bus:02x}:{dev:02x}.{func:x}"))
}

/// What detection found. Always a complete snapshot — no
/// "partial" / "unknown" middle ground — so the IPC handler can
/// serialise it directly into `status`.
///
/// Single-backend hits return the corresponding variant; when two or
/// more backends each find at least one device the `Multi` variant
/// carries all of them (each tagged with its backend) so the host
/// stats pane can render per-GPU rows instead of hiding half the
/// hardware.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum GpuInfo {
  /// No GPU detected (or detection failed). The daemon still runs;
  /// `llama-server` falls back to CPU inference.
  CpuOnly,
  /// NVIDIA card(s) found. Multi-GPU machines surface as a list of
  /// devices.
  Nvidia { devices: Vec<GpuDevice> },
  /// AMD card(s) found.
  Amd { devices: Vec<GpuDevice> },
  /// Apple Silicon — unified-memory GPU. Reports the system memory
  /// available to the GPU since Metal doesn't separate VRAM.
  AppleMetal { total_memory_bytes: u64 },
  /// `vulkaninfo` found a device but neither NVIDIA nor ROCm probes
  /// succeeded, so the vendor is unknown. The supervisor still hints
  /// that the user can attempt `-ngl > 0`; the host pane renders
  /// `backend  unknown` rather than mislabelling the card.
  Unknown { devices: Vec<GpuDevice> },
  /// Multiple backends each found one or more GPUs. Carries a
  /// per-device `backend` tag so the host stats pane can group and
  /// label them independently and render one row per device.
  Multi { devices: Vec<GpuDevice> },
}

/// How a device's unified-vs-discrete verdict was reached.
/// Surfaced in the `doctor` hardware section so a misclassification is
/// inspectable rather than silent. The serialized snake_case value
/// (`apple_unified` / `explicit_dxgi_uma` / `carve_signature` /
/// `discrete`) is the precise *method* and stays in `--json`; the human
/// [`Self::label`] collapses it to the verdict + confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassSource {
  /// Apple Silicon — unified by construction (the architecture has no
  /// dedicated VRAM at all). Authoritative.
  AppleUnified,
  /// Windows D3D12 `UMA` architecture flag — authoritative.
  ExplicitDxgiUma,
  /// Linux amdgpu: no driver flag exists; classified unified by the
  /// VRAM carve-out signature (tiny dedicated VRAM + large GTT pool).
  CarveSignature,
  /// Classified as a discrete card (dedicated VRAM at or above the
  /// carve ceiling, or the explicit flag reported non-UMA).
  Discrete,
}

impl ClassSource {
  /// Human label shown after the GPU pool size on `doctor` / `status`:
  /// the *verdict* (`unified` / `discrete`) plus a confidence qualifier
  /// when the verdict was inferred rather than read from an
  /// authoritative flag. The precise method survives in `--json` via the
  /// serialized enum value.
  pub fn label(self) -> &'static str {
    match self {
      ClassSource::AppleUnified | ClassSource::ExplicitDxgiUma => "unified",
      ClassSource::CarveSignature => "unified, inferred",
      ClassSource::Discrete => "discrete",
    }
  }
}

/// One discrete GPU device (NVIDIA / AMD path).
///
/// `utilization_pct` and `temperature_c` are best-effort: the per-tick
/// host-metrics sampler reads them from vendor tools that may or may
/// not expose them on a given platform / driver version. When a probe
/// can't surface them they stay `None`; the host stats pane renders
/// `—` in place of a numeric reading rather than dropping the row.
///
/// `backend` tags which probe produced this device ("nvidia", "amd",
/// "apple_metal", or "unknown"). Used when combining multi-backend
/// snapshots into a `GpuInfo::Multi`.
///
/// Note: this struct intentionally does not derive `Eq` because the
/// `f32` fields don't satisfy `Eq` (NaN-not-equal-to-itself). The
/// `PartialEq` derive is sufficient for the only equality use case
/// today — round-tripping in tests. Downstream consumers needing a
/// hashable / `Eq`-bound view should compare a projection (e.g. the
/// `name` + `total_memory_bytes` fields) rather than the whole struct.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GpuDevice {
  pub name: String,
  pub backend: String,
  pub total_memory_bytes: u64,
  pub used_memory_bytes: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub utilization_pct: Option<f32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub temperature_c: Option<f32>,
  /// Physical identifier stable across backends (PCI bus address,
  /// IOKit serial, or DXGI PCI path). Used to deduplicate cards
  /// found via multiple drivers.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub device_id: Option<String>,
  /// Portion of `total_memory_bytes` that lives in the system RAM
  /// pool (e.g. AMD GTT on UMA APUs like Strix Halo). When `Some`,
  /// the host pane subtracts this from the RAM gauge so the same
  /// bytes aren't counted twice (once as VRAM, once as system RAM).
  /// `None` on discrete cards and any backend without a UMA mode.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uma_shared_total_bytes: Option<u64>,
  /// Currently-allocated portion of `uma_shared_total_bytes`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uma_shared_used_bytes: Option<u64>,
  /// How this device's unified-vs-discrete verdict was reached.
  /// `None` on backends that don't classify (NVIDIA, Vulkan/unknown).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub classification_source: Option<ClassSource>,
}

impl GpuInfo {
  pub fn label(&self) -> &'static str {
    match self {
      Self::CpuOnly => "cpu_only",
      Self::Nvidia { .. } => "nvidia",
      Self::Amd { .. } => "amd",
      Self::AppleMetal { .. } => "apple_metal",
      Self::Unknown { .. } => "unknown",
      Self::Multi { .. } => "multi",
    }
  }

  /// True for any snapshot that found at least one GPU (everything
  /// except `CpuOnly`).
  pub fn is_gpu(&self) -> bool {
    !matches!(self, Self::CpuOnly)
  }

  /// Single source of truth for "is this backend unified memory?" —
  /// the GPU shares one physical pool with the CPU rather than owning
  /// dedicated VRAM. Both the init banner and the TUI host pane render
  /// from this so the two never disagree (the `*`/"unified" marker).
  ///
  /// - Apple Silicon (Metal) is unified by construction.
  /// - AMD / Nvidia / Unknown are unified when a device carries a
  ///   `uma_shared_total_bytes` portion — set by `rocm-smi`'s GTT pool
  ///   on Linux APUs and by the D3D12 `UMA` architecture flag on
  ///   Windows. Discrete cards never populate it.
  /// - CpuOnly has no GPU memory at all.
  pub fn is_unified(&self) -> bool {
    match self {
      Self::AppleMetal { .. } => true,
      Self::Multi { devices } => devices.iter().any(|d| d.uma_shared_total_bytes.is_some()),
      Self::Nvidia { devices } | Self::Amd { devices } | Self::Unknown { devices } => {
        devices.iter().any(|d| d.uma_shared_total_bytes.is_some())
      }
      Self::CpuOnly => false,
    }
  }

  /// The classification verdict + method for this host's GPU memory,
  /// for the `doctor` hardware section + `status` GPU line. Apple
  /// Metal is `AppleUnified` (unified by construction); AMD / DXGI
  /// devices carry an explicit [`ClassSource`]; a NVIDIA / AMD card with
  /// no unified marker reports `Discrete` (the verdict — a real GPU that
  /// isn't unified is discrete). Only the vendor-unknown Vulkan fallback
  /// and CPU-only return `None`, since there we genuinely can't say.
  pub fn uma_class_source(&self) -> Option<ClassSource> {
    match self {
      // Apple Metal is unified by construction — report it explicitly so
      // every surface labels it from the same source as the AMD/DXGI
      // paths rather than rendering a bare vendor word.
      Self::AppleMetal { .. } => Some(ClassSource::AppleUnified),
      Self::CpuOnly => None,
      // Vulkan fallback: vendor is unknown, so discrete-vs-unified is
      // genuinely undecidable — no suffix rather than a guessed one.
      Self::Unknown { .. } => None,
      Self::Multi { devices } | Self::Nvidia { devices } | Self::Amd { devices } => {
        // Prefer a unified verdict when any device reports one (a UMA
        // APU paired with a discrete card budgets as unified). A real GPU
        // with no unified marker (e.g. NVIDIA, which doesn't classify) is
        // discrete — fall back to that verdict rather than `None`.
        devices
          .iter()
          .filter_map(|d| d.classification_source)
          .find(|s| !matches!(s, ClassSource::Discrete))
          .or(Some(ClassSource::Discrete))
      }
    }
  }
}

/// `(card name → PCI address, "vendor:device" → PCI address)` as
/// returned by [`query_lspci`]. The named alias keeps the consumers'
/// signatures readable and drops the `clippy::type_complexity` allows.
type LspciMaps = (
  std::collections::BTreeMap<String, String>,
  std::collections::BTreeMap<String, String>,
);

/// lspci is Linux-only. On macOS/Windows the backend probes already
/// emit canonical device IDs, so the cross-backend dedup fallback is
/// never needed and this returns `None`.
#[cfg(not(target_os = "linux"))]
fn query_lspci() -> Option<LspciMaps> {
  None
}

/// Run `lspci -D -nn` and parse it into the lookup maps. Returns `None`
/// when lspci is missing/fails or finds no GPU lines. `-nn` is required
/// for the numeric `[vendor:device]` ids; `-D` forces the domain into
/// the address so it matches the nvml/rocm canonical form.
#[cfg(target_os = "linux")]
fn query_lspci() -> Option<LspciMaps> {
  let mut cmd = std::process::Command::new("lspci");
  cmd.args(["-D", "-nn"]);
  let output = run_with_timeout(cmd)?;
  if !output.status.success() {
    return None;
  }
  let stdout = String::from_utf8(output.stdout).ok()?;
  let maps = parse_lspci(&stdout);
  if maps.0.is_empty() && maps.1.is_empty() {
    None
  } else {
    Some(maps)
  }
}

/// Parse `lspci -D -nn` output into `(name → addr, "vendor:device" →
/// addr)`. A GPU line looks like:
/// `0000:0f:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA102 [GeForce RTX 3080] [10de:2206] (rev a1)`
/// — the leading token is the PCI bus address, and the trailing
/// `[vvvv:dddd]` bracket (only present with `-nn`) is the numeric
/// vendor:device id used to resolve a Vulkan device that reports
/// `0xVVVV:0xDDDD` instead of a bus address.
#[cfg(any(target_os = "linux", test))]
fn parse_lspci(stdout: &str) -> LspciMaps {
  let mut name_map = std::collections::BTreeMap::new();
  let mut pci_id_map = std::collections::BTreeMap::new();
  for line in stdout.lines() {
    let line = line.trim();
    if !line.contains("VGA") && !line.contains("Display") && !line.contains("3D") {
      continue;
    }
    let Some((addr_tok, rest)) = line.split_once(' ') else {
      continue;
    };
    let Some(addr) = normalize_pci(addr_tok) else {
      continue;
    };
    if let Some((vendor, device)) = last_vendor_device(rest) {
      pci_id_map.insert(format!("{vendor}:{device}"), addr.clone());
    }
    if let Some(name) = lspci_card_name(rest) {
      name_map.insert(name, addr.clone());
    }
  }
  (name_map, pci_id_map)
}

/// Find the last `[vvvv:dddd]` numeric vendor:device bracket in the
/// line tail, returning lowercase `(vendor, device)` hex strings. The
/// class bracket (`[0300]`) and the marketing-name bracket carry no
/// colon / non-hex, so they're skipped.
#[cfg(any(target_os = "linux", test))]
fn last_vendor_device(s: &str) -> Option<(String, String)> {
  let mut rest = s;
  while let Some(open) = rest.rfind('[') {
    if let Some(close_rel) = rest[open..].find(']') {
      let inner = &rest[open + 1..open + close_rel];
      if let Some((v, d)) = inner.split_once(':') {
        let (v, d) = (v.trim(), d.trim());
        if u16::from_str_radix(v, 16).is_ok() && u16::from_str_radix(d, 16).is_ok() {
          return Some((v.to_lowercase(), d.to_lowercase()));
        }
      }
    }
    rest = &rest[..open];
  }
  None
}

/// Best-effort card name: the text after the class-bracket `]:`, with
/// the trailing `[vendor:device]` group trimmed. lspci names rarely
/// equal the vendor tools' marketing strings, so this is a secondary
/// fallback behind the PCI-id match.
#[cfg(any(target_os = "linux", test))]
fn lspci_card_name(rest: &str) -> Option<String> {
  let after = rest.split_once("]:").map(|(_, t)| t).unwrap_or(rest).trim();
  let name = after
    .rsplit_once(" [")
    .map(|(head, _)| head)
    .unwrap_or(after)
    .trim();
  (!name.is_empty()).then(|| name.to_string())
}

/// Resolve a canonical PCI address for a device.
///
/// Uses the device's own `device_id` (already normalized to canonical
/// `00000000:bb:dd.f` by the backend probe) as the primary source.
/// Falls back to lspci name lookup when device_id is absent or
/// looks like a vendor:device ID (e.g. "0x1002:0x7551").
fn resolve_device_id(device: &GpuDevice, lspci: &Option<LspciMaps>) -> String {
  if let Some(pci) = &device.device_id {
    // PCI bus addresses: "00000002:00.0" — contains ':' but doesn't
    // start with "0x".
    // vendor:device IDs: "0x10de:0x2216" — starts with "0x".
    if pci.contains(':') && !pci.starts_with("0x") {
      return pci.clone();
    }
  }
  // Fall back to lspci lookups (name map and PCI-ID map).
  lspci
    .as_ref()
    .and_then(|(name_map, pci_id_map)| {
      name_map
        .get(&device.name)
        .or_else(|| {
          // vulkaninfo reports vendor:device as "0xVVVV:0xDDDD"; strip
          // the "0x" off *each* part (not just the leading one) and
          // lowercase so it matches lspci's "vvvv:dddd" key. Leading
          // zeros are kept — `last_vendor_device` stores the raw
          // bracket digits, so both sides agree.
          if let Some(pci) = &device.device_id {
            if pci.starts_with("0x") && pci.contains(':') {
              let key = pci
                .split(':')
                .map(|part| part.trim().trim_start_matches("0x").to_lowercase())
                .collect::<Vec<_>>()
                .join(":");
              return pci_id_map.get(&key);
            }
          }
          None
        })
        .cloned()
    })
    .unwrap_or_else(|| device.name.clone())
}

/// Normalize a GPU product name for cross-backend dedup when PCI
/// resolution is unavailable.
///
/// Vulkan reports the same physical card under a decorated name —
/// `vulkaninfo` appends a driver tag (`AMD Radeon AI PRO R9700 (RADV
/// GFX1201)`) while `rocm-smi` reports the bare `AMD Radeon AI PRO
/// R9700`. When lspci can't map both to one PCI address (the primary
/// dedup key), the raw-name fallback in [`resolve_device_id`] never
/// collapses them and a 0-VRAM Vulkan duplicate survives. Stripping any
/// trailing parenthetical group and normalizing whitespace + case lets
/// that fallback path still match.
fn normalize_card_name(name: &str) -> String {
  let base = match name.split_once('(') {
    Some((head, _)) => head,
    None => name,
  };
  let lowered = base
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();
  // Strip a single leading vendor token: one tool emits it (Vulkan's
  // "AMD Radeon RX 7900 XT"), another omits it (rocm-smi's "Radeon RX
  // 7900 XT"). Removing it from both sides closes that match gap without
  // over-collapsing — distinct cards still differ on the model tokens.
  for vendor in ["amd", "nvidia", "intel", "ati", "apple"] {
    if let Some(rest) = lowered.strip_prefix(&format!("{vendor} ")) {
      return rest.to_string();
    }
  }
  lowered
}

/// True when `candidate` (a Vulkan-fallback device) is the same physical
/// card a native probe already reported — matched by PCI id, else by
/// normalized name. The name fallback is coarse: with no PCI id (Windows
/// DXGI carries none, and there's no `lspci`) names that don't normalize
/// alike won't dedup, leaving a phantom `Multi` entry. See TODO.
fn is_cross_probe_duplicate(
  candidate: &GpuDevice,
  natives: &[&GpuDevice],
  lspci: &Option<LspciMaps>,
) -> bool {
  let id = resolve_device_id(candidate, lspci);
  let norm = normalize_card_name(&candidate.name);
  natives
    .iter()
    .any(|seen| resolve_device_id(seen, lspci) == id || normalize_card_name(&seen.name) == norm)
}

/// Enrich a list of devices with lspci PCI address lookups.
///
/// For devices whose `device_id` is missing or looks like a
/// vendor:device ID (e.g. "0x1002:0x7551" from vulkaninfo),
/// resolve the canonical PCI address using the lspci name map.
fn enrich_with_lspci(devices: &mut [GpuDevice], lspci: &Option<LspciMaps>) {
  for d in devices.iter_mut() {
    if let Some(pci) = &d.device_id {
      // Already has a canonical PCI address — skip.
      if pci.contains(':') && !pci.starts_with("0x") {
        continue;
      }
    }
    // Resolve using lspci.
    d.device_id = Some(resolve_device_id(d, lspci));
  }
}

/// Run the full detection chain. Best-effort — every probe failure
/// falls through to the next backend. Unlike the v1 single-hit probe,
/// this collects from **all** backends and returns a `Multi` snapshot
/// when two or more backends each find at least one device. A single-
/// backend hit returns that backend's native variant.
///
/// Devices from every backend are deduplicated (by PCI address on
/// Linux, by name elsewhere) so the same physical card seen through
/// two drivers — e.g. ROCm and Vulkan — collapses to one entry instead
/// of a phantom 0-VRAM duplicate. Launch-side device selection is
/// separate: it reads `llama-server --list-devices` (see
/// [`crate::backend::llama_cpp::list_devices`]), not this probe.
///
/// Suitable for daemon startup and periodic hotplug-detection
/// passes; the per-tick host-metrics refresh uses [`refresh_active`]
/// to avoid spawning every vendor tool every second.
pub fn probe() -> GpuInfo {
  let mut nvidia_devices: Vec<GpuDevice> = Vec::new();
  let mut amd_devices: Vec<GpuDevice> = Vec::new();
  let mut metal_devices: Vec<GpuDevice> = Vec::new();
  let mut unknown_devices: Vec<GpuDevice> = Vec::new();

  // NVIDIA probe
  if let Some(devs) = nvidia::probe_devices() {
    nvidia_devices = devs;
  }
  // AMD probe
  if let Some(devs) = amd::probe_devices() {
    amd_devices = devs;
  }
  // Windows-only: DXGI fills the AMD / Intel slot that `rocm-smi`
  // doesn't reach. Also catches NVIDIA on stripped Windows installs
  // where `nvidia-smi.exe` isn't on PATH. Static memory totals only —
  // no live util/temp.
  #[cfg(windows)]
  {
    if let Some(devs) = dxgi::probe_devices() {
      amd_devices.extend(devs.clone());
    }
  }
  // Apple Silicon probe
  if let Some(devs) = metal::probe_devices() {
    metal_devices = devs;
  }
  // Vulkan fallback (skipped when `config.gpu.disable_vulkan_probe` is set)
  if !DISABLE_VULKAN_PROBE.load(Ordering::Relaxed) {
    if let Some(devs) = vulkan::probe_devices() {
      unknown_devices = devs;
    }
  }

  let raw_total =
    nvidia_devices.len() + amd_devices.len() + metal_devices.len() + unknown_devices.len();
  // lspci feeds cross-probe dedup, which only matters with >1 device;
  // skip the subprocess for a lone card (it hits a single-device variant
  // that never reads `device_id`).
  let lspci = if raw_total > 1 { query_lspci() } else { None };
  resolve_devices(
    nvidia_devices,
    amd_devices,
    metal_devices,
    unknown_devices,
    lspci,
  )
}

/// Pure dedup + classification over already-probed per-backend device
/// lists. Split out of [`probe`] so every hardware combination is
/// unit-testable without the vendor-tool IO that made this logic
/// regress repeatedly on hardware no test could reach. `lspci` is the
/// Linux cross-probe PCI map (`None` on other platforms / single-device
/// hosts, where dedup isn't needed).
pub(crate) fn resolve_devices(
  mut nvidia_devices: Vec<GpuDevice>,
  mut amd_devices: Vec<GpuDevice>,
  mut metal_devices: Vec<GpuDevice>,
  mut unknown_devices: Vec<GpuDevice>,
  lspci: Option<LspciMaps>,
) -> GpuInfo {
  // Nothing found anywhere → CPU-only.
  if nvidia_devices.is_empty()
    && amd_devices.is_empty()
    && metal_devices.is_empty()
    && unknown_devices.is_empty()
  {
    return GpuInfo::CpuOnly;
  }

  // Collapse cross-probe duplicates BEFORE counting: the Vulkan fallback
  // re-detects cards a native/DXGI probe already found (the norm on
  // Windows), and counting one card twice mislabels it `Multi`.
  let raw_total =
    nvidia_devices.len() + amd_devices.len() + metal_devices.len() + unknown_devices.len();
  if raw_total > 1 {
    enrich_with_lspci(&mut nvidia_devices, &lspci);
    enrich_with_lspci(&mut amd_devices, &lspci);
    enrich_with_lspci(&mut metal_devices, &lspci);
    enrich_with_lspci(&mut unknown_devices, &lspci);
    let natives: Vec<&GpuDevice> = nvidia_devices
      .iter()
      .chain(amd_devices.iter())
      .chain(metal_devices.iter())
      .collect();
    unknown_devices.retain(|d| !is_cross_probe_duplicate(d, &natives, &lspci));
  }

  // Backend variant is decided by how many *distinct backends* survived
  // dedup, not the device count. `Multi` means two or more backends each
  // found a card (the documented `gpu_backend = "multi"` contract); a
  // single backend with several cards (dual-RTX, CrossFire) stays that
  // backend's variant so routing/admission see the real vendor — not a
  // generic `multi` that, e.g., dropped a dual-NVIDIA host onto the
  // Vulkan build instead of CUDA.
  let backends_present = [
    !nvidia_devices.is_empty(),
    !amd_devices.is_empty(),
    !metal_devices.is_empty(),
    !unknown_devices.is_empty(),
  ]
  .into_iter()
  .filter(|present| *present)
  .count();

  if backends_present == 1 {
    if !metal_devices.is_empty() {
      // Apple is one unified GPU; report its memory for the shared pool.
      return GpuInfo::AppleMetal {
        total_memory_bytes: metal_devices[0].total_memory_bytes,
      };
    }
    if !nvidia_devices.is_empty() {
      return GpuInfo::Nvidia {
        devices: nvidia_devices,
      };
    }
    if !amd_devices.is_empty() {
      return GpuInfo::Amd {
        devices: amd_devices,
      };
    }
    return GpuInfo::Unknown {
      devices: unknown_devices,
    };
  }

  // Two or more distinct backends — build one combined list. Devices are
  // already lspci-enriched and the Vulkan fallback's duplicates of a
  // native card were dropped above; the check in the loop below now only
  // guards against duplicates *within* the surviving set.
  let mut all_devices = Vec::new();
  for d in nvidia_devices {
    all_devices.push(d);
  }
  for d in amd_devices {
    all_devices.push(d);
  }
  for d in metal_devices {
    all_devices.push(d);
  }
  for d in unknown_devices {
    // Skip Vulkan devices that match an already-seen card by PCI
    // address (via lspci on Linux) or by name. The PCI path is exact;
    // when it's unavailable we fall back to a normalized-name match so
    // a RADV-decorated Vulkan duplicate of a ROCm/CUDA card still
    // collapses instead of surfacing as a phantom 0-VRAM device.
    let seen_id = resolve_device_id(&d, &lspci);
    let norm_name = normalize_card_name(&d.name);
    let is_duplicate = all_devices.iter().any(|seen| {
      resolve_device_id(seen, &lspci) == seen_id || normalize_card_name(&seen.name) == norm_name
    });
    if is_duplicate {
      continue;
    }
    all_devices.push(d);
  }

  GpuInfo::Multi {
    devices: all_devices,
  }
}

/// Refresh the already-detected backends by calling only their vendor
/// probes. Returns a new `GpuInfo` when at least one backend changed
/// this tick, `None` when nothing changed.
///
/// For single-backend hits the path is trivial (one vendor tool per
/// tick). For `Multi` we refresh every backend that previously had
/// devices so we catch driver rebinds, hotplugged cards, and late
/// driver loads.
///
/// This is the per-tick fast path used by the host-metrics sampler.
/// CPU-only / Vulkan / Metal hosts skip per-tick spawns entirely
/// (the periodic full re-probe in the sampler still catches hotplug /
/// late driver loads).
pub fn refresh_active(prev: &GpuInfo) -> Option<GpuInfo> {
  match prev {
    GpuInfo::CpuOnly | GpuInfo::AppleMetal { .. } | GpuInfo::Unknown { .. } => None,
    GpuInfo::Nvidia { .. } => nvidia::probe_devices().map(|d| GpuInfo::Nvidia { devices: d }),
    #[cfg(unix)]
    GpuInfo::Amd { .. } => amd::probe_devices().map(|d| GpuInfo::Amd { devices: d }),
    #[cfg(windows)]
    GpuInfo::Amd { .. } => None,
    GpuInfo::Multi { devices } => {
      // Derive per-backend lists from the backend tags.
      let prev_nvidia: Vec<GpuDevice> = devices
        .iter()
        .filter(|d| d.backend == "nvidia")
        .cloned()
        .collect();
      let prev_amd: Vec<GpuDevice> = devices
        .iter()
        .filter(|d| d.backend == "amd")
        .cloned()
        .collect();
      let prev_metal: Vec<GpuDevice> = devices
        .iter()
        .filter(|d| d.backend == "apple_metal")
        .cloned()
        .collect();
      let prev_unknown: Vec<GpuDevice> = devices
        .iter()
        .filter(|d| d.backend == "unknown")
        .cloned()
        .collect();

      let mut changed = false;
      let mut next_nvidia = prev_nvidia.clone();
      let mut next_amd = prev_amd.clone();
      let next_metal = prev_metal.clone();
      let next_unknown = prev_unknown.clone();
      if !prev_nvidia.is_empty() {
        if let Some(devs) = nvidia::probe_devices() {
          if !devices_match(&prev_nvidia, &devs) {
            next_nvidia = devs;
            changed = true;
          }
        }
      }
      if !prev_amd.is_empty() {
        if let Some(devs) = amd::probe_devices() {
          if !devices_match(&prev_amd, &devs) {
            next_amd = devs;
            changed = true;
          }
        }
      }
      // Metal and Vulkan data are static — no per-tick refresh needed.
      if changed {
        let mut all = Vec::new();
        all.extend(next_nvidia);
        all.extend(next_amd);
        all.extend(next_metal);
        all.extend(next_unknown);
        Some(GpuInfo::Multi { devices: all })
      } else {
        None
      }
    }
  }
}

/// Compare two device lists by name + total_memory_bytes.
/// We can't use `==` because `GpuDevice` intentionally doesn't
/// derive `Eq` (NaN-f32 fields). This is sufficient for detecting
/// changes in the active backend.
fn devices_match(a: &[GpuDevice], b: &[GpuDevice]) -> bool {
  if a.len() != b.len() {
    return false;
  }
  for (da, db) in a.iter().zip(b.iter()) {
    if da.name != db.name {
      return false;
    }
    if da.total_memory_bytes != db.total_memory_bytes {
      return false;
    }
  }
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalize_pci_canonicalizes_every_vendor_format() {
    // nvml (8-char domain), rocm-smi (4-char domain), vulkaninfo
    // (already canonical) and lspci's domain-less short form must all
    // collapse to the same canonical string so cross-backend dedup
    // matches the same physical card.
    for raw in [
      "00000000:0F:00.0", // nvml
      "0000:0F:00.0",     // rocm-smi
      "00000000:0f:00.0", // vulkaninfo
      "0f:00.0",          // lspci short (no domain)
    ] {
      assert_eq!(
        normalize_pci(raw).as_deref(),
        Some("00000000:0f:00.0"),
        "input {raw:?}"
      );
    }
    // Distinct buses stay distinct.
    assert_ne!(normalize_pci("0e:00.0"), normalize_pci("0f:00.0"));
    // Garbage is rejected, not silently mangled.
    assert_eq!(normalize_pci("not-a-pci"), None);
    assert_eq!(normalize_pci(""), None);
  }

  #[test]
  fn parse_lspci_maps_vendor_device_to_canonical_address() {
    // `lspci -D -nn` shape: leading PCI address + trailing numeric
    // [vendor:device] bracket.
    let out = "\
0000:0f:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA102 [GeForce RTX 3080] [10de:2206] (rev a1)
0000:0e:00.0 Display controller [0380]: Advanced Micro Devices, Inc. [AMD/ATI] Navi 31 [1002:744c] (rev c8)
00:1f.3 Audio device [0403]: Intel Corporation Raptor Lake HD Audio [8086:7a50]
";
    let (_names, ids) = parse_lspci(out);
    // The GPU rows resolve vendor:device -> canonical PCI address, even
    // though the AMD line also carries an "[AMD/ATI]" name bracket
    // earlier (the right-to-left scan picks the numeric one).
    assert_eq!(
      ids.get("10de:2206").map(String::as_str),
      Some("00000000:0f:00.0")
    );
    assert_eq!(
      ids.get("1002:744c").map(String::as_str),
      Some("00000000:0e:00.0")
    );
    // The non-GPU audio line (no VGA/Display/3D) is ignored.
    assert!(!ids.contains_key("8086:7a50"));
  }

  #[test]
  fn normalize_card_name_strips_vulkan_driver_tag() {
    // rocm-smi vs vulkaninfo names for the same physical card must
    // normalize to the same key so the cross-backend dedup collapses
    // the 0-VRAM Vulkan duplicate.
    assert_eq!(
      normalize_card_name("AMD Radeon AI PRO R9700 (RADV GFX1201)"),
      normalize_card_name("AMD Radeon AI PRO R9700"),
    );
    // Distinct cards stay distinct.
    assert_ne!(
      normalize_card_name("NVIDIA GeForce RTX 3080"),
      normalize_card_name("AMD Radeon AI PRO R9700"),
    );
  }

  fn named_device(name: &str, backend: &str) -> GpuDevice {
    GpuDevice {
      name: name.into(),
      backend: backend.into(),
      ..Default::default()
    }
  }

  // ---- resolve_devices: hardware-combination coverage ----
  //
  // resolve_devices is the pure dedup + classification core extracted
  // from probe(). Exercising it across every backend combination is what
  // makes this logic regression-proof on hardware no CI runner has.

  fn gib(n: u64) -> u64 {
    n * 1024 * 1024 * 1024
  }
  fn dev(name: &str, backend: &str, total: u64, pci: Option<&str>, uma: Option<u64>) -> GpuDevice {
    GpuDevice {
      name: name.into(),
      backend: backend.into(),
      total_memory_bytes: total,
      device_id: pci.map(String::from),
      uma_shared_total_bytes: uma,
      ..Default::default()
    }
  }
  fn nv(name: &str, pci: &str) -> GpuDevice {
    dev(name, "nvidia", gib(24), Some(pci), None)
  }
  fn amd_dev(name: &str, pci: &str) -> GpuDevice {
    dev(name, "amd", gib(16), Some(pci), None)
  }
  fn vk(name: &str, id: &str) -> GpuDevice {
    dev(name, "unknown", 0, Some(id), None)
  }

  #[test]
  fn resolve_empty_is_cpu_only() {
    assert_eq!(
      resolve_devices(vec![], vec![], vec![], vec![], None),
      GpuInfo::CpuOnly
    );
  }

  #[test]
  fn resolve_single_backend_variants() {
    assert!(matches!(
      resolve_devices(vec![nv("RTX 4090", "00000000:01:00.0")], vec![], vec![], vec![], None),
      GpuInfo::Nvidia { devices } if devices.len() == 1
    ));
    assert!(matches!(
      resolve_devices(vec![], vec![amd_dev("RX 7900", "00000000:03:00.0")], vec![], vec![], None),
      GpuInfo::Amd { devices } if devices.len() == 1
    ));
    assert!(matches!(
      resolve_devices(vec![], vec![], vec![dev("Apple M3 Max", "metal", gib(96), None, None)], vec![], None),
      GpuInfo::AppleMetal { total_memory_bytes } if total_memory_bytes == gib(96)
    ));
    // Vulkan-only (Intel Arc, or AMD without rocm-smi) → Unknown.
    assert!(matches!(
      resolve_devices(vec![], vec![], vec![], vec![vk("Intel Arc A770", "0x8086:0x56a0")], None),
      GpuInfo::Unknown { devices } if devices.len() == 1
    ));
  }

  #[test]
  fn resolve_two_same_backend_cards_keep_their_variant() {
    // Dual-NVIDIA / CrossFire: one backend with several cards must NOT
    // collapse to `Multi` (which would route a dual-NVIDIA Linux box to
    // the Vulkan build instead of CUDA).
    assert!(matches!(
      resolve_devices(
        vec![nv("RTX 4090", "00000000:01:00.0"), nv("RTX 4090", "00000000:02:00.0")],
        vec![], vec![], vec![], None,
      ),
      GpuInfo::Nvidia { devices } if devices.len() == 2
    ));
    assert!(matches!(
      resolve_devices(
        vec![],
        vec![amd_dev("RX 7900", "00000000:03:00.0"), amd_dev("RX 7900", "00000000:04:00.0")],
        vec![], vec![], None,
      ),
      GpuInfo::Amd { devices } if devices.len() == 2
    ));
  }

  #[test]
  fn resolve_dedups_vulkan_duplicate_by_pci() {
    // Same NVIDIA card via nvidia-smi + Vulkan, both carrying the
    // canonical PCI address → one Nvidia device, not Multi.
    let r = resolve_devices(
      vec![nv("NVIDIA GeForce RTX 4090", "00000000:01:00.0")],
      vec![],
      vec![],
      vec![vk("NVIDIA GeForce RTX 4090", "00000000:01:00.0")],
      None,
    );
    assert!(
      matches!(r, GpuInfo::Nvidia { ref devices } if devices.len() == 1),
      "{r:?}"
    );
  }

  #[test]
  fn resolve_dedups_vulkan_duplicate_by_name_without_lspci() {
    // Windows / no-lspci path: the Vulkan vendor:device id can't resolve
    // to a bus address, so dedup falls back to the normalized name.
    let r = resolve_devices(
      vec![nv("NVIDIA GeForce RTX 4090", "00000000:01:00.0")],
      vec![],
      vec![],
      vec![vk("NVIDIA GeForce RTX 4090", "0x10de:0x2684")],
      None,
    );
    assert!(
      matches!(r, GpuInfo::Nvidia { ref devices } if devices.len() == 1),
      "{r:?}"
    );
  }

  #[test]
  fn resolve_amd_apu_collapses_vulkan_via_lspci_and_keeps_unified() {
    // The Strix Halo regression end-to-end: native AMD APU (unified, GTT
    // marked shared) + its Vulkan vendor:device duplicate, collapsed via
    // lspci → one Amd device that still carries the shared-pool size.
    let lspci = Some(parse_lspci(
      "0000:c4:00.0 Display controller [0380]: AMD Strix Halo [1002:1586]\n",
    ));
    let r = resolve_devices(
      vec![],
      vec![dev(
        "card0",
        "amd",
        gib(128),
        Some("00000000:c4:00.0"),
        Some(gib(124)),
      )],
      vec![],
      vec![vk(
        "AMD Radeon 8060S Graphics (RADV STRIX_HALO)",
        "0x1002:0x1586",
      )],
      lspci,
    );
    match r {
      GpuInfo::Amd { devices } => {
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].uma_shared_total_bytes, Some(gib(124)));
      }
      other => panic!("expected Amd, got {other:?}"),
    }
  }

  #[test]
  fn resolve_genuine_two_backends_is_multi() {
    // dGPU + iGPU (NVIDIA discrete + AMD APU) → Multi with both.
    let r = resolve_devices(
      vec![nv("NVIDIA GeForce RTX 4090", "00000000:01:00.0")],
      vec![dev(
        "card0",
        "amd",
        gib(96),
        Some("00000000:c4:00.0"),
        Some(gib(92)),
      )],
      vec![],
      vec![],
      None,
    );
    assert!(
      matches!(r, GpuInfo::Multi { ref devices } if devices.len() == 2),
      "{r:?}"
    );
  }

  #[test]
  fn resolve_multi_drops_dup_keeps_genuine_second_card() {
    // NVIDIA + two Vulkan devices: one is the NVIDIA card's duplicate
    // (same PCI), the other a genuine Intel iGPU → Multi with exactly
    // the NVIDIA + Intel, the duplicate dropped.
    let r = resolve_devices(
      vec![nv("NVIDIA GeForce RTX 4090", "00000000:01:00.0")],
      vec![],
      vec![],
      vec![
        vk("NVIDIA GeForce RTX 4090", "00000000:01:00.0"),
        vk("Intel Arc A770", "00000000:03:00.0"),
      ],
      None,
    );
    match r {
      GpuInfo::Multi { devices } => {
        assert_eq!(devices.len(), 2, "{devices:?}");
        assert!(devices.iter().any(|d| d.name.contains("Intel")));
      }
      other => panic!("expected Multi, got {other:?}"),
    }
  }

  #[test]
  fn resolve_metal_plus_moltenvk_duplicate_stays_apple() {
    // MoltenVK re-reports the Apple GPU through Vulkan; the name dedup
    // (vendor-prefix-stripped) collapses it so the host stays AppleMetal.
    let r = resolve_devices(
      vec![],
      vec![],
      vec![dev("Apple M3 Max", "metal", gib(96), None, None)],
      vec![vk("Apple M3 Max", "0x106b:0x1000")],
      None,
    );
    assert!(matches!(r, GpuInfo::AppleMetal { .. }), "{r:?}");
  }

  #[test]
  fn cross_probe_duplicate_collapses_driver_tagged_vulkan_name() {
    // Same card, bare native name vs "(RADV …)"-tagged Vulkan name, no
    // PCI id (the Windows case) — must still collapse via the name path.
    let native = named_device("AMD Radeon AI PRO R9700", "amd");
    let vulkan = named_device("AMD Radeon AI PRO R9700 (RADV GFX1201)", "unknown");
    assert!(is_cross_probe_duplicate(&vulkan, &[&native], &None));
  }

  #[test]
  fn cross_probe_duplicate_keeps_genuinely_distinct_cards() {
    let native = named_device("NVIDIA GeForce RTX 4090", "nvidia");
    let vulkan = named_device("AMD Radeon RX 7900 XT (RADV NAVI31)", "unknown");
    assert!(!is_cross_probe_duplicate(&vulkan, &[&native], &None));
  }

  #[test]
  fn cross_probe_duplicate_collapses_across_vendor_prefix_mismatch() {
    // Previously a known gap: rocm-smi drops the "AMD" prefix ("Radeon RX
    // 7900 XT") while Vulkan keeps it ("AMD Radeon RX 7900 XT (RADV
    // NAVI31)"), and with no PCI id the name fallback didn't collapse
    // them → phantom `Multi`. normalize_card_name now strips the leading
    // vendor token on both sides, so they dedup even without lspci.
    let native = named_device("Radeon RX 7900 XT", "amd");
    let vulkan = named_device("AMD Radeon RX 7900 XT (RADV NAVI31)", "unknown");
    assert!(is_cross_probe_duplicate(&vulkan, &[&native], &None));
  }

  #[test]
  fn cross_probe_duplicate_collapses_vulkan_vendor_device_via_lspci() {
    // Regression (Strix Halo): the native AMD probe tags the
    // canonical PCI address, while the Vulkan probe reports only a
    // "0xVVVV:0xDDDD" vendor:device id. lspci maps that id to the same
    // address, so they must dedup — the per-part "0x" strip is what
    // makes "0x1002:0x1586" match lspci's "1002:1586" key. Before the
    // fix the second part mangled to "x1586", the lookup missed, and the
    // single iGPU surfaced as a phantom `Multi`.
    let lspci = Some(parse_lspci(
      "0000:c4:00.0 Display controller [0380]: Advanced Micro Devices, Inc. [AMD/ATI] Strix Halo [1002:1586] (rev c1)\n",
    ));
    let native = GpuDevice {
      name: "card0".into(),
      backend: "amd".into(),
      device_id: Some("00000000:c4:00.0".into()),
      ..Default::default()
    };
    let vulkan = GpuDevice {
      name: "AMD Radeon 8060S Graphics (RADV STRIX_HALO)".into(),
      backend: "unknown".into(),
      device_id: Some("0x1002:0x1586".into()),
      ..Default::default()
    };
    assert!(is_cross_probe_duplicate(&vulkan, &[&native], &lspci));
  }

  #[test]
  fn cpu_only_is_not_gpu() {
    assert!(!GpuInfo::CpuOnly.is_gpu());
    assert_eq!(GpuInfo::CpuOnly.label(), "cpu_only");
  }

  #[test]
  fn nvidia_is_gpu() {
    let info = GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "RTX 4090".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        ..Default::default()
      }],
    };
    assert!(info.is_gpu());
    assert_eq!(info.label(), "nvidia");
  }

  #[test]
  fn json_carries_tag_field() {
    let v = GpuInfo::AppleMetal {
      total_memory_bytes: 64 * 1024 * 1024 * 1024,
    };
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("\"backend\":\"apple_metal\""));
    assert!(s.contains("\"total_memory_bytes\":"));
  }

  #[test]
  fn gpu_device_omits_optional_fields_when_absent() {
    let dev = GpuDevice {
      name: "RTX 4090".into(),
      total_memory_bytes: 24 * 1024 * 1024 * 1024,
      used_memory_bytes: 0,
      utilization_pct: None,
      temperature_c: None,
      ..Default::default()
    };
    let s = serde_json::to_string(&dev).unwrap();
    assert!(!s.contains("utilization_pct"));
    assert!(!s.contains("temperature_c"));
  }

  #[test]
  fn gpu_device_emits_optional_fields_when_present() {
    let dev = GpuDevice {
      name: "RTX 4090".into(),
      total_memory_bytes: 24 * 1024 * 1024 * 1024,
      used_memory_bytes: 12 * 1024 * 1024 * 1024,
      utilization_pct: Some(84.0),
      temperature_c: Some(68.0),
      ..Default::default()
    };
    let s = serde_json::to_string(&dev).unwrap();
    assert!(s.contains("\"utilization_pct\":84"));
    assert!(s.contains("\"temperature_c\":68"));
  }
}
