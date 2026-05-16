//! Apple Silicon Metal probe via `system_profiler
//! SPDisplaysDataType -json`.
//!
//! Metal uses *unified* memory, so we report the system RAM
//! exposed to the GPU (effectively all of it on Apple Silicon) as
//! `total_memory_bytes`. The JSON shape: top-level key
//! `SPDisplaysDataType` is an array of GPU records; each has
//! `spdisplays_mtlgpufamilysupport` (a string like "Apple9") on
//! Apple Silicon. We use that as the gate — Intel Macs (where
//! `system_profiler` reports a discrete card or none) skip this
//! backend.

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(any(target_os = "macos", test))]
use serde_json::Value;

use super::GpuInfo;

#[cfg(target_os = "macos")]
pub fn probe() -> Option<GpuInfo> {
  let mut cmd = Command::new("system_profiler");
  cmd.args(["SPDisplaysDataType", "-json"]);
  let output = super::run_with_timeout(cmd)?;
  if !output.status.success() {
    return None;
  }
  let stdout = String::from_utf8(output.stdout).ok()?;
  let total_memory_bytes = parse(&stdout)?;
  Some(GpuInfo::AppleMetal { total_memory_bytes })
}

#[cfg(not(target_os = "macos"))]
pub fn probe() -> Option<GpuInfo> {
  None
}

/// Extract Apple Silicon system RAM from `system_profiler
/// SPDisplaysDataType -json`. Returns `None` on Intel Macs or
/// malformed JSON. Cross-compile-only on macOS (no Linux caller).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse(stdout: &str) -> Option<u64> {
  let v: Value = serde_json::from_str(stdout).ok()?;
  let displays = v.get("SPDisplaysDataType")?.as_array()?;
  for gpu in displays {
    let Some(obj) = gpu.as_object() else { continue };
    let family = obj
      .get("spdisplays_mtlgpufamilysupport")
      .and_then(Value::as_str)
      .unwrap_or("");
    if !family.starts_with("Apple") {
      continue;
    }
    // The JSON reports memory as a string like "16 GB" — convert
    // to bytes. Fall back to `sysinfo` total RAM if the field is
    // absent or unparseable.
    if let Some(raw) = obj
      .get("spdisplays_vram_shared")
      .or_else(|| obj.get("spdisplays_vram"))
      .and_then(Value::as_str)
    {
      if let Some(bytes) = parse_memory_string(raw) {
        return Some(bytes);
      }
    }
    return Some(system_total_memory());
  }
  None
}

#[cfg(any(target_os = "macos", test))]
fn parse_memory_string(raw: &str) -> Option<u64> {
  // Expected forms: "16 GB", "8 GB", "8192 MB", "65536 MB".
  let parts: Vec<&str> = raw.split_whitespace().collect();
  if parts.len() != 2 {
    return None;
  }
  let n: u64 = parts[0].parse().ok()?;
  let multiplier: u64 = match parts[1] {
    "GB" => 1024 * 1024 * 1024,
    "MB" => 1024 * 1024,
    "KB" => 1024,
    _ => return None,
  };
  Some(n.saturating_mul(multiplier))
}

#[cfg(any(target_os = "macos", test))]
fn system_total_memory() -> u64 {
  use sysinfo::System;
  let mut sys = System::new();
  sys.refresh_memory();
  sys.total_memory()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_apple_silicon_with_explicit_shared_vram() {
    let stdout = r#"{
      "SPDisplaysDataType": [
        {
          "spdisplays_mtlgpufamilysupport": "Apple9",
          "spdisplays_vram_shared": "16 GB"
        }
      ]
    }"#;
    assert_eq!(parse(stdout), Some(16 * 1024 * 1024 * 1024));
  }

  #[test]
  fn skips_intel_macs() {
    let stdout = r#"{
      "SPDisplaysDataType": [
        {
          "spdisplays_mtlgpufamilysupport": "Family3"
        }
      ]
    }"#;
    assert_eq!(parse(stdout), None);
  }

  #[test]
  fn malformed_memory_string_falls_back_or_skips() {
    assert!(parse_memory_string("16 PB").is_none());
    assert!(parse_memory_string("garbage").is_none());
    assert_eq!(parse_memory_string("8 GB"), Some(8u64 * 1024 * 1024 * 1024));
    assert_eq!(parse_memory_string("8192 MB"), Some(8192u64 * 1024 * 1024));
  }

  #[test]
  fn missing_displays_returns_none() {
    assert_eq!(parse(r#"{"other": []}"#), None);
    assert_eq!(parse(r#"{"SPDisplaysDataType": []}"#), None);
  }
}
