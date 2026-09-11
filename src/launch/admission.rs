//! Pre-spawn memory admission control + in-memory reservation ledger.
//!
//! llamastash delegates *placement* to llama-server's `--fit` but keeps
//! *budget authority*: before spawning a child it projects the launch's
//! demand floor against the sampled, post-headroom free memory minus the
//! bytes already reserved by in-flight launches. If the demand does not
//! fit, the launch is refused **before** spawn (cheap, deterministic) so
//! two concurrent oversized models can never double-book the same free
//! reading and OOM the box — the failure `--fit` alone can't prevent on
//! UMA, where its own free reading conflates the GTT pool with system
//! RAM.
//!
//! Design (kept deliberately simple — see plan scope amendment):
//! - **One combined budget.** UMA / Apple hosts budget the single
//!   physical pool (≈ system RAM); discrete hosts sum VRAM + system RAM.
//!   We compare combined demand against combined free rather than
//!   modelling a per-pool GPU/RAM split — conservative and adequate as a
//!   safety net.
//! - **Reservation = full demand**, held from admit until the child
//!   settles (Ready / Error / Stopped). While a child is Loading the
//!   sampler also sees its growing allocation, so the budget is counted
//!   slightly conservatively during that window — it errs toward
//!   refusing a second concurrent launch, never toward OOM.
//! - **Best-effort.** When there is no host-metrics sample yet
//!   (`unsampled`, or no sampler wired as in many tests) admission is
//!   skipped and the launch proceeds — we never block on missing data.
//! - **Never refuse on missing geometry.** A model whose GGUF lacks the
//!   attention fields contributes only its known weight bytes to demand.

use std::sync::Mutex;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::gguf::header::GgufHeader;
use crate::gguf::memory::{kv_bytes, parse_cache_type, EstimateOptions};
use crate::launch::headroom::{admissible_bytes, overhead_band_bytes, PoolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reservation {
  launch_id: u64,
  bytes: u64,
}

#[derive(Debug, Default)]
pub struct Ledger {
  inner: Mutex<Vec<Reservation>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
  pub demand_bytes: u64,
  pub effective_free_bytes: u64,
  pub reserved_bytes: u64,
}

impl Refusal {
  pub fn available_bytes(&self) -> u64 {
    self.effective_free_bytes.saturating_sub(self.reserved_bytes)
  }
}

impl Ledger {
  pub fn try_admit(
    &self,
    launch_id: u64,
    demand_bytes: u64,
    effective_free_bytes: u64,
  ) -> Result<(), Refusal> {
    let mut held = self.inner.lock().expect("admission ledger poisoned");
    let reserved_bytes: u64 = held.iter().map(|r| r.bytes).sum();
    if demand_bytes > effective_free_bytes.saturating_sub(reserved_bytes) {
      return Err(Refusal {
        demand_bytes,
        effective_free_bytes,
        reserved_bytes,
      });
    }
    held.push(Reservation { launch_id, bytes: demand_bytes });
    Ok(())
  }

  pub fn reserve(&self, launch_id: u64, demand_bytes: u64) {
    self
      .inner
      .lock()
      .expect("admission ledger poisoned")
      .push(Reservation { launch_id, bytes: demand_bytes });
  }

  pub fn release(&self, launch_id: u64) {
    self
      .inner
      .lock()
      .expect("admission ledger poisoned")
      .retain(|r| r.launch_id != launch_id);
  }

  pub fn reserved_bytes(&self) -> u64 {
    self
      .inner
      .lock()
      .expect("admission ledger poisoned")
      .iter()
      .map(|r| r.bytes)
      .sum()
  }
}

fn pool_kind(snap: &HostMetricsSnapshot) -> PoolKind {
  if snap.gpu_backend == HostMetricsSnapshot::BACKEND_APPLE_METAL {
    PoolKind::AppleUnified
  } else if snap.unified {
    PoolKind::IntegratedUma
  } else if snap.gpu_mem_total_bytes.is_some() {
    PoolKind::DiscreteVram
  } else {
    PoolKind::SystemRam
  }
}

pub fn is_sampled(snap: &HostMetricsSnapshot) -> bool {
  snap.gpu_backend != HostMetricsSnapshot::UNINITIALIZED_BACKEND
}

pub fn parse_size_bytes(raw: &str) -> Option<u64> {
  let s = raw.trim();
  let (digits, mult) = match s.chars().last()? {
    'k' | 'K' => (&s[..s.len() - 1], 1024),
    'm' | 'M' => (&s[..s.len() - 1], 1024 * 1024),
    'g' | 'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
    _ => (s, 1),
  };
  digits.trim().parse::<u64>().ok()?.checked_mul(mult)
}

pub fn dir_weight_bytes(dir: &std::path::Path) -> u64 {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return 0;
  };
  entries
    .flatten()
    .filter_map(|e| std::fs::metadata(e.path()).ok())
    .filter(|m| m.is_file())
    .map(|m| m.len())
    .fold(0u64, u64::saturating_add)
}

