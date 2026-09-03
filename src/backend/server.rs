//! Neutral **server** + **device** model shared across every backend.
//!
//! Terminology (see the server-abstraction plan):
//!
//! - a **backend** is an inference *engine* (`llamacpp` / `ds4` / `lemonade`);
//! - a **server** is one *build/binary* of a backend (llama.cpp's ROCm build,
//!   its Vulkan build, `ds4-server`, `lemond`). A backend has 1..N servers;
//! - a **device** is a GPU a server can target, identified by the exact
//!   `--device` selector that server's own probe reports. The compute backend
//!   (`ROCm` / `Vulkan` / `CUDA` / `Metal`) is a *property of the device*,
//!   surfaced in the server name — not an inference backend.
//!
//! [`build_server_catalog`] walks [`crate::backend::Backends::all`], asks each
//! backend for its [`ServerSpec`]s ([`Backend::configured_servers`]), probes
//! each spec's devices ([`Backend::probe_devices`]), and derives a stable
//! `id` / display `name` per server. **No dedup across servers** — every
//! configured binary is its own selectable server (devices *within* a server
//! still dedup by selector, which is the probe's job).
//!
//! A device is a *launch option*, not a card: a build compiled with two compute
//! APIs reports the same GPU once per API (`ROCm0` and `Vulkan0` for one
//! Radeon). Both selectors are real choices and both survive the probe, so
//! anything asking "how many GPUs?" must go through
//! [`physical_device_count`] rather than counting selectors.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Backend, Backends};
use crate::daemon::context::MethodContext;

/// One launch device a server can target, from that server's own probe.
///
/// Carries no owning-binary field — the [`Server`] that owns this device
/// already knows its binary.
///
/// Every field defaults rather than rejects, so a client reading a `status`
/// payload from an older daemon still gets a device it can count.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
  /// Exact `--device` selector (`Vulkan0`, `CUDA0`, `ROCm0`), passed verbatim.
  #[serde(default, deserialize_with = "lenient")]
  pub selector: String,
  /// Compute backend inferred from the selector prefix (`Vulkan` / `CUDA` /
  /// `ROCm` / `Metal`). Display-only.
  #[serde(default, deserialize_with = "lenient")]
  pub gpu_backend: String,
  /// Human-readable adapter name, parens and all.
  #[serde(default, deserialize_with = "lenient")]
  pub name: String,
  /// Total device memory in MiB, when the probe carried it.
  #[serde(default, deserialize_with = "lenient")]
  pub total_mib: Option<u64>,
  /// Free device memory in MiB, when the probe carried it.
  #[serde(default, deserialize_with = "lenient")]
  pub free_mib: Option<u64>,
}

/// Decode one field, falling back to its default when the wire value has the
/// wrong *type* (a missing key is already `serde(default)`'s job). The value is
/// read as JSON first so exactly one value is consumed either way and the rest
/// of the row keeps decoding.
///
/// Per-field rather than per-row because rejecting a [`Device`] takes `name`
/// with it, and a nameless device has no identity to dedup on — it claims its
/// own slot in [`physical_device_count`], so one mistyped sibling field would
/// be enough to put the multi-GPU UI back on a single-GPU host.
fn lenient<'de, D, T>(de: D) -> Result<T, D::Error>
where
  D: serde::Deserializer<'de>,
  T: serde::de::DeserializeOwned + Default,
{
  let raw = serde_json::Value::deserialize(de)?;
  Ok(serde_json::from_value(raw).unwrap_or_default())
}

/// A configured server binary **before** device probing — what
/// [`Backend::configured_servers`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
  /// Resolved absolute path to the server binary.
  pub binary: PathBuf,
  /// Explicit per-server display name from `servers: [{name}]`, if any.
  pub name: Option<String>,
}

/// A resolved server: one build/binary of a backend, with its probed devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
  /// Stable selection / persistence key (`llamacpp-rocm`, `ds4`). Also the
  /// display label — derived once by [`build_server_catalog`].
  pub id: String,
  /// The backend that owns this server (`llamacpp` / `ds4` / `lemonade`).
  pub backend_id: String,
  /// Absolute path to the server binary the supervisor spawns.
  pub binary: PathBuf,
  /// Human-readable display name (same string as [`Self::id`] today).
  pub name: String,
  /// Devices this server can target (`--device` selectors). Empty for a
  /// backend with no device probe (ds4 / lemonade) or a CPU-only build.
  pub devices: Vec<Device>,
}

