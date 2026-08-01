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
use crate::tui::tabs::{tabs_for_mode, RightTab};

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

/// How many entries the `↺ Recent` section surfaces. Five matches
/// what the user picked during planning; the daemon's storage
/// itself isn't capped — the cap is purely a render-side window.
const RECENT_LIST_CAP: usize = 5;

/// In-memory snapshot of one launched model the daemon is
/// supervising. Mirrors the IPC `status` shape — kept in App so
/// the right-pane header can show port/state without re-querying.
#[derive(Debug, Clone, Default)]
pub struct ManagedRow {
  pub launch_id: String,
  pub path: PathBuf,
  pub port: u16,
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
  pub knobs: crate::config::TypedKnobs,
  /// The advanced `--` argv tail this launch was dispatched with (the
  /// live `status` `params.extras`). Empty for external rows. Lets
  /// `Ctrl+P` save-from-running carry the advanced args into the preset.
  pub extras: Vec<String>,
  /// Native (per-backend) knobs the launch dispatched with (the live
  /// `status` `params.backend_knobs`) — the six ds4 tunables, so `Ctrl+P`
  /// save-from-running captures them into the preset (not just typed knobs).
  /// Empty for llama.cpp / Lemonade launches.
  pub backend_knobs: std::collections::BTreeMap<String, crate::config::KnobValue<String>>,
  /// Backend this launch actually resolved to (`status` `backend`): `ds4`
  /// when the launch dispatched to ds4, else `llamacpp` / `lemonade`. Keyed
  /// on by the ds4 badge / knob panel so a running row reflects the real
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
  pub knobs: crate::config::TypedKnobs,
  /// Per-backend native-knob deltas (see [`crate::launch::native_knobs`]),
  /// keyed by descriptor id. Populated for ds4; empty for llama.cpp / Lemonade.
  /// Seeds the picker's `backend_knobs` so a returning user keeps their values.
  pub backend_knobs: std::collections::BTreeMap<String, crate::config::KnobValue<String>>,
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
  /// MTP intent from the last successful launch (`auto`/`on`/`off`). Seeds the
  /// picker's MTP row so a returning user keeps their choice.
  pub mtp: crate::launch::params::MtpEnable,
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
  /// One entry per *enabled* backend whose `status.backends` row
  /// carries a resolved `binary` path. The Daemon panel's server row
  /// renders every non-llamacpp entry alongside the default
  /// `llama-server` (llamacpp's binary already rides `server_path`),
  /// tagged with the backend id — new backends appear with no
  /// per-backend TUI code.
  pub backend_binaries: Vec<BackendBinary>,
}

/// One backend's resolved binary from the `status.backends` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendBinary {
  pub id: String,
  pub binary: String,
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
  /// canonical path (`llamacpp` / `lemonade` / `ds4`). The daemon already
  /// applied the ds4 availability + quant-contract predicate and the source
  /// mapping, so this is the single source of truth for "where would a plain
  /// launch of this model route" — it drives the launch picker's
  /// `model_backend`, the right-pane ds4 badge, and the Models-list Backend
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
  /// backend fail-fast precheck: missing `llama-server`, missing
  /// `lemond`, umbrella port held by a foreign process). One failure
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
  /// `Ctrl+P` save-preset dialog. `Some(_)` while the modal is open; the
  /// input pump routes keys to its name / overwrite stages.
  pub save_preset_dialog: Option<crate::tui::save_preset_dialog::SavePresetDialog>,
  /// Pinned download status strip. Always present; the
  /// renderer reserves a 1-line slot above the body only when
  /// `download_strip.is_active()` is true.
  pub download_strip: crate::tui::download_strip::DownloadStripState,
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
/// top-level fields. Boxed in both enums so the large `TypedKnobs`
/// payload doesn't bloat every other (tiny) variant — the imbalance
/// `clippy::large_enum_variant` flags.
#[derive(Debug, Clone)]
pub struct StartModelArgs {
  pub model_path: PathBuf,
  pub ctx: Option<u32>,
  pub reasoning: Option<bool>,
  pub knobs: crate::config::TypedKnobs,
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
  /// Per-backend native-knob values from the picker (see
  /// [`crate::launch::native_knobs`]). Populated for ds4; empty for llama.cpp.
  pub backend_knobs: std::collections::BTreeMap<String, crate::config::KnobValue<String>>,
  /// Chosen server id (a build/binary of a backend) from the picker's Server
  /// row, or `None` for the priority default. Sent as `LaunchParams.server`;
  /// the daemon derives the binary (and, when `backend` is `Auto`, the backend)
  /// from it.
  pub server: Option<String>,
  /// MTP speculative-decoding intent from the picker's `mtp` cycle row
  /// (`auto`/`on`/`off`). Backend-agnostic — sent as the `mtp` start param.
  pub mtp: crate::launch::params::MtpEnable,
}

