//! Named-pipe protocol between seek-service and the Seek app.

pub mod client;
pub mod frame;
pub mod protocol;
pub mod server;

pub use client::{Client, MuxClient};
pub use frame::{read_frame, write_frame};
pub use protocol::*;
pub use server::create_server;
