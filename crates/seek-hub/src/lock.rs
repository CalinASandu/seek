//! One writer per data directory: `serve` holds an exclusive lock on `<data>/hub.lock` for its whole run, and
//! every command that writes the store without the daemon (offline `rotate-key`, `init`) must take it too.
//! The lock belongs to the open file, so the OS drops it when the process dies, even on a hard kill; a
//! leftover `hub.lock` file is harmless.

use std::fs::{self, File};
use std::io;
use std::path::Path;

const LOCK_FILE: &str = "hub.lock";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("the hub is running ({} is held by another process)", .0.display())]
    Held(std::path::PathBuf),
    #[error("cannot lock {}: {}", .0.display(), .1)]
    Io(std::path::PathBuf, io::Error),
}

/// Held while alive.
#[derive(Debug)]
pub struct DataLock {
    _file: File,
}

impl DataLock {
    /// Takes the lock without waiting; [`LockError::Held`] when another process has it.
    pub fn try_take(data: &Path) -> Result<DataLock, LockError> {
        let path = data.join(LOCK_FILE);
        let io_err = |e| LockError::Io(path.clone(), e);
        fs::create_dir_all(data).map_err(io_err)?;
        match open_locked(&path) {
            Ok(Some(file)) => Ok(DataLock { _file: file }),
            Ok(None) => Err(LockError::Held(path)),
            Err(e) => Err(io_err(e)),
        }
    }
}

/// `flock(LOCK_EX | LOCK_NB)`; `None` when another process holds it.
#[cfg(unix)]
fn open_locked(path: &Path) -> io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)?;
    // SAFETY: a valid open descriptor, owned by `file` for the lock's whole life.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(e)
    }
}

/// Windows (dev and tests): the file opened with no sharing; a second open fails with a sharing violation.
#[cfg(windows)]
fn open_locked(path: &Path) -> io::Result<Option<File>> {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(path) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(not(any(unix, windows)))]
fn open_locked(path: &Path) -> io::Result<Option<File>> {
    fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_taker_refused_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let held = DataLock::try_take(&data).unwrap();
        assert!(matches!(DataLock::try_take(&data), Err(LockError::Held(_))));
        drop(held);
        // a leftover hub.lock file does not wedge anything
        assert!(data.join(LOCK_FILE).exists());
        DataLock::try_take(&data).unwrap();
    }
}
