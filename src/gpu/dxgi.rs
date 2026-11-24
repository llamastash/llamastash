//! DXGI-based GPU detection for Windows.
//!
//! Fills the Windows AMD gap that the Linux-only `rocm-smi` probe
//! leaves open — and as a bonus also covers Intel iGPUs and the rare
//! NVIDIA-without-nvidia-smi.exe stripped-install case.
//!
//! Wraps `CreateDXGIFactory1` → `IDXGIFactory1::EnumAdapters1` →
//! `IDXGIAdapter1::GetDesc1`. Reports per-adapter:
//!  - Adapter name (`Description`, UTF-16 → `String`)
//!  - Dedicated VRAM (`DedicatedVideoMemory`)
//!  - Unified vs discrete via the D3D12 `UMA` architecture flag (see
//!    `adapter_is_uma`). Only genuine UMA adapters fold their shared
//!    system-RAM pool into the VRAM total and mark it as
//!    `uma_shared_total_bytes`. `SharedSystemMemory` from the DXGI
//!    desc is NOT a UMA signal — it's ~half system RAM on every
//!    adapter, discrete cards included.
//!  - Vendor classification by `VendorId` (0x1002 AMD, 0x10DE NVIDIA,
//!    0x8086 Intel)
//!
//! Filters out software adapters (`DXGI_ADAPTER_FLAG_SOFTWARE`) like
//! Microsoft Basic Render Driver and llvmpipe so the host pane shows
//! actual hardware.
//!
//! What it does NOT give you (DXGI limitations, not bugs):
//!  - Live VRAM-used numbers. DXGI only exposes static description
//!    fields. The `Process` / `Local` / `NonLocal` budgets via
//!    `IDXGIAdapter3::QueryVideoMemoryInfo` could surface this per-
//!    *process* (not per-supervised-child), but the Linux backends
//!    don't either today.
//!  - GPU utilization% / temperature. Use NVML (NVIDIA), ADLX (AMD),
//!    or Intel's IGCL for live metrics.
//!  - Per-PID VRAM attribution. Same reason — DXGI is adapter-level.
//!
//! The host pane renders `—` for util/temp on a DXGI-sourced backend,
//! matching how Apple Metal currently degrades.

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::{
  D3D12CreateDevice, ID3D12Device, D3D12_FEATURE_ARCHITECTURE, D3D12_FEATURE_DATA_ARCHITECTURE,
};
use windows::Win32::Graphics::Dxgi::{
  CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};

use super::{ClassSource, GpuDevice};

const VENDOR_AMD: u32 = 0x1002;
const VENDOR_NVIDIA: u32 = 0x10DE;
const VENDOR_INTEL: u32 = 0x8086;
/// "Microsoft Basic Render Driver" — software fallback adapter that
/// shows up on Server SKUs and inside VMs without GPU pass-through.
/// Skipped even when the `DXGI_ADAPTER_FLAG_SOFTWARE` bit isn't set
/// because some driver builds advertise it as hardware.
const VENDOR_MS_BASIC_RENDER: u32 = 0x1414;

/// Classification of a single adapter's `VendorId`. Only the vendors
/// we have a `GpuInfo` variant for get distinct values; everything
/// else lands in `Other` and contributes to `GpuInfo::Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vendor {
  Amd,
  Nvidia,
  Intel,
  /// Recognised but no dedicated `GpuInfo` variant — surfaces as
  /// `GpuInfo::Unknown` so the TUI says `backend unknown` rather than
  /// mis-labelling the card.
  Other,
}

pub(crate) fn vendor_from_id(id: u32) -> Vendor {
  match id {
    VENDOR_AMD => Vendor::Amd,
    VENDOR_NVIDIA => Vendor::Nvidia,
    VENDOR_INTEL => Vendor::Intel,
    _ => Vendor::Other,
  }
}

