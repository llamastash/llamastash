//! The registry — the union of every backend's declared knobs, and the lookups
//! the generated surfaces run against it.
//!
//! Registry-driven throughout: this module names no backend. It walks
//! [`Backends::all`](crate::backend::Backends::all) and asks each for its
//! declarations, so adding a backend adds its knobs to every surface with no
//! edit here.

use std::collections::BTreeMap;

use super::def::{Concept, Group, KnobDef, KnobId};
use crate::backend::{Backend, Backends};

/// Every knob every compiled-in backend declares, in a stable order:
/// backend-registry order, then each backend's own declaration order.
///
/// Built once on first use. The union is what the CLI generates flags from —
/// a backend the user has not installed still contributes its flags, because
/// the flag set must not depend on host state (a script written on one machine
/// has to parse on another).
fn all_defs() -> &'static [(&'static str, &'static KnobDef)] {
  use std::sync::OnceLock;
  static DEFS: OnceLock<Vec<(&'static str, &'static KnobDef)>> = OnceLock::new();
  DEFS.get_or_init(|| {
    let mut out = Vec::new();
    for backend in Backends::all() {
      let id = backend.id();
      for def in backend.knobs() {
        out.push((id, def));
      }
    }
    out
  })
}

/// Iterate `(backend_id, def)` across the whole registry.
pub fn iter() -> impl Iterator<Item = (&'static str, &'static KnobDef)> {
  all_defs().iter().copied()
}

/// The knobs one backend declares, by backend id. Empty for an unknown id.
pub fn for_backend(backend_id: &str) -> &'static [KnobDef] {
  Backends::all()
    .iter()
    .find(|b| b.id() == backend_id)
    .map(|b| b.knobs())
    .unwrap_or(&[])
}

/// Distinct knob ids across the whole registry, deduplicated, in registry
/// order. Two backends declaring the same id share one entry — that is the
/// mechanism by which `--threads` means one thing on the CLI regardless of
/// which backend ends up serving.
pub fn distinct_ids() -> Vec<KnobId> {
  let mut seen = std::collections::BTreeSet::new();
  let mut out = Vec::new();
  for (_, def) in iter() {
    let id = def.knob_id();
    if seen.insert(id) {
      out.push(id);
    }
  }
  out
}

/// Normalise a user-supplied key to the registry's spelling.
///
/// Underscores and dashes are interchangeable, so a config written as
/// `flash_attn` and one written as `flash-attn` both resolve. This is key
/// normalisation, not a compatibility shim: the two spellings are the same
/// name, and users type whichever their editor's neighbours use.
fn normalise(key: &str) -> String {
  key
    .trim()
    .trim_start_matches('-')
    .to_ascii_lowercase()
    .replace('_', "-")
}

/// Resolve a config / wire / CLI key to a registry id, accepting the canonical
/// id, any declared alias, or a concept's neutral spelling.
///
/// `None` means no backend declares it — which every caller turns into a
/// warning naming the key, rather than storing an orphan value nothing reads.
pub fn resolve_id(key: &str) -> Option<KnobId> {
  let want = normalise(key);
  for (_, def) in iter() {
    if normalise(def.id) == want {
      return Some(def.knob_id());
    }
    if def.aliases.iter().any(|a| normalise(a) == want) {
      return Some(def.knob_id());
    }
  }
  // Neutral concept spellings resolve to whichever knob carries that concept.
  // Ambiguous across backends by design — `--ctx` means "this backend's
  // context knob" — so the launch-time re-resolution in `for_backend_concept`
  // is what actually picks. Here we only need to know the key is legitimate.
  for (_, def) in iter() {
    if def
      .concept
      .is_some_and(|c| normalise(c.neutral_flag()) == want)
    {
      return Some(def.knob_id());
    }
  }
  None
}

