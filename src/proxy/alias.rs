//! `proxy.aliases`: a name a client is hard-wired to, standing in for a local
//! model. A tool that ships `gpt-4o-mini` in its own config can reach a local
//! model without a config edit. `docs/usage.md` (Model ids on the proxy) is the
//! copy a user reads; this module is what enforces it, together with
//! [`crate::proxy::route::resolve_client_reference`], where the table is
//! consulted.
//!
//! The table is built once at daemon start from `config.yaml` and never changes
//! while the daemon runs, so lookups are read-only. Every problem it can report
//! — a name a real model owns, a value that names no model, a value that is
//! another alias name, an alias that moves a working partial match — is logged
//! once per name through [`AliasTable::note_once`], keyed by kind so two problems
//! about one name cannot swallow each other.

use std::collections::HashSet;
use std::sync::RwLock;

/// Client name → model reference, plus the warnings already logged about it.
///
/// One table per daemon, held behind an `Arc` because `ProxyState` derives
/// `Clone` and every per-daemon field on it is shared rather than copied. The
/// warn set has to be one object process-wide for "one line per problem" to hold,
/// which a by-value field on a cloneable struct would not give.
#[derive(Debug, Default)]
pub(crate) struct AliasTable {
  /// `(name, model reference)` in config order, names already through
  /// [`normalize`]. A table is small and scanned once per request that misses,
  /// so the order the operator wrote is worth keeping: it is what makes a
  /// duplicate name resolve to the entry they put last.
  targets: Vec<(String, String)>,
  /// Names already reported, so a warning fires once per name rather than once
  /// per request.
  warned: RwLock<HashSet<String>>,
}

/// One spelling for a stored alias name: model references already resolve
/// case-insensitively, so one config entry has to answer to any client casing.
/// Reads fold case in place against these keys instead of calling this per
/// request.
fn normalize(name: &str) -> String {
  name.trim().to_ascii_lowercase()
}

// The kinds a report can be, and therefore the ways one alias name can be
// reported about without swallowing itself.
const KIND_SHADOW: &str = "shadow";
const KIND_PARTIAL: &str = "partial";
const KIND_AMBIGUOUS: &str = "ambiguous";
const KIND_DEAD: &str = "dead";
const KIND_CHAIN: &str = "chain";

/// The `KIND_PARTIAL` tag, exported so a caller can ask
/// [`AliasTable::report_pending`] before paying for the catalog pass that feeds
/// the report.
pub(crate) const REPORT_PARTIAL: &str = KIND_PARTIAL;

impl AliasTable {
  /// The table behind `proxy.aliases`, in the order the file lists the entries.
  pub(crate) fn from_config(raw: &crate::config::ProxyAliases) -> Self {
    Self::build(raw.pairs())
  }

