//! `backend.generic.servers[]`: user-declared model servers.
//!
//! Config-only on purpose. `binary` is arbitrary execution, so no IPC method or
//! CLI flag may set one; the trust level stays that of editing `config.yaml`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The placeholders the daemon fills in itself. Everything else inside `{…}`
/// must name one of the entry's knobs.
pub const BUILTIN_PLACEHOLDERS: &[&str] = &["port", "host", "name", "model"];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GenericConfig {
  #[serde(default)]
  pub servers: Vec<GenericServer>,
}

/// One declared server. See `docs/usage.md` § Generic backend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GenericServer {
  pub name: String,
  pub binary: PathBuf,
  /// The catalog GGUF(s) this server runs: a preset-key glob, a path, or a
  /// model id. Set, the entry is a server option on each matching model row
  /// rather than a row of its own, and `{model}` carries the chosen path.
  #[serde(default)]
  pub model: Option<String>,
  /// Required, but optional in the type so a missing one is refused with an
  /// error naming the entry rather than a bare serde error.
  #[serde(default)]
  pub ready: Option<String>,
  #[serde(default)]
  pub args: Vec<String>,
  #[serde(default)]
  pub knobs: Vec<KnobDecl>,
  #[serde(default)]
  pub env: BTreeMap<String, String>,
  #[serde(default)]
  pub memory_gib: Option<f64>,
  #[serde(default)]
  pub stop_grace_secs: Option<u64>,
  #[serde(default)]
  pub ready_timeout_secs: Option<u64>,
  /// Replace `body.model` with the launch's `{name}` value when forwarding, for
  /// a server that refuses any other model name.
  #[serde(default)]
  pub rewrite_model: bool,
  /// The endpoints the server answers: `chat`, `embedding`, `rerank`. Empty
  /// (the default) means all, so the engine answers or refuses each request.
  #[serde(default)]
  pub modes: Vec<crate::launch::mode::LaunchMode>,
  /// Row info for an entry without `model`, for the Arch / Params / Quant /
  /// Ctx columns. The catalog carries it like GGUF metadata, so `arch` also
  /// ranks proxy fallback candidates. A `model` row reads its GGUF.
  #[serde(default)]
  pub arch: Option<String>,
  #[serde(default)]
  pub params: Option<String>,
  #[serde(default)]
  pub quant: Option<String>,
  #[serde(default)]
  pub ctx: Option<u64>,
  /// The server takes image input. With no GGUF there is no vision projector to
  /// find, so `integrations` and the TUI only know from this.
  #[serde(default)]
  pub vision: bool,
  /// The `reasoning_effort` values the server accepts. Set, the row is a
  /// reasoning model, and `integrations` offers these levels as it does a
  /// GGUF's chat-template list. `none` means thinking can be turned off, so
  /// `[none]` alone marks a reasoning model with no levels to pick.
  #[serde(default)]
  pub reasoning_effort: Vec<String>,
  /// The level the server uses when a request sends none. Unset, tools that
  /// pin a level pick the highest.
  #[serde(default)]
  pub reasoning_effort_default: Option<String>,
  /// Mint one row per model the engine reports, instead of the single row this
  /// entry would otherwise make. See `rows_from` in `docs/usage.md`.
  #[serde(default)]
  pub rows_from: Option<RowsFrom>,
}

/// A `rows_from` block: the listing command and how its items map to rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RowsFrom {
  /// The command to run, argv-style. No shell, so no quoting.
  pub command: Vec<String>,
  /// Dot path to the array of models in the JSON body (`models`). Omit when the
  /// body is the array itself.
  #[serde(default)]
  pub items: Option<String>,
  /// Dot path to the model's name inside one item. Required: it becomes the row
  /// name, `{name}` at launch, and the name the engine reads from a request.
  pub name: RowField,
  /// Dot path or condition an item must satisfy to become a row: `installed`, or
  /// `{path: label, contains: audio}`.
  #[serde(default)]
  pub when: Option<When>,
  #[serde(default)]
  pub arch: Option<RowField>,
  #[serde(default)]
  pub params: Option<RowField>,
  #[serde(default)]
  pub quant: Option<RowField>,
  /// Dot path to the model's own context length, used for the Ctx column and
  /// the default when the entry sets no `ctx`.
  #[serde(default)]
  pub ctx: Option<RowField>,
  /// Dot path to the model's resident size in GiB, used for the size column and
  /// the admission gate. Falls back to the entry's `memory_gib`.
  #[serde(default)]
  pub memory_gib: Option<RowField>,
}

