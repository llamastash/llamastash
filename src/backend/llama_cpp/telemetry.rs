//! llama-server MTP draft-acceptance telemetry: read the speculative-decoding
//! acceptance rate out of the child's log tail.
//!
//! The `slot print_timing: … draft acceptance = <rate> ( <acc> accepted /
//! <gen> generated )` line is llama-server's own format, so the parse lives
//! with the backend (reached through [`crate::backend::Backend::mtp_runtime`])
//! and the generic `status` path carries no log-format knowledge.

use crate::backend::DraftAcceptance;

/// The most recent draft-acceptance figure in `lines`, or `None` when no line
/// carries one.
///
/// Scans newest-first so an idle model keeps its final rate rather than an
/// early warm-up figure.
pub(super) fn latest_draft_acceptance(lines: &[String]) -> Option<DraftAcceptance> {
  lines
    .iter()
    .rev()
    .find_map(|l| parse_line(l))
    .map(|(rate, accepted, generated)| DraftAcceptance {
      rate,
      accepted,
      generated,
    })
}

/// Parse one `… draft acceptance = <rate> ( <acc> accepted / <gen> generated )`
/// line. Tolerant of the surrounding log prefix and whitespace.
fn parse_line(line: &str) -> Option<(f32, u64, u64)> {
  const MARKER: &str = "draft acceptance = ";
  let rest = &line[line.find(MARKER)? + MARKER.len()..];
  let rate: f32 = rest.split_whitespace().next()?.parse().ok()?;
  let num_before = |kw: &str| -> Option<u64> {
    let head = &rest[..rest.find(kw)?];
    head.split_whitespace().last()?.parse().ok()
  };
  Some((rate, num_before("accepted")?, num_before("generated")?))
}

#[cfg(test)]
mod tests {
  use super::{latest_draft_acceptance, parse_line};

  #[test]
  fn parses_a_real_slot_print_timing_line() {
    // The exact shape a real llama-server slot emits (captured 2026-07-14).
    let line = "0.06.893 I slot print_timing: id  3 | task 0 | draft acceptance = 0.65217 (  105 accepted /   161 generated ), mean len =  2.94";
    let (rate, acc, gen) = parse_line(line).expect("parses");
    assert!((rate - 0.65217).abs() < 1e-5);
    assert_eq!((acc, gen), (105, 161));
  }

  #[test]
  fn ignores_lines_without_the_marker() {
    assert!(parse_line("0.01.000 I srv  load_model: loading model").is_none());
  }

  #[test]
  fn takes_the_newest_figure() {
    let lines = vec![
      "draft acceptance = 0.20000 ( 10 accepted / 50 generated )".to_string(),
      "srv  update_slots: all slots are idle".to_string(),
      "draft acceptance = 0.80000 ( 80 accepted / 100 generated )".to_string(),
    ];
    let got = latest_draft_acceptance(&lines).expect("parses");
    assert!((got.rate - 0.8).abs() < 1e-5);
    assert_eq!((got.accepted, got.generated), (80, 100));
  }

  #[test]
  fn no_acceptance_line_yet_reports_none() {
    let lines = vec!["srv  load_model: loading".to_string()];
    assert!(latest_draft_acceptance(&lines).is_none());
  }
}
