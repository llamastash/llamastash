//! Boot-time preload: start the models the operator asked to keep warm,
//! from `daemon.preload:` and presets that pin `preload: true`.
//!
//! Entries launch as [`LaunchOrigin::Manual`], so the idle sweep never unloads
//! them. Launches run sequentially in list order: two concurrent launches read
//! the same free-memory sample and can both be admitted against memory that
//! fits only one. List order is also priority order: the first entry wins the
//! memory. A failed entry logs one line and never fails the boot.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::daemon::context::MethodContext;
use crate::daemon::launch_service::{
  compose_and_spawn, LaunchModeWire, LaunchSelection, StartParams,
};
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::supervisor::{LaunchOrigin, ManagedModel, ManagedState};
use crate::launch::mode::LaunchMode;
use crate::launch::presets::KeyClass;
use crate::launch::presets::{materialize_preset, NamedPreset};
use crate::launch::resolve::{parse_named_reference, CatalogRow};

/// How long to wait for the first discovery scan to fill the catalog before
/// resolving references against an empty one. Preload names models the way the
/// CLI does, and that needs the scan.
const CATALOG_GRACE: Duration = Duration::from_secs(30);

/// Upper bound on waiting for one preloaded launch to leave Loading. The
/// supervisor errors on its own probe deadline; this only bounds the waiter.
const SETTLE_GRACE: Duration = Duration::from_secs(900);

/// One preload entry after resolution: this model, optionally replaying this
/// preset's parameters.
struct PreloadLaunch {
  /// What the operator wrote, for the log line.
  label: String,
  model_path: PathBuf,
  /// The preset whose knobs this launch replays. `Some` makes the launch
  /// addressable as `<model>@<preset>` (the daemon names a manual launch after
  /// the preset it resolved), which is what makes a `<model>@<preset>` preload
  /// entry round-trip.
  preset: Option<NamedPreset>,
}

/// Start every configured preload, in order. Called as a supervised background
/// task once the daemon is fully up (see `daemon::mod`).
pub async fn run(ctx: MethodContext, entries: Vec<String>, shutdown: ShutdownToken) {
  let launches = collect(&ctx, &entries).await;
  if launches.is_empty() {
    return;
  }
  log::info!("preload: starting {} configured model(s)", launches.len());
  for launch in launches {
    if shutdown.is_triggered() {
      log::info!("preload: daemon shutting down, remaining entries skipped");
      return;
    }
    match compose_and_spawn(&ctx, start_params(&launch), LaunchOrigin::Manual).await {
      Ok(started) => {
        for warning in &started.warnings {
          log::warn!("preload {}: {warning}", launch.label);
        }
        match await_settled(&started.model).await {
          Ok(()) => log::info!(
            "preload: {} ready on port {}",
            launch.label,
            started.model.port()
          ),
          Err(cause) => log::warn!("preload: {} did not come up — {cause}", launch.label),
        }
      }
      Err(e) => log::warn!("preload: {} not started — {}", launch.label, e.message),
    }
  }
}

/// Resolve every entry into a launch, list first then the presets that opted in
/// on their own. Unresolvable entries are logged and dropped here, so the caller
/// only ever sees work it can do.
async fn collect(ctx: &MethodContext, entries: &[String]) -> Vec<PreloadLaunch> {
  collect_with_rows(ctx, entries, &wait_for_catalog(ctx).await).await
}

async fn collect_with_rows(
  ctx: &MethodContext,
  entries: &[String],
  rows: &[CatalogRow],
) -> Vec<PreloadLaunch> {
  let store = ctx.presets.snapshot().await;
  let mut out: Vec<PreloadLaunch> = Vec::new();
  for entry in entries {
    match resolve_entry(entry, rows, ctx).await {
      Ok(launch) => out.push(launch),
      Err(why) => log::warn!("preload: skipping `{entry}` — {why}"),
    }
  }
  for (key, block) in &store {
    for (name, body) in &block.entries {
      if !body.preload {
        continue;
      }
      let path = match preload_target(key, rows) {
        Ok(path) => path,
        Err(why) => {
          log::warn!("preload: skipping preset `{name}` under `{key}` — {why}");
          continue;
        }
      };
      // The explicit list already covers this model (that entry picks its
      // preset, `default:` or none); don't start it twice.
      if out.iter().any(|l| l.model_path == path) {
        continue;
      }
      out.push(PreloadLaunch {
        label: format!("{key}@{name}"),
        preset: Some(materialize_preset(name, body, path.clone())),
        model_path: path,
      });
    }
  }
  out
}

