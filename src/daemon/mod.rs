//! Daemon process: lockfile, socket bind, signal handling, accept loop.
//!
//! `run_foreground(opts)` does the whole lifecycle in the calling
//! process. `start_detached` re-execs the binary as a child with `setsid`
//! applied between `fork` and `exec`, then waits for the new daemon's
//! socket to become connectable before returning. The child is the daemon;
//! no in-runtime `fork()` is involved, which keeps the tokio runtime safe.

pub mod discovery_task;
pub mod host_metrics;
pub mod lockfile;
pub mod orphans;
pub mod peercred;
pub mod ports;
pub mod probe;
pub mod registry;
pub mod resources;
pub mod server;
pub mod shutdown;
pub mod state_store;
pub mod supervisor;

use std::{
  fs,
  path::{Path, PathBuf},
  time::Duration,
};

use anyhow::{anyhow, Context, Result};
use tokio::net::UnixListener;

use self::{
  discovery_task::DiscoveryOptions,
  lockfile::{acquire, AcquireOutcome},
  registry::SupervisorRegistry,
  shutdown::{install_signal_handlers, ShutdownToken},
  state_store::{load as load_state, RunningSnapshot},
};
use crate::config::loader::PortRange;
use crate::daemon::probe::ProbeOptions;
use crate::discovery::ModelCatalog;
use crate::ipc::methods::{LaunchEnv, MethodContext, PersistedState};

/// Options for starting the daemon. `state_dir` holds the PID lockfile;
/// `socket_path` is the Unix-domain socket the server binds to. Both
/// default to the OS-conventional paths via `util::paths`, but tests and
/// alternate deployments can override them.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
  pub state_dir: PathBuf,
  pub socket_path: PathBuf,
  /// Per-launch log directory. Each `start_model` opens a file
  /// under here so the supervisor's stdout/stderr tee + the
  /// `logs_tail` IPC method have a durable backing store.
  pub log_dir: PathBuf,
  /// `llama-server` binary path. `None` defers resolution to
  /// `start_model` time (current behaviour for tests that never
  /// launch); production startup pre-resolves so the daemon fails
  /// fast if the binary is missing.
  pub binary: Option<PathBuf>,
  /// Listening-port range Unit 5's allocator probes. Defaults to
  /// the plan's `41100..=41300`.
  pub port_range: PortRange,
  /// Discovery roots and scan tunables. An empty `scan_roots` list
  /// leaves the catalog empty until the user adds paths via the TUI
  /// or CLI; tests construct one of these with a temp dir seeded
  /// with `.gguf` fixtures.
  pub discovery: DiscoveryOptions,
  /// Per-launch health-probe deadline. `None` keeps the
  /// [`ProbeOptions::default`] timeout (120 s); production wiring
  /// passes through `Config::probe_timeout_secs` so users can raise
  /// it for very large models on slow disks.
  pub probe_timeout_secs: Option<u64>,
  /// Per-architecture launch defaults from `Config.arch_defaults`
  /// (R68). The daemon's `start_model` handler merges these into
  /// `LaunchParams.advanced` only for flags the caller has not
  /// already supplied (preset / last-params / explicit CLI outrank
  /// these per R69 precedence). Default: empty map.
  pub arch_defaults: std::collections::BTreeMap<String, crate::config::ArchDefaults>,
  /// Extra CLI args to propagate to the re-exec'd child when
  /// `start_detached` spawns the daemon. Tests leave this empty;
  /// production builds it from the parent's `--model-path` /
  /// `--no-scan` / `--llama-server` / `--config` flags so the
  /// detached child resolves the same discovery surface the parent
  /// would have. Without propagation the child rebuilds its options
  /// from an empty `Cli` and silently ignores the user's flags.
  pub propagated_cli_args: Vec<std::ffi::OsString>,
}

impl DaemonOptions {
  /// Test/utility helper: pin every path under one root directory.
  /// Production callers should prefer `from_defaults` plus the CLI's
  /// `build_options` flow, which threads config-driven overrides
  /// through.
  pub fn rooted_at(root: PathBuf) -> Self {
    let socket_path = root.join("daemon.sock");
    let log_dir = root.join("logs");
    Self {
      state_dir: root,
      socket_path,
      log_dir,
      binary: None,
      port_range: PortRange::default(),
      discovery: DiscoveryOptions::new(Vec::new()),
      probe_timeout_secs: None,
      arch_defaults: std::collections::BTreeMap::new(),
      propagated_cli_args: Vec::new(),
    }
  }

