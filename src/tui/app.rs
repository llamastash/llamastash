//! TUI application state.
//!
//! The render loop and the event loop are in [`super::events`]; this
//! module is the pure state machine they drive. Keeping it pure lets
//! the TestBackend smoke test and the inline unit tests assert
//! behaviour without spinning up tokio.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::discovery::DiscoveredModel;
use crate::theme::{palette_for, Palette, ThemeName};
use crate::tui::advanced_panel::AdvancedPanelState;
use crate::tui::filter::rank;
use crate::tui::keybindings::{Action, Focus, KeyMap};
use crate::tui::launch_picker::LaunchPickerState;
use crate::tui::list_pane::{build_rows, ListRow, RowInputs};
use crate::tui::status_icons::SurfaceState;
use crate::tui::tabs::{tabs_for_mode, RightTab};

/// Maximum age of a toast before the App auto-clears it. Keeps
/// transient yank confirmations from sticking around forever.
const TOAST_TTL: Duration = Duration::from_secs(3);

/// In-memory snapshot of one launched model the daemon is
/// supervising. Mirrors the IPC `status` shape — kept in App so
/// the right-pane header can show port/state without re-querying.
#[derive(Debug, Clone)]
pub struct ManagedRow {
  pub launch_id: String,
  pub path: PathBuf,
  pub port: u16,
  pub state: SurfaceState,
  /// Latest per-PID RSS reading in bytes. `None` until the daemon's
  /// per-launch sampler has emitted at least one reading.
  pub rss_bytes: Option<u64>,
  /// Latest per-PID CPU usage percent (multi-core, may exceed 100%).
  /// `None` until the daemon's per-launch sampler primes.
  pub cpu_pct: Option<f32>,
}

/// Persisted "last successful launch params" for one model, fetched
/// via the daemon's `last_params_list` method. The TUI consults this
/// when opening the launch picker so the user lands on the same
/// ctx/reasoning/advanced they last shipped — without re-typing.
#[derive(Debug, Clone, Default)]
pub struct LastParamsRow {
  pub ctx: Option<u32>,
  pub reasoning: bool,
  pub advanced: Vec<String>,
}

/// Snapshot of the daemon-side metadata the Daemon info panel
/// renders. Mirrors the `daemon` object on the `status` response.
#[derive(Debug, Clone, Default)]
pub struct DaemonInfo {
  pub pid: Option<u32>,
  pub uptime_seconds: Option<u64>,
  pub build: Option<String>,
  pub server_path: Option<String>,
  /// Absolute path of the Unix-domain socket the daemon bound. Used
  /// by the Daemon info panel to render the `socket  …/daemon.sock
  /// pid 1234` line. `None` only when talking to an older daemon
  /// that pre-dates the field.
  pub socket_path: Option<String>,
}

/// Immutable parts of the App that don't change after construction.
#[derive(Debug, Clone)]
pub struct AppOptions {
  pub theme: ThemeName,
  /// User-defined palette resolved from `config.custom_theme`.
  /// `None` when the user didn't supply a custom theme block — in
  /// that case `ThemeName::Custom` falls through to macchiato via
  /// `palette_for`, and `cycle_theme` skips it.
  pub custom_palette: Option<Palette>,
  /// Runtime keybinding table. Built from defaults +
  /// `config.keybindings` overrides at startup; queried via
  /// `App::action_for` / `App::bindings_for`. Held in `AppOptions`
  /// (vs. directly on `App`) so it travels through the same
  /// construction path as the theme.
  pub keymap: KeyMap,
}

impl Default for AppOptions {
  fn default() -> Self {
    Self {
      theme: ThemeName::Macchiato,
      custom_palette: None,
      keymap: KeyMap::default(),
    }
  }
}

/// Central App state.
#[derive(Debug)]
pub struct App {
  pub options: AppOptions,
  pub focus: Focus,
  pub models: Vec<DiscoveredModel>,
  pub favorites: Vec<PathBuf>,
  pub managed: Vec<ManagedRow>,
  /// External (unmanaged) `llama-server` processes the daemon's
  /// sweep surfaced. Read-only — the TUI shows them with the `⇪`
  /// glyph; only `stop` is permitted on these rows.
  pub external: Vec<ManagedRow>,
  /// Last-known persisted launch params per model path. Keyed off
  /// the canonical `ModelId.path` the daemon emits.
  pub last_params: BTreeMap<PathBuf, LastParamsRow>,
  /// Selected right-pane tab. `Logs` is always reachable; mode-
  /// specific tabs (`Chat` / `Embed` / `Rerank`) become reachable
  /// when the focused model is Ready.
  pub right_tab: RightTab,
  /// Working chat session for the right-pane Chat tab. Holds the
  /// in-progress prompt and the most recent response so the render
  /// path is purely synchronous.
  pub chat: crate::tui::tabs::chat::ChatTabState,
  /// Embed-tab working state — single text input and latest
  /// response payload.
  pub embed: crate::tui::tabs::embed::EmbedTabState,
  /// Rerank-tab working state — query + candidate list.
  pub rerank: crate::tui::tabs::rerank::RerankTabState,
  /// Logs-tab buffer for the focused launch. Refreshed from the
  /// daemon's `logs_tail` IPC method on each tick.
  pub logs_state: crate::tui::tabs::logs::LogsTabState,
  /// Cursor index into the rendered row list (which mixes headers
  /// and models). Header rows are skipped during `move_*`.
  pub list_cursor: usize,
  pub filter_buffer: String,
  pub launch_picker: Option<LaunchPickerState>,
  pub advanced_panel: Option<AdvancedPanelState>,
  pub toast: Option<(String, Instant)>,
  pub daemon_connected: bool,
  /// Snapshot of the daemon-side metadata block from the most recent
  /// `status` response. Populated by [`Self::ingest_status`].
  pub daemon_info: DaemonInfo,
  /// Latest host-level CPU/RAM/GPU readings the daemon's sampler
  /// emits. Populated by [`Self::ingest_status`]; consumed by the
  /// Host info-row pane.
  pub host_metrics: HostMetricsSnapshot,
  /// Set when the user presses `q` so the event loop can exit.
  pub should_exit: bool,
  /// Whether the modal help overlay is visible. Bound to `?`.
  pub show_help: bool,
  /// Modal "are you sure?" prompt. `Some(...)` shows a centred
  /// confirmation overlay that captures `y` / Enter to dispatch
  /// the inner action and `n` / Esc to dismiss. Used by stop-model
  /// and kill-daemon so a fat-finger doesn't drop a running model
  /// or the whole supervisor.
  pub confirm_dialog: Option<ConfirmAction>,
}

