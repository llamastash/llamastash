//! `llamastash show <model> [--json]`.
//!
//! One-stop projection of everything LlamaStash knows about a single
//! model: catalog row, GGUF metadata, on-disk size (summed across
//! split shards), the yaml + built-in `arch_defaults` that would
//! feed a launch, and the last `start_model` params recorded for
//! this file. Reuses the same resolver `start` and `/v1/...` use, so
//! a reference that works on one surface works here.

use serde_json::{json, Value};

use std::path::{Path, PathBuf};

use crate::cli::cli_args::{Cli, ShowArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::colors;
use crate::cli::exit_codes::{CliExit, CliResult};
use crate::cli::output::pretty_json;
use crate::cli::resolve::{fetch_catalog, resolve_model, CatalogRow};
use crate::config::Config;
use crate::daemon::host_metrics::GpuFlavor;
use crate::discovery::shard_sizes::{self, ShardSize};
use crate::init::detection::fmt_bytes;
use crate::launch::defaults_table;

pub async fn handle(args: ShowArgs, cli: &Cli, config: &Config) -> CliResult {
  // Every CLI command must support `--json`. Errors flow through the
  // same machinery: when `--json` is set, a CliExit lands on stdout
  // as `{"error": {"code": …, "message": …}}` instead of stderr
  // prose so agents can parse failure shapes without scraping. The
  // exit code is preserved either way.
  match build_view(&args, cli, config).await {
    Ok(view) => {
      if args.json {
        println!("{}", pretty_json(&view.envelope));
      } else {
        print!(
          "{}",
          render_human(&view.row, &view.shards, view.total_bytes, &view.envelope)
        );
      }
      Ok(())
    }
    Err(exit) => {
      if args.json {
        let body = json!({
          "error": {
            "code": exit.code,
            "message": exit.message.as_deref().unwrap_or(""),
          },
        });
        println!("{}", pretty_json(&body));
        // Drop the message so `report` doesn't double-print it to
        // stderr — the JSON body on stdout is the canonical surface.
        Err(crate::cli::exit_codes::CliExit::code_only(exit.code))
      } else {
        Err(exit)
      }
    }
  }
}

struct ShowView {
  row: CatalogRow,
  shards: Vec<ShardSize>,
  total_bytes: u64,
  envelope: Value,
}

async fn build_view(args: &ShowArgs, cli: &Cli, config: &Config) -> Result<ShowView, CliExit> {
  let mut client = connect_or_spawn(cli, config).await?;
  let catalog = fetch_catalog(&mut client).await?;
  let row = resolve_model(&catalog, &args.model)?;

  // Pull last-params for this model_path. The IPC handler keys by
  // ModelId; `model_path` is part of the JSON wire shape (`entry.id.path`)
  // and is unique within the catalog, so filtering by string equality
  // is sufficient here.
  let last_params_body = client
    .call("last_params_list", None)
    .await
    .map_err(CliExit::from_client_error)?;
  let last_params = last_params_body
    .get("last_params")
    .and_then(Value::as_array)
    .and_then(|rows| {
      rows.iter().find_map(|r| {
        let p = r.get("model_path").and_then(Value::as_str)?;
        if p == row.path {
          r.get("params").cloned()
        } else {
          None
        }
      })
    });

  // GPU backend from the daemon's host-metrics sampler — keys the
  // built-in arch_defaults lookup so the values we display match
  // what `start_model` would resolve.
  let status_body = client
    .call("status", None)
    .await
    .map_err(CliExit::from_client_error)?;
  let backend_label = status_body
    .get("host")
    .and_then(|h| h.get("gpu_backend"))
    .and_then(Value::as_str)
    .unwrap_or("");
  let backend = GpuFlavor::from_label(backend_label);

  // Live running info for this exact model path: when a
  // supervisor is up, surface what `--fit` actually resolved (and any
  // ctx clamp) so `show` reflects the running reality, not just the
  // catalog metadata + arch defaults. `null` when nothing is running.
  let running = status_body
    .get("models")
    .and_then(Value::as_array)
    .and_then(|rows| {
      rows
        .iter()
        .find(|m| crate::cli::output::row_path(m) == Some(row.path.as_str()))
    })
    .map(|m| {
      let state = m.get("state").and_then(|s| {
        s.get("state")
          .and_then(Value::as_str)
          .or_else(|| s.as_str())
      });
      json!({
        "launch_id": m.get("launch_id"),
        "state": state,
        "port": m.get("port"),
        "resolved_ctx": m.get("resolved_ctx"),
        "ctx_clamped": m.get("ctx_clamped").and_then(Value::as_bool).unwrap_or(false),
      })
    });

  // Built-in arch defaults for this (arch, backend) pair — the same
  // values that ship under `LayerLabel::ArchDefault` in the launch
  // resolver. Yaml arch_defaults sit on the same layer and win
  // per-field; surface both so the user sees where each field comes
  // from.
  let arch_key = row.arch.as_deref().unwrap_or("");
  let builtin_arch_defaults = defaults_table::lookup(arch_key, backend);
  let yaml_arch_defaults = row
    .arch
    .as_deref()
    .and_then(|a| config.arch_defaults.get(a))
    .cloned();

  let shards = shard_breakdown(&row);
  let total_bytes: u64 = shards
    .iter()
    .map(|s| s.bytes)
    .fold(0u64, u64::saturating_add);
  let shards_json: Vec<Value> = shards
    .iter()
    .enumerate()
    .map(|(idx, s)| {
      json!({
        "index": idx + 1,
        "path": s.path,
        "bytes": s.bytes,
      })
    })
    .collect();

  let envelope = assemble_envelope(
    &row,
    json!({
      "weights_bytes": row.weights_bytes,
      "shard_count": shards.len(),
      "on_disk_total_bytes": total_bytes,
      "shards": shards_json,
    }),
    json!({
      "gpu_backend": format!("{backend:?}"),
      "yaml": yaml_arch_defaults,
      "builtin": builtin_arch_defaults,
    }),
    last_params,
    running,
  );

  Ok(ShowView {
    row,
    shards,
    total_bytes,
    envelope,
  })
}

/// The `show --json` envelope is the shared catalog-row wire shape
/// ([`CatalogRow`]'s serde impl, the same bytes `list --json` and IPC
/// `list_models` emit) with the show-only sections layered on top. No
/// second, hand-listed projection: capability fields (`multimodal`,
/// `mtp`, `supported_backends`, `split_siblings`, the nested
/// `metadata` block) track the catalog row by construction.
fn assemble_envelope(
  row: &CatalogRow,
  size: Value,
  arch_defaults: Value,
  last_params: Option<Value>,
  running: Option<Value>,
) -> Value {
  let mut envelope = serde_json::to_value(row).unwrap_or_else(|_| json!({}));
  envelope["size"] = size;
  envelope["arch_defaults"] = arch_defaults;
  envelope["last_params"] = last_params.unwrap_or(Value::Null);
  envelope["running"] = running.unwrap_or(Value::Null);
  envelope
}

/// Per-shard `(path, bytes)` breakdown for the resolved row. Always
/// includes shard 1 (the catalog row's `path`); for split entries
/// extends with each sibling. Delegates to the shared
/// `discovery::shard_sizes` util so the byte counts here match the
/// values the scanner folded into `metadata.weights_bytes`.
fn shard_breakdown(row: &CatalogRow) -> Vec<ShardSize> {
  let primary = PathBuf::from(&row.path);
  let siblings: Vec<PathBuf> = row.split_siblings.iter().map(PathBuf::from).collect();
  shard_sizes::per_shard(&primary, &siblings)
}

fn render_human(row: &CatalogRow, shards: &[ShardSize], total_bytes: u64, env: &Value) -> String {
  use crate::cli::format::{kv_block, section_header};
  let mut out = String::new();

  // Header row: the model name is the document title. Section bodies
  // below route through the shared `kv_block` so labels align (and bold
  // on a TTY) exactly like `status` / `presets`.
  out.push_str(&section_header(&row.name(), None));
  let mut header: Vec<(&str, String)> = Vec::new();
  // `path` covers single-file models; multi-shard sets get a full
  // per-shard listing under the `size` section below, so emit the
  // parent dir instead — shard 1's path on its own would only
  // partially describe the model on disk.
  if shards.len() == 1 {
    header.push(("path", row.path.clone()));
  }
  header.push(("parent", row.parent.clone()));
  header.push(("source", row.source.clone()));
  // Prefer the daemon-resolved backend tag; rows it didn't tag keep the
  // source-derived badge (mirrors the `list` table fallback).
  let backend_badge = row
    .backend
    .clone()
    .unwrap_or_else(|| crate::cli::output::backend_for_source(&row.source).to_string());
  header.push(("backend", backend_badge));
  if let Some(id) = &row.model_id {
    header.push(("model_id", id.clone()));
  }
  if let Some(lbl) = &row.display_label {
    header.push(("display_label", lbl.clone()));
  }
  if let Some(err) = &row.parse_error {
    header.push(("parse_error", colors::warning(err)));
  }
  out.push_str(&kv_block(&header));

  out.push('\n');
  out.push_str(&section_header("metadata", None));
  out.push_str(&kv_block(&[
    ("arch", row.arch.as_deref().unwrap_or("—").to_string()),
    ("quant", row.quant.as_deref().unwrap_or("—").to_string()),
    (
      "native_ctx",
      row
        .native_ctx
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into()),
    ),
    (
      "mode_hint",
      row.mode_hint.as_deref().unwrap_or("—").to_string(),
    ),
    (
      "parameter_label",
      row.parameter_label.as_deref().unwrap_or("—").to_string(),
    ),
    (
      "tokenizer_kind",
      row.tokenizer_kind.as_deref().unwrap_or("—").to_string(),
    ),
    (
      "weights_bytes",
      row
        .weights_bytes
        .map(fmt_bytes)
        .unwrap_or_else(|| "—".into()),
    ),
    (
      "has_chat_template",
      if row.has_chat_template { "yes" } else { "no" }.to_string(),
    ),
    (
      "has_reasoning_hint",
      if row.has_reasoning_hint { "yes" } else { "no" }.to_string(),
    ),
    ("multimodal", multimodal_label(row)),
    ("mtp", mtp_label(row)),
  ]));

  out.push('\n');
  out.push_str(&section_header("size", None));
  let mut size_rows: Vec<(String, String)> = vec![
    ("shard_count".to_string(), shards.len().to_string()),
    ("on_disk_total".to_string(), fmt_bytes(total_bytes)),
  ];
  // Per-shard breakdown so a multi-shard model shows every file
  // and its individual size, not just shard 1. Single-file models
  // collapse to one row, keeping the human output tight.
  for (idx, shard) in shards.iter().enumerate() {
    let size = if shard.bytes == 0 {
      colors::warning("missing")
    } else {
      fmt_bytes(shard.bytes)
    };
    let path = render_shard_path(&shard.path);
    size_rows.push((format!("shard {}", idx + 1), format!("{size}  {path}")));
  }
  out.push_str(&kv_block_owned(&size_rows));

  // Live running block — only when a supervisor is up for this
  // model. Shows the context window `--fit` actually resolved and flags
  // a memory-driven clamp, so `show` reflects the running reality.
  if let Some(running) = env.get("running").filter(|r| !r.is_null()) {
    out.push('\n');
    out.push_str(&section_header("running", None));
    let clamped = running
      .get("ctx_clamped")
      .and_then(Value::as_bool)
      .unwrap_or(false);
    let resolved = match running.get("resolved_ctx") {
      Some(Value::Number(n)) if clamped => {
        format!("{n} {}", colors::dim("(clamped to fit-ctx floor)"))
      }
      Some(Value::Number(n)) => n.to_string(),
      _ => "—".into(),
    };
    out.push_str(&kv_block(&[
      ("state", fmt_field(running.get("state"))),
      ("port", fmt_field(running.get("port"))),
      ("resolved_ctx", resolved),
    ]));
  }

  let backend = env
    .get("arch_defaults")
    .and_then(|a| a.get("gpu_backend"))
    .and_then(Value::as_str)
    .unwrap_or("");
  out.push('\n');
  // `arch_defaults` carries the resolving GPU backend as a dim suffix
  // on the title line, mirroring `section_header`'s `(N noun)` count
  // suffix style so the section still reads "arch_defaults (CpuOnly)".
  out.push_str(section_header("arch_defaults", None).trim_end());
  out.push(' ');
  out.push_str(&colors::dim(&format!("({backend})")));
  out.push('\n');
  let yaml = env.get("arch_defaults").and_then(|a| a.get("yaml"));
  let builtin = env.get("arch_defaults").and_then(|a| a.get("builtin"));
  out.push_str(&kv_block(&[
    ("yaml", knobs_one_line(yaml)),
    ("builtin", knobs_one_line(builtin)),
  ]));

  out.push('\n');
  out.push_str(&section_header("last_params", None));
  match env.get("last_params") {
    Some(Value::Null) | None => out.push_str(&kv_block(&[(
      "(none)",
      "launch it once to populate".to_string(),
    )])),
    Some(v) => {
      out.push_str(&kv_block(&[
        ("ctx", fmt_field(v.get("ctx"))),
        ("mode", fmt_field(v.get("mode"))),
        ("reasoning", fmt_field(v.get("reasoning"))),
        ("knobs", knobs_one_line(v.get("knobs"))),
      ]));
    }
  }

  out
}

