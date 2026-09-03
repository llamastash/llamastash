//! Minimal `*` / `?` matching for user-authored config keys.
//!
//! A `presets:` key is a model name, not a shell word, so this is
//! deliberately smaller than a real glob: no character classes, no brace
//! expansion, no escaping. `*` spans any run of characters including `/`,
//! because the useful pattern is `unsloth/*` — "every model in that repo".

/// `true` when `pattern` carries a wildcard metacharacter, i.e. it should
/// be matched with [`matches()`] rather than compared verbatim.
pub fn is_pattern(pattern: &str) -> bool {
  pattern.contains(['*', '?'])
}

/// Case-insensitive whole-string match of `text` against `pattern`.
/// `*` spans zero or more characters, `?` exactly one.
pub fn matches(pattern: &str, text: &str) -> bool {
  let p: Vec<char> = pattern.to_lowercase().chars().collect();
  let t: Vec<char> = text.to_lowercase().chars().collect();
  let (mut pi, mut ti) = (0usize, 0usize);
  // Where to resume when a literal run after the most recent `*` fails:
  // give that `*` one more character and retry. Linear-time backtracking,
  // no recursion, so a pathological pattern can't blow the stack.
  let mut star: Option<(usize, usize)> = None;
  while ti < t.len() {
    if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
      pi += 1;
      ti += 1;
    } else if pi < p.len() && p[pi] == '*' {
      star = Some((pi, ti));
      pi += 1;
    } else if let Some((sp, st)) = star {
      pi = sp + 1;
      ti = st + 1;
      star = Some((sp, st + 1));
    } else {
      return false;
    }
  }
  p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn is_pattern_only_fires_on_wildcards() {
    assert!(is_pattern("Qwen3.8-*"));
    assert!(is_pattern("gemma-4-E?B-it-Q4_K_M.gguf"));
    assert!(!is_pattern("Qwen3.8-27B-UD-Q4_K_XL.gguf"));
    assert!(!is_pattern("/m/x.gguf"));
  }

  #[test]
  fn star_spans_anything_including_path_separators() {
    assert!(matches(
      "unsloth/*",
      "unsloth/Qwen3.8-27B-GGUF/Qwen3.8-27B-Q4_K_M"
    ));
    assert!(matches("*Q4_K_M*", "Qwen3.8-27B-Q4_K_M.gguf"));
    assert!(matches("*", "anything"));
    assert!(matches("*", ""));
  }

  #[test]
  fn question_mark_is_exactly_one_character() {
    assert!(matches("gemma-4-E?B-it", "gemma-4-E2B-it"));
    assert!(!matches("gemma-4-E?B-it", "gemma-4-E27B-it"));
    assert!(!matches("gemma-4-E?B-it", "gemma-4-EB-it"));
  }

  #[test]
  fn matching_is_case_insensitive_and_anchored_at_both_ends() {
    assert!(matches("QWEN3.8-*", "qwen3.8-27b.gguf"));
    // Anchored: a bare literal must consume the whole string.
    assert!(!matches("qwen3.8", "qwen3.8-27b.gguf"));
    assert!(!matches("*-27b", "qwen3.8-27b.gguf"));
  }

  #[test]
  fn a_run_of_stars_backtracks_without_false_negatives() {
    // The classic backtracking trap: several `*` before a literal tail that
    // only matches at the very end.
    assert!(matches("*a*b*c", "xxaxxbxxc"));
    assert!(!matches("*a*b*c", "xxaxxbxxcx"));
    assert!(matches("**", "ab"));
  }

  #[test]
  fn a_literal_pattern_still_matches_exactly() {
    assert!(matches("x.gguf", "X.GGUF"));
    assert!(!matches("x.gguf", "xy.gguf"));
  }
}
