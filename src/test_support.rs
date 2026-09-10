//! Shared helpers for tests: the integration suites under `tests/`
//! and the inline `#[cfg(test)]` modules that need the same isolation.
//!
//! Gated behind the `test-fixtures` feature so consumer builds of the
//! library don't carry test-only utilities.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique temp directory for an integration test.
///
/// macOS `sun_path` is 104 bytes; the default `temp_dir()` already
/// eats ~50 of those, so we trim the time-based suffix and add a
/// process-local atomic counter. Two tests running on the same
/// millisecond used to share a directory (and a daemon, and a
/// runtime.json), which surfaced as periodic Connect-error flakes in
/// the chat smoke tests. `prefix` should be 2-5 chars.
pub fn unique_temp_dir(prefix: &str, label: &str) -> PathBuf {
  static SEQ: AtomicU64 = AtomicU64::new(0);
  let seq = SEQ.fetch_add(1, Ordering::Relaxed);
  let suffix = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_millis()
    % 0xFFFF_FFFF;
  let dir = std::env::temp_dir().join(format!(
    "{prefix}-{label}-{}-{suffix:x}-{seq:x}",
    std::process::id()
  ));
  std::fs::create_dir_all(&dir).expect("temp dir creation");
  dir
}

/// A launch-pool port range for a test daemon.
///
/// Probes a batch of ephemeral ports at once and spans the lowest to the
/// highest. The batch is the point: an ephemeral port is only ours until the
/// probe listener drops, and the daemon does not bind it until several
/// milliseconds later, so under a 40-way parallel test run another process
/// routinely takes it in between. A range sized to exactly one port has
/// nowhere to fall back and the launch dies with "no free port in N-N";
/// a spread gives `ports::allocate` (which walks the range linearly) somewhere
/// to land.
pub fn allocate_port_range(probes: usize) -> crate::config::loader::PortRange {
  let listeners: Vec<_> = (0..probes.max(1))
    .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral"))
    .collect();
  let mut ports: Vec<u16> = listeners
    .iter()
    .map(|l| l.local_addr().expect("local_addr").port())
    .collect();
  ports.sort_unstable();
  drop(listeners);
  crate::config::loader::PortRange {
    start: ports[0],
    end: ports[ports.len() - 1],
  }
}

/// Best-effort **synchronous** daemon shutdown, for test `Drop` guards (Drop
/// runs during unwind and can't drive an async client). Hand-rolls an
/// HTTP/1.0 `POST /rpc` carrying the JSON-RPC `shutdown` envelope against the
/// URL + token recorded in `runtime.json`. That trips the daemon's shutdown
/// token, so `run_foreground` runs its `stop_all_managed` step — which is
/// where every `setsid`-detached supervised child (`fake_llama_server`) gets
/// SIGTERM/SIGKILLed. Without it those children become init-owned orphans, the
/// historical source of leaked test fixtures. No-op when `runtime.json` is
/// absent (daemon already gone).
pub fn sync_shutdown_daemon(state_dir: &std::path::Path) -> std::io::Result<()> {
  use std::io::{Read, Write};
  use std::net::TcpStream;
  use std::time::Duration;
  let info = match crate::daemon::runtime_file::load(state_dir) {
    Ok(Some(i)) => i,
    _ => return Ok(()),
  };
  // The daemon binds loopback only, so the URL is always `http://127.0.0.1:<port>`.
  let host_port = info
    .ipc_url
    .strip_prefix("http://")
    .unwrap_or(info.ipc_url.as_str());
  let mut stream = TcpStream::connect(host_port)?;
  stream.set_write_timeout(Some(Duration::from_secs(1)))?;
  stream.set_read_timeout(Some(Duration::from_secs(1)))?;
  let body = br#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#;
  let req = format!(
    "POST /rpc HTTP/1.0\r\n\
     Host: {host_port}\r\n\
     Authorization: Bearer {token}\r\n\
     Content-Type: application/json\r\n\
     Content-Length: {len}\r\n\
     Connection: close\r\n\r\n",
    token = info.ipc_token,
    len = body.len(),
  );
  stream.write_all(req.as_bytes())?;
  stream.write_all(body)?;
  // Drain the response so the daemon's writer doesn't block on a full peer
  // buffer; the content doesn't matter — only that the token was tripped.
  let mut sink = [0u8; 512];
  let _ = stream.read(&mut sink);
  Ok(())
}

/// A [`RunningSnapshot`](crate::daemon::state_store::RunningSnapshot) for
/// tests, with every field pre-filled and one setter per field a test
/// actually varies.
///
/// `RunningSnapshot` is constructed in a dozen inline test modules and four
/// integration suites. Every field added to it used to mean editing each of
/// those literals; here it means one default in one place.
///
/// ```ignore
/// let row = running_row("/m/a.gguf").name("coder").launch_id("L2").build();
/// ```
pub struct RunningRow(crate::daemon::state_store::RunningSnapshot);

/// Start a [`RunningRow`] for a GGUF at `path`: launch `L1` on port 41100,
/// unnamed, chat mode, on the default backend.
pub fn running_row(path: &str) -> RunningRow {
  use crate::daemon::registry::LaunchId;
  use crate::daemon::state_store::RunningSnapshot;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;
  RunningRow(RunningSnapshot {
    id: crate::backend::identity::ModelIdentity::Gguf(crate::gguf::identity::ModelId {
      path: PathBuf::from(path),
      header_blake3: [7u8; 32],
    }),
    pid: 1,
    port: 41100,
    started_at: 0,
    launch_id: Some(LaunchId("L1".to_string())),
    name: None,
    params: LaunchParams::new(PathBuf::from(path), LaunchMode::Chat),
    actuals: Default::default(),
    resolved_backend: crate::backend::DEFAULT_BACKEND_ID.to_string(),
  })
}

impl RunningRow {
  /// Replace the GGUF identity with a backend (delegated / registry) one.
  pub fn identity(mut self, id: crate::backend::identity::ModelIdentity) -> Self {
    self.0.id = id;
    self
  }

  pub fn name(mut self, name: &str) -> Self {
    self.0.name = Some(name.to_string());
    self
  }

  /// The launch name as an `Option`, for a test that parameterises over both.
  pub fn maybe_name(mut self, name: Option<&str>) -> Self {
    self.0.name = name.map(str::to_string);
    self
  }

  pub fn launch_id(mut self, id: &str) -> Self {
    self.0.launch_id = Some(crate::daemon::registry::LaunchId(id.to_string()));
    self
  }

  /// Drop the launch id, as on a row adopted from a `state.json` written
  /// before the stamp existed.
  pub fn unstamped(mut self) -> Self {
    self.0.launch_id = None;
    self
  }

  pub fn port(mut self, port: u16) -> Self {
    self.0.port = port;
    self
  }

  pub fn pid(mut self, pid: i32) -> Self {
    self.0.pid = pid;
    self
  }

  pub fn started_at(mut self, secs: u64) -> Self {
    self.0.started_at = secs;
    self
  }

  pub fn params(mut self, params: crate::launch::params::LaunchParams) -> Self {
    self.0.params = params;
    self
  }

  pub fn resolved_backend(mut self, backend: &str) -> Self {
    self.0.resolved_backend = backend.to_string();
    self
  }

  pub fn build(self) -> crate::daemon::state_store::RunningSnapshot {
    self.0
  }
}
