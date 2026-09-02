//! The layered resolver, over [`KnobSet`].
//!
//! Same precedence chain as before — `user > default preset > last used > arch
//! defaults > built-in` — but it now runs over *every* knob a backend declares
//! rather than only the nineteen in the old llama.cpp-keyed IR. Backend-native
//! tunables gain layering, arch defaults, and source chips, none of which the
//! parallel `backend_knobs` channel had.
//!
//! Two behaviours are new:
//!
//! - **Concept carry-over.** A layer that stored a value under one backend's
//!   spelling still reaches the resolved backend when both tag the same
//!   [`Concept`](super::def::Concept). The value is re-parsed against the
//!   destination knob, so a
//!   kind mismatch is skipped rather than smuggled through.
//! - **Explicit drops.** A value the resolved backend cannot honour is
//!   reported in [`Resolved::dropped`] instead of vanishing, which is what R6
//!   asks for: dropped, logged, surfaced — never silently ignored, never a
//!   hard launch block.

use std::collections::BTreeMap;

use super::def::KnobId;
use super::registry;
use super::value::{parse_value, KnobSet, KnobValue};
use crate::config::DefaultLaunchMode;
use crate::launch::params::LayerLabel;

/// Resolver output. `knobs` is what the backend will emit; `sources` names the
/// layer each value came from so the editor can render origin chips; `dropped`
/// lists ids some layer supplied that this backend does not declare.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
  pub knobs: KnobSet,
  pub sources: BTreeMap<KnobId, LayerLabel>,
  pub dropped: Vec<KnobId>,
}

impl Resolved {
  /// Whether no layer supplied `id` — the value in `knobs` (if any) came from
  /// seeding, not from a real layer.
  ///
  /// `fallback` labels are the sentinel: no real layer ever carries
  /// `ServerDefault` / `ModelDefault`, so `source == fallback` is exact.
  pub fn is_layer_less(&self, id: KnobId, backend_id: &str) -> bool {
    let Some(def) = registry::def_for_backend(backend_id, id) else {
      return true;
    };
    self
      .sources
      .get(&id)
      .copied()
      .map(|src| src == def.fallback)
      .unwrap_or(true)
  }

  /// The subset of `sources` a real layer actually supplied — knobs that fell
  /// to their `fallback` (no layer set them) are dropped. This is what the IPC
  /// `layer_sources` response carries, so a pure-fit launch (every knob at its
  /// default) yields an empty map and the field is omitted.
  pub fn real_sources(&self, backend_id: &str) -> BTreeMap<KnobId, LayerLabel> {
    self
      .sources
      .iter()
      .filter(|(id, _)| !self.is_layer_less(**id, backend_id))
      .map(|(id, label)| (*id, *label))
      .collect()
  }
}

/// Resolve `layers` for `backend_id`, most-specific first.
///
/// Honours `LLAMASTASH_BENCH_DISABLE_DEFAULTS=1` the same way the old resolver
/// did: collapse to User-labelled layers only, so the bench harness produces
/// byte-identical argv to a raw engine invocation.
pub fn resolve_layered(backend_id: &str, layers: &[(LayerLabel, &KnobSet)]) -> Resolved {
  resolve_layered_with_disable_defaults(
    backend_id,
    layers,
    crate::launch::params::bench_disable_defaults_from_env(),
  )
}

/// Inner resolver, split out so tests exercise the bench branch without
/// mutating process environment (racy across `cargo test`'s thread pool).
pub fn resolve_layered_with_disable_defaults(
  backend_id: &str,
  layers: &[(LayerLabel, &KnobSet)],
  disable_defaults: bool,
) -> Resolved {
  let user_only: Vec<(LayerLabel, &KnobSet)>;
  let layers = if disable_defaults {
    user_only = layers
      .iter()
      .filter(|(l, _)| matches!(l, LayerLabel::User))
      .copied()
      .collect();
    &user_only[..]
  } else {
    layers
  };

  let mut knobs = KnobSet::new();
  let mut sources: BTreeMap<KnobId, LayerLabel> = BTreeMap::new();
  let defs = registry::for_backend(backend_id);

  // Seed every declared knob's source with its fallback, so a knob no layer
  // fills still reports where its value will come from.
  for def in defs {
    sources.insert(def.knob_id(), def.fallback);
  }

  for def in defs {
    let id = def.knob_id();
    for (label, layer) in layers {
      // Direct hit on this backend's own spelling. `Auto` carries only where
      // the destination declares an auto state -- the same rule `carry_over`
      // applies -- so a layer naming this id with `auto` against a knob that
      // has none falls through to the next layer rather than storing a state
      // the knob cannot mean.
      if let Some(v) = layer.get(id).filter(|v| def.has_auto() || !v.is_auto()) {
        knobs.set(id, v.clone());
        sources.insert(id, *label);
        break;
      }
      // Otherwise a sibling backend's spelling of the same concept.
      if let Some(v) = carry_over(def, layer) {
        knobs.set(id, v);
        sources.insert(id, *label);
        break;
      }
    }
  }

  // Anything a layer supplied that this backend cannot honour, in the order
  // the layers were consulted. Deduplicated: one id, one report.
  //
  // A shared concept only counts as carried when the destination knob actually
  // ended up with a value. Sharing the concept is not enough: `carry_over`
  // re-parses into the destination's kind and skips a value that kind cannot
  // hold, which used to leave the id neither set nor reported -- silently gone.
  let mut dropped = Vec::new();
  for (_, layer) in layers {
    for id in layer.ids() {
      let honoured = registry::def_for_backend(backend_id, id).is_some();
      let concept = registry::def_for(id).and_then(|s| s.concept);
      let carried = concept.is_some()
        && defs
          .iter()
          .any(|d| d.concept == concept && knobs.contains(d.knob_id()));
      if !honoured && !carried && !dropped.contains(&id) {
        dropped.push(id);
      }
    }
  }

  Resolved {
    knobs,
    sources,
    dropped,
  }
}

