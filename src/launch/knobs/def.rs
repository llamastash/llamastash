//! Knob *descriptors* — what a backend declares about one of its tunables.
//!
//! A [`KnobDef`] is the single source every surface reads: the CLI generates a
//! flag from it, the TUI generates a row, and the preset/persistence layer
//! generates a key. Nothing is hand-wired per surface, so a knob cannot exist
//! on one surface and be missing from another.

/// A knob's stable identity — its persistence key, wire key, and (by default)
/// its flag spelling minus the leading dashes.
///
/// Borrowed from the backend's `&'static` declaration rather than owned, so
/// the id is `Copy` and can key a `PickerField` without allocating. Only ids
/// the registry knows can exist: parsing a config or wire key resolves it
/// through [`super::registry::resolve_id`], which is what turns an unknown key
/// into a warning instead of a silently-stored orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KnobId(pub &'static str);

impl KnobId {
  pub fn as_str(self) -> &'static str {
    self.0
  }
}

impl std::fmt::Display for KnobId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.0)
  }
}

/// A concept two backends genuinely share, so a value can follow the user
/// across a backend switch and so one neutral CLI alias can reach whichever
/// backend is serving.
///
/// Deliberately a **small closed set**. A knob with no concept is honestly
/// backend-local — that is the common case and carries no penalty. Adding a
/// variant is a claim that two engines mean the same thing by it, which the
/// registry test enforces is at most one knob per backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Concept {
  /// Context window. `--ctx-size` / `--ctx` / `--max-model-len` / `ctx_size`.
  ContextLength,
  /// CPU threads for host-side work.
  Threads,
  /// Which accelerator(s) to target.
  Device,
  /// K-cache element type.
  KvCacheKType,
  /// V-cache element type.
  KvCacheVType,
  /// Ceiling on concurrently-served sequences.
  MaxConcurrency,
  /// Flash-attention toggle.
  FlashAttn,
  /// Serving mode (chat / embedding / rerank).
  Mode,
}

impl Concept {
  /// The neutral CLI spelling for this concept (no leading dashes). Offered
  /// alongside each backend's own flag so a script can pin one name that works
  /// whichever backend serves the model.
  pub fn neutral_flag(self) -> &'static str {
    match self {
      Concept::ContextLength => "ctx",
      Concept::Threads => "threads",
      Concept::Device => "device",
      Concept::KvCacheKType => "cache-type-k",
      Concept::KvCacheVType => "cache-type-v",
      Concept::MaxConcurrency => "parallel",
      Concept::FlashAttn => "flash-attn",
      Concept::Mode => "mode",
    }
  }
}

/// What a knob's value *is*, which drives parsing, validation, and the editing
/// affordance each surface offers (typed input vs a cycle ring vs a toggle).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KnobKind {
  /// Unsigned integer, optionally capped.
  U32 { max: Option<u32> },
  /// Float, optionally bounded (inclusive).
  F32 { min: Option<f32>, max: Option<f32> },
  /// On/off. Surfaces cycle it rather than opening a text input.
  Bool,
  /// A closed set of string values. Surfaces cycle the ring; the CLI spells
  /// the choices into `--help`.
  Enum { choices: &'static [&'static str] },
  /// A closed set the value *usually* comes from, but bare strings outside it
  /// are still accepted — custom engine builds add types we can't enumerate.
  /// Cycles the ring like [`Self::Enum`] but never rejects.
  OpenEnum { choices: &'static [&'static str] },
  /// Free-form text (device selectors, split ratios, paths).
  Str,
}

impl KnobKind {
  /// Whether a surface should open a text input for this knob. Bool and the
  /// closed rings are cycled instead, so offering an editor on them would be a
  /// dead affordance.
  pub fn is_editable(self) -> bool {
    !matches!(self, KnobKind::Bool | KnobKind::Enum { .. })
  }

  /// The ring a surface cycles for this knob; empty when it has none.
  pub fn choices(self) -> &'static [&'static str] {
    match self {
      KnobKind::Enum { choices } | KnobKind::OpenEnum { choices } => choices,
      _ => &[],
    }
  }

  /// Placeholder shown after the flag in `--help`.
  pub fn cli_value_name(self) -> &'static str {
    match self {
      KnobKind::U32 { .. } => "N",
      KnobKind::F32 { .. } => "X",
      KnobKind::Bool => "BOOL",
      KnobKind::Enum { .. } | KnobKind::OpenEnum { .. } => "VALUE",
      KnobKind::Str => "VALUE",
    }
  }
}

