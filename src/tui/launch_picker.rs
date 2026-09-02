//! Launch picker form state — the knob editor.
//!
//! Every row below the two selector rows is **generated** from the active
//! backend's declarations ([`crate::launch::knobs`]): the group headers come
//! from [`Group::all`], the rows from what that backend declares in each group,
//! and each row's label, value shape, cycle ring and edit affordance from its
//! own [`KnobDef`]. Nothing here enumerates a knob, so a backend that declares
//! one gets an editor row for it and cannot be missing one.
//!
//! The form is a vertical list: the preset cycle, the server cycle, the knob
//! groups, and a free-text `extras` row last. Up/Down moves between rows;
//! Left/Right cycles the focused row's value; `e` enters inline edit; Enter
//! launches (or commits an open edit); Backspace resets the focused row.

use std::cell::Cell;
use std::collections::BTreeMap;

use crate::launch::knobs::{
  self, Group, KnobDef, KnobId, KnobKind, KnobSet, KnobValue, Ring, Scalar,
};
use crate::launch::params::{BackendChoice, LayerLabel};

/// Value-column label for a knob the user hasn't set — it inherits from
/// the resolver chain (last used / arch default / model default / server
/// default), named by the row's source chip. One constant so every
/// surface (picker form, running view, device row) agrees on the word.
pub const INHERITED_LABEL: &str = "inherited";

/// Which row the cursor is on. The editor renders top-to-bottom in
/// [`LaunchPickerState::ordered_fields`] order, so it doubles as the
/// vertical-navigation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerField {
  /// The preset cycle row, always shown at the very top of the form.
  /// Cycling it rewrites every knob row below live.
  Preset,
  /// The server (build/binary) cycle row, shown just under `Preset` when the
  /// model has more than one compatible server. Cycling it re-scopes the
  /// Device row + multi-GPU gating and, on a cross-backend switch, the whole
  /// knob set. Hidden with 0 or 1 server (nothing to pick).
  Server,
  /// One declared knob of the active backend, by its registry id.
  Knob(KnobId),
  Extras,
}

/// One stop on the picker's preset cycle. The ring is
/// `last used → auto → <named presets…>`. The model's configured default
/// is not a separate stop: it is whichever of these stops `default:`
/// resolves to (a named preset, `auto`, or — when unset — `last used`),
/// marked with a `(default)` suffix and opened on. Selecting a stop
/// rewrites the form's user knobs + extras: `LastUsed` restores the opening
/// baseline (the pre-filled last-used params), `Auto` delegates the
/// fit-governed knobs to the engine's fitter, and `Named` seeds from the
/// named preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetStop {
  LastUsed,
  Auto,
  Named(usize),
}

/// A named preset materialised for the picker: the knob set to seed and the
/// extras argv tail.
#[derive(Debug, Clone, PartialEq)]
pub struct PresetChoice {
  pub name: String,
  /// Every knob the preset pins — the backend's own tunables included, since
  /// there is no longer a separate native-knob channel to seed.
  pub knobs: KnobSet,
  pub extras: Vec<std::ffi::OsString>,
}

/// Inline-edit state owned by [`LaunchPickerState`].
///
/// The buffer and modal `editing` flag live in `inline_edit`
/// ([`crate::tui::input_field::InputField`]) so the knob editor shares the
/// `e:edit / Esc:walk-back / Enter:Submit` contract with every
/// other text input in the TUI. The wrapper carries the two extra
/// pieces of state `crate::tui::input_field::InputField` doesn't model:
///
/// - `field` — which `PickerField` the open edit is editing, so
///   `commit_inline_edit` knows where to write the parsed value.
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
  /// edit mode). Used by the event router to send keys to the input
  /// instead of the outer keymap.
  pub fn is_open(&self) -> bool {
    self.field.is_some() && self.input.is_editing()
  }
}

/// State of the launch picker.
#[derive(Debug, Clone)]
pub struct LaunchPickerState {
  /// Display name of the focused model (rendered in the title).
  pub model_name: String,
  /// The model's native (trained) context length, when known. Trims any
  /// [`Ring::UpToTrainedContext`] ladder so the editor never offers a window
  /// larger than the model supports; `None` leaves the full ladder available.
  pub native_ctx: Option<u32>,
  /// User-supplied knobs (only what the user explicitly set; every other knob
  /// stays absent and inherits from the resolved chain on render).
  pub user_knobs: KnobSet,
  /// Resolved knobs after applying the layered resolver — what the
  /// editor shows for each row.
  pub resolved: KnobSet,
  /// Per-knob source labels for the right-aligned origin chip.
  pub sources: BTreeMap<KnobId, LayerLabel>,
  /// Free-form argv tail forwarded to the engine.
  pub extras: Vec<std::ffi::OsString>,
  /// Modal text-input for the extras row. Shares the `e:edit /
  /// Esc:walk-back / Enter:Submit` contract with every other text input in
  /// the TUI.
  pub extras_input: crate::tui::input_field::InputField,
  /// Inline edit state for the knob rows.
  pub inline_edit: InlineEdit,
  pub field: PickerField,
  /// The focused model's *own* concrete backend (the one its catalog source
  /// binds to). A model lives in exactly one backend's catalog, so there is no
  /// user-facing chooser — this drives which knobs are declared, and is what
  /// the launch dispatches to. Never `Auto`.
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
  /// the cycle stops below `auto`. Empty for a model with no presets.
  pub presets: Vec<PresetChoice>,
  /// The cycle stop the model's `default:` resolves to — `Named(i)` for a
  /// configured named default, `Auto` for `default: auto`, or `LastUsed`
  /// when unset. The cycle opens here and the row marks it `(default)`.
  pub default_stop: PresetStop,
  /// Current cycle stop. The Preset row's value column renders this.
  pub preset_stop: PresetStop,
  /// Whether the focused model can speculate (an embedded draft head, or a
  /// `mtp-*.gguf` drafter sibling). Gates the whole [`Group::Speculation`]
  /// block through [`GroupGate::SpeculationCapable`](knobs::GroupGate) —
  /// backend-agnostic.
  pub mtp_capable: bool,
  /// The non-preset baseline (the build-time `user_knobs` / `extras` seed:
  /// last-used params, or empty). Restored when cycling back to `last used`.
  preset_baseline_knobs: KnobSet,
  preset_baseline_extras: Vec<std::ffi::OsString>,
  /// Row offset clipped from the top of the rendered line list so the
  /// focused row stays visible on small viewports. Recomputed on each
  /// render using the actual area height — the `Cell` lets the
  /// read-only render path (which only has `&App`) update the cached
  /// offset without taking a mutable borrow.
  pub scroll_offset: Cell<u16>,
}

/// The launch identity to pin on a saved preset, given a server pick, the
/// server catalog, and the model's backend scope. A non-default server pick
/// determines its own backend (its owning backend), so the two fields cannot
/// disagree; with no (non-default) pick the backend falls back to the model's
/// explicit choice (or `None` for `Auto`, letting the daemon re-derive).
///
/// Free (not a method) so the no-picker capture path can compute it from the
/// cheap sources (`last_params.server` + `compatible_servers` + the predicted
/// backend) without building a full [`LaunchPickerState`].
pub fn launch_identity(
  selected_server: &Option<String>,
  servers: &[crate::backend::Server],
  model_backend: &BackendChoice,
) -> (Option<String>, Option<String>) {
  // The default stop (unset, or an explicit pin of `servers[0]`) collapses to
  // `None` so the daemon resolves the priority default.
  let is_default = match selected_server {
    None => true,
    Some(id) => servers.first().is_some_and(|s| &s.id == id),
  };
  let server = if is_default {
    None
  } else {
    selected_server.clone()
  };
  let backend = match &server {
    Some(id) => servers
      .iter()
      .find(|s| &s.id == id)
      .map(|s| s.backend_id.clone()),
    None => model_backend.explicit_id().map(str::to_string),
  };
  (backend, server)
}

