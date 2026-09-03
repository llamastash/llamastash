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
/// [`find_mtp_head`].
///
/// Name-only, so it costs no I/O — but it recognises just the shapes no real
/// model uses (an `mtp` prefix, or `mtp` as the whole stem). A trailing
/// `-MTP-<quant>` says nothing on its own: published *models* wear it to
/// advertise embedded draft layers (`DeepSeek-V4-Pro-Qwen3.5-4B-MTP-Q2_K.gguf`)
/// and so do published *heads* (`DeepSeek-V4-Flash-MTP-Q4K-Q8_0-F32.gguf`).
/// Those go to [`is_mtp_head_file`], which asks the header.
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

/// Does the filename carry a delimited draft-head token at all? The cheap gate
/// in front of [`is_mtp_head_file`]'s header read: heads are published *as*
/// draft files, so a name with neither token never earns the extra open.
///
/// `dspark` rides here too — DeepSeek's DSpark support file drafts through the
/// same `--mtp` slot and its header is already head-shaped (`mtp.*` tensors,
/// no tokenizer), but it spells none of that in its name
/// (`DeepSeek-V4-Flash-DSpark-support-0731.gguf`). Without this it scanned as a
/// launchable, tokenizer-less model.
fn name_mentions_mtp(path: &Path) -> bool {
  static RE_MTP: OnceLock<Regex> = OnceLock::new();
  let re = RE_MTP.get_or_init(|| Regex::new(r"(?:^|[-._])(?:mtp|dspark)(?:$|[-._])").unwrap());
  path
    .file_stem()
    .and_then(|n| n.to_str())
    .map(|n| re.is_match(&n.to_lowercase()))
    .unwrap_or(false)
}

/// Does this GGUF's **header** say it is a draft head?
///
/// Covers the shape a name cannot resolve: the DeepSeek-V4 head, which carries
/// only `mtp.*` tensors and no tokenizer (not a standalone model, which is
/// exactly what makes it unlaunchable) and declares its own
/// `general.architecture` of `deepseek4_mtp_support`.
///
/// The Gemma-4 head is a different animal and is **not** matched here: it is a
/// 4-layer model in its own right (`gemma4-assistant` arch, its own tokenizer,
/// ordinary `blk.*` + `nextn.*` tensors), indistinguishable by header from a
/// small standalone model. Publishers name that one `mtp-*.gguf`, so
/// [`is_mtp_companion`] claims it first and this never has to guess.
///
/// A header that won't parse reads as "not a head": misfiling a real model as
/// a companion hides it from the catalog, the worse of the two failures.
fn header_says_mtp_head(path: &Path) -> bool {
  let Ok(read) = read_path(path, HeaderReadOptions::default()) else {
    return false;
  };
  is_mtp_head_header(&read.header)
}

/// The header-shape predicate behind [`header_says_mtp_head`], split out so it
/// can be exercised against a built fixture without a file on disk.
pub(crate) fn is_mtp_head_header(header: &crate::gguf::header::GgufHeader) -> bool {
  if header
    .string(&["general.architecture"])
    .is_some_and(|a| a.ends_with(MTP_ARCH_SUFFIX))
  {
    return true;
  }
  let has_tokenizer = header.string(&["tokenizer.ggml.model"]).is_some();
  !has_tokenizer
    && !header.tensors.is_empty()
    && header
      .tensors
      .iter()
      .all(|t| t.name.starts_with(MTP_TENSOR_PREFIX))
}

/// Architecture suffix a draft head declares instead of the parent's arch
/// (`deepseek4` → `deepseek4_mtp_support`). Also the pairing key: strip it and
/// what remains is the arch of the model the head drafts for.
const MTP_ARCH_SUFFIX: &str = "_mtp_support";

/// Tensor namespace a draft head's weights live under.
const MTP_TENSOR_PREFIX: &str = "mtp.";

