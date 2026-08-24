//! Daemon-restart bookkeeping for `llama-server` children.
//!
//! Two responsibilities:
//! 1. **Re-adopt** entries from `state.json::running` whose PID is
//!    still alive, whose recorded port answers, and whose
//!    `/v1/models` reports the same model file the supervisor
//!    launched. Three-factor confirmation guards against
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
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

use crate::backend::Backend;
use crate::daemon::state_store::RunningSnapshot;

/// What `sweep` found on this daemon restart.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
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
  /// Listening port parsed from the process's argv (`--port N`).
  /// `None` when the cmdline doesn't carry one — `llama-server`
  /// defaults to 8080 but llamastash's supervisor always passes the
  /// flag explicitly, so this should be `Some` for any process we
  /// launched. The IPC layer feeds this into `collect_in_use_ports`
  /// for `launched_by_llamastash` rows so the allocator skips ports
  /// already held by orphan llamastash children.
  #[serde(default)]
  pub port: Option<u16>,
  /// True when the process's environment carries
  /// `LLAMASTASH_LAUNCHED=1`. Set by the supervisor's spawn path
  /// (`src/daemon/supervisor.rs`); a fresh `llama-server` started
  /// by hand or by another tool never has this marker. Drives the
  /// port-skip behaviour above and reads in `daemon status` as a
  /// hint that the orphan came from a sibling/previous llamastash.
  #[serde(default)]
  pub launched_by_llamastash: bool,
}

/// Whether a process basename is an instance of the backend `marker` names.
///
/// An exact basename match, plus `<marker>-<suffix>` for **compound** markers
/// only.
///
/// A bare `contains` was safe while every marker was a long compound basename
/// (`llama-server`, `ds4-server`), where a `-cuda` / `-vulkan` build suffix is
/// certainly the same program and `comm`'s 15-char cap can truncate it. It
/// stops being safe once a backend registers a short single-token marker: a
/// four-character token is a substring of unrelated tools on the host, and
/// claiming one makes it a legal `stop_external` target. Suffix tolerance is
/// therefore extended only to markers already specific enough to earn it.
fn basename_matches_marker(basename: &str, marker: &str) -> bool {
  if basename == marker {
    return true;
  }
  marker.contains('-') && basename.starts_with(&format!("{marker}-"))
}

/// Inputs to a sweep — the daemon hands them in.
#[derive(Debug, Clone)]
pub struct SweepInputs<'a> {
  pub recorded_running: &'a [RunningSnapshot],
  /// Process basenames matched to detect backend-child invocations the daemon
  /// doesn't own. In production this is [`crate::backend::external_process_markers`]
  /// (every backend's `process_marker`), so the sweep learns a new backend's
  /// server from its registration alone; tests inject a unique substring so they
  /// don't trip on the real binaries.
  pub external_markers: Vec<&'static str>,
  /// Per-probe network timeout. Each adoption candidate gets one
  /// `/v1/models` call capped at this budget. Production defaults
  /// to 1s; tests can shorten.
  pub probe_timeout: Duration,
}

impl<'a> SweepInputs<'a> {
  pub fn new(recorded: &'a [RunningSnapshot]) -> Self {
    Self {
      recorded_running: recorded,
      external_markers: crate::backend::external_process_markers(),
      probe_timeout: Duration::from_secs(1),
    }
  }
}