/// Action awaiting user confirmation in the modal popup. Captured
/// at the moment the user presses the trigger key, applied when
/// they confirm with `y` / Enter.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
  /// `s:stop` — graceful shutdown of one managed launch.
  StopModel { launch_id: String, name: String },
  /// `Q:kill daemon` — issues a `shutdown` RPC to the daemon.
  KillDaemon,
}

impl App {
  pub fn new(options: AppOptions) -> Self {
    Self {
      options,
      focus: Focus::List,
      models: Vec::new(),
      favorites: Vec::new(),
      managed: Vec::new(),
      external: Vec::new(),
      last_params: BTreeMap::new(),
      right_tab: RightTab::Logs,
      chat: Default::default(),
      embed: Default::default(),
      rerank: Default::default(),
      logs_state: Default::default(),
      list_cursor: 0,
      filter_buffer: String::new(),
      launch_picker: None,
      advanced_panel: None,
      toast: None,
      daemon_connected: false,
      daemon_info: DaemonInfo::default(),
      host_metrics: HostMetricsSnapshot::default(),
      should_exit: false,
      show_help: false,
      confirm_dialog: None,
    }
  }

  /// True when the right pane should render. We always show it as
  /// long as the user has at least one discovered model — the pane
  /// follows the cursor and surfaces Settings (and Logs/Chat when
  /// running) for whatever model is currently selected.
  pub fn right_pane_visible(&self) -> bool {
    !self.models.is_empty()
  }

  /// Toggle the modal help overlay. Bound to `?`. Esc also closes
  /// it via the existing Cancel action plumbing.
  pub fn toggle_help(&mut self) {
    self.show_help = !self.show_help;
  }

  /// Resolve the active palette. For `ThemeName::Custom`, prefer the
  /// palette loaded from `config.custom_theme`; fall back to the
  /// built-in (macchiato) if `Custom` was selected without a loaded
  /// palette. Returns a borrow tied to `&self` because the custom
  /// palette lives on `options`, not in a static slot.
  pub fn palette(&self) -> &Palette {
    if self.options.theme == ThemeName::Custom {
      if let Some(custom) = &self.options.custom_palette {
        return custom;
      }
    }
    palette_for(self.options.theme)
  }

  /// Resolve a `(focus, key, mods)` triple through the live keymap.
  /// Drop-in replacement for the legacy `keybindings::action_for`
  /// free function so renderers/events can pick up
  /// `config.keybindings` overrides without re-implementing the
  /// dispatcher.
  pub fn action_for(
    &self,
    focus: Focus,
    key: crossterm::event::KeyCode,
    mods: crossterm::event::KeyModifiers,
  ) -> Option<Action> {
    self.options.keymap.action_for(focus, key, mods)
  }

  /// Bindings the help overlay should show for `focus`. Pulls from
  /// the runtime keymap so overrides applied at startup surface in
  /// the modal help screen too.
  pub fn bindings_for(&self, focus: Focus) -> &[crate::tui::keybindings::Binding] {
    self.options.keymap.bindings_for(focus)
  }

  /// Build a `key:description` chip string for `(focus, action)`
  /// against the live keymap. Returns `None` when the user has
  /// unbound the action in `focus` entirely so callers can drop the
  /// hint rather than render a chip with no working key.
  ///
  /// The description comes from the binding's `description` field,
  /// so updates in `keybindings.rs` flow through to every chip. For
  /// cases where the binding's full description is too verbose for a
  /// chip strip (e.g. `collapse think` → `think`), see
  /// [`Self::hint_with`].
  pub fn hint(&self, focus: Focus, action: Action) -> Option<String> {
    let b = self
      .bindings_for(focus)
      .iter()
      .find(|b| b.action == action)?;
    Some(format!("{}:{}", b.label, b.description))
  }

  /// Like [`Self::hint`] but with a caller-supplied description
  /// override. Same `None`-on-unbound semantics so the hint strip
  /// stays honest about what keys actually work.
  pub fn hint_with(&self, focus: Focus, action: Action, description: &str) -> Option<String> {
    let b = self
      .bindings_for(focus)
      .iter()
      .find(|b| b.action == action)?;
    Some(format!("{}:{}", b.label, description))
  }

  /// Apply a `list_models` IPC response. The TUI calls this after
  /// every refresh.
  pub fn ingest_list_models(&mut self, body: &Value) {
    let arr = match body.get("models").and_then(Value::as_array) {
      Some(a) => a,
      None => return,
    };
    let mut next: Vec<DiscoveredModel> = Vec::with_capacity(arr.len());
    for row in arr {
      if let Some(m) = parse_list_models_row(row) {
        next.push(m);
      }
    }
    self.models = next;
    self.clamp_cursor();
  }

