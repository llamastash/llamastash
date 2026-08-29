//! Launch-mode enum shared by the CLI args layer, the supervisor, and
//! the IPC protocol. The clap value-enum (`crate::cli::cli_args::LaunchMode`)
//! parses `--mode` into its own type and converts *into* this one at the
//! CLI boundary, so non-CLI consumers (supervisor, params composer) never
//! pull `clap` into their dep graph and `launch` never depends "up" on
//! `cli`.

use crate::gguf::metadata::ModeHint;
use serde::{Deserialize, Serialize};

/// Concrete launch mode chosen for a model. Distinct from
/// [`ModeHint`] (which describes what discovery *thinks* the model is)
/// — the user can override, and the supervisor records the resolved
/// choice for IPC and `state.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
  Chat,
  Embedding,
  Rerank,
}

impl LaunchMode {
  /// Parse a mode back from its [`Self::label`]. The inverse used when a
  /// stored knob value is projected onto the typed `LaunchParams.mode`.
  pub fn from_label(s: &str) -> Option<Self> {
    match s {
      "chat" => Some(LaunchMode::Chat),
      "embedding" => Some(LaunchMode::Embedding),
      "rerank" => Some(LaunchMode::Rerank),
      _ => None,
    }
  }

  pub fn label(&self) -> &'static str {
    match self {
      LaunchMode::Chat => "chat",
      LaunchMode::Embedding => "embedding",
      LaunchMode::Rerank => "rerank",
    }
  }

  /// Resolve the launch mode from an optional user-supplied override
  /// (CLI `--mode`, already converted to the domain enum) plus the GGUF
  /// discovery hint. Contract: when the override is `None` and the hint
  /// is `Unknown`, callers must error out rather than silently default
  /// to `Chat` — see `cli_args.rs::StartArgs::mode` comment.
  pub fn resolve(override_mode: Option<LaunchMode>, hint: ModeHint) -> Option<LaunchMode> {
    if let Some(m) = override_mode {
      return Some(m);
    }
    match hint {
      ModeHint::Chat => Some(LaunchMode::Chat),
      ModeHint::Embedding => Some(LaunchMode::Embedding),
      ModeHint::Rerank => Some(LaunchMode::Rerank),
      ModeHint::Unknown => None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn override_wins_over_hint() {
    let m = LaunchMode::resolve(Some(LaunchMode::Embedding), ModeHint::Chat);
    assert_eq!(m, Some(LaunchMode::Embedding));
  }

  #[test]
  fn hint_used_when_no_override() {
    assert_eq!(
      LaunchMode::resolve(None, ModeHint::Rerank),
      Some(LaunchMode::Rerank)
    );
    assert_eq!(
      LaunchMode::resolve(None, ModeHint::Embedding),
      Some(LaunchMode::Embedding)
    );
  }

  #[test]
  fn unknown_hint_with_no_override_returns_none() {
    assert!(LaunchMode::resolve(None, ModeHint::Unknown).is_none());
  }

  #[test]
  fn json_round_trips_lowercase() {
    let v = serde_json::to_string(&LaunchMode::Embedding).unwrap();
    assert_eq!(v, "\"embedding\"");
    let back: LaunchMode = serde_json::from_str(&v).unwrap();
    assert_eq!(back, LaunchMode::Embedding);
  }
}
