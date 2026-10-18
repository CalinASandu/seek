//! Linked PCs (`peers.json`) and this PC's static key (`host.key`).

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::noise::Keys;
use crate::Error;

/// A group holds at most this many PCs, this one included.
pub const MAX_PCS: usize = 5;
const PEERS_FILE: &str = "peers.json";
const KEY_FILE: &str = "host.key";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PeerState {
    Linked,
    /// The PC came back with a different key (reinstalled): it must be linked again.
    NeedsRelink,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub host_id: uuid::Uuid,
    pub name: String,
    /// Base64 of the 32-byte static public key.
    pub public_key: String,
    /// `ip:port` it was last reached at.
    pub addr: Option<String>,
    /// `(local SID, remote SID)` pairs that are the same person.
    #[serde(default)]
    pub linked_users: Vec<(String, String)>,
    /// The peer's `(SID, profile folder)` list.
    #[serde(default)]
    pub profiles: Vec<(String, String)>,
    #[serde(default)]
    pub last_sync_ms: i64,
    pub state: PeerState,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerFile {
    /// "Share this PC's files with linked PCs".
    #[serde(default = "yes")]
    pub sharing: bool,
    /// Own devices excluded from sharing.
    #[serde(default)]
    pub unshared: Vec<uuid::Uuid>,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

impl Default for PeerFile {
    fn default() -> Self {
        Self { sharing: true, unshared: Vec::new(), peers: Vec::new() }
    }
}

pub struct PeerStore {
    dir: PathBuf,
    pub file: PeerFile,
}

impl PeerStore {
    /// Loads `<dir>/peers.json`; missing → empty; unreadable → moved to `peers.json.bad` and empty.
    pub fn load(dir: &Path) -> PeerStore {
        let path = dir.join(PEERS_FILE);
        let file = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::error!("{} unreadable ({e}); starting without linked PCs", path.display());
                let _ = std::fs::rename(&path, dir.join("peers.json.bad"));
                PeerFile::default()
            }),
            Err(_) => PeerFile::default(),
        };
        PeerStore { dir: dir.to_path_buf(), file }
    }

    pub fn save(&self) -> Result<(), Error> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(PEERS_FILE);
        let tmp = self.dir.join("peers.json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.file).map_err(|e| Error::Protocol(e.to_string()))?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, host: uuid::Uuid) -> Option<&Peer> {
        self.file.peers.iter().find(|p| p.host_id == host)
    }

    pub fn get_mut(&mut self, host: uuid::Uuid) -> Option<&mut Peer> {
        self.file.peers.iter_mut().find(|p| p.host_id == host)
    }

    /// Adds or replaces a peer; user links are merged.
    pub fn upsert(&mut self, mut p: Peer) {
        if let Some(old) = self.get_mut(p.host_id) {
            for l in std::mem::take(&mut old.linked_users) {
                if !p.linked_users.contains(&l) {
                    p.linked_users.push(l);
                }
            }
            *old = p;
        } else {
            self.file.peers.push(p);
        }
    }

    pub fn remove(&mut self, host: uuid::Uuid) -> Option<Peer> {
        let pos = self.file.peers.iter().position(|p| p.host_id == host)?;
        Some(self.file.peers.remove(pos))
    }

    pub fn by_key(&self, key: &[u8; 32]) -> Option<&Peer> {
        let k = key_to_b64(key);
        self.file.peers.iter().find(|p| p.public_key == k)
    }
}

pub fn key_to_b64(k: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(k)
}

pub fn key_from_b64(s: &str) -> Option<[u8; 32]> {
    let v = base64::engine::general_purpose::STANDARD.decode(s.trim()).ok()?;
    v.try_into().ok()
}

/// This PC's static key from `<dir>/host.key` (base64 private key), created on first use.
pub fn load_or_create_keys(dir: &Path) -> Result<Keys, Error> {
    let path = dir.join(KEY_FILE);
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Some(private) = key_from_b64(&s) {
            return Ok(Keys::from_private(private));
        }
        tracing::error!("{} unreadable; creating a new key (linked PCs must link again)", path.display());
    }
    let keys = Keys::generate();
    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, key_to_b64(&keys.private))?;
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(key: &[u8; 32], links: Vec<(String, String)>) -> Peer {
        Peer {
            host_id: uuid::Uuid::from_u128(7),
            name: "Laptop".into(),
            public_key: key_to_b64(key),
            addr: Some("10.0.0.2:47321".into()),
            linked_users: links,
            profiles: vec![],
            last_sync_ms: 0,
            state: PeerState::Linked,
        }
    }

    #[test]
    fn store_roundtrip_and_keys_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let k1 = load_or_create_keys(dir.path()).unwrap();
        let k2 = load_or_create_keys(dir.path()).unwrap();
        assert_eq!(k1.public, k2.public);
        let mut s = PeerStore::load(dir.path());
        assert!(s.file.sharing && s.file.peers.is_empty());
        let a = ("S-a".to_string(), "S-b".to_string());
        let c = ("S-c".to_string(), "S-d".to_string());
        s.upsert(peer(&k1.public, vec![a.clone()]));
        s.upsert(peer(&k1.public, vec![a.clone(), c.clone()]));
        s.upsert(peer(&k1.public, vec![]));
        s.save().unwrap();
        let s2 = PeerStore::load(dir.path());
        assert_eq!(s2.file.peers.len(), 1);
        assert_eq!(s2.get(uuid::Uuid::from_u128(7)).unwrap().linked_users, vec![a, c]);
        assert!(s2.by_key(&k1.public).is_some());
        assert!(s2.by_key(&Keys::generate().public).is_none());
    }

    #[test]
    fn corrupt_store_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("peers.json"), b"{").unwrap();
        let s = PeerStore::load(dir.path());
        assert!(s.file.peers.is_empty() && s.file.sharing);
        assert!(dir.path().join("peers.json.bad").exists());
    }

    #[test]
    fn older_files_default_to_sharing() {
        let f: PeerFile = serde_json::from_str("{}").unwrap();
        assert!(f.sharing);
    }
}
