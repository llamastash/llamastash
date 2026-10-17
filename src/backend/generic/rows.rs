//! Catalog rows minted from a server's own model listing.
//!
//! An engine that owns its model store (FastFlowLM on an AMD NPU, anything
//! else with a `list --json`) knows which models it has, which context each
//! one wants and what it costs to keep. Duplicating that in `config.yaml` is
//! how a recipe goes stale, so `rows_from` runs a command and mints one row
//! per reported model. The entry still decides how to launch: `{name}` is the
//! minted row's tag, so the same entry launches any model it minted.

use std::path::PathBuf;

use serde_json::Value;

use crate::discovery::{DiscoveredModel, ModelSource};
use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

use super::config::{GenericServer, RowsFrom};
use super::{gib_to_bytes, GENERIC_PATH_SCHEME};

/// How long the listing command may run before its rows are dropped.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Rows one entry may mint. A listing this long is the wrong command, not a
/// model store, and every row costs a registry entry and a TUI line.
const MAX_ROWS: usize = 1000;

/// Run the entry's listing command and mint its rows.
///
/// Any failure (no binary, non-zero exit, unparseable JSON) mints nothing and
/// logs one line: a model store that is briefly unreadable must not empty the
/// catalog or fail discovery.
pub async fn mint(server: &GenericServer) -> Vec<DiscoveredModel> {
  let Some(spec) = server.rows_from.as_ref() else {
    return Vec::new();
  };
  match run(spec).await {
    Ok(stdout) => rows_from_json(server, spec, &stdout),
    Err(err) => {
      log::warn!(
        "backend.generic entry `{}`: `rows_from` listing failed: {err}",
        server.name
      );
      Vec::new()
    }
  }
}

async fn run(spec: &RowsFrom) -> Result<String, String> {
  let program = crate::util::paths::expand_user_path(std::path::Path::new(&spec.command[0]));
  let mut cmd = tokio::process::Command::new(&program);
  cmd
    .args(&spec.command[1..])
    .stdin(std::process::Stdio::null())
    // A listing that hangs must not leave a process behind on every rescan.
    .kill_on_drop(true);
  let out = tokio::time::timeout(COMMAND_TIMEOUT, cmd.output())
    .await
    .map_err(|_| format!("`{}` timed out", spec.command.join(" ")))?
    .map_err(|e| format!("`{}`: {e}", spec.command.join(" ")))?;
  if !out.status.success() {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: String = stderr.trim().chars().take(200).collect();
    return Err(format!(
      "`{}` exited {}: {tail}",
      spec.command.join(" "),
      out.status,
    ));
  }
  String::from_utf8(out.stdout).map_err(|_| "listing output is not UTF-8".to_string())
}

/// Mint rows from a listing body. Exported for tests and for a caller that
/// already has the JSON.
pub fn rows_from_json(server: &GenericServer, spec: &RowsFrom, body: &str) -> Vec<DiscoveredModel> {
  let parsed: Value = match serde_json::from_str(body) {
    Ok(v) => v,
    Err(e) => {
      log::warn!(
        "backend.generic entry `{}`: `rows_from` listing is not JSON: {e}",
        server.name
      );
      return Vec::new();
    }
  };
  let items = match spec.items.as_deref() {
    Some(path) => dot(&parsed, path),
    None => Some(&parsed),
  };
  let Some(items) = items.and_then(Value::as_array) else {
    log::warn!(
      "backend.generic entry `{}`: `rows_from` found no array at `{}`",
      server.name,
      spec.items.as_deref().unwrap_or("(root)")
    );
    return Vec::new();
  };
  let mut out: Vec<DiscoveredModel> = Vec::new();
  for item in items {
    if !keeps(spec.when.as_ref(), item) {
      continue;
    }
    let Some(tag) = field(item, &spec.name).and_then(|v| v.string) else {
      log::debug!(
        "backend.generic entry `{}`: `rows_from` item has no `{}`",
        server.name,
        spec.name
      );
      continue;
    };
    if !tag_is_usable(&tag) {
      log::debug!(
        "backend.generic entry `{}`: skipping unusable model name `{tag}`",
        server.name
      );
      continue;
    }
    if out.len() >= MAX_ROWS {
      log::warn!(
        "backend.generic entry `{}`: `rows_from` stopped at {MAX_ROWS} rows",
        server.name
      );
      break;
    }
    let arch = spec
      .arch
      .as_ref()
      .and_then(|p| field(item, p))
      .and_then(|v| v.string);
    let params = spec
      .params
      .as_ref()
      .and_then(|p| field(item, p))
      .and_then(|v| v.string);
    let quant = spec
      .quant
      .as_ref()
      .and_then(|p| field(item, p))
      .and_then(|v| v.string);
    let ctx = spec
      .ctx
      .as_ref()
      .and_then(|p| field(item, p))
      .and_then(|v| v.number)
      .map(|n| n as u64);
    let memory_gib = spec
      .memory_gib
      .as_ref()
      .and_then(|p| field(item, p))
      .and_then(|v| v.number);
    out.push(row(server, &tag, arch, params, quant, ctx, memory_gib));
  }
  out
}

