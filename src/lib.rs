//! llamadash library — TUI + CLI + daemon for managing local llama.cpp
//! servers. The binary at `src/main.rs` is a thin wrapper around the
//! modules exposed here so integration tests (in `tests/`) can drive the
//! same code paths the binary uses.

#![warn(rust_2018_idioms)]
#![deny(clippy::shadow_unrelated)]
// Crate-wide `dead_code` allow removed at Unit 9 release prep — every
// scaffold module is now consumed. If you add a new module that hasn't
// landed all of its consumers yet, narrow the allow to that item
// (`#[allow(dead_code)] fn …`) rather than re-blanketing the crate.

pub mod banner;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod discovery;
pub mod gguf;
pub mod gpu;
pub mod ipc;
pub mod launch;
pub mod theme;
pub mod tui;
pub mod util;
