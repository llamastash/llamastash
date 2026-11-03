//! TUI application state.
//!
//! The render loop and the event loop are in [`super::events`]; this
//! module is the pure state machine they drive. Keeping it pure lets
//! the TestBackend smoke test and the inline unit tests assert
//! behaviour without spinning up tokio.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use serde_json::Value;

use crate::daemon::host_metrics::HostMetricsSnapshot;
use crate::discovery::DiscoveredModel;
use crate::theme::{palette_for, Palette, ThemeName};
use crate::tui::filter::rank;
use crate::tui::keybindings::{Action, Focus, KeyMap};
use crate::tui::launch_picker::{LaunchPickerState, PresetChoice, PresetStop};
use crate::tui::list_pane::{build_rows, ListRow, RowInputs, RunningLaunchRow};
use crate::tui::status_icons::SurfaceState;
use crate::tui::tabs::{tabs_for_mode, tabs_without_mode, RightTab};

/// Maximum age of a toast before the App auto-clears it. Keeps
/// transient yank confirmations from sticking around forever.
const TOAST_TTL: Duration = Duration::from_secs(3);

/// Severity of a transient toast. Drives the colour the bar paints
/// in: `Info` rides the theme accent (neutral confirmations like
/// "copied logs", refusal guards like "nothing to copy"), while
/// `Error` paints on `palette.error` so genuine failures (clipboard
/// unavailable, writer offline, port in use) read as red rather than
/// masquerading as a positive accent confirmation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToastKind {
  #[default]
  Info,
  Error,
}

/// Body width (cells) at which the dashboard flips from wide-pane
/// to compact-pane mode. At ≥ 100 cells both panes render side by
/// side (current 65/35 layout). Below 100 cells the right pane is
/// "drill-in only" — hidden by default, visible whenever focus
/// lives inside it (see [`App::right_pane_visible_at`]).
pub const COMPACT_WIDTH_THRESHOLD: u16 = 100;

/// VRAM the fit math sizes against: the smallest card the ranked backend can
/// actually use, which is what `init::detection::aggregate_vram_bytes` does and
/// what a launch gets. The daemon's `gpu_mem_total_bytes` sums every device, so
/// a 2x24 GiB box would rank models neither card runs, and a hybrid laptop
/// would fold the iGPU's memory into the RTX's. Falls back to the sum when the
/// metrics carry no usable device rows.
fn ranked_vram_bytes(metrics: &HostMetricsSnapshot, backend: &str) -> Option<u64> {
  let devices = metrics.gpu_devices.as_deref().unwrap_or(&[]);
  let multi = metrics.gpu_backend == HostMetricsSnapshot::BACKEND_MULTI;
  let smallest = devices
    .iter()
    .filter(|d| !multi || recommender_backend_key(&d.backend) == backend)
    .map(|d| d.total_memory_bytes)
    .filter(|&bytes| bytes > 0)
    .min();
  smallest.or(metrics.gpu_mem_total_bytes)
}

/// Map the daemon's wire-stable `gpu_backend` label
/// (`"nvidia"`/`"amd"`/...) to the recommender's per-backend key
/// (`"cuda"`/`"hip"`/...). Kept here so the dialog stays decoupled
/// from the daemon's sampler vocabulary. R113: `"unknown"` and
/// `"unsampled"` pass through verbatim so `vram_fit_for_file` can
/// return `FileFit::Unknown`.
fn recommender_backend_key(wire: &str) -> &'static str {
  use crate::daemon::host_metrics::HostMetricsSnapshot as H;
  match wire {
    H::BACKEND_NVIDIA => "cuda",
    H::BACKEND_AMD => "hip",
    H::BACKEND_APPLE_METAL => "metal",
    H::BACKEND_CPU_ONLY => "cpu",
    // Vulkan-only or never-sampled fall through; the dialog renders
    // `FileFit::Unknown` rather than fake confidence.
    H::BACKEND_UNKNOWN => "unknown",
    _ => "unknown",
  }
}

/// Backend key for a `multi`-backend host, which the wire label alone cannot
/// answer: pick from the devices the sampler found, in the CUDA > ROCm >
/// Vulkan order `init`'s VRAM ceiling uses, so a hybrid laptop ranks against
/// the card that will run the model instead of reporting no usable backend.
fn multi_recommender_backend(devices: &[crate::daemon::host_metrics::DeviceRow]) -> &'static str {
  use crate::daemon::host_metrics::HostMetricsSnapshot as H;
  if devices.iter().any(|d| d.backend == H::BACKEND_NVIDIA) {
    "cuda"
  } else if devices.iter().any(|d| d.backend == H::BACKEND_AMD) {
    "hip"
  } else {
    "unknown"
  }
}

/// How many entries the `↺ Recent` section surfaces. Five matches
/// what the user picked during planning; the daemon's storage
/// itself isn't capped — the cap is purely a render-side window.
const RECENT_LIST_CAP: usize = 5;

/// How many status ticks a bell watch survives with no row for its model, at
/// the refresh cadence in `events` (~750 ms each). Comfortably longer than the
/// gap between a dispatch and the row it creates, short enough that a start
/// which never happened stops waiting before the next launch of that model.
const BELL_WATCH_MAX_MISSES: u32 = 8;

/// One launch this TUI started, waiting for its bell.
///
/// Rows are matched by path, and the same model can hold several rows, so a
/// watch also carries what it knows about the launch:
/// * `known`: the launch ids on screen when the launch was queued. Those rows
///   predate it, so they can never be its end.
/// * `launch_id`: the id the daemon named in its `start_model` reply, once that
///   arrives. Until then any row for the path that is not `known` is a
///   candidate, which is why `misses` (ticks with no candidate) still expires a
///   watch whose launch never appeared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BellWatch {
  pub path: PathBuf,
  pub misses: u32,
  pub known: Vec<String>,
  pub launch_id: Option<String>,
}

impl BellWatch {
  /// A watch that has not missed a tick yet.
  pub fn waiting(path: impl Into<PathBuf>) -> Self {
    Self {
      path: path.into(),
      misses: 0,
      known: Vec::new(),
      launch_id: None,
    }
  }

  /// The rows this watch may still consider its own launch.
  fn candidate(&self, row: &ManagedRow) -> bool {
    row.path == self.path
      && match &self.launch_id {
        Some(id) => row.launch_id == *id,
        None => !self.known.contains(&row.launch_id),
      }
  }
}

/// In-memory snapshot of one launched model the daemon is
/// supervising. Mirrors the IPC `status` shape — kept in App so
/// the right-pane header can show port/state without re-querying.
#[derive(Debug, Clone, Default)]
pub struct ManagedRow {
  pub launch_id: String,
  pub path: PathBuf,
  pub port: u16,
  /// The process id, set only for external rows: such a row has no launch of
  /// ours behind it, so the pid is the only thing that identifies it.
  pub pid: Option<u32>,
  /// User-chosen launch name (from `--name`). When set, the launch is
  /// addressable as `<model-id>@<name>`; the TUI surfaces it in the
  /// running-launch view so a named launch is distinguishable from an
  /// unnamed one of the same model. `None` for unnamed launches.
  pub name: Option<String>,
  /// The preset this launch resolved (`--preset`, a named picker stop, the
  /// model's `default:`, or a `<model>@<name>` auto-start address), from the
  /// status row. `None` when no preset was in play; rendered as a read-only
  /// `preset` row in the running-launch Settings view.
  pub preset: Option<String>,
  pub state: SurfaceState,
  /// Launch device selector (`CUDA0`, `Vulkan1`, etc.) when set.
  pub device: Option<String>,
  /// Latest per-PID RSS reading in bytes. `None` until the daemon's
  /// per-launch sampler has emitted at least one reading.
  pub rss_bytes: Option<u64>,
  /// Latest per-PID CPU usage percent (multi-core, may exceed 100%).
  /// `None` until the daemon's per-launch sampler primes.
  pub cpu_pct: Option<f32>,
  /// Context window `--fit` actually resolved, read from the child's
  /// `/props` after Ready. `None` until that fetch lands (or when
  /// the build omits it). The running-launch settings view shows this
  /// real number instead of the dispatched `auto` sentinel.
  pub resolved_ctx: Option<u32>,
  /// True when `--fit` had to clamp the context window down to the floor
  /// under memory pressure. The running view tags the resolved ctx
  /// with a "clamped" note so the user knows it was squeezed.
  pub ctx_clamped: bool,
  /// The knobs this launch was actually dispatched with (the live
  /// `status` `params.knobs`), so the running-launch settings view shows
  /// what the server is *running* with — `auto` for a fit-delegated
  /// knob, a pinned number when set — rather than the user's saved
  /// `last_params` delta (which can be empty even for an auto launch).
  pub knobs: crate::launch::knobs::KnobSet,
  /// The advanced `--` argv tail this launch was dispatched with (the
  /// live `status` `params.extras`). Empty for external rows. Lets
  /// `Ctrl+P` save-from-running carry the advanced args into the preset.
  pub extras: Vec<String>,
  /// Backend this launch actually resolved to (`status` `backend`): the
  /// engine the launch dispatched to. Keyed
  /// on by the backend badge / knob panel so a running row reflects the real
  /// backend, not the `list_models` routing prediction. `None` when untagged.
  pub backend: Option<String>,
  /// Server (build/binary) id the launch picked (`status` `params.server`),
  /// when one was chosen. Drives the running view's read-only `server` row so
  /// the operator can see which build served the model. `None` when the launch
  /// took the backend's default server (no explicit pick).
  pub server: Option<String>,
}

/// Persisted "last successful launch params" for one model, fetched
/// via the daemon's `last_params_list` method. The TUI consults this
/// when opening the launch picker so the user lands on the same
/// ctx/reasoning/advanced they last shipped — without re-typing.
#[derive(Debug, Clone, Default)]
pub struct LastParamsRow {
  pub ctx: Option<u32>,
  pub reasoning: bool,
  /// User-supplied typed-knob deltas from the last successful launch
  /// (the *user's* contribution, not the resolved set). The editor
  /// seeds its `user_knobs` row directly from this so a returning
  /// user lands on the same overrides; rows the user never touched
  /// re-resolve from yaml / built-in / model default.
  pub knobs: crate::launch::knobs::KnobSet,
  /// Free-form argv tail that landed on `--`. Surfaces back in the
  /// editor's `extras` row.
  pub extras: Vec<String>,
  /// Port the model was last successfully bound on. The picker
  /// passes this back as a soft preference (`prefer_port`) so a
  /// returning user lands on the same port if it's still free.
  pub port: Option<u16>,
  /// Server id (build/binary) the last launch used, when one was picked.
  /// Seeds the picker's Server row so a relaunch reuses the same build.
  pub server: Option<String>,
}

/// Snapshot of the daemon-side metadata the Daemon info panel
/// renders. Mirrors the `daemon` object on the `status` response.
#[derive(Debug, Clone, Default)]
pub struct DaemonInfo {
  pub pid: Option<u32>,
  pub uptime_seconds: Option<u64>,
  pub build: Option<String>,
  pub server_path: Option<String>,
  /// HTTP control-plane URL the daemon bound on (e.g.
  /// `http://127.0.0.1:48134`). Rendered in the Daemon info panel
  /// alongside pid + uptime so an operator can see at a glance where
  /// the IPC channel is. `None` when the daemon hasn't surfaced the
  /// field (pre-Phase-A binaries don't).
  pub ipc_url: Option<String>,
  /// Latest snapshot of the OpenAI-compat proxy listener.
  /// `None` when talking to a pre-Unit-5 daemon that omits the
  /// field — info_pane renders the proxy row as `proxy   —` in that case.
  pub proxy: Option<ProxyInfo>,
  /// One entry per *enabled* backend with at least one server, from the
  /// `status.backends` rows. The Daemon panel's server row renders each as
  /// `<id> (<label>|<label>)`, so new backends appear with no per-backend TUI
  /// code.
  pub backend_servers: Vec<BackendServers>,
}

/// One backend's server labels from the `status.backends` array, in catalog
/// order (`["rocm", "vulkan"]`). Before the server catalog is probed, the
/// resolved binary's file name stands in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendServers {
  pub id: String,
  pub labels: Vec<String>,
}

/// Wire shape of the proxy listener block surfaced via the IPC
/// `status` response. Parsed from the daemon's JSON and held
/// on `DaemonInfo` so [`crate::tui::info_pane`] can render a one-line
/// summary in the Daemon panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyInfo {
  /// Mirrors config: `false` when `proxy.enabled: false`. Tracks
  /// whether the listener was *intended* to be up, separately from
  /// whether the bind succeeded.
  pub enabled: bool,
  /// Attempted bind address. `None` only when `enabled == false`.
  pub listen: Option<String>,
  /// One of `disabled` / `listening` / `port_in_use` / `unbound` /
  /// `refused_insecure`.
  pub status: String,
  /// OS-level cause when `status == "unbound"`; the fix hint when
  /// `status == "refused_insecure"`; `None` otherwise.
  pub bind_error: Option<String>,
  /// Auth posture: `"enforced"` (a bearer key is required on the data
  /// routes), `"none"`, or `"required"` (the `refused_insecure` case).
  /// `None` when an older daemon omits the field.
  pub auth: Option<String>,
}

/// Per-frame cache of the screen rectangles that mouse-focus needs
/// to hit-test against. Populated by [`crate::tui::render`] each
/// frame; consumed by the click handler in [`crate::tui::events`].
///
/// Lives in a `RefCell` because the render path takes `&App` (the
/// renderer is read-mostly and we keep the borrow non-mutable so the
/// per-frame memo helpers stay simple). The struct is dirt-cheap to
/// rewrite each frame — five `Rect`s + a small `Vec` — so there's no
/// staleness concern: the event loop only ever reads what the most
/// recent draw produced.
#[derive(Debug, Clone, Default)]
pub struct MouseHitRects {
  /// Models list pane (left half of the body, full-body when the
  /// right pane is hidden). Empty `Rect` until the first draw.
  pub list_pane: Rect,
  /// Right pane outer area (border included). Empty when the right
  /// pane is hidden for this frame.
  pub right_pane: Rect,
  /// Per-tab clickable spans inside the right pane's title strip.
  /// Each entry is the exact cell range for one label so the hit
  /// test is `mouse.x ∈ rect && mouse.y == rect.y`. Empty when the
  /// right pane is hidden.
  pub right_tabs: Vec<(RightTab, Rect)>,
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
  /// Resolved offline mode for this TUI session. `true` when the
  /// user passed `--offline` on the CLI or `LLAMASTASH_OFFLINE=1`
  /// at startup. Threads into the HF dialog's FetchClient + the
  /// download dispatch so a pull confirmation can't trigger network
  /// I/O behind the user's back.
  pub offline: bool,
  /// Opt into terminal mouse capture (`mouse_focus: true` in
  /// `config.yaml`). When enabled, [`super::events::run`] turns on
  /// SGR mouse reporting so a left-click on the Models list or a
  /// right-pane tab label moves focus / switches tab. The default is
  /// off so the terminal keeps native click-and-drag text selection
  /// — see the long comment in [`super::events::run`] for the trade.
  pub mouse_focus: bool,
  /// Ring the terminal bell on the ends the user is waiting for: TUI downloads
  /// and launches this TUI started. From `bell: true` in `config.yaml`
  /// (the factory). See [`crate::util::bell`].
  pub bell: bool,
  /// Left (Models list) pane width percentages the `Alt+L` shortcut cycles
  /// through in wide mode. Already sanitized (≤5 slots, each `0..=100`, never
  /// empty) by [`crate::config::loader::sanitize_left_pane_ratios`]. Slot 0 is
  /// the startup default; `App::left_pane_ratio_slot` tracks the live pick.
  pub left_pane_ratios: Vec<u16>,
}

impl Default for AppOptions {
  fn default() -> Self {
    Self {
      theme: ThemeName::Macchiato,
      custom_palette: None,
      keymap: KeyMap::default(),
      offline: false,
      mouse_focus: false,
      bell: true,
      left_pane_ratios: crate::config::loader::default_left_pane_ratios(),
    }
  }
}

