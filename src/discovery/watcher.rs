//! Live-update the discovered-model list via a debounced filesystem
//! watcher (origin: R22).
//!
//! Events are coalesced (e.g., copying a split-shard set, or `hf-hub`
//! writing many `.part` files in quick succession) into one event per
//! debounce window — 500 ms by default per the plan. Each event surfaces
//! to the caller as a [`WatchEvent`] over an `mpsc::Receiver`; the daemon's discovery task consumes
//! these and re-runs the affected scan slice to refresh
//! `list_models`.
//!
//! A 5-minute `tokio::time::interval` periodic rescan tick rides
//! alongside as a backstop: deeply-nested cache trees (HuggingFace
//! hub) can drop events under load, and a missed `.gguf` should not
//! mean a permanently invisible model. The tick fires on the same
//! channel with [`WatchEvent::PeriodicRescan`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// What the watcher reports. Consumers don't need to distinguish
/// create/modify/delete at this layer — the discovery task re-runs the
/// scanner over the affected root regardless.
#[derive(Debug, Clone)]
pub enum WatchEvent {
  /// Filesystem activity under one of the watched roots. `paths`
  /// lists every path the debouncer collected within the quiet
  /// window; consumers can target a re-scan to the impacted dirs.
  Changed { paths: Vec<PathBuf> },
  /// 5-minute periodic backstop. Consumers should re-walk every
  /// watched root in case the OS dropped an event under load.
  PeriodicRescan,
}

/// How deeply to watch a given root. Two-mode shape because that's
/// what `notify` exposes; deeper depth-limiting (e.g., "two levels
/// down only") happens at the discovery-task layer by enumerating
/// child paths up-front and registering each as a `Shallow` watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
  /// Recursive watch — recommended for user-managed model
  /// directories that aren't deeply nested.
  Recursive,
  /// Non-recursive watch — used for HuggingFace's `hub/` cache, where
  /// a recursive watch would register thousands of inotify slots for
  /// every `models--<owner>--<repo>/snapshots/<rev>/blobs/` subtree.
  /// Direct children of the watched path still fire events; deeper
  /// changes are caught by the 5-minute periodic rescan backstop.
  Shallow,
}

impl From<WatchMode> for RecursiveMode {
  fn from(m: WatchMode) -> Self {
    match m {
      WatchMode::Recursive => RecursiveMode::Recursive,
      WatchMode::Shallow => RecursiveMode::NonRecursive,
    }
  }
}

/// One root to watch plus the depth policy for that root. Caller-
/// constructed so the discovery layer (which knows source labels)
/// can pick a sensible mode per provenance.
#[derive(Debug, Clone)]
pub struct WatchRoot {
  pub path: PathBuf,
  pub mode: WatchMode,
}

impl WatchRoot {
  pub fn recursive(path: impl Into<PathBuf>) -> Self {
    Self {
      path: path.into(),
      mode: WatchMode::Recursive,
    }
  }

  pub fn shallow(path: impl Into<PathBuf>) -> Self {
    Self {
      path: path.into(),
      mode: WatchMode::Shallow,
    }
  }
}

/// Tunables. Production defaults match the plan: 500 ms debounce,
/// 5-minute periodic backstop. Tests shorten them for responsiveness.
#[derive(Debug, Clone, Copy)]
pub struct WatcherOptions {
  pub debounce: Duration,
  pub periodic_rescan: Duration,
  /// Buffer for the outbound event channel. Defaults to 64 so a slow
  /// consumer doesn't starve the watcher thread.
  pub channel_capacity: usize,
}

impl Default for WatcherOptions {
  fn default() -> Self {
    Self {
      debounce: Duration::from_millis(500),
      periodic_rescan: Duration::from_secs(5 * 60),
      // 256 absorbs a single HF snapshot's worth of debounced events
      // without dropping. Below this, a download of a multi-shard
      // model would trigger backstop reconciliation; above this, we
      // start sitting on stale events for a slow consumer.
      channel_capacity: 256,
    }
  }
}

/// Handle that keeps the watcher alive. Dropping it stops the
/// filesystem watcher, its debounce thread and the periodic-rescan task;
/// in-flight events already on the channel are still deliverable.
pub struct WatcherHandle {
  _watcher: RecommendedWatcher,
  _periodic_task: tokio::task::JoinHandle<()>,
}