#[cfg(target_os = "linux")]
fn running_in_lxc() -> bool {
  if let Ok(container) = std::fs::read_to_string("/run/systemd/container") {
    if container.trim() == "lxc" {
      return true;
    }
  }
  if let Ok(environ) = std::fs::read("/proc/1/environ") {
    if environ.split(|byte| *byte == 0).any(|entry| entry == b"container=lxc") {
      return true;
    }
  }
  if let Ok(cgroup) = std::fs::read_to_string("/proc/1/cgroup") {
    if cgroup.lines().any(|line| {
      line.contains("/lxc/") || line.ends_with("/lxc") || line.contains("name=lxc")
    }) {
      return true;
    }
  }
  false
}

#[cfg(not(target_os = "linux"))]
fn running_in_lxc() -> bool {
  false
}

fn use_lxc_amd_gtt_budget(snap: &HostMetricsSnapshot, lxc: bool) -> bool {
  lxc
    && snap.gpu_backend == HostMetricsSnapshot::BACKEND_AMD
    && snap.unified
    && snap.uma_shared_total_bytes.is_some()
}

/// Post-headroom free bytes across the budget pool(s). Discrete hosts
/// sum post-headroom VRAM free + post-headroom system-RAM free.
///
/// UMA hosts normally budget `min(ram_free, gtt_free)` because both the
/// GPU's GTT cap and available system RAM can be limiting resources. There
/// is one container-specific exception: on Linux AMD UMA APUs inside an LXC,
/// the LXC memory limit applies to the container's CPU-side RAM accounting,
/// while the amdgpu GTT pool exposed to the container is the GPU allocation
/// budget. In that environment using the container's `MemAvailable` as a
/// second GPU limit incorrectly caps a 96 GiB Radeon 8060S to the LXC's 8 GiB
/// memory limit.
///
/// The exception is deliberately narrow: only Linux LXC + AMD + unified GPU
/// + sampled GTT data uses the GTT pool directly. Bare-metal AMD/Intel UMA,
/// Apple Silicon, and other container runtimes keep the existing conservative
/// `min(ram_free, gtt_free)` policy.
pub fn effective_free_bytes(snap: &HostMetricsSnapshot) -> u64 {
  effective_free_bytes_for(snap, running_in_lxc())
}

fn effective_free_bytes_for(snap: &HostMetricsSnapshot, lxc: bool) -> u64 {
  let ram_free = snap.ram_total_bytes.saturating_sub(snap.ram_used_bytes);
  let unified = snap.unified || snap.gpu_backend == HostMetricsSnapshot::BACKEND_APPLE_METAL;
  if unified {
    let pool_free = match snap.uma_shared_total_bytes {
      Some(gtt_total) => {
        let gtt_free = gtt_total.saturating_sub(snap.uma_shared_used_bytes.unwrap_or(0));
        if use_lxc_amd_gtt_budget(snap, lxc) {
          gtt_free
        } else {
          ram_free.min(gtt_free)
        }
      }
      None => ram_free,
    };
    admissible_bytes(pool_free, pool_kind(snap))
  } else if let (Some(total), Some(used)) = (snap.gpu_mem_total_bytes, snap.gpu_mem_used_bytes) {
    let vram_free = total.saturating_sub(used);
    admissible_bytes(vram_free, PoolKind::DiscreteVram)
      + admissible_bytes(ram_free, PoolKind::SystemRam)
  } else {
    admissible_bytes(ram_free, PoolKind::SystemRam)
  }
}

