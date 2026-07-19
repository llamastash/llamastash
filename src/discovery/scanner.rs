//! Walk one or more scan roots, group split-GGUF shards, parse each
//! launchable file's header on a bounded pool, and stream results to
//! the caller over an `mpsc` channel (origin: R1, R5, R9).
//!
//! The walk uses the `ignore` crate so `.gitignore` rules and the
//! caller's exclude globs are honoured for free. CPU-bound parsing
//! runs on `tokio::task::spawn_blocking` so the scan tasks don't
//! starve the runtime when the user has hundreds of GGUFs on disk.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use futures::stream::{self, StreamExt};
use ignore::WalkBuilder;
use regex::Regex;
use tokio::sync::mpsc;

use crate::discovery::metadata_cache::{self, CachedParse, MetadataCache};
use crate::discovery::split_gguf::{group, DiscoveredEntry};
use crate::discovery::{DiscoveredModel, ModelSource, Multimodal};
use crate::gguf::{read_path, summarise_metadata, GgufError, HeaderReadOptions, ModelMetadata};

/// One root to scan plus how to label files found beneath it.
#[derive(Debug, Clone)]
pub struct ScanRoot {
  pub path: PathBuf,
  pub source: ModelSource,
}

/// Options for [`scan`]. `excludes` are appended to the gitignore-
/// derived ignores; absolute or relative-to-root globs both work.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
  pub excludes: Vec<String>,
  /// Capacity for the streaming channel. The TUI is usually faster
  /// than disk, but a tiny capacity makes back-pressure visible in
  /// tests; production defaults to a comfortable buffer.
  pub channel_capacity: Option<usize>,
  /// Optional per-file parse cache. When `Some`, unchanged
  /// `(canonical path, mtime, size)` triples reuse the cached
  /// parse instead of re-reading + re-parsing the header. The
  /// daemon's discovery task wires a shared cache so successive
  /// watcher-driven re-scans don't re-parse the whole tree.
  pub metadata_cache: Option<MetadataCache>,
}

impl ScanOptions {
  pub fn channel_capacity(&self) -> usize {
    self.channel_capacity.unwrap_or(64)
  }
}

/// Begin a scan across `roots`. Returns the receiver immediately; the
/// scan runs in the background and closes the channel when every root
/// has been walked.
///
/// Errors per-file (unreadable directories, parse failures) are
/// surfaced via `DiscoveredModel.parse_error` rather than aborting the
/// whole scan — a single bad model file should not blind the user to
/// the rest of their library (origin: R9 "scan continues with other
/// roots").
pub fn scan(roots: Vec<ScanRoot>, opts: ScanOptions) -> mpsc::Receiver<DiscoveredModel> {
  let (tx, rx) = mpsc::channel(opts.channel_capacity());
  let excludes = Arc::new(opts.excludes);
  let cache = opts.metadata_cache;
  tokio::spawn(async move {
    for root in roots {
      walk_root(root, Arc::clone(&excludes), cache.clone(), tx.clone()).await;
    }
    // dropping `tx` here closes the receiver
  });
  rx
}

async fn walk_root(
  root: ScanRoot,
  excludes: Arc<Vec<String>>,
  cache: Option<MetadataCache>,
  tx: mpsc::Sender<DiscoveredModel>,
) {
  let path = root.path.clone();
  let source = root.source;
  let excludes_for_walk = Arc::clone(&excludes);
  let paths = tokio::task::spawn_blocking(move || collect_gguf_paths(&path, &excludes_for_walk))
    .await
    .unwrap_or_else(|join_err| {
      log::warn!(
        "scan walker task for {} panicked: {join_err}",
        root.path.display()
      );
      Vec::new()
    });

  // Parse files in parallel on the blocking pool. The per-file
  // `build_discovered_model` already pushes its CPU-bound work onto
  // `spawn_blocking`; `buffer_unordered` just lets several of those
  // happen at once instead of strict one-at-a-time await. On a cold
  // HF cache with hundreds of GGUFs this cuts first-scan latency from
  // serial-disk-bound to parallel-disk-bound. We deliberately do NOT
  // use rayon — the work is mostly waiting on disk, and we want the
  // tokio scheduler to interleave with the rest of the daemon.
  let entries: Vec<_> = group(paths);
  let cache_ref = cache.clone();
  let mut stream = stream::iter(entries.into_iter().map(|entry| {
    let cache_ref = cache_ref.clone();
    async move { build_discovered_model(entry, source, cache_ref.as_ref()).await }
  }))
  .buffer_unordered(parallel_parse_limit());
  while let Some(model) = stream.next().await {
    if tx.send(model).await.is_err() {
      return;
    }
  }
}

/// Is this `.gguf` file a multimodal projector companion (e.g.,
/// `mmproj-model-f16.gguf`)? Projector files are not independently
/// launchable — they are tensors that pair with a parent chat model
/// for vision/audio input. The user policy is to hide them from the
/// Models list unless they could be launched on their own, which they
/// cannot. The filename prefix is the upstream convention (used by
/// `llama.cpp`'s `convert_hf_to_gguf.py` and every published
/// HuggingFace repo that ships a projector). Filtering on the name
/// avoids paying the cost of a header re-read.
pub(crate) fn is_projector_companion(path: &Path) -> bool {
  path
    .file_name()
    .and_then(|n| n.to_str())
    .map(|n| {
      let n = n.to_lowercase();
      n.ends_with(".gguf")
        && (n.starts_with("mmproj-")
          || n.starts_with("mmproj_")
          || n.contains(".mmproj.")
          || n.ends_with(".mmproj.gguf")
          || n.ends_with("-mmproj.gguf")
          || n.ends_with("_mmproj.gguf")
          || n == "mmproj.gguf")
    })
    .unwrap_or(false)
}

/// Is this `.gguf` a **separate MTP draft-head** companion (e.g.
/// `mtp-gemma-4.gguf`)? Like a projector, an MTP head is not launchable on
/// its own — it pairs with a parent chat model as the speculative drafter
/// the serving backend loads (the Gemma-4 shape; embedded-head models carry the
/// draft layers inside the base file and ship no such sibling). Excluded from
/// the launchable Models list exactly as projectors are, and paired via
/// [`find_mtp_head`]. The `mtp` token must be delimited (prefix/suffix/infix)
/// so an ordinary model with "mtp" mid-word isn't misfiled as a head.
pub(crate) fn is_mtp_companion(path: &Path) -> bool {
  path
    .file_name()
    .and_then(|n| n.to_str())
    .map(|n| {
      let n = n.to_lowercase();
      n.ends_with(".gguf")
        && (n.starts_with("mtp-")
          || n.starts_with("mtp_")
          || n.contains(".mtp.")
          || n.ends_with(".mtp.gguf")
          || n.ends_with("-mtp.gguf")
          || n.ends_with("_mtp.gguf")
          || n == "mtp.gguf")
    })
    .unwrap_or(false)
}

