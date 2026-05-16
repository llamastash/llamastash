//! AMD GPU probe via `rocm-smi`.
//!
//! v1 baseline: shell out to `rocm-smi` with three flags combined —
//! `--showmeminfo vram --showuse --showtemp --json` — and walk the
//! response. The JSON shape varies between ROCm releases; rather than
//! pin to one schema, we look for any object with `VRAM Total Memory
//! (B)` / `VRAM Used Memory (B)` keys (plus their lower-case
//! variants), and accept `GPU use (%)` / `Temperature (Sensor edge)
//! (C)` for utilization and temperature respectively. Missing
//! keys fall back to `None` rather than dropping the device.

use std::process::Command;

use serde_json::Value;

use super::{run_with_timeout, GpuDevice, GpuInfo};

/// rocm-smi argument variants to try, in order. Older ROCm releases
/// (pre-5.4) may reject the combined four-flag form or emit non-JSON
/// stdout, which would silently demote an AMD machine to `cpu_only`.
/// Falling back to the leaner three-flag and two-flag forms preserves
/// VRAM data even when util/temp are missing.
const ROCM_SMI_ARG_VARIANTS: &[&[&str]] = &[
  &["--showmeminfo", "vram", "--showuse", "--showtemp", "--json"],
  &["--showmeminfo", "vram", "--showuse", "--json"],
  &["--showmeminfo", "vram", "--json"],
];

pub fn probe() -> Option<GpuInfo> {
  for args in ROCM_SMI_ARG_VARIANTS {
    let mut cmd = Command::new("rocm-smi");
    cmd.args(*args);
    let Some(output) = run_with_timeout(cmd) else {
      continue;
    };
    if !output.status.success() {
      continue;
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
      continue;
    };
    let devices = parse(&stdout);
    if !devices.is_empty() {
      return Some(GpuInfo::Amd { devices });
    }
  }
  // Every variant produced either a process-spawn failure, non-zero
  // exit, non-UTF-8 stdout, or non-JSON output. Log so the operator
  // can tell that AMD probing was attempted but failed (avoids the
  // silent degrade-to-cpu_only failure mode).
  log::debug!("rocm-smi probe failed across all argument variants; treating as no-AMD");
  None
}

pub(crate) fn parse(stdout: &str) -> Vec<GpuDevice> {
  let v: Value = match serde_json::from_str(stdout) {
    Ok(v) => v,
    Err(_) => return Vec::new(),
  };
  let mut out = Vec::new();
  if let Some(obj) = v.as_object() {
    for (gpu_key, gpu_value) in obj {
      let Some(card) = gpu_value.as_object() else {
        continue;
      };
      let total = pick_u64(card, &["VRAM Total Memory (B)", "vram total memory (B)"]);
      // Newer rocm-smi releases emit `VRAM Total Used Memory (B)`
      // (note the extra "Total"); older releases drop "Total" from
      // the key. Probe both spellings so we don't silently read 0
      // used VRAM on Strix Halo / RDNA4 boxes.
      let used = pick_u64(
        card,
        &[
          "VRAM Total Used Memory (B)",
          "VRAM Used Memory (B)",
          "vram total used memory (B)",
          "vram used memory (B)",
        ],
      );
      let utilization_pct = pick_f32(card, &["GPU use (%)", "gpu use (%)", "GPU Use (%)"]);
      // ROCm reports edge temperature on a per-sensor basis; the
      // canonical key is `Temperature (Sensor edge) (C)`, with
      // `junction` and `memory` siblings on newer cards. Prefer edge
      // (matches `nvidia-smi`'s `temperature.gpu`).
      let temperature_c = pick_f32(
        card,
        &[
          "Temperature (Sensor edge) (C)",
          "Temperature (Sensor edge) (c)",
          "Temperature (Sensor #1) (C)",
          "Temperature (Sensor) (C)",
        ],
      );
      if let Some(total_bytes) = total {
        out.push(GpuDevice {
          name: gpu_key.clone(),
          total_memory_bytes: total_bytes,
          used_memory_bytes: used.unwrap_or(0),
          utilization_pct,
          temperature_c,
        });
      }
    }
  }
  out
}

fn pick_u64(card: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
  for k in keys {
    if let Some(raw) = card.get(*k) {
      if let Some(n) = raw.as_u64() {
        return Some(n);
      }
      if let Some(s) = raw.as_str() {
        if let Ok(parsed) = s.parse::<u64>() {
          return Some(parsed);
        }
      }
    }
  }
  None
}