/// Central App state.
#[derive(Debug)]
pub struct App {
  pub options: AppOptions,
  pub focus: Focus,
  pub models: Vec<DiscoveredModel>,
  /// The daemon's per-model `list_models` `backend` prediction, keyed by
  /// canonical path (the engine's id). The daemon already
  /// applied each backend's availability + routing predicate and the source
  /// mapping, so this is the single source of truth for "where would a plain
  /// launch of this model route" — it drives the launch picker's
  /// `model_backend`, the right-pane backend badge, and the Models-list Backend
  /// column, with no backend logic re-derived TUI-side. Refreshed with `models`.
  pub backend_by_path: std::collections::BTreeMap<PathBuf, String>,
  pub favorites: Vec<PathBuf>,
  pub managed: Vec<ManagedRow>,
  /// External (unmanaged) `llama-server` processes the daemon's
  /// sweep surfaced. Read-only — the TUI shows them with the `⇪`
  /// glyph; only `stop` is permitted on these rows.
  pub external: Vec<ManagedRow>,
  /// Last-known persisted launch params per model path. Keyed off
  /// the canonical `ModelId.path` the daemon emits.
  pub last_params: BTreeMap<PathBuf, LastParamsRow>,
  /// Raw config `presets:` blocks from `presets_all`, keyed by model
  /// name / arch id. The launch picker resolves each model's effective
  /// set against the catalog (see [`crate::launch::presets`]) to build
  /// its preset cycle. Empty until the first refresh lands.
  pub config_presets: BTreeMap<String, crate::config::ConfigPresetBlock>,
  /// Top-N recently-launched paths in recency order (most recent
  /// first). Surfaced via the `↺ Recent` section. Populated from
  /// `last_params_list`; see `RECENT_LIST_CAP`.
  pub recent_paths: Vec<PathBuf>,
  /// Selected right-pane tab. `Settings` is always reachable;
  /// `Logs` plus the mode-specific tab (`Chat` / `Embed` /
  /// `Rerank`) become reachable when the focused model is running.
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
  /// Requests-tab data for the focused model. Refreshed from the
  /// daemon's `requests_tail` IPC method while the tab is open.
  pub requests: crate::tui::tabs::requests::RequestsTabState,
  /// Cursor index into the rendered row list (which mixes headers
  /// and models). Header rows are skipped during `move_*`.
  pub list_cursor: usize,
  /// Filter input — modal text field backed by [`crate::tui::input_field::InputField`] so the
  /// editing semantics (`e` enters edit, `Esc` walks back exit-edit
  /// → clear → close) match every other text input. Filter auto-
  /// enters edit on `open_filter` so the user can type immediately.
  pub filter_input: crate::tui::input_field::InputField,
  pub launch_picker: Option<LaunchPickerState>,
  /// Vertical scroll offset for the Settings tab's read-only
  /// running-launch view. The view shows ~18 rows (launch id, 15
  /// typed knobs, extras, footer); on a short viewport the user
  /// scrolls with ↑/↓. Resets on model-list nav and when the picker
  /// opens or closes.
  ///
  /// `Cell` so the render path (which holds `&App`) can write back
  /// the clamped value when the stored offset drifts past
  /// `max_scroll` — without writeback, holding ↓ past the bottom
  /// would inflate the stored offset and the next ↑ press would
  /// appear to do nothing until the value dropped back below
  /// `max_scroll`. Same pattern as
  /// [`LaunchPickerState::scroll_offset`].
  pub running_view_scroll: Cell<u16>,
  pub toast: Option<(String, Instant, ToastKind)>,
  pub daemon_connected: bool,
  /// Why the startup auto-spawn refused to bring a daemon up (the
  /// backend fail-fast precheck: a missing `llama-server` binary, or
  /// nothing else to launch). One failure
  /// per line. Rendered in the Daemon panel's server row while no
  /// daemon is connected; cleared the moment any daemon responds.
  pub daemon_start_error: Option<String>,
  /// Snapshot of the daemon-side metadata block from the most recent
  /// `status` response. Populated by [`Self::ingest_status`].
  pub daemon_info: DaemonInfo,
  /// Latest host-level CPU/RAM/GPU readings the daemon's sampler
  /// emits. Populated by [`Self::ingest_status`]; consumed by the
  /// Host info-row pane.
  pub host_metrics: HostMetricsSnapshot,
  /// Neutral server catalog from `status.servers` — every backend's build /
  /// binary variants with their probed `--device` selectors. The single
  /// launch-device source: the daemon-pane `server` row (grouped by backend),
  /// the launch picker's Server + Device rows, and the multi-GPU gates all
  /// derive from it. Populated by [`Self::ingest_status`].
  pub servers: Vec<crate::backend::Server>,
  /// Set when the user presses `q` so the event loop can exit.
  pub should_exit: bool,
  /// Set by the first `q` while a delete runs; the second `q` quits.
  pub quit_confirm: bool,
  /// The path a delete is running on. Set from the confirm until the worker's
  /// outcome lands; drives the row's `deleting…` marker, refuses a second
  /// `Ctrl+D`, and makes the first `q` ask again.
  pub delete_in_flight: Option<PathBuf>,
  /// Whether the modal help overlay is visible. Bound to `?`.
  pub show_help: bool,
  /// Vertical scroll offset for the help overlay (in lines). Lets the
  /// overlay survive on terminals too short to fit every category.
  /// Reset to `0` whenever the overlay closes; advanced by `j`/`k`,
  /// arrow keys, and PgUp/PgDn while it's open.
  pub help_scroll: u16,
  /// True after `g` was pressed in the right pane and we're awaiting
  /// the second half of a vim-style `gt` / `gT` chord. Cleared on the
  /// next keystroke regardless of whether the chord completed — one-
  /// shot, no timeout.
  pub pending_g_prefix: bool,
  /// Modal "are you sure?" prompt. `Some(...)` shows a centred
  /// confirmation overlay that captures `y` / Enter to dispatch
  /// the inner action and `n` / Esc to dismiss. Used by stop-model
  /// and kill-daemon so a fat-finger doesn't drop a running model
  /// or the whole supervisor.
  pub confirm_dialog: Option<ConfirmAction>,
  /// HuggingFace pull dialog. `Some(_)` whenever the modal
  /// is open; the input pump routes through `Focus::HfDialog` to the
  /// per-stage key handler.
  pub hf_dialog: Option<crate::tui::hf_dialog::HfDialogState>,
  /// Benchmark snapshot newer than the one compiled into the binary, once the
  /// dialog's one background refresh brings one in. Cached for the session so
  /// every later dialog open ranks from it without another fetch; `None` keeps
  /// the bundled copy as the source.
  pub hf_snapshot: Option<crate::init::benchmark::BenchmarkSnapshot>,
  /// `Ctrl+P` save-preset dialog. `Some(_)` while the modal is open; the
  /// input pump routes keys to its name / overwrite stages.
  pub save_preset_dialog: Option<crate::tui::save_preset_dialog::SavePresetDialog>,
  pub memory_dialog: Option<crate::tui::memory_dialog::MemoryDialog>,
  pub memory_budget: Option<crate::daemon::memory_policy::MemoryBudget>,
  /// `Alt+⏎` launch-name dialog. `Some(_)` while the modal is open; on accept
  /// the normal launch picker opens carrying the typed name.
  pub launch_name_dialog: Option<crate::tui::launch_name_dialog::LaunchNameDialog>,
  /// Pinned download status strip. Always present; the
  /// renderer reserves a 1-line slot above the body only when
  /// `download_strip.is_active()` is true.
  pub download_strip: crate::tui::download_strip::DownloadStripState,
  /// Launches this TUI started that are still waiting to ring. One entry per
  /// launch: a duplicate launch of a model already loading gets its own entry,
  /// so two launches ring twice. Launches started elsewhere (CLI, `preload`)
  /// are never watched, so they never ring here.
  pub bell_watch: Vec<BellWatch>,
  /// Per-frame memo of `rendered_rows()`. Primed at the top of
  /// `render::render` and cleared at the bottom — the biggest single
  /// per-frame perf win. The same `Vec<ListRow>`
  /// used to be rebuilt 5+ times per frame via `focused_path`,
  /// `focused_managed`, `focused_name`, and the right-pane render
  /// helpers. None outside a frame so event handlers always see
  /// fresh state.
  pub(crate) rows_cache: Option<Vec<ListRow>>,
  /// Per-frame memo of `available_right_tabs()`. Three calls per
  /// frame used to walk `models` linearly + allocate a fresh
  /// `Vec<RightTab>` each time. Same lifetime
  /// rules as `rows_cache`.
  pub(crate) right_tabs_cache: Option<Vec<RightTab>>,
  /// Hit-test rectangles refreshed every frame by the renderer.
  /// Read by the mouse-click dispatch in [`crate::tui::events`] so a
  /// `MouseEventKind::Down(Left)` can resolve `(column, row)` to a
  /// focus / right-tab without the event handler re-computing the
  /// layout. Only meaningful when `options.mouse_focus` is true; left
  /// at its default empty state otherwise.
  pub(crate) hit_rects: RefCell<MouseHitRects>,
  /// Sender into the unified TUI event channel. `Some(_)` during a
  /// real `run()` session; `None` in unit tests that drive `pump_input`
  /// directly without spinning a runtime. Subsystems (chat stream,
  /// embed/rerank, HF dialog, download tasks) clone this when
  /// spawning a tokio task so their results land back on the same
  /// `recv` the main loop blocks on.
  pub events_tx: Option<tokio::sync::mpsc::Sender<crate::tui::events::Event>>,
  /// Previous observed `proxy.status` label. Used by
  /// [`Self::ingest_status`] to fire a one-shot toast on the
  /// transition into `port_in_use` (plan §Approach: "flag on first
  /// observation"). Reading the cell on every tick would surface a
  /// toast each refresh; transitions keep the noise to once-per-
  /// session-collision.
  pub(crate) last_proxy_status: Option<String>,
  /// Live index into [`AppOptions::left_pane_ratios`] — the wide-mode split the
  /// `Alt+L` shortcut cycles. Session-only: starts at slot 0 each launch.
  pub(crate) left_pane_ratio_slot: usize,
}

/// Fully-resolved launch request shared by `WriterCmd::StartModel`
/// and [`ConfirmAction::LaunchDuplicate`]. `ctx` / `reasoning` are
/// projected out of `knobs` for the wire payload's backward-compat
/// top-level fields. Boxed in both enums so the large `crate::launch::knobs::KnobSet`
/// payload doesn't bloat every other (tiny) variant — the imbalance
/// `clippy::large_enum_variant` flags.
#[derive(Debug, Clone)]
pub struct StartModelArgs {
  pub model_path: PathBuf,
  pub ctx: Option<u32>,
  pub reasoning: Option<bool>,
  pub knobs: crate::launch::knobs::KnobSet,
  pub extras: Vec<String>,
  pub mode: Option<crate::launch::mode::LaunchMode>,
  pub prefer_port: Option<u16>,
  /// Per-model backend choice from the Launch picker. `Auto` runs
  /// the identity rule on the daemon side.
  pub backend: crate::launch::params::BackendChoice,
  /// Launch selection signal for the daemon resolver. The TUI flattens
  /// the form client-side, so a form launch is `"explicit"`; the `auto`
  /// cycle stop sends `"auto"` (pure fit). Never `"default"` — the TUI
  /// always resolves a concrete stop.
  pub selection: &'static str,
  /// Chosen server id (a build/binary of a backend) from the picker's Server
  /// row, or `None` for the priority default. Sent as `LaunchParams.server`;
  /// the daemon derives the binary (and, when `backend` is `Auto`, the backend)
  /// from it.
  pub server: Option<String>,
  /// User-chosen launch name (from the `Alt+⏎` "launch as…" dialog). `None`
  /// for a plain `⏎` launch, which stays unnamed as before. Sent as the
  /// `name` start param so the launch is addressable as `<model-id>@<name>`.
  pub name: Option<String>,
  /// The named preset stop the form launched from, when it did. Sent so the
  /// daemon can stamp the running row; `None` for the `last used` / `auto`
  /// stops (the values are flattened into `knobs`/`extras` either way).
  pub preset: Option<String>,
}

/// Which server a focused running model launched on when it uses a
/// **non-default** build: its backend and its position among that backend's
/// catalog servers, which is also its index in the backend's `server_labels`.
/// Returned by [`App::focused_override_device`] so the daemon-pane `server`
/// row can highlight that label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideServer {
  pub backend_id: String,
  pub index: usize,
}

/// How alarming a confirm prompt is, so the overlay can tone its
/// border/title accordingly. `Destructive` (red) is reserved for
/// prompts that lose work or data — stopping/killing a process,
/// deleting a file, cancelling a download. `Neutral` (accent/warning)
/// is for reversible or additive prompts where red would cry wolf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmSeverity {
  Destructive,
  Neutral,
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
  /// `Ctrl+R:restart daemon` — shuts the daemon down and re-spawns
  /// a fresh one. All managed launches are stopped in the process.
  RestartDaemon,
  /// `Ctrl+D:delete` — remove a non-running model from disk. The plan
  /// (shards, mmproj projector, MTP draft head, HF cache blobs) is
  /// resolved when the key is pressed so the popup can name what goes
  /// and the confirm handler does no fresh discovery.
  DeleteModel {
    plan: Box<crate::tui::delete::DeletePlan>,
    display_name: String,
  },
  /// `Ctrl+X:cancel download` — abort the currently-active HF
  /// download. The queue stays intact; the next queued pull is
  /// promoted on confirm. `friendly_name` is what the popup renders
  /// so the user reads the same identifier the strip is showing.
  CancelDownload {
    repo_id: String,
    friendly_name: String,
  },
  /// `Enter:launch` on a model that already has a managed launch
  /// (round-8). v1 supports duplicate launches on fresh ports, but
  /// we ask the user to confirm so a stray Enter doesn't silently
  /// spin up another instance. The payload mirrors
  /// `WriterCmd::StartModel` — captured at popup time so the
  /// launch dispatches even if focus moves while the popup is up.
  LaunchDuplicate {
    /// Display name for the popup body.
    name: String,
    /// Existing managed instance count, surfaced in the popup
    /// body so the user understands what they're piling on top of.
    active_instances: usize,
    /// The launch to dispatch on confirm — boxed and shared with
    /// `WriterCmd::StartModel`.
    args: Box<StartModelArgs>,
  },
}

impl ConfirmAction {
  /// Severity tier for the confirm overlay's border/title tone. Only
  /// the work-losing prompts read red; an additive duplicate-launch
  /// prompt stays neutral so red keeps meaning "you're about to lose
  /// something".
  pub fn severity(&self) -> ConfirmSeverity {
    match self {
      ConfirmAction::StopModel { .. }
      | ConfirmAction::KillDaemon
      | ConfirmAction::RestartDaemon
      | ConfirmAction::DeleteModel { .. }
      | ConfirmAction::CancelDownload { .. } => ConfirmSeverity::Destructive,
      ConfirmAction::LaunchDuplicate { .. } => ConfirmSeverity::Neutral,
    }
  }
}

impl App {
  pub fn new(options: AppOptions) -> Self {
    Self {
      options,
      focus: Focus::List,
      models: Vec::new(),
      backend_by_path: std::collections::BTreeMap::new(),
      favorites: Vec::new(),
      managed: Vec::new(),
      external: Vec::new(),
      last_params: BTreeMap::new(),
      config_presets: BTreeMap::new(),
      recent_paths: Vec::new(),
      right_tab: RightTab::Settings,
      chat: Default::default(),
      embed: Default::default(),
      rerank: Default::default(),
      logs_state: Default::default(),
      requests: Default::default(),
      list_cursor: 0,
      filter_input: crate::tui::input_field::InputField::new(),
      launch_picker: None,
      running_view_scroll: Cell::new(0),
      toast: None,
      daemon_connected: false,
      daemon_start_error: None,
      daemon_info: DaemonInfo::default(),
      host_metrics: HostMetricsSnapshot::default(),
      hf_snapshot: None,
      servers: Vec::new(),
      should_exit: false,
      quit_confirm: false,
      delete_in_flight: None,
      show_help: false,
      help_scroll: 0,
      pending_g_prefix: false,
      confirm_dialog: None,
      hf_dialog: None,
      save_preset_dialog: None,
      memory_dialog: None,
      memory_budget: None,
      launch_name_dialog: None,
      download_strip: crate::tui::download_strip::DownloadStripState::default(),
      bell_watch: Vec::new(),
      rows_cache: None,
      right_tabs_cache: None,
      hit_rects: RefCell::new(MouseHitRects::default()),
      events_tx: None,
      last_proxy_status: None,
      left_pane_ratio_slot: 0,
    }
  }

  /// The left (Models list) pane width % for the current cycle slot, used by
  /// the wide-mode body split. Falls back to the factory default if the slot
  /// list is somehow empty (sanitizer guarantees it isn't).
  pub fn left_pane_ratio(&self) -> u16 {
    let slots = &self.options.left_pane_ratios;
    slots.get(self.left_pane_ratio_slot).copied().unwrap_or(65)
  }

  /// Advance `Alt+L` to the next left/right split slot, wrapping. Session-only
  /// (never persisted). No-op when only one slot is configured.
  pub fn cycle_left_pane_ratio(&mut self) {
    let len = self.options.left_pane_ratios.len();
    if len > 0 {
      self.left_pane_ratio_slot = (self.left_pane_ratio_slot + 1) % len;
    }
  }

  /// `true` when the download strip should be rendered. Delegates
  /// to [`crate::tui::download_strip::DownloadStripState`] so the renderer's layout
  /// decision and the strip's render contract stay aligned.
  pub fn download_strip_active(&self) -> bool {
    self.download_strip.is_active()
  }

  /// Open the HuggingFace pull dialog. Initialises in the Search
  /// stage and snaps focus into [`Focus::HfDialog`] so the per-stage
  /// key router takes over. The dialog reads its offline flag from
  /// [`AppOptions::offline`] (which is itself the runtime-resolved
  /// `--offline` ∨ `LLAMASTASH_OFFLINE` value) so the "search
  /// disabled" hint renders immediately and the dialog's spawned
  /// fetch tasks short-circuit before any HF traffic.
  pub fn open_hf_dialog(&mut self) {
    if self.hf_dialog.is_none() {
      // One snapshot read for both the fit band and the recommendation
      // ranking, so opening the dialog parses it once. The bundled copy ranks
      // immediately; the remote tier `init` ranks against arrives behind the
      // same event channel the HF search uses, if it is newer.
      let offline = self.options.offline || crate::init::fetch::offline_requested(false);
      let snapshot = self
        .hf_snapshot
        .clone()
        .unwrap_or_else(crate::init::benchmark::load_bundled);
      let ctx = self.hf_hardware_fit_ctx(&snapshot);
      let mut state = crate::tui::hf_dialog::HfDialogState::open(offline, ctx);
      state.recs = state.hardware_fit_ctx.recommendations(&snapshot);
      self.hf_dialog = Some(state);
      if self.hf_snapshot.is_none() && !offline {
        crate::tui::hf_pull::spawn_hf_snapshot_refresh(offline, self.events_tx.clone());
      }
    }
    self.focus = Focus::HfDialog;
  }

  /// Snapshot the inputs `vram_fit_for_file` needs into the dialog
  /// state at open time. Backend / VRAM / RAM come from the
  /// daemon's host-metrics sampler; the per-backend overhead band
  /// is read from the bundled benchmark snapshot. R111 + R113.
  pub(crate) fn hf_hardware_fit_ctx(
    &self,
    snapshot: &crate::init::benchmark::BenchmarkSnapshot,
  ) -> crate::tui::hf_dialog::HardwareFitContext {
    use crate::tui::hf_dialog::HardwareFitContext;
    let wire = self.host_metrics.gpu_backend.as_str();
    let backend = match wire {
      crate::daemon::host_metrics::HostMetricsSnapshot::BACKEND_MULTI => {
        multi_recommender_backend(self.host_metrics.gpu_devices.as_deref().unwrap_or(&[]))
      }
      _ => recommender_backend_key(wire),
    };
    let vram_bytes = ranked_vram_bytes(&self.host_metrics, backend);
    // `unified` is a host verdict and a hybrid laptop earns it from the iGPU's
    // UMA marker, even though the card we ranked for is discrete. shortcut: a
    // host whose only accelerator is a unified AMD APU also lands in `multi`
    // when a second enumeration splits it, and then ranking it as discrete
    // lets expert spill into memory that is the same pool as VRAM. Upgrade by
    // carrying a per-device UMA marker over IPC.
    let discrete_on_multi = wire == crate::daemon::host_metrics::HostMetricsSnapshot::BACKEND_MULTI
      && matches!(backend, "cuda" | "hip");
    let unified = self.host_metrics.unified && !discrete_on_multi;
    let uma_shared_total_bytes = if unified {
      self.host_metrics.uma_shared_total_bytes
    } else {
      None
    };
    let ram_total_bytes = self.host_metrics.ram_total_bytes;
    let overhead_band_bytes = snapshot
      .recommender_weights
      .overhead_band_bytes
      .get(backend)
      .copied();
    HardwareFitContext {
      backend: backend.to_string(),
      vram_bytes,
      ram_total_bytes,
      overhead_band_bytes,
      ctx_tokens: crate::init::recommender::DEFAULT_CTX,
      unified,
      uma_shared_total_bytes,
    }
  }

  /// Close the HuggingFace pull dialog and snap focus back to the
  /// Models list. Background download tasks the dialog spawned
  ///  keep ticking under the pinned strip — closing the
  /// dialog does not cancel them.
  pub fn close_hf_dialog(&mut self) {
    self.hf_dialog = None;
    self.focus = Focus::List;
  }

  /// Memoize `rendered_rows()` and `available_right_tabs()` for the
  /// duration of one frame so the 12+ in-frame `rendered_rows()`
  /// calls and 3+ in-frame `available_right_tabs()` calls amortise
  /// to a single build each. Paired with
  /// [`Self::clear_frame_caches`].
  pub(crate) fn prime_frame_caches(&mut self) {
    self.rows_cache = Some(self.rendered_rows_uncached());
    self.right_tabs_cache = Some(self.available_right_tabs_uncached());
  }

  /// Drop the per-frame memos. They must not outlive a frame —
  /// event handlers between frames mutate models / managed /
  /// favorites freely and would observe stale state.
  pub(crate) fn clear_frame_caches(&mut self) {
    self.rows_cache = None;
    self.right_tabs_cache = None;
  }

  /// Back-compat shim; new code should call `prime_frame_caches`.
  #[doc(hidden)]
  pub(crate) fn prime_rows_cache(&mut self) {
    self.prime_frame_caches();
  }

  /// Back-compat shim; new code should call `clear_frame_caches`.
  #[doc(hidden)]
  pub(crate) fn clear_rows_cache(&mut self) {
    self.clear_frame_caches();
  }

  /// True when the right pane should render at the supplied body
  /// width.
  ///
  /// **Wide** (`body_width >= COMPACT_WIDTH_THRESHOLD`): pane is
  /// always visible as long as the user has at least one discovered
  /// model — it follows the cursor and surfaces Settings (and
  /// Logs/Chat when running) for the focused row.
  ///
  /// **Compact** (`body_width < COMPACT_WIDTH_THRESHOLD`): pane is
  /// "drill-in only" — hidden by default, opens when focus moves
  /// into it (`open_launch_picker` from `Enter` on a model row, or
  /// any focus chord that lands in the right pane). `Esc` /
  /// `close_launch_picker` moves focus back to `List` and the pane
  /// disappears, expanding the list back to full width.
  ///
  /// Focus is the single source of truth in compact mode — no
  /// separate flag — so every existing path that moves focus
  /// (Cancel, FocusList, close_launch_picker) already implicitly
  /// closes the pane.
  pub fn right_pane_visible_at(&self, body_width: u16) -> bool {
    if self.models.is_empty() {
      return false;
    }
    body_width >= COMPACT_WIDTH_THRESHOLD || self.right_pane_focused()
  }

  /// True when focus currently lives inside the right pane (or one
  /// of its tab-bound input fields). Used by `right_pane_visible_at`
  /// to keep the pane open in compact mode whenever the user has
  /// drilled in. Mirrors the predicate in `render::right_is_focused`
  /// — keep them in sync.
  pub fn right_pane_focused(&self) -> bool {
    matches!(
      self.focus,
      Focus::RightPane | Focus::ChatInput | Focus::EmbedInput | Focus::RerankInput
    )
  }

