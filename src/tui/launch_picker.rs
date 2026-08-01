//! Launch picker form state — the typed-knob editor.
//!
//! The Settings tab renders a vertical list of rows: every
//! `TypedKnobs` field (ctx, reasoning, n_gpu_layers, … in
//! `knob_specs()` order) with a per-row source label (`(user)`,
//! `(last used)`, `(arch default)`, `(model default)`,
//! `(server default)`), plus an `extras` free-text row at the
//! bottom. Up/Down moves between rows; Left/Right cycles the focused
//! row's value; `e` enters inline edit; Enter launches (or commits
//! an open edit); Backspace resets the focused row.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::config::{KnobValue, TypedKnobs};
use crate::launch::flag_aliases::{
  knob_display_groups, knob_row_visible, KnobField, KV_CACHE_TYPES, SPLIT_MODES,
};
use crate::launch::native_knobs::{NativeKnobDescriptor, NativeKnobKind};
use crate::launch::params::{BackendChoice, LayerLabel};

/// Cycle ring for a boolean native knob (`inherited → on → off → inherited`).
const NATIVE_BOOL_RING: &[&str] = &["true", "false"];

/// Pre-canned context-length presets surfaced as quick picks, doubling up to
/// the launcher ceiling (`MAX_CTX_TOKENS` = 1 Mi). The cycle is gated per model
/// to the trained window (see `LaunchPickerState::ctx_presets`); custom
/// values still flow through the same field when the user types digits.
pub const CTX_PRESETS: &[u32] = &[
  2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576,
];

/// Value-column label for a knob the user hasn't set — it inherits from
/// the resolver chain (last used / arch default / model default / server
/// default), named by the row's source chip. One constant so every
/// surface (picker form, running view, device row) agrees on the word.
pub const INHERITED_LABEL: &str = "inherited";

/// Which row the cursor is on. The editor renders top-to-bottom in
/// [`PickerField::all`] order so it doubles as the vertical-navigation
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerField {
  /// The preset cycle row, always shown at the very top of the form.
  /// Cycling it rewrites every knob row below live.
  Preset,
  /// The server (build/binary) cycle row, shown just under `Preset` when the
  /// model has more than one compatible server. Cycling it re-scopes the
  /// Device row + multi-GPU gating and, on a cross-backend switch, the knob
  /// set. Hidden with 0 or 1 server (nothing to pick).
  Server,
  Knob(KnobField),
  /// A backend-declared native knob, by index into the active backend's
  /// [`NativeKnobDescriptor`] slice (see [`crate::launch::native_knobs`]).
  /// Index-keyed (not the id string) so `PickerField` stays `Copy`; the
  /// descriptor is resolved through [`LaunchPickerState::native_descriptors`].
  /// Six rows for ds4; none for llama.cpp / Lemonade.
  NativeKnob(usize),
  /// MTP (multi-token prediction) enable cycle (`auto`/`on`/`off`). Shown only
  /// when the model is MTP-capable, and backend-agnostic: the resolved intent
  /// is honored by whichever backend runs the model.
  Mtp,
  Extras,
}

/// One stop on the picker's preset cycle. The ring is
/// `last used → auto → <named presets…>`. The model's configured default
/// is not a separate stop: it is whichever of these stops `default:`
/// resolves to (a named preset, `auto`, or — when unset — `last used`),
/// marked with a `(default)` suffix and opened on. Selecting a stop
/// rewrites the form's user knobs + extras: `LastUsed` restores the opening
/// baseline (the pre-filled last-used params), `Auto` delegates the
/// fit-governed knobs to `--fit`, and `Named` seeds from the named preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetStop {
  LastUsed,
  Auto,
  Named(usize),
}

/// A named preset materialised for the picker: the user-knob set to seed
/// (ctx / reasoning folded into the typed knobs, matching `user_knobs`)
/// and the extras argv tail.
#[derive(Debug, Clone, PartialEq)]
pub struct PresetChoice {
  pub name: String,
  pub knobs: TypedKnobs,
  pub extras: Vec<std::ffi::OsString>,
  /// Per-backend native-knob values the preset pins (see
  /// [`crate::launch::native_knobs`]). Seeds the picker's `backend_knobs`
  /// when this preset is selected. Populated for ds4; empty for llama.cpp.
  pub backend_knobs: BTreeMap<String, KnobValue<String>>,
}

/// Lazily-built navigation order — every knob in editor **display**
/// order (the flattened [`knob_display_groups`], which clusters knobs
/// by function and is distinct from the pinned argv order), followed
/// by `Extras`. Built once on first access so per-keypress navigation
/// does no allocation.
static ALL_FIELDS: LazyLock<Box<[PickerField]>> = LazyLock::new(|| {
  // Preset row leads, then the server row (both cycle-only selectors);
  // knob groups follow; extras last. `Server` is gated to `>1` server in
  // `field_visible`, so a single-server host never lands on it.
  let mut v: Vec<PickerField> = vec![PickerField::Preset, PickerField::Server];
  for group in knob_display_groups() {
    for field in group.fields {
      v.push(PickerField::Knob(*field));
    }
  }
  v.push(PickerField::Extras);
  v.into_boxed_slice()
});

impl PickerField {
  /// All rows in render / navigation order. Returns a static slice so
  /// `next_field` / `prev_field` don't allocate on each keypress.
  pub fn all() -> &'static [PickerField] {
    &ALL_FIELDS
  }

  /// Whether `e:edit` opens an inline buffer on this row.
  ///
  /// - Numeric / float / enum knobs and the free-text `Extras` row
  ///   open an [`crate::tui::input_field::InputField`] for typing.
  /// - Boolean knobs (reasoning, flash_attn, mlock, no_mmap) don't —
  ///   they're cycled with ←/→. Surfacing `e:edit` on a boolean row
  ///   would be a no-op chip and a misleading affordance.
  ///
  /// Shared between the Settings-row edit handler (which early-returns
  /// on booleans) and the right-pane hint strip (which hides the chip
  /// on those rows) so the chip and the handler stay in lockstep.
  pub fn is_editable(self) -> bool {
    match self {
      // Preset / Server / Mtp are cycle-only rows (←/→), like a boolean knob.
      PickerField::Preset | PickerField::Server | PickerField::Mtp => false,
      PickerField::Extras => true,
      // The native row's editability depends on its descriptor kind, which
      // the bare index can't see — resolved by
      // [`LaunchPickerState::focused_is_editable`]. Conservative default.
      PickerField::NativeKnob(_) => false,
      PickerField::Knob(k) => match k {
        KnobField::Reasoning | KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => false,
        KnobField::Ctx
        | KnobField::NGpuLayers
        | KnobField::NCpuMoe
        | KnobField::Threads
        | KnobField::Parallel
        | KnobField::BatchSize
        | KnobField::UbatchSize
        | KnobField::Keep
        | KnobField::RopeFreqScale
        | KnobField::CacheTypeK
        | KnobField::CacheTypeV
        | KnobField::Device
        | KnobField::TensorSplit
        | KnobField::MainGpu
        | KnobField::SplitMode => true,
      },
    }
  }
}

/// Inline-edit state owned by [`LaunchPickerState`].
///
/// The buffer and modal `editing` flag live in `inline_edit`
/// ([`crate::tui::input_field::InputField`]) so the typed-knob editor shares the
/// `e:edit / Esc:walk-back / Enter:Submit` contract with every
/// other text input in the TUI. The wrapper carries the two extra
/// pieces of state `crate::tui::input_field::InputField` doesn't model:
///
/// - `field` — which `PickerField` the open edit is editing (numeric
///   / enum knob or the extras row), so `commit_inline_edit` knows
///   where to write the parsed value.
/// - `error` — the inline parse / validation error rendered under
///   the row when commit fails.
///
/// Both reset when the edit closes (either via successful commit
/// or `Esc` walk-back).
#[derive(Debug, Clone, Default)]
pub struct InlineEdit {
  pub field: Option<PickerField>,
  pub input: crate::tui::input_field::InputField,
  pub error: Option<String>,
}

impl InlineEdit {
  /// Open the edit on `field`, seed the buffer with `initial`, and
  /// enter edit mode so subsequent keystrokes append to the buffer.
  pub fn open(&mut self, field: PickerField, initial: String) {
    self.field = Some(field);
    self.input.set_text(initial);
    self.input.enter_edit();
    self.error = None;
  }

  /// Close the edit — clear the field marker, drop the buffer, exit
  /// edit mode, and clear any stale error.
  pub fn close(&mut self) {
    self.field = None;
    self.input.clear();
    self.input.exit_edit();
    self.error = None;
  }

  /// True while the user is actively typing into the buffer (the
  /// edit is open *and* `crate::tui::input_field::InputField` reports
  /// edit mode). Used by
  /// the event router to send keys to the input instead of the
  /// outer keymap.
  pub fn is_open(&self) -> bool {
    self.field.is_some() && self.input.is_editing()
  }
}

