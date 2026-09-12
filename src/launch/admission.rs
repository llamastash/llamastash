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

  /// Record a reservation without checking it against the budget.
  ///
  /// For a launch that goes ahead in spite of a refusal (`start --force`):
  /// [`try_admit`](Self::try_admit) reserves nothing when it refuses, so the
  /// forced launch's demand would be invisible to the next launch's check and
  /// a second one could be admitted against memory the first is already
  /// taking. Overcommitting once is the user's call; doing it twice by
  /// accident is not.
  pub fn reserve(&self, launch_id: u64, demand_bytes: u64) {
    self
      .inner
      .lock()
      .expect("admission ledger poisoned")
      .push(Reservation {
        launch_id,
        bytes: demand_bytes,
      });
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

/// The cache budget a unified-memory guard hands an engine that would
/// otherwise size its KV pool against the whole pool. Shared by every backend
/// whose engine has that default, so the three figures cannot drift apart.
///
/// Default budget when nothing else bounds it. Generous for a single user
/// (~85x concurrency at 2k context on a 0.5B) and small enough that the
/// launch cannot take the host down.
pub const DEFAULT_KV_CACHE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Floor for the cap. Below this the cache cannot serve a useful context, so
/// the launch is refused outright rather than admitted with a token cache that
/// would only fail after a full weight load.
pub const MIN_KV_CACHE_BYTES: u64 = 512 * 1024 * 1024;

/// Reserve left free for the OS and everything else after weights + cache,
/// for an engine whose own footprint has **not** been measured.
///
/// Prefer [`unified_host_reserve_bytes`] wherever that figure exists: a flat
/// 8 GiB is the number that left ~1.3 GiB free at ready on a 121 GiB box once
/// a measured engine's own 5.4-6.7 GiB came out of it.
pub const UNIFIED_HOST_RESERVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// What the OS itself needs kept free once the engine's own footprint is
/// counted separately: kernel, page cache breathing room, a shell.
pub const UNIFIED_OS_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Floor on the reserve as a share of the pool, so a large APU keeps more than
/// the fixed terms alone would leave it. 15% of 121.69 GiB is 18.25 GiB; on a
/// 32 GiB APU the fixed terms dominate instead.
const UNIFIED_RESERVE_FRACTION: f64 = 0.15;

/// Reserve left free after weights + cache on a unified host, given the
/// engine's own measured overhead beyond weights and the pool.
///
/// The flat [`UNIFIED_HOST_RESERVE_BYTES`] was "the OS and everything else",
/// calibrated on a box where the reserve never actually bound. Measured where
/// it does bind, the engine spent 5.4-6.7 GiB of it before the OS saw
/// anything, leaving about 1.3 GiB at ready — under an OOM killer's line on a
/// 121 GiB machine. So the reserve is the OS floor plus the engine's own
/// overhead plus the compute band the admission gate already prices, with a
/// share of the pool as the floor under all of that.
pub fn unified_host_reserve_bytes(
  ram_total_bytes: u64,
  gpu_backend: &str,
  engine_overhead_bytes: u64,
) -> u64 {
  let fixed = UNIFIED_OS_FLOOR_BYTES
    .saturating_add(engine_overhead_bytes)
    .saturating_add(crate::launch::headroom::overhead_band_bytes(gpu_backend));
  let share = (ram_total_bytes as f64 * UNIFIED_RESERVE_FRACTION) as u64;
  fixed.max(share)
}

/// The KV cache byte budget for a unified-memory host. **Always a value.**
///
/// `None` means **refuse the launch**, not "no opinion". An earlier version
/// floored the cap instead of refusing, on the theory that the admission gate
/// would catch the tight case. It cannot: the gate's demand is weights + *this*
/// figure, so shrinking the figure shrinks the very term the gate evaluates,
/// and a launch that should have been refused was admitted with the host
/// reserve silently abandoned. Whoever decides there is not enough memory has
/// to be whoever holds the number, which is here.
pub fn unified_kv_cache_budget(
  free_bytes: u64,
  weights_bytes: u64,
  reserve_bytes: u64,
) -> Option<u64> {
  let headroom = free_bytes
    .saturating_sub(weights_bytes)
    .saturating_sub(reserve_bytes);
  (headroom >= MIN_KV_CACHE_BYTES).then(|| headroom.min(DEFAULT_KV_CACHE_BYTES))
}

/// `1.5 GiB`-style label for a refusal message.
pub fn human_gib(b: u64) -> String {
  const GIB: f64 = (1024 * 1024 * 1024) as f64;
  format!("{:.1} GiB", b as f64 / GIB)
}

/// Parse a byte count that may carry a `K`/`M`/`G` suffix (the spelling a
/// launcher's own size flags accept), or a plain integer.
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

/// On-disk bytes of a directory-shaped model: every regular file directly
/// inside it, symlinks followed (the HF cache stores weights as links into
/// `blobs/`). Zero when `dir` is not a readable directory.
///
/// The fallback when the catalog has no size — a launch by absolute path from
/// outside the configured scan roots — because `stat` on a directory reports
/// its inode size, not its contents.
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

/// The pool an engine's **fraction knob** is a fraction *of*, or `0` when the
/// host cannot say.
///
/// `gpu_memory_utilization` / `mem_fraction_static` are shares of the device
/// total, not of what is free and not of the model — so a projection needs the
/// total, and which total depends on the host. On a unified host the device
/// total *is* system RAM, which is how these engines have frozen one; the
/// measured default is in each engine's setup doc. On a discrete host it is
/// VRAM, and system RAM is irrelevant to the fraction.
///
/// **Deliberately not the GTT pool**, even though [`effective_free_bytes`]
/// budgets `min(ram_free, gtt_free)` on a UMA host that reports one. The two
/// sides of the gate are then denominated differently, and on a default-config
/// APU — GTT roughly half of RAM — that over-refuses every hand-set fraction:
/// `0.9` prices against full RAM while free cannot exceed the GTT cap. Safe
/// but unhelpful, and `--force` or an absolute byte cap is the way through.
///
/// The alternative (`uma_shared_total_bytes.unwrap_or(ram_total_bytes)`) puts
/// both sides on one pool, but it is only correct if torch on an ROCm APU
/// reports GTT as its device total. If it reports sysmem instead, the engine
/// really does attempt `0.9 x full RAM` and pricing against GTT would
/// *understate* it — the freeze this function exists to project. Under-
/// projection is the direction that takes the host down, so the conservative
/// total stays until someone reads the real device total off an ROCm APU.
/// Tracked in `TODO.md`.
pub fn engine_pool_total_bytes(snap: &HostMetricsSnapshot) -> u64 {
  let unified = snap.unified || snap.gpu_backend == HostMetricsSnapshot::BACKEND_APPLE_METAL;
  if unified {
    snap.ram_total_bytes
  } else {
    snap.gpu_mem_total_bytes.unwrap_or(0)
  }
}

/// What a backend needs to price its own launch for the admission gate.
///
/// The gate holds these numbers already; passing them in stops each backend
/// re-deriving them from the snapshot and drifting from the figure the gate
/// actually compares against.
#[derive(Debug, Clone, Copy)]
pub struct DemandInputs {
  /// Post-headroom free bytes — the figure the demand is compared against.
  pub free_bytes: u64,
  /// The pool a fraction knob is a share of, from
  /// [`engine_pool_total_bytes`]. `0` when unknown.
  pub pool_total_bytes: u64,
  /// Weights the gate is **already** counting, so a backend whose knob covers
  /// weights as well as cache can report only the part beyond them.
  pub weights_bytes: u64,
}

/// An engine pool fraction priced for the admission gate: the bytes it will
/// hold **beyond the weights** the gate already counts.
///
/// Two corrections live here, and they pull opposite ways, which is why the
/// old `free * fraction` was not simply conservative:
///
/// - The fraction is of the pool, not of what is free. **On a unified host**
///   free is the smaller number, so multiplying by it *understated* the
///   allocation — with 52 GiB free of a 121 GiB pool, `0.9` projected 47 GiB
///   against a real 109 GiB, and the gate admitted the launch that freezes
///   the host. That is the case this fixes.
/// - The fraction also covers the weights, so the part beyond them is what the
///   gate wants; adding the whole figure counted the weights twice.
///
/// On a **discrete** host the inequality flips: [`effective_free_bytes`] sums
/// post-headroom VRAM free *and* system-RAM free, so free can exceed the VRAM
/// pool a fraction is priced against (a 24 GiB card beside 60 GiB of idle RAM
/// reads ~84 GiB free against a 24 GiB pool). The old code over-refused there;
/// this one under-tightens, and `0.9 x 24 GiB` is admitted by the gate for the
/// engine's own startup check to refuse instead. Same safety, worse message.
///
/// With no pool total (an unsampled or VRAM-less host) the free reading is the
/// only number available, so it stands in — understating, but the gate is
/// better engaged than skipped.
pub fn pool_fraction_beyond_weights(host: &DemandInputs, fraction: f64) -> u64 {
  let fraction = fraction.clamp(0.0, 1.0);
  let pool = if host.pool_total_bytes > 0 {
    host.pool_total_bytes
  } else {
    host.free_bytes
  };
  let engine_total = (pool as f64 * fraction) as u64;
  engine_total.saturating_sub(host.weights_bytes)
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
/// `resident_weight_bytes` is what the launch actually **holds** — the
/// shard-aware weight total minus the tensors the engine streams from the
/// mapping — measured once per launch by
/// [`launch_resident_bytes`](crate::daemon::launch_service) so the gate, the
/// probe scaler and the backend's cache cap all price the same figure. It is
/// deliberately not `weights_bytes(header)`: that only sums the tensors in
/// the header it is handed, which for a split GGUF is just the primary shard
/// (`…-00001-of-000NN.gguf`) and silently drops every trailing shard, so a
/// split model would be under-projected by the size of those shards and
/// wrongly admitted. The header is still used for the KV term (all attention
/// geometry lives in the primary shard's metadata).
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
#[allow(clippy::too_many_arguments)] // one arg per independent memory input
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
    // The GPU/RAM split is not modelled here — demand is the combined
    // total against the combined pool free — so `n_gpu_layers` would be
    // ignored downstream. Left unset rather than threaded in.
    n_gpu_layers: None,
  };
  resident_weight_bytes
    .saturating_add(kv_bytes(header, arch, opts))
    .saturating_add(overhead_band_bytes(backend))
    .saturating_add(mtp_band_bytes(resident_weight_bytes, mtp_active))
}

