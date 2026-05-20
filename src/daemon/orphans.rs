//! Daemon-restart bookkeeping for `llama-server` children.
//!
//! Two responsibilities:
//! 1. **Re-adopt** entries from `state.json::running` whose PID is
//!    still alive, whose recorded port answers, and whose
//!    `/v1/models` reports the same model file the supervisor
//!    launched (R42). Three-factor confirmation guards against
//!    PID-reuse: the kernel may have handed our recorded PID to an
//!    unrelated process by the time we restart.
//! 2. **Surface external** `llama-server` processes (started
//!    outside the daemon — say, by the user typing
//!    `llama-server -m ...` directly) so the TUI's `external` row
//!    isn't blind to them. External entries are read-only: only
//!    `stop` is permitted; no edit/restart path exists.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

use crate::daemon::state_store::RunningSnapshot;

/// What `sweep` found on this daemon restart.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SweepReport {
  /// Snapshots whose three-factor probe (PID alive, port listening,
  /// `/v1/models` model path matches) all passed. The supervisor
  /// rebuilds a `ManagedModel` for each.
  pub adopted: Vec<RunningSnapshot>,
  /// Snapshots whose owner has died, whose port no longer answers,
  /// or whose `/v1/models` reports a different model. The
  /// supervisor drops these from `state.json::running` on next save.
  pub stale: Vec<RunningSnapshot>,
  /// `llama-server` processes the daemon doesn't own. Surfaced as
  /// `external` in the IPC `status` response.
  pub external: Vec<ExternalProcess>,
}

/// One unmanaged `llama-server` process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalProcess {
  pub pid: u32,
  pub cmdline: String,
  /// Detected `-m <path>` from the process's command line, if any.
  /// Helps the user identify which model is running outside
  /// llamastash's control.
  pub model_path: Option<PathBuf>,
  /// Boot-time snapshot of the process's start_time (seconds since
  /// kernel boot or epoch, depending on platform). Used by
  /// `stop_external` to defend against PID-recycling: if the
  /// process's current start_time has changed between the sweep
  /// snapshot and the stop request, the original process has exited
  /// and the kernel has handed the pid to someone else — refuse to
  /// signal.
  #[serde(default)]
  pub start_time_secs: u64,
}

/// Inputs to a sweep — the daemon hands them in.
#[derive(Debug, Clone)]
pub struct SweepInputs<'a> {
  pub recorded_running: &'a [RunningSnapshot],
  /// Substring matched against process command lines to detect
  /// `llama-server` invocations the daemon doesn't own. Defaults
  /// to "llama-server" in production; tests inject a unique
  /// substring so they don't trip on the real binary.
  pub external_marker: &'a str,
  /// Per-probe network timeout. Each adoption candidate gets one
  /// `/v1/models` call capped at this budget. Production defaults
  /// to 1s; tests can shorten.
  pub probe_timeout: Duration,
}

impl<'a> SweepInputs<'a> {
  pub fn new(recorded: &'a [RunningSnapshot]) -> Self {
    Self {
      recorded_running: recorded,
      external_marker: "llama-server",
      probe_timeout: Duration::from_secs(1),
    }
  }
}