/// One minted row. The path keeps the entry, so any of the entry's rows
/// resolves back to it at launch; the display name is the bare tag, which is
/// what `{name}` sends and what the engine echoes.
fn row(
  server: &GenericServer,
  tag: &str,
  arch: Option<String>,
  params: Option<String>,
  quant: Option<String>,
  ctx: Option<u64>,
  memory_gib: Option<f64>,
) -> DiscoveredModel {
  use crate::launch::mode::LaunchMode::{Embedding, Rerank};
  let gib = memory_gib.or(server.memory_gib).filter(|g| *g > 0.0);
  DiscoveredModel {
    path: PathBuf::from(format!("{GENERIC_PATH_SCHEME}{}/{tag}", server.name)),
    parent: PathBuf::from(GENERIC_PATH_SCHEME),
    source: ModelSource::Config,
    metadata: Some(ModelMetadata {
      arch,
      total_parameters: None,
      parameter_label: params,
      quant: Quant::Unknown(0),
      quant_label: quant,
      native_ctx: ctx.or(server.ctx),
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: !server.reasoning_effort.is_empty(),
      mode_hint: match server.modes.first() {
        Some(Embedding) => ModeHint::Embedding,
        Some(Rerank) => ModeHint::Rerank,
        _ => ModeHint::Chat,
      },
      weights_bytes: gib.map(gib_to_bytes),
      lazy_tensor_bytes: Vec::new(),
      mtp: None,
    }),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: Some(tag.to_string()),
    multimodal: server.vision.then_some(crate::discovery::Multimodal {
      vision: true,
      audio: false,
    }),
    supported_backends: vec![super::GENERIC_BACKEND_ID.to_string()],
    mtp_head: None,
  }
}

/// Whether an item passes the `when` filter.
fn keeps(when: Option<&super::config::When>, item: &Value) -> bool {
  use super::config::When;
  match when {
    None => true,
    Some(When::Path(path)) => truthy(dot(item, path)),
    Some(When::Match {
      path,
      equals,
      contains,
    }) => {
      let Some(v) = dot(item, path) else {
        return false;
      };
      match (equals, contains) {
        (Some(want), None) => v.as_str().is_some_and(|s| s == want),
        (None, Some(want)) => match v {
          // A list matches on a member, a string on a substring, so an engine
          // that reports `["reasoning"]` and one that reports `"reasoning"`
          // both work.
          Value::Array(arr) => arr.iter().any(|e| e.as_str() == Some(want.as_str())),
          Value::String(s) => s.contains(want.as_str()),
          _ => false,
        },
        (Some(_), Some(_)) | (None, None) => false,
      }
    }
  }
}

/// A minted name must survive being a display name, a `generic://` path tail
/// and a CLI argument. Tags like `qwen3:0.6b` are fine; a name with a slash or
/// a space would make a row that cannot be named back.
fn tag_is_usable(tag: &str) -> bool {
  !tag.is_empty()
    && !tag
      .chars()
      .any(|c| c == '/' || c == '@' || c == '|' || c.is_whitespace() || c.is_control())
}

