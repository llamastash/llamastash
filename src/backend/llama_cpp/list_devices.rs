//! Authoritative device discovery via `llama-server --list-devices`.
//! Owned by the llama.cpp backend — this is the llama.cpp device probe.
//!
//! A single `llama-server` binary is compiled against exactly one GPU
//! backend family (CUDA *or* HIP/ROCm, optionally plus Vulkan), so the
//! only trustworthy source for the strings that `--device` accepts is
//! the binary itself. We run each configured binary with
//! `--list-devices` and parse its output:
//!
//! ```text
//! Available devices:
//!   Vulkan0: AMD Radeon AI PRO R9700 (RADV GFX1201) (32624 MiB, 31221 MiB free)
//!   Vulkan1: NVIDIA GeForce RTX 3080 (10240 MiB, 9729 MiB free)
//! ```
//!
//! The selector (`Vulkan0`) is what we pass back to `--device`
//! verbatim — no index math, no backend guessing. The backend label
//! (`Vulkan`, `CUDA`, `ROCm`, `Metal`) is the selector's alphabetic
//! prefix, used only for display.
//!
//! [`probe_devices`] returns one binary's [`crate::backend::Device`] list; the
//! neutral server catalog ([`crate::backend::build_server_catalog`]) carries the
//! owning binary per [`crate::backend::Server`]. `CUDA0` and `Vulkan1` for the
//! same physical card stay distinct — they are genuinely different launch
//! options (different compute backend, different binary).

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Wall-clock budget for a single `--list-devices` invocation. The
/// command loads the backend (which can spin up the Vulkan loader /
/// CUDA runtime) so it is heavier than a pure arg-parse, but still
/// well under a few seconds on healthy hardware.
const LIST_DEVICES_TIMEOUT: Duration = Duration::from_secs(10);

/// One device as reported by a specific binary's `--list-devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryDevice {
  /// Exact `--device` selector (`Vulkan0`, `CUDA0`, `ROCm0`).
  pub selector: String,
  /// Backend label inferred from the selector prefix (`Vulkan`,
  /// `CUDA`, `ROCm`, `Metal`). Display-only.
  pub backend: String,
  /// Human-readable adapter name, parens and all
  /// (`AMD Radeon AI PRO R9700 (RADV GFX1201)`).
  pub name: String,
  /// Total device memory in MiB, when the line carried it.
  pub total_mib: Option<u64>,
  /// Free device memory in MiB, when the line carried it.
  pub free_mib: Option<u64>,
}

/// Selector's alphabetic prefix, e.g. `Vulkan0` → `Vulkan`. Empty when
/// the selector somehow has no leading letters (shouldn't happen for
/// real llama.cpp device names, but stays total).
fn backend_of(selector: &str) -> String {
  selector
    .chars()
    .take_while(|c| c.is_ascii_alphabetic())
    .collect()
}

/// Pull a `(<total> MiB, <free> MiB free)` suffix off the end of the
/// device description, returning `(name_without_suffix, total, free)`.
/// The name is whatever precedes the memory parenthetical — it may
/// itself contain parens (`(RADV GFX1201)`), so we anchor on the
/// *last* group that matches the memory shape rather than the first
/// `(`.
fn split_memory(desc: &str) -> (String, Option<u64>, Option<u64>) {
  let trimmed = desc.trim();
  // Find the last '(' that opens a group ending in ')' at the end of
  // the string and parse it as memory. If parsing fails, treat the
  // whole thing as the name.
  if trimmed.ends_with(')') {
    if let Some(open) = trimmed.rfind('(') {
      let inner = &trimmed[open + 1..trimmed.len() - 1];
      if let Some((total, free)) = parse_memory_inner(inner) {
        let name = trimmed[..open].trim().to_string();
        return (name, Some(total), Some(free));
      }
    }
  }
  (trimmed.to_string(), None, None)
}

/// Parse the inside of a memory parenthetical:
/// `32624 MiB, 31221 MiB free` → `(32624, 31221)`. Returns `None` if
/// the shape doesn't match so callers fall back to "no memory info".
fn parse_memory_inner(inner: &str) -> Option<(u64, u64)> {
  let (total_part, free_part) = inner.split_once(',')?;
  let total = total_part.trim().strip_suffix("MiB")?.trim().parse().ok()?;
  let free = free_part
    .trim()
    .strip_suffix("free")?
    .trim()
    .strip_suffix("MiB")?
    .trim()
    .parse()
    .ok()?;
  Some((total, free))
}

/// Parse `--list-devices` stdout into a device list. Lines that don't
/// match the `  <Selector>: <desc>` shape (the `Available devices:`
/// header, blank lines, stray backend warnings that leaked to stdout)
/// are skipped. A valid selector starts with a letter and contains at
/// least one trailing digit.
pub fn parse_list_devices(stdout: &str) -> Vec<BinaryDevice> {
  let mut out = Vec::new();
  for line in stdout.lines() {
    let trimmed = line.trim();
    let Some((selector, desc)) = trimmed.split_once(':') else {
      continue;
    };
    let selector = selector.trim();
    if !is_selector(selector) {
      continue;
    }
    let (name, total_mib, free_mib) = split_memory(desc);
    out.push(BinaryDevice {
      backend: backend_of(selector),
      selector: selector.to_string(),
      name,
      total_mib,
      free_mib,
    });
  }
  out
}