/// State of the launch picker.
#[derive(Debug, Clone)]
pub struct LaunchPickerState {
  /// Display name of the focused model (rendered in the title).
  pub model_name: String,
  /// The model's native (trained) context length, when known. Gates the ctx
  /// quick-pick cycle so it never offers a window larger than the model
  /// supports; `None` leaves the full preset ladder available.
  pub native_ctx: Option<u32>,
  /// User-supplied typed knobs (only fields the user explicitly set;
  /// every other field stays `None` and inherits from the resolved
  /// chain on render). Includes `ctx` and `reasoning`.
  pub user_knobs: TypedKnobs,
  /// Resolved knobs after applying the layered resolver — what the
  /// editor shows for each row.
  pub resolved: TypedKnobs,
  /// Per-knob source labels for the right-aligned origin chip.
  pub sources: BTreeMap<KnobField, LayerLabel>,
  /// Free-form argv tail forwarded to llama-server.
  pub extras: Vec<std::ffi::OsString>,
  /// The active backend's native-knob descriptors (see
  /// [`crate::launch::native_knobs`]). Seeded from `backend.native_knobs()`
  /// when the picker is built — six for ds4, empty for llama.cpp / Lemonade.
  /// Tests inject a stub slice directly.
  pub native_descriptors: &'static [NativeKnobDescriptor],
  /// User-set native-knob values, keyed by descriptor id. Parallel to
  /// `user_knobs`; seeds from last-used / preset and returns into
  /// [`crate::launch::params::LaunchParams::backend_knobs`].
  pub backend_knobs: BTreeMap<String, KnobValue<String>>,
  /// Modal text-input for the extras row (`is_editing()` replaces
  /// the bespoke `extras_editing` bool; `buffer()` replaces the raw
  /// string + cursor pair). Shares the `e:edit / Esc:walk-back /
  /// Enter:Submit` contract with every other text input in the TUI.
  pub extras_input: crate::tui::input_field::InputField,
  /// Inline edit state for numeric / enum rows. Wraps an
  /// [`crate::tui::input_field::InputField`] plus the `PickerField`
  /// marker so the commit path
  /// knows which row to write back to, and an optional parse-error
  /// string rendered under the row.
  pub inline_edit: InlineEdit,
  pub field: PickerField,
  /// The focused model's *own* concrete backend (the one its catalog source
  /// binds to): `LlamaCpp` for a GGUF, `Lemonade` for a `lemonade://`
  /// registry model. A model lives in exactly one backend's catalog, so
  /// there is no user-facing chooser — this drives knob visibility
  /// and is what the launch dispatches to. Never `Auto`.
  pub model_backend: BackendChoice,
  pub active_instances: usize,
  pub prefer_port: Option<u16>,
  /// Compatible servers for this model (priority-ordered): the builds the
  /// Server row cycles through, from `status.servers` filtered to the model's
  /// `supported_backends`. Each carries its own probed `--device` selectors;
  /// the Device row + multi-GPU gating scope to the [`Self::current_server`].
  /// Empty when the daemon hasn't probed any binary.
  pub servers: Vec<crate::backend::Server>,
  /// The user's chosen server id (`llamacpp-vulkan`, `ds4`), or `None` for
  /// the priority default (`servers[0]`). Sent verbatim as
  /// [`crate::launch::params::LaunchParams::server`]; seeded from last_params.
  pub selected_server: Option<String>,
  /// Walk cursor over the scoped server's devices — the GPU `Space` toggles.
  /// ←/→ move it; it does **not** itself select. Clamped to the device count
  /// on render; reset to 0 when the scoped server changes.
  device_cursor: usize,
  /// Effective presets for this model (per-model ∪ arch), name-sorted —
  /// the cycle stops below `auto`. Empty for a model with no presets, in
  /// which case the Preset row is hidden.
  pub presets: Vec<PresetChoice>,
  /// The cycle stop the model's `default:` resolves to — `Named(i)` for a
  /// configured named default, `Auto` for `default: auto`, or `LastUsed`
  /// when unset. The cycle opens here and the row marks it `(default)`.
  pub default_stop: PresetStop,
  /// Current cycle stop. The Preset row's value column renders this.
  pub preset_stop: PresetStop,
  /// MTP speculative-decoding intent for this launch (`auto`/`on`/`off`). The
  /// `Mtp` cycle row renders this; sent as the `mtp` start param. Backend-agnostic.
  pub mtp: crate::launch::params::MtpEnable,
  /// Whether the focused model is MTP-capable (embedded nextn head or a separate
  /// `mtp-*.gguf` drafter). Gates the `Mtp` row — hidden when the model can't do
  /// MTP, shown for any capable model regardless of backend.
  pub mtp_capable: bool,
  /// The non-preset baseline (the build-time `user_knobs` / `extras` seed:
  /// last-used params, or empty). Restored when cycling back to `auto`.
  preset_baseline_knobs: TypedKnobs,
  preset_baseline_extras: Vec<std::ffi::OsString>,
  preset_baseline_backend_knobs: BTreeMap<String, KnobValue<String>>,
  /// Row offset clipped from the top of the rendered line list so the
  /// focused row stays visible on small viewports. Recomputed on each
  /// render using the actual area height — the `Cell` lets the
  /// read-only render path (which only has `&App`) update the cached
  /// offset without taking a mutable borrow.
  pub scroll_offset: Cell<u16>,
}

impl LaunchPickerState {
  pub fn for_model(model_name: impl Into<String>) -> Self {
    Self {
      model_name: model_name.into(),
      native_ctx: None,
      user_knobs: TypedKnobs::default(),
      resolved: TypedKnobs::default(),
      sources: BTreeMap::new(),
      extras: Vec::new(),
      native_descriptors: &[],
      backend_knobs: BTreeMap::new(),
      extras_input: crate::tui::input_field::InputField::default(),
      inline_edit: InlineEdit::default(),
      field: PickerField::Knob(KnobField::Ctx),
      model_backend: BackendChoice::from_id(crate::backend::DEFAULT_BACKEND_ID),
      active_instances: 0,
      prefer_port: None,
      servers: Vec::new(),
      selected_server: None,
      device_cursor: 0,
      presets: Vec::new(),
      default_stop: PresetStop::LastUsed,
      preset_stop: PresetStop::LastUsed,
      mtp: crate::launch::params::MtpEnable::default(),
      mtp_capable: false,
      preset_baseline_knobs: TypedKnobs::default(),
      preset_baseline_extras: Vec::new(),
      preset_baseline_backend_knobs: BTreeMap::new(),
      scroll_offset: Cell::new(0),
    }
  }

  /// Whether the model has any **named** presets (per-model ∪ arch). The
  /// preset row itself is always shown (it always offers `last used` ↔
  /// `auto`); this only reports whether named stops exist beyond those.
  pub fn has_presets(&self) -> bool {
    !self.presets.is_empty()
  }

  /// Seed the preset cycle from the model's effective set. Captures the
  /// current `user_knobs` / `extras` (the pre-filled last-used params) as
  /// the `last used` baseline, records the resolved default stop, and opens
  /// on it (matching what the daemon would resolve for a no-selection
  /// launch). A `Named` default with a stale index falls back to `LastUsed`.
  /// The cursor is left where it was; the Preset row leads visually but
  /// isn't auto-focused.
  pub fn set_presets(&mut self, presets: Vec<PresetChoice>, default_stop: PresetStop) {
    self.preset_baseline_knobs = self.user_knobs.clone();
    self.preset_baseline_extras = self.extras.clone();
    self.preset_baseline_backend_knobs = self.backend_knobs.clone();
    self.presets = presets;
    self.default_stop = match default_stop {
      PresetStop::Named(i) if i < self.presets.len() => PresetStop::Named(i),
      PresetStop::Named(_) => PresetStop::LastUsed,
      other => other,
    };
    self.preset_stop = self.default_stop;
    self.apply_preset_stop();
  }

  /// The cycle ring in order: `last used → auto → named…`. The default is
  /// not a separate stop — it is whichever of these `default_stop` names.
  fn preset_ring(&self) -> Vec<PresetStop> {
    let mut ring = Vec::with_capacity(self.presets.len() + 2);
    ring.push(PresetStop::LastUsed);
    ring.push(PresetStop::Auto);
    ring.extend((0..self.presets.len()).map(PresetStop::Named));
    ring
  }

  /// Re-seed `user_knobs` / `extras` from the current cycle stop.
  fn apply_preset_stop(&mut self) {
    match self.preset_stop {
      PresetStop::LastUsed => {
        self.user_knobs = self.preset_baseline_knobs.clone();
        self.extras = self.preset_baseline_extras.clone();
        self.backend_knobs = self.preset_baseline_backend_knobs.clone();
      }
      PresetStop::Auto => self.apply_auto(),
      PresetStop::Named(i) => self.seed_from_preset(i),
    }
  }

  /// `auto` stop: delegate every supported fit-governed knob to `--fit`
  /// (the only knobs where `Auto` is meaningful), clear the rest to
  /// inherited, and drop any manual extras. The form reads "auto" on the
  /// fit-governed rows and "inherited" elsewhere.
  fn apply_auto(&mut self) {
    self.user_knobs = TypedKnobs::default();
    for group in knob_display_groups() {
      for &field in group.fields {
        if field.fit_governed() && self.knob_supported(field) {
          crate::launch::set_field_auto(&mut self.user_knobs, field);
        }
      }
    }
    self.extras.clear();
    // `auto` delegates llama.cpp fit-governed knobs to `--fit`; native knobs
    // have no fit notion, so the stop simply drops them back to inherited.
    self.backend_knobs.clear();
  }

  fn seed_from_preset(&mut self, i: usize) {
    if let Some(p) = self.presets.get(i) {
      self.user_knobs = p.knobs.clone();
      self.extras = p.extras.clone();
      self.backend_knobs = p.backend_knobs.clone();
    }
  }

  /// Cycle to the next/previous preset stop and re-seed the form.
  fn cycle_preset(&mut self, forward: bool) {
    let ring = self.preset_ring();
    if ring.is_empty() {
      return;
    }
    let cur = ring
      .iter()
      .position(|s| *s == self.preset_stop)
      .unwrap_or(0);
    let n = ring.len();
    let next = if forward {
      (cur + 1) % n
    } else {
      (cur + n - 1) % n
    };
    self.preset_stop = ring[next];
    self.apply_preset_stop();
  }

  /// Value-column label for the Preset row: `last used`, `auto`, or the
  /// bare preset name — with a ` (default)` suffix when the current stop is
  /// the model's configured default (`last used (default)`, `auto (default)`,
  /// or `long-ctx (default)`).
  pub fn preset_value_label(&self) -> String {
    let base = match self.preset_stop {
      PresetStop::LastUsed => "last used".to_string(),
      PresetStop::Auto => "auto".to_string(),
      PresetStop::Named(i) => self
        .presets
        .get(i)
        .map(|p| p.name.clone())
        .unwrap_or_default(),
    };
    if self.preset_stop == self.default_stop {
      format!("{base} (default)")
    } else {
      base
    }
  }

  /// The server whose devices + backend scope the form: the explicitly
  /// selected one, else the priority default (`servers[0]`). `None` only when
  /// no server was probed.
  pub fn current_server(&self) -> Option<&crate::backend::Server> {
    match &self.selected_server {
      Some(id) => self.servers.iter().find(|s| &s.id == id),
      None => self.servers.first(),
    }
  }

  /// Devices the currently-scoped server can target — the cycle space for the
  /// Device row and the multi-GPU gating count. Empty when no server / a
  /// device-less (ds4 / lemonade / CPU-only) server is scoped.
  fn current_devices(&self) -> &[crate::backend::Device] {
    self
      .current_server()
      .map(|s| s.devices.as_slice())
      .unwrap_or(&[])
  }

  /// Whether the current pick resolves to the priority default: either unset,
  /// or an explicit pin of `servers[0]` (the highest-priority compatible
  /// server). Pinning `servers[0]` is identical to leaving it unset, so the two
  /// collapse into one cycle stop.
  fn server_is_default(&self) -> bool {
    match &self.selected_server {
      None => true,
      Some(id) => self.servers.first().is_some_and(|s| &s.id == id),
    }
  }

  /// Cycle the Server row. The ring is the **default** stop (position 0, which
  /// folds in `servers[0]` — pinning it is identical to the unset default, so it
  /// isn't offered twice) followed by each **non-default** server
  /// (`servers[1..]`). Landing on the default clears the pick (`None`) so the
  /// daemon resolves the priority default / last-used; each other stop pins that
  /// id. A switch clears the device selection (a stale selector wouldn't
  /// validate against the new server) and re-seeds the native-knob descriptors
  /// (the backend may change on a cross-backend switch, deepseek4 ds4 →
  /// llamacpp).
  fn cycle_server(&mut self, forward: bool) {
    // <2 servers means only the default exists — nothing to cycle.
    if self.servers.len() < 2 {
      self.selected_server = None;
      return;
    }
    let mut stops: Vec<Option<String>> = vec![None];
    stops.extend(self.servers.iter().skip(1).map(|s| Some(s.id.clone())));
    let cur_pos = if self.server_is_default() {
      0
    } else {
      stops
        .iter()
        .position(|s| s.as_deref() == self.selected_server.as_deref())
        .unwrap_or(0)
    };
    let len = stops.len();
    let next_pos = if forward {
      (cur_pos + 1) % len
    } else {
      (cur_pos + len - 1) % len
    };
    self.selected_server = stops[next_pos].clone();
    self.set_user_str(KnobField::Device, None);
    self.device_cursor = 0;
    self.seed_native_descriptors();
  }

  /// Value-column label for the Server row: `"<id> (default)"` naming the
  /// priority default the daemon resolves when the pick is the default stop,
  /// else the pinned server id. `"default"` alone when no server was probed.
  pub fn server_value_label(&self) -> String {
    if self.server_is_default() {
      return match self.servers.first() {
        Some(s) => format!("{} (default)", s.id),
        None => "default".to_string(),
      };
    }
    // Not the default → an explicit non-`servers[0]` pin.
    self
      .selected_server
      .clone()
      .unwrap_or_else(|| "default".to_string())
  }