/// A dot path in one listing item. Typed by use, not by shape: the same string
/// is a name, a label or a number depending on the field it maps to.
pub type RowField = String;

/// A `when` filter: a bare dot path (truthy), or a value or array-member test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum When {
  Path(String),
  Match {
    path: String,
    #[serde(default)]
    equals: Option<String>,
    #[serde(default)]
    contains: Option<String>,
  },
}

// `memory_gib` is the only float, and validation refuses a NaN, so equality
// stays reflexive for every config that loads.
impl Eq for GenericServer {}

/// A knob declaration. A bare string is shorthand for `{flag: <string>}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KnobDecl {
  Flag(String),
  Full(KnobSpec),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct KnobSpec {
  pub flag: String,
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub default: Option<String>,
  #[serde(default)]
  pub ctx: bool,
  /// An on/off knob: `true` sends the bare `flag`, `false` sends nothing.
  #[serde(default)]
  pub switch: bool,
  #[serde(default)]
  pub label: Option<String>,
  #[serde(default)]
  pub help: Option<String>,
}

impl KnobDecl {
  pub fn spec(&self) -> KnobSpec {
    match self {
      KnobDecl::Flag(flag) => KnobSpec {
        flag: flag.clone(),
        ..KnobSpec::default()
      },
      KnobDecl::Full(spec) => spec.clone(),
    }
  }
}

impl KnobSpec {
  /// The knob id: the declared `id`, else the flag without leading dashes.
  pub fn knob_id(&self) -> String {
    match &self.id {
      Some(id) => id.trim().to_string(),
      None => self.flag.trim().trim_start_matches('-').to_string(),
    }
  }
}

impl GenericServer {
  /// `binary` with a leading `~` expanded.
  pub fn binary_path(&self) -> PathBuf {
    let raw = self.binary.to_string_lossy();
    if let Some(rest) = raw.strip_prefix("~/") {
      if let Some(home) = crate::util::paths::home_dir() {
        return home.join(rest);
      }
    }
    self.binary.clone()
  }

  /// Whether this entry's `model` names the catalog model at `path`. Uses the
  /// preset-key rules: a glob over filename, stem and path, else the exact
  /// path or case-insensitive filename / model id.
  pub fn serves(&self, path: &Path) -> bool {
    let Some(want) = self.model.as_deref() else {
      return false;
    };
    let Some(path_str) = path.to_str() else {
      return false;
    };
    if path_str.contains("://") {
      return false;
    }
    let want = match want.strip_prefix("~/") {
      Some(rest) => crate::util::paths::home_dir()
        .map(|h| h.join(rest).to_string_lossy().into_owned())
        .unwrap_or_else(|| want.to_string()),
      None => want.to_string(),
    };
    let file_label = crate::util::paths::model_file_label(path);
    crate::launch::presets::preset_key_matches(&want, &file_label, path_str)
      || (!crate::util::glob::is_pattern(&want)
        && want.eq_ignore_ascii_case(&crate::util::paths::model_display_name(path)))
  }

  /// The row's Ctx column: the declared `ctx`, else the `ctx: true` knob's
  /// default.
  pub fn row_ctx(&self) -> Option<u64> {
    self.ctx.or_else(|| {
      self
        .knobs
        .iter()
        .find_map(|d| match d {
          KnobDecl::Full(k) if k.ctx => Some(k),
          _ => None,
        })
        .and_then(|k| k.default.as_deref()?.trim().parse().ok())
    })
  }

