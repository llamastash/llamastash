//! Civil-calendar arithmetic and ISO-8601 formatting without a chrono
//! dependency. Algorithms from Howard Hinnant's date library (public
//! domain).

use std::time::SystemTime;

/// Compact RFC 3339 in UTC: `YYYY-MM-DDTHH:MM:SSZ`. Best-effort —
/// pre-1970 `SystemTime`s clamp to epoch.
pub fn iso8601(t: SystemTime) -> String {
  let secs = t
    .duration_since(SystemTime::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
  let (y, mo, d, h, mi, s) = secs_to_ymdhms(secs);
  format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

pub fn iso8601_now() -> String {
  iso8601(SystemTime::now())
}

/// Whole milliseconds since the Unix epoch, `0` on a clock set before it. A
/// wall clock, so a backward step can shorten a duration: a caller that times
/// elapsed work uses a monotonic clock instead.
pub fn now_ms() -> u64 {
  SystemTime::now()
    .duration_since(SystemTime::UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .unwrap_or(0)
}

pub fn yyyymmdd_from_secs(total_secs: u64) -> String {
  let days = (total_secs / 86_400) as i64;
  let (y, m, d) = civil_from_days(days);
  format!("{y:04}-{m:02}-{d:02}")
}

/// Current date as `YYYY-MM-DD`, or `None` on pre-1970 clocks.
pub fn current_yyyymmdd() -> Option<String> {
  let secs = SystemTime::now()
    .duration_since(SystemTime::UNIX_EPOCH)
    .ok()?
    .as_secs();
  Some(yyyymmdd_from_secs(secs))
}

/// Parse `YYYY-MM-DD` into `(year, month, day)`. Returns `None` on
/// malformed input or out-of-range components.
pub fn parse_yyyymmdd(s: &str) -> Option<(i32, u32, u32)> {
  let trimmed = s.trim();
  let parts: Vec<&str> = trimmed.split('-').collect();
  if parts.len() != 3 {
    return None;
  }
  let y = parts[0].parse::<i32>().ok()?;
  let m = parts[1].parse::<u32>().ok()?;
  let d = parts[2].parse::<u32>().ok()?;
  if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
    return None;
  }
  Some((y, m, d))
}

/// Days elapsed from `from` to `to`. `None` if `to` is earlier than
/// `from`.
pub fn days_between(from: (i32, u32, u32), to: (i32, u32, u32)) -> Option<u64> {
  let a = days_from_civil(from);
  let b = days_from_civil(to);
  if b < a {
    None
  } else {
    Some((b - a) as u64)
  }
}

/// Decompose `total_secs` into `(year, month, day, hour, minute,
/// second)`.
pub fn secs_to_ymdhms(total_secs: u64) -> (i32, u32, u32, u32, u32, u32) {
  let secs_per_day = 86_400_u64;
  let days = (total_secs / secs_per_day) as i64;
  let secs = (total_secs % secs_per_day) as u32;
  let h = secs / 3600;
  let mi = (secs % 3600) / 60;
  let s = secs % 60;
  let (y, mo, d) = civil_from_days(days);
  (y, mo, d, h, mi, s)
}

/// Wall-clock `(hour, minute, second)` of a Unix timestamp in the machine's
/// local time zone. UTC on a platform without `localtime_r`.
pub fn local_hms(epoch_secs: u64) -> (u32, u32, u32) {
  #[cfg(unix)]
  {
    let t = epoch_secs as libc::time_t;
    // SAFETY: `tm` is plain integers (and a pointer `localtime_r` sets), so
    // the zeroed value is valid; both pointers outlive the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
      return (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32);
    }
  }
  let (_, _, _, h, m, s) = secs_to_ymdhms(epoch_secs);
  (h, m, s)
}

/// Howard Hinnant's `civil_from_days` (public domain).
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
  let z = z + 719_468;
  let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
  let doe = (z - era * 146_097) as u32;
  let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
  let y = (yoe as i32) + (era as i32) * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = doy - (153 * mp + 2) / 5 + 1;
  let m = if mp < 10 { mp + 3 } else { mp - 9 };
  let y = if m <= 2 { y + 1 } else { y };
  (y, m, d)
}

/// Howard Hinnant's `days_from_civil` (public domain).
pub fn days_from_civil((y, m, d): (i32, u32, u32)) -> i64 {
  let y = if m <= 2 { y - 1 } else { y };
  let era = if y >= 0 { y } else { y - 399 } / 400;
  let yoe = (y - era * 400) as u32;
  let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
  let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
  (era as i64) * 146_097 + (doe as i64) - 719_468
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{Duration, UNIX_EPOCH};

  #[test]
  fn iso8601_known_epoch() {
    let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert_eq!(iso8601(t), "2023-11-14T22:13:20Z");
  }

  #[test]
  fn iso8601_epoch_zero() {
    assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
  }

  #[test]
  fn yyyymmdd_roundtrip() {
    let day = yyyymmdd_from_secs(1_700_000_000);
    assert_eq!(day, "2023-11-14");
    let parsed = parse_yyyymmdd(&day).unwrap();
    assert_eq!(parsed, (2023, 11, 14));
  }

  #[test]
  fn parse_yyyymmdd_rejects_garbage() {
    assert!(parse_yyyymmdd("nope").is_none());
    assert!(parse_yyyymmdd("2023-13-01").is_none());
    assert!(parse_yyyymmdd("2023-12-32").is_none());
  }

  #[test]
  fn local_hms_is_the_utc_clock_shifted_by_whole_quarter_hours() {
    // Every zone offset in use is a multiple of 15 minutes, so the local
    // clock keeps UTC's second and is a whole number of quarter hours away.
    for epoch in [1_700_000_000u64, 1_700_003_661, 1_720_000_000] {
      let (hour, min, sec) = local_hms(epoch);
      let (_, _, _, utc_hour, utc_min, utc_sec) = secs_to_ymdhms(epoch);
      assert!(hour < 24 && min < 60, "{hour}:{min}");
      assert_eq!(sec, utc_sec);
      let apart = i64::from(hour * 60 + min) - i64::from(utc_hour * 60 + utc_min);
      assert_eq!(
        apart.rem_euclid(15),
        0,
        "{hour}:{min} vs {utc_hour}:{utc_min}"
      );
    }
  }

  #[test]
  fn days_between_basic() {
    assert_eq!(days_between((2023, 11, 14), (2023, 11, 21)), Some(7));
    assert_eq!(days_between((2023, 11, 21), (2023, 11, 14)), None);
  }
}