const QUANT_PATTERN: &str = r"(?:^|[-._])(bf16|f16|f32|mxfp4_moe|iq[1-8](_?s|_?xs|_?xxs|_?m|_?nl|_?nl_xl)?|q[1-8](_?[01])?(_?k)?(_?[sml]|_?xl)?)\b";

/// Strip all quantization tokens and separators from a name to derive
/// a canonical base name for matching.
fn canonical_base(s: &str) -> String {
  static RE_QUANT: OnceLock<Regex> = OnceLock::new();
  let re = RE_QUANT.get_or_init(|| Regex::new(QUANT_PATTERN).unwrap());

  let mut s = s.to_lowercase();

  // Strip all quantization tokens. We loop because multiple tokens
  // might exist (rare but possible).
  while let Some(m) = re.find(&s) {
    let start = m.start();
    let end = m.end();
    s.replace_range(start..end, "");
  }

  // Normalize all separators to dashes and collapse multiple dashes
  // so that "model_name" matches "model-name".
  s = s.replace(['.', '_'], "-");
  while s.contains("--") {
    s = s.replace("--", "-");
  }

  s.trim_matches('-').to_string()
}

/// Strip mmproj-related prefixes and suffixes to find the base model
/// name part.
fn strip_mmproj_markers(name: &str) -> String {
  let Some(s) = name.strip_suffix(".gguf") else {
    return name.to_lowercase();
  };
  let mut s = s.to_lowercase();

  if let Some(rest) = s
    .strip_prefix("mmproj-")
    .or_else(|| s.strip_prefix("mmproj_"))
  {
    s = rest.to_string();
  }

  if let Some(rest) = s
    .strip_suffix("-mmproj")
    .or_else(|| s.strip_suffix("_mmproj"))
    .or_else(|| s.strip_suffix(".mmproj"))
  {
    s = rest.to_string();
  }

  s = s.replace(".mmproj.", ".");

  if s == "mmproj" {
    return "".to_string();
  }

  s
}

/// Strip `mtp` prefixes/suffixes to recover the base model name a separate
/// MTP head pairs with. Mirrors [`strip_mmproj_markers`] with the `mtp` token.
fn strip_mtp_markers(name: &str) -> String {
  let Some(s) = name.strip_suffix(".gguf") else {
    return name.to_lowercase();
  };
  let mut s = s.to_lowercase();

  if let Some(rest) = s.strip_prefix("mtp-").or_else(|| s.strip_prefix("mtp_")) {
    s = rest.to_string();
  }

  if let Some(rest) = s
    .strip_suffix("-mtp")
    .or_else(|| s.strip_suffix("_mtp"))
    .or_else(|| s.strip_suffix(".mtp"))
  {
    s = rest.to_string();
  }

  s = s.replace(".mtp.", ".");

  if s == "mtp" {
    return "".to_string();
  }

  s
}

/// Collect projector filenames for a diagnostic log line.
fn projector_names(paths: &[PathBuf]) -> Vec<String> {
  paths
    .iter()
    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    .collect()
}

/// Find the multimodal projector (mmproj) companion file for a given
/// model path. The projector file is expected to be in the same
/// directory.
///
/// Matching ignores quantization labels in both filenames. The rules,
/// in order:
///
/// 1. A projector whose quant-stripped base name equals the model's
///    wins. If several match (multiple models share the directory and
///    their bases collide after quant-stripping), one is picked
///    deterministically with a warning.
/// 2. No name match, but the directory holds exactly one model and one
///    projector → pair them. One model + one projector per folder is
///    the dominant layout (HuggingFace snapshots, LM Studio), and
///    upstream repos routinely name the projector generically
///    (`mmproj-model-f16.gguf`) or after a different quant than the
///    model, so a strict name match would miss the common case. Gating
///    on a single model in the directory keeps a flat multi-model
///    folder from cross-assigning one model's projector to another.
/// 3. Otherwise, a lone "anonymous" projector (`mmproj.gguf`,
///    `mmproj-f16.gguf`) is the directory's catch-all and is used.
///    Anything else is genuinely ambiguous: emit nothing and let the
///    user pass `--mmproj` rather than load a mismatched projector.
pub fn find_mmproj(model_path: &Path) -> Option<PathBuf> {
  let parent = model_path.parent()?;
  let model_filename = model_path.file_name()?.to_str()?;
  let model_stem = model_path.file_stem()?.to_str()?;
  // Launching a projector directly has no projector of its own.
  if is_projector_companion(model_path) {
    return None;
  }

  let model_base = canonical_base(model_stem);

  let mut all_projectors: Vec<PathBuf> = Vec::new();
  let mut base_matches: Vec<PathBuf> = Vec::new();
  let mut anonymous: Vec<PathBuf> = Vec::new();
  // Count sibling model files (non-projector `.gguf`s, including the
  // model itself) so the single-projector fallback only fires when this
  // is the only model in the directory.
  let mut model_file_count = 0usize;

  for entry in std::fs::read_dir(parent).ok()?.flatten() {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
      continue;
    };
    if !name.to_lowercase().ends_with(".gguf") {
      continue;
    }
    if !is_projector_companion(&path) {
      model_file_count += 1;
      continue;
    }
    if name == model_filename {
      continue;
    }
    let proj_base = canonical_base(&strip_mmproj_markers(name));
    if proj_base.is_empty() {
      anonymous.push(path.clone());
    } else if proj_base == model_base {
      base_matches.push(path.clone());
    }
    all_projectors.push(path);
  }

  // 1. Name match wins.
  if !base_matches.is_empty() {
    base_matches.sort();
    if base_matches.len() > 1 {
      log::warn!(
        "multiple mmproj candidates match model {model_filename}; using {}: {:?}",
        base_matches[0]
          .file_name()
          .unwrap_or_default()
          .to_string_lossy(),
        projector_names(&base_matches),
      );
    }
    return base_matches.into_iter().next();
  }

  // 2. Single model + single projector → pair them regardless of name.
  if model_file_count == 1 && all_projectors.len() == 1 {
    return all_projectors.into_iter().next();
  }

  // 3. Lone anonymous catch-all, else ambiguous → give up.
  if anonymous.len() == 1 {
    return anonymous.into_iter().next();
  }
  if !all_projectors.is_empty() {
    log::warn!(
      "multiple mmproj files in {} but none match model {model_filename}; \
       skipping auto-detection (pass --mmproj to choose): {:?}",
      parent.display(),
      projector_names(&all_projectors),
    );
  }
  None
}

