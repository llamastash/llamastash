//! Top-middle info-row pane: daemon socket + pid + uptime,
//! llama-server binary, OpenAI-compat proxy listener, discovery
//! counters, and a one-line running summary.
//!
//! Five label-prefixed rows. Long paths left-truncate with `…/`. The
//! `running` line collapses to `—` when nothing is supervised. Width
//! is flexible — this panel takes whatever's between Host (fixed 32)
//! and Logo (fixed 25 when present). Build version lives on the
//! title row (`render_title_left`) and is not repeated here.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::theme::Palette;
use crate::tui::app::App;

const LABEL_WIDTH: usize = 8;
const LABEL_SERVER: &str = "server  ";
const LABEL_MODELS: &str = "models  ";
const LABEL_RUNNING: &str = "running ";
const LABEL_PROXY: &str = "proxy   ";
const LABEL_PORT: &str = "port    ";

/// Render the Daemon info panel into `area`. The block title is
/// `Daemon`; inner content is five label-prefixed rows (daemon,
/// server, proxy, counts, running).
pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App, palette: &Palette) {
  let block = palette.panel_block(" Daemon ", true);
  let inner = block.inner(area);
  frame.render_widget(block, area);

  let row_budget = inner.width.saturating_sub(LABEL_WIDTH as u16) as usize;
  let lines: Vec<Line<'_>> = vec![
    daemon_row(app, row_budget, palette),
    server_row(app, row_budget, palette),
    proxy_row(app, palette),
    counts_row(app, palette),
    running_row(app, row_budget, palette),
  ];
  frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the proxy listener's state. Always on (the third row): when
/// the proxy is up, the row reports `<listen> · webui <listen>/ui` so
/// both the live OpenAI-compat endpoint and the browser web-UI entry
/// point are one glance away; when disabled or absent, the row renders an
/// explicit `disabled` / `—` so the reader can tell "off by config"
/// from "not reported yet". Labels match the wire `status` values
///  and the success/error palette signals liveness without
/// forcing the user to read the endpoint.
fn proxy_row<'a>(app: &'a App, palette: &'a Palette) -> Line<'a> {
  let mut spans = vec![Span::styled(LABEL_PROXY, palette.label_style())];
  match app.daemon_info.proxy.as_ref() {
    None => spans.push(Span::styled(
      crate::tui::glyphs::active().placeholder(),
      palette.muted_style(),
    )),
    Some(info) => {
      let listen = info.listen.as_deref().unwrap_or("");
      match info.status.as_str() {
        "listening" => {
          // `<listen> (auth)? · webui <listen>/ui`. The endpoints render
          // in the success tone (live); the ` · webui ` label rides the
          // row's label colour so `webui` reads as a sibling of `proxy`,
          // not part of the API endpoint. The `(auth)` flag sits right
          // after the API endpoint so it reads as a listener property,
          // not a UI one.
          let mut endpoint = listen.to_string();
          if info.auth.as_deref() == Some("enforced") {
            endpoint.push_str(" (auth)");
          }
          spans.push(Span::styled(endpoint, palette.success_style()));
          spans.push(Span::styled(
            format!(" {} webui ", crate::tui::glyphs::active().middot()),
            palette.label_style(),
          ));
          spans.push(Span::styled(
            format!("{listen}/ui"),
            palette.success_style(),
          ));
        }
        "port_in_use" => {
          spans.push(Span::styled(
            format!("port_in_use {listen}"),
            palette.error_style(),
          ));
        }
        "unbound" => {
          let cause = info.bind_error.as_deref().unwrap_or("bind failed");
          spans.push(Span::styled(
            format!("unbound {listen} ({cause})"),
            palette.error_style(),
          ));
        }
        "refused_insecure" => {
          spans.push(Span::styled(
            format!("refused {listen} (no auth; set a key or --insecure-no-auth)"),
            palette.error_style(),
          ));
        }
        "disabled" => spans.push(Span::styled("disabled", palette.muted_style())),
        other => spans.push(Span::styled(
          format!("{other} {listen}"),
          palette.muted_style(),
        )),
      }
    }
  }
  Line::from(spans)
}

fn daemon_row<'a>(app: &'a App, _budget: usize, palette: &'a Palette) -> Line<'a> {
  // Layout: `port    48134 · pid 1234 · up 3h12m`. The panel title is
  // already "Daemon", so a leading `daemon  ` label would just
  // repeat it. The full control-plane URL is loopback-only and the
  // host half is always `127.0.0.1`, so we surface the port (the
  // only operator-relevant chunk) directly. `port` is padded to
  // LABEL_WIDTH so its value column lines up with the other rows;
  // `port`, `pid`, and `up` render in the panel's label colour and
  // numeric values render in text colour.
  let dash = crate::tui::glyphs::active().placeholder();
  let port_val = app
    .daemon_info
    .ipc_url
    .as_deref()
    .and_then(parse_port_from_url)
    .map(|p| p.to_string())
    .unwrap_or_else(|| dash.into());
  let pid_val = app
    .daemon_info
    .pid
    .map(|p| p.to_string())
    .unwrap_or_else(|| dash.into());
  let uptime_val = match (app.daemon_connected, app.daemon_info.uptime_seconds) {
    (true, Some(secs)) => format_uptime(secs),
    _ => dash.into(),
  };

  Line::from(vec![
    Span::styled(LABEL_PORT, palette.label_style()),
    Span::styled(port_val, palette.text_style()),
    Span::styled(
      format!(" {} pid ", crate::tui::glyphs::active().middot()),
      palette.label_style(),
    ),
    Span::styled(pid_val, palette.text_style()),
    Span::styled(
      format!(" {} up ", crate::tui::glyphs::active().middot()),
      palette.label_style(),
    ),
    Span::styled(uptime_val, palette.text_style()),
  ])
}