  /// Toggle the modal help overlay. Bound to `?`. Esc also closes
  /// it via the existing Cancel action plumbing.
  pub fn toggle_help(&mut self) {
    self.show_help = !self.show_help;
    self.help_scroll = 0;
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

  /// Build a `key:hint` chip string for `(focus, action)` against the
  /// live keymap. Returns `None` when the user has unbound the
  /// action in `focus` entirely so callers can drop the hint rather
  /// than render a chip with no working key.
  ///
  /// The chip text comes from `Action::hint_for(focus)` with a
  /// fallback to the binding's `hint` field, so updates in
  /// `keybindings.rs` flow through to every chip. For one-off
  /// caller-supplied overrides see [`Self::hint_with`].
  pub fn hint(&self, focus: Focus, action: Action) -> Option<String> {
    let b = self
      .bindings_for(focus)
      .iter()
      .find(|b| b.action == action)?;
    let hint = action.hint_for(focus).unwrap_or(b.hint);
    Some(format!("{}:{}", b.label, hint))
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

  /// Resolve the live label (`Esc`, `Enter`, `Ctrl+X`, …) for
  /// `(focus, action)`, falling back to `fallback` when the action
  /// is unbound. Used by dialog renderers that want a single-token
  /// chord label (without a description) so `format!("{label}
  /// returns to ...")` reads naturally. Both the HF dialog footer
  /// and the confirm-popup body call this; the helper lives on
  /// `App` so we have one canonical lookup.
  pub fn resolve_label(&self, focus: Focus, action: Action, fallback: &str) -> String {
    self
      .bindings_for(focus)
      .iter()
      .find(|b| b.action == action)
      .map(|b| b.label.to_string())
      .unwrap_or_else(|| fallback.to_string())
  }

  /// Apply a `list_models` IPC response. The TUI calls this after
  /// every refresh.
  pub fn ingest_list_models(&mut self, body: &Value) {
    let arr = match body.get("models").and_then(Value::as_array) {
      Some(a) => a,
      None => return,
    };
    let mut next: Vec<DiscoveredModel> = Vec::with_capacity(arr.len());
    let mut backend_by_path = std::collections::BTreeMap::new();
    for row in arr {
      if let Some(m) = parse_list_models_row(row) {
        // Capture the daemon's honest per-row backend prediction (the single
        // source for the picker, the backend badge, and the Backend column).
        if let Some(backend) = row.get("backend").and_then(Value::as_str) {
          backend_by_path.insert(m.path.clone(), backend.to_string());
        }
        next.push(m);
      }
    }
    self.models = next;
    self.backend_by_path = backend_by_path;
    self.clamp_cursor();
  }

  /// The daemon's predicted backend for `path` (an engine id, and
  /// so on), or `None` when the model isn't in the current catalog.
  pub fn predicted_backend(&self, path: &std::path::Path) -> Option<&str> {
    self.backend_by_path.get(path).map(String::as_str)
  }

  /// True when more than one backend is actually in play — any model routes to
  /// something other than the default `llamacpp`. Gates the Models-list Backend
  /// column (like `multi_device` gates the Device column) so a single-backend
  /// host never sees a redundant all-`llamacpp` column.
  pub fn multi_backend(&self) -> bool {
    self
      .backend_by_path
      .values()
      .any(|b| b != crate::backend::DEFAULT_BACKEND_ID)
  }

  /// Queue the bell for a launch this TUI is about to send, remembering the
  /// rows already on screen for that path. Those belong to earlier launches:
  /// without this a duplicate launch of a running model would ring a second
  /// after the keystroke off the old Ready row and stay quiet at its own end.
  pub fn queue_bell_watch(&mut self, path: impl Into<PathBuf>) {
    let path = path.into();
    self.bell_watch.push(BellWatch {
      known: self
        .managed
        .iter()
        .filter(|row| row.path == path)
        .map(|row| row.launch_id.clone())
        .collect(),
      ..BellWatch::waiting(path)
    });
  }

  /// Name the launch a queued bell belongs to, from the daemon's `start_model`
  /// reply. Oldest unbound watch first: the writer answers in dispatch order.
  pub fn bind_bell_watch(&mut self, launch_id: &str) {
    if let Some(watch) = self.bell_watch.iter_mut().find(|w| w.launch_id.is_none()) {
      watch.launch_id = Some(launch_id.to_string());
      watch.misses = 0;
    }
  }

  /// Bell bookkeeping for one status tick. Rings once per watched launch whose
  /// own row reached a terminal load state, and keeps the rest. A watch with no
  /// candidate row ages: a start the daemon refused, one that never reached the
  /// daemon, or one whose reply never came back has no row of its own to turn
  /// terminal, so it expires instead of ringing for some later launch of the
  /// same model. Pure, because `ring` writes to the process's own stderr and is
  /// a no-op in a test run.
  fn bell_tick(watch: &[BellWatch], rows: &[ManagedRow]) -> (Vec<BellWatch>, usize) {
    let mut keep: Vec<BellWatch> = Vec::with_capacity(watch.len());
    let mut claimed: Vec<usize> = Vec::with_capacity(watch.len());
    let mut fired = 0usize;
    for entry in watch {
      let hit = rows.iter().enumerate().find(|(i, row)| {
        !claimed.contains(i)
          && entry.candidate(row)
          && matches!(row.state, SurfaceState::Ready | SurfaceState::Error)
      });
      if let Some((i, _)) = hit {
        claimed.push(i);
        fired += 1;
        continue;
      }
      if rows.iter().any(|row| entry.candidate(row)) {
        // Still loading: keep waiting. A row on screen is proof the launch is
        // real, so the clock restarts and a later disappearance gets its own
        // full grace.
        keep.push(BellWatch {
          misses: 0,
          ..entry.clone()
        });
      } else if entry.misses + 1 < BELL_WATCH_MAX_MISSES {
        keep.push(BellWatch {
          misses: entry.misses + 1,
          ..entry.clone()
        });
      }
    }
    (keep, fired)
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
      let next: Vec<ManagedRow> = arr.iter().filter_map(parse_status_row).collect();
      let prev_ids: std::collections::BTreeSet<String> =
        self.managed.iter().map(|m| m.launch_id.clone()).collect();
      // The launch the cursor sat on last tick. The Running group can
      // appear under a cursor that never moved, so comparing focused
      // paths after the merge would miss it: the row at that index
      // changed identity while the index stood still.
      let prev_focus_launch = self.focused_managed().map(|m| m.launch_id.clone());
      // Detect transitions into `Error` so we can auto-jump the
      // right pane to Logs — the user explicitly wants to see the
      // failure tail, not the static Settings form. Keyed by
      // launch_id: each launch carries the same id across state
      // transitions, so a managed row going Running → Error is
      // matched on the same key.
      let prev_state_by_id: BTreeMap<String, SurfaceState> = self
        .managed
        .iter()
        .map(|m| (m.launch_id.clone(), m.state))
        .collect();
      let newly_errored: Vec<String> = next
        .iter()
        .filter(|m| m.state == SurfaceState::Error)
        .filter(|m| {
          prev_state_by_id
            .get(&m.launch_id)
            .map(|prev| *prev != SurfaceState::Error)
            .unwrap_or(true)
        })
        .map(|m| m.launch_id.clone())
        .collect();
      // Merge while preserving recency order:
      //   1. Launches that are new in this tick land at the top
      //      (newest first per the daemon's emission order).
      //   2. Launches that were already present keep their prior
      //      relative position.
      //   3. Launches that vanished (stopped) drop out.
      let next_by_id: BTreeMap<String, ManagedRow> = next
        .iter()
        .map(|m| (m.launch_id.clone(), m.clone()))
        .collect();
      let mut merged: Vec<ManagedRow> = Vec::with_capacity(next.len());
      let mut newest: Option<String> = None;
      for row in &next {
        if !prev_ids.contains(&row.launch_id) {
          if newest.is_none() {
            newest = Some(row.launch_id.clone());
          }
          merged.push(row.clone());
        }
      }
      for prev in &self.managed {
        if let Some(updated) = next_by_id.get(&prev.launch_id) {
          merged.push(updated.clone());
        }
      }
      self.managed = merged;
      // Snap the cursor onto the newest running launch so the user
      // sees their just-launched model selected. Only fires when a
      // genuinely new launch_id appeared on this tick.
      if let Some(launch_id) = newest {
        self.snap_cursor_to_launch(&launch_id);
      }
      // A cursor that now sits on a different launch than it did last
      // tick gets the same treatment as an explicit move, which covers
      // the opening frame and a launch that appeared under the cursor.
      if self.focused_managed().map(|m| m.launch_id.clone()) != prev_focus_launch {
        self.running_view_scroll.set(0);
        self.right_tab = self.first_right_tab();
      }
      // If the focused launch just transitioned into Error, snap
      // the right pane to Logs so the user sees the failure tail
      // without an extra Tab keystroke. Other launches in the
      // newly_errored set get picked up the moment the user focuses
      // them — `ensure_right_tab_reachable` keeps Logs reachable
      // for Error rows.
      if !newly_errored.is_empty() {
        if let Some(focused) = self.focused_managed() {
          if newly_errored.contains(&focused.launch_id) {
            self.right_tab = RightTab::Logs;
          }
        }
      }
      if self.options.bell {
        let (keep, fired) = Self::bell_tick(&self.bell_watch, &self.managed);
        self.bell_watch = keep;
        if fired > 0 {
          // One sink for the tick, not one per ring.
          let mut sink = crate::util::bell::tty_sink();
          for _ in 0..fired {
            let _ = crate::util::bell::ring_into(true, &mut sink);
          }
        }
      }
    } else {
      self.managed.clear();
    }
    if let Some(arr) = body.get("external").and_then(Value::as_array) {
      self.external = arr.iter().filter_map(parse_external_row).collect();
    } else {
      self.external.clear();
    }
    self.memory_budget = body
      .pointer("/daemon/memory")
      .and_then(|value| serde_json::from_value(value.clone()).ok());
    if self
      .memory_dialog
      .as_ref()
      .is_some_and(|dialog| !dialog.saving)
      && !self
        .memory_budget
        .as_ref()
        .is_some_and(|budget| budget.supports_editing())
    {
      self.memory_dialog = None;
      self.show_error_toast("Memory reserve editing is no longer available on this daemon.");
    }
    if let Some(daemon) = body.get("daemon") {
      // Preserve the proxy snapshot — it lives in a sibling field
      // (`body["proxy"]`) but on the same daemon-info struct so the
      // info_pane can render both in one block.
      let proxy = body.get("proxy").and_then(parse_proxy_info);
      // The backends rows (also a sibling field): every enabled backend's
      // server labels, for the server row. A row with no `enabled` field
      // counts as enabled (llamacpp has no off switch); an explicit
      // `enabled: false` without the matching daemon flag is dropped.
      let backend_servers: Vec<BackendServers> = body
        .get("backends")
        .and_then(Value::as_array)
        .map(|rows| {
          rows
            .iter()
            .filter(|r| r.get("enabled").and_then(Value::as_bool).unwrap_or(true))
            .filter_map(|r| {
              let mut labels: Vec<String> = r
                .get("server_labels")
                .and_then(Value::as_array)
                .map(|a| {
                  a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
                })
                .unwrap_or_default();
              if labels.is_empty() {
                let binary = Path::new(r.get("binary").and_then(Value::as_str)?);
                labels.push(binary.file_name()?.to_string_lossy().into_owned());
              }
              Some(BackendServers {
                id: r.get("id").and_then(Value::as_str)?.to_string(),
                labels,
              })
            })
            .collect()
        })
        .unwrap_or_default();
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
        ipc_url: daemon
          .get("ipc_url")
          .and_then(Value::as_str)
          .map(String::from),
        proxy,
        backend_servers,
      };
      // First-observation toast on `port_in_use`. Only fires when the
      // last *observed* status was something other than `port_in_use`
      // (the daemon emits the same status on every tick, so reading
      // the cell value alone would re-toast forever). Snapshot the
      // fields we need out of the borrow before `show_toast` claims
      // a mutable borrow on `self`.
      let (cur_status, cur_listen): (Option<String>, Option<String>) =
        match self.daemon_info.proxy.as_ref() {
          Some(p) => (Some(p.status.clone()), p.listen.clone()),
          None => (None, None),
        };
      if let Some(status) = cur_status {
        let was_port_in_use = matches!(self.last_proxy_status.as_deref(), Some("port_in_use"));
        if status == "port_in_use" && !was_port_in_use {
          let listen = cur_listen.as_deref().unwrap_or("?");
          self.show_error_toast(format!(
            "proxy: port {listen} already in use; daemon continues without the OpenAI-compat router"
          ));
        }
        let was_refused = matches!(self.last_proxy_status.as_deref(), Some("refused_insecure"));
        if status == "refused_insecure" && !was_refused {
          let listen = cur_listen.as_deref().unwrap_or("?");
          self.show_error_toast(format!(
            "proxy: refused to expose {listen} without auth; set proxy.api_key or pass --insecure-no-auth"
          ));
        }
        self.last_proxy_status = Some(status);
      }
    }
    if let Some(host) = body.get("host") {
      if !host.is_null() {
        if let Ok(snap) = serde_json::from_value::<HostMetricsSnapshot>(host.clone()) {
          self.host_metrics = snap;
        }
      }
    }
    if let Some(servers) = body.get("servers") {
      if !servers.is_null() {
        if let Ok(parsed) = serde_json::from_value::<Vec<crate::backend::Server>>(servers.clone()) {
          self.servers = parsed;
        }
      }
    }
  }

  /// Apply a `presets_all` IPC response — the raw config `presets:`
  /// blocks. The launch picker resolves each model's effective set from
  /// this against the catalog. A malformed body leaves the cache as-is.
  pub fn ingest_presets(&mut self, body: &Value) {
    if let Some(map) = body
      .get("presets")
      .cloned()
      .and_then(|v| serde_json::from_value(v).ok())
    {
      self.config_presets = map;
    }
  }

