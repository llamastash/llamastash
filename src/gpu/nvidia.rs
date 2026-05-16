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

use std::process::Command;

use super::{run_with_timeout, GpuDevice, GpuInfo};

/// Run `nvidia-smi`. Returns `None` if the binary isn't on `$PATH`,
/// its exit status is non-zero (no NVIDIA driver loaded), or the
/// invocation exceeds the per-probe wall-clock deadline.
pub fn probe() -> Option<GpuInfo> {
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
  let devices = parse(&stdout);
  if devices.is_empty() {
    return None;
  }
  Some(GpuInfo::Nvidia { devices })
}

/// Parse the `--format=csv,noheader,nounits` output. Exposed so unit
/// tests can pin the format without spawning a subprocess.
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
    let total_mib: u64 = match parts[1].parse() {
      Ok(v) => v,
      Err(_) => continue,
    };
    let used_mib: u64 = parts[2].parse().unwrap_or(0);
    let utilization_pct = parts.get(3).and_then(|s| parse_optional_f32(s));
    let temperature_c = parts.get(4).and_then(|s| parse_optional_f32(s));
    out.push(GpuDevice {
      name,
      total_memory_bytes: total_mib.saturating_mul(1024 * 1024),
      used_memory_bytes: used_mib.saturating_mul(1024 * 1024),
      utilization_pct,
      temperature_c,
    });
  }
  out
}

/// Parse a `nvidia-smi` numeric field. Returns `None` for unsupported
/// columns (`[Not Supported]`, `[N/A]`, empty), so older drivers
/// missing `utilization.gpu` / `temperature.gpu` don't break the
/// reading.
fn parse_optional_f32(raw: &str) -> Option<f32> {
  let trimmed = raw.trim();
  if trimmed.is_empty() || trimmed.starts_with('[') {
    return None;
  }
  trimmed.parse::<f32>().ok()
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
  fn malformed_rows_are_skipped() {
    let stdout = "bad row only\nNVIDIA RTX, 8192, 100, 50, 60\nnoise, also bad\n";
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "NVIDIA RTX");
    assert_eq!(devices[0].utilization_pct, Some(50.0));
  }
}
