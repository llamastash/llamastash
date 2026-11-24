//! GGUF parser, metadata extraction, model identity, and memory estimator.
//!
//! Implementation note: we hand-rolled the header reader rather than depend
//! on `gguf-rs` because (a) we need the exact raw header bytes returned so
//! [`identity::compute`] can BLAKE3 them, and (b) we cap the read at
//! `DEFAULT_HEADER_CAP_BYTES` by default — many GGUF crates default to a
//! full-file mmap which is wasteful and unsuitable for a launcher that may
//! touch thousands of files during a single discovery sweep.

pub mod errors;
pub mod header;
pub mod identity;
pub mod memory;
pub mod metadata;

// So `cargo install` / release builds don't ship `FixtureBuilder` /
// `build_minimal_gguf` to consumers.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub mod test_fixtures;

pub use errors::{GgufError, GgufResult};
pub use header::{read_path, GgufHeader, GgufValue, HeaderReadOptions, ReadHeader, TensorInfo};
pub use identity::{compute as compute_identity, ModelId};
pub use memory::{estimate as estimate_memory, CacheType, EstimateOptions, MemoryEstimate};
pub use metadata::{summarise as summarise_metadata, ModeHint, ModelMetadata, Quant};
