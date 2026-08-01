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

use crate::config::{KnobValueOpt, TypedKnobs};
use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::gguf::header::GgufHeader;
use crate::gguf::memory::{kv_bytes, parse_cache_type, EstimateOptions};
use crate::launch::headroom::{admissible_bytes, overhead_band_bytes, PoolKind};

/// One in-flight launch's hold on the budget, keyed by `launch_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reservation {
  launch_id: u64,
  bytes: u64,
}

/// In-memory reservation ledger. Shared across every launch entry point
/// (CLI `start`, TUI, proxy auto-start) via the daemon's
/// `MethodContext`, so check-and-reserve is atomic against concurrent
/// launches. Never persisted — restart safety comes from conservative
/// re-sampling, not from a durable ledger.
#[derive(Debug, Default)]
pub struct Ledger {
  inner: Mutex<Vec<Reservation>>,
}

/// Why a launch was refused, with the numbers needed to explain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
  /// Projected demand floor (weights + KV + overhead band).
  pub demand_bytes: u64,
  /// Post-headroom free across the budget pool(s), before reservations.
  pub effective_free_bytes: u64,
  /// Bytes already reserved by other in-flight launches.
  pub reserved_bytes: u64,
}

impl Refusal {
  /// Free bytes actually available to this launch (effective − reserved).
  pub fn available_bytes(&self) -> u64 {
    self
      .effective_free_bytes
      .saturating_sub(self.reserved_bytes)
  }
}

impl Ledger {
  /// Atomically check `demand_bytes` against `effective_free_bytes` minus
  /// the bytes already reserved, and on success record the reservation.
  /// One lock spans the read-and-reserve so two concurrent leaders cannot
  /// both pass against the same free reading.
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
    held.push(Reservation {
      launch_id,
      bytes: demand_bytes,
    });
    Ok(())
  }

  /// Drop the reservation for `launch_id` (on Ready / Error / Stopped, or
  /// when a refused launch releases its port). Idempotent.
  pub fn release(&self, launch_id: u64) {
    self
      .inner
      .lock()
      .expect("admission ledger poisoned")
      .retain(|r| r.launch_id != launch_id);
  }

  /// Total reserved bytes — for diagnostics and tests.
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

/// Headroom kind for the host's budget pool.
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

/// `true` once the daemon has a real host-metrics sample (not the
/// pre-first-tick `unsampled` placeholder). Admission only engages when
/// this holds.
pub fn is_sampled(snap: &HostMetricsSnapshot) -> bool {
  snap.gpu_backend != HostMetricsSnapshot::UNINITIALIZED_BACKEND
}