  /// Apply a `status` IPC response. Refreshes the supervisor's
  /// per-launch rows, the read-only external rows, the daemon-info
  /// block, and the host-metrics snapshot. Discovery rows survive
  /// intact — `list_models` owns those.
  pub fn ingest_status(&mut self, body: &Value) {
    // Both `models` and `external` clear when their field is absent.
    // Asymmetry would let ghost supervised rows persist across a
    // schema change or transient framing error, so the TUI would
    // continue offering a stop affordance for a launch the daemon no
    // longer tracks.
    if let Some(arr) = body.get("models").and_then(Value::as_array) {
      self.managed = arr.iter().filter_map(parse_status_row).collect();
    } else {
      self.managed.clear();
    }
    if let Some(arr) = body.get("external").and_then(Value::as_array) {
      self.external = arr.iter().filter_map(parse_external_row).collect();
    } else {
      self.external.clear();
    }
    if let Some(daemon) = body.get("daemon") {
      self.daemon_info = DaemonInfo {
        pid: daemon.get("pid").and_then(Value::as_u64).map(|n| n as u32),
        uptime_seconds: daemon.get("uptime_seconds").and_then(Value::as_u64),
        build: daemon
          .get("build")
          .and_then(Value::as_str)
          .map(String::from),
        server_path: daemon
          .get("server_path")
          .and_then(Value::as_str)
          .map(String::from),
        socket_path: daemon
          .get("socket_path")
          .and_then(Value::as_str)
          .map(String::from),
      };
    }
    if let Some(host) = body.get("host") {
      if !host.is_null() {
        if let Ok(snap) = serde_json::from_value::<HostMetricsSnapshot>(host.clone()) {
          self.host_metrics = snap;
        }
      }
    }
  }