/// Run a sweep. Pure-ish modulo a `sysinfo` scan of process tables
/// and one short HTTP probe per adoption candidate.
pub async fn sweep(inputs: SweepInputs<'_>) -> SweepReport {
  let mut sys = System::new_with_specifics(
    RefreshKind::nothing()
      .with_processes(ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always)),
  );
  sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

  let mut adopted: Vec<RunningSnapshot> = Vec::new();
  let mut stale: Vec<RunningSnapshot> = Vec::new();
  for snap in inputs.recorded_running.iter().cloned() {
    if !pid_alive(&sys, snap.pid) {
      stale.push(snap);
      continue;
    }
    // Three-factor confirmation: PID alive (above), port listening,
    // and `/v1/models` returns the recorded model path. Only then is
    // it safe to claim ownership again — otherwise we may be looking
    // at a recycled PID or an unrelated `llama-server` invocation.
    if !models_endpoint_matches(snap.port, &snap.id.path, inputs.probe_timeout).await {
      stale.push(snap);
      continue;
    }
    adopted.push(snap);
  }
  let adopted_pids: BTreeSet<u32> = adopted.iter().map(|s| s.pid as u32).collect();

  let external: Vec<ExternalProcess> = sys
    .processes()
    .iter()
    .filter_map(|(pid, proc)| {
      let pid_u32 = pid.as_u32();
      if adopted_pids.contains(&pid_u32) {
        return None;
      }
      let cmd: Vec<String> = proc
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy().into())
        .collect();
      if !cmd.iter().any(|c| c.contains(inputs.external_marker)) {
        return None;
      }
      let model_path = extract_model_path(&cmd);
      let start_time_secs = proc.start_time();
      Some(ExternalProcess {
        pid: pid_u32,
        cmdline: cmd.join(" "),
        model_path,
        start_time_secs,
      })
    })
    .collect();

  SweepReport {
    adopted,
    stale,
    external,
  }
}

fn pid_alive(sys: &System, pid: i32) -> bool {
  if pid <= 0 {
    return false;
  }
  sys.process(Pid::from_u32(pid as u32)).is_some()
}

/// Probe `/v1/models` on the recorded port and check that the
/// reported model id matches the supervisor's recorded path. Any
/// network error, non-200 response, malformed body, or mismatched
/// id evaluates to `false` — the sweep treats those as stale.
async fn models_endpoint_matches(port: u16, expected: &Path, timeout: Duration) -> bool {
  match fetch_models_body(port, timeout).await {
    Ok((200, body)) => body_mentions_path(&body, expected),
    _ => false,
  }
}

/// GET `/v1/models` via `reqwest` — the same client the right-pane
/// chat tab uses, so the orphan probe doesn't carry its own HTTP/1.1
/// framing. Capped at 32 KiB so a misbehaving peer can't balloon our
/// memory (audit §2.2). Returns `(status, body)` or an io error.
async fn fetch_models_body(port: u16, timeout: Duration) -> std::io::Result<(u16, Vec<u8>)> {
  let client = reqwest::Client::builder()
    .timeout(timeout)
    .build()
    .map_err(|e| std::io::Error::other(e.to_string()))?;
  let url = format!("http://127.0.0.1:{port}/v1/models");
  let resp = client
    .get(&url)
    .send()
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?;
  let status = resp.status().as_u16();
  // Cap the response body the matcher inspects. `/v1/models` is
  // small in practice; the cap defends against an unrelated peer
  // streaming an unbounded body on the recorded port.
  const MAX_BODY: usize = 32 * 1024;
  let mut body = resp
    .bytes()
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
    .to_vec();
  body.truncate(MAX_BODY);
  Ok((status, body))
}

/// Strict match: parse the `/v1/models` body as JSON and accept
/// adoption only when the documented `data[].id` field equals the
/// expected path exactly. This is tighter than the previous
/// substring-anywhere match, which would falsely adopt any local
/// process whose response body happened to contain the basename
/// (think `python -m http.server` serving a directory whose
/// listing mentions `llama.gguf`). The strict match is the right
/// boundary because llama-server emits the literal `-m <path>` it
/// received and we recorded the same canonical path on launch.
fn body_mentions_path(body: &[u8], expected: &Path) -> bool {
  let Ok(text) = std::str::from_utf8(body) else {
    return false;
  };
  // Fast reject: if the canonical path text isn't anywhere in the
  // body, no JSON shape can match.
  let expected_str = expected.to_string_lossy();
  if expected_str.is_empty() || !text.contains(expected_str.as_ref()) {
    return false;
  }
  // Parse the body strictly. Only accept the documented OpenAI
  // shape `{ "data": [ { "id": "<path>" }, ... ] }`. Any extra
  // fields are allowed (forward-compatible); a substring-only hit
  // outside `data[].id` is rejected as accidental.
  let parsed: serde_json::Value = match serde_json::from_str(text) {
    Ok(v) => v,
    Err(_) => return false,
  };
  let Some(arr) = parsed.get("data").and_then(|v| v.as_array()) else {
    return false;
  };
  arr.iter().any(|row| {
    row
      .get("id")
      .and_then(|v| v.as_str())
      .map(|id| id == expected_str.as_ref())
      .unwrap_or(false)
  })
}