  /// Build options using the conventional XDG / macOS paths. Returns an
  /// error if the platform can't supply a state directory.
  pub fn from_defaults() -> Result<Self> {
    let state_dir = crate::util::paths::state_dir()
      .context("could not resolve a state directory for this platform")?;
    let socket_path = crate::util::paths::runtime_socket_path();
    let log_dir = crate::util::paths::log_dir()
      .context("could not resolve a cache/log directory for this platform")?;
    Ok(Self {
      state_dir,
      socket_path,
      log_dir,
      binary: None,
      port_range: PortRange::default(),
      // Production-default discovery: no scan roots until a later
      // commit threads config + CLI flags through `handle_start`.
      // Empty roots still produce a working daemon — `list_models`
      // returns `{"models": []}`.
      discovery: DiscoveryOptions::new(Vec::new()),
      probe_timeout_secs: None,
      arch_defaults: std::collections::BTreeMap::new(),
      propagated_cli_args: Vec::new(),
    })
  }
}

/// Outcome of starting the daemon — surfaces the "another daemon is
/// already running" case so the CLI can exit 0 with a friendly message
/// rather than a generic error.
pub enum StartOutcome {
  /// Daemon ran to clean shutdown.
  RanToCompletion,
  /// Another instance is already running.
  AlreadyRunning(i32),
}

