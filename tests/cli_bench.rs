//! Integration coverage for `llamastash bench`: the numbers come back from the
//! proxy's request log, the launch is stopped on every path, and the up-front
//! refusals cost no launch.
//!
//! The daemon runs as its own process against an isolated state dir, the way a
//! user runs it, and the fake engine behind it answers the chat requests the
//! bench sends.

#![cfg(all(feature = "test-fixtures", unix))]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use llamastash::gguf::test_fixtures::FixtureBuilder;

fn bin() -> Command {
  Command::new(env!("CARGO_BIN_EXE_llamastash"))
}

fn fake_server() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-bench", label)
}

/// A port nothing is holding. The proxy prefers 11435, which the developer's own
/// daemon already has, so every test binds its proxy on a port of its own.
fn free_port() -> u16 {
  let l = TcpListener::bind("127.0.0.1:0").expect("free port");
  let port = l.local_addr().expect("addr").port();
  drop(l);
  port
}

/// Two saved presets, so a cycle has more than one row to produce. The key is the
/// model's file name, which is how a per-model `presets:` block names its model.
const PRESETS: &str = "presets:\n  chat.gguf:\n    entries:\n      fast:\n        knobs:\n          ctx_size: 2048\n      slow:\n        knobs:\n          ctx_size: 4096\n";

/// The two presets plus one that cannot answer a chat prompt: it pins the launch
/// to embedding mode, so it fails after the load rather than at the parse.
const BROKEN_PRESETS: &str = "presets:\n  chat.gguf:\n    entries:\n      sunk:\n        knobs:\n          mode: embedding\n      fast:\n        knobs:\n          ctx_size: 2048\n";

/// A daemon with a chat model, an embedding model, and two saved presets.
struct Daemon {
  state: PathBuf,
  cache: PathBuf,
  model_dir: PathBuf,
  config: PathBuf,
}

impl Daemon {
  /// `presets` is appended to the config, so the daemon resolves the names the
  /// same way a hand-edited `config.yaml` does.
  fn start(label: &str, presets: &str) -> Self {
    let state = temp(label);
    let cache = temp("cache");
    let model_dir = temp("models");
    std::fs::write(
      model_dir.join("chat.gguf"),
      FixtureBuilder::new()
        .with_arch("llama")
        .with_chat_template("{% for m in messages %}{{ m.content }}{% endfor %}")
        .build(),
    )
    .expect("write chat fixture model");
    // A model that cannot carry a chat session at all.
    std::fs::write(
      model_dir.join("embed.gguf"),
      FixtureBuilder::new().with_arch("bert").build(),
    )
    .expect("write embedding fixture model");
    let proxy_port = free_port();
    // Each daemon needs its own launch range too, or parallel tests both hand
    // their fake engine port 41100 and the second launch dies on bind.
    let launch_start = free_port();
    let config = state.join("config.yaml");
    std::fs::write(
      &config,
      format!(
        "proxy:\n  port: {proxy_port}\ndaemon:\n  port_range:\n    start: {launch_start}\n    end: {}\n{presets}",
        launch_start.saturating_add(15),
      ),
    )
    .expect("write config");

    let daemon = Self {
      state,
      cache,
      model_dir,
      config,
    };
    let out = daemon
      .cmd()
      .args(["daemon", "start", "--proxy-port", &proxy_port.to_string()])
      .output()
      .expect("daemon start");
    assert!(
      out.status.success(),
      "daemon start failed: {}",
      String::from_utf8_lossy(&out.stderr)
    );
    daemon.wait_for_catalog();
    daemon
  }

  /// The no-presets daemon: one plain chat model and nothing saved.
  fn plain(label: &str) -> Self {
    Self::start(label, "")
  }