/// Run a sweep. Pure-ish modulo a `sysinfo` scan of process tables
/// and one short HTTP probe per adoption candidate.
pub async fn sweep(inputs: SweepInputs<'_>) -> SweepReport {
  // `with_environ` makes `proc.environ()` populate the process
  // environment block, which the marker check below reads to spot
  // orphan llamastash children carrying `LLAMASTASH_LAUNCHED=1`.
  //
  // Unix-only: on Linux/macOS the backend reads `/proc/<pid>/environ`
  // (and the equivalent on macOS) as one same-UID file per process,
  // measured in single-digit milliseconds. On Windows the backend
  // walks every process's PEB via `NtQueryInformationProcess` +
  // `PROCESS_VM_READ`, which is slow and access-restricted enough
  // that enabling it on the CI runner pushed the daemon's startup
  // sweep past the test harness's 3-second socket-bind deadline
  // and broke every `wait_for_socket` integration test. The
  // marker-driven port-skip is a proactive nice-to-have; the
  // bind-probe in `ports::try_bind_probe` (called from
  // `reserve_port`) is the correctness gate and runs on every
  // platform. Skipping environ on Windows means
  // `launched_by_llamastash` stays `false` there but launches
  // still avoid externally-held ports via the probe.
  let process_refresh = {
    #[cfg(unix)]
    {
      ProcessRefreshKind::nothing()
        .with_cmd(sysinfo::UpdateKind::Always)
        .with_environ(sysinfo::UpdateKind::Always)
    }
    #[cfg(not(unix))]
    {
      ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always)
    }
  };
  let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(process_refresh));
  sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

  let mut adopted: Vec<RunningSnapshot> = Vec::new();
  let mut stale: Vec<RunningSnapshot> = Vec::new();
  for snap in inputs.recorded_running.iter().cloned() {
    if !pid_alive(snap.pid) {
      stale.push(snap);
      continue;
    }
    let backend = crate::backend::Backends::all()
      .into_iter()
      .find(|b| b.id() == snap.resolved_backend)
      .unwrap_or_else(crate::backend::default_backend);
    // Orphan re-adoption is process-based, so it needs a path to confirm
    // against. Keyed on **lifecycle**, not identity shape: a managed
    // multiplexer is served by the shared umbrella and has no re-adoptable
    // child of its own, but a process-per-model backend spawns a real child
    // with a real path even when its identity is not a GGUF. Dropping those
    // unprobed left the server alive, holding its port and its GPU
    // allocation, in neither `running` nor `external` and reachable by no
    // stop command.
    let path = match snap.id.as_gguf() {
      Some(g) => g.path.clone(),
      None if backend.lifecycle() == crate::backend::Lifecycle::ProcessPerModel => {
        snap.params.model_path.clone()
      }
      None => {
        stale.push(snap);
        continue;
      }
    };
    // Three-factor confirmation: PID alive (above), port listening, and the
    // identity match — but the id contract is backend-specific (D-adopt), so
    // dispatch on the *recorded* backend tag, not the live process's basename.
    // A renamed / wrapped backend server still stamped its tag at launch, so
    // keying on the tag adopts it where a basename match would drop the live
    // child as stale (invisible to `status`, un-stoppable). The tag resolves to
    // a backend whose `adoption_matches` owns the id rule: the default
    // path/basename `/v1/models` match, or a backend's own (e.g. an alias body
    // plus an argv `-m` cross-check). PID reuse is still caught: a mismatched
    // live process fails that check regardless of tag.
    let argv: Vec<String> = sys
      .process(sysinfo::Pid::from_u32(snap.pid as u32))
      .map(|p| p.cmd().iter().map(|s| s.to_string_lossy().into()).collect())
      .unwrap_or_default();
    let matched = backend
      .adoption_matches(&path, &argv, snap.port, inputs.probe_timeout)
      .await;
    if !matched {
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
      // Skip threads. `sysinfo` lists each kernel task (thread) under its
      // own TID alongside the main process; without this filter a
      // multi-threaded `llama-server` surfaces once per thread (e.g. 36
      // identical rows for one process). Real processes have
      // `thread_kind() == None`.
      if proc.thread_kind().is_some() {
        return None;
      }
      let cmd: Vec<String> = proc
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy().into())
        .collect();
      // Match the marker against the executable basename only, not
      // arbitrary argv elements. Earlier versions scanned every argv
      // string for "llama-server", which caused llamastash to flag
      // itself: its argv carries `--llama-server <path>` whose value
      // contains the substring. `proc.name()` is the kernel-reported
      // basename — for `/usr/bin/llama-server` it is `llama-server`,
      // for `target/debug/llamastash` it is `llamastash`.
      let basename = proc.name().to_string_lossy();
      if !inputs
        .external_markers
        .iter()
        .any(|m| basename_matches_marker(&basename, m))
      {
        return None;
      }
      let model_path = extract_model_path(&cmd);
      let port = extract_port(&cmd);
      let start_time_secs = proc.start_time();
      // Inheritance marker — see `supervisor::spawn` for the stamp
      // side and the docstring on `ExternalProcess::launched_by_llamastash`
      // for the semantics. `proc.environ()` reads the live OS view,
      // so this stays accurate even when our own state.json is
      // empty (e.g. a UAT daemon that wrote to /tmp shut down,
      // children still alive).
      let launched_by_llamastash = proc
        .environ()
        .iter()
        .any(|e| e.to_string_lossy() == "LLAMASTASH_LAUNCHED=1");
      Some(ExternalProcess {
        pid: pid_u32,
        cmdline: cmd.join(" "),
        model_path,
        start_time_secs,
        port,
        launched_by_llamastash,
      })
    })
    .collect();

  SweepReport {
    adopted,
    stale,
    external,
  }
}

