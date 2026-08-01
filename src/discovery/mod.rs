//! Filesystem discovery: scan roots for `.gguf` files, group split-shard
//! sets, surface them on a streaming `mpsc::Receiver` so the TUI can
//! render rows as they arrive (origin: R1, R9).
//!
//! Module layout mirrors the responsibilities in the v1 plan:
//! - [`scanner`] — walk one or more roots and emit `DiscoveredModel`s.
//! - [`split_gguf`] — collapse `*-NNNNN-of-MMMMM.gguf` sets into one
//!   user-visible entry.
//!
//! Cache-aware enumerators (HuggingFace, Ollama, LM Studio), the
//! filesystem watcher, and the HuggingFace pull worker live in the
//! sibling modules.

pub mod catalog;
pub mod known_caches;
pub mod lm_studio;
pub mod metadata_cache;
pub mod ollama;
pub mod scanner;
pub mod shard_sizes;
pub mod split_gguf;
pub mod watcher;

pub use catalog::ModelCatalog;
pub use metadata_cache::MetadataCache;

use std::path::PathBuf;

use crate::gguf::metadata::ModelMetadata;

/// A model surfaced by discovery. Sent over the scanner's mpsc channel
/// so the TUI can render rows incrementally rather than waiting for
/// the whole scan to finish.
///
/// `metadata` is `None` when the GGUF header parse failed (truncated,
/// bad magic, unsupported version, …); discovery still surfaces the
/// row with a warning glyph rather than dropping the file (edge case:
/// "empty file with `.gguf` extension").
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
  /// Canonical absolute path to the launchable file. For split-shard
  /// sets this is shard 1; for singles it's the file itself.
  pub path: PathBuf,
  /// Directory the file (or shard set) lives in. Discovery groups by
  /// parent for the TUI's "Models / `dir`" rendering.
  pub parent: PathBuf,
  /// Source cache the file was discovered through, if any. `None` for
  /// user-configured `--model-path` roots that don't match a known
  /// cache layout.
  pub source: ModelSource,
  /// Parsed GGUF metadata, or `None` if the header parse failed.
  pub metadata: Option<ModelMetadata>,
  /// Diagnostic surfaced when `metadata` is `None`. The TUI renders a
  /// warning glyph and the user can hover to see the cause.
  pub parse_error: Option<String>,
  /// Sibling shards when this entry represents a split-GGUF set.
  /// Empty for a single-file model.
  pub split_siblings: Vec<PathBuf>,
  /// Source-supplied human label preferred over `path.file_stem()` by
  /// the TUI / CLI display layers. Ollama populates this with the
  /// resolved `<name>:<tag>` so users see `gemma4:e2b` instead of the
  /// content-addressed blob hash. `None` for sources where the file
  /// stem is already meaningful (HF cache, LM Studio, user paths).
  pub display_label: Option<String>,
  /// Multimodal capability of the model's auto-detected mmproj projector
  /// companion (vision / audio), or `None` when no projector was found.
  /// Derived from the projector GGUF's `clip.has_*_encoder` keys by the
  /// scanner (see [`scanner::find_mmproj`]). Surfaced after the model
  /// title in the TUI so the user knows a model launches multimodal.
  pub multimodal: Option<Multimodal>,
  /// The backends that can serve this model, **priority-ordered** (first = the
  /// auto-route default). Computed once at scan time from the same header parse
  /// that fills `metadata` (and cached), so the hot `list_models` path reads a
  /// precomputed value instead of re-reading the header on every call. Empty
  /// for registry sources and parse failures. The `list` "backend" column +
  /// right-pane badges show all of them; launch routing prefers the first
  /// available one. Determined generically via
  /// [`crate::backend::supported_backends_for`], so this field names no backend.
  pub supported_backends: Vec<String>,
  /// Path to a **separate MTP draft head** (`mtp-*.gguf` sibling) this model
  /// pairs with as its speculative drafter, or `None` when the
  /// model is embedded-MTP (draft layers in the base file) or has no head.
  /// Resolved once at scan time (see [`scanner::find_mtp_head`]) and cached, so
  /// the badge can predict MTP capability without a launch. Combined with
  /// `metadata.mtp` (the embedded count), it yields the model's MTP capability:
  /// `metadata.mtp.is_some() || mtp_head.is_some()`.
  pub mtp_head: Option<PathBuf>,
}

impl DiscoveredModel {
  /// The model's default (highest-priority) backend id, or `None` when unknown
  /// (registry source / parse failure). The `list` badge and launch routing
  /// treat this as the auto-route pick.
  pub fn default_backend(&self) -> Option<&str> {
    self.supported_backends.first().map(String::as_str)
  }
}

impl DiscoveredModel {
  /// Whether this model can run MTP speculative decoding: either it carries an
  /// embedded draft head (`metadata.mtp > 0`) or a separate head sibling was
  /// found on disk. The single capability predicate the catalog JSON and the
  /// TUI badge read.
  pub fn mtp_capable(&self) -> bool {
    self.metadata.as_ref().and_then(|m| m.mtp).is_some() || self.mtp_head.is_some()
  }
}

/// Capability glyph + description for MTP (multi-token prediction) speculative
/// decoding, rendered after a model title (like the multimodal glyphs) when the
/// model is MTP-capable. Single source of truth shared by the right-pane header
/// and the help-overlay legend so the two never drift.
pub const MTP_LEGEND: (char, &str) = ('↯', "MTP speculative decoding");