fn pick_f32(card: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f32> {
  for k in keys {
    if let Some(raw) = card.get(*k) {
      if let Some(n) = raw.as_f64() {
        return Some(n as f32);
      }
      if let Some(n) = raw.as_u64() {
        return Some(n as f32);
      }
      if let Some(s) = raw.as_str() {
        if let Ok(parsed) = s.parse::<f32>() {
          return Some(parsed);
        }
      }
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_canonical_rocm_smi_output() {
    let stdout = r#"{
      "card0": {
        "VRAM Total Memory (B)": 17163091968,
        "VRAM Used Memory (B)": 256000000,
        "GPU use (%)": "73",
        "Temperature (Sensor edge) (C)": "62.0"
      }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "card0");
    assert_eq!(devices[0].total_memory_bytes, 17163091968);
    assert_eq!(devices[0].used_memory_bytes, 256000000);
    assert_eq!(devices[0].utilization_pct, Some(73.0));
    assert_eq!(devices[0].temperature_c, Some(62.0));
  }

  #[test]
  fn falls_back_to_lowercase_key() {
    let stdout = r#"{
      "card0": { "vram total memory (B)": "1024", "vram used memory (B)": "512" }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].total_memory_bytes, 1024);
    assert_eq!(devices[0].used_memory_bytes, 512);
    assert_eq!(devices[0].utilization_pct, None);
    assert_eq!(devices[0].temperature_c, None);
  }

  #[test]
  fn missing_util_or_temp_keeps_device_with_none() {
    // Older rocm-smi versions don't emit the util/temp keys at all
    // (or report them under a non-canonical name). The card row must
    // still surface; only the affected fields drop to `None`.
    let stdout = r#"{
      "card0": {
        "VRAM Total Memory (B)": 1024,
        "VRAM Used Memory (B)": 512
      }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].utilization_pct, None);
    assert_eq!(devices[0].temperature_c, None);
  }

  #[test]
  fn multi_card_each_gets_its_own_readings() {
    let stdout = r#"{
      "card0": {
        "VRAM Total Memory (B)": 1024,
        "VRAM Used Memory (B)": 0,
        "GPU use (%)": "20",
        "Temperature (Sensor edge) (C)": "55.0"
      },
      "card1": {
        "VRAM Total Memory (B)": 2048,
        "VRAM Used Memory (B)": 1024,
        "GPU use (%)": "80",
        "Temperature (Sensor edge) (C)": "72.0"
      }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices.len(), 2);
    // BTreeMap-backed serde_json::Map iterates lexicographically, so
    // card0 sorts first, card1 second.
    let card0 = devices.iter().find(|d| d.name == "card0").unwrap();
    let card1 = devices.iter().find(|d| d.name == "card1").unwrap();
    assert_eq!(card0.utilization_pct, Some(20.0));
    assert_eq!(card1.utilization_pct, Some(80.0));
    assert_eq!(card1.temperature_c, Some(72.0));
  }

  #[test]
  fn accepts_numeric_keys_not_strings() {
    let stdout = r#"{
      "card0": {
        "VRAM Total Memory (B)": 1024,
        "VRAM Used Memory (B)": 0,
        "GPU use (%)": 65,
        "Temperature (Sensor edge) (C)": 58
      }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices[0].utilization_pct, Some(65.0));
    assert_eq!(devices[0].temperature_c, Some(58.0));
  }

  #[test]
  fn empty_or_invalid_json_yields_no_devices() {
    assert!(parse("").is_empty());
    assert!(parse("not json").is_empty());
    assert!(parse("{}").is_empty());
  }

  #[test]
  fn parses_alternative_temperature_keys() {
    // Different ROCm releases capitalise the unit differently or
    // emit a numbered-sensor key. All variants must resolve.
    let cases = [
      (
        "Temperature (Sensor edge) (c)",
        r#"{"card0":{"VRAM Total Memory (B)":1024,"VRAM Used Memory (B)":0,"Temperature (Sensor edge) (c)":"42.0"}}"#,
        42.0_f32,
      ),
      (
        "Temperature (Sensor #1) (C)",
        r#"{"card0":{"VRAM Total Memory (B)":1024,"VRAM Used Memory (B)":0,"Temperature (Sensor #1) (C)":"57.5"}}"#,
        57.5_f32,
      ),
      (
        "Temperature (Sensor) (C)",
        r#"{"card0":{"VRAM Total Memory (B)":1024,"VRAM Used Memory (B)":0,"Temperature (Sensor) (C)":"61"}}"#,
        61.0_f32,
      ),
    ];
    for (label, stdout, expected) in cases {
      let devices = parse(stdout);
      assert_eq!(devices.len(), 1, "{label}: expected one device");
      assert_eq!(
        devices[0].temperature_c,
        Some(expected),
        "{label}: temp parse failed"
      );
    }
  }
}