/// Resolve the multimodal capability a model's mmproj projector
/// advertises, or `None` when the model has no projector companion.
///
/// Reads the projector GGUF's `clip.has_vision_encoder` /
/// `clip.has_audio_encoder` flags (the llama.cpp clip convention) from a
/// header-only parse. A projector that advertises neither — older
/// vision-only mmproj files predate the audio split — is treated as
/// vision so the common case still surfaces a badge. Best-effort: an
/// unreadable projector header yields `None` rather than failing the
/// scan.
fn detect_multimodal(model_path: &Path) -> Option<Multimodal> {
  let projector = find_mmproj(model_path)?;
  let read = read_path(&projector, HeaderReadOptions::default()).ok()?;
  // clip flags are GGUF booleans, but some projector writers encode them
  // as a uint8 0/1. Accept either so an audio-only projector isn't
  // misread as the vision default.
  let flag = |key: &str| {
    read
      .header
      .metadata
      .get(key)
      .map(|v| {
        v.as_bool()
          .unwrap_or_else(|| v.as_u64().is_some_and(|n| n != 0))
      })
      .unwrap_or(false)
  };
  let vision = flag("clip.has_vision_encoder");
  let audio = flag("clip.has_audio_encoder");
  Some(if !vision && !audio {
    Multimodal {
      vision: true,
      audio: false,
    }
  } else {
    Multimodal { vision, audio }
  })
}

/// Find a **separate MTP draft-head** companion for `model_path` — the
/// sibling GGUF the serving backend loads as the drafter for speculative
/// decoding (the Gemma-4 shape). `None` when the model is embedded-MTP
/// (draft layers inside the base file) or has no head sibling.
///
/// Mirrors [`find_mmproj`]'s three-rule resolution so head naming stays as
/// forgiving as projector naming (repos label heads inconsistently):
/// 1. an MTP companion whose quant-stripped base equals the model's wins;
/// 2. else, a lone model + lone head in the directory pair regardless of name;
/// 3. else, a single anonymous `mtp.gguf` catch-all is used; anything more
///    ambiguous yields `None` (the user can pair a head explicitly).
pub fn find_mtp_head(model_path: &Path) -> Option<PathBuf> {
  let parent = model_path.parent()?;
  let model_filename = model_path.file_name()?.to_str()?;
  let model_stem = model_path.file_stem()?.to_str()?;
  // A head file has no head of its own.
  if is_mtp_companion(model_path) {
    return None;
  }

  let model_base = canonical_base(model_stem);

  let mut all_heads: Vec<PathBuf> = Vec::new();
  let mut base_matches: Vec<PathBuf> = Vec::new();
  let mut anonymous: Vec<PathBuf> = Vec::new();
  // Count launchable model files (neither an MTP head nor a projector) so the
  // single-head fallback only fires when this is the only model in the folder.
  let mut model_file_count = 0usize;

  for entry in std::fs::read_dir(parent).ok()?.flatten() {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
      continue;
    };
    if !name.to_lowercase().ends_with(".gguf") {
      continue;
    }
    if !is_mtp_companion(&path) {
      if !is_projector_companion(&path) {
        model_file_count += 1;
      }
      continue;
    }
    if name == model_filename {
      continue;
    }
    let head_base = canonical_base(&strip_mtp_markers(name));
    if head_base.is_empty() {
      anonymous.push(path.clone());
    } else if head_base == model_base {
      base_matches.push(path.clone());
    }
    all_heads.push(path);
  }

  // 1. Name match wins.
  if !base_matches.is_empty() {
    base_matches.sort();
    return base_matches.into_iter().next();
  }
  // 2. Single model + single head → pair them regardless of name.
  if model_file_count == 1 && all_heads.len() == 1 {
    return all_heads.into_iter().next();
  }
  // 3. Lone anonymous catch-all, else ambiguous → give up.
  if anonymous.len() == 1 {
    return anonymous.into_iter().next();
  }
  None
}

/// Concurrency cap for [`walk_root`]'s per-file parse. Default to
/// `num_cpus()`-flavoured but capped — too many parallel
/// `spawn_blocking` calls land everything on the blocking pool
/// regardless. Empirically 8 saturates a single NVMe.
fn parallel_parse_limit() -> usize {
  std::thread::available_parallelism()
    .map(|n| n.get().clamp(2, 8))
    .unwrap_or(4)
}

/// Synchronous file-system walk. Returns every `.gguf` file under
/// `root` honouring gitignore semantics and the caller's exclude
/// globs. Unreadable subdirectories are logged and skipped rather
/// than aborting the walk.
fn collect_gguf_paths(root: &Path, excludes: &[String]) -> Vec<PathBuf> {
  if !root.exists() {
    log::warn!("scan root does not exist: {}", root.display());
    return Vec::new();
  }
  let mut builder = WalkBuilder::new(root);
  builder
    .standard_filters(true)
    .require_git(false)
    // Follow symlinks so users who alias a GGUF into a scan root
    // (e.g., `ln -s /big-disk/model.gguf ~/models/`) still see the
    // model. The `ignore` walker detects cycles, so following links
    // doesn't expose us to symlink loops. A hostile symlink pointing
    // at a non-GGUF file is bounded by (a) the `.gguf` extension
    // gate below and (b) the GGUF parser's BadMagic short-circuit
    // and 4 MiB header cap — opening such a file reads at most a
    // few KB and surfaces as a parse error.
    .follow_links(true)
    .hidden(false);
  if !excludes.is_empty() {
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pat in excludes {
      // `ignore`'s override globs treat a leading `!` as include-back,
      // so prefix every user exclude with `!` to mean "exclude this".
      // A plain `*.tmp` glob would otherwise be interpreted as
      // "include only files matching this".
      if let Err(e) = overrides.add(&format!("!{pat}")) {
        log::warn!("invalid scan exclude glob {pat:?}: {e}");
      }
    }
    match overrides.build() {
      Ok(o) => {
        builder.overrides(o);
      }
      Err(e) => log::warn!("scan exclude globs failed to compile: {e}"),
    }
  }

  let mut out = Vec::new();
  let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
  for result in builder.build() {
    match result {
      Ok(entry) => {
        let p = entry.path();
        // Skip `.gguf.part` (mid-download) and only emit regular files
        // ending in `.gguf`. With `follow_links(true)` above, an entry
        // pointing at a symlink reports the *target*'s file type.
        if p.extension().and_then(|s| s.to_str()) == Some("gguf")
          && entry.file_type().map(|t| t.is_file()).unwrap_or(false)
          && !is_projector_companion(p)
          && !is_mtp_companion(p)
        {
          // Canonicalise before dedup so a real file and a symlink to
          // it collapse to a single row. Falling back to the raw path
          // if canonicalisation fails (broken symlink, permission
          // denied) keeps the row visible — the user can investigate.
          let raw = p.to_path_buf();
          let canonical = crate::util::paths::canonicalize(p).unwrap_or_else(|_| raw.clone());
          if seen.insert(canonical.clone()) {
            // For most files we emit the canonical path so user-managed
            // aliases (e.g. `ln -s /big-disk/m.gguf ~/models/`) display
            // under their target name. The exception is HuggingFace's
            // hub layout: blobs are sha256-named files with no `.gguf`
            // extension, surfaced via `snapshots/<rev>/<name>.gguf`
            // symlinks. llama.cpp's split-GGUF loader parses the
            // filename for `-NNNNN-of-NNNNN.gguf` and rejects bare
            // sha256 names, so emitting the canonical path would make
            // every HF-cached multi-part model fail to launch with
            // `invalid split file name`. When canonicalisation strips
            // the `.gguf` extension we treat that as the HF-blob signal
            // and keep the symlink path. Single-file HF models still
            // load fine either way; the path swap only matters for the
            // split-aware loader.
            let emit = if canonical.extension().and_then(|s| s.to_str()) == Some("gguf") {
              canonical
            } else {
              raw
            };
            out.push(emit);
          }
        }
      }
      Err(e) => log::warn!("scan walker error under {}: {e}", root.display()),
    }
  }
  out
}