impl LaunchPickerState {
  pub fn for_model(model_name: impl Into<String>) -> Self {
    Self {
      model_name: model_name.into(),
      native_ctx: None,
      user_knobs: KnobSet::new(),
      resolved: KnobSet::new(),
      sources: BTreeMap::new(),
      extras: Vec::new(),
      extras_input: crate::tui::input_field::InputField::default(),
      inline_edit: InlineEdit::default(),
      field: PickerField::Preset,
      model_backend: BackendChoice::from_id(crate::backend::DEFAULT_BACKEND_ID),
      active_instances: 0,
      prefer_port: None,
      servers: Vec::new(),
      selected_server: None,
      device_cursor: 0,
      presets: Vec::new(),
      default_stop: PresetStop::LastUsed,
      preset_stop: PresetStop::LastUsed,
      mtp_capable: false,
      preset_baseline_knobs: KnobSet::new(),
      preset_baseline_extras: Vec::new(),
      scroll_offset: Cell::new(0),
    }
  }

  // ---------------------------------------------------------------- rows

  /// The active backend's knobs bucketed for render: group order, then
  /// declaration order within a group, with groups whose rows are all hidden
  /// on this host / model dropped entirely (header included).
  pub fn visible_groups(&self) -> Vec<(Group, Vec<&'static KnobDef>)> {
    knobs::registry::grouped_for_backend(self.active_backend_id())
      .into_iter()
      .filter(|(g, _)| self.group_gate_open(*g))
      .collect()
  }

  /// Whether a group's runtime gate is satisfied. A group with no gate is
  /// always open.
  fn group_gate_open(&self, group: Group) -> bool {
    group.gate_open(self.gate_facts())
  }

  /// The gate inputs for the editor, scoped to the selected server — cycling
  /// the `server` row re-answers them.
  fn gate_facts(&self) -> knobs::GateFacts {
    knobs::GateFacts {
      device_choice: self.device_choice(),
      multi_device: self.multi_device(),
      mtp_capable: self.mtp_capable,
    }
  }

  /// All rows in render / navigation order: `Preset`, `Server`, every visible
  /// knob row, then `Extras` last.
  pub fn ordered_fields(&self) -> Vec<PickerField> {
    let mut v = vec![PickerField::Preset, PickerField::Server];
    for (_, defs) in self.visible_groups() {
      v.extend(defs.into_iter().map(|d| PickerField::Knob(d.knob_id())));
    }
    v.push(PickerField::Extras);
    v
  }

