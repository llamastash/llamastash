//! TUI shell (Unit 6).
//!
//! Module layout mirrors the plan's file list:
//! - [`app`] — pure App state machine.
//! - [`events`] — input → state pump and the async run loop.
//! - [`render`] — single-frame composition of every panel.
//! - [`layout`], [`list_pane`], [`help_bar`], [`filter`],
//!   [`launch_picker`], [`advanced_panel`], [`status_icons`],
//!   [`keybindings`] — focused widgets and helpers.

pub mod advanced_panel;
pub mod app;
pub mod events;
pub mod filter;
pub mod fmt;
pub mod help_bar;
pub mod host_stats_pane;
pub mod info_pane;
pub mod keybindings;
pub mod launch_picker;
pub mod layout;
pub mod list_pane;
pub mod logo_pane;
pub mod oai_client;
pub mod render;
pub mod right_pane;
pub mod status_icons;
pub mod tabs;

pub use app::{App, AppOptions, ManagedRow};
pub use events::{launch, refresh_apply, RefreshTick};
pub use tabs::RightTab;