/// Human label for the multimodal capability block: `vision + audio`,
/// `vision`, `audio`; `—` when the model has no projector companion.
fn multimodal_label(row: &CatalogRow) -> String {
  match &row.multimodal {
    Some(mm) => {
      let mut parts: Vec<&str> = Vec::new();
      if mm.vision {
        parts.push("vision");
      }
      if mm.audio {
        parts.push("audio");
      }
      if parts.is_empty() {
        "—".into()
      } else {
        parts.join(" + ")
      }
    }
    None => "—".into(),
  }
}

/// Human label for the MTP capability block: `embedded (N layers)` /
/// `separate head` (joined when both); `—` when not MTP-capable.
fn mtp_label(row: &CatalogRow) -> String {
  match &row.mtp {
    Some(m) => {
      let mut parts: Vec<String> = Vec::new();
      if let Some(n) = m.embedded_layers {
        parts.push(format!(
          "embedded ({n} layer{})",
          if n == 1 { "" } else { "s" }
        ));
      }
      if m.separate_head {
        parts.push("separate head".into());
      }
      if parts.is_empty() {
        "—".into()
      } else {
        parts.join(" + ")
      }
    }
    None => "—".into(),
  }
}

/// `kv_block` over owned `(String, String)` rows. The shared helper
/// takes `&[(&str, String)]`; the per-shard rows below build their
/// labels at runtime (`shard 1`, `shard 2`, …), so we borrow each
/// owned key into that shape rather than leaking `'static` strings.
fn kv_block_owned(rows: &[(String, String)]) -> String {
  let borrowed: Vec<(&str, String)> = rows.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
  crate::cli::format::kv_block(&borrowed)
}

