//! Integration coverage for `llamastash session`: the exec'd tool gets the
//! proxy address and the published model id in its environment, the tool's own
//! `--` args arrive unchanged, a dry run prints the documented JSON, and a tool
//! that is not installed is a binary-not-found exit.
//!
//! The daemon runs as its own process against an isolated state dir, the way a
//! user runs it, because `session` replaces the calling process — a test
//! process must not be the one that gets replaced.

#![cfg(all(feature = "test-fixtures", unix))]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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
  llamastash::test_support::unique_temp_dir("ls-sess", label)
}

/// A port nothing is holding. The proxy prefers 11435, which the developer's own
/// daemon already has, so every test binds its proxy on a port of its own.
fn free_port() -> u16 {
  let l = TcpListener::bind("127.0.0.1:0").expect("free port");
  let port = l.local_addr().expect("addr").port();
  drop(l);
  port
}

/// A daemon with one chat model in its catalog and the fake engine behind it.
struct Daemon {
  state: PathBuf,
  cache: PathBuf,
  model_dir: PathBuf,
  config: PathBuf,
  proxy_port: u16,
}

impl Daemon {
  /// `proxy_api_key` is written into the config the CLI reads, which is what
  /// decides whether a dry run masks the key.
  fn start(proxy_api_key: Option<&str>) -> Self {
    let state = temp("state");
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
    // A GGUF whose mode hint the parser cannot read, which is most of them once
    // they carry no chat template.
    std::fs::write(
      model_dir.join("nohint.gguf"),
      llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
    )
    .expect("write hint-less fixture model");
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
    let mut body = format!(
      "proxy:\n  port: {proxy_port}\ndaemon:\n  port_range:\n    start: {launch_start}\n    end: {}\n",
      launch_start.saturating_add(15)
    );
    if let Some(key) = proxy_api_key {
      body.push_str(&format!("  api_key: \"{key}\"\n"));
    }
    std::fs::write(&config, body).expect("write config");

    let daemon = Self {
      state,
      cache,
      model_dir,
      config,
      proxy_port,
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

  /// Wait until the daemon's first scan has listed the fixture models.
  ///
  /// `daemon start` answers as soon as the control plane binds, and the scan
  /// runs after that. The daemon inherits the host's `HF_HOME`, so on a
  /// developer's machine it walks a real model cache first and can take seconds
  /// to publish a catalog; a `start` in that window exits 66 (`no model
  /// matches`, 0 known). Empty CI caches never hit this.
  fn wait_for_catalog(&self) {
    for _ in 0..150 {
      let out = self
        .cmd()
        .args(["list", "--json"])
        .output()
        .expect("list catalog");
      let body = String::from_utf8_lossy(&out.stdout).into_owned();
      if ["chat.gguf", "nohint.gguf", "embed.gguf"]
        .iter()
        .all(|name| body.contains(name))
      {
        return;
      }
      std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the catalog never listed the fixture models");
  }

  /// `llamastash` wired to this daemon: isolated state, cache, and config, and
  /// the fake engine as the launcher's `llama-server`.
  fn cmd(&self) -> Command {
    self.cmd_with_config(&self.config)
  }

  /// The same command against a different `config.yaml`. What a dry run masks
  /// depends on whether the caller resolves a real key, and that is a
  /// caller-side config question, not a daemon one.
  fn cmd_with_config(&self, config: &Path) -> Command {
    let mut cmd = bin();
    cmd
      .env("LLAMASTASH_STATE_DIR", &self.state)
      .env("LLAMASTASH_CACHE_DIR", &self.cache)
      .env("LLAMASTASH_CONFIG_DIR", self.state.join("config-dir"))
      .arg("--config")
      .arg(config)
      .arg("--llama-server")
      .arg(fake_server())
      .arg("--model-path")
      .arg(&self.model_dir);
    cmd
  }

  /// Start the catalog model and wait for it, so `session` has a running launch.
  fn start_model(&self) {
    let out = self
      .cmd()
      .args(["start", "chat", "--wait", "--json"])
      .output()
      .expect("start");
    assert!(
      out.status.success(),
      "start failed ({}): {}{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    );
  }

  /// How many launches the daemon has. A session that fails must leave this at 0.
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

  /// A config that enforces a bearer key, for what a dry run may print.
  fn keyed_config(&self) -> PathBuf {
    let keyed = self.state.join("keyed.yaml");
    std::fs::write(
      &keyed,
      format!(
        "proxy:\n  port: {}\n  api_key: \"sk-real-key\"\n",
        self.proxy_port
      ),
    )
    .expect("write keyed config");
    keyed
  }

  fn sessions_dir(&self) -> PathBuf {
    self.cache.join("sessions")
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
    std::fs::remove_dir_all(&self.state).ok();
    std::fs::remove_dir_all(&self.cache).ok();
    std::fs::remove_dir_all(&self.model_dir).ok();
  }
}

/// A stand-in for a coding tool that records what it was started with.
fn fake_tool(dir: &Path, name: &str, dump: &Path) {
  std::fs::create_dir_all(dir).expect("tool dir");
  std::fs::write(
    dir.join(name),
    format!(
      "#!/bin/sh\n{{ echo \"argv:$*\"; env | grep -E '^(ANTHROPIC|CLAUDE|LLAMASTASH|AIDER|OPENCODE)' | sort; }} > '{}'\n",
      dump.display()
    ),
  )
  .expect("write fake tool");
  let mut perms = std::fs::metadata(dir.join(name))
    .expect("stat")
    .permissions();
  perms.set_mode(0o755);
  std::fs::set_permissions(dir.join(name), perms).expect("chmod");
}

fn path_with(dir: &Path) -> String {
  format!(
    "{}:{}",
    dir.display(),
    std::env::var("PATH").unwrap_or_default()
  )
}

fn stdout_json(out: &std::process::Output) -> Value {
  serde_json::from_slice(&out.stdout).expect("valid json on stdout")
}

#[test]
fn the_executed_tool_gets_the_proxy_its_model_and_the_trailing_args() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let tools = temp("tools");
  std::fs::create_dir_all(&tools).expect("tools dir");
  let dump = tools.join("dump");
  fake_tool(&tools, "claude", &dump);

  let out = daemon
    .cmd()
    .env("PATH", path_with(&tools))
    // The whole point of `env_remove`: a real key in the parent env would
    // compete with the auth token the session sets.
    .env("ANTHROPIC_API_KEY", "must-not-reach-the-tool")
    .args(["session", "claude", "--", "--foo"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "session failed: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  let note = String::from_utf8_lossy(&out.stderr).to_string();
  assert!(note.contains("claude → chat (L1"), "note: {note}");

  let seen = std::fs::read_to_string(&dump).expect("fake tool ran and dumped env");
  let lines: Vec<&str> = seen.lines().collect();
  assert_eq!(
    lines.first().copied(),
    Some("argv:--foo"),
    "the tool's args arrive last and unchanged: {seen}"
  );
  assert!(
    lines.contains(&format!("ANTHROPIC_BASE_URL=http://127.0.0.1:{}", daemon.proxy_port).as_str()),
    "base url: {seen}"
  );
  assert!(lines.contains(&"ANTHROPIC_MODEL=chat"), "model id: {seen}");
  assert!(
    lines.contains(&"ANTHROPIC_SMALL_FAST_MODEL=chat"),
    "small model: {seen}"
  );
  assert!(
    lines.contains(&"ANTHROPIC_AUTH_TOKEN=llamastash"),
    "keyless stub: {seen}"
  );
  assert!(
    !seen.contains("ANTHROPIC_API_KEY"),
    "the removed var must be gone: {seen}"
  );
}

#[test]
fn a_dry_run_prints_the_launch_and_changes_nothing() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let out = daemon
    .cmd()
    .args([
      "session",
      "codex",
      "--dry-run",
      "--json",
      "--",
      "exec",
      "hi",
    ])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "dry run failed: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  let body = stdout_json(&out);
  assert_eq!(body["tool"], "codex");
  assert_eq!(body["model"], "chat");
  assert_eq!(body["running"], true);
  assert_eq!(body["launch_id"], "L1");
  assert_eq!(body["program"], "codex");
  assert_eq!(body["env"]["LLAMASTASH_API_KEY"], "llamastash");
  assert_eq!(body["tool_args"], serde_json::json!(["exec", "hi"]));
  let args = body["args"].as_array().expect("args array");
  assert!(
    !args.contains(&Value::String("exec".into())),
    "the pass-through is listed apart from the tool's own args: {args:?}"
  );
  assert!(
    args.contains(&Value::String("-c".into())),
    "codex takes -c overrides: {args:?}"
  );
  assert!(
    body["files"].as_array().expect("files array").is_empty(),
    "codex writes no file"
  );
  assert!(body["context_window"].is_number(), "ctx: {body}");

  // Under enforced auth only the bearer key is a secret. Everything else has to
  // print as it is, or the line cannot be pasted back. The daemon is unaffected:
  // a dry run never reads the key from it.
  let keyed = daemon.keyed_config();
  let dry = |tool: &str| {
    let out = daemon
      .cmd_with_config(&keyed)
      .args(["session", tool, "--dry-run", "--json"])
      .output()
      .expect("run masked dry run");
    assert!(
      out.status.success(),
      "{} dry run failed: {}{}",
      tool,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    );
    stdout_json(&out)
  };
  let body = dry("codex");
  assert_eq!(
    body["env"]["LLAMASTASH_API_KEY"], "***",
    "the key must not be printed: {body}"
  );

  let body = dry("claude");
  assert_eq!(
    body["env"]["ANTHROPIC_AUTH_TOKEN"], "***",
    "claude carries the key as its auth token: {body}"
  );
  assert_eq!(
    body["env"]["ANTHROPIC_BASE_URL"],
    format!("http://127.0.0.1:{}", daemon.proxy_port),
    "the address a masked dry run prints must still be runnable: {body}"
  );
  assert_eq!(body["env"]["ANTHROPIC_MODEL"], "chat", "{body}");

  let body = dry("aider");
  assert_eq!(body["env"]["AIDER_OPENAI_API_KEY"], "***", "{body}");
  assert_eq!(
    body["env"]["AIDER_OPENAI_API_BASE"],
    format!("http://127.0.0.1:{}/v1", daemon.proxy_port),
    "{body}"
  );
  assert_eq!(body["env"]["AIDER_MODEL"], "openai/chat", "{body}");

  // OpenCode takes the key through an env var, so its config content holds no
  // secret and prints whole.
  let body = dry("opencode");
  let content = body["env"]["OPENCODE_CONFIG_CONTENT"]
    .as_str()
    .expect("opencode config content");
  assert!(
    !content.contains("sk-real-key"),
    "the key is not in that content: {content}"
  );
  assert!(content.contains("baseURL"), "{content}");
  assert!(
    content.contains("{env:LLAMASTASH_API_KEY}"),
    "the key is referenced, never inlined: {content}"
  );
}

#[test]
fn a_dry_run_writes_no_file_anywhere() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  // pi is the one tool whose launch needs a generated file.
  let out = daemon
    .cmd()
    .args(["session", "pi", "--dry-run", "--json"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "{}",
    String::from_utf8_lossy(&out.stderr)
  );
  assert_eq!(stdout_json(&out)["files"].as_array().unwrap().len(), 1);
  assert!(
    !daemon.sessions_dir().exists(),
    "a dry run describes a file, it does not write one"
  );
}

#[test]
fn a_pi_session_writes_its_extension_under_the_cache_dir() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let tools = temp("tools");
  let dump = tools.join("dump");
  fake_tool(&tools, "pi", &dump);
  let out = daemon
    .cmd()
    .env("PATH", path_with(&tools))
    .args(["session", "pi"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "session failed: {}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  let written: Vec<PathBuf> = std::fs::read_dir(daemon.sessions_dir())
    .expect("sessions dir written under the cache dir")
    .filter_map(|e| e.ok().map(|e| e.path()))
    .filter(|p| {
      p.file_name()
        .map(|n| n.to_string_lossy().starts_with("pi-"))
        .unwrap_or(false)
    })
    .collect();
  assert_eq!(written.len(), 1, "one extension file for one session");
  let mode = std::fs::metadata(&written[0])
    .expect("the file the session wrote")
    .permissions()
    .mode();
  assert_eq!(
    mode & 0o777,
    0o600,
    "the extension names no secret but stays private"
  );
}

#[test]
fn a_pi_dry_run_names_the_file_its_extension_argument_points_at() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let out = daemon
    .cmd()
    .args(["session", "pi", "chat", "--dry-run", "--json"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "{}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  let body = stdout_json(&out);
  let path = body["files"][0]["path"]
    .as_str()
    .unwrap_or_default()
    .to_string();
  let args: Vec<&str> = body["args"]
    .as_array()
    .map(|a| a.iter().filter_map(Value::as_str).collect())
    .unwrap_or_default();
  let at = args
    .iter()
    .position(|a| *a == "-e")
    .expect("an -e argument");
  assert_eq!(
    args.get(at + 1).copied().unwrap_or_default(),
    path,
    "the argument loads the very file the session writes"
  );
  let path = Path::new(&path);
  assert!(path.is_absolute(), "{path:?} is not absolute");
  assert_eq!(
    path.parent(),
    Some(daemon.sessions_dir().as_path()),
    "the extension sits in the cache dir"
  );
}

#[test]
fn a_tool_that_writes_no_file_needs_no_cache_dir() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  // No HOME and no XDG cache means `cache_dir()` resolves to nothing. Only a
  // tool that generates a file has to care.
  let out = daemon
    .cmd()
    .env_remove("LLAMASTASH_CACHE_DIR")
    .env_remove("HOME")
    .env_remove("XDG_CACHE_HOME")
    .args(["session", "claude", "chat", "--dry-run", "--json"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "no cache dir is no obstacle for a tool that writes nothing: {}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  assert_eq!(stdout_json(&out)["program"], "claude");
}

#[test]
fn no_tool_named_is_a_usage_error_that_lists_the_tools() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let json = daemon
    .cmd()
    .args(["session", "--dry-run", "--json"])
    .output()
    .expect("run session");
  assert_eq!(
    json.status.code(),
    Some(64),
    "{}{}",
    String::from_utf8_lossy(&json.stdout),
    String::from_utf8_lossy(&json.stderr)
  );
  let message = stdout_json(&json)["error"]["message"]
    .as_str()
    .unwrap_or_default()
    .to_string();
  for tool in ["claude", "codex", "opencode", "pi", "aider"] {
    assert!(
      message.contains(tool),
      "the message names {tool}: {message}"
    );
  }
  // Without `--json` the same refusal is the plain error line.
  let plain = daemon
    .cmd()
    .args(["session"])
    .output()
    .expect("run session");
  assert_eq!(plain.status.code(), Some(64));
  let err = String::from_utf8_lossy(&plain.stderr);
  for tool in ["claude", "codex", "opencode", "pi", "aider"] {
    assert!(err.contains(tool), "the error line names {tool}: {err}");
  }
}

/// A dir holding a stand-in `claude`. A missing tool exits 70 before the model is
/// looked at, so a test about the model needs one on `PATH`, whatever the host
/// has installed.
fn path_with_fake_claude() -> String {
  let tools = temp("tools");
  fake_tool(&tools, "claude", &tools.join("dump"));
  path_with(&tools)
}

#[test]
fn nothing_running_and_no_model_named_is_a_model_not_found() {
  let daemon = Daemon::start(None);
  let out = daemon
    .cmd()
    .env("PATH", path_with_fake_claude())
    .args(["session", "claude"])
    .output()
    .expect("run");
  assert_eq!(
    out.status.code(),
    Some(66),
    "{}",
    String::from_utf8_lossy(&out.stderr)
  );
  assert_eq!(daemon.running(), 0, "nothing was started");
}

#[test]
fn a_model_that_cannot_carry_a_chat_session_is_refused_before_it_starts() {
  let daemon = Daemon::start(None);
  let out = daemon
    .cmd()
    .env("PATH", path_with_fake_claude())
    .args(["session", "claude", "embed"])
    .output()
    .expect("run");
  assert_eq!(
    out.status.code(),
    Some(64),
    "{}",
    String::from_utf8_lossy(&out.stderr)
  );
  assert!(
    String::from_utf8_lossy(&out.stderr)
      .to_string()
      .contains("need a chat model"),
    "the message says why"
  );
  assert_eq!(
    daemon.running(),
    0,
    "an embedding model never gets launched"
  );
}

#[test]
fn a_model_that_is_not_running_is_reported_not_started_in_a_dry_run() {
  let daemon = Daemon::start(None);
  let out = daemon
    .cmd()
    .args(["session", "codex", "chat", "--dry-run", "--json"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "dry run failed: {}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  let body = stdout_json(&out);
  assert_eq!(body["running"], false, "{body}");
  assert_eq!(body["launch_id"], Value::Null, "{body}");
}

#[test]
fn a_model_with_no_mode_hint_starts_as_chat_for_a_session() {
  let daemon = Daemon::start(None);
  let tools = temp("tools");
  let dump = tools.join("dump");
  fake_tool(&tools, "codex", &dump);
  let out = daemon
    .cmd()
    .env("PATH", path_with(&tools))
    .args(["session", "codex", "nohint"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "session failed: {}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  // A session can only be chat, so the mode is stated rather than left for the
  // daemon to refuse over.
  let note = String::from_utf8_lossy(&out.stderr).to_string();
  assert!(note.contains("codex → nohint (L1"), "note: {note}");
  assert!(std::fs::read_to_string(&dump).is_ok(), "the tool ran");
}

/// `@auto` is pure fit, so the launch it starts carries no preset name, exactly
/// like `start --preset auto`. A stamped `auto` would claim a preset that is the
/// absence of one.
#[test]
fn an_auto_session_launch_stamps_no_preset() {
  let daemon = Daemon::start(None);
  let tools = temp("tools");
  let dump = tools.join("dump");
  fake_tool(&tools, "codex", &dump);
  let out = daemon
    .cmd()
    .env("PATH", path_with(&tools))
    .args(["session", "codex", "chat@auto"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "session failed: {}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  let status = daemon
    .cmd()
    .args(["status", "--json"])
    .output()
    .expect("status");
  let models = stdout_json(&status)["models"]
    .as_array()
    .expect("models array")
    .clone();
  let launch = models
    .iter()
    .find(|m| m["name"].as_str() == Some("auto"))
    .or_else(|| models.first())
    .expect("the session's launch");
  assert!(
    launch["preset"].is_null(),
    "pure fit stamps no preset: {launch}"
  );
}

#[test]
fn a_tool_that_is_not_installed_is_a_binary_not_found() {
  let daemon = Daemon::start(None);
  let empty = temp("empty-bin");
  std::fs::create_dir_all(&empty).expect("empty dir");
  let out = daemon
    .cmd()
    .env("PATH", &empty)
    .args(["session", "aider", "chat"])
    .output()
    .expect("run session");
  assert_eq!(out.status.code(), Some(70), "exit 70 expected");
  assert!(
    String::from_utf8_lossy(&out.stderr)
      .to_string()
      .contains("not found on PATH"),
    "message names the missing program"
  );
  // The tool is looked up before a model is started, so a missing tool costs no
  // load and no launch.
  assert_eq!(
    daemon.running(),
    0,
    "nothing was started for a missing tool"
  );
  // A dry run still prints what it would have run.
  let dry = daemon
    .cmd()
    .env("PATH", &empty)
    .args(["session", "aider", "chat", "--dry-run", "--json"])
    .output()
    .expect("run dry run");
  assert!(
    dry.status.success(),
    "a dry run does not need the binary: {}{}",
    String::from_utf8_lossy(&dry.stdout),
    String::from_utf8_lossy(&dry.stderr)
  );
  assert_eq!(stdout_json(&dry)["program"], "aider");
}