/// Lift `-m <path>` out of a llama-server cmdline. Returns the
/// path the user passed (relative or absolute) without
/// canonicalising — the orphan caller does that step itself.
pub fn extract_model_path(cmd: &[String]) -> Option<PathBuf> {
  let mut iter = cmd.iter();
  while let Some(arg) = iter.next() {
    if arg == "-m" || arg == "--model" {
      if let Some(value) = iter.next() {
        return Some(PathBuf::from(value));
      }
    } else if let Some(rest) = arg.strip_prefix("--model=") {
      return Some(PathBuf::from(rest));
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::path::PathBuf;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;

  use crate::gguf::identity::ModelId;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;

  fn fake_snapshot(pid: i32, port: u16, path: &str, tag: u8) -> RunningSnapshot {
    RunningSnapshot {
      id: ModelId {
        path: PathBuf::from(path),
        header_blake3: [tag; 32],
      },
      pid,
      port,
      started_at: 1_700_000_000,
      params: LaunchParams::new(PathBuf::from(path), LaunchMode::Chat),
    }
  }

  fn allocate_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
  }

  /// Spin up a tiny single-shot HTTP responder on `port` returning
  /// the supplied (status, body) pair to one connection. Used by
  /// the orphan probe tests so we don't need the full
  /// `fake_llama_server` binary just to validate the matcher.
  async fn spawn_one_shot(port: u16, status: u16, body: String) -> tokio::task::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
      .await
      .expect("bind probe responder");
    tokio::spawn(async move {
      if let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let reason = match status {
          200 => "OK",
          404 => "Not Found",
          _ => "Status",
        };
        let header = format!(
          "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        let _ = sock.write_all(header.as_bytes()).await;
        let _ = sock.write_all(body.as_bytes()).await;
        let _ = sock.shutdown().await;
      }
    })
  }

  #[test]
  fn extract_model_path_handles_dash_m_and_long_forms() {
    let dash_m: Vec<String> = vec![
      "llama-server".into(),
      "--port".into(),
      "41100".into(),
      "-m".into(),
      "/models/a.gguf".into(),
    ];
    assert_eq!(
      extract_model_path(&dash_m),
      Some(PathBuf::from("/models/a.gguf"))
    );

    let long_pair: Vec<String> = vec!["llama-server".into(), "--model".into(), "/m/b.gguf".into()];
    assert_eq!(
      extract_model_path(&long_pair),
      Some(PathBuf::from("/m/b.gguf"))
    );

    let inline_eq: Vec<String> = vec!["llama-server".into(), "--model=/m/c.gguf".into()];
    assert_eq!(
      extract_model_path(&inline_eq),
      Some(PathBuf::from("/m/c.gguf"))
    );

    let bare: Vec<String> = vec!["llama-server".into()];
    assert_eq!(extract_model_path(&bare), None);
  }

  #[test]
  fn body_mentions_path_requires_strict_id_match() {
    let body = br#"{"data":[{"id":"/m/a.gguf","object":"model"}]}"#;
    assert!(body_mentions_path(body, Path::new("/m/a.gguf")));
    // Same basename in a different directory must NOT adopt. This is
    // the PID-reuse / friendly-fire case the strict matcher rejects.
    let body_renamed = br#"{"data":[{"id":"/different/dir/a.gguf"}]}"#;
    assert!(!body_mentions_path(body_renamed, Path::new("/m/a.gguf")));
    let body_other = br#"{"data":[{"id":"/m/other.gguf"}]}"#;
    assert!(!body_mentions_path(body_other, Path::new("/m/a.gguf")));
    // Non-OpenAI shape that merely contains the path text must be
    // rejected — the legacy substring matcher would have accepted.
    let body_html = b"<html><body>I serve /m/a.gguf here, but not as a llama-server</body></html>";
    assert!(!body_mentions_path(body_html, Path::new("/m/a.gguf")));
    // Decoy field with the right value but not at `data[].id`.
    let body_decoy = br#"{"notes":"/m/a.gguf","data":[{"id":"/m/other.gguf"}]}"#;
    assert!(!body_mentions_path(body_decoy, Path::new("/m/a.gguf")));
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn dead_pid_lands_in_stale() {
    let dead = 2_147_483_646;
    let recorded = vec![fake_snapshot(dead, 41123, "/m/a.gguf", 1)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_marker: "llamastash-sweep-marker-that-matches-nothing-9f3a",
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    let stale_pids: Vec<i32> = report.stale.iter().map(|r| r.pid).collect();
    assert_eq!(stale_pids, vec![dead]);
    assert!(report.adopted.is_empty());
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn live_pid_with_port_silent_lands_in_stale() {
    // PID is alive (us), but no one is listening on the recorded
    // port. The three-factor probe must reject the adoption.
    let live = std::process::id() as i32;
    let port = allocate_port(); // released immediately, nothing listens
    let recorded = vec![fake_snapshot(live, port, "/m/a.gguf", 1)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_marker: "llamastash-sweep-marker-that-matches-nothing-9f3a",
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    assert!(report.adopted.is_empty(), "no listener → must be stale");
    assert_eq!(report.stale.len(), 1);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn live_pid_with_matching_port_and_model_path_adopts() {
    let live = std::process::id() as i32;
    let port = allocate_port();
    let body = serde_json::json!({
      "object": "list",
      "data": [{"id": "/m/match.gguf", "object": "model"}],
    })
    .to_string();
    let _resp = spawn_one_shot(port, 200, body).await;

    let recorded = vec![fake_snapshot(live, port, "/m/match.gguf", 1)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_marker: "llamastash-sweep-marker-that-matches-nothing-9f3a",
      probe_timeout: Duration::from_secs(1),
    })
    .await;
    assert_eq!(report.adopted.len(), 1, "matching probe must adopt");
    assert!(report.stale.is_empty());
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn live_pid_with_mismatched_model_path_is_stale_pid_reuse_guard() {
    // Same PID + listening port, but the responder advertises a
    // different model — the canonical PID-reuse case. Adopt would
    // bind a stale `state.json::running` entry to an unrelated
    // process. Three-factor confirmation rejects it.
    let live = std::process::id() as i32;
    let port = allocate_port();
    let body = serde_json::json!({
      "object": "list",
      "data": [{"id": "/m/different.gguf", "object": "model"}],
    })
    .to_string();
    let _resp = spawn_one_shot(port, 200, body).await;

    let recorded = vec![fake_snapshot(live, port, "/m/expected.gguf", 1)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_marker: "llamastash-sweep-marker-that-matches-nothing-9f3a",
      probe_timeout: Duration::from_secs(1),
    })
    .await;
    assert!(
      report.adopted.is_empty(),
      "mismatched model path must reject adoption"
    );
    assert_eq!(report.stale.len(), 1);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn sweep_finds_unmanaged_processes_via_cmdline_marker() {
    // Spawn a long-lived dummy process whose program name matches
    // the marker, sweep, then kill it.
    use std::process::{Command, Stdio};
    use std::time::Duration as StdDuration;

    let mut child = Command::new("sleep")
      .arg("30")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("spawn sleep");

    std::thread::sleep(StdDuration::from_millis(100));
    let recorded: Vec<RunningSnapshot> = Vec::new();
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_marker: "sleep",
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    let found = report.external.iter().any(|p| p.pid == child.id());
    let _ = child.kill();
    let _ = child.wait();
    assert!(
      found,
      "sweep should have found the spawned `sleep` process by cmdline marker"
    );
  }
}
