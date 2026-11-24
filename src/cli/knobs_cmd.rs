//! `llamastash knobs` — what every backend declares it can be tuned with.
//!
//! Reads only the compiled-in registry, so it needs no daemon and answers the
//! same on any host.

use serde_json::{json, Value};

use crate::cli::cli_args::KnobsArgs;
use crate::cli::exit_codes::CliResult;
use crate::cli::output::pretty_json;
use crate::launch::knobs::{self, AutoKind, KnobDef, KnobKind};

/// One knob's value shape, as a short human token (`N`, `0.0-1.0`,
/// `on|off`, `auto|half|…`).
fn kind_label(def: &KnobDef) -> String {
  match def.kind {
    KnobKind::U32 { max: Some(m) } => format!("N (max {m})"),
    KnobKind::U32 { max: None } => "N".into(),
    KnobKind::F32 { min, max } => match (min, max) {
      (Some(lo), Some(hi)) => format!("{lo}-{hi}"),
      _ => "X".into(),
    },
    KnobKind::Bool => "on|off".into(),
    KnobKind::Enum { choices } => choices.join("|"),
    KnobKind::OpenEnum { choices, .. } => format!("{}|…", choices.join("|")),
    KnobKind::Ratio => "N,N[,…]".into(),
    KnobKind::Str => "TEXT".into(),
  }
}

/// What this knob's `auto` means, or `-` when it has no auto state.
fn auto_label(def: &KnobDef) -> &'static str {
  match def.auto {
    Some(AutoKind::Delegate) => "fit",
    Some(AutoKind::Capability) => "capability",
    None => "-",
  }
}

/// One listing section: a backend's compiled-in knobs, or one model's runtime
/// table (`model` set), in registry order then install order.
struct Section {
  backend: &'static str,
  model: Option<&'static str>,
  defs: &'static [KnobDef],
}

fn sections(backend_filter: Option<&str>) -> Vec<Section> {
  let static_rows = knobs::registry::by_backend()
    .into_iter()
    .map(|(backend, defs)| Section {
      backend,
      model: None,
      defs,
    });
  let scoped_rows = knobs::registry::scoped_all()
    .into_iter()
    .filter_map(|(scope, defs)| {
      let (backend, model) = knobs::registry::split_scope(scope)?;
      Some(Section {
        backend,
        model: Some(model),
        defs,
      })
    });
  static_rows
    .chain(scoped_rows)
    .filter(|s| backend_filter.is_none_or(|want| s.backend == want))
    .collect()
}

fn as_json(backend_filter: Option<&str>) -> Value {
  let backends: Vec<Value> = sections(backend_filter)
    .into_iter()
    .map(
      |Section {
         backend: id,
         model,
         defs,
       }| {
        let knobs: Vec<Value> = defs
          .iter()
          .map(|d| {
            let mut o = serde_json::Map::new();
            o.insert("id".into(), json!(d.id));
            o.insert("flag".into(), json!(d.emit_flag()));
            o.insert("kind".into(), json!(kind_label(d)));
            o.insert("help".into(), json!(d.help));
            o.insert("group".into(), json!(d.group.title()));
            o.insert("auto".into(), json!(d.auto.map(|_| auto_label(d))));
            o.insert("concept".into(), json!(d.concept.map(|c| c.neutral_flag())));
            let choices = d.kind.choices();
            if !choices.is_empty() {
              o.insert("choices".into(), json!(choices));
            }
            if !d.aliases.is_empty() {
              o.insert("aliases".into(), json!(d.aliases));
            }
            Value::Object(o)
          })
          .collect();
        let mut section = json!({ "backend": id, "knobs": knobs });
        if let Some(m) = model {
          section["model"] = json!(m);
        }
        section
      },
    )
    .collect();
  json!({ "backends": backends })
}

fn render_human(backend_filter: Option<&str>) -> String {
  use crate::cli::{colors, format};
  let mut out = String::new();
  let mut total = 0usize;
  for Section {
    backend: id,
    model,
    defs,
  } in sections(backend_filter)
  {
    if !out.is_empty() {
      out.push('\n');
    }
    // Bold when the terminal takes it, the bare id when piped, so `awk -F\t`
    // pipelines still key on the tables.
    let title = match model {
      Some(m) => format!("{id} {m}"),
      None => id.to_string(),
    };
    out.push_str(&format!("{}\n", colors::launch_id(&title)));
    let header = ["KNOB", "FLAG", "VALUE", "AUTO", "DESCRIPTION"];
    let rows: Vec<Vec<String>> = defs
      .iter()
      .map(|d| {
        total += 1;
        vec![
          d.id.to_string(),
          d.emit_flag(),
          kind_label(d),
          auto_label(d).to_string(),
          d.help.to_string(),
        ]
      })
      .collect();
    out.push_str(&format::table(&header, &rows));
  }
  if out.is_empty() {
    return format!(
      "{}\n",
      colors::dim(&format!(
        "(no backend named {})",
        backend_filter.unwrap_or("?")
      ))
    );
  }
  if console::colors_enabled() {
    out.push_str(&colors::count(total, "knobs"));
    out.push('\n');
  }
  out
}

pub async fn handle(args: KnobsArgs) -> CliResult {
  let filter = args.backend.as_deref();
  if args.json {
    println!("{}", pretty_json(&as_json(filter)));
  } else {
    print!("{}", render_human(filter));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn json_lists_every_backend_and_knob() {
    let v = as_json(None);
    // Runtime tables other tests install in this process come after, marked
    // with `model`; the compiled-in sections are the stable part.
    let backends: Vec<&Value> = v["backends"]
      .as_array()
      .unwrap()
      .iter()
      .filter(|b| b.get("model").is_none())
      .collect();
    assert_eq!(
      backends.len(),
      knobs::registry::by_backend().len(),
      "one entry per declaring backend"
    );
    let total: usize = backends
      .iter()
      .map(|b| b["knobs"].as_array().unwrap().len())
      .sum();
    assert_eq!(total, knobs::registry::iter().count());
  }

  #[test]
  fn a_json_row_carries_what_an_agent_needs_to_use_the_flag() {
    let v = as_json(None);
    let row = v["backends"][0]["knobs"][0].clone();
    for key in ["id", "flag", "kind", "help", "group"] {
      assert!(!row[key].is_null(), "{key} missing from {row}");
    }
    assert!(
      row["flag"].as_str().unwrap().starts_with("--"),
      "flag is spelled as the engine takes it: {row}"
    );
  }

  #[test]
  fn filtering_by_backend_narrows_to_that_backend() {
    let home = crate::backend::DEFAULT_BACKEND_ID;
    let v = as_json(Some(home));
    let backends = v["backends"].as_array().unwrap();
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0]["backend"], serde_json::json!(home));
  }

  #[test]
  fn an_unknown_backend_renders_an_empty_sentinel_rather_than_nothing() {
    let out = render_human(Some("definitely-not-a-backend"));
    assert!(out.contains("no backend named"), "{out}");
  }

  #[test]
  fn a_bounded_float_shows_its_range() {
    // The value column has to say more than "X", or `--gpu-memory-utilization
    // 7` looks reasonable right up until the launch fails.
    let bounded = knobs::registry::iter().find(|(_, d)| {
      matches!(
        d.kind,
        KnobKind::F32 {
          min: Some(_),
          max: Some(_)
        }
      )
    });
    let Some((_, def)) = bounded else { return };
    let label = kind_label(def);
    assert!(label.contains('-'), "expected a range, got {label:?}");
  }
}