  fn build<'a>(entries: impl Iterator<Item = (&'a str, &'a str)>) -> Self {
    let mut targets: Vec<(String, String)> = Vec::new();
    for (name, target) in entries {
      let (key, target) = (normalize(name), target.trim().to_string());
      if key.is_empty() || target.is_empty() {
        log::warn!("proxy.aliases: ignoring `{name}` — both the name and the model it points at are required");
        continue;
      }
      // `name@launch` is a client's address for one launch of a model, and an
      // alias is consulted before that split. An alias could not honour the
      // launch half anyway, so a name spelled like an address is refused instead
      // of quietly taking the address over.
      if key.contains('@') {
        log::warn!("proxy.aliases: ignoring `{name}` — an alias names a model, not a `<model>@<launch>` address");
        continue;
      }
      if let Some(at) = targets.iter().position(|(existing, _)| *existing == key) {
        log::warn!("proxy.aliases: `{name}` is listed twice; the later entry is used");
        targets.remove(at);
      }
      targets.push((key, target));
    }
    Self {
      targets,
      warned: RwLock::new(HashSet::new()),
    }
  }

  /// The model reference `requested` stands in for, or `None` when it is not an
  /// alias.
  pub(crate) fn target(&self, requested: &str) -> Option<&str> {
    self.lookup(requested).map(|(_, target)| target.as_str())
  }

  /// The entry a client's string means, if it means one at all. Nearly every
  /// request asks and gets "no", so the comparison folds case in place instead of
  /// building a lowercased copy of the name first.
  fn lookup(&self, requested: &str) -> Option<&(String, String)> {
    if self.targets.is_empty() {
      return None;
    }
    let name = requested.trim();
    self
      .targets
      .iter()
      .find(|(key, _)| key.eq_ignore_ascii_case(name))
  }

  /// A real model owns `requested`, so the alias of that name never answers. The
  /// client gets the model it named, which is right, so this is a note about the
  /// config rather than about the request.
  pub(crate) fn warn_shadowed(&self, requested: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_SHADOW);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` names a model that exists, so that model is used and the alias is not"
      );
    }
    first
  }

  /// The alias won, but `requested` also reached `other` on its own - a longer
  /// file name that contains it - so clients that were already being served move
  /// to another model. Worth saying, because nothing in a response shows it.
  pub(crate) fn warn_moved_partial(&self, requested: &str, target: &str, other: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_PARTIAL);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` names `{target}`, but it also matched `{other}` on its own; the alias is used"
      );
    }
    first
  }

  /// Two or more models answer to the alias's value. Naming one is the operator's
  /// call, and the client sending the alias name has no way to settle it.
  pub(crate) fn warn_ambiguous_target(&self, requested: &str, target: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_AMBIGUOUS);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` points at `{target}`, which matches more than one model; name one of them"
      );
    }
    first
  }

  /// The alias's value names no model, so every request under that name is going
  /// to miss until the config changes.
  pub(crate) fn warn_dead_target(&self, requested: &str, target: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_DEAD);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` points at `{target}`, which names no model on its own; give a full name or a path"
      );
    }
    first
  }

  /// The alias's value is another alias name, which is a chain that reaches no
  /// model. When the same string is also a real model, that model answered
  /// instead and this was never reached.
  pub(crate) fn warn_points_at_alias(&self, requested: &str, target: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_CHAIN);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` points at `{target}`, which is another alias name; name the model instead"
      );
    }
    first
  }

  /// True when `kind` has not been reported for this alias name yet, without
  /// claiming the report. A caller uses this to skip work that only feeds the
  /// once-per-name line.
  pub(crate) fn report_pending(&self, requested: &str, kind: &str) -> bool {
    match self.report_key(requested, kind) {
      Some(key) => self
        .warned
        .read()
        .map(|seen| !seen.contains(&key))
        .unwrap_or(false),
      None => false,
    }
  }

  /// Every report is about a configured alias name, and each kind gets its own
  /// prefix so two problems about one name cannot swallow each other.
  fn note_as_alias(&self, requested: &str, kind: &str) -> bool {
    match self.report_key(requested, kind) {
      Some(key) => self.note_once(key),
      None => false,
    }
  }

  /// The warning key for `requested`, when it names a configured alias.
  fn report_key(&self, requested: &str, kind: &str) -> Option<String> {
    self
      .lookup(requested)
      .map(|(key, _)| format!("{kind} {key}"))
  }

  /// First call for this key wins, for the whole daemon.
  fn note_once(&self, key: String) -> bool {
    self
      .warned
      .write()
      .map(|mut seen| seen.insert(key))
      .unwrap_or(false)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn table(pairs: &[(&str, &str)]) -> AliasTable {
    let raw = crate::config::ProxyAliases::from_pairs(
      pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())),
    );
    AliasTable::from_config(&raw)
  }

  #[test]
  fn target_resolves_any_client_spelling() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert_eq!(t.target("gpt-4o-mini"), Some("qwen3.8-27b"));
    assert_eq!(t.target("GPT-4o-Mini"), Some("qwen3.8-27b"));
    assert_eq!(t.target("  gpt-4o-mini  "), Some("qwen3.8-27b"));
    assert_eq!(t.target("gpt-4o"), None);
  }

  #[test]
  fn blank_entries_are_dropped_not_stored() {
    let t = table(&[("", "qwen3.8-27b"), ("claude-haiku", "  "), ("ok", " x ")]);
    assert_eq!(t.target(""), None);
    assert_eq!(t.target("claude-haiku"), None);
    // A target keeps its text minus the surrounding blanks.
    assert_eq!(t.target("ok"), Some("x"));
  }

  #[test]
  fn shadow_is_reported_once_per_name() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(
      !t.report_pending("some-real-model", REPORT_PARTIAL),
      "a name that is not an alias has nothing to report"
    );
    assert!(t.warn_shadowed("gpt-4o-mini"));
    assert!(
      !t.warn_shadowed("GPT-4o-MINI"),
      "the same name must not warn twice"
    );
  }

  #[test]
  fn a_dead_alias_target_is_reported_once_and_apart_from_a_shadow() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(!t.report_pending("some-real-model", REPORT_PARTIAL));
    assert!(t.warn_dead_target("gpt-4o-mini", "qwen3.8-27b"));
    assert!(
      !t.warn_dead_target("GPT-4o-Mini", "qwen3.8-27b"),
      "the same name must not warn twice"
    );
    // A name can be shadowed once and its dead target reported once: two
    // different problems, so neither swallows the other.
    assert!(t.warn_shadowed("gpt-4o-mini"));
    assert!(!t.warn_shadowed("gpt-4o-mini"), "the shadow warns once too");
  }

  #[test]
  fn a_name_spelled_like_a_launch_address_is_refused() {
    // `qwen3@dev` as an alias name would be consulted before the `<model>@<name>`
    // split, so it would take a client's real address away and honour none of it.
    let t = table(&[("qwen3@dev", "somewhere-else")]);
    assert_eq!(t.target("qwen3@dev"), None);
  }

  #[test]
  fn an_alias_pointing_at_another_alias_stays_in_the_table() {
    // The refusal belongs to resolution, not to the table: `hardwired -> y` names
    // no model, but `y` may still be a name a real model answers to, and dropping
    // the entry here would take that away too.
    let t = table(&[("hardwired", "y"), ("y", "realmodel")]);
    assert_eq!(t.target("hardwired"), Some("y"));
    assert_eq!(t.target("y"), Some("realmodel"));
  }

  #[test]
  fn a_chain_is_reported_once_and_apart_from_a_dead_target() {
    let t = table(&[("a", "b")]);
    assert!(t.warn_points_at_alias("a", "b"));
    assert!(!t.warn_points_at_alias("a", "b"));
    assert!(
      t.warn_dead_target("a", "b"),
      "a chain and a dead target are different problems"
    );
  }

  #[test]
  fn an_ambiguous_target_is_reported_once() {
    let t = table(&[("gpt-4o-mini", "qwen")]);
    assert!(t.warn_ambiguous_target("gpt-4o-mini", "qwen"));
    assert!(!t.warn_ambiguous_target("gpt-4o-mini", "qwen"));
    assert!(
      t.warn_dead_target("gpt-4o-mini", "qwen"),
      "an ambiguous target and a dead one are different problems"
    );
  }

  #[test]
  fn every_kind_of_report_survives_an_alias_named_like_another_kind_of_key() {
    // The reports share one set, so they are keyed by kind as well as by name.
    let t = table(&[("target qwen", "alpha"), ("partial qwen", "alpha")]);
    assert!(t.warn_dead_target("target qwen", "alpha"));
    assert!(
      t.warn_shadowed("target qwen"),
      "the dead-target report must not spend the shadow one"
    );
    assert!(t.warn_moved_partial("partial qwen", "alpha", "other.gguf"));
    assert!(t.warn_shadowed("partial qwen"));
  }

  #[test]
  fn an_alias_overriding_a_working_partial_match_is_reported_once() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(
      !t.warn_moved_partial("some-real-model", "qwen3.8-27b", "other.gguf"),
      "not an alias name"
    );
    assert!(t.warn_moved_partial("gpt-4o-mini", "qwen3.8-27b", "other.gguf"));
    assert!(
      !t.warn_moved_partial("GPT-4o-Mini", "qwen3.8-27b", "other.gguf"),
      "the same name must not warn twice"
    );
    assert!(
      t.warn_shadowed("gpt-4o-mini"),
      "a third kind of report is not swallowed by this one"
    );
  }

  #[test]
  fn an_empty_table_has_nothing_to_report() {
    // The common case: no aliases configured, and every routed request still
    // asks whether a name is one of them.
    let t = table(&[]);
    assert!(!t.report_pending("anything", REPORT_PARTIAL));
    assert!(!t.warn_shadowed("anything"));
    assert!(!t.warn_dead_target("anything", "wherever"));
  }

  #[test]
  fn a_report_is_pending_until_it_is_made() {
    // The caller pays a catalog pass to answer the override question, so it asks
    // first whether the line is still unspent.
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(!t.report_pending("some-real-model", REPORT_PARTIAL));
    assert!(t.report_pending("gpt-4o-mini", REPORT_PARTIAL));
    assert!(t.warn_moved_partial("gpt-4o-mini", "qwen3.8-27b", "other.gguf"));
    assert!(
      !t.report_pending("gpt-4o-mini", REPORT_PARTIAL),
      "spent reports are not asked about again"
    );
  }

  #[test]
  fn a_repeated_name_keeps_the_entry_written_last() {
    // Order is the operator's, so the entry they wrote last is the one that
    // answers — for the map spelling as much as for the `name:`/`target:` list.
    let t = table(&[("gpt-4o-mini", "first"), ("gpt-4o-mini", "second")]);
    assert_eq!(t.target("gpt-4o-mini"), Some("second"));
  }

  #[test]
  fn a_pair_table_keeps_file_order() {
    let raw = crate::config::ProxyAliases::from_pairs(vec![
      ("a".to_string(), "first".to_string()),
      ("a".to_string(), "second".to_string()),
      ("B".to_string(), "third".to_string()),
    ]);
    let t = AliasTable::from_config(&raw);
    assert_eq!(t.target("a"), Some("second"));
    assert_eq!(t.target("b"), Some("third"));
  }

  #[test]
  fn an_empty_table_answers_nothing_and_never_warns() {
    let t = AliasTable::from_config(&crate::config::ProxyAliases::default());
    assert_eq!(t.target("anything"), None);
    assert!(!t.warn_shadowed("anything"));
  }
}
