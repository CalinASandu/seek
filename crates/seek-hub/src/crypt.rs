//! Encrypted-at-rest files: `SEEKENC1` | key id (8) | nonce (24) | XChaCha20-Poly1305 ciphertext.
//! The associated data is the header followed by the file's name, so a valid file copied over another fails.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{Aead, AeadInPlace, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;

const MAGIC: &[u8; 8] = b"SEEKENC1";
const KEY_FILE: &str = "hub.key";
/// A rotation's new key, until every file is re-encrypted with it.
pub const NEW_KEY_FILE: &str = "hub.key.new";
const HEADER_LEN: usize = 8 + 8 + 24;

#[derive(Debug, thiserror::Error)]
pub enum CryptError {
    #[error("no hub key found (hub.key is missing)")]
    NoKey,
    #[error("file was encrypted with a different key")]
    WrongKey,
    #[error("encrypted file is corrupt or was modified")]
    Tampered,
    #[error("file is not encrypted")]
    NotEncrypted,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The hub's 32-byte at-rest key, stored in `<keys_dir>/hub.key`.
#[derive(Clone)]
pub struct HubKey {
    key: [u8; 32],
}

impl HubKey {
    /// Generate a new key file; fails if one already exists.
    pub fn create(dir: &Path) -> io::Result<HubKey> {
        fs::create_dir_all(dir)?;
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        // Written whole as a temp file, then linked into place (which fails if a key exists): a crash cannot
        // leave a half-written hub.key that would block `create` forever.
        let path = dir.join(KEY_FILE);
        let tmp = tmp_path(&path);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let placed = (|| {
            let mut f = opts.open(&tmp)?;
            f.write_all(&key)?;
            f.sync_all()?;
            drop(f);
            fs::hard_link(&tmp, &path)
        })();
        let _ = fs::remove_file(&tmp);
        placed?;
        sync_dir(dir)?;
        Ok(HubKey { key })
    }

    pub fn load(dir: &Path) -> Result<HubKey, CryptError> {
        HubKey::load_file(&dir.join(KEY_FILE))
    }

    /// A new random key, not saved anywhere yet (key rotation).
    pub fn generate() -> HubKey {
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        HubKey { key }
    }

    /// The key a rotation is moving to (`<dir>/hub.key.new`), if one was started and not finished.
    pub fn load_new(dir: &Path) -> Result<Option<HubKey>, CryptError> {
        match HubKey::load_file(&dir.join(NEW_KEY_FILE)) {
            Ok(k) => Ok(Some(k)),
            Err(CryptError::NoKey) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Durably writes this key as `<dir>/hub.key.new` (temp + fsync + rename + directory fsync): a rotation
    /// must not encrypt a single file with a key that a power cut could still lose.
    pub fn save_new(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join(NEW_KEY_FILE);
        let tmp = tmp_path(&path);
        {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(&self.key)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        sync_dir(dir)
    }

    /// Ends a rotation: `hub.key.new` replaces `hub.key` (one atomic rename).
    pub fn commit_new(dir: &Path) -> io::Result<()> {
        fs::rename(dir.join(NEW_KEY_FILE), dir.join(KEY_FILE))?;
        sync_dir(dir)
    }

    fn load_file(path: &Path) -> Result<HubKey, CryptError> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(CryptError::NoKey),
            Err(e) => return Err(e.into()),
        };
        let key: [u8; 32] = bytes.try_into().map_err(|_| {
            CryptError::Io(io::Error::new(io::ErrorKind::InvalidData, format!("{} must be 32 bytes", path.display())))
        })?;
        Ok(HubKey { key })
    }

    /// First 8 bytes of BLAKE2s(key); stored in every file so a wrong key is told apart from tampering.
    pub fn id(&self) -> [u8; 8] {
        let h = Blake2s256::digest(self.key);
        let mut id = [0u8; 8];
        id.copy_from_slice(&h[..8]);
        id
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.key).into())
    }
}

/// Persists renames in `dir` (directories can't be opened for sync on Windows).
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(if dir.as_os_str().is_empty() { Path::new(".") } else { dir })?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// The key id in `path`'s header, without decrypting it.
pub fn key_id_of(path: &Path) -> Result<[u8; 8], CryptError> {
    let mut head = Vec::with_capacity(16);
    fs::File::open(path)?.take(16).read_to_end(&mut head)?;
    if head.len() < MAGIC.len() || &head[..8] != MAGIC {
        return Err(CryptError::NotEncrypted);
    }
    head[8..].try_into().map_err(|_| CryptError::Tampered)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Header bytes + the final path component (UTF-8, lossy for non-UTF-8 names).
fn aad(header: &[u8], path: &Path) -> Vec<u8> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut a = Vec::with_capacity(header.len() + name.len());
    a.extend_from_slice(header);
    a.extend_from_slice(name.as_bytes());
    a
}

/// Encrypt `plain` and atomically replace `path` (write `<path>.tmp`, fsync, rename).
pub fn write(key: &HubKey, path: &Path, plain: &[u8]) -> io::Result<()> {
    write_vec(key, path, plain.to_vec())
}

