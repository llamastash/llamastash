//! Unified knob registry — every backend declares its own tunables, and every
//! surface is generated from those declarations.
//!
//! **The parity contract.** A knob exists in exactly one place: a
//! [`KnobDef`] in `src/backend/<id>/knobs.rs`. From that one declaration the
//! CLI derives a flag, the TUI derives a row, and the preset / persistence
//! layer derives a key. No surface hand-wires a knob, so none can be missing
//! one. A 2026-08-25 audit found the inverse held perfectly under the old
//! two-channel model: every setting off the generated path had a parity gap on
//! at least one surface, and every setting on it had none.
//!
//! This replaces the split between the llama.cpp-keyed `TypedKnobs` IR and the
//! stringly-typed `native_knobs` channel. The IR carried one usable slot out of
//! nineteen for three of four backends while their real tunables sat in a
//! parallel channel with no layering, no arch defaults and no CLI surface.
//!
//! - [`def`] — what a backend declares ([`KnobDef`], [`KnobKind`], [`Concept`]).
//! - [`value`] — what a launch carries ([`KnobSet`], [`KnobValue`], [`Scalar`]).
//! - [`registry`] — the union across backends, id resolution, and validation.
//!
//! Values are keyed by the backend's own flag spelling, so one string serves
//! the YAML key, the CLI flag, and the engine's own `--help`. Genuinely shared
//! ideas additionally carry a [`Concept`], which is what lets a value follow
//! the user across a backend switch and gives scripts one neutral spelling.

pub mod def;
pub mod registry;
pub mod value;

pub use def::{AutoKind, Concept, Emit, Group, KnobDef, KnobId, KnobKind};
pub use registry::{
  def_for, def_for_backend, def_for_backend_concept, distinct_ids, for_backend, resolve_id,
  RegistryError,
};
pub use value::{parse_value, KnobSet, KnobValue, ParseError, Scalar, AUTO_TOKEN};
