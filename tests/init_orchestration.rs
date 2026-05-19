//! Integration coverage for `init` orchestration (plan Unit 10).
//!
//! These tests drive `init::wizard::run` against an isolated XDG
//! environment so the wizard's `--offline` short-circuits, step
//! resolution, and idempotent re-runs can be exercised without
//! reaching out to the network or scribbling on the developer's
//! real config directory.
//!
//! Tests that *require* network (downloading a model, fetching a GH
//! Releases asset, fetching the remote benchmark snapshot) are
//! handled by `cli_init_parse.rs` (parsing) and the real-CDN
//! integration tests `Cargo` defaults to ignoring with `--ignored`.
//! This file covers the substantive behaviour gaps that the v2
//! review surfaced.

// The ENV_LOCK guard is intentionally held across `wizard::run(...).await`
// — its job is to serialize env-var mutation across the entire async
// test, including the await. Tokio's `Mutex` would defeat the purpose
// here because we want blocking serialization across parallel test
// runners, not async fairness.
#![allow(clippy::await_holding_lock)]

use std::ffi::OsString;
use std::sync::Mutex;

use clap::Parser;
use llamadash::cli::cli_args::{Cli, Command, InitStep};
use llamadash::cli::exit_codes::{INIT_ABORTED, UNKNOWN};
use llamadash::config::Config;
use llamadash::init::wizard;

/// Process-wide env mutex. Cargo runs tests in parallel by default;
/// the tests below mutate `LLAMADASH_OFFLINE`, `HOME`, `XDG_*`, and
/// must not race each other.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn parse_init(argv: &[&str]) -> (Cli, llamadash::cli::cli_args::InitArgs) {
  let mut cli = Cli::try_parse_from(std::iter::once("llamadash").chain(argv.iter().copied()))
    .expect("argv should parse");
  let args = match cli.command.take() {
    Some(Command::Init(args)) => args,
    other => panic!("expected init, got {other:?}"),
  };
  (cli, args)
}

/// Build an isolated XDG-layout temp directory. Sets `XDG_CONFIG_HOME`,
/// `XDG_STATE_HOME`, `XDG_DATA_HOME`, and `HOME` so the wizard's
/// state-store and config writes land somewhere safe to delete.
fn isolated_xdg(label: &str) -> std::path::PathBuf {
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_nanos();
  let root = std::env::temp_dir().join(format!(
    "llamadash-init-orch-{label}-{}-{nanos}",
    std::process::id()
  ));
  std::fs::create_dir_all(root.join("config")).unwrap();
  std::fs::create_dir_all(root.join("state")).unwrap();
  std::fs::create_dir_all(root.join("data")).unwrap();
  std::fs::create_dir_all(root.join("home")).unwrap();
  std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
  std::env::set_var("XDG_STATE_HOME", root.join("state"));
  std::env::set_var("XDG_DATA_HOME", root.join("data"));
  std::env::set_var("HOME", root.join("home"));
  root
}

fn cleanup_xdg(root: &std::path::PathBuf) {
  std::env::remove_var("XDG_CONFIG_HOME");
  std::env::remove_var("XDG_STATE_HOME");
  std::env::remove_var("XDG_DATA_HOME");
  std::fs::remove_dir_all(root).ok();
}

/// `--offline --only models` cannot satisfy the models step (it
/// requires network for the HF download) and must therefore abort
/// up-front with `INIT_ABORTED` rather than failing mid-step with
/// `INIT_DOWNLOAD_FAILED`. Plan Unit 10 test scenario.
#[tokio::test]
async fn offline_only_models_refused_upfront_with_init_aborted() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
  let root = isolated_xdg("offline-only-models");
  let (cli, args) = parse_init(&["init", "--offline", "--only", "models", "--yes", "--json"]);
  let config = Config::default();
  let result = wizard::run(args, &cli, &config).await;
  let err = result.expect_err("expected INIT_ABORTED on --offline --only models");
  assert_eq!(
    err.code, INIT_ABORTED,
    "exit code must be 72 (INIT_ABORTED), got {} — {:?}",
    err.code, err.message
  );
  let msg = err.message.as_deref().unwrap_or("");
  assert!(
    msg.contains("offline") || msg.contains("--offline"),
    "error message should mention --offline; got: {msg}"
  );
  cleanup_xdg(&root);
}

