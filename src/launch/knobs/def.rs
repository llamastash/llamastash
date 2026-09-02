//! Knob *descriptors* — what a backend declares about one of its tunables.
//!
//! A [`KnobDef`] is the single source every surface reads: the CLI generates a
//! flag from it, the TUI generates a row, and the preset/persistence layer
//! generates a key. Nothing is hand-wired per surface, so a knob cannot exist
//! on one surface and be missing from another.

use crate::launch::params::LayerLabel;
use serde::{Deserialize, Serialize};

/// A knob's stable identity — its persistence key, wire key, and (by default)
/// its flag spelling minus the leading dashes.
///
/// Borrowed from the backend's `&'static` declaration rather than owned, so
/// the id is `Copy` and can key a `PickerField` without allocating. Only ids
/// the registry knows can exist: parsing a config or wire key resolves it
/// through [`super::registry::resolve_id`], which is what turns an unknown key
/// into a warning instead of a silently-stored orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
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
  /// A closed set the value *usually* comes from, plus anything matching
  /// `shape` — custom engine builds add types we cannot enumerate, and the
  /// engine stays the authority on what it accepts. Cycles the ring like
  /// [`Self::Enum`] but only rejects what could not possibly be a value.
  OpenEnum {
    choices: &'static [&'static str],
    shape: Shape,
  },
  /// Comma-separated numbers (a per-GPU split ratio). Validated because a
  /// typo here is otherwise only caught by the engine, minutes into a load.
  Ratio,
  /// Free-form text (device selectors, paths).
  Str,
}

/// The shape an [`KnobKind::OpenEnum`] value must have to be accepted outside
/// its listed choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
  /// A single identifier token: leading letter, then letters/digits/`_`.
  /// What a quantisation type name can look like.
  Identifier,
}

impl Shape {
  pub fn accepts(self, v: &str) -> bool {
    match self {
      Shape::Identifier => {
        let mut chars = v.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
          && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
      }
    }
  }
}

impl KnobKind {
  /// Whether a surface should open a text input for this knob. Only a bool is
  /// excluded: it has exactly two states, so typing one is strictly worse than
  /// toggling it, and the editor chip would be a dead affordance. Everything
  /// else — a closed ring included — accepts a typed value, validated on
  /// commit by [`super::parse_value`].
  pub fn is_editable(self) -> bool {
    !matches!(self, KnobKind::Bool)
  }

  /// The ring a surface cycles for this knob; empty when it has none.
  pub fn choices(self) -> &'static [&'static str] {
    match self {
      KnobKind::Enum { choices } | KnobKind::OpenEnum { choices, .. } => choices,
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
      KnobKind::Ratio => "RATIO",
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
  /// `--flag on` / `--flag off`. For engines that require an explicit value on
  /// a boolean flag and would otherwise swallow the next argv token as it.
  FlagOnOff,
  /// The backend consumes the value itself (non-argv transports, or a value
  /// that expands into a bundle of flags).
  Custom,
}

/// What `←`/`→` cycles on this knob's editor row.
///
/// The ring is part of the *declaration* because the editor is generated from
/// it: a knob whose backend forgot to say how it cycles would render a dead
/// row, which is exactly the drift the registry exists to make impossible.
/// Three variants resolve against runtime state the declaration cannot know
/// — offering a stop the host or model cannot honor is offering a launch that
/// fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ring {
  /// Nothing to cycle. Free-form values are `e`-edited instead, and an
  /// [`KnobKind::Enum`] / [`KnobKind::OpenEnum`] cycles its declared
  /// `choices` — restating those here would be a second source for one fact.
  None,
  /// A fixed ladder of stops, ascending.
  Fixed(&'static [&'static str]),
  /// A fixed ladder trimmed to the window the *model* was trained for.
  UpToTrainedContext(&'static [&'static str]),
  /// `0 .. N-1` over the devices actually in play. A fixed ladder would offer
  /// GPU indices a smaller host does not have.
  DeviceIndex,
  /// Not a value ring: `←`/`→` walks the host's devices and `Space` toggles
  /// each in or out of the selection.
  DeviceCheckbox,
}

/// A runtime condition that has to hold for a [`Group`]'s rows to be worth
/// showing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupGate {
  /// The server offers more than one `--device` selector. Not the same as
  /// [`Self::MultiDevice`]: a build carrying two compute APIs reports one card
  /// twice (`ROCm0` + `Vulkan0`), which is a real choice of compute path but
  /// nothing to place a model across.
  DeviceChoice,
  /// The server sees more than one physical GPU.
  MultiDevice,
  /// The model can actually speculate (an embedded draft head, or a drafter
  /// sibling on disk).
  SpeculationCapable,
}

/// The runtime facts the [`GroupGate`]s ask about, gathered once per render.
///
/// A struct rather than positional bools: `device_choice` and `multi_device`
/// differ only on hosts where one card answers to two selectors, so a swapped
/// pair of arguments would pass every test written on ordinary hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateFacts {
  /// The scoped server offers more than one `--device` selector.
  pub device_choice: bool,
  /// The scoped server sees more than one physical GPU.
  pub multi_device: bool,
  /// The model can speculate.
  pub mtp_capable: bool,
}

