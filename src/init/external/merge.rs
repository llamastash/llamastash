//! Format-neutral merge + structural diff over `serde_json::Value`.
//!
//! Mirrors [`crate::config::writer`]'s YAML semantics but stays on
//! JSON so the same code path serves both the JSON tool patchers
//! (OpenCode, Zed, pi.dev) and the YAML tool patchers (Aider,
//! Continue.dev) via the trivial JSON↔YAML round-trip at the
//! reader/writer boundaries.
//!
//! Arrays and scalars in `additions` *replace* their counterpart in
//! `current` (no array concat). Same rule the YAML writer uses;
//! consistent UX. User-authored keys outside the additions tree are
//! left alone.

use serde_json::Value;

use crate::util::config_patch::{DiffEntry, DiffKind};

/// Existing keys stay where they are and new keys go at the end, so a
/// user's file keeps its order. A remove-then-insert would not: `remove`
/// swaps the last key into the gap.
pub fn merge(current: Value, additions: Value) -> Value {
  match (current, additions) {
    (Value::Object(mut cur), Value::Object(add)) => {
      for (k, v) in add {
        match cur.get_mut(&k) {
          Some(existing) => *existing = merge(existing.take(), v),
          None => {
            cur.insert(k, v);
          }
        }
      }
      Value::Object(cur)
    }
    (_, other) => other,
  }
}

/// One entry per leaf added or modified; unchanged leaves emit nothing.
/// The `path`
/// fields are dotted; `value_yaml` is JSON-inline (the field is
/// named `value_yaml` historically — treat it as "value's
/// serialised representation in the target format").
pub fn diff(before: &Value, after: &Value) -> Vec<DiffEntry> {
  let mut out = Vec::new();
  walk("", before, after, &mut out);
  out
}

fn walk(prefix: &str, before: &Value, after: &Value, out: &mut Vec<DiffEntry>) {
  match (before, after) {
    (Value::Object(b), Value::Object(a)) => {
      for (k, v_after) in a {
        let path = if prefix.is_empty() {
          k.clone()
        } else {
          format!("{prefix}.{k}")
        };
        match b.get(k) {
          Some(v_before) if v_before == v_after => {}
          Some(v_before) => walk(&path, v_before, v_after, out),
          None => out.push(DiffEntry {
            path,
            kind: DiffKind::Added,
            value_yaml: serialise_inline(v_after),
          }),
        }
      }
    }
    (b, a) if b == a => {}
    (_, a) => out.push(DiffEntry {
      path: prefix.to_string(),
      kind: DiffKind::Changed,
      value_yaml: serialise_inline(a),
    }),
  }
}

fn serialise_inline(v: &Value) -> String {
  serde_json::to_string(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn json(s: &str) -> Value {
    serde_json::from_str(s).expect("json fixture")
  }

  #[test]
  fn merge_replaces_scalars_and_recurses_into_objects() {
    let cur = json(r#"{"theme":"latte","port":{"start":41100,"end":41300}}"#);
    let add =
      json(r#"{"port":{"start":50000},"providers":{"llamastash":{"baseURL":"http://x/v1"}}}"#);
    let out = merge(cur, add);
    assert_eq!(out["theme"], "latte");
    assert_eq!(out["port"]["start"], 50000);
    assert_eq!(out["port"]["end"], 41300);
    assert_eq!(out["providers"]["llamastash"]["baseURL"], "http://x/v1");
  }

  #[test]
  fn merge_preserves_user_keys_inside_managed_object() {
    let cur = json(r#"{"providers":{"llamastash":{"name":"old"},"openai":{"apiKey":"k"}}}"#);
    let add = json(r#"{"providers":{"llamastash":{"baseURL":"http://x/v1"}}}"#);
    let out = merge(cur, add);
    assert_eq!(out["providers"]["openai"]["apiKey"], "k");
    assert_eq!(out["providers"]["llamastash"]["name"], "old");
    assert_eq!(out["providers"]["llamastash"]["baseURL"], "http://x/v1");
  }

  #[test]
  fn diff_flags_added_and_changed_leaves() {
    let before = json(r#"{"theme":"latte","port":{"start":41100,"end":41300}}"#);
    let after = json(r#"{"theme":"latte","port":{"start":50000,"end":41300},"providers":{"x":1}}"#);
    let rows = diff(&before, &after);
    let by_path: std::collections::HashMap<_, _> =
      rows.iter().map(|r| (r.path.as_str(), r)).collect();
    assert!(by_path.contains_key("port.start"));
    assert_eq!(by_path["port.start"].kind, DiffKind::Changed);
    assert!(by_path.contains_key("providers"));
    assert_eq!(by_path["providers"].kind, DiffKind::Added);
  }

  #[test]
  fn merge_keeps_existing_key_order() {
    let cur = json(r#"{"zeta": 1, "alpha": {"b": 1, "a": 2}, "mid": 3}"#);
    let add = json(r#"{"alpha": {"a": 9, "c": 4}, "new": 5, "zeta": 0}"#);
    let out = merge(cur, add);
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&out), ["zeta", "alpha", "mid", "new"]);
    assert_eq!(keys(&out["alpha"]), ["b", "a", "c"]);
    assert_eq!(out["alpha"]["a"], 9);
  }

  #[test]
  fn array_replaces_not_concats() {
    let cur = json(r#"{"models":[{"id":"a"}]}"#);
    let add = json(r#"{"models":[{"id":"b"}]}"#);
    let out = merge(cur, add);
    assert_eq!(out["models"].as_array().unwrap().len(), 1);
    assert_eq!(out["models"][0]["id"], "b");
  }

  #[test]
  fn diff_empty_when_no_change() {
    let v = json(r#"{"a":1,"b":{"c":2}}"#);
    assert!(diff(&v, &v).is_empty());
  }
}
