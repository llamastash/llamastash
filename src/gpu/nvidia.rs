//! NVIDIA GPU probe via `nvidia-smi`.
//!
//! Querying CSV output keeps the parser tiny and stable across driver
//! versions. We request five columns:
//!
//!   `name,memory.total,memory.used,utilization.gpu,temperature.gpu`
//!
//! Older drivers that don't expose `utilization.gpu` or
//! `temperature.gpu` emit `[Not Supported]` or `[N/A]` in those
//! columns — the parser tolerates that by storing `None` for the
//! affected field rather than skipping the row.
//!
//! Coherent-UMA parts (GB10 / Grace Blackwell, Jetson) mark the
//! *memory* columns the same way: CPU and GPU share one pool, so there
//! is no framebuffer to report. Those rows are kept and their memory
//! filled from the system pool instead.

use std::process::Command;

use sysinfo::{MemoryRefreshKind, RefreshKind, System};

use super::{run_with_timeout, ClassSource, GpuDevice};

/// Run `nvidia-smi`. Returns `None` if the binary isn't on `$PATH`,
/// its exit status is non-zero (no NVIDIA driver loaded), or the
/// invocation exceeds the per-probe wall-clock deadline.
pub fn probe_devices() -> Option<Vec<GpuDevice>> {
  let mut cmd = Command::new("nvidia-smi");
  cmd.args([
    "--query-gpu=name,memory.total,memory.used,utilization.gpu,temperature.gpu",
    "--format=csv,noheader,nounits",
  ]);
  let output = run_with_timeout(cmd)?;
  if !output.status.success() {
    return None;
  }
  let stdout = String::from_utf8(output.stdout).ok()?;
  let mut devices = parse(&stdout);
  if devices.is_empty() {
    return None;
  }
  fill_unified_memory(&mut devices);
  // Also grab PCI bus IDs — needed for deduplication across backends.
  let mut pci_cmd = Command::new("nvidia-smi");
  pci_cmd.args(["--query-gpu=pci.bus_id", "--format=csv,noheader"]);
  let pci_ids = run_with_timeout(pci_cmd)
    .filter(|o| o.status.success())
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .map(|s| parse_pci_ids(&s))
    .unwrap_or_default();
  Some(
    devices
      .into_iter()
      .enumerate()
      .map(|(i, mut d)| {
        d.device_id = pci_ids.get(i).cloned();
        d
      })
      .collect(),
  )
}

/// Parse `nvidia-smi --query-gpu=pci.bus_id --format=csv,noheader`
/// output. Each row is a PCI bus address like `00000000:0F:00.0`.
/// Normalized to canonical `00000000:0f:00.0` (8-char, lowercase).
fn parse_pci_ids(stdout: &str) -> Vec<String> {
  stdout
    .lines()
    .filter_map(|l| crate::gpu::normalize_pci(l.trim()))
    .collect()
}

/// Parse the `--format=csv,noheader,nounits` output. Exposed so unit
/// tests can pin the format without spawning a subprocess.
/// Each device is tagged with the "nvidia" backend.
pub(crate) fn parse(stdout: &str) -> Vec<GpuDevice> {
  let mut out = Vec::new();
  for line in stdout.lines() {
    let trimmed = line.trim();
    if trimmed.is_empty() {
      continue;
    }
    let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
    if parts.len() < 3 {
      continue;
    }
    let name = parts[0].to_string();
    // A coherent-UMA part (GB10 / Grace Blackwell, Jetson) reports the
    // framebuffer columns as unsupported — there is no dedicated VRAM to
    // size. Keep the row and let the caller fill memory from the system
    // pool; dropping it hides a GPU whose other columns read fine.
    let unified = is_unsupported_marker(parts[1]);
    let total_mib: u64 = match parts[1].parse() {
      Ok(v) => v,
      Err(_) if unified => 0,
      Err(_) => continue,
    };
    let used_mib: u64 = parts[2].parse().unwrap_or(0);
    let utilization_pct = parts.get(3).and_then(|s| parse_optional_f32(s));
    let temperature_c = parts.get(4).and_then(|s| parse_optional_f32(s));
    out.push(GpuDevice {
      name,
      backend: "nvidia".into(),
      total_memory_bytes: total_mib.saturating_mul(1024 * 1024),
      used_memory_bytes: used_mib.saturating_mul(1024 * 1024),
      utilization_pct,
      temperature_c,
      classification_source: unified.then_some(ClassSource::NvmlNoFramebuffer),
      ..Default::default()
    });
  }
  out
}