  /// Seed the resolved knobs + source map from the layered resolver
  /// output. The user-knobs layer is empty on a freshly-opened
  /// editor — the rows show inherited values.
  pub fn set_resolved(&mut self, resolved: TypedKnobs, sources: BTreeMap<KnobField, LayerLabel>) {
    self.resolved = resolved;
    self.sources = sources;
  }

  /// All rows in render / navigation order: the static base
  /// ([`PickerField::all`] = `[Preset, knob groups…, Extras]`) with one
  /// [`PickerField::NativeKnob`] per active descriptor spliced in **just
  /// before** the trailing `Extras` row, so the free-text extras row always
  /// stays last. For shipping backends the descriptor slice is empty, so this
  /// equals the static order exactly.
  pub fn ordered_fields(&self) -> Vec<PickerField> {
    let mut natives: Vec<PickerField> = (0..self.native_descriptors.len())
      .map(PickerField::NativeKnob)
      .collect();
    // The MTP row rides just after the native knobs (before Extras), shown only
    // for an MTP-capable model.
    if self.mtp_capable {
      natives.push(PickerField::Mtp);
    }
    let mut v: Vec<PickerField> = PickerField::all().to_vec();
    let extras_at = v
      .iter()
      .position(|f| matches!(f, PickerField::Extras))
      .unwrap_or(v.len());
    v.splice(extras_at..extras_at, natives);
    v
  }

