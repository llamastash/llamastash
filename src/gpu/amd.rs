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

use std::collections::HashMap;
use std::process::Command;

use serde_json::Value;

use super::{classify_amd_memory, normalize_pci, run_with_timeout, GpuDevice};

/// rocm-smi argument variants to try, in order. Older ROCm releases
/// (pre-5.4) may reject the combined four-flag form or emit non-JSON
/// stdout, which would silently demote an AMD machine to `cpu_only`.
/// Falling back to the leaner three-flag and two-flag forms preserves
/// VRAM data even when util/temp are missing.
const ROCM_SMI_ARG_VARIANTS: &[&[&str]] = &[
  // Query VRAM + GTT together so UMA APUs (Strix Halo / Phoenix) can
  // surface their real GPU memory budget — the BIOS-dedicated VRAM
  // heap is tiny by design (e.g. 4 GiB on Strix Halo), while GTT is
  // the system-RAM-backed pool that holds the actual model weights.
  // Discrete cards keep the VRAM-only number; the carve-out signature
  // decides which (see `super::classify_amd_memory`).
  &[
    "--showmeminfo",
    "vram",
    "gtt",
    "--showuse",
    "--showtemp",
    "--json",
  ],
  &["--showmeminfo", "vram", "gtt", "--showuse", "--json"],
  &["--showmeminfo", "vram", "gtt", "--json"],
  // Backward-compat: older rocm-smi releases that don't accept the
  // multi-arg `vram gtt` form fall through to VRAM-only queries.
  &["--showmeminfo", "vram", "--showuse", "--showtemp", "--json"],
  &["--showmeminfo", "vram", "--showuse", "--json"],
  &["--showmeminfo", "vram", "--json"],
];

/// Probe AMD GPUs. On Linux the `/sys/class/drm` reads are preferred
/// (stable kernel interface, no JSON-key churn, cheap enough for the
/// per-tick refresh); `rocm-smi` is the fallback. When sysfs finds no
/// amdgpu card but rocm-smi does, the sysfs nodes are missing where the
/// vendor tool succeeds — log loudly (feeds the doctor finding) rather
/// than silently degrading to a worse data source.
pub fn probe_devices() -> Option<Vec<GpuDevice>> {
  #[cfg(target_os = "linux")]
  {
    if let Some(devices) = super::sysfs::probe_devices() {
      return Some(devices);
    }
    let rocm = probe_devices_rocm_smi();
    if rocm.is_some() {
      log::warn!(
        "amdgpu sysfs probe found no card but rocm-smi did; falling back to rocm-smi (sysfs mem_info nodes missing?)"
      );
    }
    rocm
  }
  #[cfg(not(target_os = "linux"))]
  {
    probe_devices_rocm_smi()
  }
}

/// `rocm-smi`-based probe — the fallback for the sysfs path above and
/// the only path on non-Linux Unix.
pub fn probe_devices_rocm_smi() -> Option<Vec<GpuDevice>> {
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
      // Grab PCI bus IDs for cross-backend deduplication.
      let pci_cmd_output = run_with_timeout({
        let mut c = Command::new("rocm-smi");
        c.args(["--showbus", "--json"]);
        c
      });
      let pci_map = pci_cmd_output
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| parse_pci_map(&s))
        .unwrap_or_default();
      // Also grab product names so we can match lspci entries for
      // cross-backend dedup (Vulkan, lspci use human-readable names).
      let name_cmd_output = run_with_timeout({
        let mut c = Command::new("rocm-smi");
        c.args(["--showproductname", "--json"]);
        c
      });
      let name_map = name_cmd_output
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| parse_product_name_map(&s))
        .unwrap_or_default();
      let tagged: Vec<GpuDevice> = devices
        .into_iter()
        .enumerate()
        .map(|(i, mut d)| {
          // PCI from rocm-smi is already normalized to canonical format
          // by `parse_pci_map`. Use it as the device_id for cross-backend
          // dedup (Vulkan lspci use the same canonical format).
          d.device_id = pci_map
            .get(&format!("card{}", i))
            .or(pci_map.get(&format!("gpu{}", i)))
            .cloned();
          // Use human-readable name from product name for lspci matching.
          if let Some(name) = name_map.get(&format!("card{}", i)) {
            d.name = name.clone();
          }
          d
        })
        .collect();
      // Tag each device with the "amd" backend for multi-backend
      // aggregation in `gpu::probe()`. The AMD probe is always the
      // AMD source — no need to disambiguate.
      return Some(tagged);
    }
  }
  // Every variant produced either a process-spawn failure, non-zero
  // exit, non-UTF-8 stdout, or non-JSON output. Log so the operator
  // can tell that AMD probing was attempted but failed (avoids the
  // silent degrade-to-cpu_only failure mode).
  log::debug!("rocm-smi probe failed across all argument variants; treating as no-AMD");
  None
}