/// `nvidia-smi` renders an unavailable column as a bracketed marker
/// (`[N/A]`, `[Not Supported]`) rather than omitting it.
fn is_unsupported_marker(raw: &str) -> bool {
  raw.trim().starts_with('[')
}

/// Fill memory for coherent-UMA devices from the system pool, which is
/// the pool they actually allocate from.
///
/// Uses *available* rather than *free* memory: on a shared pool the page
/// cache is reclaimable and the gap between the two routinely runs to tens
/// of GiB, so `free` would understate what a model can use. The whole pool
/// is system RAM, so it is also recorded as the UMA-shared portion — that
/// is what stops the host pane counting the same bytes twice.
fn fill_unified_memory(devices: &mut [GpuDevice]) {
  if !devices
    .iter()
    .any(|d| d.classification_source == Some(ClassSource::NvmlNoFramebuffer))
  {
    return;
  }
  let sys = System::new_with_specifics(
    RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
  );
  let total = sys.total_memory();
  let used = total.saturating_sub(sys.available_memory());
  for device in devices
    .iter_mut()
    .filter(|d| d.classification_source == Some(ClassSource::NvmlNoFramebuffer))
  {
    device.total_memory_bytes = total;
    device.used_memory_bytes = used;
    device.uma_shared_total_bytes = Some(total);
    device.uma_shared_used_bytes = Some(used);
  }
}