/// Post-headroom free bytes across the budget pool(s). Discrete hosts
/// sum post-headroom VRAM free + post-headroom system-RAM free.
///
/// UMA hosts budget the **GPU pool**, not all of system RAM. On an
/// AMD/Intel integrated APU the GPU can only allocate within the amdgpu
/// GTT cap (carve-out + GTT), which on a default-config box is roughly
/// half of system RAM. llama.cpp's own free reading conflates the two
/// and hard-OOMs (it sees system-RAM free, allocates past the GTT cap,
/// and `hipMalloc` fails); sysfs GTT is the budget authority. So when
/// the snapshot carries the GTT pool (`uma_shared_*`, from the sysfs
/// probe) we budget `min(ram_free, gtt_free)` — the GTT cap bounds the
/// GPU allocation, and `ram_free` still guards the rare case where
/// system RAM is the tighter constraint. Apple Silicon has no GTT carve
/// (it leaves `uma_shared_*` unset), so it falls back to `ram_free` with
/// its 0.75 headroom.
pub fn effective_free_bytes(snap: &HostMetricsSnapshot) -> u64 {
  let ram_free = snap.ram_total_bytes.saturating_sub(snap.ram_used_bytes);
  // Apple is unified by construction (the `|| apple_metal` just guards
  // it); the host-pane VRAM gauge keys off the same `unified` flag.
  let unified = snap.unified || snap.gpu_backend == HostMetricsSnapshot::BACKEND_APPLE_METAL;
  if unified {
    let pool_free = match snap.uma_shared_total_bytes {
      Some(gtt_total) => {
        let gtt_free = gtt_total.saturating_sub(snap.uma_shared_used_bytes.unwrap_or(0));
        ram_free.min(gtt_free)
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

/// Demand floor for a launch: model weights + KV cache at the effective
/// context window + the backend's fixed overhead band. Missing attention
/// geometry yields a KV of 0, so demand degrades to weights + band
/// rather than refusing on missing data.
///
/// `weights_total_bytes` is the **shard-aware** on-disk weight total
/// (from `discovery::shard_sizes` via the catalog row), not
/// `weights_bytes(header)` — the latter only sums the tensors in the
/// header it is handed, which for a split GGUF is just the primary shard
/// (`…-00001-of-000NN.gguf`) and silently drops every trailing shard. A
/// split model would otherwise be under-projected by the size of those
/// shards and wrongly admitted. The header is still used for the KV term
/// (all attention geometry lives in the primary shard's metadata).
///
/// **It is a floor, not a ceiling.** Under Auto the caller passes
/// `fit_ctx_floor` as `effective_ctx` (a pinned `--ctx` passes the pin),
/// so the KV term reflects the *minimum* context, not the (possibly much
/// larger) window `--fit` ends up choosing. So admission guarantees the
/// floor-sized launch fits, not fit's actual choice. The residual window
/// is "weights fit, fit then grows ctx past the floor": on a discrete
/// host fit self-limits against its own correct VRAM reading; on UMA the
/// GTT-pool budget in [`effective_free_bytes`] bounds it, and the
/// in-process load check is the final backstop. Weights dominate demand,
/// so the gross "this model is too big" case is always caught here.
pub fn project_demand(
  header: &GgufHeader,
  arch: Option<&str>,
  knobs: &TypedKnobs,
  effective_ctx: u32,
  backend: &str,
  weights_total_bytes: u64,
  mtp_active: bool,
) -> u64 {
  let opts = EstimateOptions {
    ctx_len: effective_ctx as u64,
    cache_type_k: parse_cache_type(knobs.cache_type_k.set_value().map(String::as_str)),
    cache_type_v: parse_cache_type(knobs.cache_type_v.set_value().map(String::as_str)),
    // The GPU/RAM split is not modelled here — demand is the combined
    // total against the combined pool free — so `n_gpu_layers` would be
    // ignored downstream. Left unset rather than threaded in.
    n_gpu_layers: None,
  };
  weights_total_bytes
    .saturating_add(kv_bytes(header, arch, opts))
    .saturating_add(overhead_band_bytes(backend))
    .saturating_add(mtp_band_bytes(weights_total_bytes, mtp_active))
}

/// A conservative memory band for MTP speculative decoding, so the local OOM
/// gate isn't over-optimistic for a barely-fitting model. MTP adds the draft
/// head's resident weights + its draft context/compute buffers; `--fit` owns
/// GPU placement, but this local floor still under-projects without a band.
///
/// Calibrated from a measured idle delta of ~11% of weights (MTP on vs off on
/// Qwen3.5-4B-MTP: +320 MiB on 2.7 GiB weights), rounded up to ~16.7%
/// (`weights / 6`) to leave headroom for the active draft context under load.
/// Zero when MTP is off. Saturating.
fn mtp_band_bytes(weights_total_bytes: u64, mtp_active: bool) -> u64 {
  if mtp_active {
    weights_total_bytes / 6
  } else {
    0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const GIB: u64 = 1024 * 1024 * 1024;

  #[test]
  fn admits_when_demand_fits_and_records_reservation() {
    let ledger = Ledger::default();
    assert!(ledger.try_admit(1, 10 * GIB, 60 * GIB).is_ok());
    assert_eq!(ledger.reserved_bytes(), 10 * GIB);
  }

  #[test]
  fn refuses_when_demand_exceeds_free_minus_reservations() {
    let ledger = Ledger::default();
    // First model reserves 44 GiB of a 60 GiB pool.
    ledger
      .try_admit(1, 44 * GIB, 60 * GIB)
      .expect("first admits");
    // Second model wants 37 GiB; only 16 GiB remains → refused, never
    // double-booked against the same free reading.
    let refusal = ledger
      .try_admit(2, 37 * GIB, 60 * GIB)
      .expect_err("second must be refused");
    assert_eq!(refusal.reserved_bytes, 44 * GIB);
    assert_eq!(refusal.available_bytes(), 16 * GIB);
    assert_eq!(
      ledger.reserved_bytes(),
      44 * GIB,
      "refusal reserves nothing"
    );
  }

  #[test]
  fn release_frees_the_pool_for_a_retry() {
    let ledger = Ledger::default();
    ledger
      .try_admit(1, 44 * GIB, 60 * GIB)
      .expect("first admits");
    ledger
      .try_admit(2, 37 * GIB, 60 * GIB)
      .expect_err("refused while first holds");
    ledger.release(1);
    assert_eq!(ledger.reserved_bytes(), 0);
    ledger
      .try_admit(2, 37 * GIB, 60 * GIB)
      .expect("admits once the pool frees");
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
    ledger.release(1); // no-op second time
    assert_eq!(ledger.reserved_bytes(), 10 * GIB);
  }

  fn snap(backend: &str, unified: bool, ram_total: u64, ram_used: u64) -> HostMetricsSnapshot {
    HostMetricsSnapshot {
      gpu_backend: backend.to_string(),
      unified,
      ram_total_bytes: ram_total,
      ram_used_bytes: ram_used,
      ..HostMetricsSnapshot::default()
    }
  }

  #[test]
  fn uma_budget_falls_back_to_ram_when_gtt_unknown() {
    // No sysfs GTT data on the snapshot → budget system-RAM free at the
    // IntegratedUma 1.0 fraction.
    let s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 128 * GIB, 28 * GIB);
    assert_eq!(effective_free_bytes(&s), 100 * GIB);
  }

  #[test]
  fn uma_budget_uses_gtt_pool_not_system_ram() {
    // Default-config UMA box: the amdgpu GTT cap is ~half of system RAM.
    // A resident model leaves plenty of system RAM free but little GTT.
    // Admission must budget the GTT pool, or it admits a model that then
    // hard-OOMs on hipMalloc (the exact conflation this feature defeats).
    let mut s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 160 * GIB, 80 * GIB);
    s.uma_shared_total_bytes = Some(80 * GIB); // GTT cap ~50% of RAM
    s.uma_shared_used_bytes = Some(60 * GIB); // 20 GiB GTT free
                                              // ram_free is 80 GiB but GTT free is only 20 GiB → budget GTT.
    assert_eq!(effective_free_bytes(&s), 20 * GIB);
    // A 37 GiB launch is refused against the 20 GiB GTT pool, not
    // admitted against the 80 GiB system-RAM figure.
    let ledger = Ledger::default();
    assert!(ledger
      .try_admit(1, 37 * GIB, effective_free_bytes(&s))
      .is_err());
  }

  #[test]
  fn uma_budget_clamps_to_ram_when_gtt_exceeds_ram_free() {
    // Reference-box config: GTT raised to ~full RAM, so GTT free can
    // exceed system-RAM free; min() keeps the tighter (RAM) bound.
    let mut s = snap(HostMetricsSnapshot::BACKEND_AMD, true, 128 * GIB, 70 * GIB);
    s.uma_shared_total_bytes = Some(124 * GIB);
    s.uma_shared_used_bytes = Some(40 * GIB); // 84 GiB GTT free
                                              // ram_free 58 GiB < gtt_free 84 GiB → budget the RAM bound.
    assert_eq!(effective_free_bytes(&s), 58 * GIB);
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
    // 16 GiB VRAM free + 64 GiB RAM free, both at 1.0 fraction.
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
    use crate::gguf::header::GgufHeader;
    // Empty header (no tensors): the per-shard `weights_bytes(header)`
    // this used to call would be 0. A split GGUF launches off its
    // primary shard, whose header omits every trailing shard's tensors,
    // so the old path under-projected demand by those shards. With the
    // shard-aware total threaded in, demand must reflect the passed
    // weight total regardless of what the header carries.
    let header = GgufHeader {
      version: 3,
      tensor_count: 0,
      metadata: std::collections::HashMap::new(),
      tensors: Vec::new(),
    };
    let knobs = TypedKnobs::default();
    // arch `None` → KV term is 0, isolating weights + overhead band.
    let band = overhead_band_bytes(HostMetricsSnapshot::BACKEND_AMD);
    let demand = project_demand(
      &header,
      None,
      &knobs,
      16384,
      HostMetricsSnapshot::BACKEND_AMD,
      53 * GIB,
      false,
    );
    assert_eq!(
      demand,
      53 * GIB + band,
      "weights term is the shard-aware total, not the header's tensor sum"
    );
    // MTP active adds a conservative weights-fraction band (weights / 6) on
    // top, so a barely-fitting model's OOM gate isn't over-optimistic.
    let mtp_demand = project_demand(
      &header,
      None,
      &knobs,
      16384,
      HostMetricsSnapshot::BACKEND_AMD,
      53 * GIB,
      true,
    );
    assert_eq!(
      mtp_demand,
      53 * GIB + band + (53 * GIB / 6),
      "MTP-active demand adds the ~16.7% draft-head band"
    );
  }
}