/// Extract the port from an `http://host:port` URL. Returns `None`
/// when the URL is malformed or no port is present — the caller
/// renders `—` in that case rather than guessing.
fn parse_port_from_url(url: &str) -> Option<u16> {
  let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
  let authority = after_scheme.split('/').next().unwrap_or("");
  authority
    .rsplit_once(':')
    .and_then(|(_, port)| port.parse::<u16>().ok())
}

fn server_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  // Startup auto-spawn was refused (backend fail-fast precheck) and no
  // daemon has answered since: this row carries the explanation. Takes
  // precedence over the stale `daemon_info` paths below — with no
  // daemon, `server_path` is `None` and the generic "Not found" hint
  // would mask the real, actionable failure.
  if !app.daemon_connected {
    if let Some(err) = app.daemon_start_error.as_deref() {
      return server_error_row(err, budget, palette);
    }
  }

  // When the cursor sits on a model running on a *non-default*
  // `llama-server` binary, swap the row to that binary and paint it in
  // the success colour so it's obvious the hovered launch is on a
  // different backend than the daemon's default server. Multi-binary
  // setups point CUDA / ROCm / Vulkan launches at separate builds, so
  // moving the cursor lets the operator see which one each model uses.
  if let Some(dev) = app.focused_override_device() {
    // The backend tag comes from the catalog entry's own selector
    // prefix (the hovered launch's binary), not `flavor_label` — that
    // helper reports the *default* server's host-derived backend, which
    // would mislabel an override build on a mixed-vendor host.
    let flavor_chunk = format!(" ({})", dev.backend.to_lowercase());
    let path = dev.binary.display().to_string();
    let path_budget = budget.saturating_sub(flavor_chunk.width());
    let path_truncated = crate::tui::fmt::truncate_start(&path, path_budget);
    let path_style = palette
      .success_style()
      .add_modifier(ratatui::style::Modifier::BOLD);
    return Line::from(vec![
      Span::styled(LABEL_SERVER, palette.label_style()),
      Span::styled(path_truncated, path_style),
      Span::styled(flavor_chunk, palette.success_style()),
    ]);
  }

  // Build one `(path, tag)` entry per backend. Prefer the neutral server
  // catalog once probed — each backend's first server binary, tagged with its
  // compute backends joined by `|` (`(rocm|vulkan)`) or the backend id when it
  // exposes no devices (`(lemonade)`, `(ds4)`). Before the probe lands, fall
  // back to the default `llama-server` + the per-backend binaries from
  // `status.backends`. Every entry gets an equal share of the width; paths
  // left-truncate (`…/`) so the binary name — the discriminating part —
  // always survives.
  let entries: Vec<(String, String)> = if !app.servers.is_empty() {
    server_entries_by_backend(&app.servers)
  } else if let Some(p) = app.daemon_info.server_path.as_deref() {
    let flavor_chunk = match flavor_label(app) {
      Some(f) => format!(" ({f})"),
      None => String::new(),
    };
    let mut e = vec![(p.to_string(), flavor_chunk)];
    for b in &app.daemon_info.backend_binaries {
      if b.id != crate::backend::DEFAULT_BACKEND_ID {
        e.push((b.binary.clone(), format!(" ({})", b.id)));
      }
    }
    e
  } else {
    Vec::new()
  };

  if entries.is_empty() {
    // No binary resolved: point the user at the override knobs instead of a
    // bare `—` — they can install llama.cpp or set the env / flag to fix it.
    let hint = "Not found. Set LLAMASTASH_LLAMA_SERVER or pass --llama-server";
    let trimmed = right_ellipsise(hint, budget);
    return Line::from(vec![
      Span::styled(LABEL_SERVER, palette.label_style()),
      Span::styled(trimmed, palette.error_style()),
    ]);
  }
  let sep = crate::tui::glyphs::active().middot_sep();
  let per_entry = equal_entry_budget(budget, entries.len(), sep.width());
  let mut spans = vec![Span::styled(LABEL_SERVER, palette.label_style())];
  for (i, (path, tag)) in entries.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(sep, palette.muted_style()));
    }
    let path_truncated =
      crate::tui::fmt::truncate_start(path, per_entry.saturating_sub(tag.width()));
    spans.push(Span::styled(path_truncated, palette.text_style()));
    spans.push(Span::styled(tag.clone(), palette.muted_style()));
  }
  Line::from(spans)
}