impl Server {
  /// How many **physical GPUs** this server can see, as opposed to how many
  /// selectors it offers. See [`physical_device_count`].
  pub fn physical_device_count(&self) -> usize {
    physical_device_count(&self.devices)
  }
}

/// The server a launch spawns on when it pins none: the entry whose binary is
/// the daemon's default (`daemon.server_path`), else the first in catalog
/// order.
///
/// This is the client-side mirror of the daemon's own fallback — with no
/// `--server` pick and no device selector, `launch_service::pick_launch_binary`
/// spawns the daemon's default binary, which is not always the catalog-first
/// build. Callers pass **one backend's** servers: a backend that ships its own
/// binary overrides that fallback in [`Backend::resolve_launch_binary`], so its
/// group's catalog-first entry is its default.
///
/// Everything that says "default" about a server goes through here — the launch
/// picker's ring, the running view's `server` row, and the knob gates beside
/// it — so one panel cannot name one build while another answers for a second.
pub fn default_server<'a, I>(mut servers: I, default_binary: Option<&Path>) -> Option<&'a Server>
where
  I: Iterator<Item = &'a Server> + Clone,
{
  let mut catalog_first = servers.clone();
  servers
    .find(|s| default_binary.is_some_and(|d| s.binary.as_path() == d))
    .or_else(|| catalog_first.next())
}

/// Adapter name reduced to a physical-identity key: lower-cased, every
/// parenthetical dropped, whitespace collapsed. The same card is named
/// `AMD Radeon 8060S Graphics` by ROCm and
/// `AMD Radeon 8060S Graphics (RADV STRIX_HALO)` by Vulkan — the driver tag is
/// the only difference, and `Intel(R) Arc(TM) A770` reduces the same way.
fn physical_key(name: &str) -> String {
  let mut out = String::with_capacity(name.len());
  let mut depth = 0usize;
  for c in name.chars() {
    match c {
      '(' => depth += 1,
      ')' => depth = depth.saturating_sub(1),
      _ if depth > 0 => {}
      c if c.is_whitespace() => {
        if !out.ends_with(' ') {
          out.push(' ');
        }
      }
      c => out.extend(c.to_lowercase()),
    }
  }
  out.trim().to_string()
}

/// How many physical GPUs a device list represents.
///
/// Within one compute family every index is its own card (two identical
/// `Vulkan0` / `Vulkan1` 3080s are two GPUs). Across families, equal adapter
/// names are the same card seen twice — one card cannot appear twice under the
/// same API, so each selector claims the first slot with a matching name that
/// has no device from its family yet. `CUDA0,CUDA1,Vulkan0,Vulkan1` on two
/// identical cards therefore pairs up into two slots, not one or four.
///
/// Memory size is deliberately not part of the identity: the same card reports
/// 126976 MiB under ROCm and 128505 MiB under Vulkan, so requiring a match
/// would defeat the dedup. A name that doesn't match counts as another card —
/// the failure direction that keeps a real second GPU visible.
pub fn physical_device_count(devices: &[Device]) -> usize {
  // (identity key, families already occupying this slot). Linear scans over a
  // handful of devices; a map would cost more than it saves.
  let mut slots: Vec<(String, Vec<String>)> = Vec::new();
  for d in devices {
    let key = physical_key(&d.name);
    let family = d.gpu_backend.to_ascii_lowercase();
    // An empty name carries no identity to match on, so it always takes a new
    // slot rather than colliding with every other unnamed device.
    let slot = if key.is_empty() {
      None
    } else {
      slots
        .iter_mut()
        .find(|(k, fams)| *k == key && !fams.contains(&family))
    };
    match slot {
      Some((_, fams)) => fams.push(family),
      None => slots.push((key, vec![family])),
    }
  }
  slots.len()
}

/// A per-backend `servers: [{binary, name?}]` config entry.
///
/// Replaces llama.cpp's `binary` + `additional_binaries` and ds4/lemonade's
/// single `binary`. Entries are objects (not bare strings) so future per-server
/// keys — env, extra flags — extend the schema without breaking it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ServerConfig {
  /// Path to this server's binary.
  pub binary: PathBuf,
  /// Optional display-name override (`<backend>-<name>`); auto-derived when
  /// unset.
  #[serde(default)]
  pub name: Option<String>,
}

