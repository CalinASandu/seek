//! Seek core: device model, in-memory index, query language, search engine and persistence.
//! Must stay free of OS-specific dependencies.

pub mod index;
pub mod matcher;
pub mod model;
pub mod persist;
pub mod query;
pub mod registry;
pub mod search;
pub mod space;
pub mod synth;
pub mod types;

pub use index::*;
pub use model::*;
pub use persist::*;
pub use registry::*;
pub use search::*;
pub use space::*;
pub use synth::synth_index;
pub use types::*;

/// Errors surfaced by persistence and registry code. Corrupt input is always `Format`, never a panic.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid format: {0}")]
    Format(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