/// Whether an event can change what a scan finds. Opens and read-only closes
/// are dropped: the scan's own directory walk and header reads produce them,
/// so passing them on made every rescan schedule the next one.
fn changes_content(kind: &EventKind) -> bool {
  match kind {
    EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
    EventKind::Access(_) => false,
    _ => true,
  }
}

/// Collect raw event paths and send them as one [`WatchEvent::Changed`] once
/// `debounce` has passed since the first of them. Returns when the watcher
/// (the only sender) is dropped.
fn debounce_loop(
  raw: std_mpsc::Receiver<Vec<PathBuf>>,
  tx: mpsc::Sender<WatchEvent>,
  debounce: Duration,
) {
  let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
  let mut deadline: Option<Instant> = None;
  loop {
    let next = match deadline {
      None => raw
        .recv()
        .map_err(|_| std_mpsc::RecvTimeoutError::Disconnected),
      Some(d) => raw.recv_timeout(d.saturating_duration_since(Instant::now())),
    };
    match next {
      Ok(paths) => {
        deadline.get_or_insert_with(|| Instant::now() + debounce);
        pending.extend(paths);
      }
      Err(std_mpsc::RecvTimeoutError::Timeout) => {
        deadline = None;
        if !flush(&tx, &mut pending) {
          return;
        }
      }
      Err(std_mpsc::RecvTimeoutError::Disconnected) => {
        flush(&tx, &mut pending);
        return;
      }
    }
  }
}

/// Send `pending` as one event. `false` once the consumer is gone.
fn flush(tx: &mpsc::Sender<WatchEvent>, pending: &mut BTreeSet<PathBuf>) -> bool {
  if pending.is_empty() {
    return true;
  }
  let paths: Vec<PathBuf> = std::mem::take(pending).into_iter().collect();
  // `try_send` so a slow consumer can't pin this thread; a dropped burst is
  // reconciled by the periodic rescan. Warn so watcher pressure shows in logs
  // rather than as "models take 5 minutes to show up after a download spike".
  match tx.try_send(WatchEvent::Changed { paths }) {
    Ok(()) => true,
    Err(mpsc::error::TrySendError::Full(_)) => {
      log::warn!(
        "watcher channel full; dropping fs event burst (will reconcile on next periodic rescan)"
      );
      true
    }
    Err(mpsc::error::TrySendError::Closed(_)) => false,
  }
}

/// Begin watching `roots`. Returns a receiver that yields
/// [`WatchEvent`]s and a handle that must be retained for the watcher
/// to keep running.
///
/// Roots that don't exist (or aren't readable) are logged and
/// skipped — discovery should still surface events for the remaining
/// roots. An empty roots list yields a receiver that only ever
/// produces [`WatchEvent::PeriodicRescan`] ticks, which is the
/// degenerate "no scan paths configured" shape.
pub fn start(
  roots: Vec<WatchRoot>,
  opts: WatcherOptions,
) -> Result<(WatcherHandle, mpsc::Receiver<WatchEvent>), notify::Error> {
  let (tx, rx) = mpsc::channel(opts.channel_capacity);

  let (raw_tx, raw_rx) = std_mpsc::channel::<Vec<PathBuf>>();
  let mut watcher =
    notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
      Ok(event) if changes_content(&event.kind) && !event.paths.is_empty() => {
        let _ = raw_tx.send(event.paths);
      }
      Ok(_) => {}
      Err(err) => log::warn!("filesystem watcher error: {err}"),
    })?;
  let tx_for_debounce = tx.clone();
  let debounce = opts.debounce;
  std::thread::Builder::new()
    .name("llamastash-watch-debounce".into())
    .spawn(move || debounce_loop(raw_rx, tx_for_debounce, debounce))
    .map_err(|e| notify::Error::generic(&format!("debounce thread: {e}")))?;

  for root in &roots {
    if !root.path.exists() {
      log::warn!(
        "watcher: root does not exist, skipping: {}",
        root.path.display()
      );
      continue;
    }
    if let Err(e) = watcher.watch(&root.path, RecursiveMode::from(root.mode)) {
      log::warn!("watcher: cannot watch {}: {e}", root.path.display());
    }
  }

  // Periodic rescan tick. A `tokio::time::interval` fires roughly on
  // the configured cadence; missed ticks coalesce so a paused
  // consumer doesn't get a flurry on resume.
  let tx_for_periodic = tx;
  let periodic_period = opts.periodic_rescan;
  let periodic_task = tokio::spawn(async move {
    let mut ticker = tokio::time::interval(periodic_period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick — callers do the initial scan
    // themselves when they wire the watcher up.
    ticker.tick().await;
    loop {
      ticker.tick().await;
      if tx_for_periodic
        .send(WatchEvent::PeriodicRescan)
        .await
        .is_err()
      {
        return;
      }
    }
  });

  Ok((
    WatcherHandle {
      _watcher: watcher,
      _periodic_task: periodic_task,
    },
    rx,
  ))
}