  /// Apply a `last_params_list` IPC response. The TUI uses the
  /// snapshot to seed the launch picker for the focused model.
  pub fn ingest_last_params(&mut self, body: &Value) {
    let arr = match body.get("last_params").and_then(Value::as_array) {
      Some(a) => a,
      None => return,
    };
    let mut next: BTreeMap<PathBuf, LastParamsRow> = BTreeMap::new();
    for row in arr {
      let path = row
        .get("model_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
      let params = row.get("params");
      if let (Some(path), Some(params)) = (path, params) {
        let ctx = params.get("ctx").and_then(Value::as_u64).map(|n| n as u32);
        let reasoning = params
          .get("reasoning")
          .and_then(Value::as_bool)
          .unwrap_or(false);
        let advanced = params
          .get("advanced")
          .and_then(Value::as_array)
          .map(|items| {
            items
              .iter()
              .filter_map(|v| v.as_str().map(String::from))
              .collect()
          })
          .unwrap_or_default();
        next.insert(
          path,
          LastParamsRow {
            ctx,
            reasoning,
            advanced,
          },
        );
      }
    }
    self.last_params = next;
  }

  /// Apply a `favorite_list` IPC response.
  pub fn ingest_favorites(&mut self, body: &Value) {
    let arr = match body.get("favorites").and_then(Value::as_array) {
      Some(a) => a,
      None => return,
    };
    self.favorites = arr
      .iter()
      .filter_map(|row| {
        row
          .get("id")
          .and_then(|id| id.get("path"))
          .and_then(Value::as_str)
      })
      .map(PathBuf::from)
      .collect();
  }

  /// Build the list of rows the renderer should draw, applying any
  /// active filter. Cached results aren't worth it: discovery
  /// snapshots are small (hundreds of rows) and the filter is
  /// hand-rolled subsequence matching.
  pub fn rendered_rows(&self) -> Vec<ListRow> {
    let model_states = self.surface_states();
    let mut all = build_rows(RowInputs {
      models: &self.models,
      favorites: &self.favorites,
      model_states: &model_states,
    });
    if !self.filter_buffer.is_empty() {
      all = apply_filter(&all, &self.filter_buffer);
    }
    all
  }

  fn surface_states(&self) -> BTreeMap<PathBuf, SurfaceState> {
    // Managed rows win over external when both reference the same
    // path — the daemon's sweep excludes adopted PIDs, so this
    // collision is rare in practice. External rows surface paths
    // that aren't part of the discovered catalog, so they're
    // skipped here and rendered separately (see
    // `rendered_rows`).
    let mut out: BTreeMap<PathBuf, SurfaceState> = BTreeMap::new();
    for m in &self.external {
      out.insert(m.path.clone(), m.state);
    }
    for m in &self.managed {
      out.insert(m.path.clone(), m.state);
    }
    out
  }

  /// Rows for external `llama-server` processes the daemon detected
  /// outside its supervisor. Surfaced read-only (stop is the only
  /// action allowed) — used by Unit 7's right pane to show "this
  /// model is unmanaged" hints.
  pub fn external_rows(&self) -> &[ManagedRow] {
    &self.external
  }

  /// Move cursor down to the next selectable (model) row.
  pub fn move_down(&mut self) {
    let rows = self.rendered_rows();
    self.move_down_in(&rows);
  }

  pub fn move_up(&mut self) {
    let rows = self.rendered_rows();
    self.move_up_in(&rows);
  }

  fn move_down_in(&mut self, rows: &[ListRow]) {
    if rows.is_empty() {
      return;
    }
    let mut next = self.list_cursor + 1;
    while next < rows.len() && !rows[next].is_selectable() {
      next += 1;
    }
    if next < rows.len() {
      self.list_cursor = next;
    }
  }

  fn move_up_in(&mut self, rows: &[ListRow]) {
    if self.list_cursor == 0 {
      return;
    }
    let mut next = self.list_cursor;
    while next > 0 {
      next -= 1;
      if rows.get(next).map(|r| r.is_selectable()).unwrap_or(false) {
        self.list_cursor = next;
        return;
      }
    }
  }

  /// Page-step: move the cursor by `delta` selectable rows. Positive
  /// values go down, negative up. Builds the rendered row vec once
  /// rather than once per step. Use this for PageUp/PageDown so a
  /// single keypress doesn't rebuild rows 10×.
  pub fn move_by(&mut self, delta: i32) {
    let rows = self.rendered_rows();
    if delta >= 0 {
      for _ in 0..delta {
        self.move_down_in(&rows);
      }
    } else {
      for _ in 0..-delta {
        self.move_up_in(&rows);
      }
    }
  }

  pub fn go_top(&mut self) {
    let rows = self.rendered_rows();
    for (i, r) in rows.iter().enumerate() {
      if r.is_selectable() {
        self.list_cursor = i;
        return;
      }
    }
  }

  pub fn go_bottom(&mut self) {
    let rows = self.rendered_rows();
    for (i, r) in rows.iter().enumerate().rev() {
      if r.is_selectable() {
        self.list_cursor = i;
        return;
      }
    }
  }

  fn clamp_cursor(&mut self) {
    let rows = self.rendered_rows();
    if rows.is_empty() {
      self.list_cursor = 0;
      return;
    }
    if self.list_cursor >= rows.len() {
      self.list_cursor = rows.len() - 1;
    }
    if !rows[self.list_cursor].is_selectable() {
      // Snap to next selectable row.
      self.go_top();
    }
  }

  /// Path of the model the cursor sits on.
  pub fn focused_path(&self) -> Option<PathBuf> {
    let rows = self.rendered_rows();
    rows
      .get(self.list_cursor)
      .and_then(|r| r.path().map(|p| p.to_path_buf()))
  }

  /// Display name of the focused model.
  pub fn focused_name(&self) -> Option<String> {
    let rows = self.rendered_rows();
    match rows.get(self.list_cursor) {
      Some(ListRow::Model { name, .. }) => Some(name.clone()),
      _ => None,
    }
  }

  pub fn focused_managed(&self) -> Option<&ManagedRow> {
    let path = self.focused_path()?;
    self.managed.iter().find(|m| m.path == path)
  }

  /// The ManagedRow the right pane should display — the model the
  /// cursor sits on, or `None` when the cursor row has no managed
  /// launch. The right pane follows the cursor with no sticky
  /// fallback: an unlaunched row shows just the Settings tab for
  /// the selected model.
  pub fn right_pane_focus(&self) -> Option<&ManagedRow> {
    self.focused_managed()
  }

  /// Open the launch picker for the focused model. Seeds from
  /// persisted `last_params` (R20) when the daemon has reported any
  /// for the focused path, so a returning user lands on the params
  /// they last shipped. No-op when the cursor is on a header.
  pub fn open_launch_picker(&mut self) {
    let name = match self.focused_name() {
      Some(n) => n,
      None => return,
    };
    let path = self.focused_path();
    let active_count = path
      .as_ref()
      .map(|p| self.managed.iter().filter(|m| &m.path == p).count())
      .unwrap_or(0);
    let mut state = LaunchPickerState::for_model(name);
    if let Some(p) = &path {
      if let Some(last) = self.last_params.get(p) {
        state.ctx = last.ctx;
        state.reasoning = last.reasoning;
        state.preset_idx = last.ctx.and_then(|c| {
          crate::tui::launch_picker::CTX_PRESETS
            .iter()
            .position(|val| *val == c)
        });
        if !last.advanced.is_empty() {
          let buffer = last.advanced.join(" ");
          let cursor = buffer.len();
          self.advanced_panel = Some(AdvancedPanelState { buffer, cursor });
        }
      }
    }
    state.active_instances = active_count;
    self.launch_picker = Some(state);
    // Pressing Enter:launch on a model routes the user to the
    // Settings tab in the right pane. The pane itself is always
    // visible (it follows the cursor) so we only have to flip the
    // focus + active tab — no `force_open` plumbing required.
    self.right_tab = RightTab::Settings;
    self.focus = Focus::RightPane;
  }

  pub fn close_launch_picker(&mut self) {
    self.launch_picker = None;
    self.focus = Focus::List;
    // Snap the right tab back to Logs so the next launch doesn't
    // re-open on a stale Settings view.
    self.right_tab = RightTab::Logs;
  }

  pub fn open_advanced_panel(&mut self) {
    self.advanced_panel = Some(AdvancedPanelState::default());
    self.focus = Focus::AdvancedPanel;
  }

  pub fn close_advanced_panel(&mut self) {
    self.advanced_panel = None;
    // Return to launch picker if it was the previous focus, else List.
    self.focus = if self.launch_picker.is_some() {
      Focus::LaunchPicker
    } else {
      Focus::List
    };
  }

  pub fn open_filter(&mut self) {
    self.focus = Focus::Filter;
  }

  /// Esc clears + leaves filter mode.
  pub fn clear_filter(&mut self) {
    self.filter_buffer.clear();
    self.focus = Focus::List;
    self.clamp_cursor();
  }

  /// Apply a transient toast.
  pub fn show_toast(&mut self, msg: impl Into<String>) {
    self.toast = Some((msg.into(), Instant::now()));
  }

  /// Drop the toast if it's older than [`TOAST_TTL`].
  pub fn expire_toast(&mut self) {
    if let Some((_, at)) = &self.toast {
      if at.elapsed() > TOAST_TTL {
        self.toast = None;
      }
    }
  }

  pub fn toast_message(&self) -> Option<&str> {
    self.toast.as_ref().map(|(s, _)| s.as_str())
  }

  /// Tabs the right pane should expose for the currently focused
  /// model. The rule is binary: a *running* selection (Launching /
  /// Loading / Ready) gets Logs + the mode-appropriate input tab +
  /// Settings; an unlaunched / stopped / errored / unfocused
  /// selection gets only Settings. There's no sticky fallback —
  /// what the user sees in the right pane is the model under the
  /// cursor, nothing else.
  pub fn available_right_tabs(&self) -> Vec<RightTab> {
    let managed = match self.focused_managed() {
      Some(m) => m,
      None => return vec![RightTab::Settings],
    };
    match managed.state {
      SurfaceState::Ready => {
        let mode = self
          .models
          .iter()
          .find(|m| m.path == managed.path)
          .and_then(|m| m.metadata.as_ref())
          .map(|md| md.mode_hint)
          .unwrap_or(crate::gguf::metadata::ModeHint::Chat);
        tabs_for_mode(mode)
      }
      // Process alive but not yet serving — Logs help the user
      // watch the startup pipeline, Settings stay available for
      // launch-time tweaks once the model is back to NotLaunched.
      SurfaceState::Launching | SurfaceState::Loading => {
        vec![RightTab::Logs, RightTab::Settings]
      }
      _ => vec![RightTab::Settings],
    }
  }

  /// Advance the right-pane tab. Skips tabs that aren't reachable
  /// for the current focus (e.g. Chat when the model isn't Ready).
  pub fn cycle_right_tab(&mut self) {
    let tabs = self.available_right_tabs();
    if tabs.is_empty() {
      self.right_tab = RightTab::Logs;
      return;
    }
    let pos = tabs.iter().position(|t| *t == self.right_tab).unwrap_or(0);
    let next = (pos + 1) % tabs.len();
    self.right_tab = tabs[next];
  }

  /// Cycle to the previous right-pane tab — used by `Left` arrow
  /// alongside [`Self::cycle_right_tab`] (`Right` arrow / Tab).
  pub fn cycle_right_tab_prev(&mut self) {
    let tabs = self.available_right_tabs();
    if tabs.is_empty() {
      self.right_tab = RightTab::Logs;
      return;
    }
    let pos = tabs.iter().position(|t| *t == self.right_tab).unwrap_or(0);
    let prev = (pos + tabs.len() - 1) % tabs.len();
    self.right_tab = tabs[prev];
  }

  /// Clamp `right_tab` back to a reachable choice if the focused
  /// model's available tabs shrink (e.g. the model dropped from
  /// Ready to Stopped). Called by the renderer before drawing.
  pub fn ensure_right_tab_reachable(&mut self) {
    let tabs = self.available_right_tabs();
    if !tabs.contains(&self.right_tab) {
      self.right_tab = RightTab::Logs;
    }
  }

  /// Cycle to the next theme. Used by the `t` hotkey.
  ///
  /// `Custom` is part of the cycle only when a user palette is loaded
  /// (`options.custom_palette.is_some()`). Otherwise it would render
  /// as the macchiato fallback and feel like a no-op tick. Built-ins
  /// always cycle; the custom slot slips in after `mono`.
  pub fn cycle_theme(&mut self) {
    use strum::IntoEnumIterator;
    let order: Vec<ThemeName> = ThemeName::iter()
      .filter(|t| *t != ThemeName::Custom || self.options.custom_palette.is_some())
      .collect();
    if order.is_empty() {
      return;
    }
    let pos = order
      .iter()
      .position(|t| *t == self.options.theme)
      .unwrap_or(order.len() - 1);
    let next = order[(pos + 1) % order.len()];
    self.options.theme = next;
  }
}

fn apply_filter(rows: &[ListRow], query: &str) -> Vec<ListRow> {
  // Only model rows take part in the rank — headers regroup
  // around the surviving models.
  let model_idx: Vec<usize> = rows
    .iter()
    .enumerate()
    .filter_map(|(i, r)| match r {
      ListRow::Model { .. } => Some(i),
      _ => None,
    })
    .collect();
  let names: Vec<String> = model_idx
    .iter()
    .filter_map(|i| match &rows[*i] {
      ListRow::Model {
        name, arch, quant, ..
      } => Some(format!("{name} {arch} {quant}")),
      _ => None,
    })
    .collect();
  let ranked = rank(query, &names);
  let kept: std::collections::BTreeSet<usize> = ranked.into_iter().map(|i| model_idx[i]).collect();
  // Reproduce the same section ordering, dropping headers whose
  // groups have no surviving model rows.
  let mut out: Vec<ListRow> = Vec::with_capacity(kept.len() + 4);
  let mut i = 0;
  while i < rows.len() {
    match &rows[i] {
      // The column-label row is always the first row in the
      // unfiltered list; preserve it at the top of the filtered
      // view so the columns still have labels.
      ListRow::TableHeader => {
        out.push(ListRow::TableHeader);
        i += 1;
      }
      ListRow::Header { .. } => {
        let header = rows[i].clone();
        let mut j = i + 1;
        let mut group: Vec<ListRow> = Vec::new();
        while j < rows.len() {
          if matches!(rows[j], ListRow::Header { .. } | ListRow::TableHeader) {
            break;
          }
          if kept.contains(&j) {
            group.push(rows[j].clone());
          }
          j += 1;
        }
        if !group.is_empty() {
          out.push(header);
          out.extend(group);
        }
        i = j;
      }
      ListRow::Model { .. } => {
        if kept.contains(&i) {
          out.push(rows[i].clone());
        }
        i += 1;
      }
    }
  }
  out
}

fn parse_list_models_row(row: &Value) -> Option<DiscoveredModel> {
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModelMetadata, Quant};