/// A device selector is alphabetic-prefix + numeric-suffix, e.g.
/// `Vulkan0`, `CUDA10`, `ROCm0`. Guards against treating the
/// `Available devices` header (no trailing digit, and the split on the
/// first `:` would yield `Available devices`) or other noise as a row.
fn is_selector(s: &str) -> bool {
  let mut chars = s.chars();
  let first_alpha = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
  let ends_digit = s.chars().last().is_some_and(|c| c.is_ascii_digit());
  // Whole string must be alnum (no spaces) — `Available devices` has a
  // space and so is rejected even before the digit check.
  let all_alnum = s.chars().all(|c| c.is_ascii_alphanumeric());
  first_alpha && ends_digit && all_alnum
}

/// Probe one `llama-server` binary for the neutral [`crate::backend::Device`]
/// list — the backend's [`crate::backend::Backend::probe_devices`] hook. Maps
/// each parsed [`BinaryDevice`] to a [`crate::backend::Device`] (the selector's
/// prefix becomes `gpu_backend`); the owning binary is carried by the
/// [`crate::backend::Server`] this feeds, not the device.
pub fn probe_devices(binary: &Path) -> Vec<crate::backend::Device> {
  probe(binary)
    .into_iter()
    .map(|d| crate::backend::Device {
      selector: d.selector,
      gpu_backend: d.backend,
      name: d.name,
      total_mib: d.total_mib,
      free_mib: d.free_mib,
    })
    .collect()
}

/// Run `<binary> --list-devices` and parse the result. Returns an
/// empty vec on any failure (missing binary, non-zero exit, timeout) —
/// a binary that can't enumerate just contributes nothing to the
/// catalog rather than failing the whole probe.
pub fn probe(binary: &Path) -> Vec<BinaryDevice> {
  let mut cmd = Command::new(binary);
  // The unified app takes server flags behind `serve`; `llama --list-devices`
  // is an unknown command.
  cmd.args(super::serve_prefix(binary));
  cmd.arg("--list-devices");
  match crate::util::process::run_with_drain_and_timeout(cmd, LIST_DEVICES_TIMEOUT) {
    Ok(out) => {
      let stdout = String::from_utf8_lossy(&out.stdout);
      parse_list_devices(&stdout)
    }
    Err(e) => {
      log::warn!(
        "`{} --list-devices` failed: {e:?}; contributing no devices",
        binary.display()
      );
      Vec::new()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_vulkan_multi_device_block() {
    let out = "\
Available devices:
  Vulkan0: AMD Radeon AI PRO R9700 (RADV GFX1201) (32624 MiB, 31221 MiB free)
  Vulkan1: NVIDIA GeForce RTX 3080 (10240 MiB, 9729 MiB free)
";
    let devs = parse_list_devices(out);
    assert_eq!(devs.len(), 2);
    assert_eq!(devs[0].selector, "Vulkan0");
    assert_eq!(devs[0].backend, "Vulkan");
    assert_eq!(devs[0].name, "AMD Radeon AI PRO R9700 (RADV GFX1201)");
    assert_eq!(devs[0].total_mib, Some(32624));
    assert_eq!(devs[0].free_mib, Some(31221));
    assert_eq!(devs[1].selector, "Vulkan1");
    assert_eq!(devs[1].name, "NVIDIA GeForce RTX 3080");
    assert_eq!(devs[1].total_mib, Some(10240));
  }

  #[test]
  fn parses_rocm_block() {
    let out = "\
Available devices:
  ROCm0: AMD Radeon AI PRO R9700 (32624 MiB, 32542 MiB free)
";
    let devs = parse_list_devices(out);
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].selector, "ROCm0");
    assert_eq!(devs[0].backend, "ROCm");
    assert_eq!(devs[0].name, "AMD Radeon AI PRO R9700");
    assert_eq!(devs[0].total_mib, Some(32624));
    assert_eq!(devs[0].free_mib, Some(32542));
  }

  #[test]
  fn skips_header_and_noise() {
    let out = "\
WARNING: radv is not a conformant Vulkan implementation, testing use only.
Available devices:
  CUDA0: NVIDIA GeForce RTX 3080 (10240 MiB, 9729 MiB free)

random line: with a colon but not a selector
";
    let devs = parse_list_devices(out);
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].selector, "CUDA0");
    assert_eq!(devs[0].backend, "CUDA");
  }

  #[test]
  fn device_line_without_memory_keeps_name() {
    let devs = parse_list_devices("  Vulkan0: Some Adapter\n");
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].name, "Some Adapter");
    assert_eq!(devs[0].total_mib, None);
    assert_eq!(devs[0].free_mib, None);
  }

  #[test]
  fn distinct_selectors_for_same_card_are_kept() {
    // CUDA0 and Vulkan1 may be the same physical 3080, but they are
    // different launch options and must both survive.
    let devs = parse_list_devices(
      "  CUDA0: NVIDIA GeForce RTX 3080 (10240 MiB, 9729 MiB free)\n  Vulkan1: NVIDIA GeForce RTX 3080 (10240 MiB, 9729 MiB free)\n",
    );
    assert_eq!(devs.len(), 2);
    assert_ne!(devs[0].selector, devs[1].selector);
  }

  #[test]
  fn name_with_trailing_non_memory_paren_is_preserved() {
    // A trailing paren that isn't a memory group must not be eaten.
    let devs = parse_list_devices("  Vulkan0: Intel Arc (DG2)\n");
    assert_eq!(devs[0].name, "Intel Arc (DG2)");
    assert_eq!(devs[0].total_mib, None);
  }
}