  /// Every `{…}` placeholder in `args` and `env` values, in order of appearance.
  pub fn placeholders(&self) -> Vec<String> {
    self
      .args
      .iter()
      .chain(self.env.values())
      .flat_map(|s| placeholders_in(s))
      .collect()
  }
}

/// The `{ident}` tokens in `s`. A brace pair whose content is not a plain
/// identifier (`{"a": 1}`) is literal text, so JSON arguments pass untouched.
pub fn placeholders_in(s: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut rest = s;
  while let Some(open) = rest.find('{') {
    let after = &rest[open + 1..];
    match after.find('}') {
      Some(close) if is_placeholder_ident(&after[..close]) => {
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
      }
      _ => rest = after,
    }
  }
  out
}

fn is_placeholder_ident(s: &str) -> bool {
  !s.is_empty()
    && s
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Replace each `{ident}` in `s` via `lookup`; unknown idents stay as written.
pub fn substitute(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
  let mut out = String::with_capacity(s.len());
  let mut rest = s;
  while let Some(open) = rest.find('{') {
    out.push_str(&rest[..open]);
    let after = &rest[open + 1..];
    match after.find('}') {
      Some(close) if is_placeholder_ident(&after[..close]) => {
        let key = &after[..close];
        match lookup(key) {
          Some(v) => out.push_str(&v),
          None => {
            out.push('{');
            out.push_str(key);
            out.push('}');
          }
        }
        rest = &after[close + 1..];
      }
      _ => {
        out.push('{');
        rest = after;
      }
    }
  }
  out.push_str(rest);
  out
}

/// Whether `id` can serve as a knob id: the same shape the static registry
/// requires, since it doubles as a YAML key and a `--<id>` extras token.
fn id_is_wellformed(id: &str) -> bool {
  !id.is_empty()
    && !id.starts_with('-')
    && !id.ends_with('-')
    && id
      .chars()
      .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn name_is_wellformed(name: &str) -> bool {
  !name.is_empty()
    && !name
      .chars()
      .any(|c| c == '@' || c == '/' || c == '|' || c.is_whitespace() || c.is_control())
}

impl GenericConfig {
  /// Whether any entry is a catalog row of its own. A `model:` entry only
  /// attaches to rows found elsewhere.
  pub fn declares_rows(&self) -> bool {
    self.servers.iter().any(|s| s.model.is_none())
  }

  /// Every config-load refusal, as one error naming the entry. `taken` says
  /// whether a knob id is already a built-in knob id, alias or neutral
  /// spelling.
  pub fn validate(&self, taken: &dyn Fn(&str) -> bool) -> Result<(), String> {
    let mut names = BTreeSet::new();
    // A knob id reused across entries must agree on kind, because the flat
    // `id: value` shape of presets and `last_params` parses by id alone.
    let mut ctx_ness: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for s in &self.servers {
      let entry = |msg: String| format!("backend.generic entry `{}`: {msg}", s.name);
      if !name_is_wellformed(&s.name) {
        return Err(entry(
          "`name` must be non-empty with no `@`, `/`, `|` or whitespace".into(),
        ));
      }
      if !names.insert(s.name.to_ascii_lowercase()) {
        return Err(entry("duplicate `name`".into()));
      }
      let raw_binary = s.binary.to_string_lossy();
      // `has_root`, not `is_absolute`: the rule refuses cwd-relative paths,
      // and a drive-less `/opt/x` on Windows resolves against the drive, not
      // the cwd.
      if !(s.binary.has_root() || raw_binary.starts_with("~/")) {
        return Err(entry(format!(
          "`binary` must be an absolute or `~/` path, got `{raw_binary}`"
        )));
      }
      match s.ready.as_deref() {
        Some(p) if p.starts_with('/') => {}
        Some(p) => {
          return Err(entry(format!("`ready` must start with `/`, got `{p}`")));
        }
        None => {
          return Err(entry(
            "`ready` is required (the HTTP path that returns 200 once the model is loaded)".into(),
          ));
        }
      }
      if let Some(m) = s.memory_gib {
        if !(m > 0.0 && m.is_finite()) {
          return Err(entry(format!("`memory_gib` must be > 0, got {m}")));
        }
        if s.model.is_some() {
          return Err(entry(
            "`memory_gib` applies only without `model`; a catalog model is sized from its GGUF"
              .into(),
          ));
        }
      }
      if let Some(spec) = &s.rows_from {
        if s.model.is_some() {
          return Err(entry(
            "`rows_from` mints rows, so it cannot combine with `model`".into(),
          ));
        }
        if spec.command.is_empty() || spec.command.iter().any(|a| a.is_empty()) {
          return Err(entry(
            "`rows_from.command` must be a non-empty argv list".into(),
          ));
        }
        for (field, path) in [
          ("name", Some(&spec.name)),
          ("items", spec.items.as_ref()),
          ("arch", spec.arch.as_ref()),
          ("params", spec.params.as_ref()),
          ("quant", spec.quant.as_ref()),
          ("ctx", spec.ctx.as_ref()),
          ("memory_gib", spec.memory_gib.as_ref()),
        ] {
          if path.is_some_and(|p| p.trim().is_empty()) {
            return Err(entry(format!("`rows_from.{field}` is empty")));
          }
        }
        if let Some(When::Match {
          path,
          equals,
          contains,
        }) = &spec.when
        {
          if path.trim().is_empty() || equals.is_some() == contains.is_some() {
            return Err(entry(
              "`rows_from.when` needs a `path` and exactly one of `equals` or `contains`".into(),
            ));
          }
        }
      }
      let is_blank = |v: &Option<String>| v.as_deref().is_some_and(|v| v.trim().is_empty());
      let info = [
        ("arch", s.arch.is_some(), is_blank(&s.arch)),
        ("params", s.params.is_some(), is_blank(&s.params)),
        ("quant", s.quant.is_some(), is_blank(&s.quant)),
        ("ctx", s.ctx.is_some(), s.ctx == Some(0)),
        ("vision", s.vision, false),
        ("reasoning_effort", !s.reasoning_effort.is_empty(), false),
        (
          "reasoning_effort_default",
          s.reasoning_effort_default.is_some(),
          false,
        ),
      ];
      for (field, set, blank) in info {
        if set && s.model.is_some() {
          return Err(entry(format!(
            "`{field}` applies only without `model`; a catalog model reads its GGUF"
          )));
        }
        if blank {
          let rule = if field == "ctx" {
            "must be > 0"
          } else {
            "is empty"
          };
          return Err(entry(format!("`{field}` {rule}")));
        }
      }
      // Levels go to tools and the server verbatim, and tools match them by
      // exact name, so `Low` or `low ` would silently never match.
      let is_level = |l: &str| {
        !l.is_empty()
          && l
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
      };
      if let Some(l) = s.reasoning_effort.iter().find(|l| !is_level(l)) {
        return Err(entry(format!(
          "`reasoning_effort` level `{l}` must be lowercase letters, digits, `-` or `_`"
        )));
      }
      if let Some(d) = s
        .reasoning_effort_default
        .as_ref()
        .filter(|d| *d == "none" || !s.reasoning_effort.contains(d))
      {
        return Err(entry(format!(
          "`reasoning_effort_default` must be a `reasoning_effort` level other than `none`, got `{d}`"
        )));
      }
      let placeholders = s.placeholders();
      match s.model.as_deref() {
        Some(m) if m.trim().is_empty() => return Err(entry("`model` is empty".into())),
        Some(_) if !placeholders.iter().any(|p| p == "model") => {
          return Err(entry(
            "`model` is set but no arg or env uses `{model}` to pass the chosen path".into(),
          ));
        }
        None if placeholders.iter().any(|p| p == "model") => {
          return Err(entry("`{model}` needs a `model` field to fill it".into()));
        }
        _ => {}
      }
      let mut ids = BTreeSet::new();
      let mut ctx_count = 0;
      for decl in &s.knobs {
        let spec = decl.spec();
        let id = spec.knob_id();
        if !id_is_wellformed(&id) {
          return Err(entry(format!(
            "knob id `{id}` must be lowercase letters, digits and `-` (set `id:`)"
          )));
        }
        if spec.flag.trim().is_empty() {
          return Err(entry(format!("knob `{id}` has an empty `flag`")));
        }
        let head = spec.flag.split('=').next().unwrap_or(&spec.flag);
        if crate::launch::params::is_forbidden_head(head) {
          return Err(entry(format!(
            "knob flag `{}` is refused (loopback / credential contract)",
            spec.flag
          )));
        }
        if taken(&id) {
          return Err(entry(format!(
            "knob id `{id}` is already a built-in knob or alias; set `id:` to rename it"
          )));
        }
        if BUILTIN_PLACEHOLDERS.contains(&id.as_str()) {
          return Err(entry(format!(
            "knob id `{id}` is a reserved placeholder; set `id:` to rename it"
          )));
        }
        if !ids.insert(id.clone()) {
          return Err(entry(format!("duplicate knob id `{id}`")));
        }
        if spec.switch {
          if spec.ctx {
            return Err(entry(format!(
              "knob `{id}` cannot be both `ctx` and `switch`"
            )));
          }
          if let Some(d) = &spec.default {
            if !matches!(d.trim(), "true" | "false") {
              return Err(entry(format!(
                "`switch: true` knob `{id}` needs a `true` or `false` default, got `{d}`"
              )));
            }
          }
        }
        if spec.ctx {
          ctx_count += 1;
          if let Some(d) = &spec.default {
            if d.trim().parse::<u32>().is_err() {
              return Err(entry(format!(
                "`ctx: true` knob `{id}` needs a token-count default, got `{d}`"
              )));
            }
          }
        }
        match ctx_ness.get(&id) {
          Some((other_ctx, other_entry)) if *other_ctx != spec.ctx => {
            return Err(entry(format!(
              "knob id `{id}` is `ctx: {}` here but `ctx: {other_ctx}` in `{other_entry}`; \
               use a different `id:`",
              spec.ctx
            )));
          }
          Some(_) => {}
          None => {
            ctx_ness.insert(id.clone(), (spec.ctx, s.name.clone()));
          }
        }
      }
      if ctx_count > 1 {
        return Err(entry("at most one knob may set `ctx: true`".into()));
      }
      for p in s.placeholders() {
        if !BUILTIN_PLACEHOLDERS.contains(&p.as_str()) && !ids.contains(&p) {
          return Err(entry(format!(
            "unknown placeholder `{{{p}}}` (use {{port}}, {{host}}, {{name}}, {{model}} or a knob id)"
          )));
        }
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const EXAMPLE: &str = r#"
servers:
  - name: flash-next-gufo
    binary: /opt/gufo/gufo
    args: [serve, --host, "{host}", --port, "{port}", llm, --served-model-name, "{name}"]
    knobs:
      - flag: --context
        ctx: true
        default: "262144"
      - flag: --speculative
        default: mtp
      - flag: -t
        id: temperature
        default: "1.0"
      - --seed
    ready: /ready
    memory_gib: 95
    stop_grace_secs: 60
  - name: flash-next-halogen
    binary: ~/bin/halogen-serve.sh
    args: ["{port}"]
    knobs:
      - flag: --ctx-window
        id: halogen-ctx
        ctx: true
        default: "65536"
      - flag: --temperature
        default: "1.0"
    env:
      HALOGEN_CTX: "{halogen-ctx}"
      HALOGEN_TEMPERATURE: "{temperature}"
    ready: /v1/models
    ready_timeout_secs: 600
"#;

  fn parse(yaml: &str) -> GenericConfig {
    yaml_serde::from_str(yaml).unwrap()
  }

  fn builtin(id: &str) -> bool {
    crate::launch::knobs::registry::resolve_static_id(id).is_some()
  }

  fn refusal(yaml: &str) -> String {
    parse(yaml).validate(&builtin).unwrap_err()
  }

  #[test]
  fn the_documented_example_parses_and_validates() {
    let c = parse(EXAMPLE);
    assert_eq!(c.servers.len(), 2);
    c.validate(&builtin).unwrap();
    assert_eq!(
      c.servers[0].knobs[3].spec(),
      KnobSpec {
        flag: "--seed".into(),
        ..KnobSpec::default()
      },
      "shorthand is `{{flag: ...}}`"
    );
    assert_eq!(c.servers[0].knobs[2].spec().knob_id(), "temperature");
  }

  #[test]
  fn two_entries_may_reuse_an_id() {
    let yaml = r#"
servers:
  - {name: a, binary: /a, ready: /h, knobs: [--speculative]}
  - {name: b, binary: /b, ready: /h, knobs: [--speculative]}
"#;
    parse(yaml).validate(&builtin).unwrap();
  }

  #[test]
  fn each_refusal_names_the_entry() {
    let cases = [
      ("{name: e, binary: /b, ready: /h, knobs: [{flag: --x, id: threads}]}", "built-in"),
      ("{name: e, binary: /b, ready: /h, knobs: [-t]}", "built-in"),
      ("{name: e, binary: /b, ready: /h, knobs: [--x, --x]}", "duplicate knob"),
      ("{name: e, binary: /b, ready: /h, knobs: [--port]}", "refused"),
      (
        "{name: e, binary: /b, ready: /h, knobs: [{flag: --ca, ctx: true}, {flag: --cb, ctx: true}]}",
        "at most one",
      ),
      ("{name: e, binary: /b}", "`ready` is required"),
      ("{name: e, binary: /b, ready: /h, memory_gib: 0}", "memory_gib"),
      ("{name: e, binary: /b, ready: /h, args: [\"{foo}\"]}", "unknown placeholder"),
      ("{name: e@x, binary: /b, ready: /h}", "`name`"),
      ("{name: \"e|x\", binary: /b, ready: /h}", "`name`"),
      ("{name: e, binary: rel/b, ready: /h}", "absolute"),
      ("{name: e, binary: /b, ready: /h, knobs: [{flag: --x, id: port}]}", "reserved"),
      (
        "{name: e, binary: /b, ready: /h, knobs: [{flag: --x, switch: true, ctx: true}]}",
        "both `ctx` and `switch`",
      ),
      (
        "{name: e, binary: /b, ready: /h, knobs: [{flag: --x, switch: true, default: yes}]}",
        "`true` or `false` default",
      ),
    ];
    for (entry, want) in cases {
      let msg = refusal(&format!("servers:\n  - {entry}\n"));
      assert!(msg.contains("entry `e"), "{msg}");
      assert!(msg.contains(want), "{entry}: {msg}");
    }
    for (entry, want) in [
      (
        "{name: e, binary: /b, ready: /h, model: \"*.gguf\"}",
        "no arg or env",
      ),
      (
        "{name: e, binary: /b, ready: /h, args: [\"{model}\"]}",
        "needs a `model`",
      ),
      (
        "{name: e, binary: /b, ready: /h, model: x, args: [\"{model}\"], memory_gib: 4}",
        "only without `model`",
      ),
      (
        "{name: e, binary: /b, ready: /h, model: x, args: [\"{model}\"], quant: Q4_K_M}",
        "`quant` applies only without `model`",
      ),
      (
        "{name: e, binary: /b, ready: /h, model: x, args: [\"{model}\"], ctx: 4096}",
        "`ctx` applies only without `model`",
      ),
      (
        "{name: e, binary: /b, ready: /h, model: x, args: [\"{model}\"], vision: true}",
        "`vision` applies only without `model`",
      ),
      (
        "{name: e, binary: /b, ready: /h, model: x, args: [\"{model}\"], reasoning_effort: [low]}",
        "`reasoning_effort` applies only without `model`",
      ),
      (
        "{name: e, binary: /b, ready: /h, model: x, args: [\"{model}\"], reasoning_effort_default: low}",
        "`reasoning_effort_default` applies only without `model`",
      ),
    ] {
      let msg = refusal(&format!("servers:\n  - {entry}\n"));
      assert!(msg.contains(want), "{entry}: {msg}");
    }
    for (entry, want) in [
      (
        "{name: e, binary: /b, ready: /h, arch: ''}",
        "`arch` is empty",
      ),
      (
        "{name: e, binary: /b, ready: /h, params: '  '}",
        "`params` is empty",
      ),
      (
        "{name: e, binary: /b, ready: /h, quant: ''}",
        "`quant` is empty",
      ),
      (
        "{name: e, binary: /b, ready: /h, ctx: 0}",
        "`ctx` must be > 0",
      ),
      (
        "{name: e, binary: /b, ready: /h, reasoning_effort: [low, ' ']}",
        "`reasoning_effort` level ` ` must be lowercase letters, digits, `-` or `_`",
      ),
      (
        "{name: e, binary: /b, ready: /h, reasoning_effort: [Low]}",
        "`reasoning_effort` level `Low` must be lowercase letters, digits, `-` or `_`",
      ),
      (
        "{name: e, binary: /b, ready: /h, reasoning_effort: [low], reasoning_effort_default: high}",
        "`reasoning_effort_default` must be a `reasoning_effort` level other than `none`, got `high`",
      ),
      (
        "{name: e, binary: /b, ready: /h, reasoning_effort: [low, none], reasoning_effort_default: none}",
        "`reasoning_effort_default` must be a `reasoning_effort` level other than `none`, got `none`",
      ),
      (
        "{name: e, binary: /b, ready: /h, reasoning_effort_default: low}",
        "`reasoning_effort_default` must be a `reasoning_effort` level other than `none`, got `low`",
      ),
    ] {
      let msg = refusal(&format!("servers:\n  - {entry}\n"));
      assert!(msg.ends_with(want), "{entry}: {msg}");
    }
    let dup = refusal(
      "servers:\n  - {name: e, binary: /b, ready: /h}\n  - {name: e, binary: /c, ready: /h}\n",
    );
    assert!(dup.contains("duplicate `name`"), "{dup}");
  }

  #[test]
  fn literal_braces_are_not_placeholders() {
    assert_eq!(
      placeholders_in(r#"{"a": 1} {port} x{name}y"#),
      vec!["port", "name"]
    );
    let out = substitute(r#"{"a": 1} {port} {nope}"#, &|k| {
      (k == "port").then(|| "9".into())
    });
    assert_eq!(out, r#"{"a": 1} 9 {nope}"#);
  }

  #[test]
  fn model_matches_like_a_preset_key() {
    let e = |m: &str| GenericServer {
      name: "e".into(),
      binary: "/b".into(),
      model: Some(m.into()),
      ready: Some("/h".into()),
      args: vec!["{model}".into()],
      knobs: vec![],
      env: Default::default(),
      memory_gib: None,
      stop_grace_secs: None,
      ready_timeout_secs: None,
      rewrite_model: false,
      modes: vec![],
      arch: None,
      params: None,
      quant: None,
      ctx: None,
      vision: false,
      reasoning_effort: vec![],
      reasoning_effort_default: None,
      rows_from: None,
    };
    let split = Path::new("/hf/Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf");
    let other = Path::new("/hf/gemma-4-Q4_K_M.gguf");
    assert!(e("*Flash-Next*").serves(split));
    assert!(!e("*Flash-Next*").serves(other));
    assert!(e("Qwen3.8-Flash-Next-UD-Q4_K_XL").serves(split), "model id");
    assert!(e("/hf/gemma-4-Q4_K_M.gguf").serves(other), "exact path");
    assert!(
      !e("*").serves(Path::new("generic://e")),
      "never another entry's row"
    );
  }

  #[test]
  fn only_entries_without_model_declare_rows() {
    let attached =
      parse("servers:\n  - {name: a, binary: /bin/a, model: 'qwen*', ready: /health}\n");
    assert!(!attached.declares_rows());
    let own = parse("servers:\n  - {name: b, binary: /bin/b, ready: /health}\n");
    assert!(own.declares_rows());
    assert!(!GenericConfig::default().declares_rows());
  }
}