fn pid_alive(pid: i32) -> bool {
  if pid <= 0 {
    return false;
  }
  // Defer to the cross-platform liveness check so the Windows
  // backend lights up here without a second migration.
  crate::util::process_control::platform_default().is_alive(pid as u32)
}

/// Probe `/v1/models` on the recorded port and check that the
/// reported model id matches the supervisor's recorded path. Any
/// network error, non-200 response, malformed body, or mismatched
/// id evaluates to `false` — the sweep treats those as stale. This is the
/// default (llama.cpp) adoption id rule; the `Backend::adoption_matches`
/// default delegates here.
pub(crate) async fn models_endpoint_matches(port: u16, expected: &Path, timeout: Duration) -> bool {
  match fetch_models_body(port, timeout).await {
    Ok((200, body)) => body_mentions_path(&body, expected),
    _ => false,
  }
}

/// Adoption confirmation for a backend that advertises a **served name**
/// rather than the model path in `/v1/models`.
///
/// Exact match against the parsed `data[].id` values, never a scan of the raw
/// body — same reasoning as the readiness probe: a substring hit could come
/// from another entry's `root` path rather than from an id this server owns.
pub(crate) async fn models_endpoint_serves_id(
  port: u16,
  expected: &str,
  timeout: Duration,
) -> bool {
  let Ok((200, body)) = fetch_models_body(port, timeout).await else {
    return false;
  };
  crate::daemon::probe::served_model_ids(&body)
    .is_some_and(|ids| ids.iter().any(|id| id == expected))
}

/// Compare two model paths for adoption, tolerant of canonicalisation: try a
/// canonical compare first (resolves symlinks / `..`), fall back to a direct
/// path compare when either can't be canonicalised (file already gone). Shared
/// by a backend's `adoption_matches` argv `-m` cross-check.
pub(crate) fn paths_equal(a: &Path, b: &Path) -> bool {
  match (a.canonicalize(), b.canonicalize()) {
    (Ok(ca), Ok(cb)) => ca == cb,
    _ => a == b,
  }
}