/// Parse a `nvidia-smi` numeric field. Returns `None` for unsupported
/// columns (`[Not Supported]`, `[N/A]`, empty), so older drivers
/// missing `utilization.gpu` / `temperature.gpu` don't break the
/// reading. Robust to trailing unit suffixes (`84 %`, `68 C`, `°C`)
/// that some driver builds emit despite the `--format=csv,nounits`
/// flag — strips anything after the leading numeric run.
fn parse_optional_f32(raw: &str) -> Option<f32> {
  let trimmed = raw.trim();
  if trimmed.is_empty() || trimmed.starts_with('[') {
    return None;
  }
  // Find the first non-numeric character (allowing one '.' and a
  // leading '-') and parse up to it. This tolerates "84 %", "84%",
  // "68.0 C", "68°C", etc., while still rejecting pure garbage.
  let mut seen_dot = false;
  let end = trimmed
    .char_indices()
    .find(|(i, c)| {
      if c.is_ascii_digit() {
        return false;
      }
      if *c == '-' && *i == 0 {
        return false;
      }
      if *c == '.' && !seen_dot {
        seen_dot = true;
        return false;
      }
      true
    })
    .map(|(i, _)| i)
    .unwrap_or(trimmed.len());
  let numeric = &trimmed[..end];
  if numeric.is_empty() {
    return None;
  }
  numeric.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_canonical_five_column_csv() {
    let stdout =
      "NVIDIA GeForce RTX 4090, 24564, 312, 84, 68\nNVIDIA GeForce RTX 4080, 16376, 0, 12, 42\n";
    let devices = parse(stdout);
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].name, "NVIDIA GeForce RTX 4090");
    assert_eq!(devices[0].total_memory_bytes, 24564 * 1024 * 1024);
    assert_eq!(devices[0].used_memory_bytes, 312 * 1024 * 1024);
    assert_eq!(devices[0].utilization_pct, Some(84.0));
    assert_eq!(devices[0].temperature_c, Some(68.0));
    assert_eq!(devices[1].total_memory_bytes, 16376 * 1024 * 1024);
    assert_eq!(devices[1].utilization_pct, Some(12.0));
    assert_eq!(devices[1].temperature_c, Some(42.0));
  }

  #[test]
  fn parses_legacy_three_column_csv_without_util_or_temp() {
    // Older driver versions or `--query-gpu=name,memory.total,memory.used`
    // output land with `parts.len() == 3` — utilization/temperature
    // must surface as `None` rather than fail the whole reading.
    let stdout = "NVIDIA RTX, 8192, 100\n";
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "NVIDIA RTX");
    assert_eq!(devices[0].utilization_pct, None);
    assert_eq!(devices[0].temperature_c, None);
  }

  #[test]
  fn unsupported_marker_falls_back_to_none() {
    // Some driver builds return `[Not Supported]` or `[N/A]` for
    // util/temp fields — keep the row but mark the field absent.
    let stdout = "NVIDIA RTX, 8192, 100, [Not Supported], [N/A]\n";
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].utilization_pct, None);
    assert_eq!(devices[0].temperature_c, None);
  }

  #[test]
  fn empty_stdout_yields_no_devices() {
    assert!(parse("").is_empty());
    assert!(parse("\n   \n").is_empty());
  }

  #[test]
  fn strips_trailing_unit_suffixes_from_optional_columns() {
    // Some driver builds emit `--format=csv,nounits` rows that still
    // carry a `%` or `C` suffix (or `°C`) on the util/temp columns.
    // The parser should pick up the leading numeric run regardless.
    let stdout = "RTX 4090, 24564, 312, 84 %, 68°C\nRTX 3090, 24564, 312, 50%, 60 C\n";
    let devices = parse(stdout);
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].utilization_pct, Some(84.0));
    assert_eq!(devices[0].temperature_c, Some(68.0));
    assert_eq!(devices[1].utilization_pct, Some(50.0));
    assert_eq!(devices[1].temperature_c, Some(60.0));
  }

  #[test]
  fn malformed_rows_are_skipped() {
    let stdout = "bad row only\nNVIDIA RTX, 8192, 100, 50, 60\nnoise, also bad\n";
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "NVIDIA RTX");
    assert_eq!(devices[0].utilization_pct, Some(50.0));
  }

  #[test]
  fn coherent_uma_memory_marker_keeps_the_device() {
    // Real GB10 / DGX Spark output: no framebuffer, so both memory
    // columns are `[N/A]` while util/temp read fine. Dropping the row
    // here is what made the GPU invisible entirely.
    let stdout = "NVIDIA GB10, [N/A], [N/A], 0, 39\n";
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "NVIDIA GB10");
    assert_eq!(
      devices[0].classification_source,
      Some(ClassSource::NvmlNoFramebuffer)
    );
    assert_eq!(devices[0].utilization_pct, Some(0.0));
    assert_eq!(devices[0].temperature_c, Some(39.0));
  }

  #[test]
  fn discrete_cards_are_left_unclassified() {
    let devices = parse("NVIDIA GeForce RTX 4090, 24564, 312, 84, 68\n");
    assert_eq!(devices[0].classification_source, None);
    assert_eq!(devices[0].uma_shared_total_bytes, None);
  }

  #[test]
  fn non_marker_garbage_in_the_memory_column_still_drops_the_row() {
    // Only the bracketed markers mean "unsupported". Genuine junk is
    // still a malformed row and must not be promoted to a UMA device.
    assert!(parse("NVIDIA RTX, banana, 100, 50, 60\n").is_empty());
  }

  #[test]
  fn unified_fill_populates_from_the_system_pool() {
    let mut devices = parse("NVIDIA GB10, [N/A], [N/A], 0, 39\n");
    fill_unified_memory(&mut devices);
    // Exact figures are host-dependent; the invariants are not.
    assert!(devices[0].total_memory_bytes > 0);
    assert_eq!(
      devices[0].uma_shared_total_bytes,
      Some(devices[0].total_memory_bytes)
    );
    assert!(devices[0].used_memory_bytes <= devices[0].total_memory_bytes);
  }

  #[test]
  fn unified_fill_leaves_discrete_cards_alone() {
    let mut devices = parse("NVIDIA GeForce RTX 4090, 24564, 312, 84, 68\n");
    fill_unified_memory(&mut devices);
    assert_eq!(devices[0].total_memory_bytes, 24564 * 1024 * 1024);
    assert_eq!(devices[0].uma_shared_total_bytes, None);
  }
}