/// Convenience: filter a [`WatchEvent::Changed`]'s paths down to just
/// those whose extension is `.gguf` (live `.part` files and other
/// noise drop out). Returns an empty vec for other event variants.
pub fn changed_gguf_paths(event: &WatchEvent) -> Vec<&Path> {
  match event {
    WatchEvent::Changed { paths } => paths
      .iter()
      .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("gguf"))
      .map(PathBuf::as_path)
      .collect(),
    WatchEvent::PeriodicRescan => Vec::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-watcher-{label}-{}-{nanos}",
      std::process::id()
    ));
    fs::create_dir_all(&p).expect("temp root");
    p
  }

  fn fast_opts() -> WatcherOptions {
    WatcherOptions {
      debounce: Duration::from_millis(50),
      periodic_rescan: Duration::from_millis(150),
      channel_capacity: 16,
    }
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn changed_event_fires_when_gguf_lands_in_watched_root() {
    let root = temp_root("change");
    let (_handle, mut rx) =
      start(vec![WatchRoot::recursive(root.clone())], fast_opts()).expect("start watcher");

    // Drop a file *after* the watcher is wired up.
    let gguf = root.join("dropped.gguf");
    fs::write(&gguf, b"GGUF\x03\x00\x00\x00").unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
      .await
      .expect("watcher emits within 2s")
      .expect("channel still open");
    match event {
      WatchEvent::Changed { paths } => {
        assert!(
          paths.iter().any(|p| p.ends_with("dropped.gguf")),
          "expected dropped.gguf in event, got {paths:?}"
        );
      }
      WatchEvent::PeriodicRescan => {
        // Periodic ticks may interleave on slow machines — fish for
        // the actual change event.
        let next = tokio::time::timeout(Duration::from_secs(2), rx.recv())
          .await
          .expect("second event within 2s")
          .expect("channel open");
        match next {
          WatchEvent::Changed { paths } => assert!(
            paths.iter().any(|p| p.ends_with("dropped.gguf")),
            "expected dropped.gguf, got {paths:?}"
          ),
          other => panic!("expected Changed after PeriodicRescan, got {other:?}"),
        }
      }
    }
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn periodic_rescan_fires_on_its_own() {
    let root = temp_root("periodic");
    let (_handle, mut rx) =
      start(vec![WatchRoot::recursive(root.clone())], fast_opts()).expect("start watcher");

    // Drain spurious `Changed` events that some platforms emit when
    // the watcher first attaches to a freshly-created temp dir
    // (macOS FSEvents flushes a synthetic "directory exists" event
    // on subscribe). We're proving the periodic tick fires on its
    // own, so the assertion is "at least one PeriodicRescan arrives
    // within the deadline", not "the very first event is a tick".
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut got_tick = false;
    while std::time::Instant::now() < deadline {
      let remaining = deadline.saturating_duration_since(std::time::Instant::now());
      match tokio::time::timeout(remaining, rx.recv()).await {
        Ok(Some(WatchEvent::PeriodicRescan)) => {
          got_tick = true;
          break;
        }
        Ok(Some(WatchEvent::Changed { .. })) => continue,
        Ok(None) => panic!("watcher channel closed before PeriodicRescan"),
        Err(_) => break,
      }
    }
    assert!(got_tick, "no PeriodicRescan event within 2s deadline");
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn missing_root_is_logged_and_skipped_without_failure() {
    let alive = temp_root("alive");
    let dead = PathBuf::from("/nonexistent/llamastash/watcher/root");
    let (_handle, _rx) = start(
      vec![
        WatchRoot::recursive(dead),
        WatchRoot::recursive(alive.clone()),
      ],
      fast_opts(),
    )
    .expect("missing root must not error");
    fs::remove_dir_all(&alive).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn shallow_root_does_not_observe_deep_descendant_writes() {
    // The HF-style scope: watch the root non-recursively so a
    // deeply-nested write (a blob land in `models--*/snapshots/...`)
    // does NOT trip an instant event. The periodic-rescan backstop
    // is the safety net for those.
    let root = temp_root("shallow");
    std::fs::create_dir_all(root.join("nested/two/three")).unwrap();
    // Drain the post-create events the kernel may emit, then attach
    // the watcher.
    let (_handle, mut rx) =
      start(vec![WatchRoot::shallow(root.clone())], fast_opts()).expect("start watcher");
    // Write into a *deep* descendant. With Shallow mode, this must
    // not produce a `Changed` event — only the eventual periodic
    // tick will surface it.
    fs::write(root.join("nested/two/three/deep.gguf"), b"GGUF\x03").unwrap();
    let first = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    match first {
      Ok(Some(WatchEvent::Changed { paths })) => {
        // Some platforms report the directory-level modify on the
        // *immediate* parent the watcher sees, which is fine — what
        // matters is no event for the deep file path itself.
        assert!(
          !paths.iter().any(|p| p.ends_with("deep.gguf")),
          "shallow watch must not surface deep descendant writes, got {paths:?}"
        );
      }
      // No event within 1s and no periodic tick within 1s is the
      // expected behaviour — shallow watch ignored the deep write.
      Ok(Some(WatchEvent::PeriodicRescan)) | Ok(None) | Err(_) => {}
    }
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn shallow_root_still_observes_immediate_child_writes() {
    // A new direct child (e.g., a new `models--owner--repo` dir
    // appearing in `~/.cache/huggingface/hub/`) must still trip a
    // `Changed` event — that's the watch surface that keeps the
    // HF root reactive even with depth-limiting.
    let root = temp_root("shallow-child");
    let (_handle, mut rx) =
      start(vec![WatchRoot::shallow(root.clone())], fast_opts()).expect("start watcher");
    fs::create_dir(root.join("models--owner--repo")).unwrap();
    let mut saw_child = false;
    for _ in 0..3 {
      let event = match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        Ok(Some(e)) => e,
        _ => break,
      };
      if let WatchEvent::Changed { paths } = event {
        if paths.iter().any(|p| p.ends_with("models--owner--repo")) {
          saw_child = true;
          break;
        }
      }
    }
    assert!(
      saw_child,
      "immediate child creation must fire a Changed event"
    );
    fs::remove_dir_all(&root).ok();
  }

  /// SPIKE: temporary probe, not a real test. Establishes whether the reads
  /// themselves fire a change event on Windows, or whether the setup creation
  /// just arrives late. Phase A watches with no filesystem access at all, so
  /// anything that shows up in phase B came from the reads.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn spike_reads_event_timing() {
    use std::time::Instant;

    let root = temp_root("spike-reads");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/model.gguf"), b"GGUF\x03").unwrap();
    let t0 = Instant::now();
    let mut log: Vec<String> = Vec::new();
    let opts = WatcherOptions {
      periodic_rescan: Duration::from_secs(3600),
      ..fast_opts()
    };
    let (_handle, mut rx) =
      start(vec![WatchRoot::recursive(root.clone())], opts).expect("start watcher");
    log.push(format!("{:?} root created, watcher started", t0.elapsed()));

    loop {
      match tokio::time::timeout(Duration::from_millis(3000), rx.recv()).await {
        Ok(Some(e)) => log.push(format!("{:?} A {}", t0.elapsed(), path_summary(&e, &root))),
        _ => break,
      }
    }
    log.push(format!("{:?} A quiet for 3s", t0.elapsed()));

    for _ in 0..5 {
      let _ = fs::read_dir(root.join("sub")).unwrap().count();
      let _ = fs::read(root.join("sub/model.gguf")).unwrap();
    }
    log.push(format!("{:?} B reads issued", t0.elapsed()));

    loop {
      match tokio::time::timeout(Duration::from_millis(3000), rx.recv()).await {
        Ok(Some(e)) => log.push(format!("{:?} B {}", t0.elapsed(), path_summary(&e, &root))),
        _ => break,
      }
    }
    log.push(format!("{:?} B quiet for 3s", t0.elapsed()));
    fs::remove_dir_all(&root).ok();
    panic!(
      "SPIKE TRANSCRIPT root={}\n{}",
      root.display(),
      log.join("\n")
    );
  }

  fn path_summary(event: &WatchEvent, root: &std::path::Path) -> String {
    match event {
      WatchEvent::Changed { paths } => paths
        .iter()
        .map(|p| pstrip(p, root))
        .collect::<Vec<_>>()
        .join(", "),
      other => format!("{other:?}"),
    }
  }

  fn pstrip(path: &std::path::Path, root: &std::path::Path) -> String {
    match path.strip_prefix(root) {
      Ok(rel) if !rel.as_os_str().is_empty() => {
        format!("<root>/{}", rel.display())
      }
      _ => format!("{}", path.display()),
    }
  }

  /// The scan reads every directory and header under a root; if those reads
  /// counted as changes, each rescan would trigger the next one.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn reads_do_not_fire_a_changed_event() {
    let root = temp_root("reads");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/model.gguf"), b"GGUF\x03").unwrap();
    let opts = WatcherOptions {
      periodic_rescan: Duration::from_secs(3600),
      ..fast_opts()
    };
    let (_handle, mut rx) =
      start(vec![WatchRoot::recursive(root.clone())], opts).expect("start watcher");
    // `root` was built before the watcher started, and on macOS and Windows the
    // creation still arrives afterwards as one `Changed` over the whole tree.
    // Drain until the channel goes quiet, otherwise the read window below picks
    // up that event and the reads look like what triggered it.
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
      // Startup creation events, discarded until the channel goes quiet.
    }
    for _ in 0..5 {
      let _ = fs::read_dir(root.join("sub")).unwrap().count();
      let _ = fs::read(root.join("sub/model.gguf")).unwrap();
    }
    let got = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
    assert!(got.is_err(), "reads must not fire an event, got {got:?}");

    fs::write(root.join("sub/model.gguf"), b"GGUF\x03\x00").unwrap();
    let after_write = tokio::time::timeout(Duration::from_secs(2), rx.recv())
      .await
      .expect("a write fires an event")
      .expect("channel open");
    assert!(matches!(after_write, WatchEvent::Changed { .. }));
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn changed_gguf_paths_filters_to_gguf_extension() {
    let event = WatchEvent::Changed {
      paths: vec![
        PathBuf::from("/a/model.gguf"),
        PathBuf::from("/a/model.gguf.part"),
        PathBuf::from("/a/notes.txt"),
      ],
    };
    let filtered: Vec<_> = changed_gguf_paths(&event).into_iter().collect();
    assert_eq!(filtered.len(), 1);
    assert!(filtered[0].ends_with("model.gguf"));
    // Periodic rescan never carries paths.
    assert!(changed_gguf_paths(&WatchEvent::PeriodicRescan).is_empty());
  }

  #[tokio::test]
  async fn empty_roots_still_yields_periodic_rescan_ticks() {
    // Degenerate "no scan paths configured" shape: the receiver must
    // still be alive and produce PeriodicRescan ticks. We use a tiny
    // periodic interval so the test wakes quickly.
    let opts = WatcherOptions {
      debounce: Duration::from_millis(10),
      periodic_rescan: Duration::from_millis(50),
      channel_capacity: 4,
    };
    let (handle, mut rx) = start(Vec::new(), opts).expect("watcher start");
    let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
      .await
      .expect("must produce a periodic tick within 2s")
      .expect("channel still open");
    assert!(matches!(evt, WatchEvent::PeriodicRescan));
    drop(handle);
  }
}