  /// Apply a `last_params_list` IPC response. The TUI uses the
  /// snapshot to seed the launch picker for the focused model.
  pub fn ingest_last_params(&mut self, body: &Value) {
    let arr = match body.get("last_params").and_then(Value::as_array) {
      Some(a) => a,
      None => return,
    };
    // Track the IPC response order separately — the daemon emits
    // `last_params` newest-first now (see
    // `state_store::upsert_last_params`), so we use that order to
    // populate `recent_paths` for the `↺ Recent` section.
    let mut next: BTreeMap<PathBuf, LastParamsRow> = BTreeMap::new();
    let mut recent: Vec<PathBuf> = Vec::with_capacity(RECENT_LIST_CAP);
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
        let knobs: crate::launch::knobs::KnobSet = params
          .get("knobs")
          .and_then(|v| serde_json::from_value(v.clone()).ok())
          .unwrap_or_default();
        let extras = params
          .get("extras")
          .and_then(Value::as_array)
          .map(|items| {
            items
              .iter()
              .filter_map(|v| v.as_str().map(String::from))
              .collect()
          })
          .unwrap_or_default();
        let port = params
          .get("port")
          .and_then(Value::as_u64)
          .and_then(|n| u16::try_from(n).ok());
        let server = params
          .get("server")
          .and_then(Value::as_str)
          .map(String::from);
        if recent.len() < RECENT_LIST_CAP {
          recent.push(path.clone());
        }
        next.insert(
          path,
          LastParamsRow {
            ctx,
            reasoning,
            knobs,
            extras,
            port,
            server,
          },
        );
      }
    }
    self.last_params = next;
    self.recent_paths = recent;
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
    if let Some(cached) = self.rows_cache.as_ref() {
      return cached.clone();
    }
    self.rendered_rows_uncached()
  }

  /// The expensive `build_rows + apply_filter` walk. Public only
  /// for the cache primer; every other caller goes through
  /// [`Self::rendered_rows`] so the per-frame memo applies.
  fn rendered_rows_uncached(&self) -> Vec<ListRow> {
    let model_states = self.surface_states();
    let model_ports = self.surface_ports();
    let running: Vec<RunningLaunchRow> = self
      .managed
      .iter()
      .map(|m| RunningLaunchRow {
        launch_id: m.launch_id.clone(),
        path: m.path.clone(),
        port: m.port,
        state: m.state,
        device: m.device.clone(),
        // The backend the launch actually resolved to (honest even for a
        // `--backend llamacpp` override on a file another backend claims).
        backend: m.backend.clone(),
        // User-chosen launch name (from `--name` / `Alt+⏎`), when set.
        launch_name: m.name.clone(),
      })
      .collect();
    let mut all = build_rows(RowInputs {
      models: &self.models,
      favorites: &self.favorites,
      model_states: &model_states,
      model_ports: &model_ports,
      running: &running,
      recent_paths: &self.recent_paths,
      backend_by_path: &self.backend_by_path,
    });
    if !self.filter_input.is_empty() {
      all = apply_filter(&all, self.filter_input.buffer());
    }
    all
  }

  /// Move `list_cursor` onto the row whose `launch_id` matches.
  /// Used by `ingest_status` to land the user on a just-spawned
  /// launch so they don't have to chase it manually. No-op when
  /// the row isn't found (the merge may have ordered things
  /// differently on this tick).
  ///
  /// Routes through `sync_pane_to_focus` so any picker staged
  /// for the prior focused path is cleared when the snap lands on
  /// a different model — otherwise Settings would keep painting
  /// the stale picker's name/port for the just-snapped row.
  fn snap_cursor_to_launch(&mut self, launch_id: &str) {
    let rows = self.rendered_rows();
    if let Some((idx, _)) = rows.iter().enumerate().find(|(_, r)| match r {
      ListRow::Model {
        launch_id: Some(id),
        ..
      } => id == launch_id,
      _ => false,
    }) {
      let before = self.focused_row_key();
      self.list_cursor = idx;
      self.sync_pane_to_focus(before);
    }
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

  /// Companion to [`Self::surface_states`] — collapses every active
  /// launch the daemon knows about into one `path → port` map for
  /// the Port column. The list pane is currently one row per
  /// discovered path, so the duplicate-launch case (which lives in
  /// the Running group after the next polish round) deliberately
  /// keeps just the last entry seen; the table cell still resolves
  /// to a useful port, just not a uniqueness-aware one.
  fn surface_ports(&self) -> BTreeMap<PathBuf, u16> {
    let mut out: BTreeMap<PathBuf, u16> = BTreeMap::new();
    for m in &self.external {
      // A scan row whose argv carried no port flag has no port to show;
      // inserting it would render as `:0` in the list's Port column.
      if m.port == 0 {
        continue;
      }
      out.insert(m.path.clone(), m.port);
    }
    for m in &self.managed {
      out.insert(m.path.clone(), m.port);
    }
    out
  }

  /// Rows for external `llama-server` processes the daemon detected
  /// outside its supervisor. Surfaced read-only (stop is the only
  /// action allowed) — used by the right pane to show "this
  /// model is unmanaged" hints.
  pub fn external_rows(&self) -> &[ManagedRow] {
    &self.external
  }

  /// Move cursor down to the next selectable (model) row.
  pub fn move_down(&mut self) {
    let rows = self.rendered_rows();
    let before = self.focused_row_key();
    self.move_down_in(&rows);
    self.sync_pane_to_focus(before);
  }

  pub fn move_up(&mut self) {
    let rows = self.rendered_rows();
    let before = self.focused_row_key();
    self.move_up_in(&rows);
    self.sync_pane_to_focus(before);
  }

  /// Follow the cursor when it lands on a different row. The right pane
  /// belongs to the focused row, with no sticky fallback: a picker staged
  /// for model A must not linger while the user scrolls to model B, and
  /// the active tab re-opens at the new row's first one, so a launch
  /// opens on Requests and a model with no launch on Settings. Caller
  /// passes the focused key *before* the move to compare against the one
  /// *after*.
  fn sync_pane_to_focus(&mut self, before: Option<(PathBuf, Option<String>)>) {
    let after = self.focused_row_key();
    if before == after {
      return;
    }
    // Scroll offset is per-focused-row; clear it on cursor moves so the
    // new row's running-launch view opens at the top.
    self.running_view_scroll.set(0);
    self.right_tab = self.first_right_tab();
    // The picker belongs to a model, so two launches of one path share a
    // staged one and only a path change drops it.
    let path = |key: &(PathBuf, Option<String>)| key.0.clone();
    if before.as_ref().map(path) != after.as_ref().map(path) {
      self.launch_picker = None;
    }
  }

  /// The row under the cursor, keyed as finely as the list differentiates
  /// it: the launch id when the row carries one, so two named launches of
  /// one model are two rows, else the path.
  fn focused_row_key(&self) -> Option<(PathBuf, Option<String>)> {
    match self.rendered_rows().get(self.list_cursor) {
      Some(ListRow::Model {
        path, launch_id, ..
      }) => Some((path.clone(), launch_id.clone())),
      _ => None,
    }
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
    let before = self.focused_row_key();
    if delta >= 0 {
      for _ in 0..delta {
        self.move_down_in(&rows);
      }
    } else {
      for _ in 0..-delta {
        self.move_up_in(&rows);
      }
    }
    self.sync_pane_to_focus(before);
  }

  pub fn go_top(&mut self) {
    let rows = self.rendered_rows();
    let before = self.focused_row_key();
    for (i, r) in rows.iter().enumerate() {
      if r.is_selectable() {
        self.list_cursor = i;
        break;
      }
    }
    self.sync_pane_to_focus(before);
  }

  pub fn go_bottom(&mut self) {
    let rows = self.rendered_rows();
    let before = self.focused_row_key();
    for (i, r) in rows.iter().enumerate().rev() {
      if r.is_selectable() {
        self.list_cursor = i;
        break;
      }
    }
    self.sync_pane_to_focus(before);
  }

  pub(crate) fn clamp_cursor(&mut self) {
    let rows = self.rendered_rows();
    if rows.is_empty() {
      self.list_cursor = 0;
      return;
    }
    if self.list_cursor >= rows.len() {
      self.list_cursor = rows.len() - 1;
    }
    if !rows[self.list_cursor].is_selectable() {
      // Nearest model row below the cursor; with none below, the last one.
      match rows[self.list_cursor..]
        .iter()
        .position(ListRow::is_selectable)
      {
        Some(offset) => {
          let before = self.focused_row_key();
          self.list_cursor += offset;
          self.sync_pane_to_focus(before);
        }
        None => self.go_bottom(),
      }
    }
  }

  /// Path of the model the cursor sits on.
  pub fn focused_path(&self) -> Option<PathBuf> {
    let rows = self.rendered_rows();
    rows
      .get(self.list_cursor)
      .and_then(|r| r.path().map(|p| p.to_path_buf()))
  }

  /// Whether `path` names a file on disk that a delete can unlink. A path the
  /// catalog does not know is treated as a local file.
  pub fn has_local_file(&self, path: &Path) -> bool {
    self
      .models
      .iter()
      .find(|m| m.path == path)
      .is_none_or(|m| m.source.has_local_file())
  }

  /// Friendly display label for `path` if the discovery layer
  /// supplied one (Ollama's `<name>:<tag>`). Right-pane / info-pane
  /// callers fall back to `util::paths::model_display_name` when this
  /// returns `None` so non-Ollama rows keep their file_stem render.
  pub fn display_label_for(&self, path: &Path) -> Option<String> {
    self
      .models
      .iter()
      .find(|m| m.path == path)
      .and_then(|m| m.display_label.clone())
  }

  /// The one canonical model name for a path, shared by every surface (list
  /// rows, right-pane header, info pane, chat field): the live catalog's
  /// friendly `display_label` when the path is known, else the path-derived
  /// fallback ([`crate::util::paths::model_display_name`], scheme-aware). One
  /// resolver so a model reads identically in every state — the catalog match
  /// transiently misses while an engine is mid-load, and this keeps
  /// the name stable across that gap instead of flipping to a truncated form.
  pub fn model_label(&self, path: &Path) -> String {
    self
      .display_label_for(path)
      .unwrap_or_else(|| crate::util::paths::model_display_name(path))
  }

  /// Multimodal capability of the model at `path` (vision / audio) if
  /// discovery detected an mmproj projector companion. Drives the glyph
  /// the right-pane header renders after the model title.
  pub fn multimodal_for(&self, path: &Path) -> Option<crate::discovery::Multimodal> {
    self
      .models
      .iter()
      .find(|m| m.path == path)
      .and_then(|m| m.multimodal)
  }

  /// Whether the model at `path` is MTP-capable (embedded nextn head or a
  /// separate `mtp-*.gguf` drafter), per discovery. Drives the `↯` glyph the
  /// right-pane header renders after the model title.
  pub fn mtp_capable_for(&self, path: &Path) -> bool {
    self
      .models
      .iter()
      .find(|m| m.path == path)
      .map(|m| m.mtp_capable())
      .unwrap_or(false)
  }

  /// Friendly display name with fallback. Prefers the discovery
  /// label, falls back to the path's file_stem. Centralised so every
  /// surface (toast, confirm dialog, header, daemon panel) renders
  /// the same name for the same model.
  pub fn display_name_for(&self, path: &Path) -> String {
    self
      .display_label_for(path)
      .unwrap_or_else(|| crate::util::paths::model_display_name(path))
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
    let rows = self.rendered_rows();
    let row = rows.get(self.list_cursor)?;
    match row {
      ListRow::Model {
        launch_id: Some(id),
        ..
      } => self.managed.iter().find(|m| &m.launch_id == id),
      ListRow::Model { path, .. } => self.managed.iter().find(|m| &m.path == path),
      _ => None,
    }
  }

  /// The ManagedRow the right pane should display — the model the
  /// cursor sits on, or `None` when the cursor row has no managed
  /// launch. The right pane follows the cursor with no sticky
  /// fallback: an unlaunched row shows just the Settings tab for
  /// the selected model.
  pub fn right_pane_focus(&self) -> Option<&ManagedRow> {
    self.focused_managed()
  }

  /// Gates the Models-list Device column and the multi-GPU placement knobs.
  /// True when **some single server** sees more than one physical GPU — a
  /// launch targets one server's devices, so cross-build selector variety (the
  /// same physical GPU seen as `ROCm0` by one build and `Vulkan0` by another)
  /// is a *server* choice, not a device one, and must not trip this. Neither
  /// does one build reporting that same card under two compute APIs, which is
  /// why the count is physical (`Server::physical_device_count`) rather than
  /// `devices.len()`.
  pub fn multi_device(&self) -> bool {
    self.servers.iter().any(|s| s.physical_device_count() > 1)
  }

  /// The server a running launch is actually on, resolved the way the daemon
  /// resolves it (`launch_service::pick_launch_binary`): an explicit pick, else
  /// the server owning the launch's device selector, else the backend's default
  /// server — the catalog entry matching the daemon's `server_path`, or that
  /// backend's first entry. `None` when the catalog can't place it (still
  /// probing, or a rebuilt binary that has left the catalog), which callers
  /// read as "fall back to the host-wide answer" rather than hiding rows.
  pub fn server_for_managed(&self, row: &ManagedRow) -> Option<&crate::backend::Server> {
    if let Some(id) = &row.server {
      if let Some(s) = self.servers.iter().find(|s| &s.id == id) {
        return Some(s);
      }
    }
    if let Some(selector) = &row.device {
      if let Some(s) = self
        .servers
        .iter()
        .find(|s| s.devices.iter().any(|d| &d.selector == selector))
      {
        return Some(s);
      }
    }
    let backend = row.backend.as_deref()?;
    crate::backend::default_server(
      self.servers.iter().filter(|s| s.backend_id == backend),
      self.daemon_info.server_path.as_deref().map(Path::new),
    )
  }

  /// The knob-group gates for a running launch's read-only settings view,
  /// scoped to the server that launch is on. A model on the default one-device
  /// build must not show placement rows because some *other* configured build
  /// has two selectors.
  pub fn gate_facts_for_managed(&self, row: &ManagedRow) -> crate::launch::knobs::GateFacts {
    let mtp_capable = self.mtp_capable_for(&row.path);
    match self.server_for_managed(row) {
      Some(server) => crate::launch::knobs::GateFacts {
        device_choice: server.devices.len() > 1,
        multi_device: server.physical_device_count() > 1,
        mtp_capable,
      },
      None => crate::launch::knobs::GateFacts {
        device_choice: self.servers.iter().any(|s| s.devices.len() > 1),
        multi_device: self.multi_device(),
        mtp_capable,
      },
    }
  }

  /// The server the focused running model's device override runs on, when it
  /// is a **non-default** server binary. Resolves the
  /// row's `--device` selector against the server catalog (the owning server
  /// carries the binary). `None` when the focused row has no selector or
  /// resolves to the daemon's default server.
  pub fn focused_override_device(&self) -> Option<OverrideServer> {
    let rows = self.rendered_rows();
    let selector = match rows.get(self.list_cursor) {
      Some(ListRow::Model {
        device: Some(d), ..
      }) => d.clone(),
      _ => return None,
    };
    let server = self
      .servers
      .iter()
      .find(|s| s.devices.iter().any(|d| d.selector == selector))?;
    let is_default = self
      .daemon_info
      .server_path
      .as_deref()
      .is_some_and(|def| server.binary == Path::new(def));
    if is_default {
      return None;
    }
    let index = self
      .servers
      .iter()
      .filter(|s| s.backend_id == server.backend_id)
      .position(|s| s.id == server.id)?;
    Some(OverrideServer {
      backend_id: server.backend_id.clone(),
      index,
    })
  }

  /// Servers compatible with `path`'s model, priority-ordered: for each backend
  /// in the model's `supported_backends` (priority order), every catalog server
  /// of that backend. Falls back to every backend in the catalog when the model
  /// declares no supported backends (older daemon / no metadata).
  ///
  /// Within a backend the build the daemon spawns with no pin
  /// ([`crate::backend::default_server`]) leads its group, so `servers[0]` *is*
  /// the default — which is what the picker's cycle ring,
  /// `server_value_label` and `launch_identity` all read position 0 as. The
  /// catalog already arrives that way (each backend's `configured_servers`
  /// leads with the binary the daemon resolved), so this re-sort changes
  /// nothing today; it makes the picker depend on a property of the list it
  /// was handed instead of on that distant guarantee. Feeds the launch
  /// picker's Server row.
  pub(crate) fn compatible_servers(&self, path: &Path) -> Vec<crate::backend::Server> {
    let supported: Vec<String> = self
      .models
      .iter()
      .find(|m| m.path == path)
      .map(|m| m.supported_backends.clone())
      .unwrap_or_default();
    let backends: Vec<String> = if supported.is_empty() {
      let mut seen: Vec<String> = Vec::new();
      for s in &self.servers {
        if !seen.contains(&s.backend_id) {
          seen.push(s.backend_id.clone());
        }
      }
      seen
    } else {
      supported
    };
    let default_binary = self.daemon_info.server_path.as_deref().map(Path::new);
    let mut out = Vec::new();
    for backend_id in &backends {
      let owner = crate::backend::Backends::from_id(backend_id);
      let of_backend = || {
        self.servers.iter().filter(|s| {
          &s.backend_id == backend_id
            && owner
              .as_ref()
              .is_none_or(|b| crate::backend::Backend::server_serves(b, &s.id, path))
        })
      };
      let default_id = crate::backend::default_server(of_backend(), default_binary)
        .map(|s| s.id.clone())
        .unwrap_or_default();
      out.extend(of_backend().filter(|s| s.id == default_id).cloned());
      out.extend(of_backend().filter(|s| s.id != default_id).cloned());
    }
    out
  }

  /// Build a [`LaunchPickerState`] seeded for the currently
  /// focused model — pure, no side effects. Returns `None` when
  /// the cursor sits on a header (no model focused). Both
  /// [`Self::open_launch_picker`] and the Settings tab's
  /// default-render path use this so the form contents (ctx,
  /// reasoning, preset_idx, prefer_port, active_instances) reflect
  /// persisted `last_params` even before the user has interacted
  /// with the picker.
  pub fn build_default_picker(&self) -> Option<LaunchPickerState> {
    let name = self.focused_name()?;
    let path = self.focused_path();
    let active_count = path
      .as_ref()
      .map(|p| self.managed.iter().filter(|m| &m.path == p).count())
      .unwrap_or(0);
    let mut state = LaunchPickerState::for_model(name);
    state.model_path = path.clone();
    if let Some(p) = &path {
      // Gate the ctx quick-picks to the focused model's trained window.
      state.native_ctx = self
        .models
        .iter()
        .find(|m| &m.path == p)
        .and_then(|m| m.metadata.as_ref())
        .and_then(|md| md.native_ctx)
        .and_then(|c| u32::try_from(c).ok());
      // Show the MTP row only when this model can actually do MTP (embedded
      // nextn head or a separate drafter sibling) — the same signal that lights
      // the `↯` badge. Backend-agnostic.
      state.mtp_capable = self.mtp_capable_for(p);
      if let Some(last) = self.last_params.get(p) {
        state.prefer_port = last.port;
        // returning user inherits the typed-knob deltas they
        // last shipped. The daemon persists only user-supplied
        // deltas (not the fully resolved set) so seeding straight
        // into `user_knobs` keeps the picker's source labels
        // honest — every persisted knob shows `(user)`, the rest
        // re-resolve from yaml / built-in / model default.
        //
        // `ctx` and `reasoning` are now part of `crate::launch::knobs::KnobSet`, so
        // they ride along inside `last.knobs` without dedicated
        // seeding paths.
        state.user_knobs = last.knobs.clone();
        // Seed extras here (before `set_presets`) so the preset cycle's
        // `last used` stop captures them as part of its baseline.
        if !last.extras.is_empty() {
          state.extras = last.extras.iter().map(std::ffi::OsString::from).collect();
        }
      }
    }
    state.active_instances = active_count;
    // Scope the backend chooser to the focused model's own backend so the
    // picker can't force a cross-backend launch — straight off the daemon's
    // per-row prediction (`backend_by_path`), no TUI-side re-derivation.
    use crate::launch::params::BackendChoice;
    state.model_backend = path
      .as_ref()
      .and_then(|p| self.predicted_backend(p))
      .map(BackendChoice::from_id)
      .unwrap_or_else(|| BackendChoice::from_id(crate::backend::DEFAULT_BACKEND_ID));
    // Populate the Server row from the model's compatible servers (priority-
    // ordered) and seed the last-used pick. The Server row cycles these; the
    // Device row + multi-GPU gating scope to whichever server is current.
    if let Some(p) = &path {
      state.servers = self.compatible_servers(p);
      state.selected_server = self.last_params.get(p).and_then(|l| l.server.clone());
    }
    // Seed the preset cycle from the model's effective set (per-model ∪
    // arch, resolved against the catalog). Always called — the Preset row
    // is always shown (it offers `last used` ↔ `auto` even with no named
    // presets), and it captures the seeds above as its `last used` baseline.
    if let Some(p) = &path {
      let (choices, default_stop) = self.effective_preset_choices(p);
      state.set_presets(choices, default_stop);
    }
    // Open with the cursor on the always-shown Preset row — it leads the form
    // and is the first thing a user picks when staging a launch. Centralized
    // here so every construction path agrees (the launch picker *and* the
    // Settings-tab render fallback); `set_presets` deliberately leaves `field`
    // alone so tests can drive it in isolation.
    state.field = crate::tui::launch_picker::PickerField::Preset;
    Some(state)
  }

  /// Resolve the focused model's effective presets into picker-ready
  /// choices (ctx/reasoning folded into the typed knobs) plus the default
  /// cycle stop. Reuses [`crate::launch::presets::effective_presets`] against
  /// the local catalog, so the TUI and daemon agree on classification: the
  /// default stop is `Auto` for `default: auto`, `Named(i)` for a configured
  /// named default, else `LastUsed` (unset).
  fn effective_preset_choices(&self, path: &Path) -> (Vec<PresetChoice>, PresetStop) {
    use crate::launch::presets::{effective_presets, preset_body_from_launch_params};
    if self.config_presets.is_empty() {
      return (Vec::new(), PresetStop::LastUsed);
    }
    let (rows, name, arch) = self.preset_resolution_inputs(path);
    let path_str = path.display().to_string();
    let eff = effective_presets(
      &name,
      &path_str,
      arch.as_deref(),
      &self.config_presets,
      &rows,
    );
    let choices: Vec<PresetChoice> = eff
      .presets
      .iter()
      .map(|np| PresetChoice {
        name: np.name.clone(),
        knobs: preset_body_from_launch_params(&np.params).knobs,
        extras: np.params.extras.clone(),
        server: np.params.server.clone(),
        backend: np.params.backend.explicit_id().map(str::to_string),
      })
      .collect();
    let default_stop = if eff.default_is_auto() {
      PresetStop::Auto
    } else if let Some(i) = eff
      .default
      .as_ref()
      .and_then(|d| choices.iter().position(|c| &c.name == d))
    {
      PresetStop::Named(i)
    } else {
      PresetStop::LastUsed
    };
    (choices, default_stop)
  }

  /// Resolution inputs for `path`: the catalog projected into
  /// [`crate::launch::resolve::CatalogRow`]s, the model's display name
  /// (basename fallback off the catalog), and its arch. Shared by the
  /// preset-cycle and save-dialog paths so they classify identically.
  fn preset_resolution_inputs(
    &self,
    path: &Path,
  ) -> (
    Vec<crate::launch::resolve::CatalogRow>,
    String,
    Option<String>,
  ) {
    use crate::launch::resolve::CatalogRow;
    let rows: Vec<CatalogRow> = self
      .models
      .iter()
      .map(|m| {
        CatalogRow::for_resolution(
          m.path.display().to_string(),
          m.display_label.clone(),
          m.metadata.as_ref().and_then(|md| md.arch.clone()),
        )
      })
      .collect();
    let path_str = path.display().to_string();
    let row = rows.iter().find(|r| r.path == path_str);
    let name = row
      .map(|r| r.name())
      .unwrap_or_else(|| crate::util::paths::model_file_label(path));
    let arch = row.and_then(|r| r.arch.clone());
    (rows, name, arch)
  }

  /// Existing preset names the focused model resolves, split into the
  /// per-model presets a save would **overwrite** and the arch presets it
  /// would only **shadow** (a per-model save creates an override; the arch
  /// entry survives and still applies to other models of that arch). The
  /// save dialog uses the split to ask the right question.
  fn existing_preset_names(&self, path: &Path) -> (Vec<String>, Vec<String>) {
    use crate::launch::presets::effective_presets;
    if self.config_presets.is_empty() {
      return (Vec::new(), Vec::new());
    }
    let (rows, name, arch) = self.preset_resolution_inputs(path);
    let path_str = path.display().to_string();
    // Arch layer skipped (arch = None) → per-model entries only.
    let per_model: Vec<String> =
      effective_presets(&name, &path_str, None, &self.config_presets, &rows)
        .presets
        .iter()
        .map(|np| np.name.clone())
        .collect();
    let arch_only: Vec<String> = effective_presets(
      &name,
      &path_str,
      arch.as_deref(),
      &self.config_presets,
      &rows,
    )
    .presets
    .iter()
    .map(|np| np.name.clone())
    .filter(|n| !per_model.contains(n))
    .collect();
    (per_model, arch_only)
  }

  /// Drill into the focused model row — the action `Enter` fires on
  /// the Models list. Two branches:
  ///
  /// - **Focused row is running** (`focused_managed().is_some()`):
  ///   focus the right pane on its first tab, which is Requests.
  ///   *No* picker is staged — pressing `Enter` alone shouldn't
  ///   silently spin up a duplicate launch before the user has any
  ///   chance to inspect or edit the params. The bottom-strip chip
  ///   leads with `e:edit for launch`; the user goes `e → edit
  ///   fields → Enter` to launch a new instance.
  /// - **Focused row is idle**: stage the launch picker with
  ///   last-used params so the next `Enter` (on the Settings tab)
  ///   dispatches the launch.
  ///
  /// Header rows / empty selections no-op. This is the "show or
  /// focus the right pane" gesture; the heavy-handed
  /// [`open_launch_picker`](Self::open_launch_picker) gate is
  /// reserved for the explicit `e:edit for launch` flow that
  /// dispatches from running view.
  pub fn drill_into_focused_model(&mut self) {
    if self.focused_name().is_none() {
      return;
    }
    if self.focused_managed().is_some() {
      self.right_tab = self.first_right_tab();
      self.focus = Focus::RightPane;
      self.running_view_scroll.set(0);
      return;
    }
    self.open_launch_picker();
  }

  /// Open the launch picker for the focused model. Seeds from
  /// persisted `last_params` when the daemon has reported any
  /// for the focused path, so a returning user lands on the params
  /// they last shipped. No-op when the cursor is on a header.
  ///
  /// Called by `e:edit for launch` (the explicit stage-over-running
  /// gate) and by `Enter`-on-list for idle rows via
  /// [`drill_into_focused_model`](Self::drill_into_focused_model).
  pub fn open_launch_picker(&mut self) {
    let picker = match self.build_default_picker() {
      Some(p) => p,
      None => return,
    };
    // `build_default_picker` seeds extras + the preset cycle and opens the
    // cursor on the Preset row.
    self.launch_picker = Some(picker);
    self.running_view_scroll.set(0);
    self.right_tab = RightTab::Settings;
    self.focus = Focus::RightPane;
  }

  pub fn close_launch_picker(&mut self) {
    self.launch_picker = None;
    self.running_view_scroll.set(0);
    self.focus = Focus::List;
    self.right_tab = RightTab::Settings;
  }

  /// Open the `Ctrl+P` save-preset dialog for the focused model. Captures
  /// the launch settings in view: a running model's live dispatched knobs +
  /// advanced `--` tail (from the `status` row), else the open launch
  /// picker's user knobs (an about-to-launch config staged in Settings),
  /// else a freshly-built default picker. Auto / inherited markers ride
  /// through untouched. The caller gates *when* this opens (running-row
  /// only in the Models pane; always in the Settings pane).
  pub fn open_save_preset_dialog(&mut self) {
    let Some(path) = self.focused_path() else {
      return;
    };
    let model_name = self
      .display_label_for(&path)
      .unwrap_or_else(|| crate::util::paths::model_file_label(&path));

    // Capture knobs + extras + launch identity from whichever surface is in
    // view. One map now, so a backend's own knobs ride with the
    // rest, and the identity (which engine / build) pins the preset to the
    // run it was captured from. A running row carries the *resolved*
    // identity; a picker carries the *intended* one (its concrete engine and
    // chosen server build).
    let (knobs, extras, backend, server) = if let Some(m) = self.focused_managed() {
      (
        m.knobs.clone(),
        m.extras.clone(),
        m.backend.clone(),
        m.server.clone(),
      )
    } else if let Some(p) = &self.launch_picker {
      Self::picker_capture(p)
    } else {
      // No picker staged: capture the persisted last-used identity straight
      // from the cheap sources (`last_params` + the daemon's per-row backend
      // prediction + the compatible-server catalog), skipping
      // `build_default_picker`'s full preset-cascade resolution — which
      // `existing_preset_names` below reruns anyway.
      let last = self.last_params.get(&path);
      let knobs = last.map(|l| l.knobs.clone()).unwrap_or_default();
      let extras = last.map(|l| l.extras.clone()).unwrap_or_default();
      let selected_server = last.map(|l| l.server.clone()).unwrap_or_default();
      let (backend, server) = crate::tui::launch_picker::launch_identity(
        &selected_server,
        &self.compatible_servers(&path),
        &self
          .predicted_backend(&path)
          .map(crate::launch::params::BackendChoice::from_id)
          // Mirror `build_default_picker`: pin the explicit default backend
          // (not `Auto`) when the row has no prediction yet, so the preset saves
          // the llamacpp build actually shown instead of letting the daemon's
          // replay auto-routing pick another backend.
          .unwrap_or_else(|| {
            crate::launch::params::BackendChoice::from_id(crate::backend::DEFAULT_BACKEND_ID)
          }),
      );
      (knobs, extras, backend, server)
    };

    let (existing, arch_shadow) = self.existing_preset_names(&path);
    self.save_preset_dialog = Some(crate::tui::save_preset_dialog::SavePresetDialog::open(
      crate::tui::save_preset_dialog::SavePresetArgs {
        model_path: path,
        model_name,
        knobs,
        extras,
        backend,
        server,
        existing,
        arch_shadow,
      },
    ));
  }

  /// Open the `Alt+⏎` launch-name dialog for the focused model. On accept the
  /// normal launch picker opens carrying the typed name, so the launch is
  /// addressable as `<model-id>@<name>`. A plain `⏎` (OpenLaunchPicker) is
  /// untouched and launches unnamed.
  pub fn open_launch_name_dialog(&mut self) {
    let Some(path) = self.focused_path() else {
      return;
    };
    let model_name = self
      .display_label_for(&path)
      .unwrap_or_else(|| crate::util::paths::model_file_label(&path));
    self.launch_name_dialog = Some(crate::tui::launch_name_dialog::LaunchNameDialog::open(
      model_name,
    ));
  }

  /// The capture tuple (knobs, extras, backend, server) from a launch picker —
  /// shared by the open-picker and default-picker branches so a future field
  /// is added once. The identity comes from [`LaunchPickerState::launch_identity`]
  /// so `backend` and `server` cannot disagree.
  fn picker_capture(
    p: &LaunchPickerState,
  ) -> (
    crate::launch::knobs::KnobSet,
    Vec<String>,
    Option<String>,
    Option<String>,
  ) {
    let (backend, server) = p.launch_identity();
    (
      p.user_knobs.clone(),
      p.extras
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect(),
      backend,
      server,
    )
  }

  pub fn open_filter(&mut self) {
    self.focus = Focus::Filter;
    // Auto-enter edit so the user can type immediately. The Esc
    // walk-back (exit-edit → clear → close) handles teardown.
    self.filter_input.enter_edit();
  }

  /// Close the filter input entirely (matches the legacy
  /// `Esc clears + leaves filter mode` behaviour for callers that
  /// still want the one-shot reset). Distinct from
  /// [`crate::tui::input_field::InputField`]'s `Esc` walk-back, which only exits edit / clears
  /// the buffer; this resets both at once.
  pub fn clear_filter(&mut self) {
    self.filter_input.clear();
    self.filter_input.exit_edit();
    self.focus = Focus::List;
    self.clamp_cursor();
  }

  /// Apply a transient neutral toast (theme accent). Use for
  /// confirmations and refusal guards.
  pub fn show_toast(&mut self, msg: impl Into<String>) {
    self.toast = Some((msg.into(), Instant::now(), ToastKind::Info));
  }

  /// Apply a transient error toast (red). Use only for genuine
  /// failures — an attempted operation that did not complete — so the
  /// red bar stays meaningful.
  pub fn show_error_toast(&mut self, msg: impl Into<String>) {
    self.toast = Some((msg.into(), Instant::now(), ToastKind::Error));
  }

  /// Drop the toast if it's older than `TOAST_TTL`.
  pub fn expire_toast(&mut self) {
    if let Some((_, at, _)) = &self.toast {
      if at.elapsed() > TOAST_TTL {
        self.toast = None;
      }
    }
  }

  pub fn toast_message(&self) -> Option<&str> {
    self.toast.as_ref().map(|(s, _, _)| s.as_str())
  }

  /// Severity of the active toast, for the renderer to pick the bar
  /// colour. `None` when no toast is showing.
  pub fn toast_kind(&self) -> Option<ToastKind> {
    self.toast.as_ref().map(|(_, _, kind)| *kind)
  }

  /// The catalog mode hint for `path`, defaulting to chat when the row
  /// is missing or carries no metadata (the historical default for the
  /// right pane's mode surface).
  fn mode_hint_for(&self, path: &std::path::Path) -> crate::gguf::metadata::ModeHint {
    self
      .models
      .iter()
      .find(|m| m.path.as_path() == path)
      .and_then(|m| m.metadata.as_ref())
      .map(|md| md.mode_hint)
      .unwrap_or(crate::gguf::metadata::ModeHint::Chat)
  }

  /// Tabs the right pane should expose for the currently focused
  /// model. Every *managed* selection gets Requests, Logs and
  /// Settings, whatever its lifecycle state — the daemon's log ring
  /// buffer outlives the child process, and the requests a launch
  /// answered with stay filed under it. What varies is the rest: a
  /// Ready selection adds the mode-appropriate input tab, and an
  /// unlaunched / unfocused row gets Settings alone. There's no
  /// sticky fallback — what the user sees in the right pane is the
  /// model under the cursor, nothing else.
  pub fn available_right_tabs(&self) -> Vec<RightTab> {
    if let Some(cached) = self.right_tabs_cache.as_ref() {
      return cached.clone();
    }
    self.available_right_tabs_uncached()
  }

  /// The expensive compute path. Public only for the cache primer.
  fn available_right_tabs_uncached(&self) -> Vec<RightTab> {
    let managed = match self.focused_managed() {
      Some(m) => m,
      None => return vec![RightTab::Settings],
    };
    // A model with no mode surface (hint `Unknown`), or one that is not Ready,
    // gets the strip without one: the 503s of a failed load are filed under
    // the model, and Requests is where they show.
    match managed.state {
      SurfaceState::Ready => tabs_for_mode(self.mode_hint_for(&managed.path)),
      // Process alive but not yet serving, or dead but still in the
      // registry. Logs is fetchable either way: `status` only reports
      // rows the registry still holds, and the ring buffer is the only
      // surface that explains why a launch died (a post-Ready death by
      // external kill or OOM is recorded as plain `Stopped` with no
      // cause). Requests stays in both: the request waiting on a load,
      // or the 503 a failed auto-start answered with, is filed here.
      SurfaceState::Launching
      | SurfaceState::Loading
      | SurfaceState::Error
      | SurfaceState::Stopped => tabs_without_mode(),
      _ => vec![RightTab::Settings],
    }
  }

  /// The tab a row opens on: its first available one. Requests for any
  /// launch, Settings for a selection with no launch behind it.
  fn first_right_tab(&self) -> RightTab {
    *self
      .available_right_tabs()
      .first()
      .unwrap_or(&RightTab::Settings)
  }

  /// Clamp `right_tab` back to a reachable choice if the focused
  /// model's available tabs shrink (e.g. the model dropped from
  /// Ready to Stopped, or the cursor moved to an unlaunched row).
  /// Snaps to the first reachable tab — Round-8 fixed a latent bug
  /// where the fallback was hardcoded to `Logs`, which isn't part
  /// of the reachable set for unlaunched models, leaving the right
  /// pane painting nothing for those rows. Called by the renderer
  /// before drawing.
  pub fn ensure_right_tab_reachable(&mut self) {
    if !self.available_right_tabs().contains(&self.right_tab) {
      self.right_tab = self.first_right_tab();
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

  /// Reverse of [`Self::cycle_theme`] — walks the same active set
  /// backwards. Bound to `Shift+T` so a user who overshoots the
  /// theme they wanted with `t` can step back instead of cycling
  /// through every theme to land on it again.
  pub fn cycle_theme_prev(&mut self) {
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
      .unwrap_or(0);
    let prev = order[(pos + order.len() - 1) % order.len()];
    self.options.theme = prev;
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
      ListRow::Divider => {
        // Dividers are purely structural — they separate Favorites
        // from the folder groups in the unfiltered view. The
        // filtered view re-derives sections from kept rows, so the
        // divider has nothing to separate and just drops out.
        i += 1;
      }
    }
  }
  out
}

fn parse_list_models_row(row: &Value) -> Option<DiscoveredModel> {
  // Deserialise the one shared `CatalogRow` wire shape, then map it into the
  // TUI's `DiscoveredModel`. The JSON keys are read in exactly one place —
  // `CatalogRow`'s serde impl — so this never drifts from the daemon.
  let cr: crate::launch::resolve::CatalogRow = serde_json::from_value(row.clone()).ok()?;
  if cr.path.is_empty() {
    return None;
  }
  Some(discovered_from_catalog_row(&cr))
}

/// Map a wire [`CatalogRow`](crate::launch::resolve::CatalogRow) into the TUI's
/// domain [`DiscoveredModel`]. MTP capability round-trips through a sentinel
/// head path so `mtp_capable()` (and the `↯` badge) light up client-side; the
/// daemon re-resolves the real head at launch — the client only needs the bit.
fn discovered_from_catalog_row(cr: &crate::launch::resolve::CatalogRow) -> DiscoveredModel {
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModelMetadata, Quant};

  let mtp_embedded = cr.mtp.and_then(|m| m.embedded_layers);
  let mtp_head = cr
    .mtp
    .filter(|m| m.separate_head)
    .map(|_| PathBuf::from("<separate-mtp-head>"));
  // The daemon emits a `metadata` block iff a header parsed (and always sets
  // `mode_hint` when it does), so these signals track "had a metadata block".
  let has_metadata = cr.mode_hint.is_some()
    || cr.arch.is_some()
    || cr.quant.is_some()
    || cr.native_ctx.is_some()
    || cr.parameter_label.is_some()
    || cr.weights_bytes.is_some()
    || cr.total_parameters.is_some()
    || cr.tokenizer_kind.is_some()
    || cr.has_chat_template
    || cr.has_reasoning_hint
    || mtp_embedded.is_some();
  let metadata = has_metadata.then(|| ModelMetadata {
    arch: cr.arch.clone(),
    total_parameters: cr.total_parameters,
    parameter_label: cr.parameter_label.clone(),
    quant: cr
      .quant
      .as_deref()
      .map(parse_quant)
      .unwrap_or(Quant::Unknown(0)),
    // The wire's `quant` *is* the display label — `catalog_row` already
    // resolved the backend overlay into it. Keeping it verbatim is the only
    // way a non-GGML label survives: `parse_quant` below maps anything outside
    // the GGML tag set to `Unknown`, so an FP8/AWQ/GPTQ repo rendered `—` here
    // while `list` and `show` showed the real label.
    quant_label: cr.quant.clone(),
    native_ctx: cr.native_ctx,
    chat_template: None,
    tokenizer_kind: cr.tokenizer_kind.clone(),
    reasoning_hint: cr.has_reasoning_hint,
    mode_hint: parse_mode_hint(cr.mode_hint.as_deref()),
    weights_bytes: cr.weights_bytes,
    lazy_tensor_bytes: Vec::new(),
    mtp: mtp_embedded,
  });
  DiscoveredModel {
    path: PathBuf::from(&cr.path),
    parent: PathBuf::from(&cr.parent),
    source: ModelSource::from_label(&cr.source).unwrap_or(ModelSource::UserPath),
    metadata,
    parse_error: cr.parse_error.clone(),
    split_siblings: cr.split_siblings.iter().map(PathBuf::from).collect(),
    display_label: cr.display_label.clone(),
    multimodal: cr.multimodal,
    // Priority-ordered backends this model can run on, by engine id.
    // Feeds the launch picker's Server row (filters the server catalog).
    supported_backends: cr.supported_backends.clone(),
    mtp_head,
  }
}