/// Find a value in `layer` stored under a *different* backend's spelling of
/// `def`'s concept, converted into `def`'s own kind.
///
/// The conversion goes through the same `parse_value` every other surface
/// uses, so a value that cannot be represented in the destination kind is
/// skipped rather than stored as a mistyped `Scalar`.
pub fn carry_over(def: &super::def::KnobDef, layer: &KnobSet) -> Option<KnobValue> {
  let concept = def.concept?;
  for (id, value) in layer.iter() {
    if registry::def_for(id).and_then(|d| d.concept) != Some(concept) {
      continue;
    }
    return match value {
      // Auto carries only where the destination has an Auto state at all.
      KnobValue::Auto => def.has_auto().then_some(KnobValue::Auto),
      KnobValue::Set(scalar) => parse_value(def, &scalar.to_arg()).ok(),
    };
  }
  None
}

/// Re-key a user's knob set into `backend_id`'s vocabulary, dropping what that
/// backend does not declare.
///
/// For a surface that lets the user change backend mid-edit. A value the
/// destination has no knob for would otherwise sit in the set — invisible,
/// since no row renders it, but still shipped on the wire. Shared concepts
/// survive the move (a pinned context window is still a pinned context window
/// after switching engines); the rest are honestly backend-local and go.
pub fn rescope(layer: &KnobSet, backend_id: &str) -> KnobSet {
  let mut out = KnobSet::new();
  for def in registry::for_backend(backend_id) {
    let id = def.knob_id();
    // The destination's own spelling wins; a concept sibling fills in for it.
    if let Some(v) = layer.get(id).cloned().or_else(|| carry_over(def, layer)) {
      out.set(id, v);
    }
  }
  out
}