async fn build_discovered_model(
  entry: DiscoveredEntry,
  source: ModelSource,
  cache: Option<&MetadataCache>,
) -> DiscoveredModel {
  match entry {
    DiscoveredEntry::Single(path) => parse_into_model(path, source, Vec::new(), cache).await,
    DiscoveredEntry::Split(group) => {
      // Siblings exclude the launch file itself so the field's purpose
      // ("sibling shards") matches its content.
      let siblings = group
        .shards
        .into_iter()
        .filter(|p| *p != group.launch_path)
        .collect();
      parse_into_model(group.launch_path, source, siblings, cache).await
    }
  }
}

async fn parse_into_model(
  path: PathBuf,
  source: ModelSource,
  siblings: Vec<PathBuf>,
  cache: Option<&MetadataCache>,
) -> DiscoveredModel {
  let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
  let probe_path = path.clone();
  let (mtime, size) = tokio::task::spawn_blocking(move || metadata_cache::probe(&probe_path))
    .await
    .unwrap_or((None, 0));

  // Cache lookup first. A hit short-circuits the header read entirely.
  if let Some(c) = cache {
    if let Some(hit) = c.get(&path, mtime, size).await {
      let mut hit_metadata = hit.metadata;
      apply_split_total_weights(&mut hit_metadata, &path, &siblings).await;
      apply_split_total_parameters(&mut hit_metadata, &path, &siblings).await;
      return DiscoveredModel {
        path,
        parent,
        source,
        metadata: hit_metadata,
        parse_error: hit.parse_error,
        split_siblings: siblings,
        display_label: None,
        multimodal: hit.multimodal,
        supported_backends: hit.supported_backends.clone(),
        mtp_head: hit.mtp_head,
      };
    }
  }

  // On a cache miss, parse the model header and detect its mmproj
  // modality + separate MTP head together on one blocking-pool hop.
  // Detection runs only here (not on warm cache hits) so periodic
  // rescans don't repeat the sibling `read_dir`s.
  let path_for_parse = path.clone();
  let (parsed, multimodal, mtp_head): (Result<_, GgufError>, Option<Multimodal>, Option<PathBuf>) =
    tokio::task::spawn_blocking(move || {
      let parsed = read_path(&path_for_parse, HeaderReadOptions::default());
      let multimodal = detect_multimodal(&path_for_parse);
      let mtp_head = find_mtp_head(&path_for_parse);
      (parsed, multimodal, mtp_head)
    })
    .await
    .unwrap_or_else(|join_err| {
      (
        Err(GgufError::Io(std::io::Error::other(format!(
          "parser task panicked: {join_err}"
        )))),
        None,
        None,
      )
    });
  let cached = match parsed {
    // Compute the routing verdict from the same header parse (free — no extra
    // IO) so the `list_models` hot path never re-reads tensor info. The
    // registry decides; this site names no backend.
    Ok(read) => CachedParse {
      metadata: Some(summarise_metadata(&read.header)),
      parse_error: None,
      multimodal,
      supported_backends: crate::backend::supported_backends_for(&read.header),
      mtp_head,
    },
    Err(e) => CachedParse {
      metadata: None,
      parse_error: Some(e.to_string()),
      multimodal,
      supported_backends: Vec::new(),
      mtp_head,
    },
  };
  if let Some(c) = cache {
    c.put(path.clone(), mtime, size, cached.clone()).await;
  }
  let mut metadata = cached.metadata;
  apply_split_total_weights(&mut metadata, &path, &siblings).await;
  apply_split_total_parameters(&mut metadata, &path, &siblings).await;
  DiscoveredModel {
    path,
    parent,
    source,
    metadata,
    parse_error: cached.parse_error,
    split_siblings: siblings,
    display_label: None,
    multimodal: cached.multimodal,
    supported_backends: cached.supported_backends.clone(),
    mtp_head: cached.mtp_head,
  }
}

/// For split-GGUF entries, replace the shard-1-only `weights_bytes`
/// with an approximation of the total tensor footprint across every
/// shard. The per-shard `summarise_metadata` only sees shard 1's
/// header, so a 2-shard 80B model was reporting ~half its real size
/// — visible as a wrong SIZE column in `llamastash list`, an
/// undersized estimate from the recommender's VRAM-fit predicate, and
/// the same wrong number in `llamastash show`.
///
/// File size is a tight upper bound on tensor bytes (GGUF header +
/// per-tensor alignment padding is <1% on quant models), and reading
/// the file metadata is cheap, so we sum on-disk sizes instead of
/// reading each sibling's header. No-op when `siblings` is empty.
async fn apply_split_total_weights(
  metadata: &mut Option<ModelMetadata>,
  path: &Path,
  siblings: &[PathBuf],
) {
  if siblings.is_empty() {
    return;
  }
  let Some(meta) = metadata.as_mut() else {
    return;
  };
  let primary = path.to_path_buf();
  let sibling_paths: Vec<PathBuf> = siblings.to_vec();
  let total = tokio::task::spawn_blocking(move || {
    crate::discovery::shard_sizes::on_disk_total(&primary, &sibling_paths)
  })
  .await
  .unwrap_or(0);
  if total > 0 {
    meta.weights_bytes = Some(total);
  }
}

