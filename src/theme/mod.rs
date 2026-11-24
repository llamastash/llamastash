//! Built-in colour palettes for the TUI.
//!
//! Themes are static, compile-time constants resolved by name from the
//! user's config. Adding a new theme means adding a sibling module here
//! and a match arm in `palette::palette_for`.

mod gruvbox;
mod latte;
mod macchiato;
mod mono;
mod solarized;

pub mod custom;
pub mod palette;

pub use custom::CustomThemeConfig;
pub use palette::{palette_for, Palette, ThemeName, UnknownThemeError};