/// The compute-backend tag for a server, lower-cased for ids/display: the first
/// probed device's `gpu_backend` (`rocm` / `vulkan` / `cuda` / `metal`). Only
/// called with a non-empty device list — a device-less server has no detectable
/// compute type and derives the bare backend id instead (see [`derive_servers`]).
fn server_gpu_tag(devices: &[Device]) -> Option<String> {
  devices.first().map(|d| d.gpu_backend.to_ascii_lowercase())
}

/// Basename of the binary's parent directory (`…/build-hip/bin/llama-server` →
/// `build-hip`), the third-tier disambiguator when the gpu-backend tag collides.
fn binary_dir_tag(binary: &Path) -> String {
  binary
    .parent()
    .and_then(|p| {
      // `…/build-hip/bin` → prefer the grandparent `build-hip` over `bin`.
      if p.file_name().and_then(|n| n.to_str()) == Some("bin") {
        p.parent()
      } else {
        Some(p)
      }
    })
    .and_then(|p| p.file_name())
    .and_then(|n| n.to_str())
    .map(|s| s.to_string())
    .unwrap_or_else(|| "server".to_string())
}

/// Derive a stable `id` / `name` for one backend's servers. The suffix after
/// `<backend>-` comes from, in order: an explicit `name:` → the unique
/// `<gpu_backend>` a device probe reveals (`rocm` / `vulkan` / `cuda` /
/// `metal`) → the `<binary-dir>` basename when that gpu tag collides (two ROCm
/// builds). A **device-less** server (ds4 / lemonade / a CPU-only build — no
/// detectable compute type) gets the **bare backend id** (`ds4`, `lemonade`),
/// disambiguated `-N` only when several collide; an explicit `name:` (`rocm`,
/// `cuda`) is the way to label those (`ds4-rocm`). Input pairs are
/// `(spec, probed devices)`.
fn derive_servers(backend_id: &str, probed: Vec<(ServerSpec, Vec<Device>)>) -> Vec<Server> {
  // Provisional gpu tag per device-bearing, name-less server — used only to
  // detect gpu-tag collisions (two ROCm builds). `None` for named or
  // device-less servers, which don't participate in gpu-tag collision counting.
  let gpu_tags: Vec<Option<String>> = probed
    .iter()
    .map(|(spec, devices)| {
      if spec.name.is_none() {
        server_gpu_tag(devices)
      } else {
        None
      }
    })
    .collect();

  let mut tag_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
  for tag in gpu_tags.iter().flatten() {
    *tag_counts.entry(tag.as_str()).or_insert(0) += 1;
  }

  let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
  let mut out = Vec::with_capacity(probed.len());
  for (i, (spec, devices)) in probed.into_iter().enumerate() {
    // The id suffix after `<backend>-`; `None` means the bare backend id (a
    // device-less server whose compute type we can't name).
    let base: Option<String> = match &spec.name {
      Some(name) => Some(name.clone()),
      None => match &gpu_tags[i] {
        Some(tag) if tag_counts.get(tag.as_str()) == Some(&1) => Some(tag.clone()),
        Some(_) => Some(binary_dir_tag(&spec.binary)),
        None => None,
      },
    };
    let mut candidate = match &base {
      Some(b) => format!("{backend_id}-{b}"),
      None => backend_id.to_string(),
    };
    // Final collision guard: append `-N` until unique within this backend.
    let mut n = 2;
    while used.contains(&candidate) {
      candidate = match &base {
        Some(b) => format!("{backend_id}-{b}-{n}"),
        None => format!("{backend_id}-{n}"),
      };
      n += 1;
    }
    used.insert(candidate.clone());
    out.push(Server {
      id: candidate.clone(),
      backend_id: backend_id.to_string(),
      binary: spec.binary,
      name: candidate,
      devices,
    });
  }
  out
}