/// Parse the fixed 128-wide-char `Description` field into a String.
/// Stops at the first NUL; falls back to lossy decoding for the rare
/// invalid-surrogate case. Trims whitespace because some driver
/// builds right-pad the field with spaces.
pub(crate) fn description_to_string(desc: &[u16; 128]) -> String {
  let end = desc.iter().position(|&c| c == 0).unwrap_or(desc.len());
  String::from_utf16_lossy(&desc[..end]).trim().to_string()
}

/// Run the DXGI enumeration and return discovered devices.
/// Returns `None` if `CreateDXGIFactory1` fails (no DXGI runtime —
/// exotic Windows configurations) or if every enumerated adapter is
/// software / Microsoft Basic Render. The probe chain in `gpu::mod`
/// falls through to `vulkan::probe` in that case.
pub fn probe_devices() -> Option<Vec<GpuDevice>> {
  // SAFETY: `CreateDXGIFactory1` is a documented stdcall entry point
  // available since Windows 7. Returning `Err` is the documented
  // failure mode for missing DXGI runtime; we propagate via `ok()?`.
  let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
    Ok(f) => f,
    Err(e) => {
      log::debug!("dxgi probe: CreateDXGIFactory1 failed: {e}");
      return None;
    }
  };

  let mut adapters: Vec<(Vendor, GpuDevice)> = Vec::new();
  for idx in 0u32..32 {
    // SAFETY: `EnumAdapters1` is documented to return DXGI_ERROR_NOT_FOUND
    // when `idx` is past the last adapter — we break on any Err. The
    // outer `0..32` cap is a sanity bound; real machines have <16
    // adapters in any configuration.
    let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(idx) } {
      Ok(a) => a,
      Err(_) => break,
    };
    // SAFETY: `IDXGIAdapter1` is a live COM interface; `GetDesc1`
    // returns a `Result<DXGI_ADAPTER_DESC1>` (plain-old-data) per the
    // windows-rs binding. Documented failure is `DXGI_ERROR_*`.
    let desc = match unsafe { adapter.GetDesc1() } {
      Ok(d) => d,
      Err(e) => {
        log::debug!("dxgi probe: adapter {idx} GetDesc1 failed: {e}");
        continue;
      }
    };
    if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
      continue;
    }
    if desc.VendorId == VENDOR_MS_BASIC_RENDER {
      continue;
    }
    let vendor = vendor_from_id(desc.VendorId);
    let dedicated = desc.DedicatedVideoMemory as u64;
    let shared = desc.SharedSystemMemory as u64;
    // Genuine unified-memory APUs (Strix Halo / Phoenix / integrated
    // Intel/AMD) vs discrete cards can only be told apart by the D3D12
    // `UMA` architecture flag — `SharedSystemMemory` is ~half system
    // RAM on *every* adapter, so it can't carry the signal (the old
    // `dedicated < shared` heuristic mis-flagged discrete cards and
    // made the host pane subtract bogus bytes from the RAM gauge).
    let (total_memory_bytes, uma_shared_total, class_source) = if adapter_is_uma(&adapter) {
      // The usable GPU pool is the small BIOS-carved dedicated heap
      // plus the shareable system-RAM pool that holds the actual model
      // weights. Fold them into one total (mirrors the Linux rocm-smi
      // VRAM+GTT path) and mark the shared portion so init / the host
      // pane flag it as unified. The D3D12 `UMA` flag is the explicit,
      // authoritative classification signal.
      (
        dedicated.saturating_add(shared),
        Some(shared),
        ClassSource::ExplicitDxgiUma,
      )
    } else {
      // Discrete card: dedicated VRAM is the real number; the shared
      // pool is just the driver's GART aperture and must not inflate
      // VRAM or be mistaken for unified memory.
      (dedicated, None, ClassSource::Discrete)
    };
    // DXGI gives no backend label, so the vendor id picks it. Intel and
    // unrecognised vendors have no `GpuInfo` variant of their own, so
    // they report as `unknown`.
    let backend = match vendor {
      Vendor::Nvidia => "nvidia",
      Vendor::Amd => "amd",
      Vendor::Intel => "unknown",
      Vendor::Other => "unknown",
    };
    adapters.push((
      vendor,
      GpuDevice {
        name: description_to_string(&desc.Description),
        backend: backend.into(),
        total_memory_bytes,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        // DXGI exposes no PCI bus address; cross-backend dedup on
        // Windows falls back to the adapter name.
        device_id: None,
        uma_shared_total_bytes: uma_shared_total,
        uma_shared_used_bytes: None,
        classification_source: Some(class_source),
      },
    ));
  }
  // No hardware adapter survived filtering — let the probe chain fall
  // through to the Vulkan fallback rather than reporting an empty set.
  let devices = classify_devices(adapters);
  if devices.is_empty() {
    None
  } else {
    Some(devices)
  }
}