/// Run the daemon in the current process. Returns when the accept loop
/// exits (either via the `shutdown` method, SIGINT, or SIGTERM).
pub async fn run_foreground(opts: DaemonOptions) -> Result<StartOutcome> {
  // 1. PID lockfile.
  let lockfile = match acquire(&opts.state_dir).context("acquiring PID lockfile")? {
    AcquireOutcome::Acquired(lock) => lock,
    AcquireOutcome::AlreadyRunning { pid, .. } => return Ok(StartOutcome::AlreadyRunning(pid)),
  };

  // 2. Bind the Unix socket. A stale socket from a SIGKILL'd previous run
  // must be cleared, but only after we hold the lockfile — otherwise we
  // could race with a legitimate running daemon.
  if opts.socket_path.exists() {
    fs::remove_file(&opts.socket_path)
      .with_context(|| format!("removing stale socket at {}", opts.socket_path.display()))?;
  }
  ensure_parent_dir(&opts.socket_path)?;
  // Bind under a restrictive umask so the socket inode is created
  // with mode 0o600 from the moment it exists. A bind→chmod sequence
  // would leave a TOCTOU window where the file is world-accessible
  // on Linux (peercred is the real auth boundary, but no need to
  // leave the door visibly open).
  let listener = with_restrictive_umask(|| {
    UnixListener::bind(&opts.socket_path)
      .with_context(|| format!("binding socket at {}", opts.socket_path.display()))
  })?;
  apply_socket_permissions(&opts.socket_path)?;
  log::info!("daemon listening on {}", opts.socket_path.display());

  // 3. Shutdown plumbing.
  let token = ShutdownToken::new();
  let _signal_task = install_signal_handlers(token.clone());

  // 4. Discovery. The catalog is shared between the discovery task
  // (writer) and the IPC dispatcher (reader). An empty scan_roots
  // produces a working daemon with an empty catalog — `list_models`
  // returns `{"models": []}`.
  let catalog = ModelCatalog::new();
  let _discovery = discovery_task::spawn(catalog.clone(), opts.discovery.clone());

  // 5. Persisted state — favorites, presets, last_params, running.
  // A parse failure does NOT block daemon start: the file is moved
  // to `state.json.broken-<ts>` and the daemon comes up with
  // defaults. Same posture as the plan's
  // `state.json corruption could brick the daemon` mitigation.
  let persisted_state = match load_state(&opts.state_dir) {
    Ok(s) => s,
    Err(e) => {
      log::warn!("state-store load failed; starting with defaults: {e}");
      quarantine_broken_state(&opts.state_dir);
      Default::default()
    }
  };

  // 6. Orphan / external sweep. Live recorded PIDs that still answer
  // `/v1/models` correctly are surfaced as *external* processes
  // (read-only; `stop_external` can target them) rather than re-
  // entering `state.running`. Rebuilding a full `ManagedModel` for
  // an adopted entry would require re-attaching to the live child's
  // stdout/stderr, which isn't feasible after process boundaries.
  // Without this routing change, adopted entries would persist in
  // `state.running` forever — `stop_model` returns InvalidParams
  // for them (no live supervisor), and every subsequent restart
  // would re-adopt the same row.
  let recorded_running: Vec<RunningSnapshot> = persisted_state.running.clone();
  let sweep = orphans::sweep(orphans::SweepInputs::new(&recorded_running)).await;
  let mut state_after_sweep = persisted_state;
  // Clear `running` — only the IPC `start_model` path repopulates
  // this slot via live supervisors going forward.
  state_after_sweep.running.clear();
  if let Err(e) = state_store::save(&opts.state_dir, &state_after_sweep) {
    log::warn!("state-store: failed to persist after orphan sweep: {e}");
  }
  // Merge adopted snapshots into the external slot. They share the
  // ExternalProcess shape (pid + cmdline + model_path); a synthetic
  // cmdline + best-effort start_time keeps the structure consistent.
  let mut external_combined = sweep.external.clone();
  for adopted in &sweep.adopted {
    if external_combined
      .iter()
      .any(|e| e.pid == adopted.pid as u32)
    {
      continue;
    }
    let start_time_secs = lookup_start_time(adopted.pid as u32).unwrap_or(0);
    external_combined.push(orphans::ExternalProcess {
      pid: adopted.pid as u32,
      cmdline: format!(
        "llama-server --port {} -m {}",
        adopted.port,
        adopted.id.path.display()
      ),
      model_path: Some(adopted.id.path.clone()),
      start_time_secs,
    });
  }
  log::info!(
    "orphan sweep: {} adopted (now external), {} stale, {} external",
    sweep.adopted.len(),
    sweep.stale.len(),
    sweep.external.len()
  );

  // 7. GPU probe (best-effort). Always returns *some* `GpuInfo`,
  // even if it's `CpuOnly`. Seeded for the fallback `ctx.gpu` slot
  // before the sampler's first tick lands.
  let initial_gpu = crate::gpu::probe();

  // 7b. Host-metrics sampler (1 Hz). Re-probes the active GPU
  // backend each tick for live util/temp/VRAM; sysinfo handles
  // host CPU% + RAM. The sampler also owns the live `GpuInfo` cell
  // so `status.gpu` follows hotplug instead of staying pinned to the
  // boot snapshot.
  let sampler =
    crate::daemon::host_metrics::spawn(token.clone(), std::time::Duration::from_secs(1));

  // 8. Wire the dispatcher context.
  let supervisors = SupervisorRegistry::new();
  let persisted = PersistedState::new(state_after_sweep, Some(opts.state_dir.clone()));
  let mut ctx = MethodContext::with_catalog(token.clone(), catalog)
    .with_supervisors(supervisors)
    .with_gpu(initial_gpu)
    .with_sampler(sampler)
    .with_state(persisted)
    .with_external(external_combined)
    .with_socket_path(opts.socket_path.clone());
  if let Some(binary) = opts.binary.clone() {
    if let Err(e) = std::fs::create_dir_all(&opts.log_dir) {
      log::warn!(
        "could not create log dir {}: {e}; logs may fail to open",
        opts.log_dir.display()
      );
    }
    let probe = match opts.probe_timeout_secs {
      Some(secs) => ProbeOptions {
        timeout: std::time::Duration::from_secs(secs),
        ..ProbeOptions::default()
      },
      None => ProbeOptions::default(),
    };
    ctx = ctx.with_launch_env(LaunchEnv {
      binary,
      port_range: opts.port_range,
      log_dir: opts.log_dir.clone(),
      probe,
      arch_defaults: opts.arch_defaults.clone(),
    });
  } else {
    log::info!(
      "daemon started without `llama-server` binary resolved; `start_model` will return an error until one is configured"
    );
  }

  // Hold a second handle to the dispatcher context so the
  // post-serve cleanup step (below) can reach the supervisor
  // registry after `serve` consumes its copy. `MethodContext` is
  // Arc-backed, so the clone is cheap and shares state.
  let cleanup_ctx = ctx.clone();

  // 9. Accept loop until shutdown is triggered.
  let result = server::serve(listener, ctx).await;

  // 9b. SIGTERM-then-SIGKILL every supervised `llama-server` before
  // exiting. The supervisor's `pre_exec(setsid)` makes each child a
  // session leader so it survives a daemon crash (R42's orphan
  // adoption rescues those on the next start). For *deliberate*
  // exits — `daemon stop`, SIGINT, SIGTERM, IPC `shutdown` — we
  // don't want children to leak. The 5 s grace mirrors
  // `default_grace_secs` in the IPC `stop_model` handler.
  let stopped = crate::ipc::methods::stop_all_managed(&cleanup_ctx, Duration::from_secs(5)).await;
  if !stopped.is_empty() {
    log::info!("shutdown: stopped {} managed launch(es)", stopped.len());
  }

  // 10. Cleanup. Lockfile cleans itself in Drop; the socket file is
  // removed here. We let the listener drop naturally.
  let _ = fs::remove_file(&opts.socket_path);
  drop(lockfile);

  result.map(|()| StartOutcome::RanToCompletion)
}

