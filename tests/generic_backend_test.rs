//! Generic backend: config-declared servers, driven through the production
//! daemon with `fake_llama_server` as the declared binary.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      rust_sources(&path, out);
    } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
      out.push(path);
    }
  }
}

/// "generic" is an ordinary English word in this tree, so the guard looks for
/// the ways code names the backend rather than the bare word: the id literal,
/// the module path, and its types and constants.
#[test]
fn backend_id_does_not_leak_outside_its_module() {
  const ALLOWED: &[&str] = &["src/backend/mod.rs", "src/config/mod.rs"];
  let root = repo_root();
  let mut files = Vec::new();
  rust_sources(&root.join("src"), &mut files);
  let needles = [
    concat!("\"gen", "eric\""),
    concat!("gen", "eric::"),
    concat!("Gen", "ericBackend"),
    concat!("Gen", "ericConfig"),
    concat!("GEN", "ERIC_"),
    concat!("gen", "eric://"),
  ];
  let mut leaks = Vec::new();
  for file in files {
    let rel = file
      .strip_prefix(&root)
      .unwrap_or(&file)
      .to_string_lossy()
      .replace('\\', "/");
    if rel.starts_with(concat!("src/backend/gen", "eric/")) || ALLOWED.contains(&rel.as_str()) {
      continue;
    }
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    if let Some(n) = needles.iter().find(|n| text.contains(**n)) {
      leaks.push(format!("{rel} ({n})"));
    }
  }
  assert!(
    leaks.is_empty(),
    "backend named outside its module: {leaks:?}"
  );
}

#[cfg(feature = "test-fixtures")]
mod lifecycle {
  use std::path::PathBuf;
  use std::time::{Duration, Instant};

  use llamastash::backend::BackendConfig;
  use llamastash::config::ProxyConfig;
  use llamastash::daemon::{run_foreground, DaemonOptions};
  use llamastash::ipc::Client;
  use serde_json::{json, Value};

  fn fake() -> String {
    env!("CARGO_BIN_EXE_fake_llama_server").to_string()
  }

  fn unique_temp(label: &str) -> PathBuf {
    llamastash::test_support::unique_temp_dir("ls-generic", label)
  }

  /// Every entry the tests use. Installed per daemon; entries merge by name,
  /// so parallel tests share one consistent table.
  fn backend_config() -> BackendConfig {
    let yaml = format!(
      r#"
servers:
  - name: gen-a
    binary: {bin}
    args: [--port, "{{port}}", -m, "{{name}}", --print-argv, --print-env, GEN_CTX, --sigterm-exit-delay-ms, "2000"]
    knobs:
      - {{flag: --context, id: gen-ctx, ctx: true, default: "4096"}}
      - {{flag: --speculative, default: mtp}}
      - --seed
    env:
      GEN_CTX: "{{gen-ctx}}"
    ready: /health
    memory_gib: 1.5
    stop_grace_secs: 10
  - name: gen-trap
    binary: {bin}
    args: [--port, "{{port}}", --trap-sigterm]
    ready: /health
    stop_grace_secs: 3
  - name: gen-gguf
    binary: {bin}
    model: "*-served.gguf"
    args: [--port, "{{port}}", -m, "{{name}}", --print-argv, --model-file, "{{model}}"]
    knobs:
      - --gguf-knob
    ready: /health
  - name: gen-chat
    binary: {bin}
    args: [--port, "{{port}}"]
    ready: /health
    modes: [chat]
  - name: gen-slow
    binary: {bin}
    args: [--port, "{{port}}", --health-delay-ms, "60000"]
    ready: /health
    ready_timeout_secs: 2
  - name: gen-rw
    binary: {bin}
    args: [--port, "{{port}}", -m, "{{name}}", --print-argv]
    ready: /health
    rewrite_model: true
    routes: [/rw/pinned]
  - name: gen-stt
    binary: {bin}
    args: [--port, "{{port}}"]
    ready: /health
    memory_gib: 1.0
    modes: [stt]
  - name: gen-tts-a
    binary: {bin}
    args: [--port, "{{port}}"]
    ready: /health
    memory_gib: 1.0
    modes: [tts]
  - name: gen-tts-b
    binary: {bin}
    args: [--port, "{{port}}"]
    ready: /health
    memory_gib: 1.0
    modes: [tts]
  - name: gen-routes
    binary: {bin}
    args: [--port, "{{port}}"]
    ready: /health
    memory_gib: 1.0
    routes: [/inference, /internal, /internal/deep]
"#,
      bin = fake()
    );
    BackendConfig {
      generic: yaml_serde::from_str(&yaml).unwrap(),
      ..BackendConfig::default()
    }
  }

  fn opts(state: PathBuf, proxy_port: Option<u16>) -> DaemonOptions {
    let base = DaemonOptions::rooted_at(state);
    DaemonOptions {
      binary: Some(PathBuf::from(fake())),
      port_range: llamastash::test_support::allocate_port_range(8),
      metrics_interval: Duration::from_secs(60),
      backend: backend_config(),
      proxy: ProxyConfig {
        enabled: proxy_port.is_some(),
        port: proxy_port,
        ..ProxyConfig::default()
      },
      ..base
    }
  }

  async fn boot(
    opts: DaemonOptions,
  ) -> (
    Client,
    tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
  ) {
    let dir = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
      if let Ok(c) = Client::connect(&dir).await {
        return (c, daemon);
      }
      assert!(Instant::now() < deadline, "daemon never came up");
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
  }