/// Seed knobs no layer filled, per the configured default launch mode.
///
/// Under [`DefaultLaunchMode::Auto`] a layer-less knob becomes
/// [`KnobValue::Auto`] so the engine's fitter governs it. Only knobs declaring
/// [`AutoKind::Delegate`](super::def::AutoKind::Delegate) qualify: elsewhere
/// `Auto` emits nothing and is indistinguishable from unset, so seeding it
/// would render a meaningless row.
///
/// Knobs a real layer supplied are untouched — remembered values win.
pub fn seed_layerless(resolved: &mut Resolved, backend_id: &str, mode: DefaultLaunchMode) {
  if mode != DefaultLaunchMode::Auto {
    return;
  }
  for def in registry::for_backend(backend_id) {
    if !def.is_fit_delegated() {
      continue;
    }
    let id = def.knob_id();
    if resolved.is_layer_less(id, backend_id) {
      resolved.knobs.set_auto(id);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::{Backend, Backends};
  use crate::launch::knobs::value::Scalar;

  /// The default backend's id, without naming it — the registry is the
  /// authority, and this file must stay backend-neutral.
  fn default_backend() -> &'static str {
    crate::backend::DEFAULT_BACKEND_ID
  }

  /// A backend other than the default that declares a context knob under a
  /// *different* id, so the carry-over path is exercised against real data.
  fn divergent_ctx_backend() -> Option<(&'static str, KnobId, KnobId)> {
    let home = default_backend();
    let home_ctx = registry::def_for_backend_concept(home, super::super::Concept::ContextLength)?;
    Backends::all().iter().find_map(|b| {
      let id = b.id();
      if id == home {
        return None;
      }
      let ctx = registry::def_for_backend_concept(id, super::super::Concept::ContextLength)?;
      (ctx.id != home_ctx.id).then_some((id, home_ctx.knob_id(), ctx.knob_id()))
    })
  }

  fn set(id: KnobId, v: u32) -> KnobSet {
    let mut s = KnobSet::new();
    s.set_scalar(id, Scalar::U32(v));
    s
  }

  /// A bool knob the default backend declares with **no** auto state, so
  /// `Auto` is a value it cannot mean.
  fn no_auto_bool_knob(backend: &str) -> Option<KnobId> {
    registry::for_backend(backend)
      .iter()
      .find(|d| !d.has_auto() && matches!(d.kind, super::super::KnobKind::Bool))
      .map(|d| d.knob_id())
  }

  /// `(source id, destination backend, destination id)` for two backends that
  /// declare one concept under different ids where the destination has no auto
  /// state. An `Auto` carried across that pair cannot land.
  fn auto_less_concept_pair() -> Option<(KnobId, &'static str, KnobId)> {
    for src_backend in Backends::all() {
      for src in registry::for_backend(src_backend.id()) {
        let Some(concept) = src.concept else {
          continue;
        };
        for dst_backend in Backends::all() {
          if dst_backend.id() == src_backend.id() {
            continue;
          }
          let Some(dst) = registry::def_for_backend_concept(dst_backend.id(), concept) else {
            continue;
          };
          if dst.id != src.id && !dst.has_auto() {
            return Some((src.knob_id(), dst_backend.id(), dst.knob_id()));
          }
        }
      }
    }
    None
  }

  #[test]
  fn a_direct_auto_hit_falls_through_when_the_knob_has_no_auto_state() {
    let backend = default_backend();
    let id = no_auto_bool_knob(backend).expect("a bool knob with no auto state");
    let mut user = KnobSet::new();
    user.set_auto(id);
    let mut lower = KnobSet::new();
    lower.set_scalar(id, Scalar::Bool(true));

    let r = resolve_layered_with_disable_defaults(
      backend,
      &[(LayerLabel::User, &user), (LayerLabel::LastUsed, &lower)],
      false,
    );
    assert!(
      !r.knobs.is_auto(id),
      "`auto` is not a state {id:?} can hold, so it must not be stored"
    );
    assert_eq!(
      r.knobs.bool(id),
      Some(true),
      "the hit falls through to the next layer instead of shadowing it"
    );
    assert_eq!(r.sources[&id], LayerLabel::LastUsed);
  }

  #[test]
  fn a_concept_value_the_destination_cannot_hold_is_reported_dropped() {
    let (src, dst_backend, dst) =
      auto_less_concept_pair().expect("two backends sharing a concept, destination without auto");
    let mut user = KnobSet::new();
    user.set_auto(src);

    let r = resolve_layered_with_disable_defaults(dst_backend, &[(LayerLabel::User, &user)], false);
    assert!(
      !r.knobs.contains(dst),
      "{dst:?} has no auto state, so nothing should have landed"
    );
    assert!(
      r.dropped.contains(&src),
      "{src:?} went nowhere and must be surfaced, not vanish; dropped={:?}",
      r.dropped
    );
  }

  #[test]
  fn first_layer_wins_per_knob() {
    let backend = default_backend();
    let ctx = registry::def_for_backend_concept(backend, super::super::Concept::ContextLength)
      .unwrap()
      .knob_id();
    let threads = registry::def_for_backend_concept(backend, super::super::Concept::Threads)
      .unwrap()
      .knob_id();

    let user = set(ctx, 4096);
    let mut lower = set(ctx, 9999);
    lower.set_scalar(threads, Scalar::U32(8));

    let r = resolve_layered_with_disable_defaults(
      backend,
      &[(LayerLabel::User, &user), (LayerLabel::LastUsed, &lower)],
      false,
    );
    assert_eq!(r.knobs.u32(ctx), Some(4096), "user layer wins");
    assert_eq!(r.knobs.u32(threads), Some(8), "lower layer fills the gap");
    assert_eq!(r.sources[&ctx], LayerLabel::User);
    assert_eq!(r.sources[&threads], LayerLabel::LastUsed);
  }

  #[test]
  fn real_sources_drops_knobs_that_fell_to_their_fallback() {
    let backend = default_backend();
    let ctx = registry::def_for_backend_concept(backend, super::super::Concept::ContextLength)
      .unwrap()
      .knob_id();

    // Pure-fit: no layer supplies anything, so every knob sits at its fallback
    // and the provenance map is empty — the IPC `layer_sources` field is omitted.
    let empty = KnobSet::new();
    let pure = resolve_layered_with_disable_defaults(backend, &[(LayerLabel::User, &empty)], false);
    assert!(
      pure.real_sources(backend).is_empty(),
      "a pure-fit launch resolves no real layer, so layer_sources must be empty"
    );

    // One real layer: only the knob it set survives the filter.
    let user = set(ctx, 4096);
    let r = resolve_layered_with_disable_defaults(backend, &[(LayerLabel::User, &user)], false);
    let real = r.real_sources(backend);
    assert_eq!(
      real.len(),
      1,
      "only the user-set knob is a real layer: {real:?}"
    );
    assert_eq!(real[&ctx], LayerLabel::User);
  }

  #[test]
  fn bench_disable_defaults_keeps_only_user_layers() {
    let backend = default_backend();
    let ctx = registry::def_for_backend_concept(backend, super::super::Concept::ContextLength)
      .unwrap()
      .knob_id();
    let lower = set(ctx, 9999);
    let empty = KnobSet::new();
    let r = resolve_layered_with_disable_defaults(
      backend,
      &[
        (LayerLabel::User, &empty),
        (LayerLabel::ArchDefault, &lower),
      ],
      true,
    );
    assert_eq!(r.knobs.u32(ctx), None, "arch layer is skipped entirely");
  }

  #[test]
  fn a_value_carries_across_backends_by_concept() {
    let Some((other, home_ctx, other_ctx)) = divergent_ctx_backend() else {
      return; // only one ctx spelling compiled in; nothing to carry
    };
    // Stored under the default backend's spelling…
    let stored = set(home_ctx, 32768);
    // …resolved for a backend that spells it differently.
    let r = resolve_layered_with_disable_defaults(other, &[(LayerLabel::LastUsed, &stored)], false);
    assert_eq!(
      r.knobs.u32(other_ctx),
      Some(32768),
      "the context value should land on {other}'s own knob"
    );
    assert!(
      r.dropped.is_empty(),
      "a carried value is not a drop: {:?}",
      r.dropped
    );
  }

  #[test]
  fn an_unhonoured_knob_is_reported_not_silently_lost() {
    let Some((other, _, _)) = divergent_ctx_backend() else {
      return;
    };
    // A knob only the default backend declares, with no concept to carry it.
    let home = default_backend();
    let local = registry::for_backend(home)
      .iter()
      .find(|d| d.concept.is_none() && registry::def_for_backend(other, d.knob_id()).is_none());
    let Some(local) = local else { return };

    let mut stored = KnobSet::new();
    stored.set_scalar(local.knob_id(), Scalar::U32(1));
    let r = resolve_layered_with_disable_defaults(other, &[(LayerLabel::LastUsed, &stored)], false);
    assert!(
      r.dropped.contains(&local.knob_id()),
      "expected {} in dropped, got {:?}",
      local.id,
      r.dropped
    );
  }

  #[test]
  fn seeding_touches_only_fit_delegated_layer_less_knobs() {
    let backend = default_backend();
    let empty = KnobSet::new();
    let mut r =
      resolve_layered_with_disable_defaults(backend, &[(LayerLabel::User, &empty)], false);
    seed_layerless(&mut r, backend, DefaultLaunchMode::Auto);

    for def in registry::for_backend(backend) {
      let id = def.knob_id();
      if def.is_fit_delegated() {
        assert!(r.knobs.is_auto(id), "{} should be seeded Auto", def.id);
      } else {
        assert!(
          !r.knobs.contains(id),
          "{} must stay inherited, not seeded",
          def.id
        );
      }
    }
  }

  #[test]
  fn seeding_never_overwrites_a_remembered_value() {
    let backend = default_backend();
    let ctx = registry::def_for_backend_concept(backend, super::super::Concept::ContextLength)
      .unwrap()
      .knob_id();
    assert!(
      registry::def_for_backend(backend, ctx)
        .unwrap()
        .is_fit_delegated(),
      "precondition: the context knob is fit-delegated"
    );
    let remembered = set(ctx, 8192);
    let mut r =
      resolve_layered_with_disable_defaults(backend, &[(LayerLabel::LastUsed, &remembered)], false);
    seed_layerless(&mut r, backend, DefaultLaunchMode::Auto);
    assert_eq!(r.knobs.u32(ctx), Some(8192), "remembered values win");
  }

  #[test]
  fn inherited_mode_seeds_nothing() {
    let backend = default_backend();
    let empty = KnobSet::new();
    let mut r =
      resolve_layered_with_disable_defaults(backend, &[(LayerLabel::User, &empty)], false);
    seed_layerless(&mut r, backend, DefaultLaunchMode::Inherited);
    assert!(r.knobs.is_empty(), "Inherited mode leaves every knob unset");
  }
}