/// Best-effort `start_time` lookup for a PID. Used to seed
/// [`ExternalProcess::start_time_secs`] for adopted entries the
/// daemon can't itself supervise. Falls back to 0 if `sysinfo` has
/// no record (rare; means the PID has already exited).
fn lookup_start_time(pid: u32) -> Option<u64> {
  use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
  // `everything()` over a blank kind: explicit about wanting all
  // process metadata so `start_time()` is reliably populated across
  // sysinfo versions and platforms. The cost is one extra /proc read
  // per call, negligible at boot-sweep scale.
  let refresh = ProcessRefreshKind::everything();
  let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
  sys.refresh_processes_specifics(
    sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
    true,
    refresh,
  );
  // Filter `start_time == 0` so the consumer's `live != 0` guard
  // sees `None` instead of a meaningless zero.
  sys
    .process(Pid::from_u32(pid))
    .map(|p| p.start_time())
    .filter(|s| *s != 0)
}

/// Move a malformed `state.json` aside so the daemon can restart
/// with defaults. The plan's `state-json corruption` mitigation
/// — keeps the user's prior data on disk for inspection.
fn quarantine_broken_state(state_dir: &Path) {
  let src = state_dir.join("state.json");
  if !src.exists() {
    return;
  }
  let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  let dst = state_dir.join(format!("state.json.broken-{ts}"));
  if let Err(e) = std::fs::rename(&src, &dst) {
    log::warn!(
      "could not quarantine corrupt state.json to {}: {e}",
      dst.display()
    );
  }
}

/// Re-exec the current binary as a detached daemon child and wait for it
/// to bind its socket. The parent returns to the user's shell once the
/// socket is connectable; the child is the long-lived daemon.
///
/// We deliberately do **not** call `fork()` ourselves: this function may
/// be invoked from inside a multithreaded tokio runtime, and `fork()` in
/// that situation only carries the calling thread into the child, leaving
/// any mutex held by another thread permanently locked. `Command::spawn`
/// with `pre_exec(setsid)` gets us a properly detached child without
/// touching the runtime.
///
/// Mechanism:
/// 1. Spawn `llamastash daemon start` (foreground mode) with `stdin`/
///    `stdout`/`stderr` redirected to `/dev/null` and `setsid` applied
///    between `fork` and `exec`.
/// 2. Poll the configured socket path for up to ~3s, attempting a
///    connection. Success → daemon is ready; return.
/// 3. If the child has already exited (e.g. AlreadyRunning), reap it and
///    surface its exit status.
#[cfg(unix)]
pub fn start_detached(opts: DaemonOptions) -> Result<StartOutcome> {
  let exe = std::env::current_exe().context("locating current executable for --detach")?;
  start_detached_with_exe(opts, exe)
}