fn fmt_field(v: Option<&Value>) -> String {
  match v {
    Some(Value::Null) | None => "—".into(),
    Some(Value::String(s)) => s.clone(),
    Some(other) => other.to_string(),
  }
}

fn knobs_one_line(value: Option<&Value>) -> String {
  let Some(Value::Object(map)) = value else {
    return "—".into();
  };
  let mut pairs: Vec<String> = map
    .iter()
    .filter(|(_, val)| !val.is_null())
    .map(|(key, val)| match val {
      Value::String(s) => format!("{key}={s}"),
      _ => format!("{key}={val}"),
    })
    .collect();
  pairs.sort();
  if pairs.is_empty() {
    "—".into()
  } else {
    pairs.join(", ")
  }
}

/// Friendly per-shard path: keep just the file basename — the row
/// header already showed the parent dir, so repeating the full path
/// per shard would wrap the line and bury the size column.
fn render_shard_path(path: &Path) -> String {
  path
    .file_name()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::discovery::Multimodal;
  use crate::launch::resolve::MtpCapability;
  use serde_json::json;

  fn fake_row(path: &str) -> CatalogRow {
    CatalogRow {
      path: path.into(),
      model_id: Some("deadbeef".into()),
      parent: "/m".into(),
      source: "user".into(),
      arch: Some("qwen3".into()),
      quant: Some("Q5_K".into()),
      native_ctx: Some(32768),
      mode_hint: Some("chat".into()),
      parameter_label: Some("80B".into()),
      weights_bytes: Some(40_000_000_000),
      display_label: None,
      parse_error: None,
      split_siblings: vec![format!("{path}.part2"), format!("{path}.part3")],
      has_chat_template: true,
      has_reasoning_hint: false,
      tokenizer_kind: Some("qwen2".into()),
      total_parameters: Some(80_000_000_000),
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    }
  }

  #[test]
  fn shard_breakdown_lists_every_shard_with_its_individual_size() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("m-00001-of-00002.gguf");
    std::fs::write(&p, b"1234567890").unwrap(); // 10 bytes
    let s2 = dir.path().join("m-00002-of-00002.gguf");
    std::fs::write(&s2, b"abcdef").unwrap(); // 6 bytes
    let row = CatalogRow {
      path: p.display().to_string(),
      split_siblings: vec![s2.display().to_string()],
      ..fake_row("/m/x.gguf")
    };
    let shards = shard_breakdown(&row);
    assert_eq!(shards.len(), 2);
    assert_eq!(shards[0].path, p);
    assert_eq!(shards[0].bytes, 10);
    assert_eq!(shards[1].path, s2);
    assert_eq!(shards[1].bytes, 6);
  }

  #[test]
  fn shard_breakdown_surfaces_missing_siblings_as_zero_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("present.gguf");
    std::fs::write(&p, b"0123").unwrap();
    let row = CatalogRow {
      path: p.display().to_string(),
      split_siblings: vec!["/does/not/exist.gguf-2".into()],
      ..fake_row("/m/x.gguf")
    };
    let shards = shard_breakdown(&row);
    assert_eq!(shards.len(), 2);
    assert_eq!(shards[0].bytes, 4);
    assert_eq!(shards[1].bytes, 0, "missing sibling renders as 0 not panic");
  }

  #[test]
  fn render_human_lists_every_shard_for_multipart() {
    // Regression: previous render only emitted shard 1's path under
    // the row header; siblings appeared as bare paths without sizes.
    // The size section must now show one row per shard with its
    // individual byte count.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("m-00001-of-00002.gguf");
    std::fs::write(&p, vec![0u8; 1024 * 1024]).unwrap(); // 1 MiB
    let s2 = dir.path().join("m-00002-of-00002.gguf");
    std::fs::write(&s2, vec![0u8; 2 * 1024 * 1024]).unwrap(); // 2 MiB
    let row = CatalogRow {
      path: p.display().to_string(),
      split_siblings: vec![s2.display().to_string()],
      ..fake_row("/m/x.gguf")
    };
    let shards = shard_breakdown(&row);
    let envelope = json!({
      "size": { "on_disk_total_bytes": 3 * 1024 * 1024 },
      "arch_defaults": { "gpu_backend": "CpuOnly", "yaml": null, "builtin": {} },
      "last_params": null,
    });
    let rendered =
      console::strip_ansi_codes(&render_human(&row, &shards, 3 * 1024 * 1024, &envelope))
        .into_owned();
    assert!(
      rendered.contains("shard 1"),
      "shard 1 row missing:\n{rendered}"
    );
    assert!(
      rendered.contains("shard 2"),
      "shard 2 row missing:\n{rendered}"
    );
    assert!(
      rendered.contains("1.0 MiB"),
      "shard 1 size missing:\n{rendered}"
    );
    assert!(
      rendered.contains("2.0 MiB"),
      "shard 2 size missing:\n{rendered}"
    );
    assert!(
      rendered.contains("m-00001-of-00002.gguf"),
      "shard 1 basename missing:\n{rendered}"
    );
    assert!(
      rendered.contains("m-00002-of-00002.gguf"),
      "shard 2 basename missing:\n{rendered}"
    );
    // Single `path` line should NOT appear in the row header for
    // multipart entries — the per-shard rows cover the same ground.
    assert!(
      !rendered.contains(&format!("path  {}", p.display())),
      "multipart should not duplicate shard 1 path under the header:\n{rendered}"
    );
  }

  #[test]
  fn render_human_shows_running_block_with_clamp() {
    let row = fake_row("/m/x.gguf");
    let shards = shard_breakdown(&row);
    let envelope = json!({
      "size": { "on_disk_total_bytes": 0 },
      "arch_defaults": { "gpu_backend": "CpuOnly", "yaml": null, "builtin": {} },
      "last_params": null,
      "running": {
        "launch_id": "L1",
        "state": "ready",
        "port": 41100,
        "resolved_ctx": 16384,
        "ctx_clamped": true,
      },
    });
    let rendered =
      console::strip_ansi_codes(&render_human(&row, &shards, 0, &envelope)).into_owned();
    assert!(
      rendered.contains("running"),
      "running header missing:\n{rendered}"
    );
    assert!(
      rendered.contains("41100"),
      "running port missing:\n{rendered}"
    );
    assert!(
      rendered.contains("16384") && rendered.contains("clamped to fit-ctx floor"),
      "resolved ctx + clamp note missing:\n{rendered}"
    );
  }

  #[test]
  fn render_human_omits_running_block_when_not_live() {
    let row = fake_row("/m/x.gguf");
    let shards = shard_breakdown(&row);
    let envelope = json!({
      "size": { "on_disk_total_bytes": 0 },
      "arch_defaults": { "gpu_backend": "CpuOnly", "yaml": null, "builtin": {} },
      "last_params": null,
      "running": null,
    });
    let rendered =
      console::strip_ansi_codes(&render_human(&row, &shards, 0, &envelope)).into_owned();
    assert!(
      !rendered.contains("\nrunning"),
      "running block must be absent when nothing is live:\n{rendered}"
    );
  }

  #[test]
  fn shared_fmt_bytes_rolls_through_units() {
    // `show` now reuses the canonical `detection::fmt_bytes` so the
    // on-disk-size column and the memory surfaces never drift; keep a
    // boundary check here so a future change to the shared formatter
    // that breaks `show`'s thresholds is caught at this call site too.
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(1023), "1023 B");
    assert_eq!(fmt_bytes(1024), "1 KiB");
    assert!(fmt_bytes(2 * 1024 * 1024).starts_with("2.0 MiB"));
    assert!(fmt_bytes(3 * 1024 * 1024 * 1024).starts_with("3.0 GiB"));
  }

  #[test]
  fn assemble_envelope_reuses_the_shared_catalog_row_shape() {
    // Regression guard for the second-serializer bug: every key the shared
    // `CatalogRow` wire shape emits must land in the `show --json` envelope
    // byte-identically, so capability fields (multimodal / mtp /
    // supported_backends / split_siblings) can never lag `list --json` again.
    let mut row = fake_row("/m/x.gguf");
    row.supported_backends = vec!["ds4".into(), "llamacpp".into()];
    row.multimodal = Some(Multimodal {
      vision: true,
      audio: false,
    });
    row.mtp = Some(MtpCapability {
      embedded_layers: Some(1),
      separate_head: false,
    });
    let shared = serde_json::to_value(&row).unwrap();
    let envelope = assemble_envelope(
      &row,
      json!({"weights_bytes": row.weights_bytes, "shard_count": 3}),
      json!({"gpu_backend": "CpuOnly", "yaml": null, "builtin": {}}),
      None,
      None,
    );
    for (key, value) in shared.as_object().unwrap() {
      assert_eq!(
        envelope.get(key),
        Some(value),
        "shared key `{key}` diverged between the show envelope and the catalog row"
      );
    }
    assert_eq!(
      envelope.get("multimodal"),
      Some(&json!({"vision": true, "audio": false}))
    );
    assert_eq!(
      envelope.get("mtp"),
      Some(&json!({"embedded_layers": 1, "separate_head": false}))
    );
    assert_eq!(
      envelope.get("supported_backends"),
      Some(&json!(["ds4", "llamacpp"]))
    );
    // Show-only sections layer on top.
    for key in ["size", "arch_defaults", "last_params", "running"] {
      assert!(envelope.get(key).is_some(), "missing show section {key}");
    }
  }

  #[test]
  fn assemble_envelope_omits_model_id_when_unset() {
    // The shared serializer drops `model_id` rather than emitting null;
    // the show envelope must not resurrect the key.
    let mut row = fake_row("/m/x.gguf");
    row.model_id = None;
    let envelope = assemble_envelope(&row, json!({}), json!({}), None, None);
    assert!(
      envelope.get("model_id").is_none(),
      "model_id must be omitted when unset: {envelope}"
    );
  }

  #[test]
  fn render_human_metadata_lists_multimodal_and_mtp() {
    let mut row = fake_row("/m/x.gguf");
    row.multimodal = Some(Multimodal {
      vision: true,
      audio: true,
    });
    row.mtp = Some(MtpCapability {
      embedded_layers: Some(2),
      separate_head: true,
    });
    let shards = shard_breakdown(&row);
    let envelope = json!({
      "size": { "on_disk_total_bytes": 0 },
      "arch_defaults": { "gpu_backend": "CpuOnly", "yaml": null, "builtin": {} },
      "last_params": null,
    });
    let rendered =
      console::strip_ansi_codes(&render_human(&row, &shards, 0, &envelope)).into_owned();
    assert!(
      rendered.contains("vision + audio"),
      "multimodal row missing:\n{rendered}"
    );
    assert!(
      rendered.contains("embedded (2 layers) + separate head"),
      "mtp row missing:\n{rendered}"
    );
  }

  #[test]
  fn knobs_one_line_sorts_keys_and_drops_nulls() {
    let v = json!({
      "ctx": 8192,
      "reasoning": null,
      "n_gpu_layers": 99,
      "flash_attn": true,
    });
    let line = knobs_one_line(Some(&v));
    assert!(!line.contains("reasoning"));
    assert!(line.contains("ctx=8192"));
    assert!(line.contains("flash_attn=true"));
    assert!(line.contains("n_gpu_layers=99"));
    // Sorted alphabetically: ctx < flash_attn < n_gpu_layers.
    let ctx_idx = line.find("ctx=").unwrap();
    let flash_idx = line.find("flash_attn=").unwrap();
    let ngl_idx = line.find("n_gpu_layers=").unwrap();
    assert!(ctx_idx < flash_idx && flash_idx < ngl_idx);
  }

  #[test]
  fn knobs_one_line_returns_dash_for_empty_or_null() {
    assert_eq!(knobs_one_line(None), "—");
    assert_eq!(knobs_one_line(Some(&Value::Null)), "—");
    assert_eq!(knobs_one_line(Some(&json!({}))), "—");
    // All-null map collapses to dash too.
    assert_eq!(
      knobs_one_line(Some(&json!({"ctx": null, "reasoning": null}))),
      "—"
    );
  }
}
