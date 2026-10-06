//! Windows scanners for Seek: volume listing, NTFS MFT reader, directory walker,
//! USN journal follower and directory watcher.

use seek_core::{Checkpoint, DeviceIndex};

pub mod mft;
pub mod ntfs;
pub mod time;
pub mod usn;
pub mod volumes;
pub mod walk;
pub mod watch;
mod win;

pub use ntfs::scan_ntfs;
pub use time::filetime_to_unix_ms;
pub use usn::{parse_usn_buffer, FollowError, UsnFollower, UsnRecord};
pub use volumes::{list_volumes, scan_volume, VolumeInfo};
pub use walk::scan_walk;
pub use watch::DirWatcher;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("access denied")]
    AccessDenied,
    #[error("not found")]
    NotFound,
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt data: {0}")]
    Corrupt(String),
}

/// Output of a scan. The index root id is 5 for NTFS (MFT root) and 0 for the walker; `checkpoint` is
/// not yet copied into the index.
#[derive(Debug)]
pub struct ScanResult {
    pub index: DeviceIndex,
    pub checkpoint: Option<Checkpoint>,
    pub skipped: u64,
}