fn parse_mode_hint(label: Option<&str>) -> crate::gguf::metadata::ModeHint {
  use crate::gguf::metadata::ModeHint;
  match label {
    Some("chat") => ModeHint::Chat,
    Some("embedding") => ModeHint::Embedding,
    Some("rerank") => ModeHint::Rerank,
    Some("image") => ModeHint::Image,
    Some("tts") => ModeHint::Tts,
    Some("stt") => ModeHint::Stt,
    _ => ModeHint::Unknown,
  }
}

fn parse_quant(label: &str) -> crate::gguf::metadata::Quant {
  // Route through the canonical `Quant::from_label` so the table
  // stays single-sourced; missing labels fall through to the
  // `Unknown(0)` sentinel without crashing the TUI on a future
  // quant tag the daemon learns about first. The `0` payload is
  // just "unknown ggml type" — not surfaced back to the user.
  crate::gguf::metadata::Quant::from_label(label)
    .unwrap_or(crate::gguf::metadata::Quant::Unknown(0))
}

fn parse_external_row(row: &Value) -> Option<ManagedRow> {
  let pid = u32::try_from(row.get("pid").and_then(Value::as_u64)?).ok()?;
  let path = row
    .get("model_path")
    .and_then(Value::as_str)
    .map(PathBuf::from)
    .unwrap_or_default();
  Some(ManagedRow {
    launch_id: format!("ext-{pid}"),
    pid: Some(pid),
    path,
    // Parsed off the process argv by the daemon. `0` means its argv carried no
    // port flag (or one out of range), which renderers must show as no port
    // rather than `:0`.
    port: row
      .get("port")
      .and_then(Value::as_u64)
      .and_then(|p| u16::try_from(p).ok())
      .unwrap_or(0),
    // Nothing in the TUI addresses an external row by name — `stop <model>@<name>`
    // is a CLI path — so the field is left as the sentinel the rest of the row uses.
    name: None,
    // An external process wasn't launched through a preset.
    preset: None,
    state: SurfaceState::External,
    device: None,
    rss_bytes: None,
    cpu_pct: None,
    resolved_ctx: None,
    ctx_clamped: false,
    knobs: crate::launch::knobs::KnobSet::new(),
    extras: Vec::new(),
    backend: None,
    server: None,
  })
}

/// Parse the daemon's `status.proxy` block into the TUI-side
/// [`ProxyInfo`] struct. Returns `None` for non-object / missing
/// shapes so an older daemon (pre-Unit-5) leaves the row off
/// instead of rendering a confusing "?" placeholder.
fn parse_proxy_info(v: &Value) -> Option<ProxyInfo> {
  let obj = v.as_object()?;
  let status = obj.get("status").and_then(Value::as_str)?.to_string();
  let enabled = obj
    .get("enabled")
    .and_then(Value::as_bool)
    .unwrap_or(status != "disabled");
  let listen = obj.get("listen").and_then(Value::as_str).map(String::from);
  let bind_error = obj
    .get("bind_error")
    .and_then(Value::as_str)
    .map(String::from);
  let auth = obj.get("auth").and_then(Value::as_str).map(String::from);
  Some(ProxyInfo {
    enabled,
    listen,
    status,
    bind_error,
    auth,
  })
}