  let path = PathBuf::from(row.get("path")?.as_str()?);
  let parent = PathBuf::from(row.get("parent")?.as_str()?);
  let source = match row.get("source").and_then(Value::as_str)? {
    "user" => ModelSource::UserPath,
    "huggingface" => ModelSource::HuggingFace,
    "ollama" => ModelSource::Ollama,
    "lm-studio" => ModelSource::LmStudio,
    _ => ModelSource::UserPath,
  };
  let metadata = row.get("metadata").and_then(|md| {
    if md.is_null() {
      None
    } else {
      Some(ModelMetadata {
        arch: md.get("arch").and_then(Value::as_str).map(String::from),
        total_parameters: md.get("total_parameters").and_then(Value::as_u64),
        parameter_label: md
          .get("parameter_label")
          .and_then(Value::as_str)
          .map(String::from),
        quant: md
          .get("quant")
          .and_then(Value::as_str)
          .map(parse_quant)
          .unwrap_or_else(|| Quant::Unknown(0)),
        native_ctx: md.get("native_ctx").and_then(Value::as_u64),
        chat_template: None,
        tokenizer_kind: md
          .get("tokenizer_kind")
          .and_then(Value::as_str)
          .map(String::from),
        reasoning_hint: None,
        mode_hint: parse_mode_hint(md.get("mode_hint").and_then(Value::as_str)),
        weights_bytes: md.get("weights_bytes").and_then(Value::as_u64),
      })
    }
  });
  Some(DiscoveredModel {
    path,
    parent,
    source,
    metadata,
    parse_error: row
      .get("parse_error")
      .and_then(Value::as_str)
      .map(String::from),
    split_siblings: Vec::new(),
  })
}