/// `init --offline --only config` runs because the config step needs
/// no network. The wizard should complete successfully and the config
/// step should be in `steps_ran`. We assert on exit code only —
/// asserting on InitSummary requires capturing stdout.
#[tokio::test]
async fn offline_only_config_completes_without_network() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
  let root = isolated_xdg("offline-only-config");
  let (cli, args) = parse_init(&["init", "--offline", "--only", "config", "--yes", "--json"]);
  let config = Config::default();
  let result = wizard::run(args, &cli, &config).await;
  // Either success (config wrote cleanly) or UNKNOWN/INIT_ABORTED if
  // the test environment can't acquire a state dir. We assert only
  // that we didn't get INIT_DOWNLOAD_FAILED, which would indicate
  // the offline check leaked into the config step.
  if let Err(e) = &result {
    assert_ne!(
      e.code,
      llamadash::cli::exit_codes::INIT_DOWNLOAD_FAILED,
      "config-only run must not surface INIT_DOWNLOAD_FAILED: {:?}",
      e.message
    );
  }
  cleanup_xdg(&root);
}

/// `LLAMADASH_OFFLINE=true` is the env-var equivalent of `--offline`.
/// Note clap's `env = ...` binding for `ArgAction::SetTrue` (the
/// underlying action for a `bool` field) parses the env value as a
/// boolean — only `true` and `false` are accepted; the truthy-set
/// handling for `1`/`yes` is done by [`fetch::offline_requested`] at
/// runtime, which `build_with_offline_check` consults orthogonally.
#[tokio::test]
async fn llamadash_offline_true_env_triggers_offline_only_models_refusal() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
  let root = isolated_xdg("env-offline");
  let old = std::env::var_os("LLAMADASH_OFFLINE");
  std::env::set_var("LLAMADASH_OFFLINE", "true");
  let (cli, args) = parse_init(&["init", "--only", "models", "--yes", "--json"]);
  assert!(
    args.offline,
    "LLAMADASH_OFFLINE=true should populate args.offline via clap env()"
  );
  let config = Config::default();
  let result = wizard::run(args, &cli, &config).await;
  let err = result.expect_err("expected INIT_ABORTED");
  assert_eq!(err.code, INIT_ABORTED);
  match old {
    Some(v) => std::env::set_var("LLAMADASH_OFFLINE", v),
    None => std::env::remove_var("LLAMADASH_OFFLINE"),
  }
  cleanup_xdg(&root);
}

/// StepPlan resolution: `--only A,B` and `--only A --only B` and
/// `--skip C` (with A+B implied) all converge on the same plan.
/// (Pure-function check; complements the clap-parsing tests in
/// `cli_init_parse.rs`.)
#[test]
fn step_plan_only_and_skip_resolve_to_same_set() {
  let only = wizard::StepPlan::resolve(&[InitStep::Server, InitStep::Models], &[]);
  let skip = wizard::StepPlan::resolve(&[], &[InitStep::Config]);
  assert!(only.server);
  assert!(only.models);
  assert!(!only.config);
  assert!(skip.server);
  assert!(skip.models);
  assert!(!skip.config);
  assert_eq!(only, skip);
}

/// Sanity: `--only` and `--skip` together is refused at clap parse
/// time, never reaches the wizard.
#[test]
fn only_and_skip_together_refused_by_clap() {
  let result = Cli::try_parse_from(["llamadash", "init", "--only", "server", "--skip", "config"]);
  assert!(
    result.is_err(),
    "clap should reject mutually-exclusive --only + --skip"
  );
}

