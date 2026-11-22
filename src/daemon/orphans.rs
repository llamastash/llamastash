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
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sysinfo::{ProcessRefreshKind, System};
use tokio::sync::RwLock;

use super::shutdown::ShutdownToken;

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
  /// The launch name this process was started under, for a row that
  /// came from `state.json::running` (see
  /// [`ExternalProcess::from_adopted`]). `None` for a process the
  /// scan found, which never had one. It is the only piece of the
  /// launch's identity that survives the daemon it was started by,
  /// so `status` can still call the row `<model>@<name>` and `stop`
  /// can still take that name.
  #[serde(default)]
  pub name: Option<String>,
}

impl ExternalProcess {
  /// Project a re-adopted `state.json` row into the read-only external
  /// row the daemon surfaces for it.
  ///
  /// `cmdline` and `start_time_secs` come from the live process table
  /// when the OS still has them; the reconstructed cmdline is a label
  /// of last resort once the real argv is gone.
  pub fn from_adopted(
    adopted: &RunningSnapshot,
    cmdline: Option<String>,
    start_time_secs: u64,
  ) -> Self {
    // The launch's model path, whatever shape its identity is. Reading it off
    // `id.as_gguf()` yielded `None` for every non-GGUF identity, so a re-adopted
    // row of that kind reported `model_path: null` and a cmdline ending in a
    // dangling `-m ` — the user could see a surviving process but not which
    // model it was serving, which is the one thing the row exists to say.
    let model_path = adopted
      .id
      .as_gguf()
      .map(|g| g.path.clone())
      .unwrap_or_else(|| adopted.params.model_path.clone());
    Self {
      pid: adopted.pid as u32,
      // The process's own argv when the OS still has it, so the row reads as
      // what actually launched (`llama serve …` and `llama-server …` are the
      // same backend under different binaries). Otherwise reconstruct one from
      // the recorded backend's primary marker: registry-driven, names no
      // backend, and only a label once the real argv is gone.
      cmdline: cmdline.unwrap_or_else(|| {
        format!(
          "{} --port {} -m {}",
          crate::backend::adopted_process_name(&adopted.resolved_backend),
          adopted.port,
          model_path.display()
        )
      }),
      model_path: Some(model_path),
      start_time_secs,
      port: Some(adopted.port),
      // Adopted entries went through our state.json before the
      // restart — by construction they were launched by *this*
      // daemon's previous instance and therefore carry the same
      // env marker. Marking them keeps `collect_in_use_ports`
      // consistent across the adopted-vs-external split.
      launched_by_llamastash: true,
      name: adopted.name.clone(),
    }
  }
}

/// Inputs to a sweep — the daemon hands them in.
#[derive(Debug, Clone)]
pub struct SweepInputs<'a> {
  pub recorded_running: &'a [RunningSnapshot],
  /// Process basenames matched to detect backend-child invocations the daemon
  /// doesn't own. In production this is [`crate::backend::external_process_markers`]
  /// (every backend's `process_markers`), so the sweep learns a new backend's
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

/// The synchronous half of a scan: one `sysinfo` refresh of the whole process
/// table. Separate from [`sweep`] so a resweep tick can run it on a blocking
/// thread instead of an async worker.
fn scan_processes() -> Option<System> {
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
  // `refresh_processes` would also walk every thread (`with_tasks`), ~4x the
  // /proc reads, and `with_processes` on the constructor would scan once more
  // before this refresh does.
  let mut sys = System::new().ok()?;
  sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, process_refresh);
  Some(sys)
}