/// Detached-start with an explicit executable path. Production callers
/// should use [`start_detached`], which resolves `current_exe()` itself.
/// Integration tests use this overload to point at the test binary so
/// they can exercise the full re-exec path against temp `DaemonOptions`.
#[cfg(unix)]
#[doc(hidden)]
pub fn start_detached_with_exe(opts: DaemonOptions, exe: PathBuf) -> Result<StartOutcome> {
  use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
  };

  // Fast path: a live daemon already owns the socket. Don't spawn a
  // child only to have it bail out — the parent would observe the
  // existing daemon's socket as "connectable" and falsely report success.
  if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
    if std::os::unix::net::UnixStream::connect(&opts.socket_path).is_ok() {
      return Ok(StartOutcome::AlreadyRunning(pid));
    }
  }

  let mut cmd = Command::new(&exe);
  // Global flags (`--model-path`, `--no-scan`, `--llama-server`,
  // `--config`) must appear before the subcommand. clap accepts them
  // either side because they are `global = true`, but front-loading
  // them keeps the child's argv readable in `ps` output and avoids
  // any clap parse-order surprises.
  for arg in &opts.propagated_cli_args {
    cmd.arg(arg);
  }
  cmd
    .arg("daemon")
    .arg("start")
    // Propagate the caller-supplied paths to the re-exec'd child via
    // hidden flags. Without this, the child rebuilt `DaemonOptions`
    // from XDG defaults and silently ignored the parent's choices.
    .arg("--state-dir")
    .arg(&opts.state_dir)
    .arg("--socket-path")
    .arg(&opts.socket_path)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

  // SAFETY: `pre_exec` runs in the child between fork and exec. We call
  // only async-signal-safe operations: `setsid` is on the POSIX
  // async-signal-safe list. No locks, allocations, or other tokio state
  // are touched here.
  unsafe {
    cmd.pre_exec(|| {
      if libc::setsid() < 0 {
        return Err(std::io::Error::last_os_error());
      }
      Ok(())
    });
  }

  let mut child = cmd.spawn().context("spawning detached daemon")?;

  // Poll for the socket to become connectable. Bail out early if the
  // child has already exited (most commonly AlreadyRunning).
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if let Some(status) = child.try_wait()? {
      // Child exited before socket appeared. If the lockfile exists and
      // points to a live pid, the child saw an existing daemon; we can
      // report that cleanly. Otherwise it's an unexpected failure.
      if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
        return Ok(StartOutcome::AlreadyRunning(pid));
      }
      return Err(anyhow!(
        "detached daemon exited before binding socket (exit code: {:?})",
        status.code()
      ));
    }
    if std::os::unix::net::UnixStream::connect(&opts.socket_path).is_ok() {
      return Ok(StartOutcome::RanToCompletion);
    }
    if std::time::Instant::now() > deadline {
      // Don't leave the child orphaned if it's hung — kill and reap.
      let _ = child.kill();
      let _ = child.wait();
      return Err(anyhow!(
        "detached daemon did not bind socket within 3s ({})",
        opts.socket_path.display()
      ));
    }
    std::thread::sleep(Duration::from_millis(50));
  }
}

#[cfg(not(unix))]
pub fn start_detached(_opts: DaemonOptions) -> Result<StartOutcome> {
  Err(anyhow!("--detach is only supported on Unix targets"))
}

#[cfg(not(unix))]
#[doc(hidden)]
pub fn start_detached_with_exe(_opts: DaemonOptions, _exe: PathBuf) -> Result<StartOutcome> {
  Err(anyhow!("--detach is only supported on Unix targets"))
}