  async fn start(client: &mut Client, params: Value) -> String {
    let resp = client
      .call("start_model", Some(params))
      .await
      .expect("start_model");
    resp["launch_id"].as_str().expect("launch_id").to_string()
  }

  async fn row(client: &mut Client, launch_id: &str) -> Value {
    let status = client.call("status", None).await.expect("status");
    status["models"]
      .as_array()
      .unwrap()
      .iter()
      .find(|m| m["launch_id"] == launch_id)
      .cloned()
      .unwrap_or(Value::Null)
  }

  fn state_of(row: &Value) -> &str {
    row["state"]["state"].as_str().unwrap_or("")
  }

  async fn wait_state(client: &mut Client, launch_id: &str, want: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
      let r = row(client, launch_id).await;
      if state_of(&r) == want {
        return r;
      }
      assert!(
        Instant::now() < deadline,
        "{launch_id} never reached {want}: {r}"
      );
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  async fn log_lines(client: &mut Client, launch_id: &str, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let body = client
        .call(
          "logs_tail",
          Some(json!({"launch_id": launch_id, "lines": 200})),
        )
        .await
        .expect("logs_tail");
      let lines: Vec<String> = body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str().map(str::to_string))
        .filter(|l| l.contains(needle))
        .collect();
      if !lines.is_empty() || Instant::now() > deadline {
        return lines;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  /// The server catalog fills in the background after boot.
  async fn wait_server(client: &mut Client, server: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let status = client.call("status", None).await.unwrap();
      if status["servers"].to_string().contains(server) {
        return;
      }
      assert!(
        Instant::now() < deadline,
        "never listed as a server: {}",
        status["servers"]
      );
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  async fn stop_timed(client: &mut Client, launch_id: &str, grace: u64) -> Duration {
    let t = Instant::now();
    client
      .call_with_timeout(
        "stop_model",
        Some(json!({"launch_id": launch_id, "grace_secs": grace})),
        Duration::from_secs(30),
      )
      .await
      .expect("stop_model");
    t.elapsed()
  }

  async fn shutdown(
    mut client: Client,
    daemon: tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
  ) {
    let _ = client.call("shutdown", None).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), daemon).await;
  }

  /// Wait until the proxy can see an entry's catalog row.
  async fn wait_listed(client: &mut Client, entry: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !client
      .call("list_models", None)
      .await
      .unwrap()
      .to_string()
      .contains(&format!("generic://{entry}"))
    {
      assert!(Instant::now() < deadline, "{entry} never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  /// A whisper.cpp-shaped upload: a file part and a `model` field.
  /// A `GET` written straight onto the socket, so nothing resolves dot segments on
  /// the way: `reqwest` normalizes the path before it leaves, which is exactly the
  /// step a client trying to walk sideways skips. Returns the status and the body.
  async fn raw_get(port: u16, target: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
      .await
      .expect("connect");
    let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
      .await
      .expect("read")
      .expect("body");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
      .split_whitespace()
      .nth(1)
      .and_then(|code| code.parse::<u16>().ok())
      .unwrap_or_else(|| panic!("no status line in {text:?}"));
    (status, text)
  }

  fn transcription_body(model: &str) -> (Vec<u8>, String) {
    let b = "----lsTestBoundary";
    let body = format!(
      "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
       Content-Type: audio/wav\r\n\r\nRIFFdata\r\n\
       --{b}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n\
       --{b}--\r\n"
    );
    (
      body.into_bytes(),
      format!("multipart/form-data; boundary={b}"),
    )
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_entry_is_listed_and_composes_argv_env_and_name() {
    let (mut client, daemon) = boot(opts(unique_temp("compose"), None)).await;

    // Catalog row: file-less, named after the entry, sized by `memory_gib`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let listed = loop {
      let models = client.call("list_models", None).await.unwrap()["models"].clone();
      if let Some(m) = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["path"] == "generic://gen-a")
      {
        break m.clone();
      }
      assert!(Instant::now() < deadline, "entry never listed: {models}");
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(listed["name"], "gen-a", "{listed}");
    assert_eq!(listed["source"], "config", "{listed}");
    assert_eq!(listed["backend"], "generic", "{listed}");

    // First, before any `last_params` exist: the recorder stores a launch's
    // params once it sees Ready, so a plain launch after the named one below
    // would race it and could inherit ctx 32768.
    let id2 = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
    let r2 = wait_state(&mut client, &id2, "ready").await;
    assert_eq!(r2["params"]["ctx"], 4096, "entry default fills in: {r2}");

    // Named launch, `--ctx`, an entry knob, and raw extras.
    let id = start(
      &mut client,
      json!({
        "model_path": "generic://gen-a",
        "name": "coder",
        "ctx": 32768,
        "knobs": {"speculative": "none"},
        "extras": ["--engine-flag", "x"],
      }),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic");
    assert_eq!(r["params"]["ctx"], 32768, "{r}");
    assert_eq!(r["params"]["knobs"]["speculative"], "none", "{r}");
    assert_eq!(r["params"]["extras"], json!(["--engine-flag", "x"]), "{r}");
    assert_eq!(r["stop_grace_secs"], 10, "{r}");

    let argv = log_lines(&mut client, &id, "argv ").await;
    let argv = argv.first().expect("argv line");
    assert!(
      argv.contains("-m gen-a@coder"),
      "{{name}} is the published address: {argv}"
    );
    assert!(
      argv.contains("--speculative none --engine-flag x"),
      "knobs, then extras: {argv}"
    );
    assert!(
      !argv.contains("--context"),
      "a knob referenced in env is not also a flag: {argv}"
    );
    assert!(!argv.contains("--seed"), "unset, no default: {argv}");
    let env = log_lines(&mut client, &id, "env GEN_CTX=").await;
    assert!(
      env
        .first()
        .is_some_and(|l| l.ends_with("env GEN_CTX=32768")),
      "{env:?}"
    );

    assert_ne!(r["port"], r2["port"], "each launch gets its own port");

    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_entry_launches_on_a_host_without_llama_server() {
    let opts = DaemonOptions {
      binary: None,
      ..opts(unique_temp("no-llama"), None)
    };
    let (mut client, daemon) = boot(opts).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let id = loop {
      match client
        .call(
          "start_model",
          Some(json!({"model_path": "generic://gen-a"})),
        )
        .await
      {
        Ok(resp) => break resp["launch_id"].as_str().unwrap().to_string(),
        Err(e) => {
          assert!(Instant::now() < deadline, "start_model never accepted: {e}");
          tokio::time::sleep(Duration::from_millis(50)).await;
        }
      }
    };
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn the_entry_grace_is_a_floor_under_every_stop() {
    let (mut client, daemon) = boot(opts(unique_temp("grace"), None)).await;

    // The fixture's SIGTERM delay and trap are unix-only; Windows has no SIGTERM.
    #[cfg(unix)]
    {
      // Exits 2 s after SIGTERM; a 1 s caller grace must not SIGKILL it.
      let id = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
      wait_state(&mut client, &id, "ready").await;
      let took = stop_timed(&mut client, &id, 1).await;
      assert!(
        took >= Duration::from_millis(1900) && took < Duration::from_secs(6),
        "clean exit after its own drain, not a 1 s kill: {took:?}"
      );

      // Ignores SIGTERM; the 3 s floor applies, then SIGKILL.
      let id = start(&mut client, json!({"model_path": "generic://gen-trap"})).await;
      wait_state(&mut client, &id, "ready").await;
      let took = stop_timed(&mut client, &id, 1).await;
      assert!(
        took >= Duration::from_millis(2900) && took < Duration::from_secs(6),
        "killed after the entry floor: {took:?}"
      );
    }

    // `shutdown` reports the longest grace it will wait for.
    let id = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
    wait_state(&mut client, &id, "ready").await;
    let resp = client.call("shutdown", None).await.unwrap();
    assert_eq!(resp["stop_grace_secs"], 10, "{resp}");
    let _ = tokio::time::timeout(Duration::from_secs(30), daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn ready_timeout_secs_bounds_the_readiness_wait() {
    let (mut client, daemon) = boot(opts(unique_temp("timeout"), None)).await;
    let t = Instant::now();
    let id = start(&mut client, json!({"model_path": "generic://gen-slow"})).await;
    wait_state(&mut client, &id, "error").await;
    assert!(
      t.elapsed() < Duration::from_secs(15),
      "gave up after the entry's 2 s, not the default probe budget: {:?}",
      t.elapsed()
    );
    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_entry_knob_is_remembered_across_a_daemon_restart() {
    let state = unique_temp("restart");
    let (mut client, daemon) = boot(opts(state.clone(), None)).await;
    let id = start(
      &mut client,
      json!({"model_path": "generic://gen-a", "knobs": {"seed": "7"}}),
    )
    .await;
    wait_state(&mut client, &id, "ready").await;
    // last_params is written on Ready by a background recorder.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let lp = client.call("last_params_list", None).await.unwrap();
      if lp.to_string().contains("\"seed\":\"7\"") {
        break;
      }
      assert!(Instant::now() < deadline, "seed never persisted: {lp}");
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    shutdown(client, daemon).await;

    let (mut client, daemon) = boot(opts(state, None)).await;
    let id = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["params"]["knobs"]["seed"], "7", "{r}");
    let argv = log_lines(&mut client, &id, "argv ").await;
    assert!(argv[0].contains("--seed 7"), "{argv:?}");
    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn the_proxy_routes_by_entry_name_and_auto_starts_it() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("proxy"), Some(proxy_port))).await;

    // Wait for the catalog row before asking the proxy for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !client
      .call("list_models", None)
      .await
      .unwrap()
      .to_string()
      .contains("generic://gen-a")
    {
      assert!(Instant::now() < deadline, "entry never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let http = reqwest::Client::new();
    let resp = http
      .post(format!("http://127.0.0.1:{proxy_port}/v1/chat/completions"))
      .json(&json!({
        "model": "gen-a",
        "messages": [{"role": "user", "content": "hi"}],
      }))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    assert!(resp.status().is_success(), "status {}", resp.status());

    let status = client.call("status", None).await.unwrap();
    let running = status["models"].as_array().unwrap();
    assert!(
      running
        .iter()
        .any(|m| m["id"]["path"] == "generic://gen-a" && state_of(m) == "ready"),
      "auto-started the entry: {status}"
    );
    shutdown(client, daemon).await;
  }

  /// `rewrite_model` forwards the name the engine was launched with, so a named
  /// launch is told `gen-rw@coder` on the wire as well as in `{name}`.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn rewrite_model_forwards_the_named_launch_s_own_name() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("rw"), Some(proxy_port))).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !client
      .call("list_models", None)
      .await
      .unwrap()
      .to_string()
      .contains("generic://gen-rw")
    {
      assert!(Instant::now() < deadline, "entry never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let id = start(
      &mut client,
      json!({"model_path": "generic://gen-rw", "name": "coder"}),
    )
    .await;
    wait_state(&mut client, &id, "ready").await;
    let argv = log_lines(&mut client, &id, "argv ").await;
    assert!(argv[0].contains("-m gen-rw@coder "), "{argv:?}");

    let resp = reqwest::Client::new()
      .post(format!("http://127.0.0.1:{proxy_port}/v1/chat/completions"))
      .json(&json!({
        "model": "gen-rw@coder",
        "messages": [{"role": "user", "content": "hi"}],
      }))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    let status = resp.status();
    // The fixture echoes the `model` it was sent.
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "status {status}: {text}");
    assert!(
      text.contains(r#""model":"gen-rw@coder""#),
      "the engine is told the name it serves: {text}"
    );
    shutdown(client, daemon).await;
  }

  /// An entry that declares `modes: [chat]` is listed as chat on `/v1/models`,
  /// and an embeddings request for it is refused before any launch instead of
  /// failing inside the engine.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_chat_only_entry_refuses_embeddings_and_lists_its_mode() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("modes"), Some(proxy_port))).await;
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{proxy_port}");

    let deadline = Instant::now() + Duration::from_secs(10);
    let row = loop {
      let models: Value = http
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .expect("models")
        .json()
        .await
        .unwrap();
      if let Some(r) = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "gen-chat")
      {
        break r.clone();
      }
      assert!(Instant::now() < deadline, "entry never listed: {models}");
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(row["mode"], "chat", "{row}");

    let resp = http
      .post(format!("{base}/v1/embeddings"))
      .json(&json!({"model": "gen-chat", "input": "hi"}))
      .timeout(Duration::from_secs(30))
      .send()
      .await
      .expect("proxy request");
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "unsupported_endpoint", "{body}");

    let status = client.call("status", None).await.unwrap();
    assert!(
      !status["models"].to_string().contains("generic://gen-chat"),
      "refused before launch: {status}"
    );
    shutdown(client, daemon).await;
  }

  /// The image and audio endpoints forward like any other `/v1` route: the body
  /// names the entry, and a multipart upload reaches the engine untouched.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn the_media_routes_forward_and_auto_start_the_named_entry() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("media"), Some(proxy_port))).await;
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{proxy_port}");
    wait_listed(&mut client, "gen-stt").await;
    wait_listed(&mut client, "gen-tts-a").await;

    let (body, content_type) = transcription_body("gen-stt");
    let resp = http
      .post(format!("{base}/v1/audio/transcriptions"))
      .header(reqwest::header::CONTENT_TYPE, content_type)
      .body(body)
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    let status = resp.status();
    let served_by = resp.headers().get("x-llamastash-served-by").cloned();
    let echoed: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "status {status}: {echoed}");
    assert_eq!(echoed["model"], "gen-stt", "{echoed}");
    assert!(
      echoed["received_body"]
        .as_str()
        .is_some_and(|b| b.contains("RIFFdata") && b.contains("name=\"model\"")),
      "the upload and its fields reach the engine: {echoed}"
    );
    assert_eq!(served_by, None, "routing by name is not a fallback");

    let resp = http
      .post(format!("{base}/v1/audio/speech"))
      .json(&json!({"model": "gen-tts-a", "input": "hi", "voice": "alloy"}))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    let status = resp.status();
    let echoed: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "status {status}: {echoed}");
    assert_eq!(echoed["model"], "gen-tts-a", "{echoed}");

    let status = client.call("status", None).await.unwrap();
    let running = status["models"].as_array().unwrap();
    assert!(
      running
        .iter()
        .any(|m| m["id"]["path"] == "generic://gen-stt" && state_of(m) == "ready"),
      "the media route auto-started the entry it named: {status}"
    );
    shutdown(client, daemon).await;
  }

  /// A media launch's status row names the mode its entry answers beside the
  /// launch's own mode, and a plain entry declares nothing. The launch mode stays
  /// `chat` because a media mode arrives with the request's endpoint and changes
  /// no argv; `mode_hint` is the half the operator is looking for.
  #[tokio::test]
  async fn media_launch_row_carries_its_declared_mode() {
    let (mut client, daemon) = boot(opts(unique_temp("declared-mode"), None)).await;
    wait_listed(&mut client, "gen-stt").await;
    wait_listed(&mut client, "gen-chat").await;
    // The same flag `llamastash start` waits on. Asked with a wait rather than
    // `wait_ms: 0`, because a boot scan on a slow runner can still be walking.
    let ready = client
      .call("catalog_ready", Some(json!({ "wait_ms": 3_000 })))
      .await
      .expect("catalog_ready");
    assert_eq!(
      ready["first_scan"],
      json!(true),
      "the boot scan flag never flipped"
    );

    let stt = start(&mut client, json!({ "model_path": "generic://gen-stt" })).await;
    let stt_row = wait_state(&mut client, &stt, "ready").await;
    assert_eq!(stt_row["mode"], json!("chat"), "{stt_row}");
    assert_eq!(stt_row["mode_hint"], json!("stt"), "{stt_row}");

    let chat = start(&mut client, json!({ "model_path": "generic://gen-chat" })).await;
    let chat_row = wait_state(&mut client, &chat, "ready").await;
    assert_eq!(chat_row["mode"], json!("chat"), "{chat_row}");
    assert!(
      chat_row.get("mode_hint").is_none(),
      "an entry with no media mode declares nothing: {chat_row}"
    );
    shutdown(client, daemon).await;
  }

  /// `/v1/systemone` takes no mode, so a chat entry answers it by name.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn systemone_forwards_to_the_named_entry() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("systemone"), Some(proxy_port))).await;
    wait_listed(&mut client, "gen-chat").await;

    let resp = reqwest::Client::new()
      .post(format!("http://127.0.0.1:{proxy_port}/v1/systemone"))
      .json(&json!({
        "model": "gen-chat",
        "state": "rm -rf /",
        "questions": {"risky": {"type": "noul", "instructions": "Is it risky?"}},
      }))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    let status = resp.status();
    let echoed: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "status {status}: {echoed}");
    assert_eq!(echoed["model"], "gen-chat", "{echoed}");
    assert!(
      echoed["received_body"]
        .as_str()
        .is_some_and(|b| b.contains("\"questions\"")),
      "the body reaches the engine: {echoed}"
    );
    shutdown(client, daemon).await;
  }

  /// A client that hardcodes `whisper-1` still reaches the one entry that
  /// declares `stt`: the body reaches it untouched, so an engine that maps
  /// `whisper-1` itself still can, the response says which entry answered, and
  /// the log row keeps it. Two entries for one mode leaves nothing
  /// to pick, so that answer stays `model_not_found` and says how to fix it.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_unknown_media_model_falls_back_to_the_entry_that_declares_the_mode() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("modefb"), Some(proxy_port))).await;
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{proxy_port}");
    let deadline = Instant::now() + Duration::from_secs(30);
    wait_listed(&mut client, "gen-stt").await;
    wait_listed(&mut client, "gen-tts-a").await;

    // Nothing has started, and the listing already publishes the mode each entry
    // declared, so a client can pick the endpoint from `/v1/models` first.
    let listed: Value = http
      .get(format!("{base}/v1/models"))
      .send()
      .await
      .expect("model list")
      .json()
      .await
      .expect("model list body");
    let mode_of = |name: &str| {
      listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == name)
        .and_then(|row| row["mode"].as_str())
        .unwrap_or_else(|| panic!("{name} is not listed: {listed}"))
        .to_string()
    };
    assert_eq!(mode_of("gen-stt"), "stt");
    assert_eq!(mode_of("gen-tts-a"), "tts");

    let (body, content_type) = transcription_body("whisper-1");
    let resp = http
      .post(format!("{base}/v1/audio/transcriptions"))
      .header(reqwest::header::CONTENT_TYPE, content_type)
      .body(body)
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    let status = resp.status();
    let served_by = resp
      .headers()
      .get("x-llamastash-served-by")
      .and_then(|v| v.to_str().ok())
      .map(str::to_string);
    let reason = resp
      .headers()
      .get("x-llamastash-fallback-reason")
      .and_then(|v| v.to_str().ok())
      .map(str::to_string);
    let echoed: Value = resp.json().await.unwrap();
    assert!(status.is_success(), "status {status}: {echoed}");
    assert_eq!(served_by.as_deref(), Some("gen-stt"), "{echoed}");
    assert_eq!(reason.as_deref(), Some("mode_match"));
    assert_eq!(
      echoed["model"], "whisper-1",
      "the body is not rewritten: {echoed}"
    );
    assert!(
      echoed["received_body"]
        .as_str()
        .is_some_and(|b| b.contains("RIFFdata")),
      "the upload arrives as sent: {echoed}"
    );

    // The entry is running now, so this one takes the Ready path instead of the
    // auto-start one, and it keeps the client's name too.
    let (body, content_type) = transcription_body("whisper-1");
    let echoed: Value = http
      .post(format!("{base}/v1/audio/transcriptions"))
      .header(reqwest::header::CONTENT_TYPE, content_type)
      .body(body)
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("second proxy request")
      .json()
      .await
      .unwrap();
    assert_eq!(
      echoed["model"], "whisper-1",
      "a Ready launch keeps the name too: {echoed}"
    );

    // A media route picks its row by mode alone, so a request that names no model
    // at all is routed rather than refused for a missing model.
    let resp = http
      .post(format!("{base}/v1/audio/transcriptions"))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("model-less media request");
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    assert_eq!(code, 200, "mode alone picks the row: {text}");

    let row = loop {
      let tail = client
        .call("requests_tail", Some(json!({"limit": 50})))
        .await
        .unwrap();
      if let Some(r) = tail["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["requested_model"] == "whisper-1")
      {
        break r.clone();
      }
      assert!(
        Instant::now() < deadline,
        "the request was never logged: {tail}"
      );
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(row["route"], "/v1/audio/transcriptions", "{row}");
    assert_eq!(row["fallback"], "mode_match", "{row}");
    assert_eq!(row["model"], "gen-stt", "{row}");

    // A row that does not declare the route's mode is refused, and a row that is
    // not even running is refused before the launch: naming it cannot start
    // something that has no endpoint for the request.
    for (name, path) in [("gen-routes", "/v1/audio/transcriptions")] {
      let resp = http
        .post(format!("{base}{path}"))
        .json(&json!({"model": name, "input": "hi"}))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("mode mismatch");
      let status = resp.status();
      let body: Value = resp.json().await.unwrap();
      assert_eq!(
        body["error"]["type"].as_str().unwrap_or("ok"),
        "unsupported_endpoint",
        "{name} on {path}: {status} {body}"
      );
    }
    // The refusal came before the launch: the chat entry nobody asked for never
    // started.
    let status = client.call("status", None).await.unwrap();
    assert!(
      !status["models"]
        .to_string()
        .contains("generic://gen-routes"),
      "a mode mismatch started a launch: {status}"
    );

    // A mode no entry declares is refused as such, not as a name to add.
    let resp = http
      .post(format!("{base}/v1/images/generations"))
      .json(&json!({"model": "sdxl-turbo", "prompt": "hi"}))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "model_not_found", "{body}");
    assert!(
      body["error"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("no entry declares")),
      "the refusal says nothing declares the mode: {body}"
    );

    // Two entries declare `tts` and neither is Ready: no single answer.
    let resp = http
      .post(format!("{base}/v1/audio/speech"))
      .json(&json!({"model": "tts-1", "input": "hi"}))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "model_not_found", "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
      message.contains("2 entries declare this endpoint's mode")
        && message.contains("proxy.aliases"),
      "the refusal says who declares the mode and how to make the name work: {body}"
    );
    let status = client.call("status", None).await.unwrap();
    assert!(
      !status["models"].to_string().contains("generic://gen-tts"),
      "an ambiguous mode fallback starts nothing: {status}"
    );

    // Name one of the two and the ambiguity goes away: the entry that is up is the
    // one the mode picks for a name it cannot place.
    let resp = http
      .post(format!("{base}/v1/audio/speech"))
      .json(&json!({"model": "gen-tts-a", "input": "hi"}))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("named tts launch");
    assert_eq!(resp.status().as_u16(), 200);
    let resp = http
      .post(format!("{base}/v1/audio/speech"))
      .json(&json!({"model": "tts-1", "input": "hi"}))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
      resp
        .headers()
        .get("x-llamastash-served-by")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default(),
      "gen-tts-a"
    );
    shutdown(client, daemon).await;
  }

  /// A declared prefix covers its own paths and nothing a client can spell with dot
  /// segments: `/internal/../metrics` would ask the engine for `/metrics`, which
  /// nobody declared, so the proxy refuses it instead of forwarding it.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_declared_prefix_does_not_open_the_rest_of_the_engine() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("dots"), Some(proxy_port))).await;
    wait_listed(&mut client, "gen-routes").await;

    for (target, want) in [
      ("/internal", 200u16),
      ("/internal/./x", 200),
      ("/internal/../internal", 200),
      ("/internal/../metrics", 404),
      ("/internal/../props", 404),
      ("/internal/x/../../slots", 404),
      ("/internal/%2e%2e/metrics", 404),
      ("/internal/%2E%2E/props", 404),
      ("/internal/x\\..\\..\\metrics", 404),
      ("/../metrics", 404),
      ("/slots", 404),
    ] {
      let (status, _) = raw_get(proxy_port, target).await;
      assert_eq!(status, want, "{target}");
    }

    // What the engine is asked for is the resolved path, not the one written. A
    // backslash splits here because the URL parser reads it as a separator.
    for (target, want) in [
      ("/internal/x/../deep/y", "/internal/deep/y"),
      ("/internal/x\\..\\metrics", "/internal/metrics"),
      ("/internal/x\\deep", "/internal/x/deep"),
    ] {
      let (status, text) = raw_get(proxy_port, target).await;
      assert_eq!(status, 200, "{target}");
      let want = format!("\"target\":\"{want}\"");
      assert!(text.contains(&want), "the engine saw {want}: {text}");
    }

    // An escape inside a segment stays an escape: only the `%2e` spelling is read, to
    // spot a dot segment, so nothing else about the path is decoded on the way.
    let (status, text) = raw_get(proxy_port, "/internal/%2e%2e%2fmetrics").await;
    assert_eq!(status, 200);
    assert!(
      text
        .to_ascii_uppercase()
        .contains("\"TARGET\":\"/INTERNAL/%2E%2E%2FMETRICS\""),
      "the escape reaches the engine as written: {text}"
    );
    shutdown(client, daemon).await;
  }

  /// A launch that pins its request-model name pins it on the OpenAI routes, not on
  /// a path its entry declared: the engine owns its own API and the body arrives as
  /// the client wrote it.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_declared_path_keeps_the_body_of_an_entry_that_pins_its_name() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("pin"), Some(proxy_port))).await;
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{proxy_port}");
    wait_listed(&mut client, "gen-rw").await;

    let resp = http
      .post(format!("{base}/rw/pinned"))
      .json(&json!({"model": "keep-me", "other": 1}))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("declared POST");
    assert_eq!(resp.status().as_u16(), 200);
    let echoed: Value = resp.json().await.unwrap();
    assert_eq!(echoed["target"], "/rw/pinned", "{echoed}");
    assert_eq!(
      echoed["model"], "keep-me",
      "the pin is not applied: {echoed}"
    );

    // Again with the launch up: a Ready forward pins nothing either.
    let resp = http
      .post(format!("{base}/rw/pinned"))
      .json(&json!({"model": "keep-me-again", "other": 2}))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("declared POST to a running launch");
    assert_eq!(resp.status().as_u16(), 200);
    let echoed: Value = resp.json().await.unwrap();
    assert_eq!(
      echoed["model"], "keep-me-again",
      "the pin is not applied to a running launch: {echoed}"
    );
    shutdown(client, daemon).await;
  }

  /// An entry with `model` is a server option on the catalog rows it matches,
  /// not a row of its own; picking it runs the GGUF through the entry.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_model_entry_runs_a_matching_catalog_gguf() {
    let models = unique_temp("gguf-models");
    for f in ["a-served.gguf", "b-other.gguf"] {
      std::fs::write(
        models.join(f),
        llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
      )
      .unwrap();
    }
    let mut o = opts(unique_temp("gguf"), None);
    o.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
      path: models.clone(),
      source: llamastash::discovery::ModelSource::UserPath,
    }];
    let (mut client, daemon) = boot(o).await;

    let served = models.join("a-served.gguf").display().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    let rows = loop {
      let rows = client.call("list_models", None).await.unwrap()["models"].clone();
      if rows.to_string().contains("a-served") {
        break rows;
      }
      assert!(Instant::now() < deadline, "models never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let row = |needle: &str| {
      rows
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["path"].as_str().unwrap_or("").contains(needle))
        .cloned()
        .unwrap()
    };
    assert_eq!(
      row("a-served")["supported_backends"],
      json!(["llamacpp", "generic"]),
      "offered after the default"
    );
    assert_eq!(row("b-other")["supported_backends"], json!(["llamacpp"]));
    assert!(
      !rows.to_string().contains("generic://gen-gguf"),
      "no row of its own"
    );
    wait_server(&mut client, "generic-gen-gguf").await;

    let id = start(
      &mut client,
      // The TUI sends the row's default backend beside the server pick; the
      // server decides.
      json!({
        "model_path": served,
        "backend": "llamacpp",
        "server": "generic-gen-gguf",
        "knobs": {"gguf-knob": "v"},
      }),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    let argv = log_lines(&mut client, &id, "argv ").await;
    let argv = argv.first().expect("argv line");
    assert!(
      argv.contains("-m a-served "),
      "{{name}} is the model id: {argv}"
    );
    assert!(argv.contains(&format!("--model-file {served}")), "{argv}");
    assert!(argv.contains("--gguf-knob v"), "{argv}");
    shutdown(client, daemon).await;
  }

  /// `{name}` is the id `/v1/models` publishes: two same-stem GGUFs publish
  /// qualified ids, and the proxy forwards `body.model` unchanged, so the bare
  /// stem would 404 on an engine that checks it.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn name_placeholder_is_the_published_id_when_stems_collide() {
    let roots = [unique_temp("dup-a"), unique_temp("dup-b")];
    for root in &roots {
      std::fs::write(
        root.join("dup-served.gguf"),
        llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
      )
      .unwrap();
    }
    let mut o = opts(unique_temp("dup"), None);
    o.discovery.scan_roots = roots
      .iter()
      .map(|r| llamastash::discovery::scanner::ScanRoot {
        path: r.clone(),
        source: llamastash::discovery::ModelSource::UserPath,
      })
      .collect();
    let (mut client, daemon) = boot(o).await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let rows = client.call("list_models", None).await.unwrap()["models"].clone();
      if rows.to_string().matches("dup-served").count() >= 2 {
        break;
      }
      assert!(Instant::now() < deadline, "models never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }

    wait_server(&mut client, "generic-gen-gguf").await;

    // Sent through a symlink, as macOS's `/tmp` is: the catalog keys the
    // canonical path, and the lookup must still find the published id.
    #[cfg(unix)]
    let sent_root = {
      let link = unique_temp("dup-link").join("root");
      std::os::unix::fs::symlink(&roots[0], &link).unwrap();
      link
    };
    #[cfg(not(unix))]
    let sent_root = roots[0].clone();
    let served = sent_root.join("dup-served.gguf").display().to_string();
    let id = start(
      &mut client,
      json!({"model_path": served, "server": "generic-gen-gguf"}),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    let argv = log_lines(&mut client, &id, "argv ").await;
    let argv = argv.first().expect("argv line");
    assert!(
      !argv.contains("-m dup-served "),
      "the ambiguous stem must not be sent: {argv}"
    );
    assert!(argv.contains("/dup-served "), "qualified id: {argv}");
    shutdown(client, daemon).await;
  }

  /// A server inherited from the last launch must not override an explicit
  /// backend: `start --backend llamacpp` after a generic launch ran generic.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_explicit_backend_beats_an_inherited_server() {
    let models = unique_temp("explicit-models");
    std::fs::write(
      models.join("c-served.gguf"),
      llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
    )
    .unwrap();
    let mut o = opts(unique_temp("explicit"), None);
    o.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
      path: models.clone(),
      source: llamastash::discovery::ModelSource::UserPath,
    }];
    let (mut client, daemon) = boot(o).await;
    wait_server(&mut client, "generic-gen-gguf").await;

    let served = models.join("c-served.gguf").display().to_string();
    let id = start(
      &mut client,
      json!({"model_path": served, "server": "generic-gen-gguf"}),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let lp = client.call("last_params_list", None).await.unwrap();
      if lp.to_string().contains("generic-gen-gguf") {
        break;
      }
      assert!(Instant::now() < deadline, "server never persisted: {lp}");
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop_timed(&mut client, &id, 5).await;

    let id = start(
      &mut client,
      json!({"model_path": served, "backend": "llamacpp"}),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "llamacpp", "{r}");
    assert!(r["params"]["server"].is_null(), "{r}");
    shutdown(client, daemon).await;
  }

  /// A remembered server that is gone from config must not leave its backend
  /// behind. The launch dropped the missing server but kept the remembered
  /// backend, so a plain `start` ran that backend with no server to pick,
  /// instead of the model's default.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_removed_remembered_server_falls_back_to_the_default_backend() {
    let models = unique_temp("gone-models");
    std::fs::write(
      models.join("d-served.gguf"),
      llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
    )
    .unwrap();
    let state = unique_temp("gone");
    let opts_at = |state: &PathBuf| {
      let mut o = opts(state.clone(), None);
      o.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
        path: models.clone(),
        source: llamastash::discovery::ModelSource::UserPath,
      }];
      o
    };
    let served = models.join("d-served.gguf").display().to_string();

    let (mut client, daemon) = boot(opts_at(&state)).await;
    wait_server(&mut client, "generic-gen-gguf").await;
    let id = start(
      &mut client,
      json!({"model_path": served, "server": "generic-gen-gguf"}),
    )
    .await;
    wait_state(&mut client, &id, "ready").await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let lp = client.call("last_params_list", None).await.unwrap();
      if lp.to_string().contains("generic-gen-gguf") {
        break;
      }
      assert!(Instant::now() < deadline, "server never persisted: {lp}");
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop_timed(&mut client, &id, 5).await;
    shutdown(client, daemon).await;

    // The entry is removed from config between runs. Entries are installed
    // process-wide, so rename the remembered id instead of dropping the entry.
    let file = state.join("state.json");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
      &file,
      text.replace("generic-gen-gguf", "generic-gen-removed"),
    )
    .unwrap();

    let (mut client, daemon) = boot(opts_at(&state)).await;
    wait_server(&mut client, "generic-gen-gguf").await;
    let id = start(&mut client, json!({"model_path": served})).await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "llamacpp", "{r}");
    assert!(r["params"]["server"].is_null(), "{r}");
    shutdown(client, daemon).await;
  }

  /// A declared prefix is the engine's own API: the row that publishes it answers,
  /// the proxy's own surfaces stay the proxy's, the log files the row under the
  /// declared prefix, and a path nobody declares is a 404.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_declared_path_reaches_the_engine_that_publishes_it() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("routes"), Some(proxy_port))).await;
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{proxy_port}");
    let deadline = Instant::now() + Duration::from_secs(40);
    wait_listed(&mut client, "gen-routes").await;

    // An upload naming a model no row answers still reaches the entry that
    // publishes the path, and keeps the name the client sent: an engine that owns
    // the path owns the name inside it too.
    let (body, content_type) = transcription_body("whisper-client");
    let resp = http
      .post(format!("{base}/inference"))
      .header(reqwest::header::CONTENT_TYPE, content_type)
      .body(body)
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("declared upload");
    assert_eq!(resp.status().as_u16(), 200);
    let echoed: Value = resp.json().await.unwrap();
    assert_eq!(echoed["target"], "/inference", "{echoed}");
    assert_eq!(echoed["model"], "whisper-client", "name kept: {echoed}");
    assert!(
      echoed["received_body"]
        .as_str()
        .is_some_and(|b| b.contains("RIFFdata")),
      "the file part is untouched: {echoed}"
    );

    // A prefix covers what hangs below it, keeps the query, and never matches a
    // longer name. A trailing slash survives, because engines route `/x/` and `/x`
    // apart; matching is still on the slash-less form.
    for (path, want) in [
      ("/internal", 200u16),
      ("/internal?x=1", 200),
      ("/internal/extra", 200),
      ("/internal/", 200),
      ("/internal/deep/x", 200),
      ("/internalsuffix", 404),
      ("/nonsense", 404),
    ] {
      let resp = http
        .get(format!("{base}{path}"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("declared GET");
      assert_eq!(resp.status().as_u16(), want, "{path}");
      if want == 200 {
        let echoed: Value = resp.json().await.unwrap();
        assert_eq!(echoed["target"], path, "{path}");
      }
    }
    // A body the proxy cannot read is the engine's business, not a refusal: the
    // declared path forwards it as it arrived.
    let resp = http
      .post(format!("{base}/internal"))
      .header(reqwest::header::CONTENT_TYPE, "text/plain")
      .body("not json at all")
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("declared POST");
    assert_eq!(resp.status().as_u16(), 200);
    let echoed: Value = resp.json().await.unwrap();
    assert_eq!(echoed["received_body"], "not json at all", "{echoed}");
    assert!(echoed["model"].is_null(), "nothing to read: {echoed}");

    // The client normalizes `..` before sending, so what has to be declared is the
    // path that arrives.
    let resp = http
      .get(format!("{base}/internal/../nonsense"))
      .send()
      .await
      .expect("dotted GET");
    assert_eq!(resp.status().as_u16(), 404);

    // The proxy keeps its own surfaces: `/api/tags` is the catalog's Ollama list
    // and `/ui` is the built-in page, whatever an entry declares.
    let resp = http
      .get(format!("{base}/api/tags"))
      .send()
      .await
      .expect("tags");
    assert!(
      resp.json::<Value>().await.unwrap().get("models").is_some(),
      "the curated route answered"
    );
    let resp = http.get(format!("{base}/ui")).send().await.expect("ui");
    assert_ne!(resp.status().as_u16(), 404, "the built-in page answered");

    let rows = loop {
      let tail = client
        .call("requests_tail", Some(json!({"limit": 50})))
        .await
        .unwrap();
      let rows = tail["requests"].as_array().unwrap().clone();
      if rows.iter().any(|r| r["route"] == "/internal/deep") {
        break rows;
      }
      assert!(
        Instant::now() < deadline,
        "the declared routes were never logged: {rows:?}"
      );
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let row_for = |route: &str| {
      rows
        .iter()
        .find(|r| r["route"] == route)
        .cloned()
        .unwrap_or_else(|| panic!("no row for {route}: {rows:?}"))
    };
    // Filed under the declared prefix, never under whatever the client sent.
    assert_eq!(row_for("/inference")["model"], "gen-routes");
    assert_eq!(row_for("/inference")["requested_model"], "whisper-client");
    assert_eq!(row_for("/internal")["route"], "/internal");
    assert_eq!(row_for("/internal/deep")["route"], "/internal/deep");
    shutdown(client, daemon).await;
  }
}