/// Like [`write`], taking the plaintext buffer and encrypting it in place (no second copy of a big file).
pub fn write_vec(key: &HubKey, path: &Path, mut buf: Vec<u8>) -> io::Result<()> {
    let mut nonce = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&key.id());
    header.extend_from_slice(&nonce);
    key.cipher()
        .encrypt_in_place(XNonce::from_slice(&nonce), &aad(&header, path), &mut buf)
        .map_err(|_| io::Error::other("encryption failed"))?;
    let tmp = tmp_path(path);
    let written = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&header)?;
        f.write_all(&buf)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        // a disk-full error must not leave a partial file behind (it would eat more of the disk)
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Persist the rename itself (directories can't be opened for sync on Windows).
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// Read and decrypt `path`. Never looks at `<path>.tmp`.
pub fn read(key: &HubKey, path: &Path) -> Result<Vec<u8>, CryptError> {
    let bytes = fs::read(path)?;
    if bytes.len() < MAGIC.len() || &bytes[..8] != MAGIC {
        return Err(CryptError::NotEncrypted);
    }
    if bytes.len() < HEADER_LEN {
        return Err(CryptError::Tampered);
    }
    if bytes[8..16] != key.id() {
        return Err(CryptError::WrongKey);
    }
    let nonce = XNonce::from_slice(&bytes[16..40]);
    let payload = Payload { msg: &bytes[HEADER_LEN..], aad: &aad(&bytes[..HEADER_LEN], path) };
    key.cipher().decrypt(nonce, payload).map_err(|_| CryptError::Tampered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let d = tempfile::tempdir().unwrap();
        let key = HubKey::create(d.path()).unwrap();
        let p = d.path().join("a.bin");
        write(&key, &p, b"hello").unwrap();
        assert_eq!(read(&key, &p).unwrap(), b"hello");
        assert!(!d.path().join("a.bin.tmp").exists());
        let loaded = HubKey::load(d.path()).unwrap();
        assert_eq!(loaded.id(), key.id());
        assert_eq!(read(&loaded, &p).unwrap(), b"hello");
    }

    #[test]
    fn wrong_key() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let k1 = HubKey::create(d1.path()).unwrap();
        let k2 = HubKey::create(d2.path()).unwrap();
        let p = d1.path().join("a.bin");
        write(&k1, &p, b"x").unwrap();
        assert!(matches!(read(&k2, &p), Err(CryptError::WrongKey)));
    }

    #[test]
    fn tamper_one_byte() {
        let d = tempfile::tempdir().unwrap();
        let key = HubKey::create(d.path()).unwrap();
        let p = d.path().join("a.bin");
        write(&key, &p, b"some content").unwrap();
        let mut b = std::fs::read(&p).unwrap();
        let last = b.len() - 1;
        b[last] ^= 1;
        std::fs::write(&p, b).unwrap();
        assert!(matches!(read(&key, &p), Err(CryptError::Tampered)));
    }

    #[test]
    fn plain_file() {
        let d = tempfile::tempdir().unwrap();
        let key = HubKey::create(d.path()).unwrap();
        let p = d.path().join("a.bin");
        std::fs::write(&p, b"just some plain text file").unwrap();
        assert!(matches!(read(&key, &p), Err(CryptError::NotEncrypted)));
    }

    #[test]
    fn create_refuses_existing_key() {
        let d = tempfile::tempdir().unwrap();
        HubKey::create(d.path()).unwrap();
        assert!(HubKey::create(d.path()).is_err());
        let e = tempfile::tempdir().unwrap();
        assert!(matches!(HubKey::load(e.path()), Err(CryptError::NoKey)));
    }

    #[test]
    fn swapped_files_fail() {
        let d = tempfile::tempdir().unwrap();
        let key = HubKey::create(d.path()).unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        write(&key, &a, b"for a").unwrap();
        write(&key, &b, b"for b").unwrap();
        std::fs::copy(&a, &b).unwrap();
        assert!(matches!(read(&key, &b), Err(CryptError::Tampered)));
        assert_eq!(read(&key, &a).unwrap(), b"for a");
    }

    #[test]
    fn tmp_file_left_behind_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let key = HubKey::create(d.path()).unwrap();
        let p = d.path().join("x");
        write(&key, &p, b"valid").unwrap();
        std::fs::write(d.path().join("x.tmp"), b"garbage half write").unwrap();
        assert_eq!(read(&key, &p).unwrap(), b"valid");
    }

    #[test]
    fn failed_write_leaves_no_tmp_and_key_create_survives_a_stale_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let key = HubKey::generate();
        // the target is a directory, so the final rename fails
        let target = dir.path().join("x.bin");
        fs::create_dir(&target).unwrap();
        assert!(write(&key, &target, b"data").is_err());
        assert!(!tmp_path(&target).exists(), "partial .tmp must be removed");

        // a half-written key temp from a crash does not block creating the key
        let keys = dir.path().join("keys");
        fs::create_dir_all(&keys).unwrap();
        fs::write(keys.join("hub.key.tmp"), b"half").unwrap();
        let k = HubKey::create(&keys).unwrap();
        assert_eq!(HubKey::load(&keys).unwrap().id(), k.id());
        assert!(!keys.join("hub.key.tmp").exists());
        assert!(HubKey::create(&keys).is_err(), "never overwrites an existing key");
    }
}