/// Build the neutral server catalog: every backend's configured servers, each
/// probed for devices, with derived ids. Runs in a blocking context (each
/// `probe_devices` may shell out to `--list-devices`). Names no backend — the
/// registry loop is the whole wiring.
pub fn build_server_catalog(ctx: &MethodContext) -> Vec<Server> {
  let mut out = Vec::new();
  for backend in Backends::all() {
    let backend_id = backend.id().to_string();
    let probed: Vec<(ServerSpec, Vec<Device>)> = backend
      .configured_servers(ctx)
      .into_iter()
      .map(|spec| {
        let devices = backend.probe_devices(&spec.binary);
        #[cfg(debug_assertions)]
        let devices = debug_fake_multi_gpu(devices);
        (spec, devices)
      })
      .collect();
    out.extend(derive_servers(&backend_id, probed));
  }
  out
}

/// Config-only server catalog for read-only diagnostics (`doctor`), which runs
/// without a daemon [`MethodContext`]. Enumerates each backend's configured
/// `servers:` from [`crate::config::Config`] via [`Backend::config_servers`],
/// skips any whose
/// binary doesn't resolve to a file, probes the rest for devices, and derives
/// ids exactly like [`build_server_catalog`]. Names no backend — the registry
/// loop is the whole wiring.
pub fn config_server_catalog(config: &crate::config::Config) -> Vec<Server> {
  let mut out = Vec::new();
  for backend in Backends::all() {
    let backend_id = backend.id().to_string();
    let probed: Vec<(ServerSpec, Vec<Device>)> = backend
      .config_servers(config)
      .into_iter()
      .filter_map(|sc| {
        let resolved =
          crate::util::paths::canonicalize(&sc.binary).unwrap_or_else(|_| sc.binary.clone());
        if !resolved.is_file() {
          return None;
        }
        let devices = backend.probe_devices(&resolved);
        Some((
          ServerSpec {
            binary: resolved,
            name: sc.name,
          },
          devices,
        ))
      })
      .collect();
    out.extend(derive_servers(&backend_id, probed));
  }
  out
}

/// Configured server binaries whose path does **not** resolve to a file, paired
/// with the owning backend id — the `doctor` server advisory's warning input.
/// Reads config alone (no probe).
pub fn missing_configured_servers(config: &crate::config::Config) -> Vec<(String, PathBuf)> {
  let mut out = Vec::new();
  for backend in Backends::all() {
    for sc in backend.config_servers(config) {
      let resolved =
        crate::util::paths::canonicalize(&sc.binary).unwrap_or_else(|_| sc.binary.clone());
      if !resolved.is_file() {
        out.push((backend.id().to_string(), sc.binary));
      }
    }
  }
  out
}

/// Debug-only multi-GPU simulator for the **launch device catalog**, compiled
/// out of release builds (`#[cfg(debug_assertions)]`). When
/// `LLAMASTASH_DEBUG_FAKE_GPUS=N` (N >= 2) is set on the **daemon** process of a
/// debug build, each device-bearing server's probe is fanned out into N
/// synthetic devices (stepped selector + name) so the picker's multi-device row,
/// the server-scoped gating, and `main_gpu`/`tensor_split` can be exercised on a
/// single-GPU host. Sibling of the host-metrics simulator
/// (`host_metrics::debug_fake_multi_gpu`), which independently fans out the
/// display GPUs — this one is the launch-selector list. A device-less server
/// (ds4 / lemonade / CPU-only build) has nothing to fan out and is left as-is.
#[cfg(debug_assertions)]
fn debug_fake_multi_gpu(devices: Vec<Device>) -> Vec<Device> {
  let n: usize = match std::env::var("LLAMASTASH_DEBUG_FAKE_GPUS")
    .ok()
    .and_then(|v| v.parse().ok())
  {
    Some(n) if n >= 2 => n,
    _ => return devices,
  };
  fan_out_devices(devices, n)
}

