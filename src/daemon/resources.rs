//! Per-PID resource sampler. Backed by `sysinfo` so we don't depend
//! on `/proc` directly and stay portable across macOS / Linux.
//!
//! v1 captures RSS (resident set size in bytes) and CPU% averaged
//! over the previous sample window. VRAM per-PID isn't included
//! yet — that needs NVML (NVIDIA-only) and AMD doesn't expose it
//! cleanly. The IPC `status` handler reports a *system-level* VRAM
//! number (from `gpu::probe`) alongside the per-PID RSS/CPU; the
//! UI renders both rows.

use std::time::Duration;

use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

/// One reading. Returned by [`sample`] and via the supervisor's
/// per-model sampler loop.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ResourceReading {
  pub pid: u32,
  /// Resident-set size in bytes.
  pub rss_bytes: u64,
  /// CPU utilisation averaged across all cores, in percent
  /// (i.e. a fully-saturated 8-core machine reports ~800 %).
  /// `sysinfo` returns it pre-normalised to the per-core scale —
  /// we sum the children-of-this-pid view but `llama-server` is
  /// single-process so this is effectively one reading.
  pub cpu_percent: f32,
  /// Number of threads the process has.
  pub threads: u32,
}

/// One-shot snapshot for a single PID. Returns `None` when the
/// process has already exited.
pub fn sample(pid: u32) -> Option<ResourceReading> {
  let refresh = ProcessRefreshKind::nothing().with_cpu().with_memory();
  let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
  sys.refresh_processes_specifics(
    sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
    true,
    refresh,
  );
  let proc = sys.process(Pid::from_u32(pid))?;
  Some(ResourceReading {
    pid,
    rss_bytes: proc.memory(),
    cpu_percent: proc.cpu_usage(),
    threads: proc.tasks().map(|t| t.len() as u32).unwrap_or(1),
  })
}

/// A long-lived sampler that yields one reading per `interval` for
/// the supplied PID. Stops when the process disappears or the
/// returned `Receiver` is dropped.
///
/// `sysinfo`'s `cpu_usage` returns 0.0 on the very first refresh —
/// it needs two samples to compute a delta. The loop discards the
/// first reading so the receiver never sees the spurious 0%.
pub fn sample_loop(pid: u32, interval: Duration) -> tokio::sync::mpsc::Receiver<ResourceReading> {
  let (tx, rx) = tokio::sync::mpsc::channel(8);
  tokio::spawn(async move {
    let refresh = ProcessRefreshKind::nothing().with_cpu().with_memory();
    let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    // Prime the CPU delta calculation.
    sys.refresh_processes_specifics(
      sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
      true,
      refresh,
    );
    tokio::time::sleep(interval).await;
    loop {
      sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        refresh,
      );
      let Some(proc) = sys.process(Pid::from_u32(pid)) else {
        return;
      };
      let reading = ResourceReading {
        pid,
        rss_bytes: proc.memory(),
        cpu_percent: proc.cpu_usage(),
        threads: proc.tasks().map(|t| t.len() as u32).unwrap_or(1),
      };
      // Use `try_send` rather than `send().await`: a slow consumer
      // would otherwise back-pressure this task, pinning the
      // synchronous sysinfo `/proc` refresh on a tokio worker. For
      // resource sampling, a dropped reading is fine — the next tick
      // emits a fresh one. Channel closed means the consumer is
      // gone, in which case we exit.
      use tokio::sync::mpsc::error::TrySendError;
      match tx.try_send(reading) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
          // Consumer is slow; skip this reading.
        }
        Err(TrySendError::Closed(_)) => return,
      }
      tokio::time::sleep(interval).await;
    }
  });
  rx
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sample_for_self_returns_nonzero_rss() {
    let pid = std::process::id();
    let r = sample(pid).expect("self process should exist");
    assert_eq!(r.pid, pid);
    assert!(r.rss_bytes > 0, "RSS should be non-zero for a live process");
    assert!(r.threads >= 1);
  }

  #[test]
  fn sample_returns_none_for_obviously_dead_pid() {
    // 2^31 - 1 is the kernel pid_max ceiling on 64-bit Linux and
    // won't normally be allocated. macOS uses a smaller pid_max
    // (typically 99999), so we accept either "None" or a stale
    // reading — but since sysinfo only reports active PIDs, it'll
    // be None.
    let r = sample(2_147_483_646);
    assert!(r.is_none());
  }
}