  /// The descriptor the focused row points at, when it's a native row.
  pub fn focused_native(&self) -> Option<&'static NativeKnobDescriptor> {
    match self.field {
      PickerField::NativeKnob(i) => self.native_descriptors.get(i),
      _ => None,
    }
  }

  /// Whether `e:edit` opens an inline buffer on the focused row. Resolves a
  /// native row through its descriptor kind (free-text only); delegates to
  /// [`PickerField::is_editable`] for everything else.
  pub fn focused_is_editable(&self) -> bool {
    match self.focused_native() {
      Some(d) => d.is_editable(),
      None => self.field.is_editable(),
    }
  }

  /// Cycle the focused field's value forward (Right arrow).
  pub fn cycle_focused_value_next(&mut self) {
    match self.field {
      PickerField::Preset => self.cycle_preset(true),
      PickerField::Server => self.cycle_server(true),
      PickerField::Knob(k) => self.cycle_knob(k, true),
      PickerField::NativeKnob(i) => self.cycle_native(i, true),
      PickerField::Mtp => self.mtp = self.mtp.cycled(true),
      PickerField::Extras => {}
    }
  }

  /// Cycle the focused field's value backward (Left arrow).
  pub fn cycle_focused_value_prev(&mut self) {
    match self.field {
      PickerField::Preset => self.cycle_preset(false),
      PickerField::Server => self.cycle_server(false),
      PickerField::Knob(k) => self.cycle_knob(k, false),
      PickerField::NativeKnob(i) => self.cycle_native(i, false),
      PickerField::Mtp => self.mtp = self.mtp.cycled(false),
      PickerField::Extras => {}
    }
  }

  /// Cycle a native knob through its ring (`inherited → values… → wrap`).
  /// Cycle knobs use the descriptor's preset list; bools use on/off;
  /// free-text rows don't cycle (they're `e`-edited). No `Auto` stop —
  /// native knobs are not fit-governed.
  fn cycle_native(&mut self, idx: usize, forward: bool) {
    let Some(descriptor) = self.native_descriptors.get(idx).copied() else {
      return;
    };
    let ring: &[&str] = match descriptor.kind {
      NativeKnobKind::Cycle { presets } => presets,
      NativeKnobKind::Bool => NATIVE_BOOL_RING,
      // Free-text has no preset ring — edited via `e`.
      NativeKnobKind::FreeText => return,
    };
    let cur = self
      .backend_knobs
      .get(descriptor.id)
      .and_then(KnobValue::as_set)
      .and_then(|v| ring.iter().copied().find(|t| *t == v))
      .map_or(CycleState::Inherited, CycleState::Set);
    match ring_next(cur, ring, forward, false) {
      CycleState::Set(v) => {
        self
          .backend_knobs
          .insert(descriptor.id.to_string(), KnobValue::Set(v.to_string()));
      }
      // Inherited (or the unreachable Auto with allow_auto=false) clears it.
      _ => {
        self.backend_knobs.remove(descriptor.id);
      }
    }
  }

  /// The display value for a native row: the set value, or the shared
  /// `inherited` label when unset.
  pub fn native_value_label(&self, idx: usize) -> String {
    self
      .native_descriptors
      .get(idx)
      .and_then(|d| self.backend_knobs.get(d.id))
      .and_then(KnobValue::as_set)
      .cloned()
      .unwrap_or_else(|| INHERITED_LABEL.to_string())
  }

  /// Seed text for a native free-text `e`-edit: the current set value, or
  /// empty when the row inherits.
  pub fn native_buffer_seed(&self, idx: usize) -> String {
    self
      .native_descriptors
      .get(idx)
      .and_then(|d| self.backend_knobs.get(d.id))
      .and_then(KnobValue::as_set)
      .cloned()
      .unwrap_or_default()
  }

  /// Commit a free-text native edit: a non-empty value pins `Set`, an empty
  /// one clears the row back to inherited.
  pub fn set_native_text(&mut self, idx: usize, value: &str) {
    let Some(d) = self.native_descriptors.get(idx) else {
      return;
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
      self.backend_knobs.remove(d.id);
    } else {
      self
        .backend_knobs
        .insert(d.id.to_string(), KnobValue::Set(trimmed.to_string()));
    }
  }

  /// The model's concrete backend, resolved from `model_backend`. The single
  /// backend resolution in the picker — `knob_supported`,
  /// `seed_native_descriptors`, and `active_backend_id` all go through it.
  /// Registry-driven: an explicit id resolves to that backend, `Auto` / an
  /// unknown id to the default. Names no backend.
  fn resolved_backend(&self) -> crate::backend::Backends {
    use crate::backend::{Backend, Backends};
    // An explicit server pick determines the backend (its owning backend), so
    // cycling to a llamacpp server on a deepseek4 model swaps the knob set to
    // llama.cpp's. An unset pick falls through to the model's own backend.
    if let Some(id) = &self.selected_server {
      if let Some(srv) = self.servers.iter().find(|s| &s.id == id) {
        if let Some(b) = Backends::all()
          .into_iter()
          .find(|b| b.id() == srv.backend_id.as_str())
        {
          return b;
        }
      }
    }
    match &self.model_backend {
      BackendChoice::Explicit(id) => Backends::all()
        .into_iter()
        .find(|b| b.id() == id.as_str())
        .unwrap_or_else(crate::backend::default_backend),
      BackendChoice::Auto => crate::backend::default_backend(),
    }
  }

  /// The `backend` override to send on the wire. `model_backend` is a *scoping*
  /// value the picker derives from the row's badge, not a user-chosen override
  /// (the picker exposes no backend cycle). A scope for a **direct routing**
  /// backend (a non-default backend the daemon picks by header) is downgraded to
  /// `Auto` so the daemon re-derives the route — it still lands there for a
  /// compatible file, but the daemon's Auto-only pre-spawn guards (e.g. a
  /// split-file refusal) fire instead of a doomed load. The default backend and
  /// a managed-multiplexer (identity-bound) scope pass through unchanged. Names
  /// no backend.
  pub fn launch_backend(&self) -> BackendChoice {
    match &self.model_backend {
      BackendChoice::Explicit(id)
        if id != crate::backend::DEFAULT_BACKEND_ID
          && !crate::backend::is_managed_multiplexer(id) =>
      {
        BackendChoice::Auto
      }
      other => other.clone(),
    }
  }

  /// Resolve [`Self::native_descriptors`] from the model's backend. Called
  /// once the backend is known. ds4 declares six; llama.cpp / Lemonade none —
  /// a backend opts in by overriding `native_knobs()`.
  pub fn seed_native_descriptors(&mut self) {
    use crate::backend::Backend;
    self.native_descriptors = self.resolved_backend().native_knobs();
  }

  /// Whether the model's backend honors `field`.
  /// [`Self::field_visible`] hides rows the backend can't honor.
  /// llama.cpp honors every typed knob; Lemonade honors `ctx` only
  /// (lemond's `ctx_size` load option).
  pub fn knob_supported(&self, field: KnobField) -> bool {
    use crate::backend::Backend;
    self.resolved_backend().capabilities().supports(field)
  }

  /// The model's backend id for log / label use.
  pub fn active_backend_id(&self) -> &'static str {
    use crate::backend::Backend;
    self.resolved_backend().id()
  }

  /// The ctx quick-pick ladder gated to the model's native window: every
  /// [`CTX_PRESETS`] entry `≤ native_ctx` (all of them when the native window
  /// is unknown). Keeps the cycle from offering a context larger than the
  /// model trained on; a user who wants more can still type a custom value.
  fn ctx_presets(&self) -> Vec<u32> {
    match self.native_ctx {
      Some(max) => CTX_PRESETS.iter().copied().filter(|&c| c <= max).collect(),
      None => CTX_PRESETS.to_vec(),
    }
  }

  fn cycle_knob(&mut self, field: KnobField, forward: bool) {
    match field {
      KnobField::Ctx => {
        let presets = self.ctx_presets();
        self.cycle_u32(field, &presets, forward);
      }
      KnobField::Reasoning => self.cycle_bool(field, forward),
      KnobField::NGpuLayers => self.cycle_u32(field, &[0, 16, 32, 64, 99], forward),
      KnobField::NCpuMoe => self.cycle_u32(field, &[0, 4, 8, 16, 32, 64], forward),
      KnobField::Threads => self.cycle_u32(field, &[1, 2, 4, 6, 8, 12, 16, 24], forward),
      KnobField::Parallel => self.cycle_u32(field, &[1, 2, 4, 8, 16], forward),
      KnobField::BatchSize => self.cycle_u32(field, &[256, 512, 1024, 2048, 4096], forward),
      KnobField::UbatchSize => self.cycle_u32(field, &[128, 256, 512, 1024], forward),
      KnobField::Keep => self.cycle_u32(field, &[0, 64, 128, 256, 512, 1024], forward),
      KnobField::MainGpu => {
        let presets = self.main_gpu_presets();
        self.cycle_u32(field, &presets, forward);
      }
      KnobField::RopeFreqScale => self.cycle_f32(field, &[0.5, 1.0, 2.0, 4.0], forward),
      KnobField::CacheTypeK | KnobField::CacheTypeV => {
        self.cycle_str_set(field, KV_CACHE_TYPES, forward)
      }
      KnobField::SplitMode => self.cycle_str_set(field, SPLIT_MODES, forward),
      KnobField::FlashAttn | KnobField::Mlock | KnobField::NoMmap => {
        self.cycle_bool(field, forward)
      }
      KnobField::Device => self.walk_device_cursor(forward),
      // Free-form ratio with no natural preset set — edited via `e`.
      // ←/→ is a deliberate no-op (there's nothing to cycle through).
      KnobField::TensorSplit => {}
    }
  }

  /// True when the user explicitly cycled this knob to `Auto`.
  fn user_is_auto(&self, field: KnobField) -> bool {
    crate::launch::field_is_auto(&self.user_knobs, field)
  }

  /// Set the user override for `field` to `Auto` (delegate to `--fit`).
  fn set_user_auto(&mut self, field: KnobField) {
    crate::launch::set_field_auto(&mut self.user_knobs, field);
  }

  fn cycle_u32(&mut self, field: KnobField, presets: &[u32], forward: bool) {
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      match self.user_value_u32(field) {
        Some(v) => CycleState::Set(v),
        None => CycleState::Inherited,
      }
    };
    match ring_next(cur, presets, forward, allow_auto) {
      CycleState::Inherited => self.set_user_u32(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(v) => self.set_user_u32(field, Some(v)),
    }
  }

  fn cycle_f32(&mut self, field: KnobField, presets: &[f32], forward: bool) {
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      match self.user_value_f32(field) {
        Some(v) => CycleState::Set(v),
        None => CycleState::Inherited,
      }
    };
    match ring_next(cur, presets, forward, allow_auto) {
      CycleState::Inherited => self.set_user_f32(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(v) => self.set_user_f32(field, Some(v)),
    }
  }

  /// Cycle a constrained-string knob (`cache_type_*`, `split_mode`,
  /// `tensor_split`). Fit-governed knobs carry the `Auto` stop; the rest
  /// cycle `Inherited → set… → wrap`.
  fn cycle_str_set(&mut self, field: KnobField, set: &'static [&'static str], forward: bool) {
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      // Detach the `&'static str` from `&self` so `set_user_str(&mut …)`
      // can run right after without dragging a borrow.
      match self
        .user_value_str(field)
        .and_then(|s| set.iter().copied().find(|t| *t == s))
      {
        Some(v) => CycleState::Set(v),
        None => CycleState::Inherited,
      }
    };
    match ring_next(cur, set, forward, allow_auto) {
      CycleState::Inherited => self.set_user_str(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(v) => self.set_user_str(field, Some(v.to_string())),
    }
  }

  fn cycle_bool(&mut self, field: KnobField, forward: bool) {
    // Fit-governed bools get the quad ring `Inherited → Auto → on → off`;
    // non-fit bools (every bool knob today — flash_attn, mlock, no_mmap,
    // reasoning) drop the no-op Auto stop to a tri ring
    // `Inherited → on → off`.
    let allow_auto = field.fit_governed();
    let cur = if allow_auto && self.user_is_auto(field) {
      CycleState::Auto
    } else {
      match self.user_value_bool(field) {
        Some(b) => CycleState::Set(b),
        None => CycleState::Inherited,
      }
    };
    let next = if forward {
      match cur {
        CycleState::Inherited if allow_auto => CycleState::Auto,
        CycleState::Inherited => CycleState::Set(true),
        CycleState::Auto => CycleState::Set(true),
        CycleState::Set(true) => CycleState::Set(false),
        CycleState::Set(false) => CycleState::Inherited,
      }
    } else {
      match cur {
        CycleState::Inherited => CycleState::Set(false),
        CycleState::Set(false) => CycleState::Set(true),
        CycleState::Set(true) if allow_auto => CycleState::Auto,
        CycleState::Set(true) => CycleState::Inherited,
        CycleState::Auto => CycleState::Inherited,
      }
    };
    match next {
      CycleState::Inherited => self.set_user_bool(field, None),
      CycleState::Auto => self.set_user_auto(field),
      CycleState::Set(b) => self.set_user_bool(field, Some(b)),
    }
  }

  /// Walk the Device row's cursor over the scoped server's devices (←/→). The
  /// cursor only *highlights* a GPU — `Space` ([`Self::toggle_focused_device`])
  /// toggles it into/out of the selection. Wraps at both ends; a no-op on a
  /// device-less server.
  fn walk_device_cursor(&mut self, forward: bool) {
    let n = self.current_devices().len();
    if n == 0 {
      return;
    }
    let cur = self.device_cursor.min(n - 1);
    self.device_cursor = if forward {
      (cur + 1) % n
    } else {
      (cur + n - 1) % n
    };
  }

  /// The concrete set of selected `--device` selectors in catalog order. An
  /// unset row means "all the server's GPUs" (the llama.cpp default), so it
  /// materializes to the full device list — the checkbox view then shows every
  /// box ticked, and toggling one off yields an explicit `N-1` set.
  fn device_selection(&self) -> Vec<String> {
    let devices: Vec<&str> = self
      .current_devices()
      .iter()
      .map(|d| d.selector.as_str())
      .collect();
    match self
      .effective_str(KnobField::Device)
      .filter(|s| !s.is_empty())
    {
      None => devices.iter().map(|s| s.to_string()).collect(),
      Some(csv) => {
        let picked: std::collections::HashSet<&str> = csv
          .split(',')
          .map(str::trim)
          .filter(|s| !s.is_empty())
          .collect();
        // Reorder against the catalog so `main_gpu` / `tensor_split` indices
        // stay stable regardless of the persisted string's order.
        devices
          .iter()
          .filter(|s| picked.contains(**s))
          .map(|s| s.to_string())
          .collect()
      }
    }
  }

  /// `Space` on the Device row: toggle the cursor's GPU in/out of the selection.
  /// Re-serialized in catalog order into `user_knobs.device`. Both extremes
  /// normalize to unset — selecting **all** N is the llama.cpp default (no
  /// `--device`), and clearing the **last** one has no valid "zero GPUs"
  /// meaning, so either snaps the row back to inherited (all GPUs).
  pub fn toggle_focused_device(&mut self) {
    let devices: Vec<String> = self
      .current_devices()
      .iter()
      .map(|d| d.selector.clone())
      .collect();
    if devices.is_empty() {
      return;
    }
    let cursor = self.device_cursor.min(devices.len() - 1);
    let target = &devices[cursor];
    let mut picked: std::collections::HashSet<String> =
      self.device_selection().into_iter().collect();
    if !picked.remove(target) {
      picked.insert(target.clone());
    }
    let ordered: Vec<String> = devices
      .iter()
      .filter(|s| picked.contains(*s))
      .cloned()
      .collect();
    if ordered.is_empty() || ordered.len() == devices.len() {
      self.set_user_str(KnobField::Device, None);
    } else {
      self.set_user_str(KnobField::Device, Some(ordered.join(",")));
    }
  }

  /// `main_gpu` cycle range: `0..N`, where N is the number of devices actually
  /// in play — the count of explicitly-pinned `--device` selectors, or (when
  /// none are pinned, so llama-server uses them all) the full catalog. Replaces
  /// the old hardcoded `[0, 1, 2, 3]` ring, which showed phantom GPU indices on
  /// a host with fewer than four devices.
  fn main_gpu_presets(&self) -> Vec<u32> {
    // `device_selection` already collapses unset -> "all GPUs", so the count of
    // active devices is just its length.
    let n = self.device_selection().len();
    (0..n.max(1) as u32).collect()
  }

  /// Backspace on a focused row: clear the user override and re-
  /// inherit from the resolver chain.
  pub fn reset_focused_row(&mut self) {
    match self.field {
      // Reset on the Preset row snaps back to the `last used` baseline.
      PickerField::Preset => {
        self.preset_stop = PresetStop::LastUsed;
        self.apply_preset_stop();
      }
      // Reset on the Server row snaps back to the priority default.
      PickerField::Server => {
        self.selected_server = None;
        self.set_user_str(KnobField::Device, None);
        self.device_cursor = 0;
        self.seed_native_descriptors();
      }
      // Reset on the Device row re-inherits (all GPUs); the cursor stays put.
      PickerField::Knob(KnobField::Device) => self.set_user_str(KnobField::Device, None),
      PickerField::Knob(k) => self.clear_user(k),
      PickerField::NativeKnob(i) => {
        if let Some(d) = self.native_descriptors.get(i) {
          self.backend_knobs.remove(d.id);
        }
      }
      PickerField::Extras => {
        self.extras.clear();
      }
      // Reset the MTP row back to `auto` (the default intent).
      PickerField::Mtp => {
        self.mtp = crate::launch::params::MtpEnable::default();
      }
    }
  }

  /// Whether the host exposes more than one selectable device. The
  /// `device` row is hidden (rendered and skipped in navigation) when
  /// `false` so single-GPU / CPU-only users don't see a row that can
  /// only ever hold `default`.
  pub fn multi_device(&self) -> bool {
    self.current_devices().len() > 1
  }

  /// Whether a row is currently shown / navigable. The Multi-GPU
  /// placement knobs (`device`, `tensor_split`, `main_gpu`,
  /// `split_mode`) are gated on [`Self::multi_device`]; knobs the
  /// model's backend can't honor are hidden outright — a Lemonade
  /// model shows just ctx and extras rather than a column of dead
  /// rows. Delegates to the single-source group table.
  pub fn field_visible(&self, field: PickerField) -> bool {
    match field {
      // Always shown — it offers `last used` ↔ `auto` even with no presets.
      PickerField::Preset => true,
      // Shown only with a real choice: two or more compatible servers.
      PickerField::Server => self.servers.len() > 1,
      PickerField::Knob(k) => self.knob_supported(k) && knob_row_visible(k, self.multi_device()),
      // A native row exists iff its index is within the backend's slice
      // (six for ds4, empty for llama.cpp / Lemonade).
      PickerField::NativeKnob(i) => i < self.native_descriptors.len(),
      // The MTP row shows only for an MTP-capable model (any backend).
      PickerField::Mtp => self.mtp_capable,
      PickerField::Extras => true,
    }
  }

  /// The MTP row's value label (`auto`/`on`/`off`) for the Settings render.
  pub fn mtp_value_label(&self) -> &'static str {
    self.mtp.label()
  }

  /// Move cursor to the next visible row.
  pub fn next_field(&mut self) {
    self.step_field(true);
  }

  /// Move cursor to the previous visible row.
  pub fn prev_field(&mut self) {
    self.step_field(false);
  }

  /// Advance the cursor one step in `forward`/back direction, skipping
  /// any hidden rows (e.g. `device` on single-GPU hosts).
  fn step_field(&mut self, forward: bool) {
    // Native rows trail the static base; for shipping backends the slice is
    // empty so this is byte-identical to `PickerField::all()`.
    let all = self.ordered_fields();
    let Some(i) = all.iter().position(|f| *f == self.field) else {
      return;
    };
    let n = all.len();
    for step in 1..=n {
      let idx = if forward {
        (i + step) % n
      } else {
        (i + n - step) % n
      };
      if self.field_visible(all[idx]) {
        self.field = all[idx];
        return;
      }
    }
  }

  /// True when the focused row is cyclable (Up/Down would change
  /// the value). `Extras` is non-cyclable; the rest are.
  pub fn focused_field_is_cyclable(&self) -> bool {
    !matches!(self.field, PickerField::Extras)
  }

  /// Read the value the editor row should display, taking the user
  /// override first and the resolver-chain value otherwise.
  pub fn effective_u32(&self, field: KnobField) -> Option<u32> {
    self.user_value_u32(field).or(self.resolved_u32(field))
  }

  pub fn effective_f32(&self, field: KnobField) -> Option<f32> {
    self.user_value_f32(field).or(self.resolved_f32(field))
  }

  pub fn effective_str(&self, field: KnobField) -> Option<String> {
    self
      .user_value_str(field)
      .map(str::to_string)
      .or_else(|| self.resolved_str(field).map(str::to_string))
  }

  pub fn effective_bool(&self, field: KnobField) -> Option<bool> {
    self.user_value_bool(field).or(self.resolved_bool(field))
  }

  /// Source label for `field`. Returns `LayerLabel::User` when the
  /// user has an explicit override; falls back to the resolver's
  /// source map otherwise, then to the spec's `fallback_label` when
  /// the resolver hasn't populated the map yet (freshly-opened
  /// editor before the first resolve).
  pub fn source_for(&self, field: KnobField) -> LayerLabel {
    if self.user_has(field) {
      LayerLabel::User
    } else {
      self
        .sources
        .get(&field)
        .copied()
        .unwrap_or_else(|| crate::launch::flag_aliases::spec_for(field).fallback_label)
    }
  }

  /// Whether the row's *effective* state is `Auto` — either the user
  /// cycled it to Auto, or (untouched) it resolved to Auto via the
  /// seeding rule / a remembered Auto. Drives the `Auto` value label.
  pub fn effective_is_auto(&self, field: KnobField) -> bool {
    if self.user_has(field) {
      self.user_is_auto(field)
    } else {
      crate::launch::field_is_auto(&self.resolved, field)
    }
  }

  fn user_has(&self, field: KnobField) -> bool {
    self.user_knobs.slot(field).is_some()
  }

  fn user_value_u32(&self, field: KnobField) -> Option<u32> {
    self.user_knobs.slot(field).as_u32()
  }

  fn user_value_f32(&self, field: KnobField) -> Option<f32> {
    self.user_knobs.slot(field).as_f32()
  }

  fn user_value_str(&self, field: KnobField) -> Option<&str> {
    self.user_knobs.slot(field).as_str()
  }

  fn user_value_bool(&self, field: KnobField) -> Option<bool> {
    self.user_knobs.slot(field).as_bool()
  }

  fn resolved_u32(&self, field: KnobField) -> Option<u32> {
    self.resolved.slot(field).as_u32()
  }

  fn resolved_f32(&self, field: KnobField) -> Option<f32> {
    self.resolved.slot(field).as_f32()
  }

  fn resolved_str(&self, field: KnobField) -> Option<&str> {
    self.resolved.slot(field).as_str()
  }

  fn resolved_bool(&self, field: KnobField) -> Option<bool> {
    self.resolved.slot(field).as_bool()
  }

  pub fn set_user_u32(&mut self, field: KnobField, value: Option<u32>) {
    // An explicit value sets `Set(v)`; clearing (`None`) drops the slot
    // back to Inherited. A non-`u32` field is a no-op. The Auto state is
    // set via the cycle helpers, not this typed-value setter.
    self.user_knobs.slot_mut(field).set_u32(value);
  }

  pub fn set_user_f32(&mut self, field: KnobField, value: Option<f32>) {
    self.user_knobs.slot_mut(field).set_f32(value);
  }

  pub fn set_user_str(&mut self, field: KnobField, value: Option<String>) {
    self.user_knobs.slot_mut(field).set_str(value);
  }

  pub fn set_user_bool(&mut self, field: KnobField, value: Option<bool>) {
    self.user_knobs.slot_mut(field).set_bool(value);
  }

  fn clear_user(&mut self, field: KnobField) {
    self.user_knobs.slot_mut(field).clear();
  }

  /// Display for the Device row. With >1 device (the only case the row is
  /// shown) it renders like every other cyclable knob — a single stop the
  /// ◀ ▶ arrows wrap — but each stop is a GPU with a `[x]`/`[ ]` checkbox in
  /// front: `[x] Vulkan1  ·  2 of 3`. `←/→` cycle which GPU is shown (walk the
  /// cursor); `Space` toggles its box. The `· N of M` / `· all` suffix keeps
  /// the whole-selection count visible while only one stop shows. Below two
  /// devices it degrades to the plain `"<name> (<backend>)"` / `"inherited"`
  /// label.
  pub fn device_value_display(&self) -> String {
    let devices = self.current_devices();
    if devices.is_empty() {
      return INHERITED_LABEL.into();
    }
    if devices.len() < 2 {
      let sel = self
        .effective_str(KnobField::Device)
        .filter(|v| !v.is_empty());
      return sel
        .map(|s| match devices.iter().find(|d| d.selector == s) {
          Some(d) => format!("{} ({})", d.name, d.gpu_backend),
          None => s,
        })
        .unwrap_or_else(|| INHERITED_LABEL.into());
    }
    let picked: std::collections::HashSet<String> = self.device_selection().into_iter().collect();
    let cursor = self.device_cursor.min(devices.len() - 1);
    let d = &devices[cursor];
    let ticked = if picked.contains(&d.selector) {
      "[x]"
    } else {
      "[ ]"
    };
    let summary = if picked.len() == devices.len() {
      "all".to_string()
    } else {
      format!("{} of {}", picked.len(), devices.len())
    };
    format!("{ticked} {}  ·  {summary}", d.selector)
  }
}