/// Fan the first probed device out into `n` synthetic clones (stepped selector +
/// name). Empty in, empty out; a device-less server has nothing to clone. Pure
/// so it is unit-testable without touching the env var.
#[cfg(debug_assertions)]
fn fan_out_devices(devices: Vec<Device>, n: usize) -> Vec<Device> {
  let Some(seed) = devices.into_iter().next() else {
    return Vec::new();
  };
  // The selector's alphabetic prefix (`ROCm0` -> `ROCm`) becomes the stem for
  // the synthetic selectors `ROCm0..ROCm(N-1)`, matching real llama.cpp naming.
  let prefix: String = seed
    .selector
    .chars()
    .take_while(|c| c.is_ascii_alphabetic())
    .collect();
  (0..n)
    .map(|i| Device {
      selector: format!("{prefix}{i}"),
      gpu_backend: seed.gpu_backend.clone(),
      name: format!("{} (fake #{i})", seed.name),
      total_mib: seed.total_mib,
      free_mib: seed.free_mib,
    })
    .collect()
}

#[cfg(test)]
mod tests {

  #[test]
  fn a_wrong_typed_numeric_field_does_not_blank_the_rest_of_the_device() {
    // Whole-row decode with `unwrap_or_default()` used to zero every field when
    // any one had the wrong type, and a nameless device still takes a physical
    // slot — so one malformed row beside one real GPU re-enabled the multi-GPU
    // UI on a single-GPU host.
    let v = serde_json::json!({
      "selector": "ROCm0",
      "gpu_backend": "ROCm",
      "name": "AMD Radeon 8060S Graphics",
      "total_mib": "24576",
      "free_mib": 123
    });
    let d: Device = serde_json::from_value(v).unwrap_or_default();
    assert_eq!(d.name, "AMD Radeon 8060S Graphics", "name must survive");
    assert_eq!(d.selector, "ROCm0");
    assert_eq!(d.total_mib, None, "the wrong-typed field alone falls back");
    assert_eq!(d.free_mib, Some(123), "well-typed siblings still decode");
  }
  use super::*;

  fn dev(selector: &str, gpu: &str) -> Device {
    Device {
      selector: selector.to_string(),
      gpu_backend: gpu.to_string(),
      name: format!("{gpu} card"),
      total_mib: Some(1000),
      free_mib: Some(900),
    }
  }

  fn spec(binary: &str, name: Option<&str>) -> ServerSpec {
    ServerSpec {
      binary: PathBuf::from(binary),
      name: name.map(String::from),
    }
  }

  /// A device with an explicit adapter name — the identity input.
  fn named(selector: &str, gpu: &str, name: &str) -> Device {
    Device {
      selector: selector.to_string(),
      gpu_backend: gpu.to_string(),
      name: name.to_string(),
      total_mib: None,
      free_mib: None,
    }
  }

  #[test]
  fn a_field_of_the_wrong_type_defaults_without_taking_the_row_with_it() {
    let d: Device = serde_json::from_value(serde_json::json!({
      "selector": 0,
      "gpu_backend": "ROCm",
      "name": "AMD Radeon 8060S Graphics",
      "total_mib": "126976",
      "free_mib": 61394
    }))
    .expect("a row with bad field types still decodes");
    assert_eq!(d.selector, "");
    assert_eq!(d.total_mib, None);
    // The fields that were readable are all still here — losing `name` would
    // cost the row its identity in `physical_device_count`.
    assert_eq!(d.gpu_backend, "ROCm");
    assert_eq!(d.name, "AMD Radeon 8060S Graphics");
    assert_eq!(d.free_mib, Some(61394));
    // A payload from an older daemon that never wrote the keys reads the same.
    let sparse: Device =
      serde_json::from_value(serde_json::json!({"selector": "ROCm0"})).expect("missing keys");
    assert_eq!(
      sparse,
      Device {
        selector: "ROCm0".into(),
        ..Default::default()
      }
    );
  }

  #[test]
  fn the_default_server_is_the_daemons_binary_not_the_catalog_head() {
    let srv = |id: &str, binary: &str| Server {
      id: id.to_string(),
      backend_id: "llamacpp".to_string(),
      binary: PathBuf::from(binary),
      name: id.to_string(),
      devices: Vec::new(),
    };
    let servers = [
      srv("llamacpp-rocm", "/b/rocm/llama-server"),
      srv("llamacpp-vulkan", "/b/vulkan/llama-server"),
    ];
    assert_eq!(
      default_server(servers.iter(), Some(Path::new("/b/vulkan/llama-server"))).map(|s| &s.id),
      Some(&"llamacpp-vulkan".to_string()),
      "the daemon's own binary wins over catalog order"
    );
    // No default configured, or one that has left the catalog (rebuilt binary)
    // → catalog order, which is what the daemon falls back to as well.
    assert_eq!(
      default_server(servers.iter(), None).map(|s| &s.id),
      Some(&"llamacpp-rocm".to_string())
    );
    assert_eq!(
      default_server(servers.iter(), Some(Path::new("/b/gone/llama-server"))).map(|s| &s.id),
      Some(&"llamacpp-rocm".to_string())
    );
    assert!(default_server([].iter(), None).is_none());
  }