fn parse_mode_hint(label: Option<&str>) -> crate::gguf::metadata::ModeHint {
  use crate::gguf::metadata::ModeHint;
  match label {
    Some("chat") => ModeHint::Chat,
    Some("embedding") => ModeHint::Embedding,
    Some("rerank") => ModeHint::Rerank,
    _ => ModeHint::Unknown,
  }
}

fn parse_quant(label: &str) -> crate::gguf::metadata::Quant {
  use crate::gguf::metadata::Quant;
  // Accept the canonical labels emitted by `Quant::label`. Anything
  // else lands as `Unknown(0)` so we don't crash on a future quant
  // tag the daemon learns about before the TUI does. The `0`
  // payload is just the "unknown ggml type" sentinel — we don't
  // surface it back to the user.
  match label {
    "F32" => Quant::F32,
    "F16" => Quant::F16,
    "BF16" => Quant::BF16,
    "Q4_0" => Quant::Q4_0,
    "Q4_1" => Quant::Q4_1,
    "Q5_0" => Quant::Q5_0,
    "Q5_1" => Quant::Q5_1,
    "Q8_0" => Quant::Q8_0,
    "Q8_1" => Quant::Q8_1,
    "Q2_K" => Quant::Q2_K,
    "Q3_K" => Quant::Q3_K,
    "Q4_K" => Quant::Q4_K,
    "Q5_K" => Quant::Q5_K,
    "Q6_K" => Quant::Q6_K,
    "Q8_K" => Quant::Q8_K,
    _ => Quant::Unknown(0),
  }
}

fn parse_external_row(row: &Value) -> Option<ManagedRow> {
  let pid = row.get("pid").and_then(Value::as_u64)? as u32;
  let path = row
    .get("model_path")
    .and_then(Value::as_str)
    .map(PathBuf::from)
    .unwrap_or_default();
  Some(ManagedRow {
    launch_id: format!("ext-{pid}"),
    path,
    // External processes don't have an observable port from
    // sysinfo cmdline alone — surface 0 and let the right pane
    // know to hide the endpoint slot for these rows.
    port: 0,
    state: SurfaceState::External,
    rss_bytes: None,
    cpu_pct: None,
  })
}