/// What a knob's `auto` state *means* for this knob.
///
/// This is the fix for the overload that forced MTP off the knob path
/// (plan `2026-07-14-001` KD2): `Auto` used to mean exactly one thing —
/// "delegate to llama-server's `--fit`" — which is a llama.cpp-ism baked into a
/// supposedly neutral type, and nonsense for a knob like speculation enable.
/// Making the meaning per-knob lets both live on the same path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoKind {
  /// Emit nothing and let the engine's own fitter place this. Only the
  /// placement/sizing knobs a fitter actually adjusts should declare this —
  /// elsewhere `Auto` would emit nothing and be indistinguishable from unset.
  Delegate,
  /// Resolved at launch from a runtime property of the model (e.g. "on when
  /// the model carries a draft head"), not from any config layer.
  Capability,
}

/// How a set value becomes the backend's launch input.
///
/// Not all backends spawn a process: a backend that loads over HTTP declares
/// [`Self::Custom`] and reads the value itself in `prepare_launch`. `flag` is
/// therefore "the backend's own name for this setting", not always argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
  /// `--flag value`.
  FlagValue,
  /// `--flag` when true, nothing when false. No `--no-flag` form.
  BareFlagWhenTrue,
  /// The backend consumes the value itself (non-argv transports, or a value
  /// that expands into a bundle of flags).
  Custom,
}

/// Where a knob sits in the editor, and the `--help` heading it groups under.
/// Ordered by how often a typical user touches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
  Context,
  Offload,
  MultiGpu,
  Attention,
  Throughput,
  Memory,
  Speculation,
  Advanced,
}

impl Group {
  pub fn title(self) -> &'static str {
    match self {
      Group::Context => "Context",
      Group::Offload => "GPU / CPU offload",
      Group::MultiGpu => "Multi-GPU placement",
      Group::Attention => "Attention & KV cache",
      Group::Throughput => "Throughput",
      Group::Memory => "Memory loading",
      Group::Speculation => "Speculative decoding",
      Group::Advanced => "Advanced",
    }
  }

  /// Rows only worth showing on a host with more than one selectable device.
  pub fn multi_device_only(self) -> bool {
    matches!(self, Group::MultiGpu)
  }

  /// Render / navigation order.
  pub fn all() -> &'static [Group] {
    &[
      Group::Context,
      Group::Offload,
      Group::MultiGpu,
      Group::Attention,
      Group::Throughput,
      Group::Memory,
      Group::Speculation,
      Group::Advanced,
    ]
  }
}

/// One tunable, as declared by the backend that owns it.
///
/// Declared in `src/backend/<id>/knobs.rs` and nowhere else — that file is the
/// only place a backend's flag spellings appear.
#[derive(Debug, Clone, Copy)]
pub struct KnobDef {
  /// Stable persistence / wire / config key. By convention the flag spelling
  /// without leading dashes, so one string serves the YAML key, the CLI flag,
  /// and the engine's own `--help`.
  pub id: &'static str,
  /// The backend's own spelling. `None` derives `--{id}`, which is the case
  /// for almost every knob; spell it out only when it must differ from the id
  /// (two knobs competing for one flag, or a non-argv transport).
  pub flag: Option<&'static str>,
  /// Cross-backend concept, when this is genuinely a shared idea.
  pub concept: Option<Concept>,
  pub kind: KnobKind,
  /// Whether this knob has an `Auto` state, and what it means. `None` = the
  /// knob is either set or inherited.
  pub auto: Option<AutoKind>,
  pub group: Group,
  /// Row label in the editor.
  pub label: &'static str,
  /// One-line description: the CLI `--help` text and the TUI row description.
  /// Terse and imperative — it is the only prose a user sees for this knob.
  pub help: &'static str,
  /// Extra accepted spellings (`-ngl`, `-c`). Recognised on input; never emitted.
  pub aliases: &'static [&'static str],
  pub emit: Emit,
}

impl KnobDef {
  pub fn knob_id(&self) -> KnobId {
    KnobId(self.id)
  }

  /// The flag this knob emits — the declared override, else `--{id}`.
  ///
  /// Returns owned because the derived form has no `'static` storage. Emission
  /// happens once per launch, so the allocation is not worth designing around.
  pub fn emit_flag(&self) -> String {
    match self.flag {
      Some(f) => f.to_string(),
      None => format!("--{}", self.id),
    }
  }

  /// Whether this knob offers an `Auto` state on any surface.
  pub fn has_auto(&self) -> bool {
    self.auto.is_some()
  }

  /// Whether `Auto` means "let the engine's fitter decide" — the only knobs
  /// the layer-less seeding rule may seed to `Auto`.
  pub fn is_fit_delegated(&self) -> bool {
    matches!(self.auto, Some(AutoKind::Delegate))
  }
}