#[allow(clippy::too_many_arguments)]
pub fn project_demand(
  header: &GgufHeader,
  arch: Option<&str>,
  knobs: &crate::launch::knobs::KnobSet,
  backend_id: &str,
  effective_ctx: u32,
  backend: &str,
  resident_weight_bytes: u64,
  mtp_active: bool,
) -> u64 {
  let opts = EstimateOptions {
    ctx_len: effective_ctx as u64,
    cache_type_k: parse_cache_type(
      knobs.str_by_concept(backend_id, crate::launch::knobs::Concept::KvCacheKType),
    ),
    cache_type_v: parse_cache_type(
      knobs.str_by_concept(backend_id, crate::launch::knobs::Concept::KvCacheVType),
    ),
    n_gpu_layers: None,
  };
  resident_weight_bytes
    .saturating_add(kv_bytes(header, arch, opts))
    .saturating_add(overhead_band_bytes(backend))
    .saturating_add(mtp_band_bytes(resident_weight_bytes, mtp_active))
}

fn mtp_band_bytes(resident_weight_bytes: u64, mtp_active: bool) -> u64 {
  if mtp_active { resident_weight_bytes / 6 } else { 0 }
}

#[cfg(test)]
mod tests {
  use super::*;
  const GIB: u64 = 1024 * 1024 * 1024;

  fn snap(backend: &str, unified: bool, ram_total: u64, ram_used: u64) -> HostMetricsSnapshot {
    HostMetricsSnapshot {
      gpu_backend: backend.to_string(), unified, ram_total_bytes: ram_total, ram_used_bytes: ram_used,
      ..HostMetricsSnapshot::default()
    }
  }

  #[test]
  fn admits_when_demand_fits_and_records_reservation() {
    let ledger = Ledger::default();
    assert!(ledger.try_admit(1, 10 * GIB, 60 * GIB).is_ok());
    assert_eq!(ledger.reserved_bytes(), 10 * GIB);
  }

  #[test]
  fn refuses_when_demand_exceeds_free_minus_reservations() {
    let ledger = Ledger::default();
    ledger.try_admit(1, 44 * GIB, 60 * GIB).expect("first admits");
    let refusal = ledger.try_admit(2, 37 * GIB, 60 * GIB).expect_err("second must be refused");
    assert_eq!(refusal.reserved_bytes, 44 * GIB);
    assert_eq!(refusal.available_bytes(), 16 * GIB);
    assert_eq!(ledger.reserved_bytes(), 44 * GIB);
  }

  #[test]
  fn release_frees_the_pool_for_a_retry() {
    let ledger = Ledger::default();
    ledger.try_admit(1, 44 * GIB, 60 * GIB).expect("first admits");
    ledger.try_admit(2, 37 * GIB, 60 * GIB).expect_err("refused while first holds");
    ledger.release(1);
    assert_eq!(ledger.reserved_bytes(), 0);
    ledger.try_admit(2, 37 * GIB, 60 * GIB).expect("admits once the pool frees");
  }

  #[test]
  fn two_fitting_leaders_both_admit_and_sum() {
    let ledger = Ledger::default();
    ledger.try_admit(1, 20 * GIB, 60 * GIB).expect("first");
    ledger.try_admit(2, 30 * GIB, 60 * GIB).expect("second");
    assert_eq!(ledger.reserved_bytes(), 50 * GIB);
  }

  #[test]
  fn release_is_idempotent_and_targets_one_launch() {
    let ledger = Ledger::default();
    ledger.try_admit(1, 10 * GIB, 60 * GIB).unwrap();
    ledger.try_admit(2, 10 * GIB, 60 * GIB).unwrap();
    ledger.release(1);
    ledger.release(1);
    assert_eq!(ledger.reserved_bytes(), 10 * GIB);
  }

  #[test]
  fn parse_size_bytes_accepts_the_suffixes_a_launcher_flag_takes() {
    assert_eq!(parse_size_bytes("2147483648"), Some(2147483648));
    assert_eq!(parse_size_bytes("2G"), Some(2 * 1024 * 1024 * 1024));
    assert_eq!(parse_size_bytes("512M"), Some(512 * 1024 * 1024));
    assert_eq!(parse_size_bytes(" 8g "), Some(8 * 1024 * 1024 * 1024));
    assert_eq!(parse_size_bytes("not-a-size"), None);
    assert_eq!(parse_size_bytes(""), None);
  }