/// The binary + compute-backend label a focused running model launched on
/// when it uses a **non-default** server build. Returned by
/// [`App::focused_override_device`] so the daemon-pane `server` row can swap
/// to the override build. Owned (not a borrow into `servers`) so the render
/// path can hold it across the immutable-borrow boundary.
#[derive(Debug, Clone)]
pub struct OverrideServer {
  pub binary: PathBuf,
  pub backend: String,
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
  /// `Ctrl+D:delete` — remove a non-running model from disk.
  /// `path` is the GGUF (or split-shard launch file); the deleter
  /// walks the HF snapshot dir up to the cache root when the file
  /// is symlinked into `~/.cache/huggingface`, so a confirmed
  /// delete reclaims the blob bytes too.
  DeleteModel { path: PathBuf, display_name: String },
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
      list_cursor: 0,
      filter_input: crate::tui::input_field::InputField::new(),
      launch_picker: None,
      running_view_scroll: Cell::new(0),
      toast: None,
      daemon_connected: false,
      daemon_start_error: None,
      daemon_info: DaemonInfo::default(),
      host_metrics: HostMetricsSnapshot::default(),
      servers: Vec::new(),
      should_exit: false,
      show_help: false,
      help_scroll: 0,
      pending_g_prefix: false,
      confirm_dialog: None,
      hf_dialog: None,
      save_preset_dialog: None,
      download_strip: crate::tui::download_strip::DownloadStripState::default(),
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
      let ctx = self.hf_hardware_fit_ctx();
      let offline = self.options.offline || crate::init::fetch::offline_requested(false);
      self.hf_dialog = Some(crate::tui::hf_dialog::HfDialogState::open(offline, ctx));
    }
    self.focus = Focus::HfDialog;
  }

  /// Snapshot the inputs `vram_fit_for_file` needs into the dialog
  /// state at open time. Backend / VRAM / RAM come from the
  /// daemon's host-metrics sampler; the per-backend overhead band
  /// is read from the bundled benchmark snapshot. R111 + R113.
  fn hf_hardware_fit_ctx(&self) -> crate::tui::hf_dialog::HardwareFitContext {
    use crate::tui::hf_dialog::HardwareFitContext;
    let backend = recommender_backend_key(&self.host_metrics.gpu_backend);
    let vram_bytes = self.host_metrics.gpu_mem_total_bytes;
    let ram_total_bytes = self.host_metrics.ram_total_bytes;
    let overhead_band_bytes = crate::init::benchmark::load_bundled()
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
        // source for the picker, the ds4 badge, and the Backend column).
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

  /// The daemon's predicted backend for `path` (`llamacpp` / `lemonade` /
  /// `ds4`), or `None` when the model isn't in the current catalog.
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
    } else {
      self.managed.clear();
    }
    if let Some(arr) = body.get("external").and_then(Value::as_array) {
      self.external = arr.iter().filter_map(parse_external_row).collect();
    } else {
      self.external.clear();
    }
    if let Some(daemon) = body.get("daemon") {
      // Preserve the proxy snapshot — it lives in a sibling field
      // (`body["proxy"]`) but on the same daemon-info struct so the
      // info_pane can render both in one block.
      let proxy = body.get("proxy").and_then(parse_proxy_info);
      // The backends rows (also a sibling field): every enabled
      // backend's resolved binary, for the server row. A row with no
      // `enabled` field counts as enabled (llamacpp has no off switch);
      // an explicit `enabled: false` (e.g. lemonade without
      // `--lemonade`) is dropped.
      let backend_binaries: Vec<BackendBinary> = body
        .get("backends")
        .and_then(Value::as_array)
        .map(|rows| {
          rows
            .iter()
            .filter(|r| r.get("enabled").and_then(Value::as_bool).unwrap_or(true))
            .filter_map(|r| {
              Some(BackendBinary {
                id: r.get("id").and_then(Value::as_str)?.to_string(),
                binary: r.get("binary").and_then(Value::as_str)?.to_string(),
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
        backend_binaries,
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
        let knobs = params
          .get("knobs")
          .and_then(|v| serde_json::from_value(v.clone()).ok())
          .unwrap_or_default();
        // Native knobs (omitted when empty by the daemon) — seed the picker
        // so a saved native value is reapplied next launch.
        let backend_knobs = params
          .get("backend_knobs")
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
        let mtp = params
          .get("mtp")
          .and_then(Value::as_str)
          .and_then(crate::launch::params::MtpEnable::from_token)
          .unwrap_or_default();
        if recent.len() < RECENT_LIST_CAP {
          recent.push(path.clone());
        }
        next.insert(
          path,
          LastParamsRow {
            ctx,
            reasoning,
            knobs,
            backend_knobs,
            extras,
            port,
            server,
            mtp,
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
        // `--backend llamacpp` override on a ds4-compatible file).
        backend: m.backend.clone(),
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
  /// Routes through `sync_picker_to_focus` so any picker staged
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
      let before = self.focused_path();
      self.list_cursor = idx;
      self.sync_picker_to_focus(before);
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
    let before = self.focused_path();
    self.move_down_in(&rows);
    self.sync_picker_to_focus(before);
  }

  pub fn move_up(&mut self) {
    let rows = self.rendered_rows();
    let before = self.focused_path();
    self.move_up_in(&rows);
    self.sync_picker_to_focus(before);
  }

  /// Clear `launch_picker` when the cursor moved onto a different
  /// path. Round-8: the right pane follows the cursor with no
  /// sticky fallback — letting a picker staged for model A linger
  /// while the user scrolls to model B would render the wrong
  /// model name in the Settings tab. Caller passes the focused
  /// path *before* the move so we can compare against the path
  /// *after*.
  fn sync_picker_to_focus(&mut self, before: Option<PathBuf>) {
    let after = self.focused_path();
    if before != after {
      // Scroll offset is per-focused-model; clear it on cursor moves
      // so the new model's running-launch view opens at the top.
      self.running_view_scroll.set(0);
    }
    if self.launch_picker.is_none() {
      return;
    }
    if before != after {
      self.launch_picker = None;
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
    let before = self.focused_path();
    if delta >= 0 {
      for _ in 0..delta {
        self.move_down_in(&rows);
      }
    } else {
      for _ in 0..-delta {
        self.move_up_in(&rows);
      }
    }
    self.sync_picker_to_focus(before);
  }

  pub fn go_top(&mut self) {
    let rows = self.rendered_rows();
    let before = self.focused_path();
    for (i, r) in rows.iter().enumerate() {
      if r.is_selectable() {
        self.list_cursor = i;
        break;
      }
    }
    self.sync_picker_to_focus(before);
  }

  pub fn go_bottom(&mut self) {
    let rows = self.rendered_rows();
    let before = self.focused_path();
    for (i, r) in rows.iter().enumerate().rev() {
      if r.is_selectable() {
        self.list_cursor = i;
        break;
      }
    }
    self.sync_picker_to_focus(before);
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
  /// transiently misses while a Lemonade umbrella is mid-load, and this keeps
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

  /// The launch-device catalog entry for the focused running model,
  /// but only when that model's binary differs from the daemon's
  /// default `server_path`. The Daemon panel's `server` row uses this
  /// to swap in (and highlight) the binary the hovered launch is
  /// actually running on, so in multi-binary setups the operator can
  /// see which backend each model uses as the cursor moves.
  ///
  /// Returns `None` when the cursor isn't on a running model, when the
  /// row's `--device` selector has no catalog entry (stale/un-probed),
  /// or when the resolved binary is the default — in every one of
  /// those cases the row falls back to the plain default-path render.
  /// Whether the host exposes more than one selectable GPU device.
  /// Drives the multi-GPU UI gates: the launch picker's `device` row
  /// and the model list's `Device` column only appear when `true`, so
  /// single-GPU / CPU-only users aren't shown a control that can never
  /// carry a choice.
  /// Gates the Models-list Device column and the picker's multi-GPU knob
  /// group. True when **some single server** offers more than one device — a
  /// launch targets one server's devices, so cross-build selector variety (the
  /// same physical GPU seen as `ROCm0` by one build and `Vulkan0` by another)
  /// is a *server* choice, not a device one, and must not trip this. A
  /// single-GPU host with several llama.cpp builds stays single-device here.
  pub fn multi_device(&self) -> bool {
    self.servers.iter().any(|s| s.devices.len() > 1)
  }

  /// Binary + compute-backend label for the focused running model's device
  /// override, when it runs on a **non-default** server binary. Resolves the
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
    let device = server.devices.iter().find(|d| d.selector == selector)?;
    let is_default = self
      .daemon_info
      .server_path
      .as_deref()
      .is_some_and(|def| server.binary == Path::new(def));
    if is_default {
      None
    } else {
      Some(OverrideServer {
        binary: server.binary.clone(),
        backend: device.gpu_backend.clone(),
      })
    }
  }

  /// Servers compatible with `path`'s model, priority-ordered: for each backend
  /// in the model's `supported_backends` (priority order), every catalog server
  /// of that backend (in catalog order). Falls back to the whole catalog when
  /// the model declares no supported backends (older daemon / no metadata).
  /// Feeds the launch picker's Server row.
  pub(crate) fn compatible_servers(&self, path: &Path) -> Vec<crate::backend::Server> {
    let supported: Vec<String> = self
      .models
      .iter()
      .find(|m| m.path == path)
      .map(|m| m.supported_backends.clone())
      .unwrap_or_default();
    if supported.is_empty() {
      return self.servers.clone();
    }
    let mut out = Vec::new();
    for backend_id in &supported {
      for s in self.servers.iter().filter(|s| &s.backend_id == backend_id) {
        out.push(s.clone());
      }
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
        // A returning user keeps their last MTP choice (auto/on/off).
        state.mtp = last.mtp;
        // returning user inherits the typed-knob deltas they
        // last shipped. The daemon persists only user-supplied
        // deltas (not the fully resolved set) so seeding straight
        // into `user_knobs` keeps the picker's source labels
        // honest — every persisted knob shows `(user)`, the rest
        // re-resolve from yaml / built-in / model default.
        //
        // `ctx` and `reasoning` are now part of `TypedKnobs`, so
        // they ride along inside `last.knobs` without dedicated
        // seeding paths.
        state.user_knobs = last.knobs.clone();
        // Native knobs ride the same "remember the user's last deltas" path —
        // a saved native value is reapplied next launch unless overridden.
        state.backend_knobs = last.backend_knobs.clone();
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
    // Surface the model's backend native knobs (its own set, or none for a
    // backend with no native-knob channel). Ordered after the server seed so a
    // remembered cross-backend pick resolves the right descriptor set.
    state.seed_native_descriptors();
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
        backend_knobs: np.params.backend_knobs.clone(),
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
      .unwrap_or_else(|| crate::util::paths::path_basename(path));
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
  ///   focus the right pane in its read-only running view. *No*
  ///   picker is staged — pressing `Enter` alone shouldn't silently
  ///   spin up a duplicate launch before the user has any chance to
  ///   inspect or edit the params. The bottom-strip chip leads with
  ///   `e:edit for launch`; the user goes `e → edit fields → Enter`
  ///   to launch a new instance.
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
      self.right_tab = RightTab::Settings;
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
      .unwrap_or_else(|| crate::util::paths::path_basename(&path));

    // Capture knobs + native knobs + extras from whichever surface is in view.
    let (knobs, backend_knobs, extras) = if let Some(m) = self.focused_managed() {
      // Running model: the live dispatched knobs, native (ds4) knobs, and
      // advanced `--` tail — so a ds4 launch's `--power` / `--ssd-streaming`
      // land in the preset, not just the typed knobs.
      (m.knobs.clone(), m.backend_knobs.clone(), m.extras.clone())
    } else if let Some(p) = &self.launch_picker {
      (
        p.user_knobs.clone(),
        p.backend_knobs.clone(),
        p.extras
          .iter()
          .map(|s| s.to_string_lossy().into_owned())
          .collect(),
      )
    } else if let Some(p) = self.build_default_picker() {
      (
        p.user_knobs.clone(),
        p.backend_knobs.clone(),
        p.extras
          .iter()
          .map(|s| s.to_string_lossy().into_owned())
          .collect(),
      )
    } else {
      return;
    };

    let (existing, arch_shadow) = self.existing_preset_names(&path);
    self.save_preset_dialog = Some(crate::tui::save_preset_dialog::SavePresetDialog::open(
      path,
      model_name,
      knobs,
      backend_knobs,
      extras,
      existing,
      arch_shadow,
    ));
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
  /// model. The rule is binary: a *running* selection (Launching /
  /// Loading / Ready) gets Logs + the mode-appropriate input tab +
  /// Settings; an unlaunched / stopped / errored / unfocused
  /// selection gets only Settings. There's no sticky fallback —
  /// what the user sees in the right pane is the model under the
  /// cursor, nothing else.
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
    // A managed-multiplexer umbrella row is infrastructure, not a model: it has
    // no launch params to edit and chatting with the bare umbrella is
    // meaningless. Its log tail is the one useful surface.
    if crate::backend::is_infra_launch(&crate::daemon::registry::LaunchId(
      managed.launch_id.clone(),
    )) {
      return vec![RightTab::Logs];
    }
    // A delegated multiplexer model (resident in the umbrella): the umbrella
    // honors no launch knobs, so Settings is dropped; the mode surface
    // (Chat / Embed / Rerank) stays — requests ride the umbrella's
    // OpenAI-compat port directly — and Logs tails the shared umbrella log. A
    // model with no mode surface (hint `Unknown`) gets Logs alone. Keys on the
    // resolved `backend`'s lifecycle (the umbrella row is already dropped at
    // ingest), since the launch id is a plain `L#` shared with every backend.
    if managed
      .backend
      .as_deref()
      .is_some_and(crate::backend::is_managed_multiplexer)
    {
      if managed.state != SurfaceState::Ready {
        return vec![RightTab::Logs];
      }
      return tabs_for_mode(self.mode_hint_for(&managed.path))
        .into_iter()
        .filter(|t| *t != RightTab::Settings)
        .collect();
    }
    match managed.state {
      SurfaceState::Ready => tabs_for_mode(self.mode_hint_for(&managed.path)),
      // Process alive but not yet serving — Settings stays the
      // canonical first stop so the user can still tweak relaunch
      // params, Logs sits next so the startup pipeline is one
      // Tab away.
      SurfaceState::Launching | SurfaceState::Loading => {
        vec![RightTab::Settings, RightTab::Logs]
      }
      // Error: surface Logs alongside Settings so the user can
      // read the failure tail without re-launching. The daemon
      // keeps the per-launch log buffer around after the spawn
      // fails, and the poller still hits it because the entry
      // remains in `state.running` until the user clears it.
      SurfaceState::Error => vec![RightTab::Settings, RightTab::Logs],
      _ => vec![RightTab::Settings],
    }
  }

  /// Advance the right-pane tab. Skips tabs that aren't reachable
  /// for the current focus (e.g. Chat when the model isn't Ready).
  pub fn cycle_right_tab(&mut self) {
    let tabs = self.available_right_tabs();
    let Some(first) = tabs.first().copied() else {
      self.right_tab = RightTab::Settings;
      return;
    };
    let pos = tabs.iter().position(|t| *t == self.right_tab).unwrap_or(0);
    let next = (pos + 1) % tabs.len();
    self.right_tab = tabs.get(next).copied().unwrap_or(first);
  }

  /// Cycle to the previous right-pane tab — used by `Left` arrow
  /// alongside [`Self::cycle_right_tab`] (`Right` arrow / Tab).
  pub fn cycle_right_tab_prev(&mut self) {
    let tabs = self.available_right_tabs();
    let Some(first) = tabs.first().copied() else {
      self.right_tab = RightTab::Settings;
      return;
    };
    let pos = tabs.iter().position(|t| *t == self.right_tab).unwrap_or(0);
    let prev = (pos + tabs.len() - 1) % tabs.len();
    self.right_tab = tabs.get(prev).copied().unwrap_or(first);
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
    let tabs = self.available_right_tabs();
    if !tabs.contains(&self.right_tab) {
      self.right_tab = tabs.first().copied().unwrap_or(RightTab::Settings);
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
    native_ctx: cr.native_ctx,
    chat_template: None,
    tokenizer_kind: cr.tokenizer_kind.clone(),
    reasoning_hint: cr.has_reasoning_hint,
    mode_hint: parse_mode_hint(cr.mode_hint.as_deref()),
    weights_bytes: cr.weights_bytes,
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
    // Priority-ordered backends this model can run on (`ds4`, `llamacpp`, …).
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
    device: None,
    rss_bytes: None,
    cpu_pct: None,
    resolved_ctx: None,
    ctx_clamped: false,
    knobs: crate::config::TypedKnobs::default(),
    extras: Vec::new(),
    backend_knobs: Default::default(),
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
  // A managed-multiplexer umbrella is the multiplexer *process*, not a model:
  // the daemon tracks it as a managed launch, but its path is the umbrella
  // binary, so it would render as a bogus running row and yank an invalid model
  // id. Drop it from the TUI — delegated model launches still surface as their
  // own rows. (The daemon `status` / CLI keep it; this is TUI-only.)
  if crate::backend::is_infra_launch(&crate::daemon::registry::LaunchId(launch_id.clone())) {
    return None;
  }
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
    .and_then(|k| serde_json::from_value::<crate::config::TypedKnobs>(k.clone()).ok())
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
  // Native (ds4) knobs, so `Ctrl+P` save-from-running and the ds4 knob panel
  // reflect what the launch dispatched with. Missing / shape mismatch → empty.
  let backend_knobs = row
    .get("params")
    .and_then(|p| p.get("backend_knobs"))
    .and_then(|k| serde_json::from_value(k.clone()).ok())
    .unwrap_or_default();
  // The backend this launch actually resolved to (honest ds4 signal).
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
    path,
    port,
    state,
    device,
    rss_bytes,
    cpu_pct,
    resolved_ctx,
    ctx_clamped,
    knobs,
    extras,
    backend_knobs,
    backend,
    server,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::{Device, Server};
  use crate::config::KnobValue;
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
        native_ctx: Some(8192),
        chat_template: None,
        tokenizer_kind: None,
        reasoning_hint: false,
        mode_hint: ModeHint::Chat,
        weights_bytes: Some(4_200_000_000),
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
  fn parse_list_models_row_maps_lemonade_source() {
    // Regression: the parser's source match had no "lemonade" arm, so
    // registry rows fell through to `UserPath`. Downstream that seeded
    // the launch picker with `model_backend = LlamaCpp` — the full
    // llama.cpp knob set rendered and the Backend chooser hid, even
    // though the daemon routes the launch to Lemonade.
    let row = json!({
      "path": "lemonade://qwen3.5-4b-FLM",
      "parent": "lemonade://",
      "source": "lemonade",
      "display_label": "qwen3.5-4b-FLM",
    });
    let parsed = parse_list_models_row(&row).expect("row parses");
    assert_eq!(parsed.source, ModelSource::Lemonade);
    assert_eq!(parsed.source.backend_id(), "lemonade");
  }

  #[test]
  fn build_default_picker_seeds_lemonade_backend_from_focused_row() {
    // Live-flow companion to the picker's own
    // `lemonade_model_shows_only_backend_ctx_and_extras`: that test sets
    // `model_backend` by hand; this one walks the real seeding path from
    // a catalog row, which is where the missing source arm broke it.
    use crate::tui::launch_picker::PickerField;
    let mut app = App::new(AppOptions::default());
    let mut row = fake("lemonade://qwen3.5-4b-FLM", "lemonade://");
    row.source = ModelSource::Lemonade;
    row.display_label = Some("qwen3.5-4b-FLM".into());
    app.models = vec![row];
    // The picker now reads the daemon's per-row prediction (as `ingest_list_models`
    // records it), not a TUI-side source re-derivation.
    app.backend_by_path.insert(
      PathBuf::from("lemonade://qwen3.5-4b-FLM"),
      "lemonade".into(),
    );
    app.list_cursor = 2;
    assert!(app.focused_path().is_some(), "cursor must sit on the row");
    let picker = app.build_default_picker().expect("picker builds");
    assert_eq!(
      picker.model_backend,
      crate::launch::params::BackendChoice::Explicit("lemonade".into())
    );
    let visible: Vec<PickerField> = PickerField::all()
      .iter()
      .copied()
      .filter(|f| picker.field_visible(*f))
      .collect();
    assert!(
      visible.len() == 3,
      "lemonade picker is preset + ctx + extras only, got {visible:?}"
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
  fn multi_backend_and_is_ds4_path_read_the_daemon_prediction() {
    let mut app = App::new(AppOptions::default());
    app
      .backend_by_path
      .insert(PathBuf::from("/m/a.gguf"), "llamacpp".into());
    assert!(!app.multi_backend(), "all-llamacpp is single-backend");
    app
      .backend_by_path
      .insert(PathBuf::from("/m/b.gguf"), "ds4".into());
    assert!(app.multi_backend(), "a non-default backend row flips it on");
    // The per-path backend prediction comes straight off the daemon's badge.
    assert_eq!(
      app.predicted_backend(&PathBuf::from("/m/b.gguf")),
      Some("ds4")
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
  fn ingest_status_drops_the_lemonade_umbrella_row() {
    // The umbrella process is not a model — it must not appear in the running
    // list. A specific `lemonade://<id>` launch alongside it stays.
    let mut app = App::new(AppOptions::default());
    let umbrella = crate::backend::lemonade::umbrella_launch_id()
      .as_str()
      .to_string();
    let body = json!({
      "models": [
        {
          "launch_id": umbrella,
          "id": {"path": "/usr/bin/lemond", "header_blake3": "00".repeat(32)},
          "port": 13305,
          "state": {"state": "ready"},
          "backend": "lemonade",
        },
        {
          "launch_id": "L1",
          "id": {"path": "lemonade://Llama-3.1-8B", "header_blake3": "00".repeat(32)},
          "port": 13305,
          "state": {"state": "ready"},
          "backend": "lemonade",
        }
      ],
      "gpu": {"backend": "cpu_only"}
    });
    app.ingest_status(&body);
    let ids: Vec<&str> = app.managed.iter().map(|m| m.launch_id.as_str()).collect();
    assert_eq!(ids, vec!["L1"], "umbrella row must be hidden");
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
    // picker — it just shows the read-only running view. The
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
      "drill still focuses the right pane (read-only running view)"
    );
    assert_eq!(app.right_tab, RightTab::Settings);
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
    assert_eq!(dev.binary, PathBuf::from("/opt/cuda/llama-server"));
    assert_eq!(dev.backend, "CUDA");
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
  fn compatible_servers_scopes_to_the_models_supported_backends() {
    // A Lemonade registry model (supported_backends = [lemonade]) must offer
    // only Lemonade's server in the picker, not the whole host catalog — the
    // bug where every server showed but the launch went to Lemonade anyway.
    let mut app = App::new(AppOptions::default());
    let mut m = fake("/m/qwen.gguf", "/m");
    m.supported_backends = vec!["lemonade".into()];
    app.models = vec![m];
    app.servers = vec![
      srv("/usr/bin/llama-server", vec![dev("CUDA0", "CUDA")]),
      Server {
        id: "lemonade".into(),
        backend_id: "lemonade".into(),
        binary: PathBuf::from("/usr/bin/lemond"),
        name: "lemonade".into(),
        devices: Vec::new(),
      },
    ];
    let servers = app.compatible_servers(Path::new("/m/qwen.gguf"));
    assert_eq!(servers.len(), 1, "scoped to the one lemonade server");
    assert_eq!(servers[0].backend_id, "lemonade");
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

  #[test]
  fn open_launch_picker_prefills_from_persisted_last_params() {
    // Item 6: a returning user lands on the same ctx / reasoning /
    // advanced argv they shipped on the previous launch. The daemon
    // exposes the snapshot via `last_params_list`; the App ingests it
    // into `self.last_params`; the picker seeds from it.
    let mut app = App::new(AppOptions::default());
    let path = PathBuf::from("/m/a.gguf");
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.list_cursor = 2;
    app.last_params.insert(
      path.clone(),
      LastParamsRow {
        ctx: Some(16384),
        reasoning: true,
        knobs: crate::config::TypedKnobs {
          ctx: Some(KnobValue::Set(16384)),
          reasoning: Some(KnobValue::Set(true)),
          ..Default::default()
        },
        backend_knobs: Default::default(),
        extras: vec!["--rope-freq-base".into(), "10000".into()],
        port: Some(41105),
        server: None,
        mtp: crate::launch::params::MtpEnable::default(),
      },
    );
    app.open_launch_picker();
    let picker = app.launch_picker.as_ref().expect("picker state");
    assert_eq!(
      picker.user_knobs.ctx,
      Some(KnobValue::Set(16384)),
      "ctx must seed from last_params via user_knobs"
    );
    assert_eq!(
      picker.user_knobs.reasoning,
      Some(KnobValue::Set(true)),
      "reasoning must seed from last_params via user_knobs"
    );
    assert_eq!(picker.prefer_port, Some(41105), "port must seed too");
    let extras: Vec<String> = picker
      .extras
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    assert_eq!(extras, vec!["--rope-freq-base", "10000"]);
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
  fn save_preset_from_running_launch_carries_dispatched_extras() {
    // Regression: Ctrl+P on a running model must carry the advanced `--`
    // tail (the live `status` `params.extras`) into the preset. It used to
    // pass an empty list, dropping the advanced args off a running launch.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/qwen.gguf", "/m")];
    let body = serde_json::json!({
      "models": [{
        "launch_id": "L1",
        "id": { "path": "/m/qwen.gguf", "header_hash": "h" },
        "port": 41100,
        "state": { "state": "ready" },
        "params": {
          "model_path": "/m/qwen.gguf",
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
      vec![RightTab::Settings, RightTab::Logs, RightTab::Chat],
      "Running-row selection exposes mode-appropriate tabs (Settings first)"
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
      vec![RightTab::Settings, RightTab::Logs]
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
  fn umbrella_row_offers_only_logs_tab() {
    // The lemonade umbrella is infrastructure: no launch params to
    // edit, chat against the bare umbrella is meaningless — only its
    // log tail is useful.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake("/m/a.gguf", "/m")];
    app.managed = vec![ManagedRow {
      launch_id: "lemonade-umbrella".into(),
      path: PathBuf::from("/usr/bin/lemond"),
      port: 13305,
      state: SurfaceState::Ready,
      device: None,
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }];
    // Rows: [TableHeader, Header(▶ Running), umbrella, …] — cursor on it.
    app.list_cursor = 2;
    assert_eq!(app.available_right_tabs(), vec![RightTab::Logs]);
    // A Ready tab choice from a previous focus must snap back to Logs.
    app.right_tab = RightTab::Chat;
    app.ensure_right_tab_reachable();
    assert_eq!(app.right_tab, RightTab::Logs);
  }

  #[test]
  fn delegated_lemonade_row_drops_settings_and_follows_mode() {
    // A resident lemonade model: no Settings (the umbrella honors no
    // launch knobs); the mode surface + Logs remain. A model with no
    // llamastash mode surface (Whisper → Unknown) gets Logs alone.
    let mut app = App::new(AppOptions::default());
    let mut chat_model = fake("lemonade://qwen-FLM", "lemonade://");
    chat_model.display_label = Some("qwen-FLM".into());
    let mut whisper = fake("lemonade://Whisper-Tiny", "lemonade://");
    whisper.metadata.as_mut().unwrap().mode_hint = ModeHint::Unknown;
    whisper.display_label = Some("Whisper-Tiny".into());
    app.models = vec![chat_model, whisper];
    app.managed = vec![
      ManagedRow {
        launch_id: "L1".into(),
        path: PathBuf::from("lemonade://qwen-FLM"),
        port: 13305,
        state: SurfaceState::Ready,
        device: None,
        rss_bytes: None,
        cpu_pct: None,
        backend: Some(crate::backend::lemonade::LEMONADE_BACKEND_ID.into()),
        ..Default::default()
      },
      ManagedRow {
        launch_id: "L2".into(),
        path: PathBuf::from("lemonade://Whisper-Tiny"),
        port: 13305,
        state: SurfaceState::Ready,
        device: None,
        rss_bytes: None,
        cpu_pct: None,
        backend: Some(crate::backend::lemonade::LEMONADE_BACKEND_ID.into()),
        ..Default::default()
      },
    ];
    // Rows: [TableHeader, Header(▶ Running), qwen, whisper, …].
    app.list_cursor = 2;
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Logs, RightTab::Chat],
      "chat-mode delegated row: mode surface without Settings"
    );
    app.list_cursor = 3;
    app.clear_rows_cache();
    assert_eq!(
      app.available_right_tabs(),
      vec![RightTab::Logs],
      "transcription model has no mode surface: logs only"
    );
  }

  #[test]
  fn ingest_status_collects_enabled_backend_binaries() {
    let mut app = App::new(AppOptions::default());
    app.ingest_status(&json!({
      "daemon": {"pid": 1},
      "backends": [
        // No `enabled` field → counts as enabled (llamacpp).
        {"id": "llamacpp", "binary": "/usr/bin/llama-server"},
        {"id": "lemonade", "enabled": true, "binary": "/usr/bin/lemond"},
        // Disabled rows drop out even with a binary.
        {"id": "future", "enabled": false, "binary": "/usr/bin/future"},
        // No binary resolved → no entry.
        {"id": "ghost", "enabled": true},
      ],
    }));
    assert_eq!(
      app.daemon_info.backend_binaries,
      vec![
        BackendBinary {
          id: "llamacpp".into(),
          binary: "/usr/bin/llama-server".into()
        },
        BackendBinary {
          id: "lemonade".into(),
          binary: "/usr/bin/lemond".into()
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
  fn cycle_right_tab_falls_back_to_settings_when_unreachable() {
    // F1 #2: the cycle helpers used to hardcode `Logs` as the
    // empty-set fallback, contradicting `ensure_right_tab_reachable`
    // which lands on Settings. Now both helpers walk the reachable
    // list and snap to its first entry (Settings is universal).
    let mut app = App::new(AppOptions::default());
    // No models loaded → focused_managed() is None → tabs = [Settings]
    app.right_tab = RightTab::Logs;
    app.cycle_right_tab();
    assert_eq!(
      app.right_tab,
      RightTab::Settings,
      "cycle_right_tab must land in the reachable set"
    );
    app.right_tab = RightTab::Chat;
    app.cycle_right_tab_prev();
    assert_eq!(
      app.right_tab,
      RightTab::Settings,
      "cycle_right_tab_prev must land in the reachable set"
    );
  }

  #[test]
  fn snap_cursor_to_launch_clears_stale_launch_picker() {
    // F1 #3: `snap_cursor_to_launch` is the only `list_cursor`
    // writer that used to skip `sync_picker_to_focus`. When a
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