fn parse_status_row(row: &Value) -> Option<ManagedRow> {
  let launch_id = row.get("launch_id")?.as_str()?.to_string();
  let port = row.get("port")?.as_u64()? as u16;
  let path = row
    .get("id")
    .and_then(|id| id.get("path"))
    .and_then(Value::as_str)
    .map(PathBuf::from)?;
  let state_label = row
    .get("state")
    .and_then(|s| s.get("state"))
    .and_then(Value::as_str)
    .unwrap_or("");
  let state = match state_label {
    "launching" => SurfaceState::Launching,
    "loading" => SurfaceState::Loading,
    "ready" => SurfaceState::Ready,
    "error" => SurfaceState::Error,
    "stopped" => SurfaceState::Stopped,
    _ => SurfaceState::NotLaunched,
  };
  let rss_bytes = row.get("latest_rss_bytes").and_then(Value::as_u64);
  let cpu_pct = row
    .get("latest_cpu_pct")
    .and_then(Value::as_f64)
    .map(|n| n as f32);
  Some(ManagedRow {
    launch_id,
    path,
    port,
    state,
    rss_bytes,
    cpu_pct,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
  use serde_json::json;

  fn fake(path: &str, parent: &str) -> DiscoveredModel {
    DiscoveredModel {
      path: PathBuf::from(path),
      parent: PathBuf::from(parent),
      source: ModelSource::UserPath,
      metadata: Some(ModelMetadata {
        arch: Some("llama".into()),
        total_parameters: Some(7_000_000_000),
        parameter_label: Some("7B".into()),
        quant: Quant::Q4_K,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: None,
        mode_hint: ModeHint::Chat,
        weights_bytes: Some(4_200_000_000),
      }),
      parse_error: None,
      split_siblings: Vec::new(),
    }
  }

  #[test]
  fn move_up_and_down_skip_section_headers() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/x/a.gguf", "/m/x"), fake("/m/y/b.gguf", "/m/y")];
    let rows = app.rendered_rows();
    // Layout: TableHeader, header(/m/x), model a, header(/m/y), model b
    assert_eq!(rows.len(), 5);
    app.list_cursor = 2; // model a
    app.move_down();
    assert_eq!(app.list_cursor, 4, "move_down skipped header to next model");
    app.move_up();
    assert_eq!(
      app.list_cursor, 2,
      "move_up went back past the group header"
    );
  }

  #[test]
  fn cycle_theme_walks_round_robin() {
    use strum::IntoEnumIterator;
    let mut app = App::new(AppOptions::default());
    let original = app.options.theme;
    // `Custom` is skipped when no custom palette is loaded, so a full
    // lap is the count of built-in (non-Custom) themes.
    let total = ThemeName::iter()
      .filter(|t| *t != ThemeName::Custom)
      .count();
    for _ in 0..total {
      app.cycle_theme();
    }
    assert_eq!(app.options.theme, original, "wraps after one full lap");
  }

  #[test]
  fn cycle_theme_includes_custom_when_palette_loaded() {
    use strum::IntoEnumIterator;
    let custom = crate::theme::CustomThemeConfig::default().resolve().0;
    let mut app = App::new(AppOptions {
      theme: ThemeName::Macchiato,
      custom_palette: Some(custom),
      keymap: KeyMap::default(),
    });
    let total = ThemeName::iter().count();
    let mut saw_custom = false;
    for _ in 0..total {
      app.cycle_theme();
      if app.options.theme == ThemeName::Custom {
        saw_custom = true;
      }
    }
    assert!(
      saw_custom,
      "cycle should hit Custom when a palette is loaded"
    );
  }

  #[test]
  fn cycle_theme_skips_custom_when_no_palette_loaded() {
    use strum::IntoEnumIterator;
    let mut app = App::new(AppOptions::default());
    let total = ThemeName::iter().count();
    for _ in 0..total {
      app.cycle_theme();
      assert_ne!(
        app.options.theme,
        ThemeName::Custom,
        "Custom should never appear in the cycle without a loaded palette"
      );
    }
  }

  #[test]
  fn ingest_list_models_round_trips_through_ipc_shape() {
    let mut app = App::new(AppOptions::default());
    let body = json!({
      "models": [
        {
          "path": "/m/a.gguf",
          "parent": "/m",
          "source": "huggingface",
          "metadata": {
            "arch": "llama",
            "quant": "Q4_K",
            "native_ctx": 8192,
            "mode_hint": "chat",
            "parameter_label": "7B",
          },
          "parse_error": null,
          "split_siblings": []
        }
      ]
    });
    app.ingest_list_models(&body);
    assert_eq!(app.models.len(), 1);
    assert_eq!(app.models[0].path, PathBuf::from("/m/a.gguf"));
  }

  #[test]
  fn ingest_status_populates_managed_rows() {
    let mut app = App::new(AppOptions::default());
    let body = json!({
      "models": [{
        "launch_id": "L1",
        "id": {"path": "/m/a.gguf", "header_blake3": "00".repeat(32)},
        "port": 41100,
        "mode": "chat",
        "pid": 1234,
        "ready_at": null,
        "state": {"state": "ready"},
        "latest_rss_bytes": 4_500_000_000_u64,
        "latest_cpu_pct": 312.0,
      }],
      "gpu": {"backend": "cpu_only"}
    });
    app.ingest_status(&body);
    assert_eq!(app.managed.len(), 1);
    assert_eq!(app.managed[0].launch_id, "L1");
    assert_eq!(app.managed[0].state, SurfaceState::Ready);
    assert_eq!(app.managed[0].rss_bytes, Some(4_500_000_000));
    assert_eq!(app.managed[0].cpu_pct, Some(312.0));
  }

  #[test]
  fn ingest_status_populates_daemon_info_from_daemon_block() {
    let mut app = App::new(AppOptions::default());
    let body = json!({
      "daemon": {
        "pid": 2222,
        "uptime_seconds": 9_000,
        "build": "0.1.0",
        "server_path": "/usr/bin/llama-server"
      }
    });
    app.ingest_status(&body);
    assert_eq!(app.daemon_info.pid, Some(2222));
    assert_eq!(app.daemon_info.uptime_seconds, Some(9_000));
    assert_eq!(app.daemon_info.build.as_deref(), Some("0.1.0"));
    assert_eq!(
      app.daemon_info.server_path.as_deref(),
      Some("/usr/bin/llama-server")
    );
  }

  #[test]
  fn ingest_status_populates_host_metrics_from_host_block() {
    let mut app = App::new(AppOptions::default());
    let body = json!({
      "host": {
        "cpu_pct": 47.5,
        "ram_used_bytes": 1_073_741_824_u64,
        "ram_total_bytes": 17_179_869_184_u64,
        "gpu_util_pct": 84.0,
        "gpu_mem_used_bytes": 14_000_000_000_u64,
        "gpu_mem_total_bytes": 24_000_000_000_u64,
        "gpu_temp_c": 68.0,
        "gpu_backend": "nvidia",
        "gpu_device_count": 1
      }
    });
    app.ingest_status(&body);
    assert_eq!(app.host_metrics.gpu_backend, "nvidia");
    assert_eq!(app.host_metrics.ram_total_bytes, 17_179_869_184);
    assert_eq!(app.host_metrics.gpu_util_pct, Some(84.0));
  }

  #[test]
  fn ingest_status_clears_managed_when_models_field_absent() {
    // Schema-evolution or framing error must not leave a ghost
    // ManagedRow visible — symmetric with the `external` clear path.
    let mut app = App::new(AppOptions::default());
    let with_models = json!({
      "models": [{
        "launch_id": "L1",
        "id": {"path": "/m/a.gguf", "header_blake3": "00".repeat(32)},
        "port": 41100,
        "mode": "chat",
        "pid": 1,
        "ready_at": null,
        "state": {"state": "ready"}
      }]
    });
    app.ingest_status(&with_models);
    assert_eq!(app.managed.len(), 1);
    // A subsequent status with no `models` field clears managed.
    let without_models = json!({});
    app.ingest_status(&without_models);
    assert!(
      app.managed.is_empty(),
      "managed must clear when field absent"
    );
  }

  #[test]
  fn open_launch_picker_no_op_on_header_focus() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.list_cursor = 0; // header row
    app.open_launch_picker();
    assert!(
      app.launch_picker.is_none(),
      "header focus must not open a picker"
    );
  }

  #[test]
  fn open_launch_picker_carries_model_name_and_routes_to_settings_tab() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    // Rows: [TableHeader, Header(/m), Model] — model is at index 2.
    app.list_cursor = 2;
    app.open_launch_picker();
    let picker = app.launch_picker.as_ref().expect("picker state");
    assert_eq!(picker.model_name, "a");
    // New behaviour: launch picker no longer pops a centred modal;
    // it parks focus on the right pane's Settings tab and forces
    // the pane open if no model is currently running.
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Settings);
    assert!(
      app.right_pane_visible(),
      "right pane must be visible after open_launch_picker"
    );
  }

  fn ready_managed(path: &str, port: u16, state: SurfaceState) -> ManagedRow {
    ManagedRow {
      launch_id: format!("L-{port}"),
      path: PathBuf::from(path),
      port,
      state,
      rss_bytes: None,
      cpu_pct: None,
    }
  }

  #[test]
  fn right_pane_follows_cursor_no_sticky_fallback() {
    // Two models — one running (qwen), one not (phi). Cursor on the
    // running row exposes the running model's mode-appropriate tabs;
    // moving the cursor to the unlaunched row collapses the tab set
    // to Settings only and clears `right_pane_focus()` — no sticky
    // reference to qwen survives.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m"), fake("/m/phi.gguf", "/m")];
    app.managed = vec![ready_managed("/m/qwen.gguf", 41100, SurfaceState::Ready)];
    // Row layout: [TableHeader, Header(/m), Model(phi), Model(qwen)].
    // (alphabetical: phi < qwen). Place cursor on qwen first.
    app.list_cursor = 3;
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Logs, RightTab::Chat, RightTab::Settings],
      "running selection exposes mode-appropriate tabs"
    );
    assert!(app.right_pane_focus().is_some());
    // Now cursor on phi (not launched).
    app.list_cursor = 2;
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Settings],
      "unlaunched selection collapses to Settings only"
    );
    assert!(
      app.right_pane_focus().is_none(),
      "no sticky fallback — right pane has no managed handle to draw"
    );
  }

  #[test]
  fn loading_selection_shows_logs_and_settings_but_not_chat() {
    // A model mid-startup (Launching/Loading) keeps Logs visible so
    // the user can watch the pipeline; Chat would mislead — the
    // server isn't accepting requests yet.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    app.managed = vec![ready_managed("/m/qwen.gguf", 41100, SurfaceState::Loading)];
    app.list_cursor = 2;
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Logs, RightTab::Settings]
    );
  }

  #[test]
  fn filter_keeps_matching_models_and_drops_empty_groups() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![
      fake("/m/x/qwen.gguf", "/m/x"),
      fake("/m/y/phi.gguf", "/m/y"),
    ];
    app.filter_buffer = "qwen".into();
    let rows = app.rendered_rows();
    let names: Vec<String> = rows
      .iter()
      .filter_map(|r| match r {
        ListRow::Model { name, .. } => Some(name.clone()),
        _ => None,
      })
      .collect();
    assert_eq!(names, vec!["qwen".to_string()]);
    let headers: Vec<String> = rows
      .iter()
      .filter_map(|r| match r {
        ListRow::Header { label } => Some(label.clone()),
        _ => None,
      })
      .collect();
    assert_eq!(
      headers,
      vec!["/m/x".to_string()],
      "empty groups must be dropped"
    );
  }

  #[test]
  fn toast_expires_after_ttl() {
    let mut app = App::new(AppOptions::default());
    app.show_toast("yanked");
    assert!(app.toast_message().is_some());
    // Backdate the toast to force expiry.
    if let Some((_, ref mut at)) = app.toast {
      *at = Instant::now() - Duration::from_secs(10);
    }
    app.expire_toast();
    assert!(app.toast_message().is_none());
  }
}
