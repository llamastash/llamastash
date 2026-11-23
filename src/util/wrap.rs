//! Splitting text to fit a fixed number of display cells.

use unicode_width::UnicodeWidthChar as _;
use unicode_width::UnicodeWidthStr as _;

/// Break `text` into pieces of at most `w` cells, on a space where one
/// falls inside the limit and mid-word otherwise. Returns the text whole
/// when it already fits, and unchanged for `w` of 0, so a caller whose
/// column is hidden pays nothing.
///
/// `w` counts display cells, not characters, so a CJK glyph or an emoji
/// takes two: the same measure the tables pad their columns with.
pub fn wrap_cells(text: &str, w: usize) -> Vec<String> {
  if w == 0 || text.width() <= w {
    return vec![text.to_string()];
  }
  let mut out: Vec<String> = Vec::new();
  let mut line = String::new();
  let mut used = 0usize;
  for word in text.split_whitespace() {
    let len = word.width();
    if len > w {
      // A single token wider than the column (a path, an engine cause).
      if !line.is_empty() {
        out.push(std::mem::take(&mut line));
      }
      let mut chunk = String::new();
      let mut chunk_w = 0usize;
      for ch in word.chars() {
        let cells = ch.width().unwrap_or(0);
        if !chunk.is_empty() && chunk_w + cells > w {
          out.push(std::mem::take(&mut chunk));
          chunk_w = 0;
        }
        chunk.push(ch);
        chunk_w += cells;
      }
      // The tail that did not fill a chunk carries the next words.
      line = chunk;
      used = chunk_w;
      continue;
    }
    let gap = if line.is_empty() { 0 } else { 1 };
    if used + gap + len > w {
      out.push(std::mem::take(&mut line));
      used = 0;
    }
    if used > 0 {
      line.push(' ');
      used += 1;
    }
    line.push_str(word);
    used += len;
  }
  if !line.is_empty() || out.is_empty() {
    out.push(line);
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use unicode_width::UnicodeWidthStr;

  #[test]
  fn breaks_on_spaces_and_hard_breaks_a_long_token() {
    assert_eq!(wrap_cells("short", 10), vec!["short"]);
    assert_eq!(wrap_cells("a b c", 0), vec!["a b c"]);
    assert_eq!(wrap_cells("", 10), vec![""]);
    assert_eq!(wrap_cells("one two three", 9), vec!["one two", "three"]);
    assert_eq!(wrap_cells("abcdefghij k", 4), vec!["abcd", "efgh", "ij k"]);
    let long = "launch_failed: auto-start of a very long model name that will not fit";
    for chunk in wrap_cells(long, 12) {
      assert!(chunk.width() <= 12, "{chunk:?}");
    }
    // Nothing is dropped: at a width no token needs splitting, the chunks
    // are the words in order.
    assert_eq!(wrap_cells(long, 20).join(" "), long);
  }

  #[test]
  fn a_wide_glyph_costs_two_cells_not_one() {
    // The two tables measure their columns in display cells, so a chunk has
    // to fit in cells too: five of these glyphs fill a 10-cell column.
    let note = "モデルの自動起動に失 cache full";
    assert_eq!(
      wrap_cells(note, 10),
      vec!["モデルの自", "動起動に失", "cache full"]
    );
    // A mix of wide and narrow text, including an emoji.
    let mixed = "launch_failed 🚫 KV キャッシュ that does not fit 失敗";
    for chunk in wrap_cells(mixed, 12) {
      assert!(chunk.width() <= 12, "{chunk:?}");
    }
    // Nothing is dropped or reordered.
    assert_eq!(wrap_cells(mixed, 60), vec![mixed]);
  }
}