/// GET `/v1/models` via `reqwest` — the same client the right-pane
/// chat tab uses, so the orphan probe doesn't carry its own HTTP/1.1
/// framing. Capped at 32 KiB so a misbehaving peer can't balloon our
/// memory. Returns `(status, body)` or an io error. Shared by a backend's
/// `adoption_matches` (the `/v1/models` endpoint both backends serve).
pub(crate) async fn fetch_models_body(
  port: u16,
  timeout: Duration,
) -> std::io::Result<(u16, Vec<u8>)> {
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

/// Strict-ish match: parse the `/v1/models` body as JSON and accept
/// adoption only when a documented `data[].id` field identifies the
/// expected model. We tolerate the two `llama-server` behaviours we
/// have observed in the wild (see `id_matches`), but never fall back
/// to a substring-anywhere scan — that would falsely adopt any local
/// process whose response body happened to contain the filename
/// (think `python -m http.server` serving a directory listing).
fn body_mentions_path(body: &[u8], expected: &Path) -> bool {
  let expected_str = expected.to_string_lossy();
  let Some(base) = expected.file_name().and_then(|s| s.to_str()) else {
    return false;
  };
  if base.is_empty() {
    return false;
  }
  // Only the documented OpenAI shape counts; extra fields are fine
  // (forward-compatible), a substring hit outside `data[].id` is not.
  crate::daemon::probe::served_model_ids(body).is_some_and(|ids| {
    ids
      .iter()
      .any(|id| id_matches(id, expected_str.as_ref(), base))
  })
}

/// Match a `/v1/models` `id` against the recorded model path, tolerant
/// of the two `llama-server` identity formats we've observed:
///
/// - **older builds** echo the literal `-m <path>` they were launched
///   with (an absolute path). We match it exactly, which still rejects
///   a same-basename-but-different-directory model — the PID-reuse
///   guard the three-factor sweep relies on.
/// - **b9245+** report only the file basename (e.g.
///   `gemma-4-E2B-it-Q4_K_M.gguf`). When the advertised id is itself a
///   bare basename (no path separator), we match the recorded path's
///   basename. The relaxation is gated on the id being bare, so a full
///   path that *differs* is still rejected, and the (pid, port) pair —
///   already confirmed alive and listening — pins the process.
fn id_matches(id: &str, expected_full: &str, expected_base: &str) -> bool {
  if id == expected_full {
    return true;
  }
  let id_is_bare = !id.contains('/') && !id.contains('\\');
  id_is_bare && id == expected_base
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

/// Extract `--port N` / `--port=N` / `-p N` from a `llama-server`
/// argv. The llama.cpp composer (`backend::llama_cpp::compose`) always
/// emits `--port <N>` for managed children, so this is the canonical
/// answer for any process that came from us. `None` means the cmdline
/// didn't carry the flag — the caller treats that as "port unknown"
/// rather than guessing the llama-server default.
pub fn extract_port(cmd: &[String]) -> Option<u16> {
  let mut iter = cmd.iter();
  while let Some(arg) = iter.next() {
    if arg == "--port" || arg == "-p" {
      if let Some(value) = iter.next() {
        if let Ok(p) = value.parse::<u16>() {
          return Some(p);
        }
      }
    } else if let Some(rest) = arg.strip_prefix("--port=") {
      if let Ok(p) = rest.parse::<u16>() {
        return Some(p);
      }
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A short single-token marker used to match any process whose name merely
  /// contained it — even with that backend disabled, which made unrelated
  /// tools legal `stop_external` targets. Compound markers keep their
  /// build-suffix tolerance, which is what the loose check was buying.
  #[test]
  fn a_marker_matches_its_own_binary_not_neighbours_that_share_the_name() {
    for (basename, marker, want) in [
      ("srv", "srv", true),
      ("llama-server", "llama-server", true),
      ("llama-server-cuda", "llama-server", true),
      // A short token says nothing once anything follows it.
      ("srv-router", "srv", false),
      ("srvproxy", "srv", false),
      ("my-srv-serve", "srv", false),
      ("usrv", "srv", false),
      ("not-llama-server", "llama-server", false),
      // llama.cpp's unified app: its own basename, nothing around it.
      ("llama", "llama", true),
      ("ollama", "llama", false),
      ("llama-server", "llama", false),
    ] {
      assert_eq!(
        basename_matches_marker(basename, marker),
        want,
        "{basename} vs {marker}"
      );
    }
  }

  use std::path::PathBuf;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;

  use crate::gguf::identity::ModelId;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;

  fn fake_snapshot(pid: i32, port: u16, path: &str, tag: u8) -> RunningSnapshot {
    RunningSnapshot {
      id: crate::backend::identity::ModelIdentity::Gguf(ModelId {
        path: PathBuf::from(path),
        header_blake3: [tag; 32],
      }),
      pid,
      port,
      started_at: 1_700_000_000,
      launch_id: None,
      params: LaunchParams::new(PathBuf::from(path), LaunchMode::Chat),
      actuals: Default::default(),
      resolved_backend: "llamacpp".to_string(),
    }
  }

  /// A loopback port that nothing is listening on: bind ephemeral, read
  /// it, drop. Only for the "connection refused" probe case below — the
  /// listening cases use `spawn_one_shot`, which keeps its port so there's
  /// no reuse window. A rare post-drop reuse here is absorbed by nextest
  /// `retries` on CI.
  fn allocate_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
  }

  /// Spin up a tiny single-shot HTTP responder on an OS-assigned loopback
  /// port, returning `(task, port)`. Binding the port here and holding it —
  /// rather than taking a pre-picked number — closes the bind-drop-rebind
  /// race that panicked CI with `AddrInUse` (the port could be reused
  /// between a separate allocate and this bind). Used by the orphan probe
  /// tests so we don't need the full `fake_llama_server` binary.
  async fn spawn_one_shot(status: u16, body: String) -> (tokio::task::JoinHandle<()>, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
      .await
      .expect("bind probe responder");
    let port = listener
      .local_addr()
      .expect("probe responder local_addr")
      .port();
    let task = tokio::spawn(async move {
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
    });
    (task, port)
  }

  #[test]
  fn paths_equal_matches_same_file_and_rejects_different() {
    // The ds4 adoption argv `-m` cross-check: two spellings of the same file
    // match; a different basename does not (per-file PID-reuse discrimination).
    let dir = crate::test_support::unique_temp_dir("orphans-paths", "eq");
    let a = dir.join("m.gguf");
    std::fs::write(&a, b"x").unwrap();
    assert!(paths_equal(&a, &a));
    // A `./`-prefixed spelling canonicalizes to the same file.
    let dotted = dir.join(".").join("m.gguf");
    assert!(paths_equal(&dotted, &a), "canonicalization collapses ./");
    // A different file in the same dir must not match.
    let b = dir.join("other.gguf");
    std::fs::write(&b, b"y").unwrap();
    assert!(!paths_equal(&a, &b));
    // A non-existent path falls back to a direct compare (unequal).
    assert!(!paths_equal(&dir.join("gone.gguf"), &a));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn ds4_tagged_snapshot_dispatches_to_alias_branch() {
    // A snapshot tagged `resolved_backend: "ds4"` takes the ds4 adoption path
    // (F8: dispatch keys on the recorded tag, not the process basename). The
    // test process's argv carries no `-m <recorded path>`, so the ds4 branch's
    // argv cross-check fails and the row is (correctly) stale — proving the
    // ds4 branch ran, not the llama.cpp path/basename rule (which would have
    // adopted on the alias-shaped body alone).
    let live = std::process::id() as i32;
    let body = serde_json::json!({
      "object": "list",
      "data": [{"id": "deepseek-v4-flash", "object": "model"}],
    })
    .to_string();
    let (_resp, port) = spawn_one_shot(200, body).await;
    let mut snap = fake_snapshot(live, port, "/m/deepseek-v4-flash.gguf", 1);
    snap.resolved_backend = "ds4".to_string();
    let report = sweep(SweepInputs {
      recorded_running: &[snap],
      external_markers: vec!["llamastash-sweep-marker-that-matches-nothing-9f3a"],
      probe_timeout: Duration::from_secs(1),
    })
    .await;
    assert!(
      report.adopted.is_empty() && report.stale.len() == 1,
      "ds4 branch must reject when argv `-m` doesn't match (adopted={}, stale={})",
      report.adopted.len(),
      report.stale.len()
    );
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
  fn extract_port_handles_long_short_and_inline_forms() {
    // Canonical llamastash-composed shape — long flag, space-separated
    // value. This is what `backend::llama_cpp::compose` always emits.
    let canon: Vec<String> = vec![
      "llama-server".into(),
      "--port".into(),
      "41102".into(),
      "-m".into(),
      "/m/a.gguf".into(),
    ];
    assert_eq!(extract_port(&canon), Some(41102));

    // Short flag.
    let short: Vec<String> = vec!["llama-server".into(), "-p".into(), "41200".into()];
    assert_eq!(extract_port(&short), Some(41200));

    // Inline `--port=N` (some users / wrappers prefer it).
    let inline: Vec<String> = vec!["llama-server".into(), "--port=41201".into()];
    assert_eq!(extract_port(&inline), Some(41201));

    // No port flag → None (caller decides whether to fall back).
    let bare: Vec<String> = vec!["llama-server".into(), "-m".into(), "/m/a.gguf".into()];
    assert_eq!(extract_port(&bare), None);

    // Garbage after the flag → None. The supervisor never emits this,
    // but a user could; we refuse to guess.
    let bad: Vec<String> = vec![
      "llama-server".into(),
      "--port".into(),
      "not-a-number".into(),
    ];
    assert_eq!(extract_port(&bad), None);
  }

  #[test]
  fn body_mentions_path_requires_strict_id_match() {
    let body = br#"{"data":[{"id":"/m/a.gguf","object":"model"}]}"#;
    assert!(body_mentions_path(body, Path::new("/m/a.gguf")));
    // b9245+ llama-server reports only the basename as `id`. A bare
    // basename equal to the recorded file must adopt (the (pid, port)
    // pair already pins the process).
    let body_basename = br#"{"data":[{"id":"a.gguf","object":"model"}]}"#;
    assert!(body_mentions_path(body_basename, Path::new("/m/a.gguf")));
    // A bare basename that differs must NOT adopt.
    let body_basename_other = br#"{"data":[{"id":"other.gguf"}]}"#;
    assert!(!body_mentions_path(
      body_basename_other,
      Path::new("/m/a.gguf")
    ));
    // Same basename in a different directory, advertised as a *full
    // path*, must NOT adopt. The id is a path that differs → reject
    // (the PID-reuse / friendly-fire guard for the old id format).
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
      external_markers: vec!["llamastash-sweep-marker-that-matches-nothing-9f3a"],
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
      external_markers: vec!["llamastash-sweep-marker-that-matches-nothing-9f3a"],
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    assert!(report.adopted.is_empty(), "no listener → must be stale");
    assert_eq!(report.stale.len(), 1);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn live_pid_with_matching_port_and_model_path_adopts() {
    let live = std::process::id() as i32;
    let body = serde_json::json!({
      "object": "list",
      "data": [{"id": "/m/match.gguf", "object": "model"}],
    })
    .to_string();
    let (_resp, port) = spawn_one_shot(200, body).await;

    let recorded = vec![fake_snapshot(live, port, "/m/match.gguf", 1)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: vec!["llamastash-sweep-marker-that-matches-nothing-9f3a"],
      probe_timeout: Duration::from_secs(1),
    })
    .await;
    assert_eq!(report.adopted.len(), 1, "matching probe must adopt");
    assert!(report.stale.is_empty());
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn live_pid_with_basename_only_id_adopts() {
    // Regression for the llama.cpp drift: b9245+ reports only the file
    // basename as `/v1/models` `id` (not the full `-m` path the older
    // builds echoed). The recorded path is absolute; the sweep must
    // still adopt when the basenames line up.
    let live = std::process::id() as i32;
    let body = serde_json::json!({
      "object": "list",
      "data": [{"id": "match.gguf", "object": "model"}],
    })
    .to_string();
    let (_resp, port) = spawn_one_shot(200, body).await;

    let recorded = vec![fake_snapshot(live, port, "/m/match.gguf", 1)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: vec!["llamastash-sweep-marker-that-matches-nothing-9f3a"],
      probe_timeout: Duration::from_secs(1),
    })
    .await;
    assert_eq!(
      report.adopted.len(),
      1,
      "basename-only id (llama.cpp b9245+) must still adopt"
    );
    assert!(report.stale.is_empty());
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn live_pid_with_mismatched_model_path_is_stale_pid_reuse_guard() {
    // Same PID + listening port, but the responder advertises a
    // different model — the canonical PID-reuse case. Adopt would
    // bind a stale `state.json::running` entry to an unrelated
    // process. Three-factor confirmation rejects it.
    let live = std::process::id() as i32;
    let body = serde_json::json!({
      "object": "list",
      "data": [{"id": "/m/different.gguf", "object": "model"}],
    })
    .to_string();
    let (_resp, port) = spawn_one_shot(200, body).await;

    let recorded = vec![fake_snapshot(live, port, "/m/expected.gguf", 1)];
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: vec!["llamastash-sweep-marker-that-matches-nothing-9f3a"],
      probe_timeout: Duration::from_secs(1),
    })
    .await;
    assert!(
      report.adopted.is_empty(),
      "mismatched model path must reject adoption"
    );
    assert_eq!(report.stale.len(), 1);
  }

  // Unix-only: spawns a real `sleep` binary and relies on POSIX
  // process-naming semantics. The sweep's matching *logic* is covered
  // cross-platform by the non-spawn unit tests above (`id_matches`,
  // basename adoption, stale classification). Windows has no portable
  // long-lived binary that also tolerates an arbitrary marker-bearing
  // argv (test 2), and Windows orphan re-adoption is N/A anyway — the
  // daemon's children share a kill-on-close job object.
  #[cfg(unix)]
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
      external_markers: vec!["sleep"],
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

  #[cfg(unix)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn marker_match_ignores_argv_values() {
    // Regression: the matcher must consider the executable basename
    // only, not arbitrary argv values. Spawn `sleep` with the
    // marker substring embedded in an arg — the process must NOT
    // be classified as external because its basename is `sleep`,
    // not `llama-server`. (Equivalent to llamastash carrying
    // `--llama-server <path>` in its own argv.)
    use std::process::{Command, Stdio};
    use std::time::Duration as StdDuration;

    let mut child = Command::new("sleep")
      .arg("30")
      .arg("--decoy=llama-server-path")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .expect("spawn sleep");

    std::thread::sleep(StdDuration::from_millis(100));
    let recorded: Vec<RunningSnapshot> = Vec::new();
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: vec!["llama-server"],
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    let matched_by_argv = report.external.iter().any(|p| p.pid == child.id());
    let _ = child.kill();
    let _ = child.wait();
    assert!(
      !matched_by_argv,
      "process whose basename does not match the marker must not be \
       classified as external even when an argv value contains the \
       marker substring (this was the bug that caused llamastash to \
       flag itself via `--llama-server <path>`)"
    );
  }
}
