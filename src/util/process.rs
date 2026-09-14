//! Spawn a child process with a wall-clock deadline and pipe-drain
//! threads so a verbose subcommand can't deadlock against its own
//! stdout.
//!
//! Replaces three near-identical implementations that grew across
//! the v2 surface:
//! - `crate::gpu::run_with_timeout` (vendor probes)
//! - [`crate::init::smoke::version_probe`] (`llama-server --version`)
//! - `run_brew_with_timeout` (brew install / brew --prefix)
//!
//! All three needed: capture both stdout and stderr, enforce a
//! wall-clock deadline, kill the child on expiry, never leak the
//! reader threads. Doing it in one place ensures the
//! reader-thread pattern (a.k.a. "do not call `wait_with_output`
//! after `try_wait` has reaped the child") is honoured everywhere
//! the first time.
//!
//! Callers map the returned `io::Result<Output>` into whatever typed
//! error the caller surfaces.

use std::io;
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Error kinds [`run_with_drain_and_timeout`] surfaces. Each caller
/// maps these into its own domain error.
#[derive(Debug)]
pub enum RunError {
  /// `Command::spawn` failed before any child existed.
  Spawn(io::Error),
  /// The child exceeded the wall-clock deadline; it has been killed
  /// and reaped before we return.
  Timeout { after: Duration },
  /// `try_wait` reported an OS error (extremely rare); the child has
  /// been killed and the reader threads joined before we return.
  Wait(io::Error),
}

impl std::fmt::Display for RunError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RunError::Spawn(e) => write!(f, "spawn: {e}"),
      RunError::Timeout { after } => write!(f, "exceeded {}s deadline", after.as_secs()),
      RunError::Wait(e) => write!(f, "waitpid: {e}"),
    }
  }
}

impl std::error::Error for RunError {}

/// Spawn `cmd` with piped stdio, drain stdout/stderr on background
/// threads, poll `try_wait` until either the child exits or
/// `timeout` elapses, then return the captured `Output`. On timeout
/// the child is killed + waited (so the OS doesn't leak the zombie)
/// and the reader threads are joined before this function returns.
///
/// Always pipes stdin (`Stdio::null`) and stdout/stderr; the caller's
/// `cmd` stdio settings are overwritten so the drainer can attach
/// readers reliably.
pub fn run_with_drain_and_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, RunError> {
  #[cfg(unix)]
  unsafe {
    // Put the spawned command in its own session/process group so a
    // timeout can kill shell grandchildren that inherit the same pipes.
    cmd.pre_exec(|| {
      if libc::setsid() < 0 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    });
  }
  cmd
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  let mut child = cmd.spawn().map_err(RunError::Spawn)?;
  let mut stdout_pipe = child.stdout.take().expect("piped stdout");
  let mut stderr_pipe = child.stderr.take().expect("piped stderr");
  let stdout_thread = thread::spawn(move || {
    let mut buf = Vec::new();
    let _ = stdout_pipe.read_to_end(&mut buf);
    buf
  });
  let stderr_thread = thread::spawn(move || {
    let mut buf = Vec::new();
    let _ = stderr_pipe.read_to_end(&mut buf);
    buf
  });

  let deadline = Instant::now() + timeout;
  let status = loop {
    match child.try_wait() {
      Ok(Some(s)) => break s,
      Ok(None) => {
        if Instant::now() >= deadline {
          kill_and_reap(&mut child);
          let _ = stdout_thread.join();
          let _ = stderr_thread.join();
          return Err(RunError::Timeout { after: timeout });
        }
        thread::sleep(Duration::from_millis(25));
      }
      Err(e) => {
        kill_and_reap(&mut child);
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        return Err(RunError::Wait(e));
      }
    }
  };

  let stdout = stdout_thread.join().unwrap_or_default();
  let stderr = stderr_thread.join().unwrap_or_default();
  Ok(Output {
    status,
    stdout,
    stderr,
  })
}

fn kill_and_reap(child: &mut Child) {
  terminate_child_tree(child);
  let _ = child.wait();
}

