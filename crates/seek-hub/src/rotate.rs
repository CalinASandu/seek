//! Key rotation (spec §5), safe against a crash at any point:
//!
//! 1. the new key is saved durably as `hub.key.new` (next to `hub.key`, which stays);
//! 2. every store file is re-encrypted with it, one `crypt::write` (temp + rename) at a time, so each file
//!    is always whole and under exactly one of the two keys, which its header names;
//! 3. `hub.key.new` is renamed over `hub.key`.
//!
//! Until step 3 both keys are on disk and every file is readable with the one its header names; the next
//! start ([`open_key`]) or `rotate-key` finishes steps 2 and 3. After step 3 only the new key is left and
//! every file uses it.

use std::io;
use std::path::Path;

use crate::crypt::{self, CryptError, HubKey, NEW_KEY_FILE};
use crate::store::{self, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error(transparent)]
    Key(#[from] CryptError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<io::Error> for KeyError {
    fn from(e: io::Error) -> Self {
        KeyError::Key(CryptError::Io(e))
    }
}

/// Re-encrypts every store file in `dir` that uses `from` with `to`; files already under `to` are left as
/// they are, so running it again after an interruption picks up where it stopped. Any other file (a third
/// key, damaged, not encrypted) stops it. Returns how many files were rewritten.
pub fn rekey_dir(dir: &Path, from: &HubKey, to: &HubKey) -> Result<usize, StoreError> {
    let unreadable = |path: &Path, e: CryptError| match e {
        CryptError::Io(e) => StoreError::Io(e),
        other => {
            tracing::error!("{}: {other}", path.display());
            StoreError::Unreadable(path.to_path_buf())
        }
    };
    let mut rewritten = 0;
    for path in store::files(dir)? {
        let id = crypt::key_id_of(&path).map_err(|e| unreadable(&path, e))?;
        if id == to.id() {
            continue;
        }
        let plain = crypt::read(from, &path).map_err(|e| unreadable(&path, e))?;
        crypt::write(to, &path, &plain)?;
        rewritten += 1;
    }
    Ok(rewritten)
}

/// The key to open the store in `data` with. When a rotation was interrupted (`hub.key.new` is there), it
/// is finished first: the files still under the old key are re-encrypted, then the new key replaces the old
/// one. If `keys` is read-only that last rename fails; the hub then runs with `hub.key.new` (every file uses
/// it) and says how to finish.
pub fn open_key(keys: &Path, data: &Path) -> Result<HubKey, KeyError> {
    let key = HubKey::load(keys)?;
    let Some(new) = HubKey::load_new(keys)? else {
        return Ok(key);
    };
    let n = rekey_dir(data, &key, &new)?;
    tracing::info!("finished an interrupted key rotation ({n} files re-encrypted)");
    match HubKey::commit_new(keys) {
        Ok(()) => tracing::info!("hub.key replaced by {NEW_KEY_FILE}"),
        Err(e) => tracing::warn!(
            "cannot replace hub.key with {NEW_KEY_FILE} ({e}); running with {NEW_KEY_FILE}. {}",
            READ_ONLY_HINT
        ),
    }
    Ok(new)
}

/// Shown when `/keys` is not writable.
pub const READ_ONLY_HINT: &str = "Rotating the key needs /keys writable, with the hub stopped: docker compose stop seek-hub; \
     docker run --rm -v \"$PWD/data:/data\" -v \"$PWD/keys:/keys\" ghcr.io/calinasandu/seek-hub:<version> rotate-key; \
     docker compose start seek-hub";

/// The key a rotation is moving to: the unfinished one in `keys` if any, else a new one saved durably as
/// `hub.key.new` (step 1). `true` when it resumes an unfinished rotation.
pub fn begin(keys: &Path) -> Result<(HubKey, bool), KeyError> {
    if let Some(new) = HubKey::load_new(keys)? {
        return Ok((new, true));
    }
    let new = HubKey::generate();
    new.save_new(keys)?;
    Ok((new, false))
}

/// A whole rotation with no hub running (`rotate-key` when the daemon is not up). Returns how many files
/// were re-encrypted and whether an unfinished rotation was resumed.
pub fn rotate_offline(keys: &Path, data: &Path) -> Result<(usize, bool), KeyError> {
    let key = HubKey::load(keys)?;
    let (new, resumed) = begin(keys)?;
    let n = rekey_dir(data, &key, &new)?;
    HubKey::commit_new(keys)?;
    Ok((n, resumed))
}

/// Decrypts every store file in `data` with whichever of `keys` its header names; the files that fail, and
/// why. Returns how many files were checked.
pub fn verify_dir(data: &Path, keys: &[&HubKey]) -> io::Result<(usize, Vec<String>)> {
    let files = store::files(data)?;
    let mut bad = Vec::new();
    for path in &files {
        let r = crypt::key_id_of(path).and_then(|id| match keys.iter().find(|k| k.id() == id) {
            Some(k) => crypt::read(k, path).map(drop),
            None => Err(CryptError::WrongKey),
        });
        if let Err(e) = r {
            bad.push(format!("{}: {e}", path.display()));
        }
    }
    Ok((files.len(), bad))
}

#[cfg(test)]
mod tests;