fn parse_status_row(row: &Value) -> Option<ManagedRow> {
  let launch_id = row.get("launch_id")?.as_str()?.to_string();
  let port = row.get("port")?.as_u64()? as u16;
  let name = row.get("name").and_then(Value::as_str).map(String::from);
  // The preset this launch resolved, present only when one was in play.
  let preset = row.get("preset").and_then(Value::as_str).map(String::from);
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
  let state = SurfaceState::from_wire_label(state_label);
  let rss_bytes = row.get("latest_rss_bytes").and_then(Value::as_u64);
  let cpu_pct = row
    .get("latest_cpu_pct")
    .and_then(Value::as_f64)
    .map(|n| n as f32);
  let device = row
    .get("params")
    .and_then(|p| p.get("knobs"))
    .and_then(|k| k.get("device"))
    .and_then(Value::as_str)
    .map(|s| s.to_string());
  let resolved_ctx = row
    .get("resolved_ctx")
    .and_then(Value::as_u64)
    .map(|n| n as u32);
  let ctx_clamped = row
    .get("ctx_clamped")
    .and_then(Value::as_bool)
    .unwrap_or(false);
  // The knobs the launch was dispatched with — `auto` sentinels for
  // fit-delegated rows, pinned numbers when set. Parsed from the live
  // `status` row so the running view reflects the server, not the user's
  // saved `last_params`. A shape mismatch falls back to empty (all rows
  // then read `default`, the inherited sentinel).
  let knobs = row
    .get("params")
    .and_then(|p| p.get("knobs"))
    .and_then(|k| serde_json::from_value::<crate::launch::knobs::KnobSet>(k.clone()).ok())
    .unwrap_or_default();
  // The advanced `--` tail, so `Ctrl+P` save-from-running reproduces it.
  let extras = row
    .get("params")
    .and_then(|p| p.get("extras"))
    .and_then(Value::as_array)
    .map(|a| {
      a.iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
    })
    .unwrap_or_default();
  // The backend this launch actually resolved to.
  let backend = row.get("backend").and_then(Value::as_str).map(String::from);
  // The server (build/binary) the launch picked, when one was chosen — drives
  // the running view's read-only `server` row.
  let server = row
    .get("params")
    .and_then(|p| p.get("server"))
    .and_then(Value::as_str)
    .map(String::from);
  Some(ManagedRow {
    launch_id,
    // A launch of ours is addressed by its id, so it has no use for the pid.
    pid: None,
    path,
    port,
    name,
    preset,
    state,
    device,
    rss_bytes,
    cpu_pct,
    resolved_ctx,
    ctx_clamped,
    knobs,
    extras,
    backend,
    server,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::{Device, Server};
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
  use serde_json::json;

  #[test]
  fn cycle_left_pane_ratio_walks_slots_and_wraps() {
    let mut app = App::new(AppOptions {
      left_pane_ratios: vec![65, 100, 50],
      ..Default::default()
    });
    assert_eq!(app.left_pane_ratio(), 65, "starts at slot 0");
    app.cycle_left_pane_ratio();
    assert_eq!(app.left_pane_ratio(), 100);
    app.cycle_left_pane_ratio();
    assert_eq!(app.left_pane_ratio(), 50);
    app.cycle_left_pane_ratio();
    assert_eq!(app.left_pane_ratio(), 65, "wraps back to slot 0");
  }

  #[test]
  fn cycle_left_pane_ratio_single_slot_is_stable() {
    let mut app = App::new(AppOptions {
      left_pane_ratios: vec![70],
      ..Default::default()
    });
    app.cycle_left_pane_ratio();
    assert_eq!(app.left_pane_ratio(), 70, "one slot: cycling is a no-op");
  }

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
        quant_label: None,
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: Some(4_200_000_000),
        lazy_tensor_bytes: Vec::new(),
        mtp: None,
      }),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }
  }

  #[test]
  fn parse_list_models_row_preserves_split_siblings() {
    // Regression: the parser used to throw away `split_siblings`,
    // which meant the TUI's shard-aware SIZE computation only ever
    // stat'd shard 1 and reported ~half the real size for split
    // GGUFs. The wire shape carries the siblings; the TUI needs
    // them to call `shard_sizes::on_disk_total` correctly.
    let row = json!({
      "path": "/m/m-00001-of-00002.gguf",
      "parent": "/m",
      "source": "huggingface",
      "split_siblings": [
        "/m/m-00002-of-00002.gguf",
      ],
      "metadata": {
        "arch": "qwen3next",
        "quant": "Q5_K",
        "weights_bytes": 1_000_000_000_u64,
      },
    });
    let parsed = parse_list_models_row(&row).expect("row parses");
    assert_eq!(parsed.split_siblings.len(), 1);
    assert_eq!(
      parsed.split_siblings[0],
      PathBuf::from("/m/m-00002-of-00002.gguf")
    );
  }

  #[test]
  fn parse_list_models_row_maps_a_backend_registry_source() {
    // Regression: the parser's source match had no backend-registry arm, so
    // registry rows fell through to `UserPath`. Downstream that seeded
    // the launch picker with `model_backend = LlamaCpp` — the full
    // llama.cpp knob set rendered and the Backend chooser hid, even
    // though the daemon routes the launch to the registry backend.
    // The id comes from the registry so the test names no backend; any
    // non-default backend parses as a registry row.
    use crate::backend::Backend as _;
    let id = crate::backend::Backends::all()
      .into_iter()
      .map(|b| b.id())
      .find(|id| *id != crate::backend::DEFAULT_BACKEND_ID)
      .expect("a non-default backend is registered");
    let row = json!({
      "path": "qwen3:0.6b",
      "parent": "",
      "source": id,
      "display_label": "qwen3:0.6b",
    });
    let parsed = parse_list_models_row(&row).expect("row parses");
    assert_eq!(parsed.source, ModelSource::Backend(id));
    assert_eq!(parsed.source.backend_id(), id);
  }

  #[test]
  fn build_default_picker_seeds_a_registry_backend_from_the_focused_row() {
    // Live-flow companion to the picker's own registry-row test: that one sets
    // `model_backend` by hand; this one walks the real seeding path from
    // a catalog row, which is where the missing source arm broke it.
    use crate::tui::launch_picker::PickerField;
    let mut app = App::new(AppOptions::default());
    let mut row = fake("qwen3:0.6b", "");
    row.source = ModelSource::Backend("acme");
    row.display_label = Some("qwen3:0.6b".into());
    app.models = vec![row];
    // The picker now reads the daemon's per-row prediction (as `ingest_list_models`
    // records it), not a TUI-side source re-derivation.
    app
      .backend_by_path
      .insert(PathBuf::from("qwen3:0.6b"), "acme".into());
    app.list_cursor = 2;
    assert!(app.focused_path().is_some(), "cursor must sit on the row");
    let picker = app.build_default_picker().expect("picker builds");
    assert_eq!(
      picker.model_backend,
      crate::launch::params::BackendChoice::Explicit("acme".into())
    );
    assert!(
      picker
        .ordered_fields()
        .into_iter()
        .any(|f| f == PickerField::Preset),
      "the seeded picker always offers the preset row"
    );
  }

  #[test]
  fn parse_list_models_row_reads_multimodal() {
    use crate::discovery::Multimodal;
    let base = |mm: Value| {
      json!({
        "path": "/m/a.gguf",
        "parent": "/m",
        "source": "user",
        "multimodal": mm,
      })
    };
    // Object → flags surface verbatim.
    let vision =
      parse_list_models_row(&base(json!({ "vision": true, "audio": false }))).expect("row parses");
    assert_eq!(
      vision.multimodal,
      Some(Multimodal {
        vision: true,
        audio: false
      })
    );
    // null / absent → None.
    assert_eq!(
      parse_list_models_row(&base(Value::Null))
        .expect("row parses")
        .multimodal,
      None
    );
  }

  #[test]
  fn parse_list_models_row_reads_mtp_capability_for_the_badge() {
    // A real MTP row always carries a metadata block (both come from the same
    // header parse); the embedded count rides `metadata.mtp`.
    let row = |mtp: Value| {
      json!({
        "path": "/m/a.gguf",
        "parent": "/m",
        "source": "user",
        "metadata": { "arch": "qwen35", "quant": "Q4_K", "mode_hint": "chat" },
        "mtp": mtp,
      })
    };
    // No mtp block → not capable (no ↯).
    assert!(!parse_list_models_row(&row(Value::Null))
      .expect("row parses")
      .mtp_capable());
    // Embedded head → capable, embedded count surfaces.
    let embedded = parse_list_models_row(&row(
      json!({ "embedded_layers": 1, "separate_head": false }),
    ))
    .expect("row parses");
    assert!(embedded.mtp_capable(), "embedded head lights the badge");
    assert_eq!(embedded.metadata.and_then(|m| m.mtp), Some(1));
    // Separate head only → capable via the sentinel head.
    let separate = parse_list_models_row(&row(
      json!({ "embedded_layers": null, "separate_head": true }),
    ))
    .expect("row parses");
    assert!(separate.mtp_capable(), "separate head lights the badge");
    assert!(separate.mtp_head.is_some());
  }

  #[test]
  fn multi_backend_reads_the_daemon_prediction() {
    let mut app = App::new(AppOptions::default());
    app
      .backend_by_path
      .insert(PathBuf::from("/m/a.gguf"), "llamacpp".into());
    assert!(!app.multi_backend(), "all-llamacpp is single-backend");
    app
      .backend_by_path
      .insert(PathBuf::from("/m/b.gguf"), "enginex".into());
    assert!(app.multi_backend(), "a non-default backend row flips it on");
    // The per-path backend prediction comes straight off the daemon's badge.
    assert_eq!(
      app.predicted_backend(&PathBuf::from("/m/b.gguf")),
      Some("enginex")
    );
    assert_eq!(
      app.predicted_backend(&PathBuf::from("/m/a.gguf")),
      Some("llamacpp")
    );
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
      ..Default::default()
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
  fn ingest_status_drops_out_of_range_external_port_and_pid() {
    // A daemon-side parse could hand back anything. A wrapped port would render
    // a real-looking wrong endpoint, and a wrapped pid would render a `pid <n>`
    // label that names someone else's process, so both drop instead.
    let mut app = App::new(AppOptions::default());
    app.ingest_status(&json!({
      "models": [],
      "external": [
        {"pid": 9913, "cmdline": "llama-server --port 70000 -m /m/ext.gguf",
         "model_path": "/m/ext.gguf", "port": 70000, "launched_by_llamastash": false},
        {"pid": 4294967296u64, "cmdline": "llama-server --port 41200 -m /m/wide.gguf",
         "model_path": "/m/wide.gguf", "port": 41200, "launched_by_llamastash": false},
      ]
    }));
    assert_eq!(
      app.external.len(),
      1,
      "the out-of-range pid is not ingested"
    );
    assert_eq!(app.external[0].port, 0);
    assert_eq!(app.external[0].pid, Some(9913));
  }

  #[test]
  fn ingest_status_reads_an_external_rows_port() {
    let mut app = App::new(AppOptions::default());
    let body = json!({
      "external": [
        {"pid": 9911, "cmdline": "llama-server --port 41102 -m /m/ext.gguf",
         "model_path": "/m/ext.gguf", "port": 41102, "launched_by_llamastash": false,
         "name": "coder"},
        {"pid": 9912, "cmdline": "llama-server -m /m/scan.gguf",
         "model_path": "/m/scan.gguf", "port": null, "launched_by_llamastash": false},
      ]
    });
    app.ingest_status(&body);
    assert_eq!(app.external.len(), 2);
    assert_eq!(app.external[0].port, 41102);
    // An argv with no port flag leaves `port` 0, and the renderers have to skip
    // it rather than print `:0` as if it were an endpoint. A port out of the
    // `u16` range is the same "nothing to show", not a wrapped number.
    assert_eq!(app.external[1].port, 0);
    assert_eq!(
      app.surface_ports().get(&PathBuf::from("/m/scan.gguf")),
      None,
      "a portless external row must not surface a port"
    );
    assert_eq!(
      app.surface_ports().get(&PathBuf::from("/m/ext.gguf")),
      Some(&41102)
    );
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
    // Absent `preset` key reads as no preset — the wire omits it for a
    // presetless launch rather than sending null.
    assert_eq!(app.managed[0].preset, None);
    let mut with_preset = body.clone();
    with_preset["models"][0]["preset"] = json!("fast");
    app.ingest_status(&with_preset);
    assert_eq!(app.managed[0].preset.as_deref(), Some("fast"));
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
  fn ingest_status_populates_proxy_info_from_proxy_block() {
    let mut app = App::new(AppOptions::default());
    let body = json!({
      "daemon": {"pid": 1, "uptime_seconds": 0, "active_connections": 0},
      "proxy": {
        "enabled": true,
        "listen": "127.0.0.1:11434",
        "status": "listening",
        "bind_error": null,
      }
    });
    app.ingest_status(&body);
    let proxy = app.daemon_info.proxy.as_ref().expect("proxy info parsed");
    assert!(proxy.enabled);
    assert_eq!(proxy.listen.as_deref(), Some("127.0.0.1:11434"));
    assert_eq!(proxy.status, "listening");
    assert!(proxy.bind_error.is_none());
  }

  #[test]
  fn ingest_status_toasts_once_on_port_in_use_transition() {
    // Plan §Approach: toast fires on the *transition* into
    // port_in_use, not on every poll. A second consecutive
    // port_in_use observation must not re-fire the toast.
    let mut app = App::new(AppOptions::default());
    let listening = json!({
      "daemon": {"pid": 1, "uptime_seconds": 0, "active_connections": 0},
      "proxy": {
        "enabled": true,
        "listen": "127.0.0.1:11434",
        "status": "listening",
        "bind_error": null,
      }
    });
    app.ingest_status(&listening);
    assert!(app.toast.is_none(), "listening must not toast");

    let collided = json!({
      "daemon": {"pid": 1, "uptime_seconds": 0, "active_connections": 0},
      "proxy": {
        "enabled": true,
        "listen": "127.0.0.1:11434",
        "status": "port_in_use",
        "bind_error": null,
      }
    });
    app.ingest_status(&collided);
    let first = app
      .toast
      .clone()
      .expect("transition into port_in_use must toast");
    assert!(
      first.0.contains("port") && first.0.contains("11434"),
      "toast must name the listen address: {first:?}"
    );

    // Clear the toast (simulate it expiring) and re-ingest the same
    // port_in_use status. No new toast should fire.
    app.toast = None;
    app.ingest_status(&collided);
    assert!(
      app.toast.is_none(),
      "subsequent identical port_in_use ticks must NOT re-toast"
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
  fn opening_the_hf_dialog_ranks_the_host_and_picks_up_a_multi_backend_card() {
    use crate::daemon::host_metrics::DeviceRow;
    let mut app = App::new(AppOptions::default());
    // A hybrid laptop: the wire label is `multi`, and the only card worth
    // ranking against is the NVIDIA one (the Vulkan row is an iGPU with no
    // memory of its own).
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_MULTI.into(),
      gpu_mem_total_bytes: Some(4 * 1024 * 1024 * 1024),
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      gpu_device_count: 2,
      gpu_devices: Some(vec![
        DeviceRow {
          selector: "Nvidia0".into(),
          backend: "nvidia".into(),
          name: "RTX 3050 Ti".into(),
          total_memory_bytes: 4 * 1024 * 1024 * 1024,
          used_memory_bytes: None,
          utilization_pct: None,
          temperature_c: None,
        },
        DeviceRow {
          selector: "Vulkan1".into(),
          backend: "unknown".into(),
          name: "Intel UHD".into(),
          total_memory_bytes: 0,
          used_memory_bytes: None,
          utilization_pct: None,
          temperature_c: None,
        },
      ]),
      ..Default::default()
    };
    app.open_hf_dialog();
    let dialog = app.hf_dialog.expect("dialog opens");
    assert_eq!(dialog.hardware_fit_ctx.backend, "cuda");
    assert!(
      !dialog.recs.is_empty(),
      "a 4 GiB CUDA card still gets recommendations"
    );
    assert!(dialog.shows_recommendations());
  }

  #[test]
  fn the_fit_context_sizes_against_the_smallest_card_of_the_ranked_backend() {
    use crate::daemon::host_metrics::DeviceRow;
    const GIB: u64 = 1024 * 1024 * 1024;
    let row = |name: &str, backend: &str, bytes: u64| DeviceRow {
      selector: name.into(),
      backend: backend.into(),
      name: name.into(),
      total_memory_bytes: bytes,
      used_memory_bytes: None,
      utilization_pct: None,
      temperature_c: None,
    };
    let snapshot = crate::init::benchmark::load_bundled();
    let mut app = App::new(AppOptions::default());
    // Two identical cards: a launch gets one of them, so the ranking sizes for
    // one, not for the pair the daemon sums up.
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_NVIDIA.into(),
      gpu_mem_total_bytes: Some(48 * GIB),
      ram_total_bytes: 64 * GIB,
      gpu_devices: Some(vec![
        row("RTX 4090 #0", "nvidia", 24 * GIB),
        row("RTX 4090 #1", "nvidia", 24 * GIB),
      ]),
      ..Default::default()
    };
    assert_eq!(
      app.hf_hardware_fit_ctx(&snapshot).vram_bytes,
      Some(24 * GIB),
      "two cards rank as one",
    );

    // Hybrid laptop: the iGPU's UMA marker makes the host verdict `unified`,
    // and the daemon sums the iGPU's memory into the RTX's.
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_MULTI.into(),
      gpu_mem_total_bytes: Some(10 * GIB),
      ram_total_bytes: 32 * GIB,
      unified: true,
      uma_shared_total_bytes: Some(16 * GIB),
      gpu_devices: Some(vec![
        row("Iris Xe", "unknown", 2 * GIB),
        row("RTX 4060 Laptop", "nvidia", 8 * GIB),
      ]),
      gpu_device_count: 2,
      ..Default::default()
    };
    let ctx = app.hf_hardware_fit_ctx(&snapshot);
    assert_eq!(
      ctx.backend, "cuda",
      "the discrete card is the one to rank for"
    );
    assert_eq!(
      ctx.vram_bytes,
      Some(8 * GIB),
      "the iGPU's memory is not the RTX's"
    );
    assert!(
      !ctx.unified && ctx.uma_shared_total_bytes.is_none(),
      "a ranked discrete card is not a unified pool",
    );
  }

  /// The repos the dialog's recommendation list shows, in rank order.
  fn recommendation_repos(state: &crate::tui::hf_dialog::HfDialogState) -> Vec<String> {
    use crate::init::recommender::RecommendationKind;
    state
      .recs
      .iter()
      .map(|r| match &r.kind {
        RecommendationKind::Curated { entry } => entry.repo.clone(),
        _ => String::new(),
      })
      .collect()
  }

  #[test]
  fn a_fresher_snapshot_replaces_the_ranking_and_survives_the_next_open() {
    let mut app = App::new(AppOptions::default());
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: HostMetricsSnapshot::BACKEND_NVIDIA.into(),
      gpu_mem_total_bytes: Some(4 * 1024 * 1024 * 1024),
      ram_total_bytes: 32 * 1024 * 1024 * 1024,
      ..Default::default()
    };
    app.open_hf_dialog();
    let before = recommendation_repos(app.hf_dialog.as_ref().unwrap());

    // The remote tier answering with a catalog that kept one model's rows.
    let mut fresh = crate::init::benchmark::load_bundled();
    fresh.bundle_date = "2999-01-01".into();
    fresh.models.retain(|e| e.repo == "Qwen/Qwen3-4B");
    let evt = crate::tui::hf_dialog::HfDialogEvent::Snapshot(Box::new(fresh));
    crate::tui::hf_pull::apply_hf_dialog_event(&mut app, evt);

    let after = recommendation_repos(app.hf_dialog.as_ref().unwrap());
    assert!(!after.is_empty(), "the fresh catalog still ranks a pick");
    assert!(
      after.iter().all(|repo| repo == "Qwen/Qwen3-4B"),
      "the dialog ranks from the snapshot that arrived: {after:?}"
    );
    assert_ne!(before, after, "the bundled ranking was replaced");

    // Cached, so a later open ranks from it without a second fetch.
    app.close_hf_dialog();
    app.open_hf_dialog();
    assert_eq!(recommendation_repos(app.hf_dialog.as_ref().unwrap()), after);
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
  fn drill_into_focused_model_stages_picker_when_idle() {
    // Idle row: Enter-on-list → drill stages the picker so the
    // *next* Enter dispatches a launch.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.list_cursor = 2;
    assert!(app.focused_managed().is_none());
    app.drill_into_focused_model();
    assert!(
      app.launch_picker.is_some(),
      "idle drill must stage the picker so Enter → launch reads"
    );
    assert_eq!(app.focus, Focus::RightPane);
    assert_eq!(app.right_tab, RightTab::Settings);
  }

  #[test]
  fn drill_into_focused_model_skips_picker_when_running() {
    // Running row: Enter-on-list should *not* stage a launch
    // picker — it lands on the launch's first tab (Requests). The
    // user explicitly stages via `e:edit for launch`, then Enter
    // launches. Avoids accidentally dispatching a duplicate
    // instance from a stray Enter.
    let m = fake("/m/a.gguf", "/m");
    let mut app = App::new(AppOptions::default());
    app.models = vec![m.clone()];
    app.managed = vec![ManagedRow {
      launch_id: "L1".into(),
      path: m.path.clone(),
      port: 41100,
      state: SurfaceState::Ready,
      device: None,
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }];
    app.list_cursor = 2;
    assert!(app.focused_managed().is_some());
    app.drill_into_focused_model();
    assert!(
      app.launch_picker.is_none(),
      "running drill must NOT stage a picker — only `e` does that"
    );
    assert_eq!(
      app.focus,
      Focus::RightPane,
      "drill still focuses the right pane"
    );
    assert_eq!(
      app.right_tab,
      RightTab::Requests,
      "drill opens the strip head, not the params view"
    );
  }

  fn dev(selector: &str, gpu: &str) -> Device {
    Device {
      selector: selector.into(),
      gpu_backend: gpu.into(),
      name: "Test GPU".into(),
      total_mib: Some(24576),
      free_mib: Some(24000),
    }
  }

  /// A server carrying `devices` on `binary`.
  fn srv(binary: &str, devices: Vec<Device>) -> Server {
    Server {
      id: format!("llamacpp-{binary}"),
      backend_id: "llamacpp".into(),
      binary: PathBuf::from(binary),
      name: "test".into(),
      devices,
      caps: Default::default(),
      label: None,
    }
  }

  /// Build an app whose cursor sits on a running model bound to
  /// `device`, with `server_path` as the daemon's default binary.
  fn running_on_device_app(device: Option<&str>, server_path: &str) -> App {
    let m = fake("/m/a.gguf", "/m");
    let mut app = App::new(AppOptions::default());
    app.models = vec![m.clone()];
    app.managed = vec![ManagedRow {
      launch_id: "L1".into(),
      path: m.path.clone(),
      port: 41100,
      state: SurfaceState::Ready,
      device: device.map(String::from),
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }];
    app.daemon_info = DaemonInfo {
      server_path: Some(server_path.into()),
      ..Default::default()
    };
    // Rows: [TableHeader, Header(▶ Running), Model(running)] — the
    // running row lands at index 2.
    app.list_cursor = 2;
    app
  }

  #[test]
  fn focused_override_device_returns_entry_for_non_default_binary() {
    let mut app = running_on_device_app(Some("CUDA1"), "/usr/bin/llama-server");
    app.servers = vec![
      srv("/usr/bin/llama-server", vec![dev("CUDA0", "CUDA")]),
      srv("/opt/cuda/llama-server", vec![dev("CUDA1", "CUDA")]),
    ];
    assert!(app.focused_managed().is_some(), "cursor must be on the run");
    let dev = app
      .focused_override_device()
      .expect("non-default binary must surface as an override");
    assert_eq!(
      dev,
      OverrideServer {
        backend_id: "llamacpp".into(),
        index: 1,
      }
    );
  }

  #[test]
  fn focused_override_device_none_when_binary_is_default() {
    // The hovered launch runs on the *default* binary's own device —
    // no override, so the server row stays on the plain default render.
    let mut app = running_on_device_app(Some("CUDA0"), "/usr/bin/llama-server");
    app.servers = vec![srv("/usr/bin/llama-server", vec![dev("CUDA0", "CUDA")])];
    assert!(app.focused_override_device().is_none());
  }

  #[test]
  fn focused_override_device_none_when_row_not_running() {
    // No device selector on the row (idle / auto launch) — nothing to
    // resolve against the catalog.
    let mut app = running_on_device_app(None, "/usr/bin/llama-server");
    app.servers = vec![srv("/opt/cuda/llama-server", vec![dev("CUDA1", "CUDA")])];
    assert!(app.focused_override_device().is_none());
  }

  #[test]
  fn focused_override_device_none_when_selector_absent_from_catalog() {
    // Stale persisted selector (catalog re-probed without it) must not
    // panic or fabricate an override.
    let mut app = running_on_device_app(Some("ROCm0"), "/usr/bin/llama-server");
    app.servers = vec![srv("/usr/bin/llama-server", vec![dev("CUDA0", "CUDA")])];
    assert!(app.focused_override_device().is_none());
  }

  #[test]
  fn multi_device_gates_on_within_server_count_not_cross_build_selectors() {
    let mut app = App::new(AppOptions::default());
    // One physical GPU, two llama.cpp builds: the HIP build calls it ROCm0,
    // the Vulkan build calls it Vulkan0. Two distinct selectors, but no
    // per-launch device choice on either server → column stays hidden.
    app.servers = vec![
      srv("/opt/hip/llama-server", vec![dev("ROCm0", "ROCm")]),
      srv("/opt/vulkan/llama-server", vec![dev("Vulkan0", "Vulkan")]),
    ];
    assert!(
      !app.multi_device(),
      "single GPU seen by two builds is not a multi-device host"
    );
    // A genuine two-GPU server flips it on.
    app.servers = vec![srv(
      "/opt/hip/llama-server",
      vec![dev("ROCm0", "ROCm"), dev("ROCm1", "ROCm")],
    )];
    assert!(
      app.multi_device(),
      "one server with two devices → column shows"
    );
    // No servers at all → single-device by definition.
    app.servers.clear();
    assert!(!app.multi_device());
  }

  #[test]
  fn multi_device_ignores_one_card_reported_under_two_compute_apis() {
    // A build compiled with both HIP and Vulkan reports the same card once per
    // API. Two selectors on one server, but still one GPU — `dev()` gives both
    // the same adapter name, which is what identifies them as one card.
    let mut app = App::new(AppOptions::default());
    app.servers = vec![srv(
      "/opt/fp4/llama-server",
      vec![dev("ROCm0", "ROCm"), dev("Vulkan0", "Vulkan")],
    )];
    assert!(
      !app.multi_device(),
      "one card under two compute APIs is not a multi-GPU host"
    );
  }

  #[test]
  fn gate_facts_for_managed_scope_to_the_launchs_server() {
    let mut app = App::new(AppOptions::default());
    let mut one_card_two_apis = srv(
      "/opt/fp4/llama-server",
      vec![dev("ROCm0", "ROCm"), dev("Vulkan0", "Vulkan")],
    );
    one_card_two_apis.id = "llamacpp-fp4".into();
    let mut two_gpus = srv(
      "/opt/hip/llama-server",
      vec![dev("ROCm0", "ROCm"), dev("ROCm1", "ROCm")],
    );
    two_gpus.id = "llamacpp-rocm".into();
    app.servers = vec![one_card_two_apis, two_gpus];

    let row = |server: Option<&str>, device: Option<&str>| ManagedRow {
      path: PathBuf::from("/m/a.gguf"),
      server: server.map(String::from),
      device: device.map(String::from),
      backend: Some(crate::backend::DEFAULT_BACKEND_ID.into()),
      ..Default::default()
    };

    // Dual-API build: a compute-path choice, but nothing to place across.
    let fp4 = app.gate_facts_for_managed(&row(Some("llamacpp-fp4"), None));
    assert!(fp4.device_choice);
    assert!(!fp4.multi_device);

    // Genuine two-GPU build: both open.
    let two_gpu = app.gate_facts_for_managed(&row(Some("llamacpp-rocm"), None));
    assert!(two_gpu.device_choice);
    assert!(two_gpu.multi_device);

    // No recorded pick: the selector identifies the owning server.
    let by_selector = app.gate_facts_for_managed(&row(None, Some("ROCm1")));
    assert!(
      by_selector.multi_device,
      "ROCm1 only exists on the two-GPU build"
    );

    // Neither pick nor selector: the backend's default server — the catalog
    // entry matching the daemon's `server_path`.
    app.daemon_info = DaemonInfo {
      server_path: Some("/opt/fp4/llama-server".into()),
      ..Default::default()
    };
    let by_default = app.gate_facts_for_managed(&row(None, None));
    assert!(!by_default.multi_device, "default build here sees one card");

    // A recorded id the catalog no longer has (binary rebuilt away) resolves
    // like the daemon does — down to the backend's default server.
    let stale = app.gate_facts_for_managed(&row(Some("llamacpp-gone"), None));
    assert!(!stale.multi_device, "stale id falls to the default build");

    // Nothing to place it with at all (untagged backend, catalog still
    // probing) → host-wide answer, so rows that exist stay visible.
    let untagged = ManagedRow {
      path: PathBuf::from("/m/a.gguf"),
      backend: None,
      ..Default::default()
    };
    assert!(app.server_for_managed(&untagged).is_none());
    assert!(
      app.gate_facts_for_managed(&untagged).multi_device,
      "host-wide fallback keeps rows visible"
    );
  }

  #[test]
  fn compatible_servers_scopes_to_the_models_supported_backends() {
    // A registry model (supported_backends = [enginex]) must offer only that
    // engine's server in the picker, not the whole host catalog — the bug
    // where every server showed but the launch went elsewhere anyway.
    let mut app = App::new(AppOptions::default());
    let mut m = fake("/m/qwen.gguf", "/m");
    m.supported_backends = vec!["enginex".into()];
    app.models = vec![m];
    app.servers = vec![
      srv("/usr/bin/llama-server", vec![dev("CUDA0", "CUDA")]),
      Server {
        id: "enginex".into(),
        backend_id: "enginex".into(),
        binary: PathBuf::from("/usr/bin/enginex"),
        name: "enginex".into(),
        devices: Vec::new(),
        caps: Default::default(),
        label: None,
      },
    ];
    let servers = app.compatible_servers(Path::new("/m/qwen.gguf"));
    assert_eq!(servers.len(), 1, "scoped to the one enginex server");
    assert_eq!(servers[0].backend_id, "enginex");
  }

  #[test]
  fn compatible_servers_lead_each_backend_with_the_build_the_daemon_defaults_to() {
    // Two llama.cpp builds; the daemon's default binary is the second one in
    // catalog order. The picker reads `servers[0]` as "the default" — if that
    // stayed catalog-first, the Server row would offer `llamacpp-rocm
    // (default)` while a launch with no pick actually spawned the Vulkan build.
    let mut app = App::new(AppOptions::default());
    let mut m = fake("/m/qwen.gguf", "/m");
    m.supported_backends = vec![crate::backend::DEFAULT_BACKEND_ID.into()];
    app.models = vec![m];
    app.servers = vec![
      srv("/b/rocm/llama-server", vec![dev("ROCm0", "ROCm")]),
      srv("/b/vulkan/llama-server", vec![dev("Vulkan0", "Vulkan")]),
    ];
    app.daemon_info = DaemonInfo {
      server_path: Some("/b/vulkan/llama-server".into()),
      ..Default::default()
    };
    let servers = app.compatible_servers(Path::new("/m/qwen.gguf"));
    assert_eq!(
      servers.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
      vec![
        "llamacpp-/b/vulkan/llama-server",
        "llamacpp-/b/rocm/llama-server"
      ],
      "the default build leads; the rest keep catalog order"
    );
    // And it agrees with the resolver the running view uses.
    let row = ManagedRow {
      path: PathBuf::from("/m/qwen.gguf"),
      backend: Some(crate::backend::DEFAULT_BACKEND_ID.into()),
      ..Default::default()
    };
    assert_eq!(
      app.server_for_managed(&row).map(|s| s.id.as_str()),
      Some(servers[0].id.as_str())
    );
  }

  #[test]
  fn drill_into_focused_model_noop_on_header_focus() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.list_cursor = 0;
    app.drill_into_focused_model();
    assert!(app.launch_picker.is_none());
    assert_ne!(
      app.focus,
      Focus::RightPane,
      "header drill must not move focus"
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
    // Wide width: pane shows regardless of focus (models non-empty).
    assert!(
      app.right_pane_visible_at(120),
      "right pane must be visible after open_launch_picker (wide)"
    );
    // Compact width: pane shows because focus drilled into RightPane.
    assert!(
      app.right_pane_visible_at(60),
      "right pane must be visible after open_launch_picker (compact)"
    );
  }

  fn ready_managed(path: &str, port: u16, state: SurfaceState) -> ManagedRow {
    ManagedRow {
      launch_id: format!("L-{port}"),
      path: PathBuf::from(path),
      port,
      state,
      device: None,
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }
  }

  #[test]
  fn a_tui_started_launch_rings_once_on_its_terminal_state() {
    // The bell is the "you can come back now" signal, so it fires on the
    // first tick that reports Ready or Error and never again for that
    // launch. Loading is in-flight: no ring, and the watch survives so the
    // later Ready still rings.
    use crate::tui::app::BellWatch;
    let mut app = App::new(AppOptions::default());
    app.bell_watch = vec![BellWatch::waiting("/m/qwen.gguf")];
    let loading = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "loading" },
      }]
    });
    app.ingest_status(&loading);
    assert_eq!(
      app.bell_watch,
      vec![BellWatch::waiting("/m/qwen.gguf")],
      "a load in flight keeps the watch, and a row on screen means no misses"
    );
    let ready = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "ready" },
      }]
    });
    app.ingest_status(&ready);
    assert!(
      app.bell_watch.is_empty(),
      "Ready consumes the watch so the launch rings exactly once"
    );
    // A second Ready tick has nothing left to ring for.
    app.ingest_status(&ready);
    assert!(app.bell_watch.is_empty());
  }

  #[test]
  fn two_launches_of_one_model_ring_twice() {
    // An additive duplicate launch has its own row and its own end, so "one
    // bell per launch" counts entries, not distinct paths.
    use crate::tui::app::BellWatch;
    let watch = vec![
      BellWatch::waiting("/m/qwen.gguf"),
      BellWatch::waiting("/m/qwen.gguf"),
    ];
    let rows = vec![
      ready_managed("/m/qwen.gguf", 41100, SurfaceState::Ready),
      ready_managed("/m/qwen.gguf", 41101, SurfaceState::Ready),
    ];
    let (keep, fired) = App::bell_tick(&watch, &rows);
    assert_eq!(fired, 2, "two launches, two bells");
    assert!(keep.is_empty());
    // One row and two entries: one rings now, the other keeps waiting for its
    // own launch rather than going quiet.
    let (still_waiting, second_rings) = App::bell_tick(&watch, &rows[..1]);
    assert_eq!(second_rings, 1);
    assert_eq!(still_waiting, vec![BellWatch::waiting("/m/qwen.gguf")]);
  }

  #[test]
  fn replies_bind_to_the_watches_in_dispatch_order() {
    // The whole binding rule rests on the writer answering in dispatch order:
    // one task, sequential calls, so the oldest unbound watch is the launch
    // this reply names.
    let mut app = App::new(AppOptions::default());
    app.queue_bell_watch("/m/a.gguf");
    app.queue_bell_watch("/m/b.gguf");
    app.bind_bell_watch("L-second");
    app.bind_bell_watch("L-first");
    assert_eq!(app.bell_watch[0].launch_id.as_deref(), Some("L-second"));
    assert_eq!(app.bell_watch[1].launch_id.as_deref(), Some("L-first"));
  }

  #[test]
  fn a_watch_never_claims_a_row_that_predates_its_launch() {
    // The duplicate-launch case: the model is already Ready from an earlier
    // launch, and the user launches it again. A watch matched on path alone
    // would ring a second after the keystroke off the old Ready row and then
    // stay quiet when the launch it belongs to finally comes up.
    let mut app = App::new(AppOptions::default());
    let old_ready = serde_json::json!({
      "models": [{
        "launch_id": "L-old",
        "id": { "path": "/m/a.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "ready" },
      }]
    });
    app.ingest_status(&old_ready);
    app.queue_bell_watch("/m/a.gguf");
    assert_eq!(
      app.bell_watch[0].known,
      vec!["L-old".to_string()],
      "the rows on screen at dispatch are excluded from the start"
    );
    app.ingest_status(&old_ready);
    assert_eq!(
      app.bell_watch.len(),
      1,
      "a Ready row that predates the launch is not its end"
    );

    // The daemon names the launch in its reply; only that row can satisfy the
    // watch now, even though it is for the same path.
    app.bind_bell_watch("L-new");
    let both_ready = serde_json::json!({
      "models": [
        {
          "launch_id": "L-old",
          "id": { "path": "/m/a.gguf", "header_hash": "h" },
          "port": 41100,
          "state": { "state": "ready" },
        },
        {
          "launch_id": "L-new",
          "id": { "path": "/m/a.gguf", "header_hash": "h" },
          "port": 41101,
          "state": { "state": "ready" },
        }
      ]
    });
    app.ingest_status(&both_ready);
    assert!(
      app.bell_watch.is_empty(),
      "its own Ready row rings, and the old row did not consume the watch"
    );
  }

  #[test]
  fn a_bound_watch_ignores_another_launch_of_the_same_model() {
    // The stuck-launch case: this watch's row is still loading, so the watch
    // stays, but a different launch turning Ready must not spend it.
    use crate::tui::app::BellWatch;
    // `ready_managed` names a row after its port, so this watch is bound to the
    // 41100 launch and the 41101 one belongs to someone else.
    let watch = vec![BellWatch {
      launch_id: Some("L-41100".into()),
      ..BellWatch::waiting("/m/a.gguf")
    }];
    let rows = vec![
      ready_managed("/m/a.gguf", 41100, SurfaceState::Loading),
      ready_managed("/m/a.gguf", 41101, SurfaceState::Ready),
    ];
    let (still, fired) = App::bell_tick(&watch, &rows);
    assert_eq!(fired, 0, "that Ready row belongs to another launch");
    assert_eq!(still, watch, "its own loading row holds the watch open");

    let mine_now_ready = vec![
      ready_managed("/m/a.gguf", 41100, SurfaceState::Ready),
      ready_managed("/m/a.gguf", 41101, SurfaceState::Ready),
    ];
    let (still_after, rings) = App::bell_tick(&watch, &mine_now_ready);
    assert_eq!(rings, 1);
    assert!(still_after.is_empty());
  }

  #[test]
  fn a_watch_with_no_candidate_dies_on_the_eighth_miss() {
    // Pinned to BELL_WATCH_MAX_MISSES so the boundary is a fact in the suite and
    // not an off-by-one nobody checked.
    let unrelated = [ready_managed("/m/other.gguf", 41101, SurfaceState::Ready)];
    let mut pending = vec![BellWatch::waiting("/m/qwen.gguf")];
    for tick in 1..8 {
      let (aging, fired) = App::bell_tick(&pending, &unrelated);
      assert_eq!(fired, 0, "an unwatched model's Ready row never rings");
      assert_eq!(
        aging,
        vec![BellWatch {
          misses: tick,
          ..BellWatch::waiting("/m/qwen.gguf")
        }],
        "tick {tick}: one more miss"
      );
      pending = aging;
    }
    let (aging, fired) = App::bell_tick(&pending, &unrelated);
    assert_eq!(fired, 0);
    assert!(aging.is_empty(), "the eighth miss with no row is the last");
  }

  #[test]
  fn a_bell_off_tui_leaves_its_watches_alone() {
    // The gate covers the whole bookkeeping, expiry included: nothing rings, so
    // nothing ages either.
    let mut app = App::new(AppOptions {
      bell: false,
      ..AppOptions::default()
    });
    app.bell_watch = vec![crate::tui::app::BellWatch::waiting("/m/qwen.gguf")];
    let before = app.bell_watch.clone();
    for _ in 0..20 {
      app.ingest_status(&serde_json::json!({ "models": [] }));
    }
    assert_eq!(app.bell_watch, before);
  }

  #[test]
  fn a_failed_tui_launch_rings_and_an_unwatched_one_never_rings() {
    use crate::tui::app::BellWatch;
    let errored = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "error", "cause": "probe timeout" },
      }]
    });
    let mut app = App::new(AppOptions::default());
    app.bell_watch = vec![BellWatch::waiting("/m/qwen.gguf")];
    app.ingest_status(&errored);
    assert!(
      app.bell_watch.is_empty(),
      "a failed load is the end the user is waiting for too"
    );

    // A launch started by the CLI or `preload` is not watched, so the TUI
    // stays silent for it.
    let mut other = App::new(AppOptions::default());
    other.ingest_status(&serde_json::json!({
      "models": [{
        "launch_id": "L2",
        "id": { "path": "/m/other.gguf", "header_hash": "h" },
        "port": 41102,
        "state": { "state": "ready" },
      }]
    }));
    assert!(other.bell_watch.is_empty());
  }

  #[test]
  fn bell_tick_pairs_one_ring_per_watched_terminal_row() {
    use crate::tui::app::BellWatch;
    let watch = vec![
      BellWatch::waiting("/m/a.gguf"),
      BellWatch::waiting("/m/b.gguf"),
      BellWatch::waiting("/m/gone.gguf"),
    ];
    let rows = vec![
      ready_managed("/m/a.gguf", 41100, SurfaceState::Ready),
      ready_managed("/m/b.gguf", 41101, SurfaceState::Error),
      ready_managed("/m/c.gguf", 41102, SurfaceState::Ready),
    ];
    let (keep, fired) = App::bell_tick(&watch, &rows);
    assert_eq!(fired, 2);
    assert_eq!(
      keep,
      vec![crate::tui::app::BellWatch {
        misses: 1,
        ..BellWatch::waiting("/m/gone.gguf")
      }],
      "only the missing row ages"
    );
    // Still-loading row: nothing fires.
    let (still_waiting, nothing_fired) = App::bell_tick(
      &watch[..1],
      &[ready_managed("/m/a.gguf", 41100, SurfaceState::Loading)],
    );
    assert_eq!(nothing_fired, 0);
    assert_eq!(still_waiting, watch[..1]);
  }

  #[test]
  fn ingest_status_snaps_right_tab_to_logs_on_error_transition() {
    // When the focused launch transitions from Loading → Error, the
    // right pane should auto-switch to Logs so the failure tail is
    // visible without an extra keystroke. Settings tab is too far
    // from the cause of failure for a user who just saw the launch
    // fail.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    let loading = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "loading" },
      }]
    });
    app.ingest_status(&loading);
    app.right_tab = RightTab::Settings;
    let errored = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "error", "cause": "probe timeout" },
      }]
    });
    app.ingest_status(&errored);
    assert_eq!(
      app.right_tab,
      RightTab::Logs,
      "Error transition should snap the right pane to Logs"
    );
    // Logs is also reachable for an Error row going forward (so a
    // user who arrives at the row later still sees the tab).
    assert!(
      app.available_right_tabs().contains(&RightTab::Logs),
      "Error rows must expose the Logs tab"
    );
  }

  #[test]
  fn stopped_row_exposes_the_logs_tab() {
    // A `stopped` row only ever reaches the TUI when the child died
    // without going through `stop` (external kill, OOM, post-Ready
    // crash) — a clean stop deregisters the launch and the row
    // disappears. Such an exit is recorded with no cause, so the log
    // tail is the only place the reason shows up; the tab used to be
    // unreachable, stranding it behind `llamastash logs`.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    app.ingest_status(&serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "stopped" },
      }]
    }));
    app.list_cursor = 2;
    assert_eq!(
      app.focused_managed().map(|m| m.state),
      Some(SurfaceState::Stopped),
      "cursor must land on the stopped launch row"
    );
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Requests, RightTab::Logs, RightTab::Settings],
      "stopped rows must expose the Logs and Requests tabs alongside Settings"
    );
  }

  #[test]
  fn ingest_status_snaps_cursor_to_newly_appeared_launch() {
    // A new launch_id arriving in a status tick should pull the
    // cursor onto its Running row so the user sees the model they
    // just launched selected — matches the kdash-style "latest
    // run goes to top + becomes selection" behaviour the user
    // confirmed during planning.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    let body = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "ready" },
      }]
    });
    app.ingest_status(&body);
    // Row 2 should be the Running qwen row.
    let rows = app.rendered_rows();
    let cursor_row = rows.get(app.list_cursor).expect("cursor lands in bounds");
    match cursor_row {
      ListRow::Model {
        launch_id: Some(id),
        ..
      } => assert_eq!(id, "L1", "cursor must land on the new launch"),
      other => panic!("cursor must land on a launch row, got {other:?}"),
    }
  }

  #[test]
  fn focused_managed_uses_launch_id_when_present_for_duplicate_launches() {
    // When two launches share a path, the Running rows must
    // disambiguate by launch_id — picking the wrong one would
    // route Logs/Chat/Settings to the other instance.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    app.managed = vec![
      ready_managed("/m/qwen.gguf", 41100, SurfaceState::Ready),
      ready_managed("/m/qwen.gguf", 41101, SurfaceState::Ready),
    ];
    // Row layout: [TableHeader, Header(▶ Running), Model(L-41100), Model(L-41101), Header(/m), Model(qwen catalog)]
    // The merge order in `ingest_status` is "new launches first"
    // but here we set `app.managed` directly, so the rows reflect
    // Vec order. The first managed row (L-41100) appears first.
    app.list_cursor = 2;
    let first = app.focused_managed().expect("focused managed at row 2");
    assert_eq!(first.launch_id, "L-41100");
    app.list_cursor = 3;
    let second = app.focused_managed().expect("focused managed at row 3");
    assert_eq!(second.launch_id, "L-41101");
  }

  #[test]
  fn save_preset_from_running_launch_carries_dispatched_extras_and_identity() {
    // Regression: Ctrl+P on a running model must carry the advanced `--`
    // tail (the live `status` `params.extras`) and the launch identity
    // (`status` `backend` / `params.server`) into the preset. The extras
    // half used to pass an empty list; the identity half used to be dropped
    // entirely, so a TUI-saved preset read back `server: null` and fell to
    // the daemon's default build.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    let body = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "ready" },
        "backend": "llamacpp",
        "params": {
          "model_path": "/m/qwen.gguf",
          "server": "llamacpp-vulkan",
          "extras": ["--override-kv", "tokenizer.ggml.add_bos=bool:false"],
        },
      }]
    });
    app.ingest_status(&body);
    let want = vec![
      "--override-kv".to_string(),
      "tokenizer.ggml.add_bos=bool:false".to_string(),
    ];
    // Plumbed from IPC into the managed row...
    let row = app
      .managed
      .iter()
      .find(|m| m.launch_id == "L1")
      .expect("managed row");
    assert_eq!(row.extras, want, "extras parsed from status params");
    assert_eq!(
      row.backend.as_deref(),
      Some("llamacpp"),
      "backend parsed from status"
    );
    assert_eq!(
      row.server.as_deref(),
      Some("llamacpp-vulkan"),
      "server parsed from status params"
    );
    // ...and captured by the save dialog when Ctrl+P fires on that row
    // (ingest_status snaps the cursor onto the newly appeared launch).
    app.open_save_preset_dialog();
    let dialog = app
      .save_preset_dialog
      .as_ref()
      .expect("save dialog opened on the running row");
    assert_eq!(
      dialog.extras, want,
      "advanced -- tail carried into the preset"
    );
    assert_eq!(
      dialog.backend.as_deref(),
      Some("llamacpp"),
      "resolved backend carried into the preset"
    );
    assert_eq!(
      dialog.server.as_deref(),
      Some("llamacpp-vulkan"),
      "resolved server build carried into the preset"
    );
  }

  #[test]
  fn save_preset_no_picker_pins_default_backend_when_unpredicted() {
    // Regression: the no-picker Ctrl+P path (no running launch, no picker
    // staged) must pin the explicit default backend when the row has no
    // prediction yet — mirroring `build_default_picker` — instead of falling
    // to `Auto`, which on replay lets the daemon's auto-routing pick a
    // different build than the llamacpp one actually shown.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/phi.gguf", "/m")];
    // A last-used launch with no server pick and no `backend` field on the
    // row, so `predicted_backend` is `None`.
    app.ingest_last_params(&serde_json::json!({
      "last_params": [{
        "model_path": "/m/phi.gguf",
        "params": { "ctx": 4096 },
      }],
    }));
    // Focus the catalog model row (no running launch, no picker).
    let cursor = app
      .rendered_rows()
      .iter()
      .position(|r| {
        r.path()
          .is_some_and(|p| p == std::path::Path::new("/m/phi.gguf"))
      })
      .expect("model row present");
    app.list_cursor = cursor;
    assert!(app.focused_managed().is_none(), "no running launch");
    assert!(app.launch_picker.is_none(), "no picker staged");

    app.open_save_preset_dialog();
    let dialog = app
      .save_preset_dialog
      .as_ref()
      .expect("save dialog opened on the catalog model");
    // No server pick, so the backend falls back to the model's explicit
    // choice — which, with no prediction yet, is the explicit default (not
    // `Auto`), so the preset saves the llamacpp build shown.
    assert_eq!(
      dialog.backend.as_deref(),
      Some("llamacpp"),
      "unpredicted row pins the default backend, not Auto"
    );
    assert_eq!(
      dialog.server.as_deref(),
      None,
      "no server pick means no server pin"
    );
  }

  #[test]
  fn right_pane_follows_cursor_no_sticky_fallback() {
    // Two models — one running (qwen), one not (phi). The list now
    // pins a `▶ Running` section at the top with a per-launch row,
    // and the running path drops out of its catalog group so it
    // never shows twice. Row layout:
    //   0: TableHeader
    //   1: Header(▶ Running)
    //   2: Model(qwen, launch_id) — Ready
    //   3: Header(/m)
    //   4: Model(phi) — NotLaunched
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m"), fake("/m/phi.gguf", "/m")];
    app.managed = vec![ready_managed("/m/qwen.gguf", 41100, SurfaceState::Ready)];
    app.list_cursor = 2;
    assert_eq!(
      app.available_right_tabs(),
      vec![
        RightTab::Requests,
        RightTab::Logs,
        RightTab::Settings,
        RightTab::Chat
      ],
      "Running-row selection exposes mode-appropriate tabs (Requests first)"
    );
    assert!(app.right_pane_focus().is_some());

    // phi has no managed launch → Settings-only.
    app.list_cursor = 4;
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Settings],
      "unlaunched selection collapses to Settings only"
    );
    assert!(
      app.right_pane_focus().is_none(),
      "no sticky fallback — right pane has no managed handle to draw"
    );

    // Running paths must not duplicate into Favorites / folder
    // groups — only the Running-section row should carry qwen.
    let rows = app.rendered_rows();
    let qwen_rows = rows
      .iter()
      .filter(|r| match r {
        ListRow::Model { path, .. } => path.ends_with("qwen.gguf"),
        _ => false,
      })
      .count();
    assert_eq!(
      qwen_rows, 1,
      "running qwen must appear only in the Running group, got {qwen_rows} rows"
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
      vec![RightTab::Requests, RightTab::Logs, RightTab::Settings]
    );
  }

  #[test]
  fn filter_keeps_matching_models_and_drops_empty_groups() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![
      fake("/m/x/qwen.gguf", "/m/x"),
      fake("/m/y/phi.gguf", "/m/y"),
    ];
    app.filter_input.set_text("qwen");
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
      vec!["m/x".to_string()],
      "empty groups must be dropped"
    );
  }

  #[test]
  fn cursor_move_clears_stale_launch_picker_when_path_changes() {
    // Round-8: the right pane is tied to the focused row. A
    // picker staged for model A must not survive a scroll to
    // model B — otherwise the Settings tab paints A's name + ctx
    // form while the user is looking at B.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/x/a.gguf", "/m/x"), fake("/m/y/b.gguf", "/m/y")];
    app.list_cursor = 2; // model a
    app.open_launch_picker();
    assert_eq!(app.launch_picker.as_ref().unwrap().model_name, "a");
    app.move_down();
    assert!(
      app.launch_picker.is_none(),
      "scrolling to a different model must clear the stale picker"
    );
  }

  #[test]
  fn ingest_status_collects_enabled_backend_servers() {
    let mut app = App::new(AppOptions::default());
    app.ingest_status(&json!({
      "daemon": {"pid": 1},
      "backends": [
        // No `enabled` field → counts as enabled (llamacpp).
        {"id": "llamacpp", "binary": "/usr/bin/llama-server", "server_labels": ["rocm", "vulkan"]},
        // Catalog not probed yet → the binary's file name stands in.
        {"id": "enginex", "enabled": true, "binary": "/usr/bin/enginex", "server_labels": []},
        // Disabled rows drop out even with servers.
        {"id": "future", "enabled": false, "binary": "/usr/bin/future", "server_labels": ["x"]},
        // No server and no binary → no entry.
        {"id": "ghost", "enabled": true},
      ],
    }));
    assert_eq!(
      app.daemon_info.backend_servers,
      vec![
        BackendServers {
          id: "llamacpp".into(),
          labels: vec!["rocm".into(), "vulkan".into()],
        },
        BackendServers {
          id: "enginex".into(),
          labels: vec!["enginex".into()],
        },
      ]
    );
  }

  #[test]
  fn ensure_right_tab_reachable_snaps_to_settings_for_unlaunched_focus() {
    // Round-8 fix: the fallback used to be hardcoded to Logs even
    // for unlaunched models — leaving `right_tab = Logs` while
    // `available_right_tabs()` returns `[Settings]`. Now the
    // fallback walks the reachable list and picks the first entry.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.list_cursor = 2;
    app.right_tab = RightTab::Chat;
    app.ensure_right_tab_reachable();
    assert_eq!(app.right_tab, RightTab::Settings);
  }

  #[test]
  fn ensure_right_tab_reachable_snaps_to_requests_on_a_managed_row() {
    // A launch that dies while the user sits on Chat loses that tab,
    // and the clamp falls back to the strip head — Requests for any
    // managed row, so the failed request stays on screen.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.managed = vec![ready_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    app.list_cursor = 2;
    app.right_tab = RightTab::Chat;
    app.managed = vec![ready_managed("/m/a.gguf", 41100, SurfaceState::Stopped)];
    app.ensure_right_tab_reachable();
    assert_eq!(app.right_tab, RightTab::Requests);
  }

  #[test]
  fn a_cursor_move_reopens_the_pane_on_the_new_rows_first_tab() {
    // Scrolling kept whatever tab the row before was on, so a launch
    // reached from the Settings tab kept painting its read-only launch
    // details instead of its requests.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m"), fake("/m/b.gguf", "/m")];
    app.managed = vec![ready_managed("/m/b.gguf", 41100, SurfaceState::Ready)];
    app.list_cursor = 2; // the Running group's b launch
    assert_eq!(app.focused_name().as_deref(), Some("b"));
    app.right_tab = RightTab::Chat;
    app.move_down();
    assert_eq!(app.focused_name().as_deref(), Some("a"));
    assert_eq!(
      app.right_tab,
      RightTab::Settings,
      "a model with no launch opens on Settings"
    );
    app.right_tab = RightTab::Chat;
    app.move_up();
    assert_eq!(app.focused_name().as_deref(), Some("b"));
    assert_eq!(
      app.right_tab,
      RightTab::Requests,
      "a launch opens on Requests, not on the tab the row before used"
    );
  }

  #[test]
  fn stepping_between_two_launches_of_one_model_reopens_the_pane() {
    // Two named launches of one GGUF are two rows on one path, so a
    // follow keyed on the path alone would leave the second one on
    // whatever tab the first was on.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    app.managed = vec![
      ready_managed("/m/qwen.gguf", 41100, SurfaceState::Ready),
      ready_managed("/m/qwen.gguf", 41101, SurfaceState::Ready),
    ];
    app.list_cursor = 2;
    assert_eq!(
      app.focused_managed().map(|m| m.launch_id.as_str()),
      Some("L-41100")
    );
    app.right_tab = RightTab::Chat;
    app.move_down();
    assert_eq!(
      app.focused_managed().map(|m| m.launch_id.as_str()),
      Some("L-41101")
    );
    assert_eq!(
      app.right_tab,
      RightTab::Requests,
      "the second launch opens on its own first tab"
    );
  }

  #[test]
  fn clamp_cursor_moves_a_header_hit_to_the_model_below_it() {
    // After a delete the row index can name the next group's header. That should
    // land on the next group's first model, not jump to the top of the list.
    let mut app = App::new(AppOptions::default());
    app.models = vec![
      fake("/a/one.gguf", "/a"),
      fake("/a/two.gguf", "/a"),
      fake("/b/three.gguf", "/b"),
    ];
    let header = app
      .rendered_rows()
      .iter()
      .rposition(|r| matches!(r, ListRow::Header { .. }))
      .unwrap();
    app.list_cursor = header;
    app.clamp_cursor();
    assert_eq!(app.focused_name().as_deref(), Some("three"));
  }

  #[test]
  fn the_opening_selection_of_a_launch_opens_on_requests() {
    // The cursor starts on the column-label row, so the first catalog
    // refresh clamps it onto the first launch. That move is a focus
    // change like any other, so the pane opens on the row's first tab
    // instead of the `Settings` the app was built with.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.managed = vec![ready_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    assert_eq!(app.right_tab, RightTab::Settings, "built default");
    app.clamp_cursor();
    assert_eq!(app.focused_name().as_deref(), Some("a"));
    assert_eq!(app.right_tab, RightTab::Requests);
  }

  #[test]
  fn a_launch_appearing_under_the_cursor_reopens_the_pane_on_requests() {
    // The catalog arrives before the first `status`, so the opening
    // clamp lands the cursor on an idle row. The launch that shows up
    // at that index a tick later never moves the cursor, and the pane
    // still has to follow it.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.list_cursor = 2;
    assert!(
      app.focused_managed().is_none(),
      "cursor starts on the idle row"
    );
    app.ingest_status(&serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/a.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "ready" },
      }]
    }));
    assert_eq!(
      app.focused_managed().map(|m| m.launch_id.as_str()),
      Some("L1"),
      "the launch landed on the row the cursor was already on"
    );
    assert_eq!(app.right_tab, RightTab::Requests);
  }

  #[test]
  fn a_status_tick_leaves_the_tab_the_user_picked_alone() {
    // The refresh runs several times a second, so following the focus
    // must mean a different launch under the cursor, not any change to
    // the one already there: a tick that re-snapped the pane would pull
    // the user off Chat every half second.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.managed = vec![ready_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    app.list_cursor = 2;
    app.right_tab = RightTab::Chat;
    // `ready_managed` mints `L-<port>`, and a different id here would
    // be a genuine launch change.
    let status = |state: &str| {
      serde_json::json!({
        "models": [{
          "launch_id": "L-41100",
          "id": { "path": "/m/a.gguf", "header_hash": "h" },
          "port": 41100,
          "state": { "state": state },
        }]
      })
    };
    app.ingest_status(&status("ready"));
    assert_eq!(
      app.right_tab,
      RightTab::Chat,
      "an unchanged tick keeps Chat"
    );
    app.ingest_status(&status("stopped"));
    assert_eq!(
      app.right_tab,
      RightTab::Chat,
      "a state change on the same launch is not a focus change"
    );
    // The render clamp, not the tick, is what moves him off a tab the
    // new state no longer offers.
    app.ensure_right_tab_reachable();
    assert_eq!(app.right_tab, RightTab::Requests);
  }

  #[test]
  fn rendered_rows_cache_returns_memoized_value_inside_frame() {
    // Tier-C hoist: `rendered_rows()` is memoized via `rows_cache`
    // for the duration of a single render frame. The primer must
    // populate the cache, and subsequent calls must return the
    // memoized rows even if a state mutation lands between the
    // primer and the consumer (which is exactly what happens
    // during a frame — the cache holds the rows the *frame*
    // committed to, not a stale half-build).
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/x/a.gguf", "/m/x")];
    let baseline = app.rendered_rows();
    app.prime_rows_cache();
    // Mutate the underlying model set; cache MUST hide the change
    // until cleared.
    app.models.push(fake("/m/y/b.gguf", "/m/y"));
    let cached = app.rendered_rows();
    assert_eq!(
      cached.len(),
      baseline.len(),
      "cache must hide mid-frame mutations"
    );
    app.clear_rows_cache();
    let fresh = app.rendered_rows();
    assert!(
      fresh.len() > baseline.len(),
      "post-clear render reflects the mutation"
    );
  }

  #[test]
  fn snap_cursor_to_launch_clears_stale_launch_picker() {
    // F1 #3: `snap_cursor_to_launch` is the only `list_cursor`
    // writer that used to skip `sync_pane_to_focus`. When a
    // status snapshot lands during launch and snaps to a different
    // path than the picker was staged for, Settings would render
    // ports/name for the *previous* path. Snap must now clear the
    // stale picker just like `move_up` / `move_down` do.
    //
    // With managed = [b], the row layout is:
    //   0: TableHeader
    //   1: Header(▶ Running)
    //   2: Model b (launch L-42100)
    //   3: Header(/m/x)
    //   4: Model a (catalog)
    // (empty /m/y group is pruned by `build_rows`)
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/x/a.gguf", "/m/x"), fake("/m/y/b.gguf", "/m/y")];
    app.managed = vec![ready_managed("/m/y/b.gguf", 42100, SurfaceState::Launching)];
    app.list_cursor = 4; // model a (catalog row)
    app.open_launch_picker();
    assert_eq!(app.launch_picker.as_ref().unwrap().model_name, "a");
    app.snap_cursor_to_launch("L-42100");
    assert_eq!(app.list_cursor, 2, "snap must move cursor to launch row");
    assert!(
      app.launch_picker.is_none(),
      "snap_cursor_to_launch must clear a picker staged for the prior path"
    );
  }

  #[test]
  fn toast_expires_after_ttl() {
    let mut app = App::new(AppOptions::default());
    app.show_toast("yanked");
    assert!(app.toast_message().is_some());
    // Backdate the toast to force expiry.
    if let Some((_, ref mut at, _)) = app.toast {
      *at = Instant::now() - Duration::from_secs(10);
    }
    app.expire_toast();
    assert!(app.toast_message().is_none());
  }
}
