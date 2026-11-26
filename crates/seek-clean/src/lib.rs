//! Seek Decimator's removal engine. The only code in Seek that deletes files, and only ever
//! into the Recycle Bin. Windows only.

#![cfg(windows)]

// The admin step's checks assume 64-bit Windows (the 64-bit registry view, Program Files and Program
// Files (x86)), and a 32-bit build would refuse every admin removal. Better to fail at build time.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("Seek's cleanup runs on 64-bit Windows only");

pub mod apps;
pub mod claims;
pub mod confidence;
pub mod dupes;
pub mod elevated;
pub mod engine;
pub mod facts;
pub mod history;
pub mod leftovers;
pub mod model;
pub mod paths;
pub mod protect;
pub mod recycle;
pub mod remember;
mod reasons;
