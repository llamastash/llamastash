//! Centralized memory-headroom policy.
//!
//! UMA defaults preserve Apple's historical 25% reserve and add no percentage
//! on other shared pools. Explicit overrides affect only UMA, after pool caps.
//! Engine overhead and in-flight reservations remain separate from headroom.

use std::{fmt, str::FromStr};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::daemon::host_metrics::HostMetricsSnapshot;

pub const DEFAULT_APPLE_RESERVE_PERCENT: u8 = 25;

/// Additional UMA headroom. An absent override retains the platform policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryReserve(Option<u8>);

impl MemoryReserve {
  pub const AUTO: Self = Self(None);
  pub const EXPECTED: &str = "auto or an integer from 0 to 99";

  pub fn percent(percent: u8) -> Result<Self, &'static str> {
    (percent <= 99)
      .then_some(Self(Some(percent)))
      .ok_or(Self::EXPECTED)
  }

  pub fn resolve(self, default_percent: u8) -> u8 {
    self.0.unwrap_or(default_percent)
  }

  pub fn admissible_bytes(self, available: u64, default_percent: u8) -> u64 {
    let fraction = f64::from(100u8.saturating_sub(self.resolve(default_percent))) / 100.0;
    (available as f64 * fraction) as u64
  }
}

impl fmt::Display for MemoryReserve {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.0 {
      Some(percent) => write!(f, "{percent}"),
      None => f.write_str("auto"),
    }
  }
}

impl FromStr for MemoryReserve {
  type Err = &'static str;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    if value == "auto" {
      Ok(Self::AUTO)
    } else {
      value
        .parse::<u8>()
        .map_err(|_| Self::EXPECTED)
        .and_then(Self::percent)
    }
  }
}

impl Serialize for MemoryReserve {
  fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    match self.0 {
      Some(percent) => serializer.serialize_u8(percent),
      None => serializer.serialize_str("auto"),
    }
  }
}

impl<'de> Deserialize<'de> for MemoryReserve {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct ReserveVisitor;
    impl de::Visitor<'_> for ReserveVisitor {
      type Value = MemoryReserve;

      fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(MemoryReserve::EXPECTED)
      }

      fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        u8::try_from(value)
          .ok()
          .and_then(|p| MemoryReserve::percent(p).ok())
          .ok_or_else(|| E::custom(MemoryReserve::EXPECTED))
      }

      fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value == "auto" {
          Ok(MemoryReserve::AUTO)
        } else {
          Err(E::custom(MemoryReserve::EXPECTED))
        }
      }
    }
    deserializer.deserialize_any(ReserveVisitor)
  }
}

/// The kind of physical memory pool a budget is being computed against.
/// Determined by the GPU classification (see [`crate::gpu`]); the
/// usable fraction differs per kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
  /// Apple Silicon unified memory — one pool shared with the OS, sized
  /// generously so the historical OS/app headroom matters.
  AppleUnified,
  /// Other shared pools, including AMD/Intel APUs and NVIDIA coherent UMA.
  /// Auto adds no percentage; engine guards remain independent.
  IntegratedUma,
  /// A discrete GPU's dedicated VRAM. Budgeted at full total; the
  /// overhead band carries the margin.
  DiscreteVram,
  /// Host system RAM (the CPU-offload / spill pool on discrete hosts).
  SystemRam,
}

impl PoolKind {
  pub fn default_reserve_percent(self) -> Option<u8> {
    match self {
      Self::AppleUnified => Some(DEFAULT_APPLE_RESERVE_PERCENT),
      Self::IntegratedUma => Some(0),
      Self::DiscreteVram | Self::SystemRam => None,
    }
  }
}

/// Overhead band for a CUDA / ROCm / Metal backend. Compute buffers and
/// driver allocations beyond weights + KV cache. 512 MiB per the spike.
pub const OVERHEAD_BAND_NATIVE_BYTES: u64 = 512 * 1024 * 1024;

/// Overhead band for a Vulkan / unknown backend. Wider because the
/// Vulkan path's allocation behavior is less characterized. 1024 MiB
/// per the spike.
pub const OVERHEAD_BAND_VULKAN_BYTES: u64 = 1024 * 1024 * 1024;

/// Apply additional headroom to available UMA memory, leaving other pools alone.
pub fn admissible_bytes(available: u64, pool: PoolKind, reserve: MemoryReserve) -> u64 {
  pool.default_reserve_percent().map_or(available, |default| {
    reserve.admissible_bytes(available, default)
  })
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
  fn auto_preserves_platform_defaults_and_overrides_apply_only_to_uma() {
    let free = 40 * 1024 * 1024 * 1024;
    assert_eq!(
      admissible_bytes(free, PoolKind::AppleUnified, MemoryReserve::AUTO),
      30 * 1024 * 1024 * 1024
    );
    for pool in [
      PoolKind::IntegratedUma,
      PoolKind::DiscreteVram,
      PoolKind::SystemRam,
    ] {
      assert_eq!(admissible_bytes(free, pool, MemoryReserve::AUTO), free);
    }
    for percent in [0, 10, 25, 99] {
      let reserve = MemoryReserve::percent(percent).unwrap();
      for pool in [PoolKind::AppleUnified, PoolKind::IntegratedUma] {
        assert_eq!(
          admissible_bytes(free, pool, reserve),
          reserve.admissible_bytes(free, 0)
        );
      }
      for pool in [PoolKind::DiscreteVram, PoolKind::SystemRam] {
        assert_eq!(admissible_bytes(free, pool, reserve), free);
      }
    }
  }

  #[test]
  fn reserve_scalar_roundtrips_strictly_in_json_and_yaml() {
    for raw in ["\"auto\"", "0", "10", "99"] {
      let reserve: MemoryReserve = serde_json::from_str(raw).unwrap();
      assert_eq!(serde_json::to_string(&reserve).unwrap(), raw);
      assert_eq!(
        yaml_serde::from_str::<MemoryReserve>(&yaml_serde::to_string(&reserve).unwrap()).unwrap(),
        reserve
      );
      assert_eq!(
        reserve.to_string().parse::<MemoryReserve>().unwrap(),
        reserve
      );
    }
    for raw in [
      "-1", "100", "256", "10.5", "10.0", "true", "null", "\"10\"", "\"AUTO\"", "[]", "{}",
    ] {
      assert!(serde_json::from_str::<MemoryReserve>(raw).is_err(), "{raw}");
      assert!(yaml_serde::from_str::<MemoryReserve>(raw).is_err(), "{raw}");
    }
    assert_eq!(
      yaml_serde::from_str::<MemoryReserve>("auto").unwrap(),
      MemoryReserve::AUTO
    );
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