/// One `(binary, tag)` per backend from the server catalog, in catalog order.
/// `tag` is the backend's compute backends joined with `|` (`" (rocm|vulkan)"`),
/// or `" (<backend id>)"` when its servers expose no probed devices. The binary
/// is the backend's first server (its default build).
fn server_entries_by_backend(servers: &[crate::backend::Server]) -> Vec<(String, String)> {
  let mut out: Vec<(String, String)> = Vec::new();
  let mut seen: Vec<&str> = Vec::new();
  for s in servers {
    if seen.contains(&s.backend_id.as_str()) {
      continue;
    }
    seen.push(&s.backend_id);
    let mut labels: Vec<String> = Vec::new();
    for g in servers.iter().filter(|x| x.backend_id == s.backend_id) {
      for d in &g.devices {
        let lbl = d.gpu_backend.to_lowercase();
        if !labels.contains(&lbl) {
          labels.push(lbl);
        }
      }
    }
    let tag = if labels.is_empty() {
      format!(" ({})", s.backend_id)
    } else {
      format!(" ({})", labels.join("|"))
    };
    out.push((s.binary.display().to_string(), tag));
  }
  out
}

/// Render the daemon-start refusal in the server row. `err` carries
/// one single-line failure per backend (the precheck joins them with
/// `\n`); every backend shares the row with an **equal** per-entry
/// budget, each middle-truncated so a long lemonade message can't
/// push a llama.cpp failure off-screen entirely.
fn server_error_row<'a>(err: &str, budget: usize, palette: &'a Palette) -> Line<'a> {
  let entries: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
  let mut spans = vec![Span::styled(LABEL_SERVER, palette.label_style())];
  if entries.is_empty() {
    return Line::from(spans);
  }
  let sep = crate::tui::glyphs::active().middot_sep();
  let per_entry = equal_entry_budget(budget, entries.len(), sep.width());
  for (i, entry) in entries.iter().enumerate() {
    if i > 0 {
      spans.push(Span::styled(sep, palette.muted_style()));
    }
    spans.push(Span::styled(
      middle_ellipsise(entry, per_entry),
      palette.error_style(),
    ));
  }
  Line::from(spans)
}

/// Split `budget` columns equally across `n` row entries separated by
/// `sep_w`-wide separators. Shared by the multi-backend server row and
/// the multi-failure error row so "equal share, none evicts another"
/// stays one rule.
fn equal_entry_budget(budget: usize, n: usize, sep_w: usize) -> usize {
  budget.saturating_sub(sep_w * n.saturating_sub(1)) / n.max(1)
}

/// Truncate `s` to fit `budget` terminal columns by dropping the
/// **middle**, keeping head and tail around a single `…`. The
/// daemon-start failure messages lead with the backend name and end
/// with the fix (`--force` / the config knob); middle truncation keeps
/// both visible where left- or right-truncation would sacrifice one.
fn middle_ellipsise(s: &str, budget: usize) -> String {
  if budget == 0 {
    return String::new();
  }
  if s.width() <= budget {
    return s.to_string();
  }
  let ellipsis = crate::tui::glyphs::active().ellipsis();
  let ell_w = ellipsis.width();
  if budget <= ell_w {
    // No room for content either side — show the ellipsis alone,
    // clipped to the budget so a multi-cell ASCII `...` can't overflow.
    return take_head_by_width(ellipsis, budget);
  }
  let keep = budget - ell_w; // reserve the ellipsis cells
  let head = take_head_by_width(s, keep.div_ceil(2));
  let tail = crate::tui::fmt::take_tail_by_width(s, keep / 2);
  format!("{head}{ellipsis}{tail}")
}

/// Take the longest prefix of `s` whose display width is `<= budget`.
/// Mirrors [`take_tail_by_width`]; iterates forward so a wide
/// character that wouldn't fit is dropped rather than split.
fn take_head_by_width(s: &str, budget: usize) -> String {
  let mut acc_w = 0usize;
  let mut out = String::new();
  for ch in s.chars() {
    let w = ch.to_string().width();
    if acc_w + w > budget {
      break;
    }
    out.push(ch);
    acc_w += w;
  }
  out
}

/// Right-truncate `s` to fit `budget` terminal columns, appending
/// `…` when content was dropped. Use this for free-form text like
/// hints/messages where leading characters carry the most signal;
/// [`ellipsise`] keeps the tail (paths/launch-ids) instead.
fn right_ellipsise(s: &str, budget: usize) -> String {
  if budget == 0 {
    return String::new();
  }
  if s.width() <= budget {
    return s.to_string();
  }
  let ellipsis = crate::tui::glyphs::active().ellipsis();
  let keep = budget.saturating_sub(ellipsis.width());
  let mut acc_w = 0usize;
  let mut out = String::with_capacity(keep + ellipsis.len());
  for ch in s.chars() {
    let w = ch.to_string().width();
    if acc_w + w > keep {
      break;
    }
    out.push(ch);
    acc_w += w;
  }
  out.push_str(ellipsis);
  out
}

fn counts_row<'a>(app: &'a App, palette: &'a Palette) -> Line<'a> {
  let total = app.models.len();
  if total == 0 {
    return Line::from(vec![
      Span::styled(LABEL_MODELS, palette.label_style()),
      Span::styled("no models found", palette.muted_style()),
    ]);
  }
  let ready = app
    .managed
    .iter()
    .filter(|m| matches!(m.state, crate::tui::status_icons::SurfaceState::Ready))
    .count();
  // Count only favorites whose path is in the current catalog so the
  // number matches what the user can actually find in the list. Stale
  // favorites (file deleted / moved out of watched dirs) silently
  // drop off; running favorites still count (their star is visible
  // in the folder group even when the dedicated `★ Favorites`
  // shortcut excludes them).
  let catalog: std::collections::HashSet<&std::path::Path> =
    app.models.iter().map(|m| m.path.as_path()).collect();
  let favorites = app
    .favorites
    .iter()
    .filter(|p| catalog.contains(p.as_path()))
    .count();
  Line::from(vec![
    Span::styled(LABEL_MODELS, palette.label_style()),
    Span::styled(format!("{total} found"), palette.text_style()),
    Span::styled(
      crate::tui::glyphs::active().middot_sep(),
      palette.muted_style(),
    ),
    Span::styled(format!("{ready} ready"), palette.text_style()),
    Span::styled(
      crate::tui::glyphs::active().middot_sep(),
      palette.muted_style(),
    ),
    Span::styled(
      format!("{favorites} {}", crate::tui::glyphs::active().star()),
      palette.warning_style(),
    ),
  ])
}