#[cfg(unix)]
fn terminate_child_tree(child: &mut Child) {
  let pid = child.id();
  // `pre_exec(setsid)` made the child a process-group leader, so a
  // negative pid signals the whole subtree, not just the shell wrapper.
  unsafe {
    libc::kill(-(pid as i32), libc::SIGKILL);
  }
  let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate_child_tree(child: &mut Child) {
  let _ = child.kill();
}

/// Detect whether a root filesystem contains Linux LXC container signatures.
///
/// Inspects (relative to `root`):
/// 1. `run/systemd/container`
/// 2. `proc/1/environ`
/// 3. `proc/1/cgroup`
pub fn linux_lxc_container_at(root: &std::path::Path) -> bool {
  let container = std::fs::read_to_string(root.join("run/systemd/container")).ok();
  let environ = std::fs::read_to_string(root.join("proc/1/environ")).ok();
  let cgroup = std::fs::read_to_string(root.join("proc/1/cgroup")).ok();

  is_lxc_container_signatures(container.as_deref(), environ.as_deref(), cgroup.as_deref())
}

/// Detect whether the current process is running inside a Linux LXC container.
///
/// Checks `/run/systemd/container`, `/proc/1/environ`, and `/proc/1/cgroup`
/// for container signatures. Used by UMA memory estimators and VRAM gauges
/// when the container memory limit is artificially lower than host hardware.
pub fn linux_lxc_container() -> bool {
  if !cfg!(target_os = "linux") {
    return false;
  }
  linux_lxc_container_at(std::path::Path::new("/"))
}

/// Pure signature check for LXC containers from container metadata strings.
pub fn is_lxc_container_signatures(
  systemd_container: Option<&str>,
  proc_environ: Option<&str>,
  proc_cgroup: Option<&str>,
) -> bool {
  if let Some(contents) = systemd_container {
    let normalized = contents.trim().to_ascii_lowercase();
    if !normalized.is_empty() && (normalized == "lxc" || normalized.contains("lxc")) {
      return true;
    }
  }

  if let Some(env) = proc_environ {
    let lower = env.to_ascii_lowercase();
    if lower.contains("container=lxc")
      || lower.contains("container=lxc-libvirt")
      || lower.contains("lxc")
    {
      return true;
    }
  }

  if let Some(cgroup) = proc_cgroup {
    let lower = cgroup.to_ascii_lowercase();
    if lower.contains("lxc") || lower.contains("/lxc/") || lower.contains("lxc.monitor") {
      return true;
    }
  }

  false
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn captures_stdout_from_a_clean_exit() {
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
      let dir = std::env::temp_dir().join(format!(
        "llamastash-process-test-{}-{nanos}",
        std::process::id()
      ));
      std::fs::create_dir_all(&dir).unwrap();
      let script = dir.join("emit");
      std::fs::write(&script, "#!/bin/sh\necho hello\n").unwrap();
      std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
      // Linux fork+exec across cargo's parallel test threads can race
      // and produce ETXTBSY (errno 26): a sibling thread between fork()
      // and exec() briefly holds the just-written script open. Retry a
      // bounded number of times — this is the documented workaround.
      const MAX_ETXTBSY_RETRIES: usize = 20;
      let mut attempts = 0;
      let out = loop {
        match run_with_drain_and_timeout(Command::new(&script), Duration::from_secs(2)) {
          Ok(out) => break out,
          Err(RunError::Spawn(e)) if e.raw_os_error() == Some(26) => {
            attempts += 1;
            assert!(
              attempts < MAX_ETXTBSY_RETRIES,
              "persistent ETXTBSY after {MAX_ETXTBSY_RETRIES} retries; \
               cargo test parallelism is starving fork+exec on this host"
            );
            std::thread::sleep(Duration::from_millis(50));
            continue;
          }
          Err(e) => panic!("unexpected spawn error: {e:?}"),
        }
      };
      assert!(out.status.success());
      assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
      std::fs::remove_dir_all(&dir).ok();
    }
  }

  #[test]
  fn kills_on_timeout() {
    #[cfg(unix)]
    {
      let mut cmd = Command::new("/bin/sh");
      cmd.arg("-c").arg("sleep 10");
      let start = Instant::now();
      let err = run_with_drain_and_timeout(cmd, Duration::from_millis(200)).unwrap_err();
      let elapsed = start.elapsed();
      assert!(matches!(err, RunError::Timeout { .. }));
      // Tolerance: timeout was 200 ms; we must return well under
      // the child's 10 s sleep — proves the kill+wait fired.
      assert!(
        elapsed < Duration::from_secs(2),
        "timeout should kill the child quickly; elapsed={elapsed:?}"
      );
    }
  }

  #[test]
  fn surfaces_spawn_error_for_missing_binary() {
    let cmd = Command::new("/nonexistent/path/to/some-binary");
    let err = run_with_drain_and_timeout(cmd, Duration::from_secs(1)).unwrap_err();
    assert!(matches!(err, RunError::Spawn(_)));
  }

  #[test]
  fn detects_lxc_from_systemd_container() {
    assert!(is_lxc_container_signatures(Some("lxc\n"), None, None));
    assert!(is_lxc_container_signatures(Some("LXC"), None, None));
    assert!(is_lxc_container_signatures(Some("lxc-libvirt"), None, None));
    assert!(!is_lxc_container_signatures(Some("docker"), None, None));
    assert!(!is_lxc_container_signatures(Some(""), None, None));
    assert!(!is_lxc_container_signatures(None, None, None));
  }

  #[test]
  fn detects_lxc_from_proc_environ() {
    assert!(is_lxc_container_signatures(
      None,
      Some("PATH=/usr/bin\0container=lxc\0USER=root"),
      None
    ));
    assert!(is_lxc_container_signatures(
      None,
      Some("container=lxc-libvirt"),
      None
    ));
    assert!(!is_lxc_container_signatures(
      None,
      Some("PATH=/usr/bin\0container=docker\0USER=root"),
      None
    ));
    assert!(!is_lxc_container_signatures(
      None,
      Some("container=podman"),
      None
    ));
  }

  #[test]
  fn detects_lxc_from_cgroup_paths() {
    assert!(is_lxc_container_signatures(
      None,
      None,
      Some("0::/lxc/100/init.scope")
    ));
    assert!(is_lxc_container_signatures(
      None,
      None,
      Some("1:name=systemd:/lxc.monitor/100")
    ));
    assert!(is_lxc_container_signatures(
      None,
      None,
      Some("0::/lxc.payload.100")
    ));
    assert!(!is_lxc_container_signatures(
      None,
      None,
      Some("0::/docker/e4b9d3f1a0")
    ));
    assert!(!is_lxc_container_signatures(
      None,
      None,
      Some("0::/user.slice/user-1000.slice")
    ));
  }

  #[test]
  fn linux_lxc_container_at_detects_systemd_container() {
    let dir = crate::util::test_temp::unique_temp_dir("lxc-detect-systemd");
    let container_dir = dir.join("run/systemd");
    std::fs::create_dir_all(&container_dir).expect("create run/systemd");
    std::fs::write(container_dir.join("container"), "lxc\n").expect("write container");

    assert!(linux_lxc_container_at(&dir));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn linux_lxc_container_at_detects_proc_environ() {
    let dir = crate::util::test_temp::unique_temp_dir("lxc-detect-environ");
    let proc_dir = dir.join("proc/1");
    std::fs::create_dir_all(&proc_dir).expect("create proc/1");
    std::fs::write(
      proc_dir.join("environ"),
      "PATH=/usr/bin\0container=lxc\0HOME=/root",
    )
    .expect("write environ");

    assert!(linux_lxc_container_at(&dir));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn linux_lxc_container_at_detects_cgroup() {
    let dir = crate::util::test_temp::unique_temp_dir("lxc-detect-cgroup");
    let proc_dir = dir.join("proc/1");
    std::fs::create_dir_all(&proc_dir).expect("create proc/1");
    std::fs::write(
      proc_dir.join("cgroup"),
      "0::/lxc/100/init.scope\n1:name=systemd:/lxc/100\n",
    )
    .expect("write cgroup");

    assert!(linux_lxc_container_at(&dir));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn linux_lxc_container_at_returns_false_for_non_lxc() {
    let dir = crate::util::test_temp::unique_temp_dir("lxc-detect-non-lxc");
    let container_dir = dir.join("run/systemd");
    std::fs::create_dir_all(&container_dir).expect("create run/systemd");
    std::fs::write(container_dir.join("container"), "docker\n").expect("write container");

    let proc_dir = dir.join("proc/1");
    std::fs::create_dir_all(&proc_dir).expect("create proc/1");
    std::fs::write(
      proc_dir.join("environ"),
      "PATH=/usr/bin\0container=docker\0HOME=/root",
    )
    .expect("write environ");
    std::fs::write(proc_dir.join("cgroup"), "0::/docker/e4b9d3f1a0\n").expect("write cgroup");

    assert!(!linux_lxc_container_at(&dir));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn linux_lxc_container_runs_cleanly() {
    let _ = linux_lxc_container();
  }
}