  #[test]
  fn uma_budget_falls_back_to_ram_when_gtt_unknown() {
    let s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 128 * GIB, 28 * GIB);
    assert_eq!(effective_free_bytes(&s), 100 * GIB);
  }

  #[test]
  fn uma_budget_uses_gtt_pool_not_system_ram() {
    let mut s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 160 * GIB, 80 * GIB);
    s.uma_shared_total_bytes = Some(80 * GIB);
    s.uma_shared_used_bytes = Some(60 * GIB);
    assert_eq!(effective_free_bytes(&s), 20 * GIB);
    let ledger = Ledger::default();
    assert!(ledger.try_admit(1, 37 * GIB, effective_free_bytes(&s)).is_err());
  }

  #[test]
  fn uma_budget_clamps_to_ram_when_gtt_exceeds_ram_free() {
    let mut s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 128 * GIB, 70 * GIB);
    s.uma_shared_total_bytes = Some(124 * GIB);
    s.uma_shared_used_bytes = Some(40 * GIB);
    assert_eq!(effective_free_bytes(&s), 58 * GIB);
  }

  #[test]
  fn lxc_amd_uma_uses_gtt_budget_instead_of_container_ram() {
    let mut s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 8 * GIB, 1 * GIB);
    s.uma_shared_total_bytes = Some(96 * GIB);
    s.uma_shared_used_bytes = Some(2 * GIB);
    assert_eq!(effective_free_bytes_for(&s, true), 94 * GIB);
    let ledger = Ledger::default();
    assert!(ledger.try_admit(1, 36 * GIB, effective_free_bytes_for(&s, true)).is_ok());
  }

  #[test]
  fn bare_metal_amd_uma_keeps_ram_gtt_minimum() {
    let mut s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 128 * GIB, 100 * GIB);
    s.uma_shared_total_bytes = Some(96 * GIB);
    s.uma_shared_used_bytes = Some(20 * GIB);
    assert_eq!(effective_free_bytes_for(&s, false), 28 * GIB);
  }

  #[test]
  fn non_amd_lxc_uma_does_not_use_gtt_only_budget() {
    let mut s = snap("nvidia", true, 8 * GIB, 1 * GIB);
    s.uma_shared_total_bytes = Some(96 * GIB);
    s.uma_shared_used_bytes = Some(2 * GIB);
    assert_eq!(effective_free_bytes_for(&s, true), 7 * GIB);
  }

  #[test]
  fn amd_uma_without_gtt_data_still_uses_ram() {
    let s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 8 * GIB, 1 * GIB);
    assert_eq!(effective_free_bytes_for(&s, true), 7 * GIB);
  }

  #[test]
  fn apple_budget_applies_075_headroom() {
    let s = snap(HostMetricsSnapshot::BACKEND_APPLE_METAL, true, 64 * GIB, 0);
    assert_eq!(effective_free_bytes(&s), 48 * GIB);
  }

  #[test]
  fn discrete_budget_sums_vram_and_ram_free() {
    let mut s = snap("nvidia", false, 128 * GIB, 64 * GIB);
    s.gpu_mem_total_bytes = Some(24 * GIB);
    s.gpu_mem_used_bytes = Some(8 * GIB);
    assert_eq!(effective_free_bytes(&s), 80 * GIB);
  }

  #[test]
  fn unsampled_snapshot_is_not_sampled() {
    let s = snap(HostMetricsSnapshot::UNINITIALIZED_BACKEND, false, 0, 0);
    assert!(!is_sampled(&s));
    let s2 = snap(HostMetricsSnapshot::BACKEND_AMD, true, GIB, 0);
    assert!(is_sampled(&s2));
  }

  #[test]
  fn demand_uses_shard_aware_weight_total_not_header_tensors() {
    let header = GgufHeader {
      version: 3,
      tensor_count: 0,
      metadata: std::collections::HashMap::new(),
      tensors: Vec::new(),
    };
    let knobs = crate::launch::knobs::KnobSet::new();
    let band = overhead_band_bytes(HostMetricsSnapshot::BACKEND_AMD);
    let demand = project_demand(
      &header, None, &knobs, crate::backend::DEFAULT_BACKEND_ID, 16384,
      HostMetricsSnapshot::BACKEND_AMD, 53 * GIB, false,
    );
    assert_eq!(demand, 53 * GIB + band);
    let mtp_demand = project_demand(
      &header, None, &knobs, crate::backend::DEFAULT_BACKEND_ID, 16384,
      HostMetricsSnapshot::BACKEND_AMD, 53 * GIB, true,
    );
    assert_eq!(mtp_demand, 53 * GIB + band + (53 * GIB / 6));
  }
}
