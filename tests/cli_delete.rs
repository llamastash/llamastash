//! Integration coverage for `llamastash delete`: the daemon removes the files
//! and drops the row, and every path that must not delete something says so with
//! its own exit code.
//!
//! The daemon runs as its own process against an isolated state dir, the way a
//! user runs it. Nothing here has a TTY, which is also what makes the
//! needs-`--yes` refusals testable.

#![cfg(all(feature = "test-fixtures", unix))]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use llamastash::gguf::test_fixtures::FixtureBuilder;

fn bin() -> Command {
  Command::new(env!("CARGO_BIN_EXE_llamastash"))
}

fn temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-del", label)
}

/// A port nothing is holding, so the developer's own daemon is never in play.
fn free_port() -> u16 {
  let l = TcpListener::bind("127.0.0.1:0").expect("free port");
  let port = l.local_addr().expect("addr").port();
  drop(l);
  port
}

/// A daemon over a model dir holding one chat model and one that is running.
struct Daemon {
  state: PathBuf,
  cache: PathBuf,
  model_dir: PathBuf,
  config: PathBuf,
}

impl Daemon {
  fn start(label: &str) -> Self {
    let state = temp(label);
    let cache = temp("cache");
    let model_dir = temp("models");
    std::fs::write(
      model_dir.join("gone.gguf"),
      FixtureBuilder::new()
        .with_arch("llama")
        .with_chat_template("{% for m in messages %}{{ m.content }}{% endfor %}")
        .build(),
    )
    .expect("write fixture model");
    std::fs::write(
      model_dir.join("busy.gguf"),
      FixtureBuilder::new()
        .with_arch("llama")
        .with_chat_template("{% for m in messages %}{{ m.content }}{% endfor %}")
        .build(),
    )
    .expect("write fixture model");
    let proxy_port = free_port();
    let launch_start = free_port();
    let config = state.join("config.yaml");
    std::fs::write(
      &config,
      format!(
        "proxy:\n  port: {proxy_port}\ndaemon:\n  port_range:\n    start: {launch_start}\n    end: {}\n",
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

  /// Wait until the daemon's first scan has listed both fixture models.
  fn wait_for_catalog(&self) {
    for _ in 0..150 {
      let out = self.cmd().args(["list", "--json"]).output().expect("list");
      let body = String::from_utf8_lossy(&out.stdout).into_owned();
      if body.contains("gone.gguf") && body.contains("busy.gguf") {
        return;
      }
      std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the catalog never listed the fixture models");
  }

  /// `llamastash` wired to this daemon: isolated state, cache and config, and the
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
      .arg(env!("CARGO_BIN_EXE_fake_llama_server"))
      .arg("--model-path")
      .arg(&self.model_dir);
    cmd
  }

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

  /// Run one command and hand back the raw output, for a case that asserts on
  /// stderr as well as the code.
  fn try_run(&self, args: &[&str]) -> std::process::Output {
    self.cmd().args(args).output().expect("run")
  }

  /// Run one command expected to fail, and return its exit code.
  fn fail(&self, args: &[&str]) -> i32 {
    let out = self.cmd().args(args).output().expect("run");
    assert!(
      !out.status.success(),
      "`{args:?}` was expected to fail: {}",
      stdout(&out)
    );
    out.status.code().expect("exit code")
  }

  fn model_path(&self, name: &str) -> PathBuf {
    self.model_dir.join(name)
  }

  /// Names the catalog still lists, as `list --json` reports them.
  fn listed(&self) -> Vec<String> {
    let out = self.cmd().args(["list", "--json"]).output().expect("list");
    let body = stdout_json(&out);
    body["models"]
      .as_array()
      .expect("models array")
      .iter()
      .filter_map(|row| row["path"].as_str())
      .map(str::to_string)
      .collect()
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
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

/// Every path that must not delete something, in one daemon: no terminal
/// without `--yes`, `--json` without `--yes`, and a reference that resolves to
/// nothing. Nothing on disk changes on any of them.
#[test]
fn a_delete_that_cannot_run_says_which_rule_it_hit() {
  let daemon = Daemon::start("refusals");
  let path = daemon.model_path("gone.gguf");
  // `Command::output()` gives the child no terminal, which is the case here.
  assert_eq!(daemon.fail(&["delete", "gone"]), 64);
  assert!(path.exists(), "the no-TTY refusal removes nothing");
  assert_eq!(daemon.fail(&["delete", "gone", "--json"]), 64);
  assert!(path.exists(), "the --json refusal removes nothing");
  // An unresolved reference stays a model problem (66), not a delete failure.
  assert_eq!(daemon.fail(&["delete", "no-such-model", "--yes"]), 66);
  assert!(daemon.model_path("busy.gguf").exists());
}

/// `--yes --json` deletes: the file is gone, the reply says so, and the catalog
/// no longer lists it.
#[test]
fn a_delete_with_yes_removes_the_file_and_the_row() {
  let daemon = Daemon::start("yes");
  let path = daemon.model_path("gone.gguf");
  assert!(path.exists());

  let out = daemon.run(&["delete", "gone", "--yes", "--json"]);
  let body = stdout_json(&out);
  assert_eq!(body["deleted"], Value::Bool(true), "{body}");
  assert_eq!(body["path"], Value::String(path.display().to_string()));
  assert_eq!(body["files"].as_array().expect("files").len(), 1);
  assert!(body["message"]
    .as_str()
    .unwrap_or_default()
    .contains("deleted"));
  assert!(!path.exists(), "the file is gone");
  assert!(
    !daemon.listed().iter().any(|p| p.contains("gone.gguf")),
    "the catalog stopped listing it"
  );
  assert!(
    daemon.listed().iter().any(|p| p.contains("busy.gguf")),
    "and left the model next to it alone"
  );
}

/// A launch still holds the file, so the daemon refuses and the CLI reports the
/// refusal as its own failure class.
#[test]
fn a_running_model_cannot_be_deleted() {
  let daemon = Daemon::start("running");
  daemon.run(&["start", "busy", "--wait", "--json"]);
  let path = daemon.model_path("busy.gguf");
  let out = daemon.try_run(&["delete", "busy", "--yes"]);
  assert_eq!(out.status.code(), Some(75), "stderr: {}", stderr(&out));
  assert!(
    stderr(&out).contains("stop the launch first"),
    "the refusal reaches the user: {}",
    stderr(&out)
  );
  assert!(path.exists(), "the running model keeps its file");
}