/// Is this `.gguf` an MTP draft head, by name where the name is conclusive and
/// by header where it isn't? The header read is gated on the name mentioning
/// `mtp` at all, so an ordinary catalog costs no extra opens.
pub(crate) fn is_mtp_head_file(path: &Path) -> bool {
  if is_mtp_companion(path) {
    return true;
  }
  name_mentions_mtp(path) && header_says_mtp_head(path)
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
fn companion_names(paths: &[PathBuf]) -> Vec<String> {
  paths
    .iter()
    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    .collect()
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
/// Resolution order — the header rule first, then [`find_mmproj`]'s three name
/// rules so head naming stays as forgiving as projector naming (repos label
/// heads inconsistently):
/// 0. a head declaring `<model arch>_mtp_support` wins outright — quant-blind,
///    so it still pairs in a folder holding several quants of the same model;
/// 1. else an MTP head whose quant-stripped base equals the model's;
/// 2. else, a lone model + lone head in the directory pair regardless of name;
/// 3. else, a single anonymous `mtp.gguf` catch-all is used; anything more
///    ambiguous yields `None` (the user can pair a head explicitly).
pub fn find_mtp_head(model_path: &Path, model_arch: Option<&str>) -> Option<PathBuf> {
  // The arch a head must declare to draft for this model (`deepseek4` →
  // `deepseek4_mtp_support`).
  let head_arch = model_arch.map(|a| format!("{a}{MTP_ARCH_SUFFIX}"));
  find_draft_head(model_path, head_arch.as_deref())
}

/// The two companion kinds (`mmproj` projectors, MTP draft heads) differ only
/// in recognition and name-stripping, so they share one search.
struct CompanionKind<'a> {
  is_companion: fn(&Path) -> bool,
  strip_markers: fn(&str) -> String,
  /// Draft heads only: arch a companion may declare to claim this model
  /// outright. `None` skips the header read.
  want_arch: Option<&'a str>,
  label: &'static str,
}

/// The `mmproj` projector paired with `model_path`. `None` when there is none,
/// or when several are equally plausible and the user should pass `--mmproj`.
pub fn find_mmproj(model_path: &Path) -> Option<PathBuf> {
  // Launching a projector directly has no projector of its own.
  if is_projector_companion(model_path) {
    return None;
  }
  find_companion(
    model_path,
    &CompanionKind {
      is_companion: is_projector_companion,
      strip_markers: strip_mmproj_markers,
      want_arch: None,
      label: "mmproj",
    },
  )
}

/// [`find_mtp_head`] with the head arch supplied verbatim rather than derived:
/// DSpark's support GGUF declares `deepseek4-dspark` yet drafts for
/// `deepseek4` through the same slot.
pub fn find_draft_head(model_path: &Path, head_arch: Option<&str>) -> Option<PathBuf> {
  // A head file has no head of its own.
  if is_mtp_head_file(model_path) {
    return None;
  }
  find_companion(
    model_path,
    &CompanionKind {
      is_companion: is_mtp_head_file,
      strip_markers: strip_mtp_markers,
      want_arch: head_arch,
      label: "MTP draft head",
    },
  )
}

/// Companions found in one directory, classified but not yet chosen. Tiers are
/// applied by the caller so a hit in an early directory cannot outrank a
/// stronger hit in a later one.
#[derive(Default)]
struct Candidates {
  arch: Vec<PathBuf>,
  base: Vec<PathBuf>,
  anonymous: Vec<PathBuf>,
  all: Vec<PathBuf>,
  /// Launchable models here, as shard *sets*.
  model_labels: std::collections::BTreeSet<String>,
}

impl Candidates {
  fn merge(&mut self, o: Candidates) {
    self.arch.extend(o.arch);
    self.base.extend(o.base);
    self.anonymous.extend(o.anonymous);
    self.all.extend(o.all);
    self.model_labels.extend(o.model_labels);
  }
}

/// A companion for `model_path`: its own directory first, then the rest of the
/// HF snapshot, since repos routinely put weights and companions in separate
/// subdirectories.
///
/// Widening is deliberately narrow. It needs a real `models--owner--repo`
/// cache tree, and it stops unless every model in that snapshot shares one
/// canonical base: a repo shipping `9B/` and `27B/` beside one `MTP/` would
/// otherwise hand the 9B head to the 27B model, which drafts garbage. Quants
/// of one model share a base, so the common single-model repo still pairs.
fn find_companion(model_path: &Path, kind: &CompanionKind<'_>) -> Option<PathBuf> {
  let own = collect_candidates(model_path.parent()?, model_path, kind);
  if let Some(hit) = pick(&own, true, kind, model_path) {
    return Some(hit);
  }

  let mut wide = Candidates::default();
  let dirs = hf_snapshot_sibling_dirs(model_path);
  if !dirs.is_empty() {
    // Cheap listing first: the guard rejects most multi-model snapshots, and
    // collecting would header-read every arch candidate before we threw it away.
    let mut bases: std::collections::BTreeSet<String> = own
      .model_labels
      .iter()
      .chain(
        dirs
          .iter()
          .flat_map(|d| dir_model_labels(d))
          .collect::<Vec<_>>()
          .iter(),
      )
      .map(|l| label_base(l))
      .collect();
    bases.remove("");
    if bases.len() > 1 {
      log::debug!(
        "{}: snapshot holds {} distinct models; not widening the {} search",
        model_path.display(),
        bases.len(),
        kind.label
      );
    } else {
      for dir in &dirs {
        wide.merge(collect_candidates(dir, model_path, kind));
      }
      if let Some(hit) = pick(&wide, false, kind, model_path) {
        return Some(hit);
      }
    }
  }

  // Reached on every empty-handed path, the flat-directory one included: a
  // silent `None` reads as "this model has no companion" when the truth is
  // that several were equally plausible.
  let mut seen: Vec<PathBuf> = own.all.clone();
  seen.extend(wide.all.iter().cloned());
  if !seen.is_empty() {
    log::warn!(
      "{}: {} {} candidates found but none match {}; pass one explicitly: {:?}",
      model_path.display(),
      seen.len(),
      kind.label,
      crate::util::paths::model_file_label(model_path),
      companion_names(&seen),
    );
  }
  None
}

/// Quant-stripped base of a model filename, for "is this the same model".
fn label_base(label: &str) -> String {
  canonical_base(
    Path::new(label)
      .file_stem()
      .and_then(|s| s.to_str())
      .unwrap_or(label),
  )
}

/// Launchable models in `dir` as shard sets. Name-only, no header reads, so
/// the widen guard costs one `read_dir` per directory.
fn dir_model_labels(dir: &Path) -> std::collections::BTreeSet<String> {
  let mut out = std::collections::BTreeSet::new();
  let Ok(entries) = std::fs::read_dir(dir) else {
    return out;
  };
  for e in entries.flatten() {
    let path = e.path();
    if !path.is_file() {
      continue;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
      continue;
    };
    if !name.to_lowercase().ends_with(".gguf") {
      continue;
    }
    if is_projector_companion(&path) || is_mtp_head_file(&path) {
      continue;
    }
    out.insert(crate::util::paths::model_file_label(&path));
  }
  out
}

/// Choose from classified candidates. `own_dir` enables the two unnamed tiers:
/// they rest on "the only model beside the only companion", which a companion
/// directory cannot show, and across directories would pair on nothing but
/// proximity.
fn pick(
  c: &Candidates,
  own_dir: bool,
  kind: &CompanionKind<'_>,
  model_path: &Path,
) -> Option<PathBuf> {
  let first = |v: &Vec<PathBuf>| {
    let mut v = v.clone();
    v.sort();
    v.into_iter().next()
  };
  // 0. A companion declaring the architecture it serves.
  if !c.arch.is_empty() {
    return first(&c.arch);
  }
  // 1. Name match.
  if !c.base.is_empty() {
    if c.base.len() > 1 {
      let mut sorted = c.base.clone();
      sorted.sort();
      log::warn!(
        "{}: {} {} candidates match by name; using {:?}: {:?}",
        model_path.display(),
        sorted.len(),
        kind.label,
        sorted[0].file_name().unwrap_or_default().to_string_lossy(),
        companion_names(&sorted),
      );
    }
    return first(&c.base);
  }
  if !own_dir {
    // Cross-directory: a lone companion in the snapshot, now that every model
    // in it is known to be the same one.
    return (c.all.len() == 1).then(|| c.all[0].clone());
  }
  // 2. The only model beside the only companion.
  if c.model_labels.len() == 1 && c.all.len() == 1 {
    return first(&c.all);
  }
  // 3. A lone anonymous catch-all, else genuinely ambiguous.
  if c.anonymous.len() == 1 {
    return first(&c.anonymous);
  }
  None
}

/// The rest of the HF snapshot `model_path` sits in: revision root plus its
/// immediate subdirectories, minus the model's own.
///
/// Requires a real `models--owner--repo/snapshots/<rev>/` tree. Keying on a
/// directory merely *named* `snapshots` would widen inside any user scan root
/// that happens to use the name.
fn hf_snapshot_sibling_dirs(model_path: &Path) -> Vec<PathBuf> {
  let Some(parent) = model_path.parent() else {
    return Vec::new();
  };
  let snapshots = std::ffi::OsStr::new("snapshots");
  let root = parent.ancestors().find(|a| {
    a.parent().and_then(|p| p.file_name()) == Some(snapshots)
      && a.ancestors().any(|x| {
        x.file_name()
          .and_then(|n| n.to_str())
          .is_some_and(|n| n.starts_with("models--") && n.contains("--"))
      })
  });
  let Some(root) = root else {
    return Vec::new();
  };
  let mut dirs: Vec<PathBuf> = Vec::new();
  if root != parent {
    dirs.push(root.to_path_buf());
  }
  if let Ok(entries) = std::fs::read_dir(root) {
    for e in entries.flatten() {
      let p = e.path();
      if p.is_dir() && p != parent {
        dirs.push(p);
      }
    }
  }
  dirs.sort();
  dirs
}

/// Classify every `.gguf` in one directory against `model_path`.
fn collect_candidates(dir: &Path, model_path: &Path, kind: &CompanionKind<'_>) -> Candidates {
  let mut c = Candidates::default();
  let Some(model_filename) = model_path.file_name().and_then(|n| n.to_str()) else {
    return c;
  };
  // Compare on the collapsed name: a split set is one model, and its
  // `-NNNNN-of-NNNNN` suffix is not part of the name a companion is published
  // under, so `foo-Q4_K_M-00001-of-00002` has to match `mtp-foo`.
  let model_label = crate::util::paths::model_file_label(model_path);
  let model_base = canonical_base(
    Path::new(&model_label)
      .file_stem()
      .and_then(|s| s.to_str())
      .unwrap_or(&model_label),
  );

  let Ok(entries) = std::fs::read_dir(dir) else {
    return c;
  };
  for entry in entries.flatten() {
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
    if !(kind.is_companion)(&path) {
      // A companion of the *other* kind is not a model either.
      if !is_projector_companion(&path) && !is_mtp_head_file(&path) {
        c.model_labels
          .insert(crate::util::paths::model_file_label(&path));
      }
      continue;
    }
    if name == model_filename {
      continue;
    }
    if let Some(want) = kind.want_arch {
      if read_path(&path, HeaderReadOptions::default())
        .ok()
        .and_then(|r| {
          r.header
            .string(&["general.architecture"])
            .map(str::to_string)
        })
        .is_some_and(|a| a == want)
      {
        c.arch.push(path.clone());
      }
    }
    let base = canonical_base(&(kind.strip_markers)(name));
    if base.is_empty() {
      c.anonymous.push(path.clone());
    } else if base == model_base {
      c.base.push(path.clone());
    }
    c.all.push(path);
  }
  c
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
          && !is_mtp_head_file(p)
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
      // Pairing keys on the model's own arch, so the head lookup reads it off
      // the parse we already did rather than opening the file twice.
      let arch = parsed.as_ref().ok().and_then(|r| {
        r.header
          .string(&["general.architecture"])
          .map(str::to_string)
      });
      let mtp_head = find_mtp_head(&path_for_parse, arch.as_deref());
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
    let found = find_mtp_head(&dir.join("gemma-4.gguf"), Some("gemma4"));
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mtp-gemma-4.gguf".to_string()),
      "name-matched head must pair"
    );
    // A head file has no head of its own.
    assert_eq!(
      find_mtp_head(&dir.join("mtp-gemma-4.gguf"), Some("gemma4")),
      None
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn split_model_name_matches_a_head_published_without_the_shard_suffix() {
    // A head is published against the model's name, never against shard 1's
    // filename, so the comparison has to collapse `-NNNNN-of-NNNNN` first.
    // A second model in the folder keeps the lone-pair tier from covering it,
    // so only the name match can succeed here.
    let dir = temp_dir("mtp-split-name");
    for i in 1..=2 {
      fs::write(
        dir.join(format!("gemma-4-Q4_K_M-0000{i}-of-00002.gguf")),
        build_minimal_gguf("gemma4"),
      )
      .unwrap();
    }
    fs::write(dir.join("other-model.gguf"), build_minimal_gguf("gemma4")).unwrap();
    fs::write(
      dir.join("mtp-gemma-4-Q8_0.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    let found = find_mtp_head(
      &dir.join("gemma-4-Q4_K_M-00001-of-00002.gguf"),
      Some("gemma4"),
    );
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mtp-gemma-4-Q8_0.gguf".to_string()),
      "shard suffix must not defeat the name match"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn a_split_set_counts_as_one_model_for_the_lone_pair_tier() {
    // Four shards are one model. Counting files made the count 4, so a head
    // whose name did not match could never pair with a split model.
    let dir = temp_dir("mtp-split-count");
    for i in 1..=4 {
      fs::write(
        dir.join(format!("gemma-4-Q4_K_M-0000{i}-of-00004.gguf")),
        build_minimal_gguf("gemma4"),
      )
      .unwrap();
    }
    fs::write(
      dir.join("mtp-unrelated-name.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    let found = find_mtp_head(
      &dir.join("gemma-4-Q4_K_M-00001-of-00004.gguf"),
      Some("gemma4"),
    );
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mtp-unrelated-name.gguf".to_string()),
      "a split set is one model, so the lone-pair tier must fire"
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn head_pairs_from_a_companion_directory_of_the_same_snapshot() {
    // HF repos lay out one directory per quant plus a companion directory, so
    // the head is never beside the weights. Only paths inside a
    // `snapshots/<rev>/` tree widen, and only to that tree.
    let root = temp_dir("mtp-snapshot");
    let snap = root
      .join("models--org--repo")
      .join("snapshots")
      .join("abc123");
    let quant = snap.join("UD-Q4_K_XL");
    let mtp = snap.join("MTP");
    fs::create_dir_all(&quant).unwrap();
    fs::create_dir_all(&mtp).unwrap();
    for i in 1..=2 {
      fs::write(
        quant.join(format!("gemma-4-UD-Q4_K_XL-0000{i}-of-00002.gguf")),
        build_minimal_gguf("gemma4"),
      )
      .unwrap();
    }
    fs::write(
      mtp.join("mtp-gemma-4-Q8_0.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    let found = find_mtp_head(
      &quant.join("gemma-4-UD-Q4_K_XL-00001-of-00002.gguf"),
      Some("gemma4"),
    );
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mtp-gemma-4-Q8_0.gguf".to_string()),
      "a head in a sibling snapshot directory must pair"
    );
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn a_name_match_in_a_later_directory_beats_a_lone_companion_in_an_earlier_one() {
    // Tiers are global across the snapshot. Returning the first hit from the
    // first directory let `Extras/` (sorted first, one head, no name relation)
    // outrank the real `MTP/` match and hand the model a foreign drafter.
    let root = temp_dir("mtp-tier-order");
    let snap = root.join("models--org--repo").join("snapshots").join("r1");
    let quant = snap.join("Quant");
    for d in [&quant, &snap.join("Extras"), &snap.join("MTP")] {
      fs::create_dir_all(d).unwrap();
    }
    for i in 1..=2 {
      fs::write(
        quant.join(format!("gemma-4-Q4_K_M-0000{i}-of-00002.gguf")),
        build_minimal_gguf("gemma4"),
      )
      .unwrap();
    }
    fs::write(
      snap.join("Extras").join("mtp-random.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    fs::write(
      snap.join("MTP").join("mtp-gemma-4-Q8_0.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    let found = find_mtp_head(
      &quant.join("gemma-4-Q4_K_M-00001-of-00002.gguf"),
      Some("gemma4"),
    );
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mtp-gemma-4-Q8_0.gguf".to_string()),
      "the name match must win over a lone head in an earlier directory"
    );
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn a_snapshot_holding_two_models_does_not_widen() {
    // One repo, two models, one shared head: pairing it onto either is a guess
    // that drafts garbage. Only the model's own directory may answer.
    let root = temp_dir("mtp-two-models");
    let snap = root.join("models--org--repo").join("snapshots").join("r1");
    let nine = snap.join("9B");
    let twentyseven = snap.join("27B");
    for d in [&nine, &twentyseven, &snap.join("MTP")] {
      fs::create_dir_all(d).unwrap();
    }
    fs::write(
      nine.join("gemma-4-9B-Q4_K_M.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    fs::write(
      twentyseven.join("gemma-4-27B-Q4_K_M.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    fs::write(
      snap.join("MTP").join("mtp-gemma-4-9B.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    assert_eq!(
      find_mtp_head(&twentyseven.join("gemma-4-27B-Q4_K_M.gguf"), Some("gemma4")),
      None,
      "must not hand the 9B head to the 27B model"
    );
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn widening_needs_a_real_hf_cache_tree_not_just_a_snapshots_directory() {
    // A user scan root that happens to contain a directory called `snapshots`
    // is not an HF repo and carries no model boundary.
    let root = temp_dir("mtp-fake-snapshots");
    let snap = root.join("snapshots").join("collection");
    let a = snap.join("model-a");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(snap.join("model-b")).unwrap();
    fs::write(a.join("gemma-4.gguf"), build_minimal_gguf("gemma4")).unwrap();
    fs::write(
      snap.join("model-b").join("mtp-other.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    assert_eq!(
      find_mtp_head(&a.join("gemma-4.gguf"), Some("gemma4")),
      None,
      "a directory merely named `snapshots` must not enable widening"
    );
    fs::remove_dir_all(&root).ok();
  }

  #[test]
  fn the_widened_search_does_not_escape_a_plain_models_directory() {
    // Outside an HF snapshot there is no boundary to widen within, so a head
    // in an unrelated sibling folder must stay unpaired rather than be
    // guessed at.
    let root = temp_dir("mtp-no-widen");
    let a = root.join("model-a");
    let b = root.join("model-b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("gemma-4.gguf"), build_minimal_gguf("gemma4")).unwrap();
    fs::write(
      b.join("mtp-something-else.gguf"),
      build_minimal_gguf("gemma4"),
    )
    .unwrap();
    assert_eq!(
      find_mtp_head(&a.join("gemma-4.gguf"), Some("gemma4")),
      None,
      "must not pair across unrelated directories"
    );
    fs::remove_dir_all(&root).ok();
  }

  /// A draft head shaped like antirez's published DeepSeek-V4 head: its own
  /// `_mtp_support` arch, `mtp.0.*` tensors, and no tokenizer.
  fn build_mtp_head_gguf(parent_arch: &str) -> Vec<u8> {
    use crate::gguf::test_fixtures::FixtureBuilder;
    FixtureBuilder::new()
      .with_arch(&format!("{parent_arch}{MTP_ARCH_SUFFIX}"))
      .with_tensor("mtp.0.hc_head_fn.weight", &[16384, 4], 0)
      .with_tensor("mtp.0.attn_q_a.weight", &[4096, 1024], 12)
      .build()
  }

  #[test]
  fn dspark_support_file_is_a_head_not_a_launchable_model() {
    // antirez's DSpark support GGUF: head-shaped header (`mtp.*` tensors, no
    // tokenizer) but its own `deepseek4-dspark` arch and no `mtp` token in the
    // name, so the old name gate skipped the header read and it scanned as a
    // standalone, tokenizer-less model.
    let dir = temp_dir("dspark-class");
    let support = dir.join("DeepSeek-V4-Flash-DSpark-support-0731.gguf");
    let model = dir.join("DeepSeek-V4-Flash-IQ2XXS-chat-v2-imatrix-0731.gguf");
    fs::write(
      &support,
      crate::gguf::test_fixtures::FixtureBuilder::new()
        .with_arch("deepseek4-dspark")
        .with_tensor("mtp.0.attn_q_a.weight", &[4096, 1024], 12)
        .with_tensor("mtp.0.attn_norm.weight", &[4096], 0)
        .build(),
    )
    .unwrap();
    fs::write(&model, build_minimal_gguf("deepseek4")).unwrap();

    assert!(
      !is_mtp_companion(&support),
      "the name carries no mtp token — only the header is conclusive"
    );
    assert!(
      is_mtp_head_file(&support),
      "DSpark support must classify as a draft head, not a launchable model"
    );
    // Pairs through the explicit-arch lookup: it declares `deepseek4-dspark`,
    // not the `deepseek4_mtp_support` shape `find_mtp_head` derives.
    assert_eq!(
      find_draft_head(&model, Some("deepseek4-dspark"))
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("DeepSeek-V4-Flash-DSpark-support-0731.gguf".to_string())
    );
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn mtp_head_is_classified_by_header_not_by_name() {
    // The published pair that name matching cannot separate: both wear
    // `-MTP-<quant>`, one is a head, the other a full model advertising
    // embedded draft layers.
    let dir = temp_dir("mtp-header-class");
    let head = dir.join("DeepSeek-V4-Flash-MTP-Q4K-Q8_0-F32.gguf");
    let model = dir.join("DeepSeek-V4-Pro-Qwen3.5-4B-MTP-Q2_K.gguf");
    fs::write(&head, build_mtp_head_gguf("deepseek4")).unwrap();
    fs::write(&model, build_minimal_gguf("qwen35")).unwrap();

    assert!(
      !is_mtp_companion(&head),
      "name alone cannot tell them apart"
    );
    assert!(is_mtp_head_file(&head), "header must classify the head");
    assert!(
      !is_mtp_head_file(&model),
      "a model advertising embedded MTP must stay launchable"
    );

    let listed = collect_gguf_paths(&dir, &[]);
    assert_eq!(listed.len(), 1, "head excluded, model kept: {listed:?}");
    assert!(listed[0].ends_with("DeepSeek-V4-Pro-Qwen3.5-4B-MTP-Q2_K.gguf"));
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn mtp_head_header_shape_without_the_arch_marker() {
    use crate::gguf::header::read_reader;
    use crate::gguf::test_fixtures::FixtureBuilder;
    // No `_mtp_support` arch: mtp-only tensors and no tokenizer still say head.
    let bare = FixtureBuilder::new()
      .with_arch("gemma4")
      .with_tensor("mtp.0.attn_norm.weight", &[24], 0)
      .build();
    let bare_read = read_reader(&bare[..], HeaderReadOptions::default()).unwrap();
    assert!(is_mtp_head_header(&bare_read.header));

    // A real model carries a tokenizer and non-`mtp.` tensors.
    let model = FixtureBuilder::new()
      .with_arch("gemma4")
      .with_tokenizer_model("gpt2")
      .with_tensor("blk.0.attn_q.weight", &[4096, 4096], 12)
      .build();
    let model_read = read_reader(&model[..], HeaderReadOptions::default()).unwrap();
    assert!(!is_mtp_head_header(&model_read.header));
  }

  #[test]
  fn find_mtp_head_pairs_on_arch_across_quants() {
    // Several quants of one model plus one head: the quant-stripped name match
    // fails (the head's base is its own), so the `_mtp_support` arch pairs it.
    let dir = temp_dir("mtp-arch-pair");
    let a = dir.join("DeepSeek-V4-Flash-IQ2XXS-chat-v2-imatrix.gguf");
    let b = dir.join("DeepSeek-V4-Flash-Q4KExperts-chat-v2.gguf");
    fs::write(&a, build_minimal_gguf("deepseek4")).unwrap();
    fs::write(&b, build_minimal_gguf("deepseek4")).unwrap();
    fs::write(
      dir.join("DeepSeek-V4-Flash-MTP-Q4K-Q8_0-F32.gguf"),
      build_mtp_head_gguf("deepseek4"),
    )
    .unwrap();

    for model in [&a, &b] {
      let found = find_mtp_head(model, Some("deepseek4"));
      assert_eq!(
        found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
        Some("DeepSeek-V4-Flash-MTP-Q4K-Q8_0-F32.gguf".to_string()),
        "arch match must pair {} regardless of quant",
        model.display()
      );
    }
    // A head drafting for another arch must not pair.
    assert_eq!(find_mtp_head(&a, Some("qwen35")), None);
    fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn find_mtp_head_none_when_absent() {
    let dir = temp_dir("mtp-absent");
    fs::write(dir.join("qwen35.gguf"), build_minimal_gguf("qwen35")).unwrap();
    assert_eq!(
      find_mtp_head(&dir.join("qwen35.gguf"), Some("qwen35")),
      None
    );
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
    let found = find_mtp_head(&dir.join("gemma-4-it-Q4_K_M.gguf"), Some("gemma4"));
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
  fn mmproj_pairs_from_the_snapshot_root_of_a_quant_subdir() {
    // unsloth-style layout: weights under a per-quant directory, the projector
    // at the snapshot root. A parent-only search left these models with
    // multimodal: null despite the projector sitting on disk.
    let root = temp_dir("mmproj-snapshot");
    let snap = root
      .join("models--org--repo")
      .join("snapshots")
      .join("abc123");
    let quant = snap.join("UD-Q4_K_XL");
    fs::create_dir_all(&quant).unwrap();
    for i in 1..=2 {
      fs::write(
        quant.join(format!("gemma-4-UD-Q4_K_XL-0000{i}-of-00002.gguf")),
        build_minimal_gguf("gemma4"),
      )
      .unwrap();
    }
    fs::write(snap.join("mmproj-F16.gguf"), build_minimal_gguf("gemma4")).unwrap();
    let found = find_mmproj(&quant.join("gemma-4-UD-Q4_K_XL-00001-of-00002.gguf"));
    assert_eq!(
      found.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
      Some("mmproj-F16.gguf".to_string()),
      "a projector at the snapshot root must pair with a model in a quant subdir"
    );
    fs::remove_dir_all(&root).ok();
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