/// Returns the PID owning the daemon lockfile if (and only if) a live
/// process currently holds its `flock`. Used by `start_detached` to
/// short-circuit when an existing daemon already owns the socket.
///
/// Probing via `flock` rather than `kill(pid, 0)` matches `acquire`'s
/// ownership model: a recycled-PID collision can't masquerade as a live
/// daemon because the kernel released the lock when the original daemon
/// died, regardless of what the on-disk PID still says.
#[cfg(unix)]
fn existing_daemon_pid(state_dir: &Path) -> Option<i32> {
  use std::os::unix::io::AsRawFd;
  let pidfile = state_dir.join("daemon.pid");
  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&pidfile)
    .ok()?;
  // SAFETY: `flock(2)` is a kernel syscall over a borrowed fd; no memory
  // is touched. `file` outlives the call.
  let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
  if ret == 0 {
    // We just acquired the lock — no daemon is running. Dropping `file`
    // closes the fd and releases the lock.
    return None;
  }
  // Lock contended → a daemon owns the pidfile. Read the recorded PID
  // for the friendly "already running" message; ownership is decided by
  // the lock, the PID value is informational.
  let raw = fs::read_to_string(&pidfile).ok()?;
  raw.trim().parse::<i32>().ok().filter(|p| *p > 0)
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
  if let Some(parent) = path.parent() {
    create_dir_secure(parent)
      .with_context(|| format!("creating parent dir {}", parent.display()))?;
  }
  Ok(())
}

/// `create_dir_all` with mode 0o700 on Unix so freshly-created
/// per-user runtime directories (e.g. macOS's `$TMPDIR/llamastash-$USER`
/// fallback) are not world-readable. Linux's `$XDG_RUNTIME_DIR` is
/// already 0700 by the systemd contract, but the fallback path needs
/// this to keep parity. We don't downgrade pre-existing directories
/// — if the user has a more permissive parent, that's their call.
#[cfg(unix)]
fn create_dir_secure(path: &Path) -> std::io::Result<()> {
  use std::os::unix::fs::DirBuilderExt;
  if path.exists() {
    return Ok(());
  }
  // Walk up to the first existing ancestor; create the chain back
  // down with mode 0o700.
  let mut to_create: Vec<&Path> = Vec::new();
  let mut cur = Some(path);
  while let Some(p) = cur {
    if p.exists() {
      break;
    }
    to_create.push(p);
    cur = p.parent();
  }
  to_create.reverse();
  for p in to_create {
    std::fs::DirBuilder::new()
      .mode(0o700)
      .create(p)
      .or_else(|e| {
        // Race with another creator (rare but legal): tolerate.
        if e.kind() == std::io::ErrorKind::AlreadyExists {
          Ok(())
        } else {
          Err(e)
        }
      })?;
  }
  Ok(())
}

#[cfg(not(unix))]
fn create_dir_secure(path: &Path) -> std::io::Result<()> {
  std::fs::create_dir_all(path)
}

/// Run `f` with the process umask temporarily set to 0o077 so any
/// file inode it creates inherits mode bits 0o600 / 0o700. Safe for
/// the daemon's single-threaded startup; should NOT be called from
/// arbitrary tokio tasks because umask is process-global.
#[cfg(unix)]
fn with_restrictive_umask<T, F: FnOnce() -> Result<T>>(f: F) -> Result<T> {
  // SAFETY: `umask(2)` is async-signal-safe and operates on a
  // process-global integer. We're on the single-threaded startup
  // path before any worker tokio tasks have been spawned.
  let prev = unsafe { libc::umask(0o077) };
  let out = f();
  unsafe { libc::umask(prev) };
  out
}

#[cfg(not(unix))]
fn with_restrictive_umask<T, F: FnOnce() -> Result<T>>(f: F) -> Result<T> {
  f()
}

/// Apply mode `0600` to the socket file so other users on the host cannot
/// even open it. Peercred is the auth boundary that *catches* a bypass;
/// permissions are the boundary that *prevents* one.
#[cfg(unix)]
fn apply_socket_permissions(path: &Path) -> Result<()> {
  use std::os::unix::fs::PermissionsExt;
  fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    .with_context(|| format!("chmod 0600 on {}", path.display()))?;
  Ok(())
}

#[cfg(not(unix))]
fn apply_socket_permissions(_path: &Path) -> Result<()> {
  Ok(())
}

// Re-export the symbols downstream callers reach for.
#[allow(unused_imports)]
pub use lockfile::AcquireOutcome as LockfileOutcome;
#[allow(unused_imports)]
pub use lockfile::Lockfile as DaemonLockfile;

/// Default drain timeout exposed for callers (tests, CLI status command).
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = server::DRAIN_TIMEOUT;
