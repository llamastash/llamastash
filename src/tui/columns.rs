//! Fixed-width table columns that drop out by rank as a pane narrows.
//! Shared by the Models list and the Requests tab.

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// One data column. `rank` decides which columns survive under width
/// pressure (lower = stickier — see [`fit`]); the order columns are
/// declared in is the left-to-right display order, so visible columns
/// keep their familiar positions as the terminal resizes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Column<Id> {
  pub id: Id,
  pub label: &'static str,
  pub width: usize,
  pub rank: u8,
}

/// The columns of `candidates` that fit in `budget` cells, in the order
/// they were given, and the cells they take. Each column costs its width
/// plus `sep_w`. Lower-rank columns win first.
pub(crate) fn fit<'a, Id>(
  candidates: impl IntoIterator<Item = &'a Column<Id>>,
  budget: usize,
  sep_w: usize,
) -> (Vec<&'a Column<Id>>, usize) {
  let mut by_rank: Vec<(usize, &'a Column<Id>)> = candidates.into_iter().enumerate().collect();
  by_rank.sort_by_key(|(_, c)| c.rank);

  let mut taken: Vec<(usize, &'a Column<Id>)> = Vec::with_capacity(by_rank.len());
  let mut spent = 0usize;
  // Strict rank-tail drop: once a lower-rank column refuses to fit,
  // stop trying to admit any higher-rank columns. The alternative
  // (greedy: skip the big one, keep checking smaller ones) gives a
  // tighter information density but produces non-contiguous
  // visibility — a Port column slotting in where Arch can't makes
  // it look like the data jumped a slot as the pane resizes. The
  // cutoff is what users intuit from "rank = min-width threshold".
  for (idx, c) in by_rank {
    let cost = c.width + sep_w;
    if spent + cost > budget {
      break;
    }
    spent += cost;
    taken.push((idx, c));
  }
  // Restore declaration order so columns disappear from less-
  // important slots while the survivors keep their familiar
  // positions on screen.
  taken.sort_by_key(|(idx, _)| *idx);
  (taken.into_iter().map(|(_, c)| c).collect(), spent)
}

/// Left-aligned pad/truncate to `w` display columns. Truncated
/// strings end with `…` so overflow is visible.
pub(crate) fn cell(s: &str, w: usize) -> String {
  if w == 0 {
    return String::new();
  }
  let used = s.width();
  if used <= w {
    return pad(s, w, used);
  }
  let ellipsis = crate::tui::glyphs::active().ellipsis();
  let budget = w.saturating_sub(ellipsis.width());
  let mut cut = String::with_capacity(w * 3);
  let mut cut_w = 0usize;
  for ch in s.chars() {
    let cells = ch.width().unwrap_or(0);
    if cut_w + cells > budget {
      break;
    }
    cut.push(ch);
    cut_w += cells;
  }
  let mut out = cut;
  out.push_str(ellipsis);
  let out_w = cut_w + ellipsis.width();
  if out_w >= w {
    return out;
  }
  pad(&out, w, out_w)
}

/// `s` plus the spaces that bring it to `w` display cells, given it
/// already measures `used` of them.
fn pad(s: &str, w: usize, used: usize) -> String {
  let mut out = String::with_capacity(s.len() + w);
  out.push_str(s);
  for _ in used..w {
    out.push(' ');
  }
  out
}

/// Like [`cell`], but a cut lands on a word boundary where one falls inside
/// the limit, so free prose reads as its first words rather than a mid-word
/// fragment. Only falls back to a mid-word cut when no whole word fits.
pub(crate) fn cell_words(s: &str, w: usize) -> String {
  if w == 0 {
    return String::new();
  }
  if s.width() <= w {
    return cell(s, w);
  }
  let ellipsis = crate::tui::glyphs::active().ellipsis();
  let budget = w.saturating_sub(ellipsis.width());
  let mut kept = String::new();
  let mut used = 0usize;
  for word in s.split_whitespace() {
    let gap = usize::from(!kept.is_empty());
    let cells = word.width();
    if used + gap + cells > budget {
      break;
    }
    if gap > 0 {
      kept.push(' ');
    }
    kept.push_str(word);
    used += gap + cells;
  }
  if kept.is_empty() {
    let mut hard = String::new();
    let mut hard_w = 0usize;
    for ch in s.chars() {
      let cells = ch.width().unwrap_or(0);
      if hard_w + cells > budget {
        break;
      }
      hard.push(ch);
      hard_w += cells;
    }
    kept = hard;
  }
  cell(&format!("{kept}{ellipsis}"), w)
}

#[cfg(test)]
mod tests {
  use super::*;

  const COLS: &[Column<char>] = &[
    Column {
      id: 'a',
      label: "A",
      width: 4,
      rank: 30,
    },
    Column {
      id: 'b',
      label: "B",
      width: 4,
      rank: 10,
    },
    Column {
      id: 'c',
      label: "C",
      width: 2,
      rank: 20,
    },
  ];

  fn ids(budget: usize) -> (Vec<char>, usize) {
    let (cols, spent) = fit(COLS, budget, 1);
    (cols.iter().map(|c| c.id).collect(), spent)
  }

  #[test]
  fn a_wide_glyph_costs_two_cells_not_one() {
    // "モデル失敗" is 5 characters and 10 display cells.
    assert_eq!(cell("モデル失敗", 12), "モデル失敗  ");
    assert_eq!(cell("モデル失敗", 10), "モデル失敗");
    assert_eq!(cell("モデル失敗", 9), "モデル失…");
    assert_eq!(cell("モデル失敗", 11), "モデル失敗 ");
    for w in [8usize, 9, 10, 11, 12, 20] {
      assert_eq!(cell("モデル失敗", w).width(), w, "width {w}");
      assert_eq!(cell_words("モデル失敗", w).width(), w, "width {w}");
    }
    assert_eq!(cell_words("モデル失敗", 12), "モデル失敗  ");
    assert_eq!(cell_words("モデル失敗", 9), "モデル失…");
    // A combining mark adds a character and no cell.
    assert_eq!(cell("e\u{301} ok", 7), "e\u{301} ok   ");
    assert_eq!(cell("e\u{301} ok", 7).width(), 7);
  }

  #[test]
  fn cell_words_cuts_at_a_word_boundary() {
    let ell = crate::tui::glyphs::active().ellipsis();
    assert_eq!(cell_words("in flight", 12).trim_end(), "in flight");
    assert_eq!(
      cell_words("auto-start, unloaded L3", 16).trim_end(),
      format!("auto-start,{ell}")
    );
    assert_eq!(
      cell_words("unloaded L3 L4 L5", 16).trim_end(),
      format!("unloaded L3 L4{ell}")
    );
    // No whole word fits, so it cuts mid-token.
    assert_eq!(
      cell_words("supercalifragilistic expialidocious", 12).trim_end(),
      format!("supercalifr{ell}")
    );
    assert_eq!(cell_words("anything at all", 0), "");
  }

  #[test]
  fn lower_rank_survives_and_display_order_is_kept() {
    assert_eq!(ids(100), (vec!['a', 'b', 'c'], 13));
    assert_eq!(ids(8), (vec!['b', 'c'], 8));
    assert_eq!(ids(5), (vec!['b'], 5));
    assert_eq!(ids(4), (vec![], 0));
  }

  #[test]
  fn a_column_that_does_not_fit_ends_the_pick() {
    // `b` (rank 10) costs 5 and fits. `c` (rank 20) costs 3 and does
    // not fit in the 2 cells left, so the pick stops there.
    assert_eq!(ids(7), (vec!['b'], 5));
    // With 12 cells `b` and `c` fit and `a` (rank 30, cost 5) does not.
    assert_eq!(ids(12), (vec!['b', 'c'], 8));
  }

  #[test]
  fn cell_pads_and_truncates() {
    assert_eq!(cell("ab", 4), "ab  ");
    assert_eq!(cell("abcdef", 4).chars().count(), 4);
    assert_eq!(cell("abc", 0), "");
  }
}
