//! Vulkan fallback probe — last-resort "is there *any* GPU?" check.
//!
//! Calls `vulkaninfo --summary` (much faster than the full
//! `vulkaninfo`) and looks for at least one `GPU` line. We don't
//! parse memory because the Vulkan summary format is unstable
//! between releases; the supervisor uses this signal only to hint
//! that the user can probably set `-ngl > 0`. Returns `None` if
//! `vulkaninfo` isn't installed or finds no GPU.

use std::process::Command;

use super::{GpuDevice, GpuInfo};

pub fn probe() -> Option<GpuInfo> {
  let output = Command::new("vulkaninfo").arg("--summary").output().ok()?;
  if !output.status.success() {
    return None;
  }
  let stdout = String::from_utf8(output.stdout).ok()?;
  let names = parse(&stdout);
  if names.is_empty() {
    return None;
  }
  Some(GpuInfo::Amd {
    // Vulkan can't tell us vendor reliably or memory accurately.
    // We surface it under `Amd` (the most likely match on Linux
    // when NVIDIA + ROCm both failed) with zero memory so the TUI
    // can render "VRAM: unknown".
    devices: names
      .into_iter()
      .map(|name| GpuDevice {
        name,
        total_memory_bytes: 0,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
      })
      .collect(),
  })
}

pub(crate) fn parse(stdout: &str) -> Vec<String> {
  let mut out = Vec::new();
  for line in stdout.lines() {
    let trimmed = line.trim();
    // The summary section uses lines like:
    //   "GPU0:\n\tdeviceName       = AMD Radeon RX 7900 XTX"
    if let Some(rest) = trimmed.strip_prefix("deviceName") {
      if let Some(idx) = rest.find('=') {
        let name = rest[idx + 1..].trim().to_string();
        if !name.is_empty() {
          out.push(name);
        }
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn extracts_device_names_from_vulkaninfo_summary() {
    let stdout = "==========\n\
                  GPU0:\n\
                  \tdeviceName       = AMD Radeon RX 7900 XTX\n\
                  \tapiVersion       = 1.3.250\n\
                  GPU1:\n\
                  \tdeviceName       = llvmpipe (LLVM 16.0.6, 256 bits)\n";
    let names = parse(stdout);
    assert_eq!(
      names,
      vec!["AMD Radeon RX 7900 XTX", "llvmpipe (LLVM 16.0.6, 256 bits)"]
    );
  }

  #[test]
  fn empty_summary_yields_no_devices() {
    assert!(parse("").is_empty());
    assert!(parse("WARNING: vulkan loader missing\n").is_empty());
  }
}
