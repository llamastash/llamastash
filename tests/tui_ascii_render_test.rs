//! End-to-end ASCII glyph-fallback render test.
//!
//! Runs the real binary's `--render` snapshot in both glyph modes and
//! asserts the contract:
//!  - `LLAMASTASH_ASCII=1` drops the geometric / box-drawing house
//!    style (status dots, severity triangles, gauge blocks, the logo
//!    banner, box borders, the middot / divider chrome) to 7-bit ASCII.
//!  - the default render keeps the Unicode house style unchanged.
//!
//! The only non-ASCII the ASCII frame may still carry are the
//! keyboard-symbol labels (`↑ ↓ ← → ⏎ ⇧ ↹`), which belong to the
//! keybinding-label source-of-truth system and are documented as
//! present in every monospace terminal font.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

/// Key-symbol labels that ride the keybinding-label system, not the
/// glyph house style. Allowed to remain in the ASCII frame.
const KEY_LABEL_GLYPHS: &[char] = &['↑', '↓', '←', '→', '⏎', '⇧', '↹', '⇥', '⌃', '⌥', '⌘'];

fn render(ascii: bool, size: &str, state: &PathBuf) -> String {
  let exe = PathBuf::from(env!("CARGO_BIN_EXE_llamastash"));
  let mut cmd = Command::new(exe);
  cmd
    .arg("--render")
    .arg("--render-size")
    .arg(size)
    // Daemon-less, fully path-isolated so the test never touches the
    // user's real daemon / config / cache.
    .env("LLAMASTASH_STATE_DIR", state)
    .env("LLAMASTASH_CONFIG_DIR", state)
    .env("LLAMASTASH_CACHE_DIR", state)
    .env("HF_HOME", state)
    .env("LLAMASTASH_NO_SCAN", "1")
    .env("NO_COLOR", "1");
  if ascii {
    cmd.env("LLAMASTASH_ASCII", "1");
  }
  let out = cmd.output().expect("run --render");
  String::from_utf8_lossy(&out.stdout).into_owned()
}

fn non_ascii_chars(s: &str) -> HashSet<char> {
  s.chars().filter(|c| !c.is_ascii()).collect()
}

#[test]
fn ascii_mode_render_carries_no_glyph_house_style() {
  let state = std::env::temp_dir().join(format!("ls-ascii-render-{}", std::process::id()));
  std::fs::create_dir_all(&state).expect("temp state dir");

  for size in ["120x40", "160x45", "80x24"] {
    let frame = render(true, size, &state);
    let residue = non_ascii_chars(&frame);
    let stray: Vec<char> = residue
      .iter()
      .copied()
      .filter(|c| !KEY_LABEL_GLYPHS.contains(c))
      .collect();
    assert!(
      stray.is_empty(),
      "ASCII render at {size} must drop the glyph house style; stray non-ASCII: {stray:?}"
    );
  }

  let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn default_mode_render_keeps_unicode_house_style() {
  let state = std::env::temp_dir().join(format!("ls-unicode-render-{}", std::process::id()));
  std::fs::create_dir_all(&state).expect("temp state dir");

  // 140 cols so the logo banner renders (its block glyphs are a strong
  // signal ASCII mode would strip).
  let frame = render(false, "140x40", &state);
  let residue = non_ascii_chars(&frame);
  // The default frame must still carry the geometric house style — the
  // ready dot, a box-drawing corner, and a banner block are a
  // representative sample that ASCII mode would have stripped.
  for needed in ['●', '┌', '█'] {
    assert!(
      residue.contains(&needed),
      "default render must keep the Unicode house style ({needed:?} missing)"
    );
  }

  let _ = std::fs::remove_dir_all(&state);
}
