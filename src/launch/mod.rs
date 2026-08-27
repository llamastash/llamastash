//! Launch surface: everything the supervisor needs to spawn
//! and parameterise a `llama-server` child.
//!
//! - [`binary`] — locate the `llama-server` executable on disk.
//! - [`params`] — the neutral launch IR (`LaunchParams`, typed knobs, the
//!   layered resolver). Per-backend argv emission lives with each backend.
//! - [`knobs`] — the unified knob registry: every backend declares its own
//!   tunables, and the CLI / TUI / preset surfaces are generated from them.
//! - [`mode`] — `LaunchMode` (chat/embedding/rerank) and helpers.
//! - [`presets`] / [`favorites`] — types persisted in
//!   [`crate::daemon::state_store`].

pub mod admission;
pub mod binary;
pub mod defaults_table;
pub mod favorites;
pub mod flag_aliases;
pub mod headroom;
pub mod knobs;
pub mod mode;
pub mod native_knobs;
pub mod params;
pub mod presets;
pub mod resolve;

pub use binary::{locate as locate_binary, LocateError, LocateInputs};
pub use defaults_table::lookup as lookup_defaults;
pub use favorites::{FavoriteEntry, Favorites};
pub use mode::LaunchMode;
pub use params::{LaunchParams, LayerLabel};
pub use presets::{NamedPreset, Presets};