/// A conservative memory band for MTP speculative decoding, so the local OOM
/// gate isn't over-optimistic for a barely-fitting model. MTP adds the draft
/// head's resident weights + its draft context/compute buffers; `--fit` owns
/// GPU placement, but this local floor still under-projects without a band.
///
/// Calibrated from a measured idle delta of ~11% of weights (MTP on vs off on
/// Qwen3.5-4B-MTP: +320 MiB on 2.7 GiB weights), rounded up to ~16.7%
/// (`weights / 6`) to leave headroom for the active draft context under load.
/// A fraction of the **resident** weights, not the on-disk total: the draft
/// head is an ordinary resident tensor, so it scales with what the engine
/// holds rather than with what the file weighs. Zero when MTP is off.
/// Saturating.
fn mtp_band_bytes(resident_weight_bytes: u64, mtp_active: bool) -> u64 {
  if mtp_active {
    resident_weight_bytes / 6
  } else {
    0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The freeze guard. An engine's default sizes the KV cache against the
  /// pool, which on a UMA host is system RAM — measured at ~106 GB of a
  /// 121 GB box.
  #[test]
  fn unified_budget_leaves_the_host_a_reserve_and_is_never_absent() {
    const GB: u64 = 1024 * 1024 * 1024;
    let reserve = UNIFIED_HOST_RESERVE_BYTES;
    // Plenty free: the default budget applies, not "everything that fits".
    assert_eq!(unified_kv_cache_budget(113 * GB, GB, reserve), Some(8 * GB));
    // Tight: the cap shrinks to what is left after weights + reserve.
    assert_eq!(
      unified_kv_cache_budget(20 * GB, 8 * GB, reserve),
      Some(4 * GB)
    );
    // Too tight to serve a useful context: refuse. Flooring instead used to
    // shrink the demand the admission gate evaluates, so the gate could not
    // fire and the launch went ahead with the host reserve abandoned.
    assert_eq!(unified_kv_cache_budget(10 * GB, 8 * GB, reserve), None);
    assert_eq!(unified_kv_cache_budget(4 * GB, 8 * GB, reserve), None);
    assert_eq!(unified_kv_cache_budget(0, 0, reserve), None);
    // The exact boundary is admitted, not refused.
    assert_eq!(
      unified_kv_cache_budget(8 * GB + MIN_KV_CACHE_BYTES, 0, reserve),
      Some(MIN_KV_CACHE_BYTES)
    );
    // A bigger reserve takes the difference straight off the budget.
    assert_eq!(
      unified_kv_cache_budget(20 * GB, 8 * GB, 10 * GB),
      Some(2 * GB)
    );
  }

  /// The fraction is a share of the pool, less the weights the gate already
  /// counts. Pricing it against *free* understated the allocation, which is
  /// the direction that admits a launch the host cannot hold.
  #[test]
  fn a_pool_fraction_prices_the_pool_and_nets_off_the_weights() {
    const GB: u64 = 1024 * 1024 * 1024;
    let host = DemandInputs {
      free_bytes: 52 * GB,
      pool_total_bytes: 121 * GB,
      weights_bytes: GB,
    };
    // 0.9 of 121 GiB is 108.9, less the 1 GiB of weights.
    assert_eq!(
      pool_fraction_beyond_weights(&host, 0.9),
      ((121 * GB) as f64 * 0.9) as u64 - GB
    );
    // The old arithmetic against free would have fit inside the free reading;
    // the real figure does not, which is the whole point.
    assert!(pool_fraction_beyond_weights(&host, 0.9) + GB > host.free_bytes);
    assert!((host.free_bytes as f64 * 0.9) as u64 + GB < host.free_bytes + GB);

    // Out-of-range fractions clamp rather than wrap.
    assert_eq!(pool_fraction_beyond_weights(&host, 1.5), 121 * GB - GB);
    assert_eq!(pool_fraction_beyond_weights(&host, -1.0), 0);
    // A fraction smaller than the weights is not a negative demand.
    assert_eq!(pool_fraction_beyond_weights(&host, 0.001), 0);

    // No pool total (unsampled, or a VRAM-less discrete host): free stands in
    // rather than projecting nothing at all.
    let blind = DemandInputs {
      pool_total_bytes: 0,
      ..host
    };
    assert_eq!(
      pool_fraction_beyond_weights(&blind, 0.5),
      (52 * GB) / 2 - GB
    );
  }

  /// A fraction is a share of the device total, and which device that is
  /// depends on the host: system RAM when unified, VRAM when not.
  #[test]
  fn the_fraction_pool_is_ram_when_unified_and_vram_when_not() {
    const GB: u64 = 1024 * 1024 * 1024;
    let mut s = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_AMD.to_string(),
      unified: true,
      ram_total_bytes: 121 * GB,
      gpu_mem_total_bytes: Some(16 * GB),
      ..Default::default()
    };
    assert_eq!(engine_pool_total_bytes(&s), 121 * GB, "unified: the pool");

    s.unified = false;
    assert_eq!(engine_pool_total_bytes(&s), 16 * GB, "discrete: VRAM");

    // Apple is unified by construction even without the flag.
    let apple = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_APPLE_METAL.to_string(),
      unified: false,
      ram_total_bytes: 64 * GB,
      ..Default::default()
    };
    assert_eq!(engine_pool_total_bytes(&apple), 64 * GB);

    // A discrete host with no VRAM reading cannot say.
    let blind = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_CPU_ONLY.to_string(),
      unified: false,
      ram_total_bytes: 32 * GB,
      ..Default::default()
    };
    assert_eq!(engine_pool_total_bytes(&blind), 0);
  }

  /// The reserve covers the engine's own overhead, which the flat 8 GiB did
  /// not: measured on GB10 it left ~1.3 GiB at ready in the binding case.
  #[test]
  fn the_reserve_is_the_fixed_terms_or_a_share_of_the_pool_whichever_is_more() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let engine = 7 * GIB;
    let nvidia = HostMetricsSnapshot::BACKEND_NVIDIA;
    let fixed =
      UNIFIED_OS_FLOOR_BYTES + engine + crate::launch::headroom::overhead_band_bytes(nvidia);
    // A 32 GiB APU: 15% is 4.8 GiB, so the fixed terms hold.
    assert_eq!(unified_host_reserve_bytes(32 * GIB, nvidia, engine), fixed);
    // A DGX Spark: 15% of 121.69 GiB is 18.25 GiB and dominates.
    let spark_total = 130_657_042_432;
    let reserve = unified_host_reserve_bytes(spark_total, nvidia, engine);
    assert_eq!(reserve, (spark_total as f64 * 0.15) as u64);
    assert!(reserve > fixed);
    // Unknown backend takes the wider band the gate uses for it.
    let unknown = HostMetricsSnapshot::BACKEND_UNKNOWN;
    assert_eq!(
      unified_host_reserve_bytes(0, unknown, engine),
      UNIFIED_OS_FLOOR_BYTES + engine + crate::launch::headroom::overhead_band_bytes(unknown)
    );
    // An unmeasured engine still keeps the OS floor and the band.
    assert_eq!(
      unified_host_reserve_bytes(0, nvidia, 0),
      UNIFIED_OS_FLOOR_BYTES + crate::launch::headroom::overhead_band_bytes(nvidia)
    );
  }

  const GIB: u64 = 1024 * 1024 * 1024;

  /// The gate's only weight source for a directory launched from outside the
  /// scan roots, so a wrong answer here silently disarms the OOM refusal.
  /// Sizes come from the link target, the way the HF cache stores weights.
  #[test]
  fn dir_weight_bytes_follows_links_and_ignores_subdirectories() {
    let dir = crate::util::test_temp::unique_temp_dir("admission-dir-weight");
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs).expect("blobs");
    std::fs::write(blobs.join("sha-a"), vec![0u8; 4096]).expect("blob a");
    std::fs::write(blobs.join("sha-b"), vec![0u8; 2048]).expect("blob b");

    let snapshot = dir.join("snapshot");
    std::fs::create_dir_all(&snapshot).expect("snapshot");
    #[cfg(unix)]
    {
      std::os::unix::fs::symlink(blobs.join("sha-a"), snapshot.join("a.safetensors"))
        .expect("link a");
      std::os::unix::fs::symlink(blobs.join("sha-b"), snapshot.join("b.safetensors"))
        .expect("link b");
    }
    // A nested directory contributes nothing; only files directly inside do.
    std::fs::create_dir_all(snapshot.join("nested")).expect("nested");
    std::fs::write(snapshot.join("nested").join("ignored"), vec![0u8; 9999]).expect("ignored");

    #[cfg(unix)]
    assert_eq!(dir_weight_bytes(&snapshot), 4096 + 2048);
    assert_eq!(dir_weight_bytes(&dir.join("does-not-exist")), 0);
    std::fs::remove_dir_all(&dir).ok();
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

  #[test]
  fn parse_size_bytes_accepts_the_suffixes_a_launcher_flag_takes() {
    assert_eq!(parse_size_bytes("2147483648"), Some(2147483648));
    assert_eq!(parse_size_bytes("2G"), Some(2 * 1024 * 1024 * 1024));
    assert_eq!(parse_size_bytes("512M"), Some(512 * 1024 * 1024));
    assert_eq!(parse_size_bytes(" 8g "), Some(8 * 1024 * 1024 * 1024));
    assert_eq!(parse_size_bytes("not-a-size"), None);
    assert_eq!(parse_size_bytes(""), None);
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
    let knobs = crate::launch::knobs::KnobSet::new();
    // arch `None` → KV term is 0, isolating weights + overhead band.
    let band = overhead_band_bytes(HostMetricsSnapshot::BACKEND_AMD);
    let demand = project_demand(
      &header,
      None,
      &knobs,
      crate::backend::DEFAULT_BACKEND_ID,
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
      crate::backend::DEFAULT_BACKEND_ID,
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