  #[test]
  fn one_card_seen_by_two_compute_apis_counts_once() {
    // Verbatim from `q38rocm-llama-server --list-devices` on a Strix Halo
    // host: one iGPU, one binary, two compute backends. Note the memory
    // totals differ (126976 vs 128505 MiB) for the same card.
    let devices = vec![
      Device {
        total_mib: Some(126976),
        free_mib: Some(61394),
        ..named("ROCm0", "ROCm", "AMD Radeon 8060S Graphics")
      },
      Device {
        total_mib: Some(128505),
        free_mib: Some(89217),
        ..named(
          "Vulkan0",
          "Vulkan",
          "AMD Radeon 8060S Graphics (RADV STRIX_HALO)",
        )
      },
    ];
    assert_eq!(physical_device_count(&devices), 1);
  }

  #[test]
  fn two_identical_cards_in_one_family_stay_two() {
    let devices = vec![
      named("Vulkan0", "Vulkan", "NVIDIA GeForce RTX 3080"),
      named("Vulkan1", "Vulkan", "NVIDIA GeForce RTX 3080"),
    ];
    assert_eq!(
      physical_device_count(&devices),
      2,
      "same API cannot show one card twice"
    );
  }

  #[test]
  fn two_identical_cards_across_two_families_pair_up() {
    // 4 selectors, 2 cards: each Vulkan entry pairs with the CUDA slot that
    // has no Vulkan yet, so the count is neither 1 nor 4.
    let devices = vec![
      named("CUDA0", "CUDA", "NVIDIA GeForce RTX 3080"),
      named("CUDA1", "CUDA", "NVIDIA GeForce RTX 3080"),
      named("Vulkan0", "Vulkan", "NVIDIA GeForce RTX 3080"),
      named("Vulkan1", "Vulkan", "NVIDIA GeForce RTX 3080"),
    ];
    assert_eq!(physical_device_count(&devices), 2);
  }

  #[test]
  fn different_cards_across_families_stay_distinct() {
    let devices = vec![
      named("ROCm0", "ROCm", "AMD Radeon AI PRO R9700"),
      named("Vulkan0", "Vulkan", "NVIDIA GeForce RTX 3080"),
    ];
    assert_eq!(physical_device_count(&devices), 2);
  }

  #[test]
  fn unnamed_devices_never_collapse_into_one() {
    let devices = vec![named("ROCm0", "ROCm", ""), named("Vulkan0", "Vulkan", "")];
    assert_eq!(
      physical_device_count(&devices),
      2,
      "no name is no identity — fall back to counting selectors"
    );
    assert_eq!(physical_device_count(&[]), 0);
  }

  #[test]
  fn physical_key_strips_driver_parentheticals() {
    assert_eq!(
      physical_key("AMD Radeon 8060S Graphics (RADV STRIX_HALO)"),
      physical_key("AMD Radeon 8060S Graphics")
    );
    assert_eq!(physical_key("Intel(R)  Arc(TM) A770"), "intel arc a770");
    assert_ne!(
      physical_key("AMD Radeon 8060S"),
      physical_key("AMD Radeon 8050S")
    );
  }

  #[test]
  fn server_physical_count_matches_the_free_function() {
    let server = Server {
      id: "llamacpp-fp4".into(),
      backend_id: "llamacpp".into(),
      binary: PathBuf::from("/bin/llama-server"),
      name: "llamacpp-fp4".into(),
      devices: vec![
        named("ROCm0", "ROCm", "AMD Radeon 8060S Graphics"),
        named(
          "Vulkan0",
          "Vulkan",
          "AMD Radeon 8060S Graphics (RADV STRIX_HALO)",
        ),
      ],
    };
    assert_eq!(server.devices.len(), 2, "both selectors stay selectable");
    assert_eq!(server.physical_device_count(), 1);
  }

