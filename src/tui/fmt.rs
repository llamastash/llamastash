//! Shared display helpers for the TUI panes.
//!
//! Centralizing these formatters avoids the silent drift that crept in
//! when three panes each defined their own `format_bytes` with subtly
//! different thresholds.

// `panel_title` moved to `Palette::title_style()` / `Palette::panel_block`
// during the Tier-B sweep — see `src/theme/palette.rs`.

/// Format a token count for the Ctx column / launch picker:
/// `131072` → `128k`, `262144` → `256k`, `2_000_000` → `2.0M`.
/// Sub-1024 values render as raw integers (e.g., `512`).
pub(crate) fn format_tokens(n: u64) -> String {
  const K: u64 = 1024;
  const M: u64 = K * 1024;
  if n >= M {
    let m = n as f64 / M as f64;
    if m >= 10.0 {
      format!("{m:.0}M")
    } else {
      format!("{m:.1}M")
    }
  } else if n >= K {
    let k = n as f64 / K as f64;
    if k >= 10.0 {
      format!("{k:.0}k")
    } else {
      format!("{k:.1}k")
    }
  } else {
    n.to_string()
  }
}

/// Format a byte count for compact display in panel headers and bars.
/// Rounds to a single decimal place between 1G and 10G (so `4.2G` is
/// distinguishable from `5.1G`), and drops the decimal at 10G+ to keep
/// the label inside ~4 characters.
pub(crate) fn format_bytes(bytes: u64) -> String {
  const KIB: f64 = 1024.0;
  const MIB: f64 = KIB * 1024.0;
  const GIB: f64 = MIB * 1024.0;
  let b = bytes as f64;
  if b >= GIB {
    let g = b / GIB;
    if g >= 10.0 {
      format!("{g:.0}G")
    } else {
      format!("{g:.1}G")
    }
  } else if b >= MIB {
    format!("{:.0}M", b / MIB)
  } else if b >= KIB {
    format!("{:.0}K", b / KIB)
  } else {
    format!("{bytes}B")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn format_tokens_basic_ranges() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(512), "512");
    assert_eq!(format_tokens(1024), "1.0k");
    assert_eq!(format_tokens(2048), "2.0k");
    assert_eq!(format_tokens(8192), "8.0k");
    assert_eq!(format_tokens(32_768), "32k");
    assert_eq!(format_tokens(131_072), "128k");
    assert_eq!(format_tokens(262_144), "256k");
    assert_eq!(format_tokens(1_048_576), "1.0M");
    assert_eq!(format_tokens(2_097_152), "2.0M");
    assert_eq!(format_tokens(10_485_760), "10M");
  }

  #[test]
  fn under_kib_renders_raw_bytes() {
    assert_eq!(format_bytes(0), "0B");
    assert_eq!(format_bytes(512), "512B");
    assert_eq!(format_bytes(1023), "1023B");
  }

  #[test]
  fn kib_and_mib_drop_decimals() {
    assert_eq!(format_bytes(1024), "1K");
    assert_eq!(format_bytes(1024 * 1024), "1M");
  }

  #[test]
  fn gib_below_ten_keeps_one_decimal() {
    assert_eq!(format_bytes(4_500_000_000), "4.2G");
    assert_eq!(format_bytes(9_000_000_000), "8.4G");
  }

  #[test]
  fn gib_at_or_above_ten_drops_decimal() {
    assert_eq!(format_bytes(11_000_000_000), "10G");
    assert_eq!(format_bytes(24_000_000_000), "22G");
    assert_eq!(format_bytes(100_000_000_000), "93G");
  }
}