/// Resolve a key **within one backend's own vocabulary** first.
///
/// [`resolve_id`] is backend-blind: it returns the first declaration matching
/// the key anywhere in the registry. That is right for the CLI, where a flag
/// must parse before the serving backend is known — but wrong for a backend
/// writing its own knob, because a name one backend declares as an *alias*
/// (llama.cpp's `-c`/`ctx` for `ctx-size`) can shadow another's *canonical*
/// id (ds4's `ctx`). Writing through the blind lookup then stores the value
/// under an id the emitting backend never reads.
///
/// Order: this backend's canonical ids, then its aliases, then its concepts,
/// then the global fallback.
pub fn resolve_id_for(backend_id: &str, key: &str) -> Option<KnobId> {
  let want = normalise(key);
  let defs = for_backend(backend_id);
  if let Some(d) = defs.iter().find(|d| normalise(d.id) == want) {
    return Some(d.knob_id());
  }
  if let Some(d) = defs
    .iter()
    .find(|d| d.aliases.iter().any(|a| normalise(a) == want))
  {
    return Some(d.knob_id());
  }
  if let Some(d) = defs.iter().find(|d| {
    d.concept
      .is_some_and(|c| normalise(c.neutral_flag()) == want)
  }) {
    return Some(d.knob_id());
  }
  resolve_id(key)
}

/// The first definition for `id`, whichever backend declared it. Used where a
/// surface needs the kind/label/help but not the owning backend (CLI `--help`,
/// value parsing).
pub fn def_for(id: KnobId) -> Option<&'static KnobDef> {
  iter().find(|(_, d)| d.knob_id() == id).map(|(_, d)| d)
}

/// `backend_id`'s definition for `id`, if that backend declares it.
pub fn def_for_backend(backend_id: &str, id: KnobId) -> Option<&'static KnobDef> {
  for_backend(backend_id).iter().find(|d| d.knob_id() == id)
}

/// `backend_id`'s knob carrying `concept`, if any.
///
/// The cross-backend carry-over. Two engines that both take a context window
/// spell the flag differently, so a value stored under one backend's id would
/// look unrecognised to the next. Matching on the shared concept instead finds
/// the destination backend's own knob and carries the value into it.
pub fn def_for_backend_concept(backend_id: &str, concept: Concept) -> Option<&'static KnobDef> {
  for_backend(backend_id)
    .iter()
    .find(|d| d.concept == Some(concept))
}

/// Group the registry by declaring backend, for `--help` headings and the
/// `llamastash knobs` listing. Backend order follows the registry.
pub fn by_backend() -> Vec<(&'static str, &'static [KnobDef])> {
  Backends::all()
    .iter()
    .map(|b| (b.id(), b.knobs()))
    .filter(|(_, defs)| !defs.is_empty())
    .collect()
}

/// One backend's knobs bucketed for the editor: [`Group::all`] order across
/// buckets, declaration order within one. Empty groups are dropped, so a
/// caller renders a header only where rows follow it.
///
/// This is what makes the editor generated rather than hand-listed — the row
/// order is a consequence of the declarations, not a second table that has to
/// be kept in step with them.
pub fn grouped_for_backend(backend_id: &str) -> Vec<(Group, Vec<&'static KnobDef>)> {
  let defs = for_backend(backend_id);
  Group::all()
    .iter()
    .filter_map(|g| {
      let rows: Vec<&'static KnobDef> = defs.iter().filter(|d| d.group == *g).collect();
      (!rows.is_empty()).then_some((*g, rows))
    })
    .collect()
}