  /// The declaration behind a knob row, scoped to the active backend. `None`
  /// for a row the active backend doesn't declare — which is how a row
  /// disappears after a cross-backend server switch.
  pub fn def(&self, id: KnobId) -> Option<&'static KnobDef> {
    knobs::def_for_backend(self.active_backend_id(), id)
  }

  /// Whether a row is currently shown / navigable.
  pub fn field_visible(&self, field: PickerField) -> bool {
    match field {
      // Always shown — it offers `last used` ↔ `auto` even with no presets.
      PickerField::Preset => true,
      // Shown only with a real choice: two or more compatible servers.
      PickerField::Server => self.servers.len() > 1,
      PickerField::Knob(id) => match self.def(id) {
        Some(def) => self.group_gate_open(def.group),
        // The active backend doesn't declare it.
        None => false,
      },
      PickerField::Extras => true,
    }
  }

  /// Whether `e:edit` opens an inline buffer on the focused row. Preset,
  /// Server and boolean knob rows are cycle-only — surfacing `e:edit` there
  /// would be a misleading affordance.
  pub fn focused_is_editable(&self) -> bool {
    match self.field {
      PickerField::Preset | PickerField::Server => false,
      PickerField::Extras => true,
      PickerField::Knob(id) => self.def(id).is_some_and(|d| d.is_editable()),
    }
  }

  /// True when the focused row is cyclable (←/→ would change the value).
  /// `Extras` is non-cyclable; so is a knob with nothing to cycle.
  pub fn focused_field_is_cyclable(&self) -> bool {
    match self.field {
      PickerField::Extras => false,
      PickerField::Knob(id) => match self.def(id) {
        // A bool always toggles even with no declared ring.
        Some(d) => d.kind == KnobKind::Bool || d.ring() != Ring::None,
        None => false,
      },
      _ => true,
    }
  }

  /// Move cursor to the next visible row.
  pub fn next_field(&mut self) {
    self.step_field(true);
  }

  /// Move cursor to the previous visible row.
  pub fn prev_field(&mut self) {
    self.step_field(false);
  }

  /// Advance the cursor one step in `forward`/back direction, skipping any
  /// hidden row. A cursor left on a row the active backend no longer declares
  /// (after a cross-backend server switch) restarts from the top rather than
  /// stranding navigation.
  fn step_field(&mut self, forward: bool) {
    let all = self.ordered_fields();
    let Some(i) = all.iter().position(|f| *f == self.field) else {
      self.field = PickerField::Preset;
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

  // ------------------------------------------------------------- presets

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
      }
      PresetStop::Auto => self.apply_auto(),
      PresetStop::Named(i) => self.seed_from_preset(i),
    }
  }

  /// `auto` stop: delegate every fit-governed knob the active backend
  /// declares to its engine's fitter, clear the rest to inherited, and drop
  /// any manual extras. The form reads "auto" on those rows and "inherited"
  /// elsewhere.
  fn apply_auto(&mut self) {
    self.user_knobs = KnobSet::new();
    for def in knobs::for_backend(self.active_backend_id()) {
      if def.is_fit_delegated() {
        self.user_knobs.set_auto(def.knob_id());
      }
    }
    self.extras.clear();
  }

  fn seed_from_preset(&mut self, i: usize) {
    if let Some(p) = self.presets.get(i) {
      self.user_knobs = p.knobs.clone();
      self.extras = p.extras.clone();
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
  /// the model's configured default.
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

  // ------------------------------------------------------------- servers

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
  /// device-less server is scoped.
  fn current_devices(&self) -> &[crate::backend::Device] {
    self
      .current_server()
      .map(|s| s.devices.as_slice())
      .unwrap_or(&[])
  }

  /// Whether the current pick resolves to the priority default: either unset,
  /// or an explicit pin of `servers[0]`. Pinning `servers[0]` is identical to
  /// leaving it unset, so the two collapse into one cycle stop.
  fn server_is_default(&self) -> bool {
    match &self.selected_server {
      None => true,
      Some(id) => self.servers.first().is_some_and(|s| &s.id == id),
    }
  }

  /// The launch identity this picker would pin on a saved preset — the chosen
  /// server build and the backend it runs on, kept consistent by
  /// [`launch_identity`].
  pub fn launch_identity(&self) -> (Option<String>, Option<String>) {
    launch_identity(&self.selected_server, &self.servers, &self.model_backend)
  }

  /// Cycle the Server row. The ring is the **default** stop (position 0, which
  /// folds in `servers[0]`) followed by each **non-default** server
  /// (`servers[1..]`). Landing on the default clears the pick (`None`) so the
  /// daemon resolves the priority default / last-used; each other stop pins
  /// that id. A switch clears the device selection (a stale selector wouldn't
  /// validate against the new server) and, on a cross-backend switch, changes
  /// which knobs are declared — so any user value the new backend has no knob
  /// for is dropped rather than carried silently.
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
    self.clear_device_row();
    self.device_cursor = 0;
    self.rescope_knobs_to_backend();
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
    self
      .selected_server
      .clone()
      .unwrap_or_else(|| "default".to_string())
  }

  /// Carry the user's knobs into the backend now in scope, then drop whatever
  /// it does not declare.
  ///
  /// A value keyed to a knob the new backend never heard of would otherwise
  /// sit in the set, invisible (no row renders it) but still shipped on the
  /// wire. Shared concepts survive the move — a pinned context window is still
  /// a pinned context window after switching engines.
  fn rescope_knobs_to_backend(&mut self) {
    let target = self.active_backend_id();
    self.user_knobs = knobs::resolve::rescope(&self.user_knobs, target);
    // A cursor on a row the new backend doesn't declare would strand
    // navigation; drop back to the always-present Preset row.
    if !self.field_visible(self.field) {
      self.field = PickerField::Preset;
    }
  }

  // ------------------------------------------------------------- backend

  /// The model's concrete backend. An explicit server pick determines it (its
  /// owning backend), so cycling to a llama.cpp server on a ds4 model swaps
  /// the knob set; an unset pick falls through to the model's own backend.
  /// Registry-driven — names no backend.
  fn resolved_backend(&self) -> crate::backend::Backends {
    use crate::backend::{Backend, Backends};
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

  /// The active backend's id — the vocabulary every row on this form is
  /// keyed in.
  pub fn active_backend_id(&self) -> &'static str {
    use crate::backend::Backend;
    self.resolved_backend().id()
  }

  /// The `backend` override to send on the wire. `model_backend` is a *scoping*
  /// value the picker derives from the row's badge, not a user-chosen override
  /// (the picker exposes no backend cycle). A scope for a **direct routing**
  /// backend (a non-default backend the daemon picks by header) is downgraded to
  /// `Auto` so the daemon re-derives the route — it still lands there for a
  /// compatible file, but the daemon's Auto-only pre-spawn guards fire instead
  /// of a doomed load. The default backend and a managed-multiplexer
  /// (identity-bound) scope pass through unchanged. Names no backend.
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

  // -------------------------------------------------------------- values

  /// The value the editor row should display, user override first and the
  /// resolver-chain value otherwise.
  pub fn effective(&self, id: KnobId) -> Option<&KnobValue> {
    self.user_knobs.get(id).or_else(|| self.resolved.get(id))
  }

  pub fn effective_u32(&self, id: KnobId) -> Option<u32> {
    self.effective(id)?.set_value()?.as_u32()
  }

  pub fn effective_str(&self, id: KnobId) -> Option<&str> {
    self.effective(id)?.set_value()?.as_str()
  }

  pub fn effective_bool(&self, id: KnobId) -> Option<bool> {
    self.effective(id)?.set_value()?.as_bool()
  }

  /// Source label for a row. `User` when the user has an explicit override;
  /// the resolver's source map otherwise, falling back to the knob's own
  /// declared `fallback` when the resolver hasn't populated the map yet
  /// (freshly-opened editor before the first resolve).
  pub fn source_for(&self, id: KnobId) -> LayerLabel {
    if self.user_knobs.contains(id) {
      return LayerLabel::User;
    }
    self.sources.get(&id).copied().unwrap_or_else(|| {
      self
        .def(id)
        .map(|d| d.fallback)
        .unwrap_or(LayerLabel::ServerDefault)
    })
  }

  /// Whether the row's *effective* state is `Auto` — either the user cycled it
  /// there, or (untouched) it resolved to Auto via the seeding rule.
  pub fn effective_is_auto(&self, id: KnobId) -> bool {
    self.effective(id).is_some_and(|v| v.is_auto())
  }

  /// Seed the resolved knobs + source map from the layered resolver output.
  /// The user-knobs layer is empty on a freshly-opened editor — the rows show
  /// inherited values.
  pub fn set_resolved(&mut self, resolved: KnobSet, sources: BTreeMap<KnobId, LayerLabel>) {
    self.resolved = resolved;
    self.sources = sources;
  }

  /// The value column for one knob row.
  ///
  /// `auto` for a delegated row, the shared `inherited` word for an unset one,
  /// and the device row's checkbox view for the device selector; everything
  /// else renders its scalar the way the engine would take it.
  pub fn value_label(&self, id: KnobId) -> String {
    let Some(def) = self.def(id) else {
      return INHERITED_LABEL.to_string();
    };
    if matches!(def.ring(), Ring::DeviceCheckbox) {
      return self.device_value_display();
    }
    KnobValue::render(self.effective(id), INHERITED_LABEL)
  }

  /// Seed text for an `e`-edit on a knob row: the current effective value, or
  /// empty when the row inherits or is delegated (there is no literal to edit).
  pub fn buffer_seed(&self, id: KnobId) -> String {
    match self.effective(id) {
      Some(KnobValue::Set(s)) => s.to_arg(),
      _ => String::new(),
    }
  }

  /// Commit a typed value onto a knob row, validated by the knob's own
  /// declaration. An empty buffer clears the row back to inherited — the same
  /// semantics as Backspace, reached through `e → delete → Enter`.
  ///
  /// Returns the parse error to render under the row, so one code path
  /// produces the CLI's message and the editor's.
  pub fn commit_text(&mut self, id: KnobId, raw: &str) -> Result<(), String> {
    let Some(def) = self.def(id) else {
      return Ok(());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
      self.user_knobs.clear(id);
      return Ok(());
    }
    match knobs::parse_value(def, trimmed) {
      Ok(v) => {
        self.user_knobs.set(id, v);
        Ok(())
      }
      Err(e) => Err(e.to_string()),
    }
  }

  /// Backspace on a focused row: clear the user override and re-inherit.
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
        self.clear_device_row();
        self.device_cursor = 0;
        self.rescope_knobs_to_backend();
      }
      PickerField::Knob(id) => {
        self.user_knobs.clear(id);
      }
      PickerField::Extras => {
        self.extras.clear();
      }
    }
  }

  // ---------------------------------------------------------------- mode

  /// The serving mode this form would launch with, or `None` when the row is
  /// untouched and the model's own hint should decide.
  ///
  /// `mode` is an ordinary knob row (`Emit::Custom`, so nothing emits it from
  /// the knob map) and the launch reads `LaunchParams.mode`. Same shape as
  /// `mtp_intent` beside it: one projection point between the row and the
  /// typed sibling. Without it a preset's pinned mode, and any edit the user
  /// makes on that row, rode the wire as a knob nobody consumes while the
  /// catalog hint silently decided the launch.
  pub fn mode_intent(&self) -> Option<crate::launch::mode::LaunchMode> {
    self
      .user_knobs
      .str_by_concept(self.active_backend_id(), knobs::Concept::Mode)
      .and_then(crate::launch::mode::LaunchMode::from_label)
  }

  // ----------------------------------------------------------------- mtp

  /// The speculation knob of the backend in scope, when it declares one.
  ///
  /// `mtp` is an ordinary knob row in the editor. The wire params still carry
  /// the intent as a typed sibling, so these two project between the row and
  /// that field the same way a preset does — one projection point, not a
  /// second channel.
  fn mtp_knob(&self) -> Option<&'static KnobDef> {
    knobs::def_for_backend(self.active_backend_id(), knobs::kid("mtp"))
  }

  /// The MTP intent this form would launch with.
  pub fn mtp_intent(&self) -> crate::launch::params::MtpEnable {
    use crate::launch::params::MtpEnable;
    let Some(def) = self.mtp_knob() else {
      return MtpEnable::Auto;
    };
    match self.user_knobs.get(def.knob_id()) {
      Some(KnobValue::Set(Scalar::Bool(true))) => MtpEnable::On,
      Some(KnobValue::Set(Scalar::Bool(false))) => MtpEnable::Off,
      // Explicit `auto`, and an untouched row, both mean "let the model
      // decide" — which is what `Auto` says.
      _ => MtpEnable::Auto,
    }
  }

  /// Seed the MTP row from a remembered intent.
  pub fn set_mtp_intent(&mut self, intent: crate::launch::params::MtpEnable) {
    use crate::launch::params::MtpEnable;
    let Some(def) = self.mtp_knob() else { return };
    let id = def.knob_id();
    match intent {
      MtpEnable::Auto => self.user_knobs.set_auto(id),
      MtpEnable::On => self.user_knobs.set(id, KnobValue::Set(Scalar::Bool(true))),
      MtpEnable::Off => self.user_knobs.set(id, KnobValue::Set(Scalar::Bool(false))),
    }
  }

  // -------------------------------------------------------------- cycles

  /// Cycle the focused field's value forward (Right arrow).
  pub fn cycle_focused_value_next(&mut self) {
    self.cycle_focused(true);
  }

  /// Cycle the focused field's value backward (Left arrow).
  pub fn cycle_focused_value_prev(&mut self) {
    self.cycle_focused(false);
  }

  fn cycle_focused(&mut self, forward: bool) {
    match self.field {
      PickerField::Preset => self.cycle_preset(forward),
      PickerField::Server => self.cycle_server(forward),
      PickerField::Knob(id) => self.cycle_knob(id, forward),
      PickerField::Extras => {}
    }
  }

  /// Cycle one knob row, entirely from its declaration.
  ///
  /// A bool walks its own small ring; the device row walks the host's GPUs;
  /// everything else steps the declared ring, gated to what this host / model
  /// can actually use.
  fn cycle_knob(&mut self, id: KnobId, forward: bool) {
    let Some(def) = self.def(id) else { return };
    if matches!(def.ring(), Ring::DeviceCheckbox) {
      self.walk_device_cursor(forward);
      return;
    }
    if def.kind == KnobKind::Bool {
      self.cycle_bool(def, forward);
      return;
    }
    let stops = self.ring_stops(def);
    if stops.is_empty() {
      // Nothing declared to cycle (a free-form ratio or path). `e` edits it.
      return;
    }
    let allow_auto = def.has_auto();
    let cur = match self.user_knobs.get(id) {
      Some(KnobValue::Auto) if allow_auto => CycleState::Auto,
      Some(KnobValue::Set(s)) => CycleState::Set(s.clone()),
      _ => CycleState::Inherited,
    };
    match ring_next(cur, &stops, forward, allow_auto) {
      CycleState::Inherited => {
        self.user_knobs.clear(id);
      }
      CycleState::Auto => self.user_knobs.set_auto(id),
      CycleState::Set(v) => self.user_knobs.set(id, KnobValue::Set(v)),
    }
  }

  /// A bool's ring. A knob with an `auto` state gets the quad ring
  /// `inherited → auto → on → off`; one without drops the stop that would
  /// emit nothing, leaving `inherited → on → off`.
  fn cycle_bool(&mut self, def: &'static KnobDef, forward: bool) {
    let id = def.knob_id();
    let allow_auto = def.has_auto();
    let cur = match self.user_knobs.get(id) {
      Some(KnobValue::Auto) if allow_auto => CycleState::Auto,
      Some(KnobValue::Set(s)) => match s.as_bool() {
        Some(b) => CycleState::Set(b),
        None => CycleState::Inherited,
      },
      _ => CycleState::Inherited,
    };
    let next = if forward {
      match cur {
        CycleState::Inherited if allow_auto => CycleState::Auto,
        CycleState::Inherited | CycleState::Auto => CycleState::Set(true),
        CycleState::Set(true) => CycleState::Set(false),
        CycleState::Set(false) => CycleState::Inherited,
      }
    } else {
      match cur {
        CycleState::Inherited => CycleState::Set(false),
        CycleState::Set(false) => CycleState::Set(true),
        CycleState::Set(true) if allow_auto => CycleState::Auto,
        CycleState::Set(true) | CycleState::Auto => CycleState::Inherited,
      }
    };
    match next {
      CycleState::Inherited => {
        self.user_knobs.clear(id);
      }
      CycleState::Auto => self.user_knobs.set_auto(id),
      CycleState::Set(b) => self.user_knobs.set(id, KnobValue::Set(Scalar::Bool(b))),
    }
  }

  /// The declared ring for a knob, parsed into values and trimmed to what this
  /// host / model can honor. A stop the knob's own kind rejects is dropped —
  /// `registry::validate` fails the build on one, so this is belt and braces.
  fn ring_stops(&self, def: &'static KnobDef) -> Vec<Scalar> {
    let raw: Vec<&'static str> = match def.ring() {
      Ring::None | Ring::DeviceCheckbox => return Vec::new(),
      Ring::Fixed(r) => r.to_vec(),
      Ring::UpToTrainedContext(r) => r.to_vec(),
      // `0..N` over the devices actually in play. A fixed ladder would offer
      // GPU indices this host does not have.
      Ring::DeviceIndex => {
        return (0..self.device_selection().len().max(1) as u32)
          .map(Scalar::U32)
          .collect()
      }
    };
    let ceiling = match (def.ring(), self.native_ctx) {
      (Ring::UpToTrainedContext(_), Some(max)) => Some(max),
      _ => None,
    };
    raw
      .into_iter()
      .filter_map(|s| knobs::parse_value(def, s).ok()?.set_value().cloned())
      .filter(|s| match (ceiling, s.as_u32()) {
        (Some(max), Some(v)) => v <= max,
        _ => true,
      })
      .collect()
  }

  // -------------------------------------------------------------- device

  /// Whether the selected server sees more than one physical GPU. The
  /// Multi-GPU placement group is hidden when `false` so single-GPU / CPU-only
  /// users don't see rows that can only ever hold `default`. One card reported
  /// under two compute APIs is still one GPU — see [`Self::device_choice`].
  pub fn multi_device(&self) -> bool {
    crate::backend::physical_device_count(self.current_devices()) > 1
  }

  /// Whether the selected server offers a `--device` choice at all. True for a
  /// genuine multi-GPU server *and* for a single card a dual-API build reports
  /// twice (`ROCm0` + `Vulkan0`), where picking the compute path is a real
  /// launch decision even though there is nothing to place a model across.
  pub fn device_choice(&self) -> bool {
    self.current_devices().len() > 1
  }

  /// The active backend's device knob, when it declares one.
  fn device_knob(&self) -> Option<&'static KnobDef> {
    knobs::def_for_backend_concept(self.active_backend_id(), knobs::Concept::Device)
  }

  fn clear_device_row(&mut self) {
    if let Some(def) = self.device_knob() {
      self.user_knobs.clear(def.knob_id());
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

  /// The concrete set of selected device selectors in catalog order. An unset
  /// row means "all the server's GPUs" (the engine default), so it
  /// materializes to the full device list — the checkbox view then shows every
  /// box ticked, and toggling one off yields an explicit `N-1` set.
  fn device_selection(&self) -> Vec<String> {
    let devices: Vec<&str> = self
      .current_devices()
      .iter()
      .map(|d| d.selector.as_str())
      .collect();
    let pinned = self
      .device_knob()
      .and_then(|d| self.effective_str(d.knob_id()))
      .filter(|s| !s.is_empty());
    match pinned {
      None => devices.iter().map(|s| s.to_string()).collect(),
      Some(csv) => {
        let picked: std::collections::HashSet<&str> = csv
          .split(',')
          .map(str::trim)
          .filter(|s| !s.is_empty())
          .collect();
        // Reorder against the catalog so `main-gpu` / `tensor-split` indices
        // stay stable regardless of the persisted string's order.
        devices
          .iter()
          .filter(|s| picked.contains(**s))
          .map(|s| s.to_string())
          .collect()
      }
    }
  }

  /// `Space` on the Device row: toggle the cursor's GPU in/out of the
  /// selection. Both extremes normalize to unset — selecting **all** N is the
  /// engine default (no flag), and clearing the **last** one has no valid
  /// "zero GPUs" meaning, so either snaps the row back to inherited.
  pub fn toggle_focused_device(&mut self) {
    let Some(def) = self.device_knob() else {
      return;
    };
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
    let id = def.knob_id();
    if ordered.is_empty() || ordered.len() == devices.len() {
      self.user_knobs.clear(id);
    } else {
      self
        .user_knobs
        .set(id, KnobValue::Set(Scalar::Str(ordered.join(","))));
    }
  }

  /// Display for the Device row. With >1 device (the only case the row is
  /// shown) it renders like every other cyclable knob — a single stop the
  /// ◀ ▶ arrows wrap — but each stop is a GPU with a `[x]`/`[ ]` checkbox in
  /// front: `[x] Vulkan1  ·  2 of 3`. `←/→` cycle which GPU is shown; `Space`
  /// toggles its box. The `· N of M` / `· all` suffix keeps the whole-selection
  /// count visible while only one stop shows. Below two devices it degrades to
  /// the plain `"<name> (<backend>)"` / `"inherited"` label.
  pub fn device_value_display(&self) -> String {
    let devices = self.current_devices();
    if devices.is_empty() {
      return INHERITED_LABEL.into();
    }
    if devices.len() < 2 {
      let sel = self
        .device_knob()
        .and_then(|d| self.effective_str(d.knob_id()))
        .filter(|v| !v.is_empty())
        .map(str::to_string);
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

/// A knob's position in the Auto-aware cycle ring:
/// `Inherited → Auto → Set(stop)… → wrap`.
enum CycleState<T> {
  /// Absent — inherits from the resolver chain.
  Inherited,
  /// Delegated (to the engine's fitter, or to a model capability).
  Auto,
  /// Pinned to a concrete value.
  Set(T),
}

/// Step a value knob one slot around the ring `Inherited → Auto →
/// stop[0] → … → stop[last] → Inherited`. Custom (off-ring) values snap to
/// the nearest stop in the travel direction via [`cycle_through`]; falling
/// off the top end lands on `Inherited`, off the bottom on `Auto`.
fn ring_next<T: PartialEq + Clone + Nearest>(
  current: CycleState<T>,
  stops: &[T],
  forward: bool,
  allow_auto: bool,
) -> CycleState<T> {
  // A knob with no Auto state has a two-stop ring `Inherited → stops… →
  // Inherited`. A stray Auto (e.g. a stale persisted value) coerces back to
  // Inherited so cycling escapes it.
  if !allow_auto {
    return match current {
      CycleState::Auto => CycleState::Inherited,
      CycleState::Inherited => {
        cycle_through(None, stops, forward).map_or(CycleState::Inherited, CycleState::Set)
      }
      CycleState::Set(v) => {
        cycle_through(Some(&v), stops, forward).map_or(CycleState::Inherited, CycleState::Set)
      }
    };
  }
  match current {
    CycleState::Inherited => {
      if forward {
        CycleState::Auto
      } else {
        // Backward from Inherited wraps to the last stop.
        cycle_through(None, stops, false).map_or(CycleState::Auto, CycleState::Set)
      }
    }
    CycleState::Auto => {
      if forward {
        cycle_through(None, stops, true).map_or(CycleState::Inherited, CycleState::Set)
      } else {
        CycleState::Inherited
      }
    }
    CycleState::Set(v) => match cycle_through(Some(&v), stops, forward) {
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

/// Ordering between two ring stops, where one exists.
///
/// Only numeric stops compare: "the nearest stop in the direction I pressed"
/// is a statement about magnitude, and a lexicographic answer for a device
/// selector or a quant name would be arbitrary. A `None` sends the caller to
/// the first / last stop instead, which is the right behaviour for a ring the
/// value isn't on.
trait Nearest {
  fn cmp_value(&self, other: &Self) -> Option<std::cmp::Ordering>;
}

impl Nearest for Scalar {
  fn cmp_value(&self, other: &Self) -> Option<std::cmp::Ordering> {
    match (self, other) {
      (Scalar::U32(a), Scalar::U32(b)) => Some(a.cmp(b)),
      (Scalar::F32(a), Scalar::F32(b)) => a.partial_cmp(b),
      _ => None,
    }
  }
}

/// Cycle through `stops` from `current`:
///
/// - **`current == None`** (row is inherited): wrap to the first stop
///   (forward) or the last (backward).
/// - **`current` matches a stop exactly**: advance / reverse one slot.
///   Falling off either end wraps back to `None` so the row re-inherits.
/// - **`current` sits between stops** (the user typed a custom value): snap to
///   the nearest stop *in the chosen direction* — `→` jumps to the smallest
///   stop strictly greater, `←` to the largest strictly less. Jumping to
///   `stops[0]` instead was a footgun on custom values mid-list.
///
/// `stops` is assumed ascending, which the declarations are.
fn cycle_through<T: PartialEq + Clone + Nearest>(
  current: Option<&T>,
  stops: &[T],
  forward: bool,
) -> Option<T> {
  if stops.is_empty() {
    return None;
  }
  let last = stops.len() - 1;
  match current {
    None => Some(if forward {
      stops[0].clone()
    } else {
      stops[last].clone()
    }),
    Some(v) => {
      if let Some(i) = stops.iter().position(|p| p == v) {
        return if forward {
          (i < last).then(|| stops[i + 1].clone())
        } else {
          (i > 0).then(|| stops[i - 1].clone())
        };
      }
      // Off-ring custom value: snap to the nearest stop in the direction the
      // user pressed. Falls back to first/last when every stop sits on the
      // other side (e.g. a value below `stops[0]` and the user pressed ←), and
      // for a non-numeric ring where "nearest" has no meaning.
      if forward {
        stops
          .iter()
          .find(|p| v.cmp_value(p) == Some(std::cmp::Ordering::Less))
          .cloned()
          .or_else(|| Some(stops[last].clone()))
      } else {
        stops
          .iter()
          .rev()
          .find(|p| v.cmp_value(p) == Some(std::cmp::Ordering::Greater))
          .cloned()
          .or_else(|| Some(stops[0].clone()))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::knobs::kid;

  /// A knob row by name, in the vocabulary of the backend under test.
  fn row(name: &str) -> PickerField {
    PickerField::Knob(kid(name))
  }

  fn u32_of(s: &LaunchPickerState, name: &str) -> Option<u32> {
    s.user_knobs.u32(kid(name))
  }

  fn bool_of(s: &LaunchPickerState, name: &str) -> Option<bool> {
    s.user_knobs.bool(kid(name))
  }

  fn str_of(s: &LaunchPickerState, name: &str) -> Option<String> {
    s.user_knobs.str(kid(name)).map(str::to_string)
  }

  /// The ctx row's ring as the picker would offer it.
  fn ctx_stops(s: &LaunchPickerState) -> Vec<u32> {
    let def = s.def(kid("ctx-size")).expect("llama.cpp declares ctx-size");
    s.ring_stops(def)
      .iter()
      .filter_map(|v| v.as_u32())
      .collect()
  }

  // ---------------------------------------------------------------- rings

  #[test]
  fn the_context_ring_is_trimmed_to_the_models_trained_window() {
    let mut s = LaunchPickerState::for_model("m");
    // Unknown window → the whole declared ladder.
    assert_eq!(ctx_stops(&s).len(), knobs::def::CTX_LADDER.len());
    // 128k model → capped at 131072; no 256k / 512k / 1M offered.
    s.native_ctx = Some(131072);
    assert_eq!(*ctx_stops(&s).last().unwrap(), 131072);
    assert!(!ctx_stops(&s).contains(&262144));
    // 256k model → reaches 262144 but not 524288.
    s.native_ctx = Some(262144);
    assert_eq!(*ctx_stops(&s).last().unwrap(), 262144);
    assert!(!ctx_stops(&s).contains(&524288));
    // 1M model → the whole ladder, 1 Mi included.
    s.native_ctx = Some(1048576);
    assert!(ctx_stops(&s).contains(&1048576));
    // A window below the smallest stop leaves nothing to cycle — type-only.
    s.native_ctx = Some(1024);
    assert!(ctx_stops(&s).is_empty());
  }

  #[test]
  fn cycling_context_never_passes_the_trained_window() {
    let mut s = LaunchPickerState::for_model("m");
    s.native_ctx = Some(131072);
    s.field = row("ctx-size");
    let gated = ctx_stops(&s);
    s.cycle_focused_value_next(); // Inherited → Auto (ctx is fit-delegated)
    for stop in &gated {
      s.cycle_focused_value_next();
      assert_eq!(u32_of(&s, "ctx-size"), Some(*stop));
    }
    s.cycle_focused_value_next();
    assert_eq!(
      u32_of(&s, "ctx-size"),
      None,
      "wraps at the trained cap, never into 256k+"
    );
  }

  #[test]
  fn a_bool_without_an_auto_state_walks_inherited_on_off_both_ways() {
    // `reasoning` declares no auto, so the ring drops the stop that would
    // emit nothing: Inherited → on → off → Inherited.
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = row("reasoning");
    for expected in [Some(true), Some(false), None] {
      s.cycle_focused_value_next();
      assert_eq!(bool_of(&s, "reasoning"), expected);
    }
    for expected in [Some(false), Some(true), None] {
      s.cycle_focused_value_prev();
      assert_eq!(bool_of(&s, "reasoning"), expected);
    }
  }

  #[test]
  fn flash_attn_walks_the_same_three_stops() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = row("flash-attn");
    for expected in [Some(true), Some(false), None] {
      s.cycle_focused_value_next();
      assert_eq!(bool_of(&s, "flash-attn"), expected);
    }
  }

  #[test]
  fn a_ringless_knob_does_not_move_on_the_arrows() {
    // `tensor-split` is a free-form ratio with no natural stops — ←/→ is a
    // deliberate no-op there, and `e` is the way in.
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = row("tensor-split");
    s.cycle_focused_value_next();
    s.cycle_focused_value_prev();
    assert_eq!(str_of(&s, "tensor-split"), None);
    assert!(!s.focused_field_is_cyclable());
    assert!(s.focused_is_editable(), "but it can be typed into");
  }

  #[test]
  fn a_closed_choice_knob_cycles_its_declared_choices() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = row("split-mode");
    let choices = s.def(kid("split-mode")).unwrap().kind.choices();
    for want in choices {
      s.cycle_focused_value_next();
      assert_eq!(str_of(&s, "split-mode").as_deref(), Some(*want));
    }
    s.cycle_focused_value_next();
    assert_eq!(str_of(&s, "split-mode"), None, "wraps to inherited");
  }

  // ----------------------------------------------------------- generation

  #[test]
  fn every_row_the_active_backend_declares_gets_generated() {
    // The parity claim, at the editor: no row is hand-listed, so the visible
    // set is exactly what the backend declared minus the gated groups.
    let s = LaunchPickerState::for_model("qwen");
    let mut declared: Vec<&str> = knobs::for_backend("llamacpp")
      .iter()
      .filter(|d| s.group_gate_open(d.group))
      .map(|d| d.id)
      .collect();
    let mut rendered: Vec<&str> = s
      .ordered_fields()
      .into_iter()
      .filter_map(|f| match f {
        PickerField::Knob(id) => Some(id.as_str()),
        _ => None,
      })
      .collect();
    assert!(!rendered.is_empty());
    // Sorted: the editor renders in *group* order, which is deliberately not
    // the declarations' emission order. That ordering is asserted separately;
    // what matters here is that the two sets are the same one.
    declared.sort_unstable();
    rendered.sort_unstable();
    assert_eq!(rendered, declared);
  }

  #[test]
  fn rows_are_grouped_and_ordered_by_the_declarations() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.servers = one_server(vec![
      device("CUDA0", "CUDA", "GPU 0"),
      device("CUDA1", "CUDA", "GPU 1"),
    ]);
    let groups = s.visible_groups();
    // Group order follows `Group::all()`, not declaration order.
    let titles: Vec<&str> = groups.iter().map(|(g, _)| g.title()).collect();
    let canonical: Vec<&str> = Group::all()
      .iter()
      .map(|g| g.title())
      .filter(|t| titles.contains(t))
      .collect();
    assert_eq!(titles, canonical);
    // Every row in a bucket really declares that group.
    for (g, defs) in &groups {
      assert!(defs.iter().all(|d| d.group == *g));
    }
  }

  #[test]
  fn a_lemonade_model_shows_only_what_lemonade_declares() {
    let mut s = LaunchPickerState::for_model("Qwen2.5-7B");
    s.model_backend = BackendChoice::Explicit("lemonade".into());
    // Lemonade loads over HTTP and declares one knob. There is no Backend row:
    // a model lives in exactly one backend's catalog, so there is nothing to
    // choose.
    let visible: Vec<PickerField> = s
      .ordered_fields()
      .into_iter()
      .filter(|f| s.field_visible(*f))
      .collect();
    assert_eq!(
      visible,
      vec![PickerField::Preset, row("ctx-size"), PickerField::Extras],
      "lemonade picker is preset + ctx + extras, nothing else"
    );
    assert_eq!(s.active_backend_id(), "lemonade");
  }

  #[test]
  fn a_row_the_active_backend_does_not_declare_is_not_visible() {
    let mut s = LaunchPickerState::for_model("m");
    s.model_backend = BackendChoice::Explicit("lemonade".into());
    // `flash-attn` is llama.cpp's; Lemonade never declared it.
    assert!(!s.field_visible(row("flash-attn")));
    assert!(s.def(kid("flash-attn")).is_none());
  }

  // ------------------------------------------------------------ navigation

  #[test]
  fn next_field_visits_every_visible_row_in_order() {
    let mut s = LaunchPickerState::for_model("qwen");
    // A single server with 2 devices makes the Multi-GPU group visible so
    // navigation visits every row (the Server row stays hidden with one
    // server). The single-GPU skip is covered separately.
    s.servers = one_server(vec![
      device("CUDA0", "CUDA", "GPU 0"),
      device("CUDA1", "CUDA", "GPU 1"),
    ]);
    let visible: Vec<PickerField> = s
      .ordered_fields()
      .into_iter()
      .filter(|f| s.field_visible(*f))
      .collect();
    assert!(
      visible.len() > 14,
      "should cover every declared knob + preset + extras, got {}",
      visible.len()
    );
    let start = visible
      .iter()
      .position(|f| *f == s.field)
      .expect("the initial field is visible");
    for step in 1..=visible.len() {
      s.next_field();
      assert_eq!(s.field, visible[(start + step) % visible.len()]);
    }
  }

  #[test]
  fn navigation_skips_the_device_and_placement_groups_on_a_single_gpu_host() {
    // Empty catalog → both runtime-gated groups are off, so neither direction
    // ever lands on one of their rows. Device and Multi-GPU placement gate
    // independently, so both belong in the sweep.
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(!s.multi_device());
    assert!(!s.device_choice());
    let hidden: Vec<PickerField> = knobs::for_backend("llamacpp")
      .iter()
      .filter(|d| matches!(d.group, Group::MultiGpu | Group::Device))
      .map(|d| PickerField::Knob(d.knob_id()))
      .collect();
    assert!(
      hidden.contains(&PickerField::Knob(knobs::kid("device"))),
      "the device row is one of the gated rows: {hidden:?}"
    );
    let n = s.ordered_fields().len() + hidden.len();
    for _ in 0..n {
      s.next_field();
      assert!(!hidden.contains(&s.field), "landed on hidden {:?}", s.field);
    }
    for _ in 0..n {
      s.prev_field();
      assert!(!hidden.contains(&s.field), "landed on hidden {:?}", s.field);
    }
  }

  #[test]
  fn navigation_skips_the_speculation_group_on_a_model_that_cannot_speculate() {
    let mut s = LaunchPickerState::for_model("qwen");
    let spec: Vec<PickerField> = knobs::for_backend("llamacpp")
      .iter()
      .filter(|d| d.group == Group::Speculation)
      .map(|d| PickerField::Knob(d.knob_id()))
      .collect();
    assert!(!spec.is_empty(), "llama.cpp declares speculation knobs");
    assert!(!s.mtp_capable);
    for f in &spec {
      assert!(!s.field_visible(*f));
    }
    // A capable model surfaces the whole group.
    s.mtp_capable = true;
    for f in &spec {
      assert!(s.field_visible(*f), "{f:?} should show for a capable model");
    }
  }

  // ------------------------------------------------------------------ mtp

  #[test]
  fn the_mtp_row_cycles_and_projects_onto_the_wire_intent() {
    use crate::launch::params::MtpEnable;
    let mut s = LaunchPickerState::for_model("qwen");
    s.mtp_capable = true;
    s.field = row("mtp");
    // `mtp` declares an auto state, so it carries the quad ring. An untouched
    // row and an explicit auto both mean "let the model decide".
    assert_eq!(s.mtp_intent(), MtpEnable::Auto);
    s.cycle_focused_value_next();
    assert_eq!(s.mtp_intent(), MtpEnable::Auto, "explicit auto");
    s.cycle_focused_value_next();
    assert_eq!(s.mtp_intent(), MtpEnable::On);
    s.cycle_focused_value_next();
    assert_eq!(s.mtp_intent(), MtpEnable::Off);
    s.cycle_focused_value_next();
    assert_eq!(s.mtp_intent(), MtpEnable::Auto, "wraps to inherited");
    // A remembered intent seeds the row it renders.
    s.set_mtp_intent(MtpEnable::On);
    assert_eq!(s.value_label(kid("mtp")), "on");
    s.reset_focused_row();
    assert_eq!(s.mtp_intent(), MtpEnable::Auto);
  }

  // --------------------------------------------------------------- values

  #[test]
  fn effective_is_auto_tracks_user_then_resolved_state() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(
      !s.effective_is_auto(kid("ctx-size")),
      "fresh knob is not Auto"
    );
    s.user_knobs.set_auto(kid("ctx-size"));
    assert!(s.effective_is_auto(kid("ctx-size")));
    // An untouched knob reflects the resolved (seeded / remembered) Auto.
    s.set_resolved(crate::knobset! { n_gpu_layers: auto }, BTreeMap::new());
    assert!(
      s.effective_is_auto(kid("n-gpu-layers")),
      "resolved Auto shows when the user hasn't overridden the row"
    );
  }

  #[test]
  fn reset_focused_row_clears_the_user_override() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = row("threads");
    s.cycle_focused_value_next();
    assert!(u32_of(&s, "threads").is_some());
    s.reset_focused_row();
    assert!(u32_of(&s, "threads").is_none());
  }

  #[test]
  fn source_falls_through_to_the_resolver_then_the_declared_fallback() {
    let mut s = LaunchPickerState::for_model("qwen");
    // Nothing resolved yet → the knob's own declared fallback layer.
    assert_eq!(
      s.source_for(kid("n-gpu-layers")),
      s.def(kid("n-gpu-layers")).unwrap().fallback
    );
    let mut sources = BTreeMap::new();
    sources.insert(kid("n-gpu-layers"), LayerLabel::ArchDefault);
    s.set_resolved(crate::knobset! { n_gpu_layers: 99 }, sources);
    assert_eq!(s.source_for(kid("n-gpu-layers")), LayerLabel::ArchDefault);
    // A user override flips the chip to User.
    s.user_knobs
      .set(kid("n-gpu-layers"), KnobValue::Set(Scalar::U32(32)));
    assert_eq!(s.source_for(kid("n-gpu-layers")), LayerLabel::User);
  }

  #[test]
  fn a_bool_row_reads_on_and_off_not_true_and_false() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert_eq!(s.value_label(kid("flash-attn")), INHERITED_LABEL);
    s.user_knobs
      .set(kid("flash-attn"), KnobValue::Set(Scalar::Bool(true)));
    assert_eq!(s.value_label(kid("flash-attn")), "on");
    s.user_knobs
      .set(kid("flash-attn"), KnobValue::Set(Scalar::Bool(false)));
    assert_eq!(s.value_label(kid("flash-attn")), "off");
    s.user_knobs.set_auto(kid("ctx-size"));
    assert_eq!(s.value_label(kid("ctx-size")), "auto");
  }

  #[test]
  fn commit_text_validates_through_the_knobs_own_declaration() {
    let mut s = LaunchPickerState::for_model("qwen");
    assert!(s.commit_text(kid("threads"), "8").is_ok());
    assert_eq!(u32_of(&s, "threads"), Some(8));
    // The same parser the CLI runs, so the editor accepts exactly what
    // `start --threads` would — and refuses the same.
    let err = s.commit_text(kid("threads"), "xyz").unwrap_err();
    assert!(err.contains("threads"), "{err}");
    assert_eq!(
      u32_of(&s, "threads"),
      Some(8),
      "a refused commit keeps the old value"
    );
    // An empty buffer resets the row.
    assert!(s.commit_text(kid("threads"), "  ").is_ok());
    assert_eq!(u32_of(&s, "threads"), None);
    // A ratio typo surfaces under the row rather than inside the engine.
    assert!(s.commit_text(kid("tensor-split"), "3,x").is_err());
    assert!(s.commit_text(kid("tensor-split"), "3,1").is_ok());
  }

  #[test]
  fn a_bool_row_offers_no_text_editor() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.field = row("flash-attn");
    assert!(
      !s.focused_is_editable(),
      "two states — the toggle is better"
    );
    s.field = row("threads");
    assert!(s.focused_is_editable());
    s.field = PickerField::Preset;
    assert!(!s.focused_is_editable());
  }

  // ------------------------------------------------------------ ring math

  #[test]
  fn cycle_through_starts_at_the_first_stop_when_inherited() {
    let r = [Scalar::U32(1), Scalar::U32(2), Scalar::U32(3)];
    assert_eq!(cycle_through(None, &r, true), Some(Scalar::U32(1)));
    assert_eq!(cycle_through(None, &r, false), Some(Scalar::U32(3)));
  }

  #[test]
  fn cycle_through_wraps_to_inherited_at_either_end() {
    let r = [Scalar::U32(1), Scalar::U32(2), Scalar::U32(3)];
    assert_eq!(cycle_through(Some(&Scalar::U32(3)), &r, true), None);
    assert_eq!(cycle_through(Some(&Scalar::U32(1)), &r, false), None);
  }

  #[test]
  fn an_off_ring_value_snaps_to_the_nearest_stop_in_the_travel_direction() {
    // User typed `n-gpu-layers=42` via `e`, then presses →.
    let r: Vec<Scalar> = [0, 16, 32, 64, 99].into_iter().map(Scalar::U32).collect();
    let at = Scalar::U32(42);
    assert_eq!(cycle_through(Some(&at), &r, true), Some(Scalar::U32(64)));
    assert_eq!(cycle_through(Some(&at), &r, false), Some(Scalar::U32(32)));
  }

  #[test]
  fn an_off_ring_value_past_an_end_falls_back_to_that_end() {
    let r: Vec<Scalar> = [10, 20, 30].into_iter().map(Scalar::U32).collect();
    let below = Scalar::U32(5);
    assert_eq!(cycle_through(Some(&below), &r, true), Some(Scalar::U32(10)));
    // Nothing smaller to snap to → the first stop.
    assert_eq!(
      cycle_through(Some(&below), &r, false),
      Some(Scalar::U32(10))
    );
    let above = Scalar::U32(99);
    assert_eq!(
      cycle_through(Some(&above), &r, false),
      Some(Scalar::U32(30))
    );
    assert_eq!(cycle_through(Some(&above), &r, true), Some(Scalar::U32(30)));
  }

  #[test]
  fn a_non_numeric_off_ring_value_falls_back_rather_than_comparing_text() {
    // "nearest" is meaningless for a quant name, so an unlisted value goes to
    // the end the user is travelling towards instead of sorting alphabetically.
    let r: Vec<Scalar> = ["q4_0", "q8_0"]
      .into_iter()
      .map(|s| Scalar::Str(s.into()))
      .collect();
    let custom = Scalar::Str("turbo_quant".into());
    assert_eq!(
      cycle_through(Some(&custom), &r, true),
      Some(Scalar::Str("q8_0".into()))
    );
    assert_eq!(
      cycle_through(Some(&custom), &r, false),
      Some(Scalar::Str("q4_0".into()))
    );
  }

  // ------------------------------------------------ server-scoped devices

  use crate::backend::{Device, Server};

  /// Build a neutral [`Device`] for tests. Memory fields don't affect the
  /// picker logic, so they're left `None`.
  fn device(selector: &str, gpu: &str, name: &str) -> Device {
    Device {
      total_mib: None,
      free_mib: None,
      selector: selector.into(),
      gpu_backend: gpu.into(),
      name: name.into(),
    }
  }

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
    assert_eq!(s.device_value_display(), INHERITED_LABEL);
    s.user_knobs
      .set(kid("device"), KnobValue::Set(Scalar::Str("ROCm0".into())));
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
    // The row renders through the same generic value column every other knob
    // uses, so the checkbox view is what the editor actually shows.
    assert_eq!(s.value_label(kid("device")), s.device_value_display());
  }

  #[test]
  fn the_arrows_walk_the_device_cursor_without_changing_the_selection() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    s.field = row("device");
    s.cycle_focused_value_next();
    assert_eq!(s.device_value_display(), "[x] Vulkan1  ·  all");
    s.cycle_focused_value_next();
    assert_eq!(s.device_value_display(), "[x] ROCm0  ·  all");
    s.cycle_focused_value_next(); // wrap back to the first
    assert_eq!(s.device_value_display(), "[x] Vulkan0  ·  all");
    assert_eq!(str_of(&s, "device"), None, "walking selects nothing");
  }

  #[test]
  fn toggle_device_builds_a_catalog_ordered_selection() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    s.field = row("device");
    // Move the cursor to ROCm0 (index 2) and toggle it off.
    s.cycle_focused_value_next();
    s.cycle_focused_value_next();
    s.toggle_focused_device();
    // The persisted string keeps catalog order, not toggle order.
    assert_eq!(str_of(&s, "device").as_deref(), Some("Vulkan0,Vulkan1"));
  }

  #[test]
  fn toggling_every_device_back_on_normalizes_to_unset() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    s.field = row("device");
    s.toggle_focused_device();
    assert_eq!(str_of(&s, "device").as_deref(), Some("Vulkan1,ROCm0"));
    s.toggle_focused_device();
    assert_eq!(str_of(&s, "device"), None, "all N is the engine default");
  }

  #[test]
  fn toggling_the_last_device_off_normalizes_to_unset() {
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    s.field = row("device");
    s.user_knobs
      .set(kid("device"), KnobValue::Set(Scalar::Str("Vulkan0".into())));
    s.toggle_focused_device(); // cursor at 0 = Vulkan0
    assert_eq!(str_of(&s, "device"), None, "zero GPUs has no meaning");
  }

  #[test]
  fn toggle_device_with_an_empty_catalog_is_a_no_op() {
    let mut s = LaunchPickerState::for_model("test");
    s.toggle_focused_device();
    assert_eq!(str_of(&s, "device"), None);
  }

  #[test]
  fn the_main_gpu_ring_covers_only_the_devices_in_play() {
    // A fixed ladder would offer GPU indices this host does not have.
    let mut s = LaunchPickerState::for_model("test");
    s.servers = catalog_two_vendors();
    let def = s.def(kid("main-gpu")).unwrap();
    let indices = |state: &LaunchPickerState| -> Vec<u32> {
      state
        .ring_stops(def)
        .iter()
        .filter_map(|v| v.as_u32())
        .collect()
    };
    assert_eq!(indices(&s), vec![0, 1, 2]);
    // Narrowing the device pick narrows the ring with it.
    s.user_knobs
      .set(kid("device"), KnobValue::Set(Scalar::Str("Vulkan0".into())));
    assert_eq!(indices(&s), vec![0]);
  }

  // ------------------------------------------------------ server selection

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
    // The ring folds servers[0] into the default: default → vulkan → wrap.
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
  fn the_selected_server_scopes_the_device_list() {
    let mut s = LaunchPickerState::for_model("m");
    s.servers = two_llamacpp_servers();
    // Default scopes to servers[0] (rocm, 1 device) → Multi-GPU group hidden.
    assert_eq!(s.current_devices().len(), 1);
    assert!(!s.multi_device());
    // Pick the vulkan build (2 devices) → the group appears.
    s.selected_server = Some("llamacpp-vulkan".into());
    assert_eq!(s.current_devices().len(), 2);
    assert!(s.multi_device());
  }

  #[test]
  fn a_dual_api_build_keeps_the_device_row_but_hides_placement() {
    // One card, one binary, two compute APIs: picking ROCm0 vs Vulkan0 is a
    // real launch decision (measurably different throughput), but there is no
    // second GPU to split a model across.
    let mut s = LaunchPickerState::for_model("m");
    s.servers = one_server(vec![
      device("ROCm0", "ROCm", "AMD Radeon 8060S Graphics"),
      device(
        "Vulkan0",
        "Vulkan",
        "AMD Radeon 8060S Graphics (RADV STRIX_HALO)",
      ),
    ]);
    assert!(s.device_choice(), "two selectors to choose between");
    assert!(!s.multi_device(), "…but one physical GPU");

    let rows: Vec<&str> = s
      .ordered_fields()
      .iter()
      .filter_map(|f| match f {
        PickerField::Knob(id) => Some(id.as_str()),
        _ => None,
      })
      .collect();
    assert!(rows.contains(&"device"), "device row survives: {rows:?}");
    for placement in ["tensor-split", "main-gpu", "split-mode"] {
      assert!(
        !rows.contains(&placement),
        "{placement} is noise on one GPU: {rows:?}"
      );
    }
  }

  #[test]
  fn switching_server_clears_the_device_pick() {
    let mut s = LaunchPickerState::for_model("m");
    s.servers = two_llamacpp_servers();
    s.field = PickerField::Server;
    s.selected_server = Some("llamacpp-vulkan".into());
    s.user_knobs
      .set(kid("device"), KnobValue::Set(Scalar::Str("Vulkan1".into())));
    // Cycling the server row invalidates the now-foreign selector.
    s.cycle_focused_value_next();
    assert_eq!(str_of(&s, "device"), None);
  }

  #[test]
  fn the_selected_servers_backend_regenerates_the_whole_row_set() {
    // A deepseek4-style model with a ds4 server and a llama.cpp server.
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
    assert_eq!(s.active_backend_id(), "ds4");
    // ds4's own tunables are rows here, and llama.cpp's are not.
    assert!(s.field_visible(row("ssd-streaming")));
    assert!(!s.field_visible(row("n-gpu-layers")));
    // Pick the llama.cpp server → the row set swaps wholesale.
    s.selected_server = Some("llamacpp-rocm".into());
    assert_eq!(s.active_backend_id(), "llamacpp");
    assert!(s.field_visible(row("n-gpu-layers")));
    assert!(!s.field_visible(row("ssd-streaming")));
  }

  #[test]
  fn a_backend_switch_carries_shared_concepts_and_drops_the_rest() {
    let mut s = LaunchPickerState::for_model("DeepSeek-V4-Flash");
    s.model_backend = BackendChoice::Explicit("ds4".into());
    s.servers = vec![
      server("ds4", "ds4", "/ds4/ds4-server", vec![]),
      server("llamacpp-rocm", "llamacpp", "/rocm/llama-server", vec![]),
    ];
    s.field = PickerField::Server;
    // A shared concept (context) and a ds4-only knob.
    s.user_knobs
      .set(kid("ctx"), KnobValue::Set(Scalar::U32(8192)));
    s.user_knobs
      .set(kid("ssd-streaming"), KnobValue::Set(Scalar::Bool(true)));
    s.cycle_focused_value_next();
    assert_eq!(s.active_backend_id(), "llamacpp");
    // The context window follows the user across the switch, under llama.cpp's
    // own spelling. A value the destination has no row for is dropped rather
    // than riding along invisibly.
    assert_eq!(u32_of(&s, "ctx-size"), Some(8192));
    assert!(s.user_knobs.get(kid("ssd-streaming")).is_none());
  }

  #[test]
  fn a_backend_switch_moves_a_stranded_cursor_back_to_a_real_row() {
    let mut s = LaunchPickerState::for_model("DeepSeek-V4-Flash");
    s.model_backend = BackendChoice::Explicit("ds4".into());
    s.servers = vec![
      server("ds4", "ds4", "/ds4/ds4-server", vec![]),
      server("llamacpp-rocm", "llamacpp", "/rocm/llama-server", vec![]),
    ];
    // Sit on a ds4-only row, then switch away from ds4 through the Server row.
    s.field = PickerField::Server;
    s.cycle_focused_value_next();
    assert_eq!(s.active_backend_id(), "llamacpp");
    s.field = row("ssd-streaming");
    s.next_field();
    assert!(
      s.field_visible(s.field),
      "navigation must not strand on a row the backend no longer declares"
    );
  }

  // -------------------------------------------------------- preset cycle

  fn choice(name: &str, ctx: u32) -> PresetChoice {
    PresetChoice {
      name: name.into(),
      knobs: crate::knobset! { ctx: ctx },
      extras: Vec::new(),
    }
  }

  #[test]
  fn set_presets_opens_on_the_configured_default() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.user_knobs
      .set(kid("threads"), KnobValue::Set(Scalar::U32(8)));
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
    assert_eq!(u32_of(&s, "ctx-size"), Some(65536));
  }

  #[test]
  fn reset_on_the_preset_row_snaps_to_last_used() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.user_knobs
      .set(kid("threads"), KnobValue::Set(Scalar::U32(4)));
    s.set_presets(vec![choice("only", 4096)], PresetStop::LastUsed);
    s.field = PickerField::Preset;
    s.cycle_focused_value_next(); // auto
    s.cycle_focused_value_next(); // only
    assert_ne!(s.preset_stop, PresetStop::LastUsed);
    s.reset_focused_row();
    assert_eq!(s.preset_stop, PresetStop::LastUsed);
    assert_eq!(u32_of(&s, "threads"), Some(4), "baseline restored");
  }

  #[test]
  fn the_auto_stop_delegates_every_fit_governed_row_the_backend_declares() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.set_presets(vec![choice("only", 4096)], PresetStop::LastUsed);
    s.field = PickerField::Preset;
    s.cycle_focused_value_next(); // auto
    let delegated: Vec<&str> = knobs::for_backend("llamacpp")
      .iter()
      .filter(|d| d.is_fit_delegated())
      .map(|d| d.id)
      .collect();
    assert!(!delegated.is_empty());
    for id in delegated {
      assert!(
        s.user_knobs.is_auto(kid(id)),
        "`auto` must delegate {id} to the fitter"
      );
    }
  }

  #[test]
  fn a_presets_pinned_mode_reaches_the_launch_payload() {
    // The Mode row is a knob nothing emits from the knob map, so a preset that
    // pins it only reaches argv through `mode_intent` -> `LaunchParams.mode`.
    let mut s = LaunchPickerState::for_model("qwen");
    assert_eq!(
      s.mode_intent(),
      None,
      "an untouched row leaves it to the hint"
    );

    let pinned = PresetChoice {
      name: "emb".into(),
      knobs: crate::knobset! { mode: "embedding" },
      extras: Vec::new(),
    };
    s.set_presets(vec![pinned], PresetStop::Named(0));
    assert_eq!(
      s.mode_intent(),
      Some(crate::launch::mode::LaunchMode::Embedding)
    );
  }

  #[test]
  fn an_out_of_range_default_index_falls_back_to_last_used() {
    let mut s = LaunchPickerState::for_model("qwen");
    s.set_presets(vec![choice("a", 1)], PresetStop::Named(9));
    assert_eq!(s.default_stop, PresetStop::LastUsed);
    assert_eq!(s.preset_stop, PresetStop::LastUsed);
  }
}
