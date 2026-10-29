//! Centralized memory-headroom policy.
//!
//! The budget authority (admission control) and the refusal messages it
//! produces are the *only* consumers. Every display surface — `status`,
//! the TUI host pane, the doctor hardware section, the init banner —
//! shows raw totals; the post-headroom number appears only when
//! admission refuses or degrades a launch and needs to explain why.
//!
//! Two questions live here so they have exactly one answer:
//!
//! 1. **Usable fraction** of a pool's raw total: how much can be handed
//!    to a model before the OS and other resident consumers are
//!    starved. Apple's unified pool defaults to the historical `0.75`, with an
//!    explicit user override for the additional reserve from available RAM. An AMD/Intel
//!    integrated UMA pool and a discrete VRAM pool are budgeted at the
//!    full total and lean on the overhead band below for their margin —
//!    we do not invent an unmeasured fraction for them.
//! 2. **Fixed overhead band** per GPU backend: bytes a successful load
//!    consumes beyond `weights + KV cache` (compute buffers, driver-side
//!    allocations the GGUF header can't predict). Values are the
//!    conservative defaults from `docs/spikes/2026-05-19-vram-overhead-band.md`;
//!    the spike marks them unverified, so the stance is "err high" and
//!    they must not be silently tightened.

use crate::daemon::host_metrics::HostMetricsSnapshot;

pub const DEFAULT_APPLE_RESERVE_PERCENT: u8 = 25;

/// The kind of physical memory pool a budget is being computed against.
/// Determined by the GPU classification (see [`crate::gpu`]); the
/// usable fraction differs per kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
  /// Apple Silicon unified memory — one pool shared with the OS, sized
  /// generously so the historical OS/app headroom matters.
  AppleUnified,
  /// AMD/Intel integrated UMA pool (carve-out + GTT on Strix Halo and
  /// friends). Budgeted at full total; the overhead band carries the
  /// margin.
  IntegratedUma,
  /// A discrete GPU's dedicated VRAM. Budgeted at full total; the
  /// overhead band carries the margin.
  DiscreteVram,
  /// Host system RAM (the CPU-offload / spill pool on discrete hosts).
  SystemRam,
}

/// Overhead band for a CUDA / ROCm / Metal backend. Compute buffers and
/// driver allocations beyond weights + KV cache. 512 MiB per the spike.
pub const OVERHEAD_BAND_NATIVE_BYTES: u64 = 512 * 1024 * 1024;

/// Overhead band for a Vulkan / unknown backend. Wider because the
/// Vulkan path's allocation behavior is less characterized. 1024 MiB
/// per the spike.
pub const OVERHEAD_BAND_VULKAN_BYTES: u64 = 1024 * 1024 * 1024;

/// Usable fraction of a pool's available memory. The only value below 1.0 is
/// Apple's default `0.75` can be changed with the configured reserve; every
/// other pool is budgeted at full total and relies on
/// [`overhead_band_bytes`] for its safety margin.
pub fn usable_fraction(pool: PoolKind, apple_reserve_percent: u8) -> f64 {
  match pool {
    PoolKind::AppleUnified => f64::from(100u8.saturating_sub(apple_reserve_percent)) / 100.0,
    PoolKind::IntegratedUma | PoolKind::DiscreteVram | PoolKind::SystemRam => 1.0,
  }
}

/// Apply [`usable_fraction`] to a raw pool total, returning the bytes
/// admission may hand out before the OS/app headroom is at risk.
pub fn admissible_bytes(raw_total: u64, pool: PoolKind, apple_reserve_percent: u8) -> u64 {
  (raw_total as f64 * usable_fraction(pool, apple_reserve_percent)) as u64
}

/// Fixed overhead band for a GPU backend string (the `backend` tag on
/// [`crate::gpu::GpuDevice`] / the `gpu_backend` wire value). Native
/// vendor backends use the narrow band; Vulkan/unknown uses the wide
/// one; CPU-only has no GPU compute buffer so the band is zero.
pub fn overhead_band_bytes(backend: &str) -> u64 {
  match backend {
    HostMetricsSnapshot::BACKEND_NVIDIA
    | HostMetricsSnapshot::BACKEND_AMD
    | HostMetricsSnapshot::BACKEND_APPLE_METAL
    | HostMetricsSnapshot::BACKEND_MULTI => OVERHEAD_BAND_NATIVE_BYTES,
    HostMetricsSnapshot::BACKEND_UNKNOWN => OVERHEAD_BAND_VULKAN_BYTES,
    // cpu_only / unsampled: no GPU compute buffer to reserve.
    _ => 0,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn apple_keeps_three_quarters() {
    assert_eq!(usable_fraction(PoolKind::AppleUnified, 25), 0.75);
    // 64 GiB unified → 48 GiB admissible, matching the old
    // aggregate_vram_bytes Apple ratio that moved here.
    let raw = 64 * 1024 * 1024 * 1024;
    assert_eq!(
      admissible_bytes(raw, PoolKind::AppleUnified, 25),
      48 * 1024 * 1024 * 1024
    );
  }

  #[test]
  fn non_apple_pools_use_full_total() {
    let raw = 32 * 1024 * 1024 * 1024;
    for pool in [
      PoolKind::IntegratedUma,
      PoolKind::DiscreteVram,
      PoolKind::SystemRam,
    ] {
      assert_eq!(usable_fraction(pool, 25), 1.0);
      assert_eq!(admissible_bytes(raw, pool, 25), raw);
    }
  }

  #[test]
  fn apple_memory_reserve_is_tunable_without_changing_other_pools() {
    let free = 40 * 1024 * 1024 * 1024;
    assert_eq!(
      admissible_bytes(free, PoolKind::AppleUnified, 10),
      36 * 1024 * 1024 * 1024
    );
    assert_eq!(admissible_bytes(free, PoolKind::AppleUnified, 0), free);
    assert_eq!(usable_fraction(PoolKind::AppleUnified, 99), 0.01);
    for pool in [
      PoolKind::IntegratedUma,
      PoolKind::DiscreteVram,
      PoolKind::SystemRam,
    ] {
      for reserve in [0, 10, 25, 99] {
        assert_eq!(admissible_bytes(free, pool, reserve), free);
      }
    }
  }

  #[test]
  fn overhead_band_per_backend() {
    assert_eq!(
      overhead_band_bytes(HostMetricsSnapshot::BACKEND_AMD),
      OVERHEAD_BAND_NATIVE_BYTES
    );
    assert_eq!(
      overhead_band_bytes(HostMetricsSnapshot::BACKEND_NVIDIA),
      OVERHEAD_BAND_NATIVE_BYTES
    );
    assert_eq!(
      overhead_band_bytes(HostMetricsSnapshot::BACKEND_APPLE_METAL),
      OVERHEAD_BAND_NATIVE_BYTES
    );
    assert_eq!(
      overhead_band_bytes(HostMetricsSnapshot::BACKEND_UNKNOWN),
      OVERHEAD_BAND_VULKAN_BYTES
    );
    assert_eq!(
      overhead_band_bytes(HostMetricsSnapshot::BACKEND_CPU_ONLY),
      0
    );
    assert_eq!(
      overhead_band_bytes(HostMetricsSnapshot::UNINITIALIZED_BACKEND),
      0
    );
  }
}
