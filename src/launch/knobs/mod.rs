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
//! This replaces the split between the llama.cpp-keyed `crate::launch::knobs::KnobSet` IR and the
//! stringly-typed `native_knobs` channel. The IR carried one usable slot out of
//! nineteen for three of four backends while their real tunables sat in a
//! parallel channel with no layering, no arch defaults and no CLI surface.
//!
//! - [`def`] — what a backend declares ([`KnobDef`], [`KnobKind`], [`Concept`]).
//! - [`value`] — what a launch carries ([`KnobSet`], [`KnobValue`], [`Scalar`]).
//! - [`emit`] — one generic argv emitter, replacing the per-backend translators.
//! - [`registry`] — the union across backends, id resolution, and validation.
//! - [`resolve`] — the layered precedence chain, now over every declared knob.
//! - [`serde_impl`] — the flat `id: value` shape used by config, state, and wire.
//!
//! Values are keyed by the backend's own flag spelling, so one string serves
//! the YAML key, the CLI flag, and the engine's own `--help`. Genuinely shared
//! ideas additionally carry a [`Concept`], which is what lets a value follow
//! the user across a backend switch and gives scripts one neutral spelling.

pub mod def;
pub mod emit;
pub mod registry;
pub mod resolve;
pub mod serde_impl;
pub mod value;

pub use def::{AutoKind, Concept, Emit, Group, GroupGate, KnobDef, KnobId, KnobKind, Ring, Shape};

/// Resolve a knob name to its id, panicking when no backend declares it.
///
/// For call sites that name a knob they know exists — tests, and code keyed to
/// a specific declared knob. Use [`resolve_id`] where the name comes from a
/// user and an unknown one should warn rather than abort.
pub fn kid(name: &str) -> KnobId {
  resolve_id(name).unwrap_or_else(|| panic!("no knob declared for `{name}`"))
}
pub use emit::emit_argv;
pub use registry::{
  def_for, def_for_backend, def_for_backend_concept, distinct_ids, for_backend, resolve_id,
  resolve_id_for, volatile_ids, RegistryError,
};
pub use resolve::{resolve_layered, seed_layerless, Resolved};
pub(crate) use value::parse_bool;
pub use value::{parse_value, KnobSet, KnobValue, ParseError, Scalar, AUTO_TOKEN};

/// Build a [`KnobSet`] from `name: value` pairs, the way a struct literal used
/// to read.
///
/// Names take either spelling (`flash_attn` or `flash-attn`); the bare token
/// `auto` sets the knob's Auto state. Values are typed through the knob's own
/// declaration, so a wrong type is a warning rather than a mistyped store.
///
/// ```ignore
/// let k = knobset! { ctx_size: 4096, flash_attn: true, n_gpu_layers: auto };
/// ```
#[macro_export]
macro_rules! knobset {
  () => { $crate::launch::knobs::KnobSet::new() };
  ($($rest:tt)+) => {{
    #[allow(unused_mut)]
    let mut set = $crate::launch::knobs::KnobSet::new();
    $crate::knobset_entries!(set, $($rest)+);
    set
  }};
}

/// Incremental muncher behind [`knobset!`].
///
/// A token muncher rather than one repetition because the two entry forms
/// differ in kind: `auto` is a bare keyword the macro must recognise
/// *literally*, while every other value is an arbitrary expression
/// (`"q8_0".into()`, `1.0`, `n + 1`). A single `$value:expr` capture would
/// swallow `auto` as a path expression and lose the distinction.
#[doc(hidden)]
#[macro_export]
macro_rules! knobset_entries {
  ($set:ident,) => {};
  ($set:ident, $name:ident : auto) => {
    $crate::knobset_entries!($set, $name: auto,);
  };
  ($set:ident, $name:ident : auto, $($rest:tt)*) => {
    if let Some(id) = $crate::launch::knobs::resolve_id(stringify!($name)) {
      $set.set_auto(id);
    }
    $crate::knobset_entries!($set, $($rest)*);
  };
  ($set:ident, $name:ident : $value:expr) => {
    $crate::knobset_entries!($set, $name: $value,);
  };
  ($set:ident, $name:ident : $value:expr, $($rest:tt)*) => {
    $set.set_by_name(stringify!($name), $value.to_string());
    $crate::knobset_entries!($set, $($rest)*);
  };
}

#[cfg(test)]
mod macro_check {
  #[test]
  fn knobset_macro_builds_typed_values() {
    let k = crate::knobset! { ctx_size: 4096, flash_attn: true, n_gpu_layers: auto };
    let id = |n| crate::launch::knobs::resolve_id(n).unwrap();
    assert_eq!(k.u32(id("ctx-size")), Some(4096));
    assert_eq!(k.bool(id("flash-attn")), Some(true));
    assert!(k.is_auto(id("n-gpu-layers")));
    assert!(crate::knobset!().is_empty());
  }
}