/// A registry inconsistency. These are programmer errors in a backend's
/// declaration, caught by a test rather than at runtime — a shipped binary
/// cannot have a malformed registry because the test gates the build.
#[derive(Debug, PartialEq)]
pub enum RegistryError {
  /// Two backends declare the same id with incompatible kinds, so one CLI flag
  /// would have to parse two different value shapes.
  KindConflict {
    id: &'static str,
    first: &'static str,
    second: &'static str,
  },
  /// One backend declares two knobs carrying the same concept, so a
  /// cross-backend carry-over would be ambiguous.
  DuplicateConcept {
    backend: &'static str,
    concept: Concept,
  },
  /// An id that cannot serve as a flag or a YAML key.
  MalformedId { id: &'static str },
  /// A knob claiming a flag llamastash owns, or one the loopback/credential
  /// denylist refuses. Either would let a knob rebind the listener or steal
  /// the port the daemon reserved.
  ReservedFlag {
    id: &'static str,
    flag: String,
    reason: &'static str,
  },
  /// A declared cycle stop the knob's own kind rejects. The editor would offer
  /// it and the commit would then refuse it — a dead stop the user can land on
  /// but not keep.
  UnparsableRingStop {
    id: &'static str,
    stop: &'static str,
  },
}

impl std::fmt::Display for RegistryError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RegistryError::KindConflict { id, first, second } => write!(
        f,
        "knob `{id}` is declared with different kinds by `{first}` and `{second}`"
      ),
      RegistryError::DuplicateConcept { backend, concept } => write!(
        f,
        "backend `{backend}` declares two knobs for concept {concept:?}"
      ),
      RegistryError::MalformedId { id } => {
        write!(f, "knob id `{id}` is not a valid flag / config key")
      }
      RegistryError::UnparsableRingStop { id, stop } => write!(
        f,
        "knob `{id}` offers the cycle stop `{stop}`, which its own kind rejects"
      ),
      RegistryError::ReservedFlag { id, flag, reason } => {
        write!(f, "knob `{id}` claims `{flag}`, which {reason}")
      }
    }
  }
}

/// Flags llamastash owns on its own command line. A knob claiming one of these
/// would be shadowed by the global, so the launch would silently not carry it.
const RESERVED_LLAMASTASH_FLAGS: &[&str] = &[
  "--config",
  "--json",
  "--verbose",
  "--quiet",
  "--no-colors",
  "--no-scan",
  "--no-spawn",
  "--model-path",
  "--llama-server",
  "--render",
  "--render-size",
  "--mouse-focus",
  "--preset",
  "--backend",
  "--server",
  "--port",
  "--wait",
  "--help",
  "--version",
];