/// Cycle through `presets` from `current`. Behaviour by case:
///
/// - **`current == None`** (row is on the inherited default): wrap to
///   the first preset (forward) or the last preset (backward).
/// - **`current` matches a preset exactly**: advance / reverse one
///   slot. Falling off either end wraps back to `None` so the row
///   re-inherits.
/// - **`current` sits between presets** (e.g. user typed a custom
///   value via `e`): snap to the nearest preset *in the chosen
///   direction* — pressing `→` jumps to the smallest preset strictly
///   greater than `current`; pressing `←` jumps to the largest one
///   strictly less. This keeps cycling consistent with the visible
///   direction of travel; the previous behaviour of jumping to
///   `presets[0]` was a footgun on custom values mid-list.
///
/// `presets` is assumed to be sorted in ascending order — every
/// caller in [`LaunchPickerState::cycle_knob`] passes a hand-curated
/// ascending list.
/// A knob's position in the Auto-aware cycle ring:
/// `Inherited → Auto → Set(preset)… → wrap`.
enum CycleState<T> {
  /// `None` knob — inherits from the resolver chain.
  Inherited,
  /// Delegated to `--fit`.
  Auto,
  /// Pinned to a concrete value.
  Set(T),
}

/// Step a value knob one slot around the ring `Inherited → Auto →
/// preset[0] → … → preset[last] → Inherited`. Custom (off-preset)
/// values snap to the nearest preset in the travel direction via
/// [`cycle_through`]; falling off the top end lands on `Inherited`,
/// off the bottom on `Auto`.
fn ring_next<T: PartialEq + PartialOrd + Copy>(
  current: CycleState<T>,
  presets: &[T],
  forward: bool,
  allow_auto: bool,
) -> CycleState<T> {
  // Non-fit-governed knobs have no Auto stop: a two-stop ring
  // `Inherited → presets… → Inherited`. A stray Auto (e.g. a stale
  // persisted value) coerces back to Inherited so cycling escapes it.
  if !allow_auto {
    return match current {
      CycleState::Auto => CycleState::Inherited,
      CycleState::Inherited => {
        cycle_through(None, presets, forward).map_or(CycleState::Inherited, CycleState::Set)
      }
      CycleState::Set(v) => {
        cycle_through(Some(v), presets, forward).map_or(CycleState::Inherited, CycleState::Set)
      }
    };
  }
  match current {
    CycleState::Inherited => {
      if forward {
        CycleState::Auto
      } else {
        // Backward from Inherited wraps to the last preset.
        cycle_through(None, presets, false).map_or(CycleState::Auto, CycleState::Set)
      }
    }
    CycleState::Auto => {
      if forward {
        cycle_through(None, presets, true).map_or(CycleState::Inherited, CycleState::Set)
      } else {
        CycleState::Inherited
      }
    }
    CycleState::Set(v) => match cycle_through(Some(v), presets, forward) {
      Some(p) => CycleState::Set(p),
      // Off the top → Inherited; off the bottom → Auto.
      None => {
        if forward {
          CycleState::Inherited
        } else {
          CycleState::Auto
        }
      }
    },
  }
}