/// Parse `rocm-smi --showbus --json` output into a map of
/// `cardN` -> canonical PCI address (`00000000:bb:dd.f`).
fn parse_pci_map(stdout: &str) -> HashMap<String, String> {
  let v: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(stdout) {
    Ok(Value::Object(obj)) => obj,
    _ => return std::collections::HashMap::new(),
  };
  v.into_iter()
    .filter_map(|(key, val)| {
      val
        .as_str()
        .and_then(|s| normalize_pci(s).map(|pci| (key, pci)))
    })
    .collect()
}

/// Parse `rocm-smi --showproductname --json` output into a map of
/// `cardN` -> human-readable product name (e.g. "AMD Radeon RX 7900").
fn parse_product_name_map(stdout: &str) -> HashMap<String, String> {
  let v: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(stdout) {
    Ok(Value::Object(obj)) => obj,
    _ => return std::collections::HashMap::new(),
  };
  v.into_iter()
    .filter_map(|(key, val)| {
      val
        .as_object()
        .and_then(|o| o.get("Card Series"))
        .and_then(|name_val| name_val.as_str())
        .map(|name| (key, name.to_string()))
    })
    .collect()
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
      let vram_total = pick_u64(card, &["VRAM Total Memory (B)", "vram total memory (B)"]);
      // Newer rocm-smi releases emit `VRAM Total Used Memory (B)`
      // (note the extra "Total"); older releases drop "Total" from
      // the key. Probe both spellings so we don't silently read 0
      // used VRAM on Strix Halo / RDNA4 boxes.
      let vram_used = pick_u64(
        card,
        &[
          "VRAM Total Used Memory (B)",
          "VRAM Used Memory (B)",
          "vram total used memory (B)",
          "vram used memory (B)",
        ],
      );
      // GTT (system-RAM-backed pool). Reported when the first
      // `--showmeminfo vram gtt` variant succeeds; `None` on older
      // rocm-smi releases that only emit VRAM keys.
      let gtt_total = pick_u64(card, &["GTT Total Memory (B)", "gtt total memory (B)"]);
      let gtt_used = pick_u64(
        card,
        &[
          "GTT Total Used Memory (B)",
          "GTT Used Memory (B)",
          "gtt total used memory (B)",
          "gtt used memory (B)",
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
      if let Some(vram_total_bytes) = vram_total {
        let (
          total_memory_bytes,
          used_memory_bytes,
          uma_shared_total_bytes,
          uma_shared_used_bytes,
          source,
        ) = classify_amd_memory(
          vram_total_bytes,
          vram_used.unwrap_or(0),
          gtt_total,
          gtt_used,
          // rocm-smi carries no PCI class; fall back to the carve-out
          // size heuristic. The sysfs probe (the primary path) supplies
          // the authoritative integrated flag.
          false,
        );
        out.push(GpuDevice {
          name: gpu_key.clone(),
          backend: "amd".into(),
          total_memory_bytes,
          used_memory_bytes,
          utilization_pct,
          temperature_c,
          uma_shared_total_bytes,
          uma_shared_used_bytes,
          device_id: None,
          classification_source: Some(source),
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
  fn uma_sums_vram_and_gtt_on_carve_signature() {
    // Strix Halo (current config): 512 MiB BIOS carve-out + ~124 GiB
    // GTT pool. The tiny dedicated VRAM is the carve signature → sum
    // both heaps, mark the GTT portion shared, source = carve.
    let stdout = r#"{
      "card0": {
        "VRAM Total Memory (B)": 536870912,
        "VRAM Total Used Memory (B)": 486838272,
        "GTT Total Memory (B)": 133143986176,
        "GTT Total Used Memory (B)": 2843930624
      }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(
      devices[0].total_memory_bytes,
      536_870_912 + 133_143_986_176,
      "UMA total should sum VRAM + GTT"
    );
    assert_eq!(
      devices[0].used_memory_bytes,
      486_838_272 + 2_843_930_624,
      "UMA used should sum VRAM + GTT"
    );
    assert_eq!(devices[0].uma_shared_total_bytes, Some(133_143_986_176));
    assert_eq!(
      devices[0].classification_source,
      Some(crate::gpu::ClassSource::CarveSignature)
    );
  }

  #[test]
  fn discrete_card_with_large_gtt_stays_discrete() {
    // R18 regression: a discrete 16 GiB card on a 128 GiB-RAM host
    // gets a kernel-sized GTT aperture (~64 GiB) that exceeds VRAM.
    // The old `gtt > vram` heuristic mis-summed this to 80 GiB and
    // marked it unified; the carve signature (VRAM >= 1 GiB) keeps it
    // discrete on VRAM only.
    let stdout = r#"{
      "card0": {
        "VRAM Total Memory (B)": 17179869184,
        "VRAM Total Used Memory (B)": 5368709120,
        "GTT Total Memory (B)": 68719476736,
        "GTT Total Used Memory (B)": 536870912
      }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].total_memory_bytes, 17_179_869_184);
    assert_eq!(devices[0].used_memory_bytes, 5_368_709_120);
    assert_eq!(devices[0].uma_shared_total_bytes, None);
    assert_eq!(
      devices[0].classification_source,
      Some(crate::gpu::ClassSource::Discrete)
    );
  }

  #[test]
  fn missing_gtt_falls_back_to_vram_only() {
    // Older rocm-smi without `gtt` support, or the fallback arg chain
    // succeeding without GTT — must behave like before this change.
    let stdout = r#"{
      "card0": {
        "VRAM Total Memory (B)": 17163091968,
        "VRAM Used Memory (B)": 256000000
      }
    }"#;
    let devices = parse(stdout);
    assert_eq!(devices[0].total_memory_bytes, 17_163_091_968);
    assert_eq!(devices[0].used_memory_bytes, 256_000_000);
  }

  #[test]
  fn classify_amd_memory_branches() {
    use crate::gpu::{classify_amd_memory, ClassSource};
    let carve = 512 * 1024 * 1024;
    let big_gtt = 100 * 1024 * 1024 * 1024;
    // Carve signature (vram < 1 GiB) → sum, GTT marked shared.
    assert_eq!(
      classify_amd_memory(carve, 1, Some(big_gtt), Some(40), false),
      (
        carve + big_gtt,
        41,
        Some(big_gtt),
        Some(40),
        ClassSource::CarveSignature
      )
    );
    // Discrete (vram >= 1 GiB, not integrated) → vram only even when GTT is larger.
    let vram16 = 16 * 1024 * 1024 * 1024;
    assert_eq!(
      classify_amd_memory(vram16, 5, Some(big_gtt), Some(1), false),
      (vram16, 5, None, None, ClassSource::Discrete)
    );
    // Integrated APU with a large VRAM carve-out (Strix Halo reports
    // 4 GiB ≫ the 1 GiB heuristic ceiling) → the PCI-class flag forces
    // unified: sum VRAM + GTT, GTT marked shared.
    let vram4 = 4 * 1024 * 1024 * 1024;
    assert_eq!(
      classify_amd_memory(vram4, 5, Some(big_gtt), Some(2), true),
      (
        vram4 + big_gtt,
        7,
        Some(big_gtt),
        Some(2),
        ClassSource::CarveSignature
      )
    );
    // Carve signature with GTT total but no used → still sums, adds 0.
    assert_eq!(
      classify_amd_memory(carve, 1, Some(big_gtt), None, false),
      (
        carve + big_gtt,
        1,
        Some(big_gtt),
        Some(0),
        ClassSource::CarveSignature
      )
    );
    // No GTT data on a carve card → sums with 0 GTT (degenerate).
    assert_eq!(
      classify_amd_memory(carve, 2, None, None, false),
      (carve, 2, Some(0), Some(0), ClassSource::CarveSignature)
    );
    // Integrated APU where vram_total already reflects the full GTT aperture (e.g. 96G + 96G GTT)
    // -> must not double count to 192G.
    let full96 = 96 * 1024 * 1024 * 1024;
    assert_eq!(
      classify_amd_memory(full96, 47, Some(full96), Some(47), true),
      (
        full96,
        47,
        Some(full96),
        Some(47),
        ClassSource::CarveSignature
      )
    );
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