/// Multimodal modality a model's mmproj projector advertises. A
/// projector can be vision-only, audio-only, or both (an "omni" model),
/// so the two flags are independent rather than an enum. Derived from
/// the projector GGUF's `clip.has_vision_encoder` / `clip.has_audio_encoder`
/// metadata keys (llama.cpp clip convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Multimodal {
  pub vision: bool,
  pub audio: bool,
}

impl Multimodal {
  /// `(glyph, description)` for every modality the header can render —
  /// single source of truth shared by the right-pane title glyphs and
  /// the help-overlay Legend so the two never drift. Glyphs are
  /// single-cell BMP symbols matching the `status_icons` house style.
  pub const LEGEND: [(char, &'static str); 2] = [
    ('◉', "vision (multimodal projector)"),
    ('♪', "audio (multimodal projector)"),
  ];

  /// Glyphs for the modalities this projector actually advertises, in
  /// `LEGEND` order. Empty when neither flag is set.
  pub fn glyphs(&self) -> Vec<char> {
    let mut out = Vec::new();
    if self.vision {
      out.push(Self::LEGEND[0].0);
    }
    if self.audio {
      out.push(Self::LEGEND[1].0);
    }
    out
  }
}

/// Provenance label for a discovered model. The TUI groups by source
/// in the left pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelSource {
  /// A user-configured `--model-path` or config-level scan root.
  UserPath,
  /// A HuggingFace hub cache directory.
  HuggingFace,
  /// An Ollama blob/manifest cache.
  Ollama,
  /// An LM Studio models directory.
  LmStudio,
  /// A model the Lemonade umbrella serves from its own registry — no
  /// local GGUF file. Populated by the opt-in Lemonade discovery source.
  ///
  /// This is the single file-less-source special case, and the only place a
  /// backend is named for a discovery source. A generic
  /// `ModelSource::Backend(id)` refactor (pluggable, name-free) is the
  /// deferred option.
  Lemonade,
}

impl ModelSource {
  pub fn label(&self) -> &'static str {
    match self {
      ModelSource::UserPath => "user",
      ModelSource::HuggingFace => "huggingface",
      ModelSource::Ollama => "ollama",
      ModelSource::LmStudio => "lm-studio",
      ModelSource::Lemonade => "lemonade",
    }
  }

  /// Inverse of [`label`](Self::label): parse a source-label string back into
  /// its `ModelSource`, or `None` for an unrecognized label. The single place
  /// a discovery source label is read, so consumers map a label to a backend
  /// via [`backend_id`](Self::backend_id) without naming one.
  pub fn from_label(s: &str) -> Option<ModelSource> {
    match s {
      "user" => Some(ModelSource::UserPath),
      "huggingface" => Some(ModelSource::HuggingFace),
      "ollama" => Some(ModelSource::Ollama),
      "lm-studio" => Some(ModelSource::LmStudio),
      "lemonade" => Some(ModelSource::Lemonade),
      _ => None,
    }
  }

  /// The id of the backend that serves models from this source.
  ///
  /// Disk sources (user / HF / Ollama / LM Studio) are all local GGUF files
  /// served by the direct llama.cpp backend; the Lemonade source is served
  /// by the Lemonade managed-multiplexer.
  pub fn backend_id(&self) -> &'static str {
    match self {
      ModelSource::Lemonade => crate::backend::lemonade::LEMONADE_BACKEND_ID,
      ModelSource::UserPath
      | ModelSource::HuggingFace
      | ModelSource::Ollama
      | ModelSource::LmStudio => crate::backend::DEFAULT_BACKEND_ID,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn multimodal_glyphs_track_advertised_modalities() {
    // Glyphs come out in LEGEND order: vision first, audio second.
    let vision = Multimodal {
      vision: true,
      audio: false,
    };
    assert_eq!(vision.glyphs(), vec![Multimodal::LEGEND[0].0]);

    let audio = Multimodal {
      vision: false,
      audio: true,
    };
    assert_eq!(audio.glyphs(), vec![Multimodal::LEGEND[1].0]);

    let omni = Multimodal {
      vision: true,
      audio: true,
    };
    assert_eq!(
      omni.glyphs(),
      vec![Multimodal::LEGEND[0].0, Multimodal::LEGEND[1].0]
    );

    let none = Multimodal {
      vision: false,
      audio: false,
    };
    assert!(none.glyphs().is_empty());
  }

  #[test]
  fn model_source_label_and_backend_id_are_stable() {
    // Labels are the stable wire/display strings the TUI groups by.
    assert_eq!(ModelSource::UserPath.label(), "user");
    assert_eq!(ModelSource::HuggingFace.label(), "huggingface");
    assert_eq!(ModelSource::Ollama.label(), "ollama");
    assert_eq!(ModelSource::LmStudio.label(), "lm-studio");
    assert_eq!(ModelSource::Lemonade.label(), "lemonade");

    // Disk sources resolve to the direct llama.cpp backend; only the
    // Lemonade source routes to the managed multiplexer.
    for src in [
      ModelSource::UserPath,
      ModelSource::HuggingFace,
      ModelSource::Ollama,
      ModelSource::LmStudio,
    ] {
      assert_eq!(src.backend_id(), "llamacpp", "{src:?}");
    }
    assert_eq!(ModelSource::Lemonade.backend_id(), "lemonade");
  }
}
