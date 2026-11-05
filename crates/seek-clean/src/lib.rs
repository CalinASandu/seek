//! Seek Decimator's removal engine. The only code in Seek that deletes files, and only ever
//! into the Recycle Bin. Windows only.

#![cfg(windows)]

pub mod engine;
pub mod facts;
pub mod model;
pub mod paths;
pub mod protect;
pub mod recycle;
mod reasons;