  /// Wait until the daemon's first scan has listed the fixture models.
  fn wait_for_catalog(&self) {
    for _ in 0..150 {
      let out = self.cmd().args(["list", "--json"]).output().expect("list");
      let body = String::from_utf8_lossy(&out.stdout).into_owned();
      if body.contains("chat.gguf") && body.contains("embed.gguf") {
        return;
      }
      std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the catalog never listed the fixture models");
  }

  /// `llamastash` wired to this daemon: isolated state, cache, and config, and the
  /// fake engine as the launcher's `llama-server`.
  fn cmd(&self) -> Command {
    let mut cmd = bin();
    cmd
      .env("LLAMASTASH_STATE_DIR", &self.state)
      .env("LLAMASTASH_CACHE_DIR", &self.cache)
      .env("LLAMASTASH_CONFIG_DIR", self.state.join("config-dir"))
      .arg("--config")
      .arg(&self.config)
      .arg("--llama-server")
      .arg(fake_server())
      .arg("--model-path")
      .arg(&self.model_dir);
    cmd
  }

  /// Run one command that has to succeed, and say what it printed when it did not.
  fn run(&self, args: &[&str]) -> std::process::Output {
    let out = self.cmd().args(args).output().expect("run");
    assert!(
      out.status.success(),
      "`{args:?}` failed: {}{}",
      stdout(&out),
      stderr(&out)
    );
    out
  }

  /// Run one command expected to fail, with no assertion on the code.
  fn try_run(&self, args: &[&str]) -> std::process::Output {
    self.cmd().args(args).output().expect("run")
  }

  /// How many launches the daemon holds, which is how these tests check that a
  /// bench left nothing running.
  fn running(&self) -> usize {
    let out = self
      .cmd()
      .args(["status", "--json"])
      .output()
      .expect("status");
    stdout_json(&out)["models"]
      .as_array()
      .expect("models array")
      .len()
  }

  fn start_model(&self, model: &str) {
    self
      .cmd()
      .args(["start", model, "--wait", "--json"])
      .output()
      .expect("start");
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
    // Stops the supervised engine before the dirs go, the same way a user would.
    let _ = bin()
      .env("LLAMASTASH_STATE_DIR", &self.state)
      .arg("--config")
      .arg(&self.config)
      .args(["stop", "--all", "--yes"])
      .output();
    let _ = bin()
      .env("LLAMASTASH_STATE_DIR", &self.state)
      .arg("--config")
      .arg(&self.config)
      .args(["daemon", "stop"])
      .output();
    let _ = std::fs::remove_dir_all(&self.state);
    let _ = std::fs::remove_dir_all(&self.cache);
    let _ = std::fs::remove_dir_all(&self.model_dir);
  }
}

fn stdout(out: &std::process::Output) -> String {
  String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
  String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout_json(out: &std::process::Output) -> Value {
  serde_json::from_slice(&out.stdout).expect("valid json on stdout")
}

/// The one result of a single-preset bench run.
fn only_result(out: &std::process::Output) -> Value {
  let body = stdout_json(out);
  let results = body["results"].as_array().expect("results array");
  assert_eq!(results.len(), 1, "one preset asked for one row: {body}");
  results[0].clone()
}

/// The server reported its own speeds, so the row carries them unmarked, and the
/// launch is gone afterwards.
#[test]
fn a_bench_run_reports_the_server_reported_speeds_and_stops_what_it_started() {
  let daemon = Daemon::plain("speeds");
  let out = daemon.run(&[
    "bench",
    "chat",
    "--runs",
    "2",
    "--prompt",
    "__TEST_EMIT_TIMINGS__",
    "--json",
  ]);
  assert_eq!(out.status.code(), Some(0), "a clean bench exits 0");
  let result = only_result(&out);
  assert!(result["preset"].is_null(), "no preset asked for null");
  assert!(result["error"].is_null(), "{}", result["error"]);
  assert_eq!(result["tokens_per_second"], 38.64);
  assert_eq!(result["tokens_per_second_estimated"], false);
  // The proxy works prompt speed out of the reported token count and millisecond
  // count, so it lands beside the engine's own figure rather than on it.
  let pp = result["prefill_tokens_per_second"]
    .as_f64()
    .expect("a reported prefill speed");
  assert!((pp - 38.9).abs() < 0.1, "{pp} is the reported 38.9");
  assert_eq!(result["prefill_estimated"], false);
  assert!(
    result["ttft_ms"].is_number(),
    "a streamed row carries the first-token time: {result}"
  );
  assert!(
    result["load_ms"].is_number(),
    "load time is measured: {result}"
  );
  assert_eq!(result["samples"].as_array().unwrap().len(), 2);
  assert_eq!(result["runs_done"], 2);
  assert_eq!(daemon.running(), 0, "bench stopped the launch it started");
}

/// A server that reports token counts and no speed gets the proxy's estimate,
/// marked as one, exactly as `llamastash requests` shows it.
#[test]
fn a_server_with_no_reported_speed_gets_the_estimate_marked() {
  let daemon = Daemon::plain("estimate");
  let out = daemon.run(&[
    "bench",
    "chat",
    "--runs",
    "1",
    "--prompt",
    "__TEST_EMIT_USAGE__",
    "--json",
  ]);
  let result = only_result(&out);
  assert_eq!(
    result["tokens_per_second"].as_f64().map(|v| v > 0.0),
    Some(true),
    "the estimate is a number: {result}"
  );
  assert_eq!(result["tokens_per_second_estimated"], true);
  assert_eq!(daemon.running(), 0);
}

/// A server that reports neither counts nor speed leaves the speed out of the
/// row rather than inventing a zero for it.
#[test]
fn a_server_with_no_numbers_at_all_leaves_the_speeds_null() {
  let daemon = Daemon::plain("nospeed");
  let out = daemon.run(&[
    "bench", "chat", "--runs", "1", "--prompt", "hello", "--json",
  ]);
  assert_eq!(out.status.code(), Some(0));
  let result = only_result(&out);
  assert!(result["tokens_per_second"].is_null(), "{result}");
  assert!(result["prefill_tokens_per_second"].is_null());
  assert_eq!(result["runs_done"], 1, "the run still happened");
  assert_eq!(daemon.running(), 0);
}

/// The table, not `--json`: the placeholder stands in for what never arrived, and
/// the run counts show per row.
#[test]
fn the_piped_table_prints_one_row_with_placeholders() {
  let daemon = Daemon::plain("table");
  let out = daemon.run(&["bench", "chat", "--runs", "1", "--prompt", "hello"]);
  let text = stdout(&out);
  let lines: Vec<&str> = text.lines().collect();
  assert_eq!(
    lines[0],
    "PRESET\tLOAD\tPP tok/s\tTG tok/s\tTTFT\tRUNS\tNOTE"
  );
  assert_eq!(lines.len(), 2, "one row: {lines:?}");
  let cells: Vec<&str> = lines[1].split('\t').collect();
  assert_eq!(cells[0], "default");
  assert_ne!(cells[1], "-", "the load time is always measured");
  assert_eq!(cells[2], "-");
  assert_eq!(cells[3], "-");
  assert_eq!(cells[5], "1/1");
  assert_eq!(cells[6], "-");
  assert_eq!(daemon.running(), 0);
}

/// `--preset auto` is pure fit: one row labelled `auto`, and the launch stays
/// unnamed, as it does on `start --preset auto`.
#[test]
fn the_auto_preset_is_its_own_row_and_stays_unnamed() {
  let daemon = Daemon::start("auto-preset", PRESETS);
  let out = daemon.run(&[
    "bench",
    "chat",
    "--preset",
    "auto",
    "--runs",
    "1",
    "--prompt",
    "__TEST_EMIT_TIMINGS__",
    "--json",
  ]);
  let result = only_result(&out);
  assert_eq!(result["preset"], "auto");
  assert_eq!(result["tokens_per_second"], 38.64);
  assert_eq!(daemon.running(), 0);
}

/// Two presets, two rows, in the order asked for, each from its own launch, and
/// both launches gone.
#[test]
fn every_named_preset_gets_its_own_row_and_its_own_launch() {
  let daemon = Daemon::start("two-presets", PRESETS);
  let out = daemon.run(&[
    "bench",
    "chat",
    "--preset",
    "fast,slow",
    "--runs",
    "1",
    "--prompt",
    "__TEST_EMIT_TIMINGS__",
    "--json",
  ]);
  let body = stdout_json(&out);
  assert_eq!(body["model"], "chat.gguf");
  assert_eq!(body["runs"], 1);
  let results = body["results"].as_array().unwrap();
  assert_eq!(results.len(), 2);
  assert_eq!(results[0]["preset"], "fast");
  assert_eq!(results[1]["preset"], "slow");
  assert!(
    results[0]["tokens_per_second"].is_number(),
    "{}",
    results[0]
  );
  assert!(
    results[1]["tokens_per_second"].is_number(),
    "{}",
    results[1]
  );
  assert_ne!(
    results[0]["launch_id"], results[1]["launch_id"],
    "each preset launched its own run"
  );
  assert_eq!(daemon.running(), 0, "both launches were stopped");
}

/// A preset whose launch comes up as something other than chat is that row's
/// error. The next preset is still measured, stdout stays parseable, and the
/// failed launch is stopped like any other.
#[test]
fn a_preset_that_comes_up_non_chat_is_an_error_row_and_the_next_one_still_runs() {
  let daemon = Daemon::start("non-chat-preset", BROKEN_PRESETS);
  let out = daemon.try_run(&[
    "bench",
    "chat",
    "--preset",
    "sunk,fast",
    "--runs",
    "1",
    "--prompt",
    "__TEST_EMIT_TIMINGS__",
    "--json",
  ]);
  assert_eq!(
    out.status.code(),
    Some(67),
    "a preset with no numbers is LAUNCH_FAILED"
  );
  let body = stdout_json(&out);
  let results = body["results"].as_array().expect("results array");
  assert_eq!(results.len(), 2, "both presets were attempted: {body}");
  assert_eq!(results[0]["preset"], "sunk");
  let err = results[0]["error"].as_str().expect("an error row");
  assert!(err.contains("embedding"), "names the mode: {err}");
  assert!(
    results[0]["tokens_per_second"].is_null(),
    "a failed preset measures nothing: {}",
    results[0]
  );
  assert_eq!(results[1]["preset"], "fast");
  assert!(
    results[1]["tokens_per_second"].is_number(),
    "the next preset is still measured: {}",
    results[1]
  );
  assert_eq!(results[1]["error"], serde_json::Value::Null);
  assert_eq!(daemon.running(), 0, "the failed launch was stopped too");
}

/// An unknown preset name is a usage error raised before any launch, so a typo
/// costs no model load.
#[test]
fn an_unknown_preset_refuses_before_anything_starts() {
  let daemon = Daemon::start("bad-preset", PRESETS);
  let out = daemon.try_run(&["bench", "chat", "--preset", "nope"]);
  assert_eq!(out.status.code(), Some(64), "USAGE: {}", stderr(&out));
  assert!(stderr(&out).contains("nope"), "{}", stderr(&out));
  assert_eq!(daemon.running(), 0, "nothing was started");
}

/// A model that already runs is refused by name, and the refusal leaves the
/// running launch alone.
#[test]
fn a_model_that_already_runs_is_refused_and_left_alone() {
  let daemon = Daemon::plain("already-running");
  daemon.start_model("chat");
  assert_eq!(daemon.running(), 1);
  let out = daemon.try_run(&["bench", "chat"]);
  assert_eq!(out.status.code(), Some(64), "USAGE: {}", stderr(&out));
  assert!(
    stderr(&out).contains("L1"),
    "names the launch: {}",
    stderr(&out)
  );
  assert_eq!(daemon.running(), 1, "the running launch is untouched");
}

/// `--mode chat` is what bench launches anyway, so it is accepted like every
/// other `start` override. Only the modes that cannot answer the prompt are.
#[test]
fn the_chat_mode_override_is_accepted() {
  let daemon = Daemon::plain("mode-chat");
  let out = daemon.run(&[
    "bench",
    "chat",
    "--mode",
    "chat",
    "--runs",
    "1",
    "--prompt",
    "__TEST_EMIT_TIMINGS__",
    "--json",
  ]);
  assert_eq!(only_result(&out)["tokens_per_second"], 38.64);
  assert_eq!(daemon.running(), 0);
}

/// Only chat answers the prompt bench sends, so a non-chat row is refused up
/// front rather than failing per run.
#[test]
fn an_embedding_model_is_refused() {
  let daemon = Daemon::plain("embedding");
  let out = daemon.try_run(&["bench", "embed"]);
  assert_eq!(out.status.code(), Some(64), "USAGE: {}", stderr(&out));
  assert!(stderr(&out).contains("embedding"), "{}", stderr(&out));
  assert_eq!(daemon.running(), 0);
}

/// A prompt from a file is the prompt the runs send.
#[test]
fn a_prompt_file_replaces_the_builtin_prompt() {
  let daemon = Daemon::plain("prompt-file");
  let path = daemon.state.join("p.txt");
  std::fs::write(&path, b"__TEST_EMIT_TIMINGS__\n").expect("write prompt");
  let out = daemon.run(&[
    "bench",
    "chat",
    "--runs",
    "1",
    "--prompt-file",
    path.to_str().unwrap(),
    "--json",
  ]);
  let result = only_result(&out);
  assert_eq!(result["tokens_per_second"], 38.64, "{result}");
  assert_eq!(daemon.running(), 0);
}

/// The built-in prompt reaches the engine too: the fake engine echoes nothing
/// back about it, so what proves it arrived is that the run completed with the
/// full request made.
#[test]
fn the_builtin_prompt_runs_without_any_prompt_flag() {
  let daemon = Daemon::plain("builtin-prompt");
  let out = daemon.run(&["bench", "chat", "--runs", "1", "--json"]);
  assert_eq!(out.status.code(), Some(0));
  assert_eq!(only_result(&out)["runs_done"], 1);
  assert_eq!(daemon.running(), 0);
}
