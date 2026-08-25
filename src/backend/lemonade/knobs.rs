//! Lemonade's declared knobs.
//!
//! The narrowest declaration, and the one that proves the descriptor shape is
//! not argv-shaped: Lemonade does not spawn a per-model process. It loads over
//! HTTP (`POST /api/v1/load`), where the context window is the `ctx_size`
//! *field* of a JSON body, not a command-line flag.
//!
//! So `Emit::Custom` here is not a special case bolted on — it is the reason
//! `KnobDef.flag` is documented as "the backend's own name for this setting"
//! rather than "the flag". The umbrella builds the load request in its own
//! `prepare_launch` and reads the value straight off the [`KnobSet`].
//!
//! [`KnobSet`]: crate::launch::knobs::KnobSet

use crate::launch::knobs::{AutoKind, Concept, Emit, Group, KnobDef, KnobKind};
use crate::launch::params::LayerLabel;

pub const KNOBS: &[KnobDef] = &[KnobDef {
  id: "ctx-size",
  flag: None,
  concept: Some(Concept::ContextLength),
  kind: KnobKind::U32 {
    max: Some(crate::config::MAX_CTX_TOKENS),
  },
  auto: Some(AutoKind::Delegate),
  group: Group::Context,
  label: "Context",
  help: "context length in tokens (the load request's `ctx_size`)",
  aliases: &["-c", "ctx"],
  fallback: LayerLabel::ModelDefault,
  emit: Emit::Custom,
}];