fn truthy(v: Option<&Value>) -> bool {
  match v {
    None => false,
    Some(Value::Bool(b)) => *b,
    Some(Value::Null) => false,
    Some(Value::String(s)) => !s.is_empty(),
    Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
    Some(Value::Array(a)) => !a.is_empty(),
    Some(Value::Object(o)) => !o.is_empty(),
  }
}

/// A mapped field: the string form and, for numbers, the value.
struct Found {
  string: Option<String>,
  number: Option<f64>,
}

fn field(item: &Value, path: &str) -> Option<Found> {
  dot(item, path).map(|v| Found {
    string: match v {
      Value::String(s) => Some(s.clone()),
      Value::Number(n) => Some(n.to_string()),
      Value::Bool(b) => Some(b.to_string()),
      _ => None,
    },
    number: v.as_f64().filter(|f| f.is_finite()),
  })
}

/// A dotted path through JSON objects (`details.family`).
fn dot<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
  let mut cur = root;
  for part in path.split('.') {
    cur = cur.get(part)?;
  }
  Some(cur)
}

#[cfg(test)]
mod tests {
  use super::*;

  const FLM_LISTING: &str = r#"{
    "models": [
      {"model":"qwen3:0.6b","backend":"flm","installed":true,"footprint":0.66,
       "default_context_length":32768,
       "details":{"family":"qwen3","parameter_size":"0.6B","quantization_level":"Q4_1","think":false},
       "label":["reasoning"],"think_toggleable":true},
      {"model":"llama3.2:1b","backend":"flm","installed":false,"footprint":1.4,
       "default_context_length":131072,
       "details":{"family":"llama","parameter_size":"1B","quantization_level":"Q4_0"}},
      {"model":"whisper-v3:turbo","backend":"flm","installed":true,"footprint":0.62,
       "default_context_length":448,
       "details":{"family":"whisper-v3","parameter_size":"1B","quantization_level":"Q4_1"},
       "label":["audio","transcription"]}
    ]
  }"#;

  fn entry(yaml: &str) -> GenericServer {
    let cfg: crate::backend::generic::config::GenericConfig = yaml_serde::from_str(yaml).unwrap();
    cfg.servers.into_iter().next().unwrap()
  }

  fn recipe(when: &str) -> GenericServer {
    entry(&format!(
      r#"
servers:
  - name: flm
    binary: /opt/flm
    args: [serve, "{{name}}", --port, "{{port}}"]
    ready: /v1/version
    rows_from:
      command: [/opt/flm, list, --json]
      items: models
      name: model
      when: {when}
      arch: details.family
      params: details.parameter_size
      quant: details.quantization_level
      ctx: default_context_length
      memory_gib: footprint
"#
    ))
  }

  /// (display name, weights_bytes, native_ctx, arch) per minted row.
  type Minted = Vec<(String, Option<u64>, Option<u64>, Option<String>)>;

  fn minted(when: &str) -> Minted {
    let server = recipe(when);
    let spec = server.rows_from.clone().unwrap();
    rows_from_json(&server, &spec, FLM_LISTING)
      .into_iter()
      .map(|m| {
        let md = m.metadata.unwrap();
        (
          m.display_label.unwrap(),
          md.weights_bytes,
          md.native_ctx,
          md.arch,
        )
      })
      .collect()
  }

  #[test]
  fn one_row_per_installed_item_with_the_item_s_own_sizes() {
    let rows = minted("installed");
    assert_eq!(rows.len(), 2, "the not-installed item is skipped");
    assert_eq!(rows[0].0, "qwen3:0.6b");
    assert_eq!(rows[0].2, Some(32768));
    assert_eq!(rows[0].3.as_deref(), Some("qwen3"));
    assert_eq!(rows[0].1, Some((0.66 * 1024.0f64.powi(3)) as u64));
    assert_eq!(rows[1].0, "whisper-v3:turbo");
    assert_eq!(rows[1].2, Some(448));
  }

  #[test]
  fn the_path_carries_the_entry_and_the_name_is_the_bare_tag() {
    let server = recipe("installed");
    let spec = server.rows_from.clone().unwrap();
    let rows = rows_from_json(&server, &spec, FLM_LISTING);
    assert_eq!(
      rows[0].path.to_str().unwrap(),
      "generic://flm/qwen3:0.6b",
      "the launch resolves the entry from the path"
    );
    assert_eq!(rows[0].supported_backends, ["generic"]);
    assert_eq!(rows[0].source, ModelSource::Config);
  }

  #[test]
  fn when_can_match_a_value_or_an_array_member() {
    let by_family = minted("{path: details.family, equals: qwen3}");
    assert_eq!(by_family.len(), 1);
    let by_label = minted("{path: label, contains: audio}");
    assert_eq!(
      by_label.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
      ["whisper-v3:turbo"]
    );
    assert!(minted("{path: backend, equals: nope}").is_empty());
  }

  #[test]
  fn without_a_filter_every_item_is_minted() {
    let server = recipe("installed");
    let mut spec = server.rows_from.clone().unwrap();
    spec.when = None;
    assert_eq!(rows_from_json(&server, &spec, FLM_LISTING).len(), 3);
  }

  #[test]
  fn a_broken_listing_mints_nothing() {
    let server = recipe("installed");
    let spec = server.rows_from.clone().unwrap();
    assert!(rows_from_json(&server, &spec, "not json").is_empty());
    assert!(rows_from_json(&server, &spec, r#"{"models": {}}"#).is_empty());
    // An item with no name is skipped, the rest still land.
    let body = r#"{"models":[{"installed":true},{"model":"ok","installed":true}]}"#;
    assert_eq!(
      rows_from_json(&server, &spec, body)
        .into_iter()
        .map(|m| m.display_label.unwrap())
        .collect::<Vec<_>>(),
      ["ok"]
    );
  }

  #[test]
  fn a_name_that_cannot_be_a_row_is_skipped() {
    let server = recipe("installed");
    let spec = server.rows_from.clone().unwrap();
    for bad in ["", "a b", "a/b", "a@b"] {
      let body = format!(r#"{{"models":[{{"model":"{bad}","installed":true}}]}}"#);
      assert!(
        rows_from_json(&server, &spec, &body).is_empty(),
        "`{bad}` must not mint a row"
      );
    }
  }

  /// A string label matches on a substring; a missing path or a non-string
  /// against `equals` fails closed.
  #[test]
  fn contains_matches_a_substring_and_equals_fails_closed() {
    let server = recipe("{path: label, contains: reason}");
    let spec = server.rows_from.clone().unwrap();
    let body = r#"{"models":[{"model":"a","label":"vision-reasoning"},
                          {"model":"b","label":"audio"},
                          {"model":"c"},{"model":"d","label":7}]}"#;
    assert_eq!(
      rows_from_json(&server, &spec, body)
        .into_iter()
        .map(|m| m.display_label.unwrap())
        .collect::<Vec<_>>(),
      ["a"]
    );
    let eq = recipe(r#"{path: n, equals: "2"}"#);
    let eq_spec = eq.rows_from.clone().unwrap();
    assert!(rows_from_json(&eq, &eq_spec, r#"{"models":[{"model":"d","n":2}]}"#).is_empty());
  }

  #[tokio::test]
  async fn an_entry_without_rows_from_or_with_a_dead_listing_mints_nothing() {
    let plain = entry(
      r#"
servers:
  - name: plain
    binary: /opt/x
    args: [--port, "{port}"]
    ready: /health
    memory_gib: 1
"#,
    );
    assert!(plain.rows_from.is_none());
    assert!(mint(&plain).await.is_empty(), "no listing command, no rows");
    let dead = entry(
      r#"
servers:
  - name: dead
    binary: /opt/x
    args: [--port, "{port}"]
    ready: /health
    memory_gib: 1
    rows_from:
      command: [/nonexistent-engine-xyz, list]
      items: models
      name: model
"#,
    );
    assert!(
      mint(&dead).await.is_empty(),
      "a missing listing binary mints nothing"
    );
  }
}