/// Query the authoritative D3D12 `UMA` architecture flag for an
/// adapter. `true` means the GPU shares one physical memory pool with
/// the CPU (integrated / APU); `false` is a discrete card with its own
/// VRAM. Best-effort: any failure (no D3D12 runtime, device creation
/// refused, feature query unsupported) returns `false` — the safe
/// default, since mis-flagging a discrete card as unified is exactly
/// the bug this replaced.
fn adapter_is_uma(adapter: &IDXGIAdapter1) -> bool {
  // SAFETY: `D3D12CreateDevice` is a documented entry point; we pass a
  // live COM adapter and an out-pointer it fills with `Some(device)`
  // on success. We treat any `Err` as "not UMA" rather than probing
  // further.
  let mut device: Option<ID3D12Device> = None;
  if unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, &mut device) }.is_err() {
    return false;
  }
  let Some(device) = device else {
    return false;
  };
  let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE::default();
  // SAFETY: `CheckFeatureSupport` writes the feature struct in place;
  // the pointer/size pair must describe `D3D12_FEATURE_DATA_ARCHITECTURE`
  // for `D3D12_FEATURE_ARCHITECTURE`. `NodeIndex` defaults to 0 (the
  // first/only node on single-GPU machines), which is what we want.
  let queried = unsafe {
    device.CheckFeatureSupport(
      D3D12_FEATURE_ARCHITECTURE,
      &mut arch as *mut _ as *mut core::ffi::c_void,
      std::mem::size_of::<D3D12_FEATURE_DATA_ARCHITECTURE>() as u32,
    )
  };
  queried.is_ok() && arch.UMA.as_bool()
}

/// Return the raw device list. Every DXGI adapter becomes a device
/// carrying the per-vendor backend tag that `probe_devices` set; the
/// multi-backend probe in `crate::gpu` groups and labels them.
pub(crate) fn classify_devices(adapters: Vec<(Vendor, GpuDevice)>) -> Vec<GpuDevice> {
  adapters.into_iter().map(|(_, d)| d).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn vendor_id_maps_known_ids() {
    assert_eq!(vendor_from_id(0x1002), Vendor::Amd);
    assert_eq!(vendor_from_id(0x10DE), Vendor::Nvidia);
    assert_eq!(vendor_from_id(0x8086), Vendor::Intel);
    assert_eq!(vendor_from_id(0xDEAD_BEEF), Vendor::Other);
  }

  #[test]
  fn description_to_string_trims_nul_terminator() {
    let mut buf = [0u16; 128];
    for (i, c) in "RTX 4090\0junkjunk".encode_utf16().enumerate() {
      buf[i] = c;
    }
    assert_eq!(description_to_string(&buf), "RTX 4090");
  }

  #[test]
  fn description_to_string_handles_full_buffer() {
    // No NUL anywhere — the loop should fall through to `desc.len()`
    // and decode the whole buffer rather than panic.
    let buf = [b'A' as u16; 128];
    let got = description_to_string(&buf);
    assert_eq!(got.len(), 128);
    assert!(got.chars().all(|c| c == 'A'));
  }

  #[test]
  fn description_to_string_strips_trailing_padding() {
    let mut buf = [0u16; 128];
    for (i, c) in "AMD Radeon RX 7900 XTX            "
      .encode_utf16()
      .enumerate()
    {
      buf[i] = c;
    }
    assert_eq!(description_to_string(&buf), "AMD Radeon RX 7900 XTX");
  }
}