/// The one model a `preload: true` entry may start.
///
/// A preloaded launch is manual intent, so neither the sweep nor make-room can
/// ever take it back. A key that scopes a family would therefore pin every model
/// in it — an arch preset could fill the host one sequential launch at a time,
/// each waiting on its own load, until admission refuses. One `preload: true`,
/// one model; name the model (or a glob that matches exactly one).
pub(crate) fn preload_target(key: &str, rows: &[CatalogRow]) -> Result<PathBuf, String> {
  match preset_models(key, rows).as_slice() {
    [] => Err("no discovered model matches this preset key".to_string()),
    [one] => Ok(one.clone()),
    many => Err(format!(
      "this key scopes {} models; `preload: true` needs a key that names exactly one",
      many.len()
    )),
  }
}

/// The models a preset key applies to, using the same classification
/// [`effective_presets`] uses: an arch key takes every row of that arch, a
/// wildcard or exact per-model key takes the rows it matches.
fn preset_models(key: &str, rows: &[CatalogRow]) -> Vec<PathBuf> {
  if crate::launch::presets::classify_preset_key(key, rows) == KeyClass::Arch {
    return rows
      .iter()
      .filter(|r| {
        r.arch
          .as_deref()
          .is_some_and(|arch| arch.eq_ignore_ascii_case(key))
      })
      .map(|r| PathBuf::from(&r.path))
      .collect();
  }
  rows
    .iter()
    .filter(|r| crate::launch::presets::preset_key_matches(key, &r.name(), &r.path))
    .map(|r| PathBuf::from(&r.path))
    .collect()
}

/// One `daemon.preload:` entry: a launch file, a `<model>@<preset>` address, or
/// a plain model reference (which takes the model's own `default:` preset, the
/// same thing a bare `llamastash start` does).
async fn resolve_entry(
  entry: &str,
  rows: &[CatalogRow],
  ctx: &MethodContext,
) -> Result<PreloadLaunch, String> {
  if let Some(path) = parse_launch_file(entry, rows)? {
    return Ok(path);
  }
  let (model_ref, preset_name) = match parse_named_reference(entry) {
    Some((model, name)) => (model, Some(name)),
    None => (entry, None),
  };
  let model_path = resolve_path(model_ref, rows)?;
  let preset = match preset_name {
    Some(name) => {
      let arch = arch_for(rows, &model_path);
      let eff = crate::launch::presets::effective_presets(
        &crate::util::paths::model_file_label(&model_path),
        &model_path.display().to_string(),
        arch.as_deref(),
        &ctx.presets.snapshot().await,
        rows,
      );
      match eff.named(name) {
        Some(np) => Some(clone_with_path(np, model_path.clone())),
        None => return Err(format!("no preset `{name}` for this model")),
      }
    }
    None => None,
  };
  Ok(PreloadLaunch {
    label: entry.to_string(),
    model_path,
    preset,
  })
}

/// A launch-file preload entry: the same file `llamastash run <file>` takes,
/// parsed by the same code so the two cannot disagree about what the file runs.
fn parse_launch_file(entry: &str, rows: &[CatalogRow]) -> Result<Option<PreloadLaunch>, String> {
  let path = match launch_file_path(entry, None) {
    Some(path) => path,
    None => return Ok(None),
  };
  let sel = crate::cli::launch_file::load(&path, None).map_err(|e| {
    e.message
      .unwrap_or_else(|| format!("cannot read launch file `{}`", path.display()))
  })?;
  let model_path = resolve_path(&sel.model_key, rows)?;
  Ok(Some(PreloadLaunch {
    label: entry.to_string(),
    preset: Some(materialize_preset(
      &sel.preset_name,
      &sel.body,
      model_path.clone(),
    )),
    model_path,
  }))
}