fn id_is_wellformed(id: &str) -> bool {
  !id.is_empty()
    && !id.starts_with('-')
    && !id.ends_with('-')
    && id
      .chars()
      .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Validate the whole registry. Run by a test, so a malformed declaration
/// fails the build rather than surfacing as a confusing runtime behaviour.
pub fn validate() -> Vec<RegistryError> {
  let mut errors = Vec::new();
  let mut kinds: BTreeMap<&'static str, (&'static str, super::def::KnobKind)> = BTreeMap::new();

  for (backend_id, def) in iter() {
    if !id_is_wellformed(def.id) {
      errors.push(RegistryError::MalformedId { id: def.id });
    }

    let flag = def.emit_flag();
    if RESERVED_LLAMASTASH_FLAGS.contains(&flag.as_str()) {
      errors.push(RegistryError::ReservedFlag {
        id: def.id,
        flag: flag.clone(),
        reason: "llamastash owns on its own command line",
      });
    }
    if crate::launch::params::is_forbidden_head(&flag) {
      errors.push(RegistryError::ReservedFlag {
        id: def.id,
        flag,
        reason: "the loopback / credential denylist refuses",
      });
    }

    // A stop the editor can land on but the commit refuses is a dead row.
    let stops: &[&'static str] = match def.ring {
      super::def::Ring::Fixed(r) | super::def::Ring::UpToTrainedContext(r) => r,
      _ => &[],
    };
    for stop in stops {
      if super::value::parse_value(def, stop).is_err() {
        errors.push(RegistryError::UnparsableRingStop { id: def.id, stop });
      }
    }

    match kinds.get(def.id) {
      Some((first, kind)) if *kind != def.kind => {
        errors.push(RegistryError::KindConflict {
          id: def.id,
          first,
          second: backend_id,
        });
      }
      Some(_) => {}
      None => {
        kinds.insert(def.id, (backend_id, def.kind));
      }
    }
  }

  for backend in Backends::all() {
    let mut seen: BTreeMap<Concept, usize> = BTreeMap::new();
    for def in backend.knobs() {
      if let Some(c) = def.concept {
        *seen.entry(c).or_insert(0) += 1;
      }
    }
    for (concept, count) in seen {
      if count > 1 {
        errors.push(RegistryError::DuplicateConcept {
          backend: backend.id(),
          concept,
        });
      }
    }
  }

  errors
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registry_is_valid() {
    let errors = validate();
    assert!(
      errors.is_empty(),
      "registry validation failed:\n{}",
      errors
        .iter()
        .map(|e| format!("  - {e}"))
        .collect::<Vec<_>>()
        .join("\n")
    );
  }

  #[test]
  fn every_backend_declares_at_least_a_context_knob() {
    // Context is the one tunable every serving engine has. A backend that
    // declares nothing would render an empty Settings pane and accept no
    // flags, which is always a mistake rather than a deliberate choice.
    for backend in Backends::all() {
      let defs = backend.knobs();
      assert!(
        !defs.is_empty(),
        "backend `{}` declares no knobs",
        backend.id()
      );
      assert!(
        defs
          .iter()
          .any(|d| d.concept == Some(Concept::ContextLength)),
        "backend `{}` declares no ContextLength knob",
        backend.id()
      );
    }
  }

  #[test]
  fn ids_resolve_through_canonical_alias_and_neutral_spellings() {
    // Canonical.
    assert_eq!(resolve_id("ctx-size").map(|i| i.as_str()), Some("ctx-size"));
    // Underscore spelling of the same name.
    assert_eq!(
      resolve_id("n_gpu_layers").map(|i| i.as_str()),
      Some("n-gpu-layers")
    );
    // Declared short alias.
    assert_eq!(resolve_id("-ngl").map(|i| i.as_str()), Some("n-gpu-layers"));
    // Unknown.
    assert_eq!(resolve_id("definitely-not-a-knob"), None);
  }

  #[test]
  fn concept_lookup_crosses_backends() {
    // Every backend has a context knob, and they spell it differently. The
    // concept is what lets a stored value follow the user across a switch.
    let spellings: std::collections::BTreeSet<&str> = Backends::all()
      .iter()
      .filter_map(|b| def_for_backend_concept(b.id(), Concept::ContextLength))
      .map(|d| d.id)
      .collect();
    assert!(
      spellings.len() > 1,
      "expected divergent ctx spellings across backends, got {spellings:?}"
    );
  }

  #[test]
  fn distinct_ids_dedupes_shared_knobs() {
    let all: usize = iter().count();
    let distinct = distinct_ids().len();
    assert!(
      distinct < all,
      "expected at least one knob id shared across backends ({distinct} distinct of {all})"
    );
  }
}

#[cfg(test)]
mod shared_ids {
  use super::*;

  /// Ids more than one backend declares. These are the knobs where one CLI
  /// flag has to mean the same thing whichever backend serves, so `validate`
  /// requires their kinds to agree. Pinned here because *adding* one silently
  /// is how a flag starts meaning two things.
  #[test]
  fn shared_ids_are_the_expected_set() {
    let mut counts: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (backend, def) in iter() {
      counts.entry(def.id).or_default().push(backend);
    }
    let mut shared: Vec<&str> = counts
      .iter()
      .filter(|(_, backends)| backends.len() > 1)
      .map(|(id, _)| *id)
      .collect();
    shared.sort_unstable();
    assert_eq!(
      shared,
      vec!["ctx-size", "mtp", "mtp-draft-n", "threads"],
      "shared knob ids changed; confirm the kinds still agree before pinning the new set"
    );
  }

  /// A shared id may still emit a different flag per backend — `mtp-draft-n`
  /// is `--spec-draft-n-max` on one and `--mtp-draft` on another. The id is
  /// the contract; the spelling is the backend's business.
  #[test]
  fn a_shared_id_can_emit_different_flags() {
    let flags: std::collections::BTreeSet<String> = iter()
      .filter(|(_, d)| d.id == "mtp-draft-n")
      .map(|(_, d)| d.emit_flag())
      .collect();
    assert!(
      flags.len() > 1,
      "expected divergent flags for one shared id, got {flags:?}"
    );
  }
}