/// Where a knob sits in the editor, and the `--help` heading it groups under.
/// Ordered by how often a typical user touches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
  Context,
  Offload,
  Device,
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
      Group::Device => "Device",
      Group::MultiGpu => "Multi-GPU placement",
      Group::Attention => "Attention & KV cache",
      Group::Throughput => "Throughput",
      Group::Memory => "Memory loading",
      Group::Speculation => "Speculative decoding",
      Group::Advanced => "Advanced",
    }
  }

  /// What has to be true for this group's rows to be shown, or `None` when
  /// they always are. Evaluated generically by the editor, so a group can gain
  /// a gate without the editor learning anything about it.
  pub fn gate(self) -> Option<GroupGate> {
    match self {
      Group::Device => Some(GroupGate::DeviceChoice),
      Group::MultiGpu => Some(GroupGate::MultiDevice),
      Group::Speculation => Some(GroupGate::SpeculationCapable),
      _ => None,
    }
  }

  /// Whether this group's runtime gate is satisfied, given the runtime facts a
  /// [`GroupGate`] can ask about. A group with no gate is always open. Shared
  /// so an editable picker and a read-only summary of the same backend's knobs
  /// can never answer a gate differently.
  pub fn gate_open(self, facts: GateFacts) -> bool {
    match self.gate() {
      None => true,
      Some(GroupGate::DeviceChoice) => facts.device_choice,
      Some(GroupGate::MultiDevice) => facts.multi_device,
      Some(GroupGate::SpeculationCapable) => facts.mtp_capable,
    }
  }

  /// Render / navigation order.
  pub fn all() -> &'static [Group] {
    &[
      Group::Context,
      Group::Offload,
      Group::Device,
      Group::MultiGpu,
      Group::Attention,
      Group::Throughput,
      Group::Memory,
      Group::Speculation,
      Group::Advanced,
    ]
  }
}

/// Context-window quick picks, doubling up to the launcher's own ceiling
/// (`MAX_CTX_TOKENS` = 1 Mi). Declared once here because more than one backend
/// offers the same ladder; each trims it to the model's trained window through
/// [`Ring::UpToTrainedContext`].
pub const CTX_LADDER: &[&str] = &[
  "2048", "4096", "8192", "16384", "32768", "65536", "131072", "262144", "524288", "1048576",
];

/// CPU thread-count quick picks. Declared once because more than one backend
/// offers the same ladder for its own `threads` knob.
pub const THREADS_LADDER: &[&str] = &["1", "2", "4", "6", "8", "12", "16", "24"];

/// Draft-token-count quick picks for a speculative-decoding draft-n knob.
/// Declared once because more than one backend offers the same ladder.
pub const DRAFT_N_LADDER: &[&str] = &["1", "2", "3", "4"];

/// The neutral speculation-enable knob, and the one [`KnobDef`] declared
/// outside a backend module.
///
/// Its meaning — "delegate to the engine's own capability check" — is
/// backend-agnostic, so more than one backend declares this exact knob rather
/// than a backend-specific variant. Shared rather than copied because the
/// copies were byte-identical and nothing kept them that way. It carries no
/// flag (the daemon projects the resolved intent onto it, `Emit::Custom`), so
/// hoisting it leaves no backend flag spelling outside `<id>/knobs.rs`.
pub const MTP_ENABLE: KnobDef = KnobDef {
  id: "mtp",
  flag: None,
  concept: None,
  kind: KnobKind::Bool,
  auto: Some(AutoKind::Capability),
  group: Group::Speculation,
  label: "MTP",
  help: "multi-token prediction; auto enables it when the model can",
  aliases: &[],
  fallback: LayerLabel::ServerDefault,
  emit: Emit::Custom,
  ring: Ring::None,
  volatile: false,
};

/// One tunable, as declared by the backend that owns it.
///
/// Declared in `src/backend/<id>/knobs.rs` — that file is the only place a
/// backend's flag spellings appear. A knob two backends declare *identically*
/// may instead be a shared const here ([`MTP_ENABLE`]), which is safe only
/// while that const has `flag: None`: a hoisted def carrying `--something`
/// would move a spelling out of the module that owns it. Pinned by
/// `a_knob_def_outside_a_backend_module_declares_no_flag`.
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
  /// How the editor cycles this knob. See [`Ring`].
  pub ring: Ring,
  /// Whether a user-set value must **not** be replayed from `last_params`.
  ///
  /// For a knob whose value is a judgement about *this host, right now* rather
  /// than a lasting preference. Persisting one makes a single experiment
  /// permanent: the user passes it once through a preset, and every later bare
  /// launch silently inherits it — including, for a memory knob, the launch
  /// where the automatic guard would have stepped in. The knob still applies to
  /// the launch that asked for it, and whenever its preset is named again; it
  /// is just not remembered on the user's behalf.
  ///
  /// Declared here rather than in a separate id list because the two drifted:
  /// the list kept pre-registry spellings after the ids changed, so it matched
  /// nothing and the guard silently stopped guarding.
  pub volatile: bool,
  /// Where the value comes from when *no* layer supplies one, which is what
  /// the editor renders as the origin chip. `ModelDefault` for knobs the
  /// engine reads out of the model file when the flag is omitted (context
  /// window, chat template); `ServerDefault` for everything else, where
  /// omitting the flag lands on the engine's own hardcoded default.
  ///
  /// Doubles as the "no layer supplied this" sentinel: no real layer ever
  /// carries these two labels, so `source == fallback` is an exact test.
  pub fallback: LayerLabel,
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

  /// The declared ring, with a closed-choice knob's `choices` standing in for
  /// [`Ring::None`]. Lets a surface ask one question instead of two.
  pub fn ring(&self) -> Ring {
    match self.ring {
      Ring::None if !self.kind.choices().is_empty() => Ring::Fixed(self.kind.choices()),
      other => other,
    }
  }

  /// Whether a surface should open a text input on this knob. A ring and a
  /// text input coexist — cycling is the quick path, typing the exact one.
  pub fn is_editable(&self) -> bool {
    self.kind.is_editable()
  }
}