/// For split-GGUF entries, replace the shard-1-only parameter count with
/// the sum across every shard. `summarise_metadata` only parses the
/// canonical shard, so a tensor-summed count (no explicit
/// `general.parameter_count` in the header) covers one shard — a 2-shard
/// 80B model was reporting ~56B in the `Params` column of `list` / `show`.
/// Mirrors [`apply_split_total_weights`], but tensor element counts aren't
/// derivable from file size, so each sibling's tensor table is read.
/// Skipped when the header declares an explicit count (already the
/// whole-model figure) or when `siblings` is empty.
async fn apply_split_total_parameters(
  metadata: &mut Option<ModelMetadata>,
  path: &Path,
  siblings: &[PathBuf],
) {
  if siblings.is_empty() {
    return;
  }
  let Some(meta) = metadata.as_mut() else {
    return;
  };
  let primary = path.to_path_buf();
  let sibling_paths: Vec<PathBuf> = siblings.to_vec();
  let summed = tokio::task::spawn_blocking(move || {
    let read = read_path(&primary, HeaderReadOptions::default()).ok()?;
    // An explicit count is model-level, so summing shards would double it.
    if crate::gguf::metadata::explicit_parameter_count(&read.header).is_some() {
      return None;
    }
    let mut total = crate::gguf::metadata::tensor_param_sum(&read.header);
    for s in &sibling_paths {
      if let Ok(sr) = read_path(s, HeaderReadOptions::default()) {
        total = total.saturating_add(crate::gguf::metadata::tensor_param_sum(&sr.header));
      }
    }
    Some(total)
  })
  .await
  .unwrap_or(None);
  if let Some(total) = summed.filter(|t| *t > 0) {
    meta.total_parameters = Some(total);
    meta.parameter_label = crate::gguf::metadata::label_for_param_count(total);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::fs;

  use crate::gguf::test_fixtures::build_minimal_gguf;

  fn temp_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("scanner-{label}"))
  }

  #[test]
  fn collect_gguf_paths_skips_part_files() {
    let dir = temp_dir("part");
    fs::write(dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("a.gguf.part"), b"in-progress").unwrap();
    let paths = collect_gguf_paths(&dir, &[]);
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("a.gguf"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn collect_gguf_paths_drops_mmproj_projector_companions() {
    // Multimodal projector files (`mmproj-*.gguf`) ride along with a
    // parent chat model but are not launchable on their own; without
    // this filter they showed up in the TUI's Models list as
    // selectable rows that would fail at launch time.
    let dir = temp_dir("mmproj");
    fs::write(dir.join("model.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(
      dir.join("mmproj-model-f16.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    fs::write(
      dir.join("mmproj_model_v2.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    fs::write(dir.join("model.mmproj.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("mmproj-BF16.gguf"), build_minimal_gguf("llama")).unwrap();
    let paths = collect_gguf_paths(&dir, &[]);
    let names: Vec<String> = paths
      .iter()
      .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
      .collect();
    assert_eq!(paths.len(), 1, "expected only model.gguf, got {names:?}");
    assert!(names[0].ends_with("model.gguf"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn collect_gguf_paths_drops_mtp_head_companions() {
    // A separate MTP draft head (`mtp-*.gguf`) pairs with a parent model as
    // its speculative drafter — not launchable on its own, so it
    // must not appear as a selectable Models-list row (exactly like mmproj).
    let dir = temp_dir("mtp-exclude");
    fs::write(dir.join("gemma-4.gguf"), build_minimal_gguf("gemma4")).unwrap();
    fs::write(dir.join("mtp-gemma-4.gguf"), build_minimal_gguf("gemma4")).unwrap();
    fs::write(dir.join("gemma-4-mtp.gguf"), build_minimal_gguf("gemma4")).unwrap();
    fs::write(dir.join("mtp.gguf"), build_minimal_gguf("gemma4")).unwrap();
    let paths = collect_gguf_paths(&dir, &[]);
    let names: Vec<String> = paths
      .iter()
      .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
      .collect();
    assert_eq!(paths.len(), 1, "expected only gemma-4.gguf, got {names:?}");
    assert!(names[0].ends_with("gemma-4.gguf"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn is_mtp_companion_requires_delimited_token() {
    // A model with "mtp" mid-word must not be misfiled as a head.
    assert!(is_mtp_companion(Path::new("/m/mtp-gemma.gguf")));
    assert!(is_mtp_companion(Path::new("/m/gemma-mtp.gguf")));
    assert!(is_mtp_companion(Path::new("/m/gemma.mtp.gguf")));
    assert!(is_mtp_companion(Path::new("/m/mtp.gguf")));
    assert!(!is_mtp_companion(Path::new("/m/gptmtptron.gguf")));
    assert!(!is_mtp_companion(Path::new("/m/gemma-4.gguf")));
  }

  #[test]
  fn find_mtp_head_detects_and_pairs() {
    let dir = temp_dir("mtp-find");
    fs::write(dir.join("gemma-4.gguf"), build_minimal_gguf("gemma4")).unwrap();
    fs::write(dir.join("mtp-gemma-4.gguf"), build_minimal_gguf("gemma4")).unwrap();
    let found = find_mtp_head(&dir.join("gemma-4.gguf"));
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mtp-gemma-4.gguf".to_string()),
      "name-matched head must pair"
    );
    // A head file has no head of its own.
    assert_eq!(find_mtp_head(&dir.join("mtp-gemma-4.gguf")), None);
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mtp_head_none_when_absent() {
    let dir = temp_dir("mtp-absent");
    fs::write(dir.join("qwen35.gguf"), build_minimal_gguf("qwen35")).unwrap();
    assert_eq!(find_mtp_head(&dir.join("qwen35.gguf")), None);
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mtp_head_pairs_single_model_single_head_regardless_of_name() {
    // Rule 2: one model + one anonymously-named head → pair them.
    let dir = temp_dir("mtp-single");
    fs::write(
      dir.join("gemma-4-it-Q4_K_M.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    fs::write(dir.join("mtp-f16.gguf"), build_minimal_gguf("gemma4")).unwrap();
    let found = find_mtp_head(&dir.join("gemma-4-it-Q4_K_M.gguf"));
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mtp-f16.gguf".to_string()),
      "single model + single head must pair regardless of name"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn collect_gguf_paths_honours_exclude_globs() {
    let dir = temp_dir("excl");
    fs::create_dir_all(dir.join("keep")).unwrap();
    fs::create_dir_all(dir.join("skip")).unwrap();
    fs::write(dir.join("keep/a.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("skip/b.gguf"), build_minimal_gguf("llama")).unwrap();
    let paths = collect_gguf_paths(&dir, &["skip/**".to_string()]);
    assert_eq!(paths.len(), 1);
    assert!(paths[0].to_string_lossy().contains("keep"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn nonexistent_root_returns_empty_without_panic() {
    let bogus = PathBuf::from("/nonexistent/scan-root-llamastash");
    assert!(collect_gguf_paths(&bogus, &[]).is_empty());
  }

  #[cfg(unix)]
  #[test]
  fn symlinked_gguf_is_canonicalised_and_deduped() {
    let dir = temp_dir("symlinks");
    fs::write(dir.join("real.gguf"), build_minimal_gguf("llama")).unwrap();
    // Alias the real file with a sibling symlink under the same root.
    let alias = dir.join("alias.gguf");
    std::os::unix::fs::symlink(dir.join("real.gguf"), &alias).unwrap();

    let paths = collect_gguf_paths(&dir, &[]);
    // One canonical row, not two — the symlink collapses onto the
    // real file via `canonicalize` + dedup.
    assert_eq!(
      paths.len(),
      1,
      "real + symlink should collapse to one canonical row, got {paths:?}"
    );
    // The emitted path is the canonical (target) path, not the alias.
    let canon_real = fs::canonicalize(dir.join("real.gguf")).unwrap();
    assert_eq!(paths[0], canon_real);
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn hf_cache_blob_symlink_keeps_symlink_path() {
    // Regression: in the HuggingFace hub layout, the canonical file
    // is a sha256-named blob (no `.gguf` extension) and the launch-
    // friendly path lives behind a snapshot symlink that preserves
    // the upstream name. llama.cpp's split loader requires the
    // `-NNNNN-of-NNNNN.gguf` naming convention, so emitting the
    // canonical blob path makes every multi-part HF model fail to
    // load with `invalid split file name`. The walker must therefore
    // keep the symlink path when the canonical target lacks a
    // `.gguf` extension. Layout mirrors `~/.cache/huggingface/hub`.
    let dir = temp_dir("hfcache");
    let blobs = dir.join("blobs");
    let snap = dir.join("snapshots/main");
    fs::create_dir_all(&blobs).unwrap();
    fs::create_dir_all(&snap).unwrap();
    let blob = blobs.join("403434e5c8454520");
    fs::write(&blob, build_minimal_gguf("llama")).unwrap();
    let named = snap.join("qwen2.5-32b-q4_k_m-00001-of-00005.gguf");
    std::os::unix::fs::symlink(&blob, &named).unwrap();

    let paths = collect_gguf_paths(&dir, &[]);
    assert_eq!(paths.len(), 1, "blob + symlink collapse to one row");
    let emitted = &paths[0];
    assert!(
      emitted.extension().and_then(|s| s.to_str()) == Some("gguf"),
      "emitted path must keep `.gguf` extension, got {emitted:?}"
    );
    assert_eq!(
      emitted.file_name().and_then(|s| s.to_str()),
      Some("qwen2.5-32b-q4_k_m-00001-of-00005.gguf"),
      "emitted path must be the snapshot symlink (split-aware name), \
       not the canonical blob"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn symlink_to_gguf_outside_root_is_followed_once() {
    // The target file lives outside `root`; a symlink under `root`
    // points at it. follow_links must surface the row.
    let outside = temp_dir("symlinks-outside-target");
    let target = outside.join("target.gguf");
    fs::write(&target, build_minimal_gguf("llama")).unwrap();
    let root = temp_dir("symlinks-outside-root");
    std::os::unix::fs::symlink(&target, root.join("aliased.gguf")).unwrap();

    let paths = collect_gguf_paths(&root, &[]);
    assert_eq!(paths.len(), 1, "symlink target outside root must surface");
    assert_eq!(paths[0], fs::canonicalize(&target).unwrap());
    fs::remove_dir_all(&outside).ok();
    fs::remove_dir_all(&root).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn scan_streams_discovered_models_with_metadata() {
    let dir = temp_dir("stream");
    fs::write(dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("b.gguf"), build_minimal_gguf("qwen3")).unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let mut got = Vec::new();
    while let Some(m) = rx.recv().await {
      got.push(m);
    }
    assert_eq!(got.len(), 2);
    for m in &got {
      assert!(m.metadata.is_some(), "minimal gguf should parse");
      assert_eq!(m.source, ModelSource::UserPath);
      assert!(m.split_siblings.is_empty());
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn scan_surfaces_parse_failure_without_dropping_row() {
    let dir = temp_dir("badparse");
    fs::write(dir.join("bad.gguf"), b"this is not a GGUF").unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let m = rx.recv().await.expect("one model surfaced");
    assert!(rx.recv().await.is_none(), "only one file in dir");
    assert!(m.metadata.is_none(), "invalid file → no metadata");
    assert!(m.parse_error.is_some(), "diagnostic must accompany failure");
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn scan_groups_split_shards_into_one_entry() {
    let dir = temp_dir("split");
    let bytes = build_minimal_gguf("llama");
    fs::write(dir.join("model-00001-of-00003.gguf"), &bytes).unwrap();
    fs::write(dir.join("model-00002-of-00003.gguf"), &bytes).unwrap();
    fs::write(dir.join("model-00003-of-00003.gguf"), &bytes).unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let m = rx.recv().await.expect("one grouped entry");
    assert!(
      rx.recv().await.is_none(),
      "shard set should collapse to one"
    );
    assert_eq!(m.split_siblings.len(), 2, "shard 1 plus 2 siblings");
    assert!(m
      .path
      .to_string_lossy()
      .ends_with("model-00001-of-00003.gguf"));
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn split_shards_report_summed_weights_bytes() {
    // Regression: a multi-shard set used to report only shard 1's
    // header-derived weights_bytes, so `llamastash list`,
    // `show`, and the recommender's VRAM-fit predicate all saw
    // ~half the real size for a 2-shard 80B Q5_K_M model. The
    // scanner now sums every shard's on-disk size into
    // `metadata.weights_bytes` so the displayed/used value covers
    // the whole model.
    let dir = temp_dir("split-size");
    let shard_bytes = build_minimal_gguf("qwen3");
    let per_shard = shard_bytes.len() as u64;
    fs::write(dir.join("m-00001-of-00002.gguf"), &shard_bytes).unwrap();
    fs::write(dir.join("m-00002-of-00002.gguf"), &shard_bytes).unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let m = rx.recv().await.expect("one grouped entry");
    let weights = m
      .metadata
      .as_ref()
      .expect("metadata present")
      .weights_bytes
      .expect("split should set summed weights_bytes");
    assert_eq!(
      weights,
      per_shard * 2,
      "split weights_bytes must equal sum of every shard's on-disk size"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn split_shards_sum_parameter_count() {
    // Regression: a multi-shard set reported only shard 1's tensor
    // sum as the parameter count, so an 80B model showed ~56B in the
    // `Params` column of `list` / `show`. With no explicit
    // `general.parameter_count` in the header, the scanner now sums
    // every shard's tensor elements into `total_parameters`.
    use crate::gguf::test_fixtures::FixtureBuilder;
    let dir = temp_dir("split-params");
    // 1M params per shard (1000×1000 weight), no explicit count key.
    let shard = FixtureBuilder::new()
      .with_arch("qwen3")
      .with_tensor("token_embd.weight", &[1000, 1000], 0)
      .build();
    fs::write(dir.join("m-00001-of-00002.gguf"), &shard).unwrap();
    fs::write(dir.join("m-00002-of-00002.gguf"), &shard).unwrap();
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];
    let mut rx = scan(roots, ScanOptions::default());
    let m = rx.recv().await.expect("one grouped entry");
    let md = m.metadata.as_ref().expect("metadata present");
    assert_eq!(
      md.total_parameters,
      Some(2_000_000),
      "split parameter count must sum every shard's tensor elements"
    );
    assert_eq!(md.parameter_label.as_deref(), Some("2M"));
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn metadata_cache_reuses_parse_for_unchanged_file() {
    use crate::discovery::metadata_cache::MetadataCache;

    let dir = temp_dir("cache-hit");
    fs::write(dir.join("a.gguf"), build_minimal_gguf("llama")).unwrap();
    let cache = MetadataCache::new(8);
    let opts = ScanOptions {
      metadata_cache: Some(cache.clone()),
      ..ScanOptions::default()
    };
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];

    async fn drain(mut rx: mpsc::Receiver<DiscoveredModel>) {
      while rx.recv().await.is_some() {}
    }

    // First scan: cache empty → one miss → one entry inserted.
    drain(scan(roots.clone(), opts.clone())).await;
    assert_eq!(cache.len().await, 1, "first scan populates cache");

    // Second scan: cache hit → still one entry, parse skipped.
    drain(scan(roots.clone(), opts.clone())).await;
    assert_eq!(cache.len().await, 1, "second scan does not duplicate");
    fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn metadata_cache_invalidates_when_size_changes() {
    use crate::discovery::metadata_cache::MetadataCache;
    use crate::gguf::test_fixtures::FixtureBuilder;

    let dir = temp_dir("cache-invalid");
    let path = dir.join("a.gguf");
    // First write: minimal arch=llama header.
    fs::write(&path, build_minimal_gguf("llama")).unwrap();
    let cache = MetadataCache::new(8);
    let opts = ScanOptions {
      metadata_cache: Some(cache.clone()),
      ..ScanOptions::default()
    };
    let roots = vec![ScanRoot {
      path: dir.clone(),
      source: ModelSource::UserPath,
    }];

    // Prime the cache.
    let mut first_rx = scan(roots.clone(), opts.clone());
    let first = first_rx.recv().await.expect("one model");
    while first_rx.recv().await.is_some() {}
    let first_arch = first
      .metadata
      .as_ref()
      .and_then(|m| m.arch.clone())
      .unwrap();
    assert_eq!(first_arch, "llama");

    // Mutate the file: different arch, different total tensor count
    // → different on-disk size and (typically) different mtime.
    let updated_bytes = FixtureBuilder::new()
      .with_arch("phi3")
      .with_context_length(4096)
      .with_tensor("blk.0.attn_q.weight", &[10, 10], 1)
      .build();
    fs::write(&path, &updated_bytes).unwrap();

    // Force the on-disk mtime to advance even on filesystems whose
    // mtime resolution is coarse (some CI tmpfs sets mtime to whole
    // seconds and rewrites within the same second can otherwise look
    // unchanged). Size *also* changed above, which alone invalidates
    // the cache; this just makes the invalidation independent of fs
    // mtime granularity.
    let _ = std::process::Command::new("touch")
      .arg("-t")
      .arg("203601011200")
      .arg(&path)
      .status();

    let mut second_rx = scan(roots, opts);
    let second = second_rx.recv().await.expect("one model");
    while second_rx.recv().await.is_some() {}
    let second_arch = second
      .metadata
      .as_ref()
      .and_then(|m| m.arch.clone())
      .unwrap();
    assert_eq!(
      second_arch, "phi3",
      "size change must invalidate cache and re-parse"
    );
  }

  #[test]
  fn find_mmproj_detects_mmproj_dash_prefix() {
    let dir = temp_dir("mmproj-find");
    fs::write(dir.join("model.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("mmproj-model.gguf"), build_minimal_gguf("llama")).unwrap();
    let found = find_mmproj(&dir.join("model.gguf"));
    assert!(found.is_some(), "find_mmproj must find mmproj-model.gguf");
    assert_eq!(
      found.unwrap().file_name().and_then(|s| s.to_str()),
      Some("mmproj-model.gguf")
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_detects_various_patterns() {
    let dir = temp_dir("mmproj-patterns");
    let model = "my-model";
    fs::write(
      dir.join(format!("{model}.gguf")),
      build_minimal_gguf("llama"),
    )
    .unwrap();

    let patterns = [
      format!("mmproj_{model}.gguf"),
      format!("{model}.mmproj.gguf"),
      format!("{model}-mmproj.gguf"),
      format!("{model}_mmproj.gguf"),
    ];

    for p in patterns {
      fs::write(dir.join(&p), build_minimal_gguf("llama")).unwrap();
      let found = find_mmproj(&dir.join(format!("{model}.gguf")));
      assert!(found.is_some(), "failed to find {p}");
      assert_eq!(
        found.unwrap().file_name().and_then(|s| s.to_str()),
        Some(p.as_str())
      );
      fs::remove_file(dir.join(&p)).unwrap();
    }
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_handles_quants() {
    let dir = temp_dir("mmproj-quants");
    // Model with quant
    fs::write(
      dir.join("Qwen2-7B-Q4_K_M.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();

    // Matching projector with quant
    fs::write(
      dir.join("mmproj-Qwen2-7B-Q4_K_M.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();

    let found = find_mmproj(&dir.join("Qwen2-7B-Q4_K_M.gguf"));
    assert_eq!(
      found.unwrap().file_name().and_then(|s| s.to_str()),
      Some("mmproj-Qwen2-7B-Q4_K_M.gguf")
    );

    // Projector with different separator and quant
    fs::remove_file(dir.join("mmproj-Qwen2-7B-Q4_K_M.gguf")).unwrap();
    fs::write(
      dir.join("Qwen2-7B.mmproj.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    let found_infix = find_mmproj(&dir.join("Qwen2-7B-Q4_K_M.gguf"));
    assert_eq!(
      found_infix.unwrap().file_name().and_then(|s| s.to_str()),
      Some("Qwen2-7B.mmproj.gguf")
    );

    // Test underscore separator for quant (regression test for Regex boundary)
    fs::remove_file(dir.join("Qwen2-7B.mmproj.gguf")).unwrap();
    fs::write(
      dir.join("Qwen2_7B_mmproj.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    let found_underscore = find_mmproj(&dir.join("Qwen2-7B-Q4_K_M.gguf"));
    assert_eq!(
      found_underscore
        .unwrap()
        .file_name()
        .and_then(|s| s.to_str()),
      Some("Qwen2_7B_mmproj.gguf")
    );

    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_warns_on_multiple_named_candidates() {
    let dir = temp_dir("mmproj-multiple-named");
    let model = "my-model";
    fs::write(
      dir.join(format!("{model}.gguf")),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    fs::write(
      dir.join(format!("mmproj-{model}-f16.gguf")),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    fs::write(
      dir.join(format!("mmproj-{model}-bf16.gguf")),
      build_minimal_gguf("llama"),
    )
    .unwrap();

    let found = find_mmproj(&dir.join(format!("{model}.gguf")));
    assert!(found.is_some());
    // Both are equally valid named candidates, pick the first one (arbitrary).
    // The log warning should have triggered.
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_handles_unsloth_style_quants() {
    let dir = temp_dir("mmproj-unsloth");
    fs::write(dir.join("model-Q4_K_M.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("mmproj-BF16.gguf"), build_minimal_gguf("llama")).unwrap();

    let found = find_mmproj(&dir.join("model-Q4_K_M.gguf"));
    assert!(
      found.is_some(),
      "should match mmproj-BF16 to Q4_K_M model as fallback"
    );

    fs::remove_file(dir.join("mmproj-BF16.gguf")).unwrap();
    fs::write(dir.join("mmproj-Q4_K_M.gguf"), build_minimal_gguf("llama")).unwrap();
    let found_quant = find_mmproj(&dir.join("model-Q4_K_M.gguf"));
    assert_eq!(
      found_quant.unwrap().file_name().and_then(|s| s.to_str()),
      Some("mmproj-Q4_K_M.gguf"),
      "anonymous match should work when only one exists"
    );

    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_handles_unsloth_mismatched_quants_when_single() {
    let dir = temp_dir("mmproj-unsloth-mismatch");
    fs::write(
      dir.join("mimi-0.1.Q4_K_M.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    fs::write(dir.join("mmproj-f16.gguf"), build_minimal_gguf("llama")).unwrap();

    let found = find_mmproj(&dir.join("mimi-0.1.Q4_K_M.gguf"));
    assert!(
      found.is_some(),
      "should find mmproj-f16.gguf even if model is Q4_K_M when it's the only projector"
    );
    assert_eq!(
      found.unwrap().file_name().and_then(|s| s.to_str()),
      Some("mmproj-f16.gguf")
    );

    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_ignores_ambiguous_anonymous() {
    let dir = temp_dir("mmproj-ambiguous");
    fs::write(dir.join("qwen.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("mmproj.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("mmproj-f16.gguf"), build_minimal_gguf("llama")).unwrap();

    let found = find_mmproj(&dir.join("qwen.gguf"));
    assert!(
      found.is_none(),
      "should ignore all anonymous projectors when multiple exist to avoid ambiguity"
    );

    // If a base name match is added, it should still be found
    fs::write(dir.join("qwen-mmproj.gguf"), build_minimal_gguf("llama")).unwrap();
    let found_named = find_mmproj(&dir.join("qwen.gguf"));
    assert_eq!(
      found_named.unwrap().file_name().and_then(|s| s.to_str()),
      Some("qwen-mmproj.gguf"),
      "base name match should still win even with ambiguous anonymous ones"
    );

    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn test_canonical_base_normalization() {
    assert_eq!(canonical_base("Qwen2-7B-Q4_K_M"), "qwen2-7b");
    assert_eq!(canonical_base("Qwen2_7B_Q4_K_M"), "qwen2-7b");
    assert_eq!(canonical_base("model...name---_"), "model-name");
    assert_eq!(canonical_base("model-f16"), "model");
  }

  #[test]
  fn find_mmproj_handles_separator_mismatch() {
    let dir = temp_dir("mmproj-sep-mismatch");
    fs::write(
      dir.join("qwen2_7b_q4_k_m.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    fs::write(
      dir.join("qwen2-7b-mmproj.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();

    let found = find_mmproj(&dir.join("qwen2_7b_q4_k_m.gguf"));
    assert!(found.is_some());
    assert_eq!(
      found.unwrap().file_name().and_then(|s| s.to_str()),
      Some("qwen2-7b-mmproj.gguf")
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_uses_single_projector_with_generic_name() {
    // ggml-org's official multimodal GGUF repos ship the projector as a
    // generically-named `mmproj-model-f16.gguf` next to a descriptively
    // named model. Its stripped base (`model`) matches neither the
    // model name nor "empty", so name-matching alone misses it — the
    // single-model + single-projector fallback must still pair them.
    let dir = temp_dir("mmproj-generic-single");
    fs::write(
      dir.join("gemma-3-4b-it-Q4_K_M.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();
    fs::write(
      dir.join("mmproj-model-f16.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();

    let found = find_mmproj(&dir.join("gemma-3-4b-it-Q4_K_M.gguf"));
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mmproj-model-f16.gguf".to_string()),
      "single projector in a single-model folder must pair regardless of name"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mmproj_does_not_cross_assign_in_multi_model_dir() {
    // Flat folder with two models but only one projector, named for the
    // *other* model. Launching the projector-less model must not borrow
    // the neighbour's projector — the single-projector fallback is gated
    // on there being exactly one model in the directory.
    let dir = temp_dir("mmproj-multi-model");
    fs::write(dir.join("zephyr-7b.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(dir.join("gemma-3-4b.gguf"), build_minimal_gguf("llama")).unwrap();
    fs::write(
      dir.join("mmproj-gemma-3-4b-f16.gguf"),
      build_minimal_gguf("llama"),
    )
    .unwrap();

    // gemma gets its named projector...
    assert_eq!(
      find_mmproj(&dir.join("gemma-3-4b.gguf"))
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mmproj-gemma-3-4b-f16.gguf".to_string())
    );
    // ...but zephyr must not.
    assert_eq!(
      find_mmproj(&dir.join("zephyr-7b.gguf")),
      None,
      "must not cross-assign another model's projector"
    );
    fs::remove_dir_all(&dir).ok();
  }

  /// Write a projector GGUF beside a model, optionally advertising the
  /// vision / audio clip encoders.
  fn write_projector(path: &Path, vision: bool, audio: bool) {
    use crate::gguf::header::GgufValue;
    let mut b = crate::gguf::test_fixtures::FixtureBuilder::new();
    if vision {
      b = b.with_kv("clip.has_vision_encoder", GgufValue::Bool(true));
    }
    if audio {
      b = b.with_kv("clip.has_audio_encoder", GgufValue::Bool(true));
    }
    fs::write(path, b.build()).unwrap();
  }

  #[test]
  fn detect_multimodal_none_without_projector() {
    let dir = temp_dir("mm-none");
    fs::write(dir.join("model.gguf"), build_minimal_gguf("llama")).unwrap();
    assert_eq!(detect_multimodal(&dir.join("model.gguf")), None);
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn detect_multimodal_reads_vision_audio_and_omni_flags() {
    for (vision, audio) in [(true, false), (false, true), (true, true)] {
      let dir = temp_dir("mm-flags");
      fs::write(dir.join("model.gguf"), build_minimal_gguf("llama")).unwrap();
      write_projector(&dir.join("mmproj-model.gguf"), vision, audio);
      assert_eq!(
        detect_multimodal(&dir.join("model.gguf")),
        Some(Multimodal { vision, audio }),
        "clip flags vision={vision} audio={audio} must surface verbatim"
      );
      fs::remove_dir_all(&dir).ok();
    }
  }

  #[test]
  fn detect_multimodal_defaults_to_vision_without_clip_keys() {
    // Older vision-only mmproj files predate the audio split and ship no
    // `clip.has_*_encoder` keys; treat them as vision so the badge shows.
    let dir = temp_dir("mm-legacy");
    fs::write(dir.join("model.gguf"), build_minimal_gguf("llama")).unwrap();
    write_projector(&dir.join("mmproj-model.gguf"), false, false);
    assert_eq!(
      detect_multimodal(&dir.join("model.gguf")),
      Some(Multimodal {
        vision: true,
        audio: false
      })
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn detect_multimodal_reads_int_encoded_clip_flags() {
    // Some projector writers encode the clip flags as uint8 0/1 rather
    // than GGUF bool; an audio-only projector must still read as audio
    // (not fall through to the vision default).
    use crate::gguf::header::GgufValue;
    let dir = temp_dir("mm-int");
    fs::write(dir.join("model.gguf"), build_minimal_gguf("llama")).unwrap();
    let proj = crate::gguf::test_fixtures::FixtureBuilder::new()
      .with_kv("clip.has_vision_encoder", GgufValue::U8(0))
      .with_kv("clip.has_audio_encoder", GgufValue::U8(1))
      .build();
    fs::write(dir.join("mmproj-model.gguf"), proj).unwrap();
    assert_eq!(
      detect_multimodal(&dir.join("model.gguf")),
      Some(Multimodal {
        vision: false,
        audio: true
      })
    );
    fs::remove_dir_all(&dir).ok();
  }
}