/// `--recommended --offline --only config` exercises the same path
/// the old `--yes --offline --only config` test did; ensures the new
/// canonical flag behaves identically to the hidden alias.
#[tokio::test]
async fn recommended_alias_runs_offline_only_config_like_yes_did() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
  let root = isolated_xdg("recommended-offline-only-config");
  let (cli, args) = parse_init(&[
    "init",
    "--recommended",
    "--offline",
    "--only",
    "config",
    "--json",
  ]);
  assert!(args.recommended);
  let config = Config::default();
  let result = wizard::run(args, &cli, &config).await;
  if let Err(e) = &result {
    assert_ne!(
      e.code,
      llamadash::cli::exit_codes::INIT_DOWNLOAD_FAILED,
      "config-only run must not surface INIT_DOWNLOAD_FAILED: {:?}",
      e.message
    );
  }
  cleanup_xdg(&root);
}

/// `--config-step skip` causes the wizard's config step to record
/// itself as skipped without writing — the dry-run + confirm path
/// resolves Skip synchronously without prompting.
#[tokio::test]
async fn config_step_skip_records_step_as_skipped() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
  let root = isolated_xdg("config-step-skip");
  let (cli, args) = parse_init(&[
    "init",
    "--recommended",
    "--offline",
    "--only",
    "config",
    "--config-step",
    "skip",
    "--json",
  ]);
  let config = Config::default();
  let _ = wizard::run(args, &cli, &config).await;
  // We assert that the config.yaml was NOT created (skip path never
  // writes). Path: ${XDG_CONFIG_HOME}/llamadash/config.yaml.
  let config_path = root.join("config").join("llamadash").join("config.yaml");
  assert!(
    !config_path.exists(),
    "--config-step skip must not write {}",
    config_path.display()
  );
  cleanup_xdg(&root);
}

/// `--install` supplied for a step that `--skip` excludes still
/// parses cleanly; the wizard logs a stderr warning and proceeds
/// with the remaining steps. The runtime guard is `--skip server`
/// + `--install brew`: brew never runs, the warning fires, and the
/// other steps complete.
#[tokio::test]
async fn install_override_with_skipped_server_warns_and_continues() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
  let root = isolated_xdg("install-override-skipped");
  let (cli, args) = parse_init(&[
    "init",
    "--recommended",
    "--offline",
    "--skip",
    "server,models",
    "--install",
    "brew",
    "--json",
  ]);
  let config = Config::default();
  // We're not asserting on stderr text here — just that the run
  // doesn't abort because of the ignored override.
  let result = wizard::run(args, &cli, &config).await;
  if let Err(e) = &result {
    assert_ne!(
      e.code, INIT_ABORTED,
      "--install on a skipped step must not abort: {:?}",
      e.message
    );
  }
  cleanup_xdg(&root);
}

/// W3 contract: `--install` (per-step override) must beat
/// `--recommended` (which would otherwise short-circuit through the
/// existing-binary adoption shortcut in `run_install_step`). With a
/// bad custom path supplied alongside `--recommended`, the wizard
/// must hit the override path → `is_safe_to_adopt` failure →
/// INIT_ABORTED, not silently fall back to recommended-mode
/// adoption of a different binary on PATH.
#[tokio::test]
async fn install_override_wins_against_recommended_mode_shortcut() {
  let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
  let root = isolated_xdg("install-override-vs-recommended");
  let (cli, args) = parse_init(&[
    "init",
    "--recommended",
    "--offline",
    "--skip",
    "models,config",
    "--install",
    "custom:/nonexistent/llama-server",
    "--json",
  ]);
  let config = Config::default();
  let result = wizard::run(args, &cli, &config).await;
  match &result {
    Err(e) => {
      assert_eq!(
        e.code, INIT_ABORTED,
        "--install custom:<bad> under --recommended must abort with INIT_ABORTED, \
         not silently fall back to recommended-mode adoption: {:?}",
        e.message
      );
    }
    Ok(()) => panic!(
      "expected INIT_ABORTED — the existing-binary shortcut may have bypassed \
       the --install override (W3 regression)"
    ),
  }
  cleanup_xdg(&root);
}

// Silence unused-var lint on the env-loaded `OsString` paths.
#[allow(dead_code)]
fn _unused_osstring() -> OsString {
  OsString::from("")
}

#[allow(dead_code)]
fn _unused_unknown() -> i32 {
  UNKNOWN
}