fn running_row<'a>(app: &'a App, budget: usize, palette: &'a Palette) -> Line<'a> {
  let n = app.managed.len();
  if n == 0 {
    return Line::from(vec![
      Span::styled(LABEL_RUNNING, palette.label_style()),
      Span::styled("0", palette.muted_style()),
    ]);
  }
  // Layout: `running 3 (name1 :port1 · name2 :port2 · …)`. The
  // count prefix is fixed-width, the paren-list takes the rest and
  // truncates with `…` (not `…/`) since model names read left-to-
  // right; the right edge is the dispensable end.
  let prefix = format!("{n} (");
  let suffix = ")";
  let prefix_w = prefix.width();
  let suffix_w = suffix.width();
  let list_budget = budget.saturating_sub(prefix_w + suffix_w);
  let parts: Vec<String> = app
    .managed
    .iter()
    .map(|m| format!("{} :{}", app.model_label(&m.path), m.port))
    .collect();
  let joined = parts.join(crate::tui::glyphs::active().middot_sep());
  let trimmed = right_ellipsise(&joined, list_budget);
  Line::from(vec![
    Span::styled(LABEL_RUNNING, palette.label_style()),
    Span::styled(prefix, palette.text_style()),
    Span::styled(trimmed, palette.text_style()),
    Span::styled(suffix, palette.text_style()),
  ])
}

/// Backend → human-readable flavor tag used in the `server` row.
///
/// The tag reflects the llama.cpp backend the auto-installer picks for
/// this host (see `init::install::gh_releases::pick_asset_suffix`). AMD
/// is OS-dependent: Linux installs the ROCm build, but Windows installs
/// the **Vulkan** build (ROCm's Windows support is too narrow to pick
/// blindly), so labeling a Windows AMD server `rocm` is wrong.
fn flavor_label(app: &App) -> Option<&'static str> {
  use crate::daemon::host_metrics::GpuFlavor;
  match app.host_metrics.flavor() {
    GpuFlavor::Nvidia | GpuFlavor::Multi => Some("cuda"),
    GpuFlavor::Amd => Some(if cfg!(target_os = "windows") {
      "vulkan"
    } else {
      "rocm"
    }),
    GpuFlavor::AppleMetal => Some("metal"),
    GpuFlavor::CpuOnly => Some("cpu"),
    GpuFlavor::Unknown | GpuFlavor::Unsampled => None,
  }
}

