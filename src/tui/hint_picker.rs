//! Shared "rank-and-fit" picker for chip strips.
//!
//! Three TUI surfaces show hint chips that have to compromise on
//! narrow terminals:
//!   1. The global title-row strip (`help_bar`).
//!   2. The Models pane title strip (`list_pane::build_block_title`).
//!   3. The right pane's bottom border (`right_pane::bottom_hint_chips`).
//!
//! Every chip declares a numeric `rank` (lower = stickier under
//! width pressure) and the renderer asks [`pick`] which chips fit
//! into the available cells. **Source order = display order**;
//! rank only decides which chips survive when the budget can't hold
//! them all.

/// One hint chip plus its priority rank. Lower rank wins first
/// under budget pressure (so `rank: 10` survives where `rank: 60`
/// drops). Equal ranks fall back to source order.
///
/// `text` is the rendered chip including the keycap and label
/// (e.g. `"Enter:launch"`, `"↑/↓:scroll"`). The caller resolves
/// `text` through the App's `KeyMap` so config rebinds flow
/// through.
#[derive(Debug, Clone)]
pub struct RankedChip {
  pub rank: u8,
  pub text: String,
}

impl RankedChip {
  pub fn new(rank: u8, text: impl Into<String>) -> Self {
    Self {
      rank,
      text: text.into(),
    }
  }
}

/// Convenience for test fixtures and ad-hoc chip strings — gives
/// the chip a middle-of-the-road rank (50). Production call sites
/// should set the rank explicitly via [`RankedChip::new`].
impl From<&str> for RankedChip {
  fn from(text: &str) -> Self {
    Self::new(50, text)
  }
}

impl From<String> for RankedChip {
  fn from(text: String) -> Self {
    Self::new(50, text)
  }
}

/// Greedy-fit `chips` into `budget` cells, joining picked chips
/// with `sep`. Lower-rank chips win first; equal ranks fall back
/// to the chip's source position. Returns the surviving chip
/// strings in **source order** so visible chips keep their
/// familiar left-to-right positions as the terminal resizes.
///
/// `budget` is the total width available for chips + their
/// separators. The picker charges sum(chip_widths) +
/// (n - 1) * sep_width, which is the exact width the renderer will
/// emit when it joins the surviving chips.
pub fn pick(chips: Vec<RankedChip>, budget: usize, sep: &str) -> Vec<String> {
  let sep_w = sep.chars().count();
  // Tag each chip with its source position so we can restore the
  // display order after the rank-ordered greedy pass.
  let mut by_rank: Vec<(usize, RankedChip)> = chips.into_iter().enumerate().collect();
  by_rank.sort_by_key(|(idx, c)| (c.rank, *idx));

  let mut taken: Vec<(usize, RankedChip)> = Vec::with_capacity(by_rank.len());
  let mut spent = 0usize;
  for (idx, c) in by_rank {
    let chip_w = c.text.chars().count();
    // Each added chip after the first contributes one extra
    // separator's worth of cells. Empty `taken` → no separator yet.
    let cost = if taken.is_empty() {
      chip_w
    } else {
      sep_w + chip_w
    };
    if spent + cost <= budget {
      spent += cost;
      taken.push((idx, c));
    }
  }
  taken.sort_by_key(|(idx, _)| *idx);
  taken.into_iter().map(|(_, c)| c.text).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pick_keeps_everything_when_budget_is_generous() {
    let chips = vec![
      RankedChip::new(10, "Enter:launch"),
      RankedChip::new(20, "s:stop"),
      RankedChip::new(30, "f:fav"),
    ];
    let picked = pick(chips, 100, " · ");
    assert_eq!(picked, vec!["Enter:launch", "s:stop", "f:fav"]);
  }

  #[test]
  fn pick_drops_highest_rank_first_under_pressure() {
    // Total = 12 + 6 + 5 + 2*3 (seps) = 29. Budget = 22 means one
    // chip must drop. `f:fav` (rank 30) is the loser.
    let chips = vec![
      RankedChip::new(10, "Enter:launch"),
      RankedChip::new(20, "s:stop"),
      RankedChip::new(30, "f:fav"),
    ];
    let picked = pick(chips, 22, " · ");
    assert_eq!(picked, vec!["Enter:launch", "s:stop"]);
  }

  #[test]
  fn pick_returns_chips_in_source_order_even_when_high_rank_drops() {
    // `s:stop` (rank 10, source idx 1) is the *first* picked by
    // rank but renders in the middle slot — display order matches
    // source order.
    let chips = vec![
      RankedChip::new(20, "Enter:launch"),
      RankedChip::new(10, "s:stop"),
      RankedChip::new(30, "f:fav"),
    ];
    let picked = pick(chips, 100, " · ");
    assert_eq!(picked, vec!["Enter:launch", "s:stop", "f:fav"]);
  }

  #[test]
  fn pick_returns_empty_when_budget_below_first_chip() {
    let chips = vec![RankedChip::new(10, "Enter:launch")];
    assert!(pick(chips, 5, " · ").is_empty());
  }

  #[test]
  fn pick_equal_ranks_fall_back_to_source_order_for_drop_decision() {
    // Two chips with rank 10. Budget fits only one; the earlier
    // source position wins (FIFO under tie).
    let chips = vec![RankedChip::new(10, "AAAA"), RankedChip::new(10, "BBBB")];
    let picked = pick(chips, 4, " · ");
    assert_eq!(picked, vec!["AAAA"]);
  }

  #[test]
  fn pick_skips_a_chip_thats_too_wide_but_keeps_subsequent_fitting_chips() {
    // Big spender (rank 20, width 30) doesn't fit; a smaller
    // rank-30 chip after it should still pass when budget allows.
    let chips = vec![
      RankedChip::new(10, "AAAA"),
      RankedChip::new(20, "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"),
      RankedChip::new(30, "CCCC"),
    ];
    let picked = pick(chips, 11, " · ");
    assert_eq!(picked, vec!["AAAA", "CCCC"]);
  }
}
