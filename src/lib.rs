//! llamastash library — TUI + CLI + daemon for orchestrating local LLM
//! servers. The binary at `src/main.rs` is a thin wrapper around the
//! modules exposed here so integration tests (in `tests/`) can drive the
//! same code paths the binary uses.

#![warn(rust_2018_idioms)]
#![deny(clippy::shadow_unrelated)]
// If you add a new module that hasn't landed all of its
// consumers yet, narrow the allow to that item
// (`#[allow(dead_code)] fn …`) rather than re-blanketing the crate.

pub mod backend;
pub mod banner;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod discovery;
pub mod gguf;
pub mod gpu;
pub mod init;
pub mod ipc;
pub mod launch;
pub mod proxy;
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub mod test_support;
pub mod theme;
pub mod tui;
pub mod util;
