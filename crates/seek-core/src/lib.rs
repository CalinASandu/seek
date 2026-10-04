//! Seek core: device model, in-memory index, query language, search engine and persistence.
//! Must stay free of OS-specific dependencies.

pub mod model;
pub mod types;

pub use model::*;
pub use types::*;
