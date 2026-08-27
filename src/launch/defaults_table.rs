//! Built-in `(architecture, gpu_backend) → crate::launch::knobs::KnobSet` table.
//!
//! Authoritative opinion on launch flags for every (arch, backend)
//! the recommender can pick. Lives in code so a fresh install on
//! any supported backend gets sensible defaults without ever touching
//! YAML; the wizard no longer seeds `arch_defaults`. The YAML escape
//! hatch stays for hand-edited overrides.
//!
//! Maintenance note (lifted into `AGENTS.md`): when
//! `data/benchmark-snapshot.json` adds a new recommender pick, audit
//! the table coverage. Anything not explicitly listed falls through
//! to the `*` row.

use crate::launch::knobs::KnobSet;
use crate::daemon::host_metrics::GpuFlavor;

/// Look up the built-in defaults row for `(arch, backend)`. The
/// architecture string is lower-cased; unknown architectures fall
/// back to the `*` row. The result already has `None` for fields the
/// row doesn't opinionate — the layered resolver fills the rest from
/// upstream layers (YAML, last_used, preset) or leaves them
/// unset (llama-server default).
///
/// `backend` carries the typed `GpuFlavor` view of
/// `HostMetricsSnapshot::gpu_backend`. `Unsampled` is treated
/// identically to `Unknown` — the brief window after daemon start
/// before the first sampler tick gets the conservative path, not the
/// GPU path.
pub fn lookup(arch: &str, backend: GpuFlavor) -> KnobSet {
  let arch = arch.to_ascii_lowercase();
  let explicit = lookup_explicit(arch.as_str(), backend);
  let fallback = lookup_wildcard(backend);
  merge(explicit, fallback)
}

fn lookup_wildcard(_backend: GpuFlavor) -> KnobSet {
  // The `*` row no longer pins `n_gpu_layers` — offload placement is
  // delegated to `--fit` (Auto). A layer-less `n_gpu_layers` is seeded
  // `Auto` by the resolver, which emits no `-ngl` and lets fit decide.
  // flash_attn opt-in stays per-arch (see `lookup_explicit`).
  KnobSet::new()
}

/// Architecture-specific row. `None` means "no explicit row — caller
/// falls through to the wildcard".
fn lookup_explicit(arch: &str, backend: GpuFlavor) -> Option<KnobSet> {
  // Architectures we explicitly cover. Anything else falls through to
  // the `*` row.
  if !COVERED_ARCHS.contains(&arch) {
    return None;
  }
  let mut k = KnobSet::new();
  // flash-attn: only the flash-attn-eligible architectures on
  // nvidia / apple_metal. AMD/HIP coverage is uneven — leave to user
  // override. Vulkan/unknown can't enumerate VRAM safely; CPU
  // obviously doesn't apply. `n_gpu_layers` is *not* pinned anymore —
  // fit owns offload placement (Auto).
  if FLASH_ATTN_ELIGIBLE.contains(&arch)
    && matches!(backend, GpuFlavor::Nvidia | GpuFlavor::AppleMetal)
  {
    k.set_by_name("flash-attn", "true");
  }
  Some(k)
}

/// Layer `over` onto `under`, taking each `Some` from `over` first.
/// Shares the one layering primitive with [`KnobSet::overlay`].
fn merge(over: Option<KnobSet>, under: KnobSet) -> KnobSet {
  let Some(over) = over else { return under };
  let mut out = under;
  out.overlay(&over);
  out
}

/// Architectures the table explicitly covers. Cross-referenced
/// against `data/benchmark-snapshot.json` so every recommender pick
/// hits an explicit row or the `*` fallback.
const COVERED_ARCHS: &[&str] = &[
  "llama",
  "llama2",
  "llama3",
  "llama4",
  "qwen2",
  "qwen2_moe",
  "qwen3",
  "qwen3_moe",
  "qwen3moe",
  "qwen3next",
  "mistral",
  "mixtral",
  "gemma",
  "gemma2",
  "gemma3",
  "phi",
  "phi3",
  "deepseek",
  "deepseek2",
  "deepseek3",
  "granite",
  "falcon",
  "stablelm",
  "command-r",
];

/// Architectures the table opts into `flash_attn: Some(true)` on
/// flash-attn-eligible backends (nvidia / apple_metal).
const FLASH_ATTN_ELIGIBLE: &[&str] = &[
  "qwen2",
  "qwen2_moe",
  "qwen3",
  "qwen3_moe",
  "qwen3moe",
  "qwen3next",
  "llama2",
  "llama3",
  "llama4",
];

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ngl_is_never_pinned_on_any_backend() {
    // Offload placement is delegated to `--fit` (Auto); the table no
    // longer pins n_gpu_layers on any (arch, backend).
    for backend in [
      GpuFlavor::Nvidia,
      GpuFlavor::Amd,
      GpuFlavor::AppleMetal,
      GpuFlavor::Multi,
      GpuFlavor::CpuOnly,
      GpuFlavor::Unknown,
      GpuFlavor::Unsampled,
    ] {
      assert_eq!(
        lookup("qwen2", backend).u32(crate::launch::knobs::kid("n-gpu-layers")),
        None,
        "{backend:?} must not pin ngl"
      );
      assert_eq!(
        lookup("entirely-unknown-arch", backend).u32(crate::launch::knobs::kid("n-gpu-layers")),
        None,
        "wildcard {backend:?} must not pin ngl"
      );
    }
  }


  #[test]
  fn qwen2_on_cpu_only_sets_nothing() {
    let k = lookup("qwen2", GpuFlavor::CpuOnly);
    assert_eq!(k.u32(crate::launch::knobs::kid("n-gpu-layers")), None);
    assert_eq!(k.bool(crate::launch::knobs::kid("flash-attn")), None);
  }

  #[test]
  fn unknown_arch_on_nvidia_opts_into_nothing() {
    let k = lookup("entirely-unknown-arch", GpuFlavor::Nvidia);
    assert_eq!(k.bool(crate::launch::knobs::kid("flash-attn")), None, "wildcard does not opt into flash_attn");
  }

  #[test]
  fn qwen2_on_amd_no_flash_attn() {
    let k = lookup("qwen2", GpuFlavor::Amd);
    assert_eq!(
      k.bool(crate::launch::knobs::kid("flash-attn")), None,
      "HIP flash-attn coverage is uneven — leave it to user override"
    );
  }

  #[test]
  fn gemma_on_nvidia_no_flash_attn() {
    let k = lookup("gemma", GpuFlavor::Nvidia);
    assert_eq!(
      k.bool(crate::launch::knobs::kid("flash-attn")), None,
      "gemma not on the flash-attn opt-in list at v1"
    );
  }


}
