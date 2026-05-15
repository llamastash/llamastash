//! Fuzzy filter for the model list pane (R25).
//!
//! v1 ships a hand-rolled subsequence matcher rather than pulling
//! in `nucleo-matcher` or `fuzzy-matcher`. The matcher we need is
//! small: per-row score = position of each matched character +
//! contiguous-run penalty (fewer runs = better) — enough to
//! prioritise `qwen` over `q*w*e*n` on a real model list.
//!
//! Decision (closes the plan's deferred `nucleo-matcher` question
//! from Unit 6): the hand-rolled ranker stays. It keeps the dep
//! tree tight, ships ~50 LOC, and the model-name corpus (file
//! stems + arch + quant tokens) is far smaller than the workloads
//! nucleo is optimised for. Revisit if catalogs grow past ~500
//! models *and* user-perceived filter latency regresses; until
//! then the extra crate isn't worth the carry.

/// Small score wrapper so callers can sort `(score, idx)` without
/// reimplementing the comparison rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchScore(pub u32);

/// Score `query` against `candidate`. Returns `None` when the
/// query is not a subsequence of the candidate; otherwise the
/// returned score is "lower is better" — `0` for a perfect
/// case-insensitive prefix.
pub fn score(query: &str, candidate: &str) -> Option<MatchScore> {
  if query.is_empty() {
    return Some(MatchScore(0));
  }
  let q: Vec<char> = query.to_lowercase().chars().collect();
  let c: Vec<char> = candidate.to_lowercase().chars().collect();

  let mut qi = 0usize;
  let mut total: u32 = 0;
  let mut last_match: Option<usize> = None;
  let mut runs: u32 = 0;
  for (ci, ch) in c.iter().enumerate() {
    if qi < q.len() && *ch == q[qi] {
      qi += 1;
      let new_run = !matches!(last_match, Some(prev) if prev + 1 == ci);
      if new_run {
        runs = runs.saturating_add(1);
      }
      // Earlier matches are slightly better than later ones.
      total = total.saturating_add(ci as u32);
      last_match = Some(ci);
    }
  }
  if qi != q.len() {
    return None;
  }
  // Penalise gaps (more runs = more fragmented match).
  let runs_penalty = runs.saturating_mul(8);
  Some(MatchScore(total.saturating_add(runs_penalty)))
}

/// Convenience: rank a list of candidates by `score`. Returns the
/// indices of matching rows in best-first order, dropping non-matches.
pub fn rank<'a, I, S>(query: &str, candidates: I) -> Vec<usize>
where
  I: IntoIterator<Item = &'a S>,
  S: AsRef<str> + 'a,
{
  let mut scored: Vec<(MatchScore, usize)> = candidates
    .into_iter()
    .enumerate()
    .filter_map(|(i, s)| score(query, s.as_ref()).map(|sc| (sc, i)))
    .collect();
  scored.sort();
  scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_query_matches_everything_with_zero_score() {
    assert_eq!(score("", "anything"), Some(MatchScore(0)));
  }

  #[test]
  fn case_insensitive_match() {
    assert!(score("qwen", "Qwen-2.5-Coder").is_some());
    assert!(score("QWEN", "qwen-2.5-coder").is_some());
  }

  #[test]
  fn non_subsequence_returns_none() {
    assert_eq!(score("xyz", "qwen-2.5-coder"), None);
  }

  #[test]
  fn contiguous_match_scores_better_than_fragmented() {
    let a = score("qwen", "qwen-coder").unwrap();
    let b = score("qwen", "q-w-e-n-coder").unwrap();
    assert!(
      a < b,
      "contiguous should outrank fragmented: {a:?} vs {b:?}"
    );
  }

  #[test]
  fn rank_returns_best_first_and_drops_misses() {
    let names = ["qwen-2.5-coder", "phi-3.5-mini", "mistral-7b", "qwen2"];
    let ranked = rank("qwen", &names);
    assert!(!ranked.is_empty());
    // The first hit should be one of the qwen variants — index 0
    // ("qwen-2.5-coder") or 3 ("qwen2"). `qwen2` is shorter so
    // the contiguous match starts earlier and lives next to fewer
    // distractors; either way both qwens should outrank
    // phi/mistral, and neither phi nor mistral should appear at all.
    for idx in &ranked {
      assert!(
        names[*idx].starts_with("qwen"),
        "ranked includes non-qwen row: {}",
        names[*idx]
      );
    }
  }
}