/// The launch file an entry names, once a leading `~` has been expanded to
/// `home` (the process home when that is `None`).
///
/// Expansion comes first because `is_launch_file` wants the extension *and* an
/// existing file, and neither is true of a literal `~/launches/big.yaml`.
fn launch_file_path(entry: &str, home: Option<&Path>) -> Option<PathBuf> {
  let path = crate::util::paths::expand_user_path_in(Path::new(entry), home);
  crate::cli::launch_file::is_launch_file(&path.to_string_lossy()).then_some(path)
}

/// Resolve a preload reference to a catalog path.
///
/// An absolute or `~`-rooted reference is a path by declaration, so it is read
/// from disk (a model outside every scan root still preloads) and never offered
/// to the fuzzy matcher. A bare name goes to the catalog first: a relative name
/// resolves against the *daemon's* working directory, which is not the
/// operator's, so letting it win over the catalog turns a model name that
/// happens to match a directory into the wrong launch. A catalog miss then falls
/// back to the relative path, so a file next to the daemon's cwd still works.
fn resolve_path(reference: &str, rows: &[CatalogRow]) -> Result<PathBuf, String> {
  let declared = crate::util::paths::expand_user_path(Path::new(reference));
  if declared.is_absolute() {
    if declared.is_file() || declared.is_dir() {
      return crate::util::paths::canonicalize(&declared)
        .map_err(|e| format!("cannot resolve `{reference}`: {e}"));
    }
    return Err(format!("`{reference}` is not a readable path"));
  }
  match crate::launch::resolve::resolve_model_with_candidates(rows, reference) {
    Ok(row) => Ok(PathBuf::from(row.path)),
    Err(_) if declared.is_file() || declared.is_dir() => {
      crate::util::paths::canonicalize(&declared)
        .map_err(|e| format!("cannot resolve `{reference}`: {e}"))
    }
    Err(_) => Err(format!(
      "`{reference}` does not name exactly one discovered model"
    )),
  }
}

/// The `general.architecture` of a catalog path, for preset arch keys.
fn arch_for(rows: &[CatalogRow], path: &Path) -> Option<String> {
  let path_str = path.display().to_string();
  rows
    .iter()
    .find(|r| r.path == path_str)
    .and_then(|r| r.arch.clone())
}

/// `NamedPreset` over a specific model path — `effective_presets` materialises
/// over the caller's path already, this is for the address path where the preset
/// was looked up by name after the path was known.
fn clone_with_path(preset: &NamedPreset, path: PathBuf) -> NamedPreset {
  let mut copy = preset.clone();
  copy.params.model_path = path;
  copy
}

/// Start params for one preloaded launch. A preset-backed entry sends its
/// resolved params the way the CLI flattens one client-side (`Explicit`, so no
/// `last_params` inheritance) plus the preset name for the running row; a plain
/// reference sends nothing and lets the daemon apply the model's `default:`.
fn start_params(launch: &PreloadLaunch) -> StartParams {
  let Some(preset) = &launch.preset else {
    return StartParams {
      model_path: launch.model_path.clone(),
      ..Default::default()
    };
  };
  let params = &preset.params;
  StartParams {
    model_path: launch.model_path.clone(),
    preset: Some(preset.name.clone()),
    selection: LaunchSelection::Explicit,
    ctx: params.ctx,
    reasoning: Some(params.reasoning),
    knobs: params.knobs.clone(),
    extras: params
      .extras
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect(),
    mmproj_path: params.mmproj_path.clone(),
    backend: Some(params.backend.clone()),
    server: params.server.clone(),
    // A preset pinning `Chat` is indistinguishable from pinning nothing, and the
    // daemon's own mode resolution (preset pin > header hint) handles that case
    // better than a forced `chat` on the wire.
    mode: match params.mode {
      LaunchMode::Chat | LaunchMode::Image | LaunchMode::Tts | LaunchMode::Stt => None,
      LaunchMode::Embedding => Some(LaunchModeWire::Embedding),
      LaunchMode::Rerank => Some(LaunchModeWire::Rerank),
    },
    ..Default::default()
  }
}

