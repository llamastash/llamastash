//! Linux amdgpu probing via `/sys/class/drm`.
//!
//! Preferred over shelling out to `rocm-smi`: the sysfs nodes are a
//! stable kernel interface, present whenever the `amdgpu` driver is
//! bound, and don't break when a ROCm release renames a JSON key. The
//! `amd` probe falls back to `rocm-smi` only when sysfs yields nothing.
//!
//! `mem_info_vram_total` is the BIOS-dedicated VRAM heap (the carve-out
//! on APUs, the full board RAM on discrete). `mem_info_gtt_total` is
//! the system-RAM-backed GTT pool where an APU holds the real model
//! weights. No driver-level "integrated" flag exists, so a card is
//! unified by the carve-out signature, see
//! [`super::is_carve_signature`].

use std::fs;
use std::path::{Path, PathBuf};

use super::{classify_amd_memory, normalize_pci, GpuDevice};

/// Returns `None` when no amdgpu card is present, so the caller falls
/// through to `rocm-smi`. Even a single card with unreadable memory is
/// reported as a failure signal rather than dropped silently.
pub fn probe_devices() -> Option<Vec<GpuDevice>> {
  let cards = amdgpu_card_dirs();
  if cards.is_empty() {
    return None;
  }
  let mut out = Vec::new();
  for (idx, dir) in cards.iter().enumerate() {
    if let Some(dev) = read_card(dir, idx) {
      out.push(dev);
    }
  }
  if out.is_empty() {
    None
  } else {
    Some(out)
  }
}

/// `/sys/class/drm/cardN/device` directories bound to the `amdgpu`
/// driver, sorted by card index for stable ordering.
fn amdgpu_card_dirs() -> Vec<PathBuf> {
  let mut cards: Vec<(u32, PathBuf)> = Vec::new();
  let Ok(entries) = fs::read_dir("/sys/class/drm") else {
    return Vec::new();
  };
  for entry in entries.flatten() {
    let name = entry.file_name();
    let Some(name) = name.to_str() else { continue };
    // Match `cardN` exactly — reject `cardN-DP-1` connector nodes and
    // `renderD*` nodes.
    let Some(idx) = name
      .strip_prefix("card")
      .and_then(|n| n.parse::<u32>().ok())
    else {
      continue;
    };
    let device = entry.path().join("device");
    if driver_name(&device).as_deref() == Some("amdgpu") {
      cards.push((idx, device));
    }
  }
  cards.sort_by_key(|(idx, _)| *idx);
  cards.into_iter().map(|(_, dir)| dir).collect()
}

fn driver_name(device: &Path) -> Option<String> {
  fs::read_link(device.join("driver"))
    .ok()?
    .file_name()?
    .to_str()
    .map(|s| s.to_string())
}

/// `None` when the mandatory VRAM total node is unreadable: a card
/// that can't report its memory is no use as a budget source; the
/// caller's `out.is_empty()` check then routes to the rocm-smi fallback.
fn read_card(device: &Path, idx: usize) -> Option<GpuDevice> {
  let vram_total = read_u64(&device.join("mem_info_vram_total"))?;
  let vram_used = read_u64(&device.join("mem_info_vram_used")).unwrap_or(0);
  let gtt_total = read_u64(&device.join("mem_info_gtt_total"));
  let gtt_used = read_u64(&device.join("mem_info_gtt_used"));

  let (
    total_memory_bytes,
    used_memory_bytes,
    uma_shared_total_bytes,
    uma_shared_used_bytes,
    source,
  ) = classify_amd_memory(
    vram_total,
    vram_used,
    gtt_total,
    gtt_used,
    is_integrated_pci_class(device),
  );

  Some(GpuDevice {
    name: format!("card{idx}"),
    backend: "amd".into(),
    total_memory_bytes,
    used_memory_bytes,
    utilization_pct: read_u64(&device.join("gpu_busy_percent")).map(|p| p as f32),
    temperature_c: read_hwmon_temp_c(device),
    device_id: read_pci_slot(device),
    uma_shared_total_bytes,
    uma_shared_used_bytes,
    classification_source: Some(source),
  })
}

/// `true` when the card's PCI class is "Display controller, other"
/// (`0x0380xx`). Integrated GPUs (Strix Halo et al.) enumerate this way;
/// discrete cards are "VGA compatible controller" (`0x0300xx`). This is
/// a *sufficient* integrated signal — no discrete GPU uses `0x0380`, so
/// it never misclassifies a discrete card as unified. APUs that report
/// `0x0300` fall back to the VRAM carve-out size heuristic.
fn is_integrated_pci_class(device: &Path) -> bool {
  fs::read_to_string(device.join("class"))
    .ok()
    .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
    .map(|class| class >> 8 == 0x0380)
    .unwrap_or(false)
}

fn read_u64(path: &Path) -> Option<u64> {
  fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()
}

/// `temp1_input` from the first hwmon dir that has it (millidegrees
/// C → C). amdgpu exposes edge/junction/memory sensors; the edge
/// sensor is `temp1` and is what `rocm-smi` reports, so taking the
/// first readable input matches the existing display.
fn read_hwmon_temp_c(device: &Path) -> Option<f32> {
  let hwmon_root = device.join("hwmon");
  let entries = fs::read_dir(&hwmon_root).ok()?;
  for entry in entries.flatten() {
    if let Some(milli) = read_u64(&entry.path().join("temp1_input")) {
      return Some(milli as f32 / 1000.0);
    }
  }
  None
}

/// Canonical PCI address from the card's `uevent` `PCI_SLOT_NAME`, for
/// cross-backend dedup (matches the rocm-smi / vulkaninfo form).
fn read_pci_slot(device: &Path) -> Option<String> {
  let uevent = fs::read_to_string(device.join("uevent")).ok()?;
  for line in uevent.lines() {
    if let Some(slot) = line.strip_prefix("PCI_SLOT_NAME=") {
      return normalize_pci(slot.trim());
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn classify_strix_halo_sums_and_marks_shared() {
    // Reference box: 512 MiB BIOS carve + ~124 GiB GTT → unified, sum,
    // GTT marked shared, source = carve signature.
    let (total, used, shared, shared_used, source) = classify_amd_memory(
      536_870_912,
      486_838_272,
      Some(133_143_986_176),
      Some(2_843_930_624),
      false,
    );
    assert_eq!(total, 536_870_912 + 133_143_986_176);
    assert_eq!(used, 486_838_272 + 2_843_930_624);
    assert_eq!(shared, Some(133_143_986_176));
    assert_eq!(shared_used, Some(2_843_930_624));
    assert_eq!(source, crate::gpu::ClassSource::CarveSignature);
  }

  #[test]
  fn read_u64_trims_newline() {
    let dir = std::env::temp_dir().join(format!("llamastash-sysfs-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let f = dir.join("val");
    fs::write(&f, "133143986176\n").unwrap();
    assert_eq!(read_u64(&f), Some(133_143_986_176));
    fs::write(&f, "not-a-number").unwrap();
    assert_eq!(read_u64(&f), None);
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn integrated_pci_class_detects_display_controller() {
    let dir = std::env::temp_dir().join(format!("llamastash-class-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    // 0x0380xx (Display controller, other) → integrated (Strix Halo).
    fs::write(dir.join("class"), "0x038000\n").unwrap();
    assert!(is_integrated_pci_class(&dir));
    // 0x0300xx (VGA compatible) → not a sufficient integrated signal.
    fs::write(dir.join("class"), "0x030000\n").unwrap();
    assert!(!is_integrated_pci_class(&dir));
    fs::remove_dir_all(&dir).ok();
  }
}