/// Run a sweep. Pure-ish modulo a `sysinfo` scan of process tables
/// and one short HTTP probe per adoption candidate.
pub async fn sweep(inputs: SweepInputs<'_>) -> SweepReport {
  let sys = scan_processes();
  let mut adopted: Vec<RunningSnapshot> = Vec::new();
  let mut stale: Vec<RunningSnapshot> = Vec::new();
  for snap in inputs.recorded_running.iter().cloned() {
    if !pid_alive(snap.pid) {
      stale.push(snap);
      continue;
    }
    let backend = crate::backend::Backends::from_id(&snap.resolved_backend)
      .unwrap_or_else(crate::backend::default_backend);
    // Orphan re-adoption is process-based, so it needs a path to confirm
    // against. Every backend spawns a real child with a real path, even when
    // its identity is not a GGUF. Dropping those unprobed left the server alive,
    // holding its port and its GPU allocation, in neither `running` nor
    // `external` and reachable by no stop command.
    let path = match snap.id.as_gguf() {
      Some(g) => g.path.clone(),
      None => snap.params.model_path.clone(),
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
      .as_ref()
      .and_then(|sys| sys.process(sysinfo::Pid::from_u32(snap.pid as u32)))
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

  let external = sys
    .as_ref()
    .map(|sys| external_rows(sys, &inputs.external_markers, &adopted_pids))
    .unwrap_or_default();

  SweepReport {
    adopted,
    stale,
    external,
  }
}

/// The external rows in a scanned process table: every process whose
/// basename matches a backend marker, minus `adopted_pids` (those are
/// re-adopted launches, not strangers) and this process's own children.
fn external_rows(
  sys: &System,
  external_markers: &[&'static str],
  adopted_pids: &BTreeSet<u32>,
) -> Vec<ExternalProcess> {
  let daemon_pid = std::process::id();
  sys
    .processes()
    .iter()
    .filter_map(|(pid, proc)| {
      let pid_u32 = pid.as_u32();
      if adopted_pids.contains(&pid_u32) {
        return None;
      }
      // Our own children are managed, never external. Supervised children run
      // under `setsid` so they outlive a crash, but while this daemon is up
      // their parent is still this process — including one mid-spawn that no
      // supervisor has registered yet, which a registry lookup would miss.
      if proc.parent().is_some_and(|p| p.as_u32() == daemon_pid) {
        return None;
      }
      // Skip threads. The refresh above leaves `with_tasks` off, so sysinfo
      // lists none today; if that changes, a multi-threaded `llama-server`
      // would surface once per thread (e.g. 36 identical rows for one
      // process). Real processes have `thread_kind() == None`.
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
      let matched = external_markers
        .iter()
        .find(|m| crate::backend::basename_matches_marker(&basename, m))?;
      // A basename is not always the whole answer: a backend whose server is
      // one mode of a multi-command binary gets to read the argv and disown the
      // other modes. Asked through the registry, so the sweep still names no
      // backend and no subcommand.
      if !crate::backend::external_argv_is_server(matched, &cmd) {
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
        // Found by scanning the process table, so there is no launch
        // record to take a name from.
        name: None,
      })
    })
    .collect()
}

/// Cadence of the periodic external resweep.
///
/// Fixed rather than configured: one process-table scan per tick is cheap, and
/// an interval knob would be one more thing to get wrong.
pub const RESWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Fold a fresh scan into the previous external list.
///
/// A row survives on liveness, not on being re-found. The scan matches on a
/// process-table basename plus argv, so it can miss a server the previous list
/// knows about: a re-adopted one whose binary was renamed or wrapped, or one
/// whose port rides a flag the scanner does not know. Such a process is alive,
/// holds a port and GPU memory, and is addressable through `stop --external`,
/// so its row is kept while the same process lives. The pid only counts as the
/// same process when a recorded start time still matches, otherwise the pid was
/// recycled onto something else and the row would describe a stranger.
///
/// A previous row carrying a `name` also wins over a fresh row for the same
/// pid, but only when the two rows describe the same process. The name is the
/// boot re-adoption's only record of that launch's address and a scan can never
/// produce it, so taking the scan row would erase the `<model>@<name>` form
/// `stop` takes; taking a named row for a recycled pid would name a stranger.
pub fn merge_external(
  previous: &[ExternalProcess],
  fresh: Vec<ExternalProcess>,
) -> Vec<ExternalProcess> {
  let mut merged: Vec<ExternalProcess> = fresh
    .into_iter()
    .map(|row| {
      match previous
        .iter()
        .find(|p| p.pid == row.pid && p.name.is_some())
      {
        Some(named) if start_times_agree(named.start_time_secs, row.start_time_secs) => {
          named.clone()
        }
        Some(_) | None => row,
      }
    })
    .collect();
  for carried in previous {
    if merged.iter().any(|e| e.pid == carried.pid) {
      continue;
    }
    if row_still_current(carried) {
      merged.push(carried.clone());
    }
  }
  // The scan walks a hash map, so its row order changes every tick. Sorting
  // keeps the `status` table and the TUI list from reshuffling each resweep.
  merged.sort_by_key(|e| e.pid);
  merged
}

/// Whether a previous row still describes the live process it was taken from.
///
/// `start_time_secs: 0` means the row never recorded one, which leaves liveness
/// as the only evidence; that is what a boot re-adoption of a process the
/// platform would not report a start time for looks like.
fn row_still_current(row: &ExternalProcess) -> bool {
  let live_start = match process_start_time(row.pid) {
    Some(start) => start,
    None => return false,
  };
  start_times_agree(row.start_time_secs, live_start) && pid_alive(row.pid as i32)
}

/// Whether two start times describe one process. A zero means "no start time was
/// recorded", which leaves nothing to compare; a matching non-zero one is
/// positive proof the pid was not recycled.
fn start_times_agree(recorded: u64, other: u64) -> bool {
  recorded == 0 || other == 0 || recorded == other
}

/// The process's start time in seconds, or `None` when no process has that pid.
fn process_start_time(pid: u32) -> Option<u64> {
  use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
  let mut sys = System::new().ok()?;
  sys.refresh_processes_specifics(
    ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
    true,
    ProcessRefreshKind::everything(),
  );
  sys.process(Pid::from_u32(pid)).map(|p| p.start_time())
}

/// One resweep tick: scan, merge, write.
///
/// The scan is synchronous `/proc` work and merging probes each carried row, so
/// both run on a blocking thread and nothing holds the write lock across a
/// syscall every `status` read waits behind.
async fn resweep_tick(external: &Arc<RwLock<Vec<ExternalProcess>>>) {
  let previous = external.read().await.clone();
  let scanned = previous.clone();
  let merged = tokio::task::spawn_blocking(move || {
    let markers = crate::backend::external_process_markers();
    let Some(sys) = scan_processes() else {
      return scanned;
    };
    let fresh = external_rows(&sys, &markers, &BTreeSet::new());
    merge_external(&scanned, fresh)
  })
  .await;
  let Ok(merged) = merged else { return };
  let mut slot = external.write().await;
  *slot = keep_removals(&previous, &slot, merged);
}

/// Drop merged rows the slot lost while the scan was running.
///
/// `stop_external` removes a row and kills its process. If that lands between
/// the read and the write, the scan still saw the process alive, and writing the
/// row back would resurrect a launch the operator just stopped.
fn keep_removals(
  previous: &[ExternalProcess],
  slot: &[ExternalProcess],
  merged: Vec<ExternalProcess>,
) -> Vec<ExternalProcess> {
  merged
    .into_iter()
    .filter(|row| {
      !previous.iter().any(|p| p.pid == row.pid) || slot.iter().any(|s| s.pid == row.pid)
    })
    .collect()
}

/// Refresh the shared external list every [`RESWEEP_INTERVAL`] until shutdown.
///
/// Without this the list is frozen at whatever the boot sweep found: a server
/// started after the daemon came up never appears, and a row whose process
/// exited never leaves. Nothing is re-adopted here, so a tick is one
/// process-table scan with no probing.
pub fn spawn_resweep(shutdown: ShutdownToken, external: Arc<RwLock<Vec<ExternalProcess>>>) {
  super::supervisor::spawn_supervised("external_resweep", async move {
    loop {
      tokio::select! {
        _ = shutdown.wait_until_triggered() => return,
        _ = tokio::time::sleep(RESWEEP_INTERVAL) => {}
      }
      resweep_tick(&external).await;
    }
  });
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

  use std::path::PathBuf;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;

  use crate::gguf::identity::ModelId;

  fn fake_snapshot(pid: i32, port: u16, path: &str, tag: u8) -> RunningSnapshot {
    crate::test_support::running_row(path)
      .identity(crate::backend::identity::ModelIdentity::Gguf(ModelId {
        path: PathBuf::from(path),
        header_blake3: [tag; 32],
      }))
      .pid(pid)
      .port(port)
      .started_at(1_700_000_000)
      .unstamped()
      .build()
  }

  /// A loopback port that nothing is listening on, for the "connection
  /// refused" probe case below. The listening cases use `spawn_one_shot`.
  fn allocate_port() -> u16 {
    crate::test_support::allocate_port_range(1).start
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
  async fn an_adopted_row_keeps_its_launch_name() {
    // D1's whole reason for putting the name on the snapshot rather than only
    // on the supervisor: a daemon restart re-adopts the live process from
    // `state.json`, and the launch has to still answer to `<model>@<name>`
    // afterwards. The supervisor is gone by then; the row is the only record.
    //
    // This asserts the sweep only. The caller then projects the adopted row
    // into an `external` row and used to drop the name doing it — so this test
    // passed while the address was dead. `from_adopted_carries_the_launch_name`
    // pins that half.
    let live = std::process::id() as i32;
    let body = serde_json::json!({
      "object": "list",
      "data": [{"id": "/m/match.gguf", "object": "model"}],
    })
    .to_string();
    let (_resp, port) = spawn_one_shot(200, body).await;

    let mut row = fake_snapshot(live, port, "/m/match.gguf", 1);
    row.name = Some("coder".to_string());
    let report = sweep(SweepInputs {
      recorded_running: &[row],
      external_markers: vec!["llamastash-sweep-marker-that-matches-nothing-9f3a"],
      probe_timeout: Duration::from_secs(1),
    })
    .await;
    assert_eq!(report.adopted.len(), 1, "matching probe must adopt");
    assert_eq!(
      report.adopted[0].name.as_deref(),
      Some("coder"),
      "the name has to survive the restart, or the address dies with the daemon"
    );
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
    // the marker, sweep, then kill it. It has to be detached: the sweep
    // skips the daemon's own children, and a child of the test process is
    // exactly that.
    use std::time::Duration as StdDuration;

    let pid = spawn_detached("sleep 30");
    std::thread::sleep(StdDuration::from_millis(100));
    let recorded: Vec<RunningSnapshot> = Vec::new();
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: vec!["sleep"],
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    let found = report.external.iter().any(|p| p.pid == pid);
    kill_detached(pid);
    assert!(
      found,
      "sweep should have found the spawned `sleep` process by cmdline marker"
    );
  }

  /// Spawn `cmd` so it outlives its shell parent, and return its pid.
  ///
  /// The shell backgrounds the command and exits, so the kernel re-parents it
  /// away from the test process. That is what makes the row a real external
  /// candidate: a direct child of the test process is a child of the daemon for
  /// the sweep, and gets skipped. The redirect matters: backgrounded, the child
  /// would hold the pipe `output()` reads and block the helper until it exits.
  #[cfg(unix)]
  fn spawn_detached(cmd: &str) -> u32 {
    let out = std::process::Command::new("sh")
      .arg("-c")
      .arg(format!("{cmd} >/dev/null 2>&1 & echo $!"))
      .output()
      .expect("spawn detached helper");
    String::from_utf8_lossy(&out.stdout)
      .trim()
      .parse()
      .expect("pid printed by helper shell")
  }

  #[cfg(unix)]
  fn kill_detached(pid: u32) {
    let _ = std::process::Command::new("kill")
      .arg(pid.to_string())
      .status();
  }

  #[cfg(unix)]
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn sweep_skips_the_daemons_own_children() {
    // The row a supervised launch would produce must never reach the external
    // list: it is a managed launch, stoppable by name, and `status` already
    // shows it. A direct child here stands in for a supervised child — same
    // parent pid, and no supervisor entry yet mid-spawn.
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
    let listed = report.external.iter().any(|p| p.pid == child.id());
    let _ = child.kill();
    let _ = child.wait();
    assert!(
      !listed,
      "a process the daemon spawned must not be listed as external"
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
    use std::time::Duration as StdDuration;

    let pid = spawn_detached("sleep 30 --decoy=llama-server-path");
    std::thread::sleep(StdDuration::from_millis(100));
    let recorded: Vec<RunningSnapshot> = Vec::new();
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: vec!["llama-server"],
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    let matched_by_argv = report.external.iter().any(|p| p.pid == pid);
    kill_detached(pid);
    assert!(
      !matched_by_argv,
      "process whose basename does not match the marker must not be \
       classified as external even when an argv value contains the \
       marker substring (this was the bug that caused llamastash to \
       flag itself via `--llama-server <path>`)"
    );
  }

  /// A crashed daemon leaves the launch running; its name is the only piece
  /// of the address that can survive, and this projection is where it either
  /// rides across or is lost. Testing `sweep` alone missed the drop once —
  /// `sweep` cloned the name faithfully and the caller discarded it.
  #[test]
  fn from_adopted_carries_the_launch_name() {
    let snap = crate::test_support::running_row("/models/qwen.gguf")
      .name("coder")
      .pid(4242)
      .port(41100)
      .build();
    let ext = ExternalProcess::from_adopted(&snap, Some("llama-server --port 41100".into()), 99);
    assert_eq!(ext.name.as_deref(), Some("coder"));
    assert_eq!(ext.pid, 4242);
    assert_eq!(ext.port, Some(41100));
    assert_eq!(ext.cmdline, "llama-server --port 41100");
    assert_eq!(ext.start_time_secs, 99);
    assert!(
      ext.launched_by_llamastash,
      "an adopted row came from our own state.json, so it carries the marker"
    );
  }

  /// An unnamed launch must not gain a name on the way across, and the
  /// reconstructed cmdline still has to say which model is serving.
  #[test]
  fn from_adopted_leaves_an_unnamed_row_unnamed() {
    let snap = crate::test_support::running_row("/models/qwen.gguf")
      .pid(4243)
      .port(41101)
      .build();
    let ext = ExternalProcess::from_adopted(&snap, None, 0);
    assert_eq!(ext.name, None);
    assert!(
      ext.cmdline.contains("/models/qwen.gguf"),
      "a reconstructed cmdline must still name the model: {}",
      ext.cmdline
    );
  }

  /// A process the scan found never went through `state.json`, so there is no
  /// name to claim — the `name` field must stay `None` for it.
  #[tokio::test]
  async fn scanned_external_rows_carry_no_name() {
    let recorded: Vec<RunningSnapshot> = Vec::new();
    let report = sweep(SweepInputs {
      recorded_running: &recorded,
      external_markers: vec!["llama-server"],
      probe_timeout: Duration::from_millis(100),
    })
    .await;
    assert!(
      report.external.iter().all(|e| e.name.is_none()),
      "a scan-found orphan has no launch record to take a name from"
    );
  }

  fn scan_row(pid: u32, port: Option<u16>) -> ExternalProcess {
    ExternalProcess {
      pid,
      cmdline: format!("llama-server --port {}", port.unwrap_or(8080)),
      model_path: Some(PathBuf::from("/m/a.gguf")),
      start_time_secs: 0,
      port,
      launched_by_llamastash: false,
      name: None,
    }
  }

  fn named_row(pid: u32, port: u16, name: &str) -> ExternalProcess {
    ExternalProcess {
      name: Some(name.to_string()),
      ..scan_row(pid, Some(port))
    }
  }

  #[test]
  fn merge_lets_the_fresh_scan_replace_the_previous_scan_rows() {
    // A previous row the rescan doesn't find is a process that exited. Left in
    // place, `status` would keep showing a dead server until the daemon
    // restarted.
    let previous = vec![scan_row(2_147_483_646, Some(41100))];
    let merged = merge_external(&previous, vec![scan_row(1002, Some(41101))]);
    let pids: Vec<u32> = merged.iter().map(|e| e.pid).collect();
    assert_eq!(pids, vec![1002]);
  }

  #[test]
  fn merge_keeps_a_live_named_row_the_scan_does_not_find() {
    // The re-adopted row is the only record of this launch's address, and the
    // scan can't reproduce it: it has no `state.json` to read a name from.
    let live = std::process::id();
    let previous = vec![named_row(live, 41102, "coder")];
    let merged = merge_external(&previous, Vec::new());
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].name.as_deref(), Some("coder"));
    assert_eq!(merged[0].port, Some(41102));
  }

  #[test]
  fn merge_drops_a_named_row_whose_process_has_exited() {
    // Liveness is the only thing that distinguishes "the scan missed it" from
    // "it died": a stopped server must drop off within one resweep.
    let previous = vec![named_row(2_147_483_646, 41103, "coder")];
    assert!(merge_external(&previous, Vec::new()).is_empty());
  }

  #[test]
  fn merge_keeps_the_name_when_the_scan_finds_the_same_pid_again() {
    // A daemon crash plus restart re-adopts the row, and the very next scan
    // finds the same live process without a name. Keeping the scan row here is
    // what made `<model>@<name>` stop working while the server was still up.
    let live = std::process::id();
    let previous = vec![named_row(live, 41104, "coder")];
    let merged = merge_external(&previous, vec![scan_row(live, Some(9999))]);
    assert_eq!(merged.len(), 1, "one process, one row");
    assert_eq!(merged[0].name.as_deref(), Some("coder"));
    assert_eq!(merged[0].port, Some(41104), "the named row wins outright");
  }

  #[test]
  fn merge_keeps_a_live_row_the_scan_cannot_match_even_without_a_name() {
    // A renamed or wrapped server, or one whose port rides a flag the scanner
    // doesn't know, is alive and holds a port but never comes back from a scan.
    // Carrying only named rows dropped it 30 s after boot.
    let live = std::process::id();
    // The recorded start time is the real one, so this goes through the identity
    // match rather than the "nothing recorded, liveness decides" escape.
    let live_start = process_start_time(live).unwrap_or(0);
    if live_start == 0 {
      return;
    }
    let previous = vec![ExternalProcess {
      start_time_secs: live_start,
      ..scan_row(live, Some(41106))
    }];
    let merged = merge_external(&previous, Vec::new());
    assert_eq!(merged.iter().map(|e| e.pid).collect::<Vec<_>>(), vec![live]);
    assert_eq!(merged[0].port, Some(41106));
  }

  #[test]
  fn merge_drops_a_carried_row_whose_pid_was_recycled() {
    // The pid is alive but belongs to something else now: its start time proves
    // it. Carrying the row would advertise a stranger's port as an idle server's.
    let live = std::process::id();
    let live_start = process_start_time(live).unwrap_or(0);
    if live_start == 0 {
      // No start time available on this platform, so identity can't be checked.
      return;
    }
    let previous = vec![ExternalProcess {
      start_time_secs: live_start + 1,
      ..named_row(live, 41107, "coder")
    }];
    assert!(merge_external(&previous, Vec::new()).is_empty());
  }

  #[test]
  fn merge_drops_a_carried_unnamed_row_whose_pid_was_recycled() {
    // Same rule without a name: nothing about the row makes a recycled pid safe
    // to carry.
    let live = std::process::id();
    let live_start = process_start_time(live).unwrap_or(0);
    if live_start == 0 {
      return;
    }
    let previous = vec![ExternalProcess {
      start_time_secs: live_start + 1,
      ..scan_row(live, Some(41108))
    }];
    assert!(merge_external(&previous, Vec::new()).is_empty());
  }

  #[test]
  fn merge_keeps_a_named_row_that_records_the_live_start_time() {
    // The carried row proves its own identity: recorded start time equals the
    // live one, so a recycled pid cannot imitate it.
    let live = std::process::id();
    let live_start = process_start_time(live).unwrap_or(0);
    if live_start == 0 {
      return;
    }
    let previous = vec![ExternalProcess {
      start_time_secs: live_start,
      ..named_row(live, 41109, "coder")
    }];
    let merged = merge_external(&previous, Vec::new());
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].name.as_deref(), Some("coder"));
  }

  #[test]
  fn merge_takes_the_fresh_row_when_a_named_pid_was_recycled() {
    // Same pid, different process: the name belongs to whatever used the pid
    // first, so the scan row is the honest one.
    let previous = vec![ExternalProcess {
      start_time_secs: 111,
      ..named_row(2_147_483_640, 41110, "coder")
    }];
    let fresh = vec![ExternalProcess {
      start_time_secs: 222,
      ..scan_row(2_147_483_640, Some(41111))
    }];
    let merged = merge_external(&previous, fresh);
    assert_eq!(merged.len(), 1, "one process, one row");
    assert_eq!(merged[0].name, None, "the recycled pid sheds the name");
    assert_eq!(merged[0].port, Some(41111));
  }

  #[test]
  fn merge_keeps_the_name_when_both_rows_record_one_start_time() {
    // The re-adoption and the scan agree on which process this is, so the name
    // stays and the address survives.
    let previous = vec![ExternalProcess {
      start_time_secs: 333,
      ..named_row(2_147_483_641, 41112, "coder")
    }];
    let fresh = vec![ExternalProcess {
      start_time_secs: 333,
      ..scan_row(2_147_483_641, Some(9999))
    }];
    let merged = merge_external(&previous, fresh);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].name.as_deref(), Some("coder"));
    assert_eq!(
      merged[0].port,
      Some(41112),
      "the named row still wins outright"
    );
  }

  #[test]
  fn keep_removals_does_not_resurrect_a_row_stopped_mid_tick() {
    // `stop_external` took the first row out of the slot while the scan ran. A
    // pid the scan found for the first time is new, so it stays.
    let previous = vec![
      scan_row(2_147_483_642, Some(41100)),
      scan_row(2_147_483_643, Some(41101)),
    ];
    let slot = vec![scan_row(2_147_483_643, Some(41101))];
    let merged = vec![
      scan_row(2_147_483_642, Some(41100)),
      scan_row(2_147_483_643, Some(41101)),
      scan_row(2_147_483_644, None),
    ];
    let kept: Vec<u32> = keep_removals(&previous, &slot, merged)
      .iter()
      .map(|e| e.pid)
      .collect();
    assert_eq!(kept, vec![2_147_483_643, 2_147_483_644]);
  }

  #[tokio::test]
  async fn resweep_tick_drops_a_row_whose_process_is_gone() {
    // End to end through the tick: the scan finds nothing for the dead pid, and
    // the merged list is what lands back in the slot.
    let external = Arc::new(RwLock::new(vec![scan_row(2_147_483_646, Some(41105))]));
    resweep_tick(&external).await;
    let slot = external.read().await;
    assert!(
      slot.iter().all(|e| e.pid != 2_147_483_646),
      "a dead row must not survive a tick: {slot:?}"
    );
  }

  #[test]
  fn merge_orders_rows_by_pid_so_the_list_does_not_reshuffle() {
    // The scan walks a hash map, so its row order differs every tick. Left
    // unsorted, the status table and the running list reordered every resweep.
    let merged = merge_external(
      &[],
      vec![
        scan_row(3003, None),
        scan_row(1001, None),
        scan_row(2002, None),
      ],
    );
    assert_eq!(
      merged.iter().map(|e| e.pid).collect::<Vec<_>>(),
      vec![1001, 2002, 3003]
    );
  }
}
