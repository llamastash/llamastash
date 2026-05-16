//! Shared display helpers for the TUI panes.
//!
//! Centralizing these formatters avoids the silent drift that crept in
//! when three panes each defined their own `format_bytes` with subtly
//! different thresholds.

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