/// Poll until the launch leaves Loading. Bounded by [`SETTLE_GRACE`] so a wedged
/// child cannot stall the entries behind it.
async fn await_settled(model: &ManagedModel) -> Result<(), String> {
  let deadline = Instant::now() + SETTLE_GRACE;
  loop {
    match model.state().await {
      ManagedState::Ready => return Ok(()),
      ManagedState::Error { cause, .. } => return Err(cause),
      ManagedState::Stopped => return Err("supervisor exited before Ready".into()),
      ManagedState::Stopping => return Err("supervisor stopped while launching".into()),
      ManagedState::Launching | ManagedState::Loading => {
        if Instant::now() >= deadline {
          return Err(format!("still loading after {SETTLE_GRACE:?}"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
      }
    }
  }
}

/// Wait for the first discovery scan, so a preload name resolves the way it
/// would from the CLI. On a timeout the (still empty) catalog is handed back and
/// every entry logs its own refusal rather than hanging the daemon.
async fn wait_for_catalog(ctx: &MethodContext) -> Vec<CatalogRow> {
  let deadline = Instant::now() + CATALOG_GRACE;
  loop {
    let rows = crate::ipc::methods::catalog_rows(ctx).await;
    if !rows.is_empty() || Instant::now() >= deadline {
      return rows;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(path: &str, arch: Option<&str>) -> CatalogRow {
    CatalogRow {
      path: path.to_string(),
      model_id: None,
      parent: "/m".to_string(),
      source: "user".to_string(),
      arch: arch.map(str::to_string),
      quant: None,
      native_ctx: None,
      mode_hint: None,
      parameter_label: None,
      weights_bytes: None,
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    }
  }

  #[test]
  fn resolve_path_prefers_an_existing_file_over_the_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("outside-scan-root.gguf");
    std::fs::write(&model, b"x").expect("write");
    let rows = vec![row("/elsewhere/model.gguf", None)];
    let got = resolve_path(model.to_str().unwrap(), &rows).expect("path resolves directly");
    assert_eq!(got, crate::util::paths::canonicalize(&model).unwrap());
  }

  #[test]
  fn resolve_path_falls_back_to_the_catalog_resolver() {
    let rows = vec![row("/m/demo-Q4_K_M.gguf", Some("llama"))];
    let got = resolve_path("demo-Q4_K_M", &rows).expect("catalog resolves");
    assert_eq!(got, PathBuf::from("/m/demo-Q4_K_M.gguf"));
    assert!(resolve_path("nothing-like-this", &rows).is_err());
  }

  /// A bare name must not be read as a filesystem path first: the daemon's
  /// working directory is not the operator's, so a model name that happens to
  /// name an existing directory would otherwise launch the directory.
  #[test]
  fn resolve_path_does_not_let_a_relative_name_beat_the_catalog() {
    // `Cargo.toml` is a real file next to every test's working directory, so
    // this proves the order without leaving anything behind: the catalog is
    // consulted first, and the relative path is only a fallback after a miss.
    let rows = vec![row("/m/Cargo.toml", Some("llama"))];
    let got = resolve_path("Cargo.toml", &rows).expect("catalog wins over the cwd entry");
    assert_eq!(got, PathBuf::from("/m/Cargo.toml"));
  }

  /// An absolute path that does not exist is a bad path, not a fuzzy model name.
  #[test]
  fn resolve_path_rejects_a_missing_absolute_path() {
    let rows = vec![row("/m/demo.gguf", Some("llama"))];
    // From a tempdir, because `/no/such` has no drive prefix and so is not
    // absolute on Windows.
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-model.gguf");
    let err = resolve_path(missing.to_str().unwrap(), &rows).unwrap_err();
    assert!(err.contains("not a readable path"), "{err}");
  }

  /// A preset key preloads exactly the models the read side scopes it to: an
  /// arch key warms every model of that arch, a glob every model it matches, and
  /// an exact key just its own.
  #[test]
  fn preset_models_scopes_arch_glob_and_exact_keys() {
    let rows = vec![
      row("/repos/unsloth/a-Q4_K_M.gguf", Some("qwen3")),
      row("/repos/unsloth/b-Q4_K_M.gguf", Some("qwen3")),
      row("/repos/other/c-Q4_K_M.gguf", Some("llama")),
    ];
    let mut arch = preset_models("qwen3", &rows);
    arch.sort();
    assert_eq!(
      arch,
      vec![
        PathBuf::from("/repos/unsloth/a-Q4_K_M.gguf"),
        PathBuf::from("/repos/unsloth/b-Q4_K_M.gguf"),
      ]
    );

    let mut glob = preset_models("unsloth/*", &rows);
    glob.sort();
    assert_eq!(glob, arch, "the same two rows the arch key found");

    assert_eq!(
      preset_models("c-Q4_K_M.gguf", &rows),
      vec![PathBuf::from("/repos/other/c-Q4_K_M.gguf")]
    );
    assert!(preset_models("no-such-model.gguf", &rows).is_empty());
  }

  /// A `~` entry has to be expanded before the launch-file sniff: the check wants
  /// an existing file, and `~/x.yaml` is not a path the filesystem knows.
  #[test]
  fn tilde_launch_file_entry_is_a_launch_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("lf.yaml");
    std::fs::write(&file, "presets:\n  x:\n    entries: {}\n").expect("launch file");
    assert_eq!(
      launch_file_path("~/lf.yaml", Some(dir.path())).as_deref(),
      Some(file.as_path()),
      "`~` has to be expanded before the file check"
    );
    assert_eq!(launch_file_path("~/missing.yaml", Some(dir.path())), None);
  }

  /// One `preload: true` starts one model. A key that scopes a family is refused
  /// with its count, because a preloaded launch can never be unloaded again.
  #[test]
  fn preload_needs_a_key_that_names_exactly_one_model() {
    let rows = vec![
      row("/repos/unsloth/a-Q4_K_M.gguf", Some("qwen3")),
      row("/repos/unsloth/b-Q4_K_M.gguf", Some("qwen3")),
    ];
    assert_eq!(
      preload_target("a-Q4_K_M.gguf", &rows).unwrap(),
      PathBuf::from("/repos/unsloth/a-Q4_K_M.gguf")
    );
    let arch = preload_target("qwen3", &rows).unwrap_err();
    assert!(arch.contains("2 models"), "{arch}");
    let glob = preload_target("unsloth/*", &rows).unwrap_err();
    assert!(glob.contains("2 models"), "{glob}");
    assert!(preload_target("nope.gguf", &rows)
      .unwrap_err()
      .contains("no discovered model"));
  }

  #[test]
  fn plain_reference_sends_a_default_selection_and_no_preset() {
    let launch = PreloadLaunch {
      label: "demo".into(),
      model_path: PathBuf::from("/m/demo.gguf"),
      preset: None,
    };
    let params = start_params(&launch);
    assert_eq!(params.model_path, PathBuf::from("/m/demo.gguf"));
    assert!(params.preset.is_none());
    assert_eq!(params.selection, LaunchSelection::Default);
  }

  #[test]
  fn preset_backed_entry_sends_its_params_and_name() {
    let body = crate::config::PresetBody {
      idle_ttl_secs: Some(60),
      preload: true,
      ..Default::default()
    };
    let preset = materialize_preset("coding", &body, PathBuf::from("/m/demo.gguf"));
    let launch = PreloadLaunch {
      label: "demo@coding".into(),
      model_path: PathBuf::from("/m/demo.gguf"),
      preset: Some(preset),
    };
    let params = start_params(&launch);
    assert_eq!(params.preset.as_deref(), Some("coding"));
    assert_eq!(params.selection, LaunchSelection::Explicit);
    assert_eq!(params.mode, None, "a Chat mode stays off the wire");
  }

  /// An entry that names nothing is logged and dropped; the entries around it
  /// still launch, in order. Rows are passed in so the test does not sit out the
  /// catalog grace wait on an empty catalog.
  #[tokio::test]
  async fn collect_skips_unresolvable_entries_and_keeps_the_rest_in_order() {
    let ctx = MethodContext::new(ShutdownToken::new());
    let rows = vec![
      row("/m/first-Q4_K_M.gguf", Some("llama")),
      row("/m/last-Q4_K_M.gguf", Some("llama")),
    ];
    let launches = collect_with_rows(
      &ctx,
      &[
        "first-Q4_K_M".into(),
        "definitely-not-a-model".into(),
        "last-Q4_K_M".into(),
      ],
      &rows,
    )
    .await;
    assert_eq!(
      launches
        .iter()
        .map(|l| l.model_path.clone())
        .collect::<Vec<_>>(),
      vec![
        PathBuf::from("/m/first-Q4_K_M.gguf"),
        PathBuf::from("/m/last-Q4_K_M.gguf"),
      ],
      "the bad entry is skipped, the good two keep their order"
    );
  }
}