fn cycle_through<T: PartialEq + PartialOrd + Copy>(
  current: Option<T>,
  presets: &[T],
  forward: bool,
) -> Option<T> {
  if presets.is_empty() {
    return None;
  }
  match current {
    None => Some(if forward {
      presets[0]
    } else {
      presets[presets.len() - 1]
    }),
    Some(v) => {
      if let Some(i) = presets.iter().position(|p| *p == v) {
        return if forward {
          if i + 1 >= presets.len() {
            None
          } else {
            Some(presets[i + 1])
          }
        } else if i == 0 {
          None
        } else {
          Some(presets[i - 1])
        };
      }
      // Off-preset custom value: snap to the nearest preset in the
      // direction the user pressed. Falls back to first/last when
      // every preset sits on the other side of `current` (e.g. user
      // typed something smaller than `presets[0]` then pressed ←).
      if forward {
        presets
          .iter()
          .find(|p| **p > v)
          .copied()
          .or(Some(presets[presets.len() - 1]))
      } else {
        presets
          .iter()
          .rev()
          .find(|p| **p < v)
          .copied()
          .or(Some(presets[0]))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::useless_conversion)]
  use super::*;
  use crate::config::KnobValue;

  #[test]
  fn cycle_ctx_walks_through_presets_then_returns_to_native() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Ctx);
    assert_eq!(s.user_knobs.ctx, None);
    // Ring: Inherited → Auto → presets… → wrap to Inherited.
    s.cycle_focused_value_next();
    assert_eq!(
      s.user_knobs.ctx,
      Some(KnobValue::Auto),
      "first stop is Auto"
    );
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(CTX_PRESETS[0])));
    for preset in CTX_PRESETS.iter().skip(1) {
      s.cycle_focused_value_next();
      assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(*preset)));
    }
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.ctx, None, "wraps back to inherited");
  }

  #[test]
  fn ctx_presets_gate_to_native_window() {
    let mut s = LaunchPickerState::for_model("m");
    // Unknown native window → the full ladder (all quick-picks up to 1 Mi).
    assert_eq!(s.ctx_presets(), CTX_PRESETS.to_vec());
    // 128k model → capped at 131072; no 256k / 512k / 1M offered.
    s.native_ctx = Some(131072);
    assert_eq!(*s.ctx_presets().last().unwrap(), 131072);
    assert!(!s.ctx_presets().contains(&262144));
    // 256k model → reaches the new 262144 preset but not 524288.
    s.native_ctx = Some(262144);
    assert_eq!(*s.ctx_presets().last().unwrap(), 262144);
    assert!(!s.ctx_presets().contains(&524288));
    // 1M model → the whole ladder, 1 Mi included.
    s.native_ctx = Some(1048576);
    assert_eq!(s.ctx_presets(), CTX_PRESETS.to_vec());
    assert!(s.ctx_presets().contains(&1048576));
    // A window below the smallest preset yields no quick-picks (type-only).
    s.native_ctx = Some(1024);
    assert!(s.ctx_presets().is_empty());
  }

  #[test]
  fn cycle_ctx_caps_at_native_window() {
    // A 128k model must never cycle past 131072 into the 256k+ presets.
    let mut s = LaunchPickerState::for_model("m");
    s.native_ctx = Some(131072);
    s.field = PickerField::Knob(KnobField::Ctx);
    let gated = s.ctx_presets();
    s.cycle_focused_value_next(); // Inherited → Auto
    for preset in &gated {
      s.cycle_focused_value_next();
      assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(*preset)));
    }
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.ctx, None, "wraps at the native cap, no 256k+");
  }

  #[test]
  fn reasoning_cycle_walks_tri_state_in_both_directions() {
    // `reasoning` is not fit-governed, so it has no `Auto` stop: the
    // ring is Inherited → on → off → Inherited.
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Reasoning);
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(true)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(false)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.reasoning, None);
    // Backward from Inherited lands on off (the far end of the ring).
    s.cycle_focused_value_prev();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(false)));
    s.cycle_focused_value_prev();
    assert_eq!(s.user_knobs.reasoning, Some(KnobValue::Set(true)));
    s.cycle_focused_value_prev();
    assert_eq!(s.user_knobs.reasoning, None);
  }

  #[test]
  fn gguf_model_honors_every_llamacpp_knob() {
    // Default model_backend is LlamaCpp (a GGUF row).
    let s = LaunchPickerState::for_model("qwen");
    assert!(s.knob_supported(KnobField::Ctx));
    assert_eq!(s.active_backend_id(), "llamacpp");
  }

  #[test]
  fn mtp_row_shown_only_when_capable_and_cycles() {
    use crate::launch::params::MtpEnable;
    let mut s = LaunchPickerState::for_model("qwen");
    // Not MTP-capable → row hidden, not in the ordered field list.
    assert!(!s.field_visible(PickerField::Mtp));
    assert!(!s.ordered_fields().contains(&PickerField::Mtp));
    // MTP-capable → row shown, spliced before Extras.
    s.mtp_capable = true;
    assert!(s.field_visible(PickerField::Mtp));
    let ordered = s.ordered_fields();
    let mtp_at = ordered.iter().position(|f| *f == PickerField::Mtp).unwrap();
    let extras_at = ordered
      .iter()
      .position(|f| *f == PickerField::Extras)
      .unwrap();
    assert!(mtp_at < extras_at, "MTP row sits before Extras");
    // Cycling the focused MTP row walks auto → on → off → auto.
    s.field = PickerField::Mtp;
    assert_eq!(s.mtp, MtpEnable::Auto);
    s.cycle_focused_value_next();
    assert_eq!(s.mtp, MtpEnable::On);
    s.cycle_focused_value_next();
    assert_eq!(s.mtp, MtpEnable::Off);
    s.cycle_focused_value_next();
    assert_eq!(s.mtp, MtpEnable::Auto);
    // Backspace resets to auto.
    s.mtp = MtpEnable::On;
    s.reset_focused_row();
    assert_eq!(s.mtp, MtpEnable::Auto);
  }

  #[test]
  fn lemonade_model_shows_only_ctx_and_extras() {
    let mut s = LaunchPickerState::for_model("Qwen2.5-7B");
    s.model_backend = BackendChoice::Explicit("lemonade".into());
    // Lemonade honors `ctx` (lemond's `ctx_size` load option) and the
    // free-form extras (`*_args`) — every other llama.cpp knob row is
    // hidden outright. There is no Backend row: a model lives in exactly
    // one backend's catalog, so there is nothing to choose.
    let visible: Vec<PickerField> = PickerField::all()
      .iter()
      .copied()
      .filter(|f| s.field_visible(*f))
      .collect();
    assert_eq!(
      visible,
      vec![
        PickerField::Preset,
        PickerField::Knob(KnobField::Ctx),
        PickerField::Extras
      ],
      "lemonade picker is preset + ctx + extras, nothing else"
    );
    assert_eq!(s.active_backend_id(), "lemonade");
  }

  #[test]
  fn next_field_iterates_every_visible_picker_row() {
    let mut s = LaunchPickerState::for_model("qwen");
    // A single server with 2 devices makes the `device` row visible so
    // navigation visits every row (the Server row stays hidden with one
    // server). The single-GPU skip is covered separately.
    s.servers = one_server(vec![
      device("CUDA0", "CUDA", "GPU 0"),
      device("CUDA1", "CUDA", "GPU 1"),
    ]);
    // Visible rows in nav order. The Backend chooser is hidden with a single
    // concrete backend, so it's skipped (covered by `field_visible`).
    let visible: Vec<PickerField> = PickerField::all()
      .iter()
      .copied()
      .filter(|f| s.field_visible(*f))
      .collect();
    assert!(
      visible.len() > 14,
      "should cover every typed knob (ctx, reasoning, offload, placement, …) + extras"
    );
    let start_idx = visible
      .iter()
      .position(|f| *f == s.field)
      .expect("the initial field is visible");
    for step in 1..=visible.len() {
      s.next_field();
      assert_eq!(s.field, visible[(start_idx + step) % visible.len()]);
    }
  }

  #[test]
  fn navigation_skips_multi_gpu_rows_on_single_gpu() {
    // Default picker has an empty catalog → the whole Multi-GPU
    // placement group is hidden, so neither next_field nor prev_field
    // ever lands on any of its rows.
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(!s.multi_device());
    let hidden = [
      KnobField::Device,
      KnobField::TensorSplit,
      KnobField::MainGpu,
      KnobField::SplitMode,
    ];
    let n = PickerField::all().len();
    for _ in 0..n {
      s.next_field();
      for f in hidden {
        assert_ne!(s.field, PickerField::Knob(f), "landed on hidden {f:?}");
      }
    }
    for _ in 0..n {
      s.prev_field();
      for f in hidden {
        assert_ne!(s.field, PickerField::Knob(f), "landed on hidden {f:?}");
      }
    }
  }

  #[test]
  fn cycle_knob_n_gpu_layers_walks_presets() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::NGpuLayers);
    s.cycle_focused_value_next();
    assert_eq!(
      s.user_knobs.n_gpu_layers,
      Some(KnobValue::Auto),
      "Auto stop first"
    );
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.n_gpu_layers, Some(KnobValue::Set(0)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.n_gpu_layers, Some(KnobValue::Set(16)));
  }

  #[test]
  fn cycle_knob_flash_attn_walks_tristate() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::FlashAttn);
    // flash_attn is not fit-governed → no Auto stop. Ring:
    // Inherited → on → off → Inherited.
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, Some(KnobValue::Set(true)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, Some(KnobValue::Set(false)));
    s.cycle_focused_value_next();
    assert_eq!(s.user_knobs.flash_attn, None);
  }

  #[test]
  fn effective_is_auto_tracks_user_then_resolved_state() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(
      !s.effective_is_auto(KnobField::Ctx),
      "fresh knob is not Auto"
    );
    // A user-cycled Auto reads as Auto.
    s.set_user_auto(KnobField::Ctx);
    assert!(s.effective_is_auto(KnobField::Ctx));
    // An untouched knob reflects the resolved (seeded / remembered) Auto.
    s.set_resolved(
      TypedKnobs {
        n_gpu_layers: Some(KnobValue::Auto),
        ..TypedKnobs::default()
      },
      BTreeMap::new(),
    );
    assert!(
      s.effective_is_auto(KnobField::NGpuLayers),
      "resolved Auto shows when the user hasn't overridden the row"
    );
  }

  #[test]
  fn reset_focused_row_clears_user_override() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = PickerField::Knob(KnobField::Threads);
    s.cycle_focused_value_next();
    assert!(s.user_knobs.threads.is_some());
    s.reset_focused_row();
    assert!(s.user_knobs.threads.is_none());
  }

  #[test]
  fn source_for_falls_through_to_resolver_when_no_user_override() {
    let mut s = LaunchPickerState::for_model("qwen");
    let mut sources = BTreeMap::new();
    sources.insert(KnobField::NGpuLayers, LayerLabel::ArchDefault);
    s.set_resolved(
      TypedKnobs {
        n_gpu_layers: Some(KnobValue::Set(99)),
        ..TypedKnobs::default()
      },
      sources,
    );
    assert_eq!(s.source_for(KnobField::NGpuLayers), LayerLabel::ArchDefault);
    // User override flips the source to User.
    s.user_knobs.n_gpu_layers = Some(KnobValue::Set(32));
    assert_eq!(s.source_for(KnobField::NGpuLayers), LayerLabel::User);
  }

  #[test]
  fn cycle_through_starts_at_first_preset_when_current_is_none() {
    assert_eq!(cycle_through::<u32>(None, &[1, 2, 3], true), Some(1));
    assert_eq!(cycle_through::<u32>(None, &[1, 2, 3], false), Some(3));
  }

  #[test]
  fn cycle_through_wraps_to_none_at_the_end() {
    assert_eq!(cycle_through::<u32>(Some(3), &[1, 2, 3], true), None);
    assert_eq!(cycle_through::<u32>(Some(1), &[1, 2, 3], false), None);
  }

  #[test]
  fn cycle_through_off_preset_snaps_to_nearest_in_direction() {
    // User typed `n_gpu_layers=42` via `e`, then presses →.
    let presets = &[0, 16, 32, 64, 99];
    assert_eq!(cycle_through(Some(42_u32), presets, true), Some(64));
    assert_eq!(cycle_through(Some(42_u32), presets, false), Some(32));
  }

  #[test]
  fn cycle_through_off_preset_below_first_snaps_to_first_going_forward() {
    let presets = &[10, 20, 30];
    // Forward from a value below presets[0] → first preset > current = 10.
    assert_eq!(cycle_through(Some(5_u32), presets, true), Some(10));
    // Backward from below presets[0] has nothing smaller → fall back
    // to first preset.
    assert_eq!(cycle_through(Some(5_u32), presets, false), Some(10));
  }

  #[test]
  fn cycle_through_off_preset_above_last_snaps_to_last_going_backward() {
    let presets = &[10, 20, 30];
    assert_eq!(cycle_through(Some(99_u32), presets, false), Some(30));
    // Forward from above presets[last] has nothing greater → fall back
    // to last preset.
    assert_eq!(cycle_through(Some(99_u32), presets, true), Some(30));
  }

  // ---- Server-scoped device picker tests ----

  use crate::backend::{Device, Server};

  /// Build a neutral [`Device`] for tests. Memory fields don't affect the
  /// picker logic, so they're left `None`.
  fn device(selector: &str, gpu: &str, name: &str) -> Device {
    Device {
      selector: selector.into(),
      gpu_backend: gpu.into(),
      name: name.into(),
      total_mib: None,
      free_mib: None,
    }
  }

  /// A server for tests.
  fn server(id: &str, backend: &str, binary: &str, devices: Vec<Device>) -> Server {
    Server {
      id: id.into(),
      backend_id: backend.into(),
      binary: std::path::PathBuf::from(binary),
      name: id.into(),
      devices,
    }
  }

  /// A single llama.cpp server carrying `devices` — the common single-server
  /// case, so `current_server()` (with no explicit pick) scopes to it.
  fn one_server(devices: Vec<Device>) -> Vec<Server> {
    vec![server(
      "llamacpp-test",
      "llamacpp",
      "/test/llama-server",
      devices,
    )]
  }

  /// One server with the three-device mixed-vendor catalog the device tests
  /// exercise (Vulkan0/Vulkan1/ROCm0 on one build).
  fn catalog_two_vendors() -> Vec<Server> {
    one_server(vec![
      device("Vulkan0", "Vulkan", "AMD Radeon AI PRO R9700"),
      device("Vulkan1", "Vulkan", "NVIDIA GeForce RTX 3080"),
      device("ROCm0", "ROCm", "AMD Radeon AI PRO R9700"),
    ])
  }

  #[test]
  fn device_value_display_single_device_shows_name_and_backend() {
    // Below two devices the row degrades to the plain name label (and is
    // hidden in navigation) — the checkbox view only appears with >1 GPU.
    let mut s = LaunchPickerState::for_model("test");
    s.servers = one_server(vec![device("ROCm0", "ROCm", "AMD Radeon AI PRO R9700")]);
    assert_eq!(s.device_value_display(), "inherited");
    s.set_user_str(KnobField::Device, Some("ROCm0".into()));
    assert_eq!(s.device_value_display(), "AMD Radeon AI PRO R9700 (ROCm)");
  }

  #[test]
  fn device_value_display_shows_cursor_stop_checkbox_and_count() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    // Unset == all GPUs → cursor on the first, its box ticked, `· all` count.
    assert_eq!(s.device_value_display(), "[x] Vulkan0  ·  all");
    // Toggle the cursor GPU off → explicit N-1 set, cursor stays on it, the
    // count drops to `2 of 3`.
    s.toggle_focused_device();
    assert_eq!(s.device_value_display(), "[ ] Vulkan0  ·  2 of 3");
  }

  #[test]
  fn walk_device_cursor_moves_and_wraps() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    s.field = PickerField::Knob(KnobField::Device);
    // ←/→ walk the cursor without changing the selection — one stop shown.
    s.cycle_focused_value_next();
    assert_eq!(s.device_value_display(), "[x] Vulkan1  ·  all");
    s.cycle_focused_value_next();
    assert_eq!(s.device_value_display(), "[x] ROCm0  ·  all");
    s.cycle_focused_value_next(); // wrap back to the first
    assert_eq!(s.device_value_display(), "[x] Vulkan0  ·  all");
    // Selection untouched by walking.
    assert_eq!(s.user_value_str(KnobField::Device), None);
  }

  #[test]
  fn toggle_device_builds_catalog_ordered_selection() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    // Move cursor to ROCm0 (index 2) and toggle it off.
    s.field = PickerField::Knob(KnobField::Device);
    s.cycle_focused_value_next();
    s.cycle_focused_value_next();
    s.toggle_focused_device();
    // Persisted string keeps catalog order (Vulkan0, Vulkan1), not toggle order.
    assert_eq!(
      s.user_value_str(KnobField::Device),
      Some("Vulkan0,Vulkan1".into())
    );
  }

  #[test]
  fn toggle_all_devices_back_on_normalizes_to_unset() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    s.field = PickerField::Knob(KnobField::Device);
    // Toggle the cursor GPU off, then on again → full set → unset (llama.cpp
    // default: no `--device`).
    s.toggle_focused_device();
    assert_eq!(
      s.user_value_str(KnobField::Device),
      Some("Vulkan1,ROCm0".into())
    );
    s.toggle_focused_device();
    assert_eq!(s.user_value_str(KnobField::Device), None);
  }

  #[test]
  fn toggle_last_device_off_normalizes_to_unset() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    s.field = PickerField::Knob(KnobField::Device);
    // Pin a single device, then toggle it off → empty is invalid → unset.
    s.set_user_str(KnobField::Device, Some("Vulkan0".into()));
    s.toggle_focused_device(); // cursor at 0 = Vulkan0
    assert_eq!(s.user_value_str(KnobField::Device), None);
  }

  #[test]
  fn toggle_device_with_empty_catalog_is_a_no_op() {
    let mut s = LaunchPickerState::for_model("test");
    // No servers (CPU-only / no binary) — nothing to toggle.
    s.toggle_focused_device();
    assert_eq!(s.user_value_str(KnobField::Device), None);
  }

  // ---- Server selection row ----

  /// Two llama.cpp builds (rocm + vulkan) — the multi-server case that makes
  /// the Server row selectable.
  fn two_llamacpp_servers() -> Vec<Server> {
    vec![
      server(
        "llamacpp-rocm",
        "llamacpp",
        "/rocm/llama-server",
        vec![device("ROCm0", "ROCm", "AMD Radeon AI PRO R9700")],
      ),
      server(
        "llamacpp-vulkan",
        "llamacpp",
        "/vk/llama-server",
        vec![
          device("Vulkan0", "Vulkan", "AMD Radeon AI PRO R9700"),
          device("Vulkan1", "Vulkan", "NVIDIA GeForce RTX 3080"),
        ],
      ),
    ]
  }

  #[test]
  fn server_row_hidden_with_zero_or_one_server() {
    let mut s = LaunchPickerState::for_model("m");
    assert!(!s.field_visible(PickerField::Server), "hidden with none");
    s.servers = one_server(vec![device("ROCm0", "ROCm", "card")]);
    assert!(!s.field_visible(PickerField::Server), "hidden with one");
    s.servers = two_llamacpp_servers();
    assert!(s.field_visible(PickerField::Server), "shown with two");
  }

  #[test]
  fn server_row_default_names_priority_default_then_cycles_non_default_ids() {
    let mut s = LaunchPickerState::for_model("m");
    s.servers = two_llamacpp_servers();
    s.field = PickerField::Server;
    // Unset → `<first server id> (default)` (the id leads, `(default)` trails).
    assert_eq!(s.selected_server, None);
    assert_eq!(s.server_value_label(), "llamacpp-rocm (default)");
    // Ring dedupes servers[0] into the default: default → vulkan → wrap.
    // (No separate `llamacpp-rocm` stop — pinning it == the default.)
    s.cycle_focused_value_next();
    assert_eq!(s.selected_server.as_deref(), Some("llamacpp-vulkan"));
    assert_eq!(s.server_value_label(), "llamacpp-vulkan");
    s.cycle_focused_value_next();
    assert_eq!(s.selected_server, None, "wraps back to default");
  }

  #[test]
  fn pinning_the_first_server_reads_as_the_default() {
    // last_params may persist an explicit `servers[0]` pick — it must render as
    // the default (folded), and cycling from it advances to the next server.
    let mut s = LaunchPickerState::for_model("m");
    s.servers = two_llamacpp_servers();
    s.field = PickerField::Server;
    s.selected_server = Some("llamacpp-rocm".into());
    assert_eq!(s.server_value_label(), "llamacpp-rocm (default)");
    s.cycle_focused_value_next();
    assert_eq!(s.selected_server.as_deref(), Some("llamacpp-vulkan"));
  }

  #[test]
  fn selected_server_scopes_the_device_list() {
    let mut s = LaunchPickerState::for_model("m");
    s.servers = two_llamacpp_servers();
    // Default scopes to servers[0] (rocm, 1 device) → device row hidden.
    assert_eq!(s.current_devices().len(), 1);
    assert!(!s.multi_device());
    // Pick the vulkan build (2 devices) → device row appears.
    s.selected_server = Some("llamacpp-vulkan".into());
    assert_eq!(s.current_devices().len(), 2);
    assert!(s.multi_device());
  }

  #[test]
  fn switching_server_clears_the_device_pick() {
    let mut s = LaunchPickerState::for_model("m");
    s.servers = two_llamacpp_servers();
    s.field = PickerField::Server;
    s.selected_server = Some("llamacpp-vulkan".into());
    s.set_user_str(KnobField::Device, Some("Vulkan1".into()));
    // Cycling the server row invalidates the (now-foreign) device selector.
    s.cycle_focused_value_next();
    assert_eq!(s.user_value_str(KnobField::Device), None);
  }

  #[test]
  fn selected_server_backend_swaps_the_knob_set() {
    // A deepseek4-style model with a ds4 server and a llamacpp server.
    let mut s = LaunchPickerState::for_model("DeepSeek-V4-Flash");
    s.model_backend = BackendChoice::Explicit("ds4".into());
    s.servers = vec![
      server("ds4", "ds4", "/ds4/ds4-server", vec![]),
      server(
        "llamacpp-rocm",
        "llamacpp",
        "/rocm/llama-server",
        vec![device("ROCm0", "ROCm", "card")],
      ),
    ];
    s.field = PickerField::Server;
    // Unset → the model's own backend with its native knob set.
    s.seed_native_descriptors();
    assert_eq!(s.active_backend_id(), "ds4");
    assert_eq!(s.native_descriptors.len(), 9);
    // Pick the llamacpp server → knob set swaps to llama.cpp (no native rows).
    s.selected_server = Some("llamacpp-rocm".into());
    s.seed_native_descriptors();
    assert_eq!(s.active_backend_id(), "llamacpp");
    assert!(s.native_descriptors.is_empty());
    assert!(s.knob_supported(KnobField::NGpuLayers));
  }

  /// Regression net for the silent-edit-loss class: a user value set on
  /// any `KnobField` must read back through the picker. A missing
  /// accessor arm (the bug the old wildcard `_ => None` masked) fails
  /// here for the affected variant.
  #[test]
  fn picker_round_trips_a_user_edit_for_every_knob_field() {
    use crate::launch::flag_aliases::{knob_specs, ValueKind};
    for spec in knob_specs() {
      let mut s = LaunchPickerState::for_model("qwen");
      let field = spec.field;
      match spec.kind {
        ValueKind::U32 => {
          s.set_user_u32(field, Some(7));
          assert_eq!(s.user_value_u32(field), Some(7), "{field:?} u32 round-trip");
        }
        ValueKind::F32 => {
          s.set_user_f32(field, Some(2.5));
          assert_eq!(
            s.user_value_f32(field),
            Some(2.5),
            "{field:?} f32 round-trip"
          );
        }
        ValueKind::Bool => {
          s.set_user_bool(field, Some(true));
          assert_eq!(
            s.user_value_bool(field),
            Some(true),
            "{field:?} bool round-trip"
          );
        }
        ValueKind::KvCacheType | ValueKind::SplitMode | ValueKind::Str => {
          let v = match spec.kind {
            ValueKind::KvCacheType => "q8_0",
            ValueKind::SplitMode => "row",
            _ => "0",
          };
          s.set_user_str(field, Some(v.to_string()));
          assert_eq!(s.user_value_str(field), Some(v), "{field:?} str round-trip");
        }
      }
      assert!(s.user_has(field), "{field:?} should report a user override");
    }
  }

  #[test]
  fn untouched_picker_reports_no_user_override_for_any_knob() {
    use crate::launch::flag_aliases::knob_specs;
    let s = LaunchPickerState::for_model("qwen");
    for spec in knob_specs() {
      assert!(!s.user_has(spec.field), "{:?} must start unset", spec.field);
    }
  }

  // ---- preset cycle ----

  fn choice(name: &str, ctx: u32) -> PresetChoice {
    PresetChoice {
      name: name.into(),
      knobs: TypedKnobs {
        ctx: Some(KnobValue::Set(ctx)),
        ..TypedKnobs::default()
      },
      extras: Vec::new(),
      backend_knobs: BTreeMap::new(),
    }
  }

  #[test]
  fn no_named_presets_still_shows_row_with_last_used_and_auto() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.user_knobs.threads = Some(KnobValue::Set(8)); // a last-used baseline
    s.set_presets(Vec::new(), PresetStop::LastUsed);
    assert!(!s.has_presets(), "no named presets");
    assert!(s.field_visible(PickerField::Preset), "row shown anyway");
    // Cursor isn't pulled onto the preset row.
    assert_eq!(s.field, PickerField::Knob(KnobField::Ctx));
    // Unset default → `last used` is the default stop, marked accordingly.
    assert_eq!(s.preset_stop, PresetStop::LastUsed);
    assert_eq!(s.preset_value_label(), "last used (default)");
    assert_eq!(
      s.user_knobs.threads,
      Some(KnobValue::Set(8)),
      "baseline kept"
    );
    // Cycle the preset row: last used → auto → last used.
    s.field = PickerField::Preset;
    s.cycle_focused_value_next();
    assert_eq!(s.preset_value_label(), "auto");
    assert_eq!(s.user_knobs.threads, None, "auto clears non-fit knobs");
    assert_eq!(
      s.user_knobs.ctx,
      Some(KnobValue::Auto),
      "fit-governed → auto"
    );
    s.cycle_focused_value_next();
    assert_eq!(s.preset_value_label(), "last used (default)");
    assert_eq!(
      s.user_knobs.threads,
      Some(KnobValue::Set(8)),
      "baseline restored"
    );
  }

  #[test]
  fn set_presets_opens_on_configured_default() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.user_knobs.threads = Some(KnobValue::Set(8));
    s.set_presets(
      vec![choice("short", 8192), choice("long", 65536)],
      PresetStop::Named(1),
    );
    assert!(s.has_presets());
    assert!(s.field_visible(PickerField::Preset));
    // Opens on the configured default (long), marked `(default)`, with the
    // form seeded from that preset.
    assert_eq!(s.default_stop, PresetStop::Named(1));
    assert_eq!(s.preset_stop, PresetStop::Named(1));
    assert_eq!(s.preset_value_label(), "long (default)");
    assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(65536)));
  }

  #[test]
  fn auto_default_opens_on_auto_marked_default() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.set_presets(vec![choice("short", 8192)], PresetStop::Auto);
    assert_eq!(s.default_stop, PresetStop::Auto);
    assert_eq!(s.preset_stop, PresetStop::Auto);
    assert_eq!(s.preset_value_label(), "auto (default)");
    assert_eq!(s.user_knobs.ctx, Some(KnobValue::Auto));
  }

  #[test]
  fn cycle_ring_is_last_used_auto_named_and_reseeds() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.user_knobs.threads = Some(KnobValue::Set(8)); // last-used baseline
                                                    // Unset default → opens on `last used`.
    s.set_presets(
      vec![choice("short", 8192), choice("long", 65536)],
      PresetStop::LastUsed,
    );
    s.field = PickerField::Preset;
    assert_eq!(s.preset_value_label(), "last used (default)");
    assert_eq!(s.user_knobs.threads, Some(KnobValue::Set(8)));
    // → auto: fit-governed ctx → Auto, non-fit threads cleared.
    s.cycle_focused_value_next();
    assert_eq!(s.preset_stop, PresetStop::Auto);
    assert_eq!(s.preset_value_label(), "auto");
    assert_eq!(s.user_knobs.threads, None);
    assert_eq!(s.user_knobs.ctx, Some(KnobValue::Auto));
    // → short (Named 0). No separate Default stop in the ring.
    s.cycle_focused_value_next();
    assert_eq!(s.preset_value_label(), "short");
    assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(8192)));
    // → long (Named 1).
    s.cycle_focused_value_next();
    assert_eq!(s.preset_value_label(), "long");
    assert_eq!(s.user_knobs.ctx, Some(KnobValue::Set(65536)));
    // → wraps back to `last used (default)` (baseline threads restored).
    s.cycle_focused_value_next();
    assert_eq!(s.preset_stop, PresetStop::LastUsed);
    assert_eq!(s.preset_value_label(), "last used (default)");
    assert_eq!(s.user_knobs.threads, Some(KnobValue::Set(8)));
  }

  #[test]
  fn reset_on_preset_row_snaps_to_last_used() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.user_knobs.threads = Some(KnobValue::Set(4));
    s.set_presets(vec![choice("only", 4096)], PresetStop::LastUsed);
    s.field = PickerField::Preset;
    // Move off `last used` onto a preset, then reset.
    s.cycle_focused_value_next(); // auto
    s.cycle_focused_value_next(); // only
    assert_ne!(s.preset_stop, PresetStop::LastUsed);
    s.reset_focused_row();
    assert_eq!(s.preset_stop, PresetStop::LastUsed);
    assert_eq!(
      s.user_knobs.threads,
      Some(KnobValue::Set(4)),
      "baseline restored"
    );
  }

  #[test]
  fn out_of_range_default_index_falls_back_to_last_used() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.set_presets(vec![choice("a", 1)], PresetStop::Named(9));
    assert_eq!(s.default_stop, PresetStop::LastUsed);
    assert_eq!(s.preset_stop, PresetStop::LastUsed);
  }

  // ---- native knobs (test-only descriptor slice) ----

  /// A representative descriptor slice: one Cycle + one FreeText + one Bool.
  /// The mechanism is proven against this — no shipping backend returns knobs.
  const STUB_NATIVE: &[NativeKnobDescriptor] = &[
    NativeKnobDescriptor {
      id: "kv_bits",
      label: "KV bits",
      description: "",
      kind: NativeKnobKind::Cycle {
        presets: &["4", "8"],
      },
    },
    NativeKnobDescriptor {
      id: "adapter",
      label: "Adapter",
      description: "",
      kind: NativeKnobKind::FreeText,
    },
    NativeKnobDescriptor {
      id: "trust",
      label: "Trust remote",
      description: "",
      kind: NativeKnobKind::Bool,
    },
  ];

  #[test]
  fn shipping_picker_has_no_native_rows_and_byte_identical_nav() {
    // Default (no descriptors): nav order is exactly the static base, so the
    // picker is byte-identical for every shipping backend.
    let s = LaunchPickerState::for_model("m");
    assert!(s.native_descriptors.is_empty());
    assert_eq!(s.ordered_fields(), PickerField::all().to_vec());
    assert!(!s.field_visible(PickerField::NativeKnob(0)));
  }

  #[test]
  fn native_rows_precede_extras_in_descriptor_order() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_descriptors = STUB_NATIVE;
    let fields = s.ordered_fields();
    // Native rows sit in descriptor order just ahead of the trailing Extras.
    let tail = &fields[fields.len() - 4..];
    assert_eq!(
      tail,
      [
        PickerField::NativeKnob(0),
        PickerField::NativeKnob(1),
        PickerField::NativeKnob(2),
        PickerField::Extras,
      ]
    );
    assert!(s.field_visible(PickerField::NativeKnob(2)));
    assert!(!s.field_visible(PickerField::NativeKnob(3)), "out of range");
  }

  #[test]
  fn cycle_native_knob_walks_inherited_to_presets_and_wraps() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_descriptors = STUB_NATIVE;
    s.field = PickerField::NativeKnob(0); // Cycle { 4, 8 }
    assert_eq!(s.native_value_label(0), INHERITED_LABEL);
    s.cycle_focused_value_next();
    assert_eq!(s.backend_knobs["kv_bits"], KnobValue::Set("4".into()));
    assert_eq!(s.native_value_label(0), "4");
    s.cycle_focused_value_next();
    assert_eq!(s.backend_knobs["kv_bits"], KnobValue::Set("8".into()));
    s.cycle_focused_value_next();
    assert!(
      !s.backend_knobs.contains_key("kv_bits"),
      "wraps to inherited"
    );
  }

  #[test]
  fn cycle_native_bool_toggles_inherited_on_off() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_descriptors = STUB_NATIVE;
    s.field = PickerField::NativeKnob(2); // Bool
    s.cycle_focused_value_next();
    assert_eq!(s.backend_knobs["trust"], KnobValue::Set("true".into()));
    s.cycle_focused_value_next();
    assert_eq!(s.backend_knobs["trust"], KnobValue::Set("false".into()));
    s.cycle_focused_value_next();
    assert!(!s.backend_knobs.contains_key("trust"), "wraps to inherited");
  }

  #[test]
  fn cycle_native_backward_walks_the_ring_in_reverse() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_descriptors = STUB_NATIVE;
    // Cycle knob ←: inherited → last preset (8) → 4 → inherited.
    s.field = PickerField::NativeKnob(0);
    s.cycle_focused_value_prev();
    assert_eq!(s.backend_knobs["kv_bits"], KnobValue::Set("8".into()));
    s.cycle_focused_value_prev();
    assert_eq!(s.backend_knobs["kv_bits"], KnobValue::Set("4".into()));
    s.cycle_focused_value_prev();
    assert!(
      !s.backend_knobs.contains_key("kv_bits"),
      "wraps to inherited"
    );
    // Bool ←: inherited → off → on → inherited.
    s.field = PickerField::NativeKnob(2);
    s.cycle_focused_value_prev();
    assert_eq!(s.backend_knobs["trust"], KnobValue::Set("false".into()));
    s.cycle_focused_value_prev();
    assert_eq!(s.backend_knobs["trust"], KnobValue::Set("true".into()));
  }

  #[test]
  fn seed_native_descriptors_empty_for_llamacpp_lemonade_nine_for_ds4() {
    // llama.cpp / Lemonade declare no native knobs, so the picker stays
    // byte-identical for them; ds4 declares six. (A future backend with knobs
    // must extend the exhaustive `resolved_backend` match — a compile error
    // until it does.)
    for choice in [
      BackendChoice::Auto,
      BackendChoice::Explicit("llamacpp".into()),
      BackendChoice::Explicit("lemonade".into()),
    ] {
      let mut s = LaunchPickerState::for_model("m");
      s.model_backend = choice.clone();
      s.seed_native_descriptors();
      assert!(
        s.native_descriptors.is_empty(),
        "{choice:?} must surface no native knobs"
      );
    }
    let mut s = LaunchPickerState::for_model("m");
    s.model_backend = BackendChoice::Explicit("ds4".into());
    s.seed_native_descriptors();
    assert_eq!(
      s.native_descriptors.len(),
      9,
      "ds4 must surface its nine native knobs (6 base + the MTP trio)"
    );
  }

  #[test]
  fn native_freetext_edits_and_resets() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_descriptors = STUB_NATIVE;
    s.field = PickerField::NativeKnob(1); // FreeText
    assert!(s.focused_is_editable(), "free-text rows open `e`-edit");
    assert_eq!(s.native_buffer_seed(1), "", "empty when unset");
    s.set_native_text(1, "./lora");
    assert_eq!(s.backend_knobs["adapter"], KnobValue::Set("./lora".into()));
    assert_eq!(s.native_buffer_seed(1), "./lora");
    // Backspace / empty-commit clears.
    s.reset_focused_row();
    assert!(!s.backend_knobs.contains_key("adapter"));
    s.set_native_text(1, "x");
    s.set_native_text(1, "   "); // whitespace → clear
    assert!(!s.backend_knobs.contains_key("adapter"));
  }

  #[test]
  fn native_cycle_and_bool_rows_are_not_editable() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_descriptors = STUB_NATIVE;
    s.field = PickerField::NativeKnob(0); // Cycle
    assert!(!s.focused_is_editable());
    s.field = PickerField::NativeKnob(2); // Bool
    assert!(!s.focused_is_editable());
  }

  #[test]
  fn preset_cycle_seeds_and_restores_native_knobs() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_descriptors = STUB_NATIVE;
    // A native value set before presets are seeded becomes the `last used`
    // baseline.
    s.backend_knobs
      .insert("kv_bits".into(), KnobValue::Set("4".into()));
    let mut preset_bk = BTreeMap::new();
    preset_bk.insert("kv_bits".to_string(), KnobValue::Set("8".into()));
    // Open on `last used` (unset default) so the ring is
    // last used → auto → fast.
    s.set_presets(
      vec![PresetChoice {
        name: "fast".into(),
        knobs: TypedKnobs::default(),
        extras: Vec::new(),
        backend_knobs: preset_bk,
      }],
      PresetStop::LastUsed,
    );
    // Opens on `last used` → baseline native value restored.
    assert_eq!(s.backend_knobs["kv_bits"], KnobValue::Set("4".into()));
    // Ring: last used → auto → fast. `auto` drops native knobs.
    s.field = PickerField::Preset;
    s.cycle_focused_value_next();
    assert!(s.backend_knobs.is_empty(), "auto clears native knobs");
    // → named preset seeds its native value.
    s.cycle_focused_value_next();
    assert_eq!(s.backend_knobs["kv_bits"], KnobValue::Set("8".into()));
    // → wraps back to `last used`, restoring the baseline.
    s.cycle_focused_value_next();
    assert_eq!(s.backend_knobs["kv_bits"], KnobValue::Set("4".into()));
  }
}
