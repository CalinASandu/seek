//! Seek sync between PCs: change journal, wire format, encrypted transport, pairing and the sync session.
//! Must stay free of OS-specific dependencies.

pub mod frame;
pub mod hub_session;
pub mod journal;
pub mod noise;
pub mod ownership;
pub mod pairing;
pub mod peers;
pub mod session;
pub mod transport;
pub mod wire;

pub use frame::{Frame, MAX_FRAME};
pub use hub_session::run_hub_session;
pub use journal::*;
pub use noise::Keys;
pub use session::{run_session, Outgoing, SessionCtl, SyncHost};
pub use wire::{CatalogPc, Msg, Refusal, Role, SYNC_PROTOCOL};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("wrong pairing code")]
    WrongCode,
    #[error("peer is not linked")]
    NotLinked,
    #[error("untrusted certificate: {0}")]
    Untrusted(String),
}
