//! Post-launch actuals: what `--fit` actually chose, read once from the
//! child after it reaches Ready.
//!
//! This is the **neutral result type**. Placement is delegated to the
//! backend (`--fit` for llama.cpp), so llamastash does not know the resolved
//! context window (or layer split) until the child is up. The backend fetches
//! it once on the Loading→Ready transition (see
//! [`crate::backend::Backend::fetch_actuals`]) and it surfaces on `status`
//! (and thus the TUI Running view + `show`). Best-effort: a backend that can't
//! report it, or any transport error, yields `None` and the surfaces render
//! "unavailable" rather than a wrong number.

/// What the child reports it actually loaded with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Actuals {
  /// Resolved **per-request** context window the child loaded with —
  /// the window a single sequence/conversation can actually use, what
  /// `--fit` (or a pin) settled on. `None` when the backend didn't expose
  /// it. This is the one placement value llama-server's HTTP API
  /// reports; the rest (layers, threads, batch) live only in load-time
  /// logs, so the TUI shows those as `auto`.
  ///
  /// Read straight from `/props` `default_generation_settings.n_ctx` —
  /// **not** multiplied by `total_slots`. That field already is the
  /// per-request window in both slot modes:
  /// - **non-unified** (explicit `--parallel N`): `n_ctx` is per-slot
  ///   (`total / N`), which is exactly what one request gets.
  /// - **unified** (`--parallel` auto → `kv_unified = true`, the
  ///   default): `n_ctx` is the full shared window, and a request can
  ///   use all of it.
  ///
  /// Multiplying by `total_slots` to recover the `-c` aggregate
  /// double-counts under the default kv-unified mode (`-c 8192` auto
  /// gives `n_ctx=8192, total_slots=4`, which is `8192`, not `32768`)
  /// and can report a window larger than the model's trained context.
  /// The per-request value is both correct and what users mean by
  /// "context window".
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub resolved_ctx: Option<u32>,

  /// True when `--fit` had to clamp the context window down to the
  /// `--fit-ctx` floor even though the model's trained window is larger
  /// — i.e. memory pressure, not the model's own limit. Computed by the
  /// supervisor's readiness gate (it needs the floor + the trained
  /// window, neither of which `/props` reports), not parsed from
  /// `/props`. Drives the strict-fit refusal and the soft "ctx clamped"
  /// notice on the running surfaces.
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub ctx_clamped: bool,
}

impl Actuals {
  /// True when nothing was captured — surfaces render "unavailable".
  /// Keyed on `resolved_ctx` alone: `ctx_clamped` is a derived flag that
  /// is only meaningful once a context window was resolved.
  pub fn is_empty(&self) -> bool {
    self.resolved_ctx.is_none()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn actuals_is_empty_when_unset() {
    assert!(Actuals::default().is_empty());
    assert!(!Actuals {
      resolved_ctx: Some(4096),
      ctx_clamped: false,
    }
    .is_empty());
  }
}