fn format_uptime(secs: u64) -> String {
  let hours = secs / 3600;
  let minutes = (secs % 3600) / 60;
  if hours > 0 {
    format!("{hours}h{minutes:02}m")
  } else {
    format!("{minutes}m")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::daemon::host_metrics::HostMetricsSnapshot;
  use crate::discovery::{DiscoveredModel, ModelSource};
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
  use crate::tui::app::{App, AppOptions, DaemonInfo, ManagedRow};
  use crate::tui::status_icons::SurfaceState;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;
  use std::path::PathBuf;

  fn fake_model(path: &str, parent: &str) -> DiscoveredModel {
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

  fn fake_managed(path: &str, port: u16, state: SurfaceState) -> ManagedRow {
    ManagedRow {
      launch_id: "L".into(),
      path: PathBuf::from(path),
      port,
      state,
      device: None,
      rss_bytes: Some(4_200_000_000),
      cpu_pct: Some(312.0),
      ..Default::default()
    }
  }

  fn render_lines(app: &App) -> Vec<String> {
    let palette = app.palette();
    let mut term = Terminal::new(TestBackend::new(50, 7)).unwrap();
    term
      .draw(|f| render(f, Rect::new(0, 0, 50, 7), app, palette))
      .unwrap();
    let buf = term.backend().buffer().clone();
    let mut rows: Vec<String> = Vec::new();
    for y in 0..buf.area.height {
      let mut row = String::new();
      for x in 0..buf.area.width {
        row.push_str(buf.cell((x, y)).unwrap().symbol());
      }
      rows.push(row.trim_end().to_string());
    }
    rows
  }

  #[test]
  fn format_uptime_omits_hours_when_zero() {
    assert_eq!(format_uptime(0), "0m");
    assert_eq!(format_uptime(45), "0m");
    assert_eq!(format_uptime(60), "1m");
    assert_eq!(format_uptime(3600), "1h00m");
    assert_eq!(format_uptime(3 * 3600 + 12 * 60), "3h12m");
  }

  #[test]
  fn counts_row_renders_no_models_message_when_empty() {
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    assert!(rows.iter().any(|r| r.contains("no models found")));
  }

  #[test]
  fn counts_row_renders_total_ready_favorites() {
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m")];
    app.managed = vec![fake_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    app.favorites = vec![PathBuf::from("/m/a.gguf")];
    let rows = render_lines(&app);
    assert!(
      rows.iter().any(|r| r.contains("1 found")),
      "counts row missing `1 found`: {rows:#?}"
    );
    assert!(rows.iter().any(|r| r.contains("1 ready")));
    assert!(rows.iter().any(|r| r.contains("1 ★")));
  }

  #[test]
  fn counts_row_drops_stale_favorites_not_in_catalog() {
    // A favorite whose path is no longer in `app.models` (file deleted
    // or moved out of watched dirs) must NOT inflate the `N ★` count —
    // the list pane would have no row to render it on, so reporting it
    // here desyncs the count from what's actually visible.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m")];
    app.favorites = vec![
      PathBuf::from("/m/a.gguf"),     // in catalog
      PathBuf::from("/m/ghost.gguf"), // stale — not in catalog
    ];
    let rows = render_lines(&app);
    assert!(
      rows.iter().any(|r| r.contains("1 ★")),
      "stale favorite must drop from the count: {rows:#?}"
    );
    assert!(
      !rows.iter().any(|r| r.contains("2 ★")),
      "count must not include stale favorites: {rows:#?}"
    );
  }

  #[test]
  fn counts_row_includes_running_favorites() {
    // A favorited model that's currently running still counts — the
    // user has it favorited, the catalog still surfaces it, and the
    // star is visible on the row in its folder group. Each model
    // counts exactly once.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m"), fake_model("/m/b.gguf", "/m")];
    app.managed = vec![fake_managed("/m/a.gguf", 41100, SurfaceState::Ready)];
    app.favorites = vec![PathBuf::from("/m/a.gguf"), PathBuf::from("/m/b.gguf")];
    let rows = render_lines(&app);
    assert!(
      rows.iter().any(|r| r.contains("2 ★")),
      "running favorite must still count: {rows:#?}"
    );
  }

  #[test]
  fn daemon_row_shows_em_dash_uptime_when_daemon_disconnected() {
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = false;
    app.daemon_info = DaemonInfo {
      uptime_seconds: Some(120),
      pid: Some(1234),
      ..Default::default()
    };
    let rows = render_lines(&app);
    // The uptime chunk should read `up —`, not `up 2m`, because the
    // connection went down — the cached value is stale.
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("up —"),
      "expected `up —` chunk when daemon disconnected, got: {daemon_row:?}"
    );
  }

  #[test]
  fn daemon_row_shows_uptime_chunk_when_daemon_connected() {
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = true;
    app.daemon_info = DaemonInfo {
      uptime_seconds: Some(3 * 3600 + 12 * 60),
      pid: Some(1234),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("up 3h12m"),
      "expected `up 3h12m` chunk, got: {daemon_row:?}"
    );
  }

  #[test]
  fn running_row_shows_zero_when_no_managed_rows() {
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(
      running_row.contains("0") && !running_row.contains("("),
      "expected `running 0` (no parens) when none managed, got: {running_row:?}"
    );
  }

  #[test]
  fn running_row_lists_each_managed_with_count_prefix() {
    let mut app = App::new(AppOptions::default());
    app.managed = vec![
      fake_managed("/m/qwen.gguf", 41100, SurfaceState::Ready),
      fake_managed("/m/gemma.gguf", 41101, SurfaceState::Loading),
    ];
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(
      running_row.contains("2 ("),
      "expected count prefix `2 (`, got: {running_row:?}"
    );
    assert!(
      running_row.contains("qwen :41100"),
      "expected `qwen :41100` entry, got: {running_row:?}"
    );
  }

  #[test]
  fn running_row_truncates_with_ellipsis_when_list_overflows() {
    let mut app = App::new(AppOptions::default());
    app.managed = (0..5)
      .map(|i| {
        fake_managed(
          &format!("/m/model-with-very-long-name-{i}"),
          41100 + i,
          SurfaceState::Ready,
        )
      })
      .collect();
    let rows = render_lines(&app);
    let running_row = rows.iter().find(|r| r.contains("running")).unwrap();
    assert!(
      running_row.contains("…"),
      "expected trailing `…` on overflow, got: {running_row:?}"
    );
    // The count prefix must always survive truncation.
    assert!(
      running_row.contains("5 ("),
      "expected `5 (` count prefix even on overflow, got: {running_row:?}"
    );
  }

  #[test]
  fn server_row_swaps_to_override_binary_for_focused_running_model() {
    // Cursor on a model running on a non-default binary: the server
    // row shows that binary (not the daemon default) plus the device's
    // own backend tag, so the operator can see the hovered launch is on
    // a different backend.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m")];
    app.managed = vec![ManagedRow {
      launch_id: "L1".into(),
      path: PathBuf::from("/m/a.gguf"),
      port: 41100,
      state: SurfaceState::Ready,
      device: Some("CUDA1".into()),
      rss_bytes: None,
      cpu_pct: None,
      ..Default::default()
    }];
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.servers = vec![crate::backend::Server {
      id: "llamacpp-cuda".into(),
      backend_id: "llamacpp".into(),
      binary: PathBuf::from("/opt/cuda/llama-server"),
      name: "llamacpp-cuda".into(),
      devices: vec![crate::backend::Device {
        selector: "CUDA1".into(),
        gpu_backend: "CUDA".into(),
        name: "Test GPU".into(),
        total_mib: Some(24576),
        free_mib: Some(24000),
      }],
    }];
    // Rows: [TableHeader, Header(▶ Running), Model] — running row at 2.
    app.list_cursor = 2;
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("/opt/cuda/llama-server"),
      "expected override binary path, got: {server_row:?}"
    );
    assert!(
      server_row.contains("(cuda)"),
      "expected device-derived backend tag, got: {server_row:?}"
    );
    assert!(
      !server_row.contains("/usr/bin/llama-server"),
      "default path must not appear when an override is in effect: {server_row:?}"
    );
  }

  #[test]
  fn server_row_keeps_default_path_when_no_override() {
    // Cursor on a header (no focused model) — the row renders the
    // daemon's default binary, never an override.
    let mut app = App::new(AppOptions::default());
    app.models = vec![fake_model("/m/a.gguf", "/m")];
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.list_cursor = 0;
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("/usr/bin/llama-server"),
      "expected default path with no override, got: {server_row:?}"
    );
  }

  #[test]
  fn server_row_lists_other_backend_binaries_with_equal_budget() {
    use crate::tui::app::BackendBinary;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      backend_binaries: vec![
        // llamacpp's own entry is skipped (server_path already renders it).
        BackendBinary {
          id: "llamacpp".into(),
          binary: "/usr/bin/llama-server".into(),
        },
        BackendBinary {
          id: "lemonade".into(),
          binary: "/usr/bin/lemond".into(),
        },
      ],
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("llama-server"),
      "default binary must render: {server_row:?}"
    );
    assert!(
      server_row.contains("(lemonade)"),
      "other backend gets its id tag: {server_row:?}"
    );
    assert!(
      server_row.contains(" · "),
      "entries must be separated: {server_row:?}"
    );
    assert_eq!(
      server_row.matches("llama-server").count(),
      1,
      "llamacpp must not render twice: {server_row:?}"
    );
  }

  #[test]
  fn equal_entry_budget_splits_evenly_and_survives_zero() {
    assert_eq!(equal_entry_budget(40, 1, 3), 40);
    assert_eq!(equal_entry_budget(40, 2, 3), 18);
    assert_eq!(equal_entry_budget(0, 2, 3), 0);
    assert_eq!(equal_entry_budget(40, 0, 3), 40);
  }

  #[test]
  fn server_row_emits_flavor_from_host_metrics_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "nvidia".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("(cuda)"),
      "expected `(cuda)` flavor on nvidia backend, got: {server_row:?}"
    );
  }

  #[test]
  fn server_row_picks_install_flavor_for_amd_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "amd".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    // Windows AMD installs the Vulkan build, not ROCm (see flavor_label).
    let expected = if cfg!(target_os = "windows") {
      "(vulkan)"
    } else {
      "(rocm)"
    };
    assert!(server_row.contains(expected), "{server_row:?}");
  }

  #[test]
  fn server_row_picks_metal_flavor_for_apple_metal_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "apple_metal".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(server_row.contains("(metal)"), "{server_row:?}");
  }

  #[test]
  fn server_row_omits_flavor_chunk_for_unsampled_backend() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "unsampled".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    // No parenthesised flavor chunk should appear yet.
    assert!(
      !server_row.contains('('),
      "unsampled backend should suppress the flavor chunk: {server_row:?}"
    );
  }

  #[test]
  fn daemon_row_renders_pid_when_daemon_info_carries_one() {
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      pid: Some(1234),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("pid 1234"),
      "expected `pid 1234`, got: {daemon_row:?}"
    );
  }

  #[test]
  fn daemon_row_renders_port_chunk_when_ipc_url_present() {
    // The row distils `ipc_url` down to its port — the loopback host
    // is constant (`127.0.0.1`) so showing it on every render wastes
    // panel real estate that narrow terminals need for the pid/up
    // chunks.
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      pid: Some(4242),
      ipc_url: Some("http://127.0.0.1:48134".into()),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("port    48134"),
      "expected `port    48134` chunk (label padded to LABEL_WIDTH), got: {daemon_row:?}"
    );
    assert!(
      !daemon_row.contains("127.0.0.1"),
      "host half should not bleed into the row: {daemon_row:?}"
    );
    assert!(
      daemon_row.contains("pid 4242"),
      "pid must remain on the row alongside the port: {daemon_row:?}"
    );
  }

  #[test]
  fn daemon_row_uses_middot_separators() {
    // `port    48134 · pid 4242 · up 3h12m` — the chunks are joined by
    // ` · ` (the TUI's separator convention), not the old double-space.
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = true;
    app.daemon_info = DaemonInfo {
      pid: Some(4242),
      ipc_url: Some("http://127.0.0.1:48134".into()),
      uptime_seconds: Some(3 * 3600 + 12 * 60),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("· pid ") && daemon_row.contains("· up "),
      "expected ` · pid ` / ` · up ` middot separators: {daemon_row:?}"
    );
  }

  #[test]
  fn daemon_row_renders_em_dash_port_when_ipc_url_missing() {
    // A pre-Phase-A daemon (or a status response that hasn't landed
    // yet) leaves `ipc_url = None`. The row must still render a
    // placeholder rather than collapsing the `port` chunk away — the
    // reader needs the same five fixed rhythms to keep their eye on
    // pid + uptime.
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = true;
    app.daemon_info = DaemonInfo {
      pid: Some(4242),
      uptime_seconds: Some(60),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let daemon_row = rows.iter().find(|r| r.contains("port ")).unwrap();
    assert!(
      daemon_row.contains("port    —"),
      "expected `port    —` placeholder (label padded to LABEL_WIDTH), got: {daemon_row:?}"
    );
  }

  #[test]
  fn parse_port_from_url_handles_well_formed_urls() {
    assert_eq!(parse_port_from_url("http://127.0.0.1:48134"), Some(48134));
    assert_eq!(parse_port_from_url("http://127.0.0.1:48134/"), Some(48134));
    assert_eq!(parse_port_from_url("http://localhost:8080/rpc"), Some(8080));
  }

  #[test]
  fn parse_port_from_url_returns_none_when_port_absent_or_bad() {
    // No `:port` segment — caller renders `—`.
    assert_eq!(parse_port_from_url("http://127.0.0.1"), None);
    // Non-numeric port.
    assert_eq!(parse_port_from_url("http://127.0.0.1:abc"), None);
    // Out-of-range (u16 overflow).
    assert_eq!(parse_port_from_url("http://127.0.0.1:99999"), None);
  }

  #[test]
  fn server_row_shows_actionable_hint_when_binary_unresolved() {
    // When the daemon hasn't located a `llama-server` binary, the
    // row tells the user what knobs to turn — rather than a bare
    // `—` that reads as a bug. The hint mentions both the env var
    // and the CLI flag so either fix is discoverable.
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: None,
      ..Default::default()
    };
    app.host_metrics = HostMetricsSnapshot {
      gpu_backend: "amd".into(),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    // The narrow 50-cell test backend truncates the trailing
    // `--llama-server` suggestion, but the leading "Not found"
    // tag and the full `LLAMASTASH_LLAMA_SERVER` env-var name
    // survive — both are actionable on their own.
    assert!(
      server_row.contains("Not found"),
      "expected `Not found` hint, got: {server_row:?}"
    );
    assert!(
      server_row.contains("LLAMASTASH"),
      "expected LLAMASTASH env-var hint, got: {server_row:?}"
    );
    assert!(
      !server_row.contains('('),
      "flavor must be suppressed when binary is unresolved: {server_row:?}"
    );
  }

  #[test]
  fn middle_ellipsise_keeps_head_and_tail() {
    assert_eq!(middle_ellipsise("short", 10), "short");
    let s = "lemonade umbrella port 127.0.0.1:13305 is already in use — use --force";
    let out = middle_ellipsise(s, 30);
    assert!(out.width() <= 30, "width {} > 30: {out:?}", out.width());
    assert!(out.contains('…'), "expected middle ellipsis: {out:?}");
    assert!(
      out.starts_with("lemonade"),
      "head must survive middle truncation: {out:?}"
    );
    assert!(
      out.ends_with("--force"),
      "tail (the fix hint) must survive middle truncation: {out:?}"
    );
  }

  #[test]
  fn middle_ellipsise_handles_tiny_budgets() {
    assert_eq!(middle_ellipsise("abcdef", 0), "");
    assert_eq!(middle_ellipsise("abcdef", 1), "…");
    let out = middle_ellipsise("abcdef", 3);
    assert!(out.width() <= 3, "{out:?}");
    assert!(out.contains('…'), "{out:?}");
  }

  #[test]
  fn server_row_shows_daemon_start_error_when_disconnected() {
    // Auto-spawn refused by the backend precheck: the server row
    // carries the refusal (middle-truncated), not the generic
    // "Not found. Set LLAMASTASH_LLAMA_SERVER…" hint that would
    // otherwise render for the empty daemon_info.
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = false;
    app.daemon_start_error =
      Some("llama-server binary not found — install one or pass --force.".into());
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("llama-server"),
      "head of the failure must render: {server_row:?}"
    );
    assert!(
      server_row.contains("--force"),
      "tail (the fix hint) must render: {server_row:?}"
    );
    assert!(
      !server_row.contains("Not found. Set"),
      "generic binary hint must not mask the precheck failure: {server_row:?}"
    );
  }

  #[test]
  fn server_row_splits_budget_equally_across_backend_failures() {
    // Two backends failed (one precheck line each): both must stay
    // visible on the single row, equally truncated — a long lemonade
    // message can't evict the llama.cpp one.
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = false;
    app.daemon_start_error = Some(
      "llama-A-head xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx tail-A\n\
       lemon-B-head yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy tail-B"
        .into(),
    );
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("llama-A") && server_row.contains("lemon-B"),
      "both backend heads must survive: {server_row:?}"
    );
    assert!(
      server_row.contains("tail-A") && server_row.contains("tail-B"),
      "both backend tails must survive: {server_row:?}"
    );
    assert_eq!(
      server_row.matches('…').count(),
      2,
      "each entry middle-truncates exactly once: {server_row:?}"
    );
    assert!(
      server_row.contains(" · "),
      "entries must be separated: {server_row:?}"
    );
  }

  #[test]
  fn server_row_ignores_start_error_once_daemon_connected() {
    // A live daemon supersedes the startup refusal: the row goes back
    // to the normal binary-path rendering.
    let mut app = App::new(AppOptions::default());
    app.daemon_connected = true;
    app.daemon_start_error = Some("stale refusal".into());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let server_row = rows.iter().find(|r| r.contains("server")).unwrap();
    assert!(
      server_row.contains("/usr/bin/llama-server"),
      "expected normal server path: {server_row:?}"
    );
    assert!(
      !server_row.contains("stale refusal"),
      "refusal must not render once connected: {server_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_listening_endpoint_when_set() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("127.0.0.1:11434".into()),
        status: "listening".into(),
        bind_error: None,
        auth: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows
      .iter()
      .find(|r| r.contains("proxy"))
      .expect("proxy row must render when daemon_info.proxy is set");
    assert!(
      proxy_row.contains("127.0.0.1:11434 · webui"),
      "expected `127.0.0.1:11434 · webui ...`: {proxy_row:?}"
    );
    // Keyless loopback must not gain an auth marker.
    assert!(
      !proxy_row.contains("(auth)"),
      "unexpected auth marker: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_flags_auth_on_lan_listener() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("0.0.0.0:11434".into()),
        status: "listening".into(),
        bind_error: None,
        auth: Some("enforced".into()),
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows
      .iter()
      .find(|r| r.contains("proxy"))
      .expect("proxy row");
    // `<listen> (auth) · webui <listen>/ui`: the auth flag sits on the
    // API endpoint and the `· webui` segment opens the browser entry
    // point. (The trailing webui URL is clipped by the narrow test panel.)
    assert!(
      proxy_row.contains("0.0.0.0:11434 (auth)") && proxy_row.contains("· webui"),
      "expected LAN listener with auth marker + webui segment: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_ui_label_uses_proxy_label_colour() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("127.0.0.1:11435".into()),
        status: "listening".into(),
        bind_error: None,
        auth: None,
      }),
      ..Default::default()
    };
    let palette = app.palette();
    let line = proxy_row(&app, palette);
    // The ` · webui ` segment carries the label colour (same as the `proxy`
    // row label, the first span), not the success endpoint tone.
    let ui_span = line
      .spans
      .iter()
      .find(|s| s.content.as_ref() == " · webui ")
      .expect("a ` · webui ` label span");
    assert_eq!(
      ui_span.style,
      palette.label_style(),
      "ui label must match the proxy label colour, not the endpoint tone"
    );
    // Sanity: the endpoint span is a different (success) tone, so this
    // isn't trivially passing because every span shares one style.
    let endpoint_span = line
      .spans
      .iter()
      .find(|s| s.content.as_ref() == "127.0.0.1:11435")
      .expect("an endpoint span");
    assert_eq!(endpoint_span.style, palette.success_style());
  }

  #[test]
  fn proxy_row_renders_refused_insecure() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("0.0.0.0:11434".into()),
        status: "refused_insecure".into(),
        bind_error: Some("refused".into()),
        auth: Some("required".into()),
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows
      .iter()
      .find(|r| r.contains("proxy"))
      .expect("proxy row");
    // The row is width-limited; it shows the state + cause. The full
    // fix hint (set a key / --insecure-no-auth) rides the toast.
    assert!(
      proxy_row.contains("refused") && proxy_row.contains("no auth"),
      "refused_insecure row must flag the missing auth: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_port_in_use_with_endpoint() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("127.0.0.1:11434".into()),
        status: "port_in_use".into(),
        bind_error: None,
        auth: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows.iter().find(|r| r.contains("proxy")).unwrap();
    assert!(
      proxy_row.contains("port_in_use"),
      "expected `port_in_use` label: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_em_dash_when_no_proxy_info() {
    // The proxy row is always present in the 5-row layout; when the
    // daemon hasn't reported a proxy block yet (or a pre-Unit-5
    // daemon omits it entirely), the row reads `proxy   —` instead
    // of being hidden — so the reader can tell "not reported" apart
    // from `disabled` (config off) or `listening` (live).
    let app = App::new(AppOptions::default());
    let rows = render_lines(&app);
    let proxy_row = rows
      .iter()
      .find(|r| r.contains("proxy"))
      .expect("proxy row must always render");
    assert!(
      proxy_row.contains("—"),
      "expected em-dash placeholder when daemon_info.proxy is None: {proxy_row:?}"
    );
  }

  #[test]
  fn proxy_row_renders_disabled_when_config_disabled() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      proxy: Some(ProxyInfo {
        enabled: false,
        listen: None,
        status: "disabled".into(),
        bind_error: None,
        auth: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    let proxy_row = rows.iter().find(|r| r.contains("proxy")).unwrap();
    assert!(
      proxy_row.contains("disabled"),
      "expected `disabled` body for disabled status, got: {proxy_row:?}"
    );
  }

  #[test]
  fn server_and_proxy_rows_render_simultaneously() {
    use crate::tui::app::ProxyInfo;
    let mut app = App::new(AppOptions::default());
    app.daemon_info = DaemonInfo {
      server_path: Some("/usr/bin/llama-server".into()),
      proxy: Some(ProxyInfo {
        enabled: true,
        listen: Some("127.0.0.1:11434".into()),
        status: "listening".into(),
        bind_error: None,
        auth: None,
      }),
      ..Default::default()
    };
    let rows = render_lines(&app);
    assert!(
      rows
        .iter()
        .any(|r| r.contains("server") && r.contains("llama-server")),
      "expected server row alongside the proxy row: {rows:#?}"
    );
    assert!(
      rows
        .iter()
        .any(|r| r.contains("proxy") && r.contains("127.0.0.1:11434 · webui")),
      "expected proxy row alongside the server row: {rows:#?}"
    );
  }
}