  #[test]
  fn unique_gpu_backend_tags_win() {
    let servers = derive_servers(
      "llamacpp",
      vec![
        (
          spec("/a/build-hip/bin/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
        (
          spec("/a/build-vulkan/bin/llama-server", None),
          vec![dev("Vulkan0", "Vulkan")],
        ),
      ],
    );
    assert_eq!(servers[0].id, "llamacpp-rocm");
    assert_eq!(servers[1].id, "llamacpp-vulkan");
  }

  #[test]
  fn colliding_gpu_backend_falls_back_to_dir_basename() {
    // Two ROCm builds both report ROCm0 → gpu tag collides → dir basename.
    let servers = derive_servers(
      "llamacpp",
      vec![
        (
          spec("/a/build-hip/bin/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
        (
          spec("/a/build-hip-rocwmma/bin/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
      ],
    );
    assert_eq!(servers[0].id, "llamacpp-build-hip");
    assert_eq!(servers[1].id, "llamacpp-build-hip-rocwmma");
    // No dedup: both survive even though both expose ROCm0.
    assert_eq!(servers.len(), 2);
  }

  #[test]
  fn explicit_name_overrides_derivation() {
    // The explicit `name:` must win over the auto-derived gpu tag. Use a name
    // that differs from the tag the device would produce (`rocm`) so this
    // actually proves the override — `name: "rocm"` would pass whether or not
    // the name were honored.
    let servers = derive_servers(
      "llamacpp",
      vec![(
        spec("/x/llama-server", Some("myrocm")),
        vec![dev("ROCm0", "ROCm")],
      )],
    );
    assert_eq!(servers[0].id, "llamacpp-myrocm");
    assert_eq!(servers[0].name, "llamacpp-myrocm");
  }

  #[test]
  fn single_deviceless_server_is_the_bare_backend_id() {
    // ds4 / lemonade / a CPU-only build: no probe, no detectable compute type
    // → the bare backend id, not a misleading `-cpu`.
    let servers = derive_servers("ds4", vec![(spec("/x/ds4-server", None), vec![])]);
    assert_eq!(servers[0].id, "ds4");
  }

  #[test]
  fn colliding_deviceless_servers_get_numeric_suffix() {
    // Two device-less builds with no names → bare id then `-N` (the compute
    // type isn't knowable; a `name:` override is how you label them).
    let servers = derive_servers(
      "ds4",
      vec![
        (spec("/a/one/ds4-server", None), vec![]),
        (spec("/a/two/ds4-server", None), vec![]),
      ],
    );
    assert_eq!(servers[0].id, "ds4");
    assert_eq!(servers[1].id, "ds4-2");
  }

  #[test]
  fn deviceless_server_honors_explicit_name() {
    // The compute type for a device-less backend comes from `name:` (`rocm`).
    let servers = derive_servers("ds4", vec![(spec("/x/ds4-server", Some("rocm")), vec![])]);
    assert_eq!(servers[0].id, "ds4-rocm");
  }

  #[test]
  fn fake_gpu_fan_out_steps_selectors_from_one_seed() {
    let out = fan_out_devices(vec![dev("ROCm0", "ROCm")], 3);
    assert_eq!(out.len(), 3);
    assert_eq!(
      out.iter().map(|d| d.selector.as_str()).collect::<Vec<_>>(),
      ["ROCm0", "ROCm1", "ROCm2"]
    );
    // Same compute backend, distinct names (so the picker dedup keeps them).
    assert!(out.iter().all(|d| d.gpu_backend == "ROCm"));
    assert_ne!(out[0].name, out[1].name);
  }

  #[test]
  fn fake_gpu_fan_out_of_a_deviceless_server_stays_empty() {
    assert!(fan_out_devices(vec![], 4).is_empty());
  }

  #[test]
  fn identical_dir_names_get_numeric_suffix() {
    // Pathological: same gpu tag AND same dir basename → `-N` guard.
    let servers = derive_servers(
      "llamacpp",
      vec![
        (
          spec("/a/build/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
        (
          spec("/b/build/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
      ],
    );
    assert_eq!(servers[0].id, "llamacpp-build");
    assert_eq!(servers[1].id, "llamacpp-build-2");
  }
}
