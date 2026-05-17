//! Built-in colour palettes for the TUI.
//!
//! Themes are static, compile-time constants resolved by name from the
//! user's config (or a runtime hotkey, later). Adding a new theme means
//! adding a sibling module here and a match arm in `palette::palette_for`.

mod gruvbox;
mod latte;
mod macchiato;
mod mono;
mod solarized;

pub mod custom;
pub mod palette;

// Public surface ready for the TUI renderer (Unit 6). Quiet the dead-code
// re-export warning until consumers land.
#[allow(unused_imports)]
pub use custom::CustomThemeConfig;
#[allow(unused_imports)]
pub use palette::{palette_for, Palette, ThemeName, UnknownThemeError};
