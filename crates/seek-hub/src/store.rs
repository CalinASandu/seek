//! The hub's store (spec §5): linked PCs, their drives, and each drive's index copy, all encrypted at rest.
//!
//! Copies are kept as bytes, never as live indexes: `copies/<device>.full` holds the last full copy as
//! received plus its cursor, `copies/<device>.journal` the delta batches since, as received. Only
//! compaction decodes a copy. Every file goes through `crypt::write`, always under the store's lock
//! (concurrent writers of one file would share its `.tmp` name).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use seek_core::{check_index_head, decode_index, encode_index, Device};
use seek_sync::peers::key_from_b64;
use seek_sync::session::{decode_delta, encode_delta};
use seek_sync::{ownership, CatalogPc, Cursor, Keys, Refusal};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{HubConfig, Settings};
use crate::crypt::{self, CryptError, HubKey};

const HUB_FILE: &str = "hub.json";
const HOST_KEY_FILE: &str = "host.key";
const PCS_FILE: &str = "pcs.json";
const DEVICES_FILE: &str = "devices.json";
const COPIES_DIR: &str = "copies";

/// Bytes `crypt::write` adds to a file: header (40) + AEAD tag (16).
const ENC_OVERHEAD: u64 = 40 + 16;
const CURSOR_LEN: usize = 16 + 8;
/// Framing of one journal batch: `from` | `to` | payload length (u64 LE).
const BATCH_HEAD: usize = 2 * CURSOR_LEN + 8;
/// A journal is compacted past 25 % of its full copy's size or past this many bytes.
const COMPACT_MAX_JOURNAL: u64 = 50 * 1024 * 1024;
/// `touch_pc` rewrites `pcs.json` at most this often per PC (SD cards wear out).
const TOUCH_SAVE_MS: i64 = 60_000;
/// Bounds on what a PC may tell the hub about itself (the hub stores and serves it to the other PCs).
const MAX_PROFILES: usize = 64;
const MAX_META_STR: usize = 512;
const MAX_DEVICE_JSON: usize = 4096;
/// Biggest frame a PC session accepts, whatever the quota.
const MAX_SESSION_FRAME: u64 = 256 * 1024 * 1024;

/// One linked PC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PcRecord {
    pub host_id: Uuid,
    pub name: String,
    /// Noise static public key, base64 (as in the LAN peer list).
    pub public_key: String,
    /// SID and name of the Windows user who linked the PC.
    pub user_sid: String,
    pub user_name: String,
    /// `(SID, profile folder)` list.
    pub profiles: Vec<(String, String)>,
    /// Unix epoch milliseconds.
    pub last_seen_ms: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PutError {
    /// `from` is not the stored cursor (or there is no copy): the session answers `Have{none}`.
    Mismatch,
    Refused(Refusal),
}

/// One message's worth of a stored copy, as `serve` hands it out.
#[derive(Clone, Debug, PartialEq)]
pub enum Served {
    Full { cursor: Cursor, bytes: Vec<u8> },
    Delta { from: Cursor, to: Cursor, payload: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubStats {
    pub pcs: usize,
    pub devices: usize,
    /// Encrypted bytes on disk of every copy.
    pub stored_bytes: u64,
    pub per_pc: Vec<(Uuid, u64)>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot read {} (wrong key, or the file is damaged)", .0.display())]
    Unreadable(PathBuf),
    #[error("no stored copy of device {0}")]
    NoCopy(Uuid),
    #[error("stored copy of device {0} is damaged: {1}")]
    Corrupt(Uuid, String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HubFile {
    id: Uuid,
    name: String,
    #[serde(default)]
    settings: Settings,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PcsFile {
    pcs: Vec<PcRecord>,
    /// PCs unlinked from the hub: told `Unlinked` when they next connect.
    #[serde(default)]
    tombstones: Vec<Tombstone>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tombstone {
    host_id: Uuid,
    public_key: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceRec {
    owner: Uuid,
    device: Device,
}

/// What is known about one stored copy without decrypting it.
struct CopyMeta {
    full_bytes: u64,
    journal_bytes: u64,
    /// `(full copy's cursor, newest cursor)`, read on first use.
    cursors: Option<(Cursor, Cursor)>,
}

struct Batch {
    from: Cursor,
    to: Cursor,
    payload: Vec<u8>,
}

/// Why a stored copy could not be read.
enum ReadFail {
    /// Disk trouble, maybe passing: keep the copy and try again later.
    Io(io::Error),
    /// Damaged, other key, or not decodable: the copy is dropped.
    Bad(String),
}

impl From<CryptError> for ReadFail {
    fn from(e: CryptError) -> Self {
        match e {
            CryptError::Io(e) => ReadFail::Io(e),
            other => ReadFail::Bad(other.to_string()),
        }
    }
}

/// Test seam: an error to inject into a read (`false`) or write (`true`) of `path`.
pub(crate) type Fault = Box<dyn Fn(bool, &Path) -> Option<io::Error> + Send + Sync>;

/// The hub's state on disk. `Send + Sync`; every method blocks (call from `spawn_blocking` for big copies).
pub struct Store {
    id: Uuid,
    name: String,
    noise: Keys,
    quota: u64,
    max_devices: usize,
    /// Keys of linked and unlinked PCs, for the pre-auth check; never behind `inner`'s lock.
    known: Arc<RwLock<HashSet<[u8; 32]>>>,
    inner: Mutex<Inner>,
}

struct Inner {
    dir: PathBuf,
    key: HubKey,
    /// The key being rotated away from while a rotation is unfinished: files not rewritten yet still use it.
    old_key: Option<HubKey>,
    linked: PcsFile,
    devices: Vec<DeviceRec>,
    copies: HashMap<Uuid, CopyMeta>,
    /// `last_seen_ms` of each PC as last written to `pcs.json`.
    seen_saved: HashMap<Uuid, i64>,
    fault: Option<Fault>,
    known: Arc<RwLock<HashSet<[u8; 32]>>>,
}

impl Store {
    /// Load (or create) everything under `cfg.data_dir`. A file the key cannot read → `Unreadable`.
    pub fn open(cfg: &HubConfig, key: HubKey) -> Result<Store, StoreError> {
        let dir = cfg.data_dir.clone();
        fs::create_dir_all(dir.join(COPIES_DIR))?;

        let hub_path = dir.join(HUB_FILE);
        let hub = match load_json::<HubFile>(&key, &hub_path)? {
            Some(mut h) => {
                if h.name != cfg.name {
                    h.name = cfg.name.clone();
                    save_json(&key, &hub_path, &h)?;
                }
                h
            }
            None => {
                let h = HubFile { id: Uuid::new_v4(), name: cfg.name.clone(), settings: Settings::default() };
                save_json(&key, &hub_path, &h)?;
                h
            }
        };

        let key_path = dir.join(HOST_KEY_FILE);
        let noise = match crypt::read(&key, &key_path) {
            Ok(b) => {
                let private: [u8; 32] = b.try_into().map_err(|_| StoreError::Unreadable(key_path.clone()))?;
                Keys::from_private(private)
            }
            Err(CryptError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                let k = Keys::generate();
                crypt::write(&key, &key_path, &k.private)?;
                k
            }
            Err(e) => {
                tracing::error!("{}: {e}", key_path.display());
                return Err(StoreError::Unreadable(key_path));
            }
        };

        let linked: PcsFile = load_json(&key, &dir.join(PCS_FILE))?.unwrap_or_default();
        let mut devices: Vec<DeviceRec> = load_json(&key, &dir.join(DEVICES_FILE))?.unwrap_or_default();
        // Drives of PCs that are gone (an unlink interrupted between two writes).
        devices.retain(|r| linked.pcs.iter().any(|p| p.host_id == r.owner));
        let seen_saved = linked.pcs.iter().map(|p| (p.host_id, p.last_seen_ms)).collect();
        let known = Arc::new(RwLock::new(HashSet::new()));
        let mut inner = Inner {
            dir,
            key,
            old_key: None,
            linked,
            devices,
            copies: HashMap::new(),
            seen_saved,
            fault: None,
            known: known.clone(),
        };
        inner.refresh_known();
        inner.scan_copies()?;
        Ok(Store {
            id: hub.id,
            name: hub.name,
            noise,
            quota: cfg.pc_quota_bytes,
            max_devices: cfg.pc_max_devices,
            known,
            inner: Mutex::new(inner),
        })
    }

    /// Whether `key` belongs to a linked or an unlinked (tombstoned) PC. Cheap and lock-free of the store,
    /// for the handshake that strangers can trigger.
    pub fn is_known_key(&self, key: &[u8; 32]) -> bool {
        self.known.read().unwrap_or_else(|p| p.into_inner()).contains(key)
    }

    /// Largest frame a PC may send: its quota, at most 256 MiB.
    pub fn max_frame(&self) -> usize {
        self.quota.min(MAX_SESSION_FRAME) as usize
    }

    pub fn hub_id(&self) -> Uuid {
        self.id
    }

    pub fn noise_keys(&self) -> Keys {
        self.noise.clone()
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn set_fault(&self, fault: Option<Fault>) {
        self.lock().fault = fault;
    }

    // ---- PCs ----

    /// Link a PC. Clears a tombstone for its key or host; a record with the same `host_id` (relinked with a
    /// new key) is replaced and keeps its stored drives.
    /// Another PC holding the same key is removed with its drives, so a key always names one PC.
    pub fn add_pc(&self, rec: PcRecord) -> Result<(), StoreError> {
        let mut i = self.lock();
        let key = key_from_b64(&rec.public_key);
        let same_key = |k: &str| key.is_some() && key_from_b64(k) == key;
        let mut linked = i.linked.clone();
        linked.tombstones.retain(|t| !same_key(&t.public_key) && t.host_id != rec.host_id);
        let replaced: HashSet<Uuid> = linked
            .pcs
            .iter()
            .filter(|p| p.host_id != rec.host_id && same_key(&p.public_key))
            .map(|p| p.host_id)
            .collect();
        linked.pcs.retain(|p| p.host_id != rec.host_id && !replaced.contains(&p.host_id));
        linked.pcs.push(rec);
        i.commit_pcs(linked)?;
        if !replaced.is_empty() {
            let devices = i.devices.iter().filter(|r| !replaced.contains(&r.owner)).cloned().collect();
            // A failure here only leaves orphans, which `open` removes.
            if let Err(e) = i.commit_devices(devices) {
                tracing::warn!("saving {DEVICES_FILE} failed: {e}");
            }
        }
        Ok(())
    }

    pub fn pc_by_key(&self, key: &[u8; 32]) -> Option<PcRecord> {
        self.lock().linked.pcs.iter().find(|p| key_from_b64(&p.public_key).as_ref() == Some(key)).cloned()
    }

    pub fn is_tombstoned(&self, key: &[u8; 32]) -> bool {
        self.lock().linked.tombstones.iter().any(|t| key_from_b64(&t.public_key).as_ref() == Some(key))
    }

    pub fn pcs(&self) -> Vec<PcRecord> {
        self.lock().linked.pcs.clone()
    }

    /// Delete a PC and its drives' copies and remember its key. `Ok(false)` when it was not linked.
    /// The PC list (with the tombstone) is saved first: when that fails nothing changes.
    pub fn unlink_pc(&self, host: Uuid) -> Result<bool, StoreError> {
        let mut i = self.lock();
        let mut linked = i.linked.clone();
        let Some(pos) = linked.pcs.iter().position(|p| p.host_id == host) else {
            return Ok(false);
        };
        let rec = linked.pcs.remove(pos);
        linked.tombstones.retain(|t| t.host_id != host);
        linked.tombstones.push(Tombstone { host_id: host, public_key: rec.public_key });
        i.commit_pcs(linked)?;
        let devices = i.devices.iter().filter(|r| r.owner != host).cloned().collect();
        // A failure here only leaves orphans, which `open` removes.
        if let Err(e) = i.commit_devices(devices) {
            tracing::warn!("unlink {host}: saving {DEVICES_FILE} failed: {e}");
        }
        Ok(true)
    }

    /// Record activity; written to disk at most once a minute per PC.
    pub fn touch_pc(&self, host: Uuid, now_ms: i64) {
        let mut i = self.lock();
        let Some(p) = i.linked.pcs.iter_mut().find(|p| p.host_id == host) else {
            return;
        };
        p.last_seen_ms = now_ms;
        let saved = i.seen_saved.get(&host).copied().unwrap_or(i64::MIN);
        if now_ms.saturating_sub(saved) >= TOUCH_SAVE_MS {
            if let Err(e) = i.save_pcs() {
                tracing::warn!("saving {PCS_FILE} failed: {e}");
            }
        }
    }

    /// The PC's current drive list. Devices it no longer lists are deleted with their copies.
    /// Each file is saved before memory changes, so a failed save changes nothing and the next call retries.
    pub fn set_pc_devices(
        &self,
        host: Uuid,
        devices: Vec<Device>,
        profiles: Vec<(String, String)>,
    ) -> Result<(), Refusal> {
        let mut i = self.lock();
        if !i.linked.pcs.iter().any(|p| p.host_id == host) {
            return Err(Refusal::NotSupported);
        }
        let mut listed = HashSet::new();
        let devices: Vec<Device> = devices.into_iter().filter(|d| listed.insert(d.id)).collect();
        if devices.len() > self.max_devices {
            return Err(Refusal::TooManyDevices);
        }
        // metadata the hub keeps and hands to the other PCs stays small
        if profiles.len() > MAX_PROFILES
            || profiles.iter().any(|(sid, dir)| sid.len() > MAX_META_STR || dir.len() > MAX_META_STR)
            || devices.iter().any(|d| serde_json::to_vec(d).map_or(true, |j| j.len() > MAX_DEVICE_JSON))
        {
            return Err(Refusal::QuotaFull);
        }
        for d in &devices {
            ownership::check(|id| i.owner_of(id), host, d.id)?;
            // A PC only ever lists drives it indexed itself.
            if d.indexed_by_host != host {
                return Err(Refusal::NotOwner { device: d.id });
            }
        }

        let mut new: Vec<DeviceRec> =
            i.devices.iter().filter(|r| r.owner != host || listed.contains(&r.device.id)).cloned().collect();
        for d in devices {
            match new.iter_mut().find(|r| r.device.id == d.id) {
                Some(r) => r.device = d,
                None => new.push(DeviceRec { owner: host, device: d }),
            }
        }
        if new != i.devices {
            i.commit_devices(new).map_err(hub_full)?;
        }
        let pos = i.linked.pcs.iter().position(|p| p.host_id == host).expect("checked above");
        if i.linked.pcs[pos].profiles != profiles {
            let mut linked = i.linked.clone();
            linked.pcs[pos].profiles = profiles;
            i.commit_pcs(linked).map_err(hub_full)?;
        }
        Ok(())
    }

    // ---- copies ----

    /// Newest cursor of the stored copy, `None` when there is none (or it was unreadable and dropped).
    /// Also `None` while the disk fails to read it (the copy is kept).
    pub fn cursor(&self, device: Uuid) -> Option<Cursor> {
        self.lock().load(device).ok().flatten().map(|(_, latest)| latest)
    }

    /// Replace the device's copy with a full one (and an empty journal).
    pub fn put_full(&self, host: Uuid, device: Uuid, cursor: Cursor, bytes: Vec<u8>) -> Result<(), Refusal> {
        let mut i = self.lock();
        i.check_owner(host, device)?;
        // a copy that is not an index at all would only cost the quota (and a Corrupt later)
        if check_index_head(&bytes).is_err() {
            return Err(Refusal::NotSupported);
        }
        let after = i.pc_bytes(host) - i.stored_bytes(device) + (CURSOR_LEN + bytes.len()) as u64 + ENC_OVERHEAD;
        if after > self.quota {
            return Err(Refusal::QuotaFull);
        }
        let mut plain = Vec::with_capacity(CURSOR_LEN + bytes.len() + 16);
        put_cursor(&mut plain, cursor);
        plain.extend_from_slice(&bytes);
        drop(bytes);
        let path = i.full_path(device);
        // The old copy stays whole until the rename inside `crypt::write` succeeds.
        i.write_vec(&path, plain).map_err(hub_full)?;
        let journal_bytes = i.copies.get(&device).map_or(0, |m| m.journal_bytes);
        i.copies
            .insert(device, CopyMeta { full_bytes: file_len(&path), journal_bytes, cursors: Some((cursor, cursor)) });
        if let Err(e) = i.write_journal(device, &[]) {
            // The old journal could chain onto the new full copy: drop everything, the owner resends.
            i.drop_copy(device);
            return Err(hub_full(e));
        }
        Ok(())
    }

    /// Append one delta batch (stored as received). `from` must be the stored cursor.
    pub fn put_delta(
        &self,
        host: Uuid,
        device: Uuid,
        from: Cursor,
        to: Cursor,
        payload: Vec<u8>,
    ) -> Result<(), PutError> {
        let mut i = self.lock();
        i.check_owner(host, device).map_err(PutError::Refused)?;
        let io_err = |e: io::Error| PutError::Refused(hub_full(e));
        let (full, latest) = i.load(device).map_err(io_err)?.ok_or(PutError::Mismatch)?;
        if from != latest || to.generation != from.generation || to.seq <= from.seq {
            return Err(PutError::Mismatch);
        }
        // A batch that does not decode would later cost the whole copy: ask for a full one instead.
        if decode_delta(&payload).is_err() {
            return Err(PutError::Mismatch);
        }
        let new_file = i.copies.get(&device).is_some_and(|m| m.journal_bytes == 0);
        let grow = (BATCH_HEAD + payload.len()) as u64 + if new_file { ENC_OVERHEAD } else { 0 };
        if i.pc_bytes(host) + grow > self.quota {
            return Err(PutError::Refused(Refusal::QuotaFull));
        }
        let mut batches = i.journal_checked(device, full, latest).map_err(io_err)?.ok_or(PutError::Mismatch)?;
        batches.push(Batch { from, to, payload });
        i.write_journal(device, &batches).map_err(io_err)?;
        if let Some(m) = i.copies.get_mut(&device) {
            m.cursors = Some((full, to));
        }
        Ok(())
    }

    // ---- serving ----

    /// What a PC holding `have` needs: nothing, one `Delta` from the journal, or the full copy then a
    /// `Delta` from it to the newest (omitted when the journal is empty). Nothing while the disk fails.
    pub fn serve(&self, device: Uuid, have: Option<Cursor>) -> Vec<Served> {
        let mut i = self.lock();
        let Ok(Some((full, latest))) = i.load(device) else {
            return vec![];
        };
        let Ok(Some(batches)) = i.journal_checked(device, full, latest) else {
            return vec![];
        };
        if let Some(h) = have.filter(|h| h.generation == full.generation && h.seq >= full.seq) {
            if h == latest {
                return vec![];
            }
            let start = if h == full { Some(0) } else { batches.iter().position(|b| b.to == h).map(|p| p + 1) };
            if let Some(start) = start {
                return match merge(&batches[start..]) {
                    Ok(payload) => vec![Served::Delta { from: h, to: latest, payload }],
                    Err(e) => i.corrupt(device, &e),
                };
            }
        }
        let bytes = match i.read_full(device) {
            Ok((_, bytes)) => bytes,
            Err(f) => {
                i.read_failed(device, f);
                return vec![];
            }
        };
        let mut out = vec![Served::Full { cursor: full, bytes }];
        if !batches.is_empty() {
            match merge(&batches) {
                Ok(payload) => out.push(Served::Delta { from: full, to: latest, payload }),
                Err(e) => return i.corrupt(device, &e),
            }
        }
        out
    }

    /// Every **other** linked PC and its drives.
    pub fn catalog_for(&self, host: Uuid) -> (Vec<CatalogPc>, Vec<Device>) {
        let i = self.lock();
        let pcs = i
            .linked
            .pcs
            .iter()
            .filter(|p| p.host_id != host)
            .map(|p| CatalogPc {
                host_id: p.host_id,
                name: p.name.clone(),
                user_sid: p.user_sid.clone(),
                user_name: p.user_name.clone(),
                profiles: p.profiles.clone(),
                last_seen_ms: p.last_seen_ms,
            })
            .collect();
        let devices = i.devices.iter().filter(|r| r.owner != host).map(|r| r.device.clone()).collect();
        (pcs, devices)
    }

    // ---- compaction ----

    /// Devices whose journal passed 25 % of the full copy or 50 MB.
    pub fn compact_due(&self) -> Vec<Uuid> {
        let i = self.lock();
        let mut due: Vec<Uuid> = i
            .copies
            .iter()
            .filter(|(_, m)| {
                m.journal_bytes > 0 && (m.journal_bytes * 4 > m.full_bytes || m.journal_bytes > COMPACT_MAX_JOURNAL)
            })
            .map(|(d, _)| *d)
            .collect();
        due.sort();
        due
    }

    /// Fold the journal into the full copy. Decoding runs without the lock; batches that arrive meanwhile
    /// are kept, and a full copy that arrives meanwhile wins.
    pub fn compact(&self, device: Uuid) -> Result<(), StoreError> {
        let (full, bytes, batches) = {
            let mut i = self.lock();
            let (full, latest) = i.load(device)?.ok_or(StoreError::NoCopy(device))?;
            let batches = i.journal_checked(device, full, latest)?.ok_or(StoreError::NoCopy(device))?;
            if batches.is_empty() {
                return Ok(());
            }
            match i.read_full(device) {
                Ok((_, bytes)) => (full, bytes, batches),
                Err(f) => {
                    return Err(match i.read_failed(device, f) {
                        Some(e) => e.into(),
                        None => StoreError::NoCopy(device),
                    })
                }
            }
        };
        let to = batches.last().expect("not empty").to;
        let folded = fold(bytes, &batches);
        drop(batches);

        let mut i = self.lock();
        let Some((full_now, latest)) = i.load(device)? else {
            return Ok(());
        };
        if full_now != full {
            return Ok(());
        }
        let folded = match folded {
            Ok(b) => b,
            Err(e) => {
                i.corrupt(device, &e);
                return Err(StoreError::Corrupt(device, e));
            }
        };
        let Some(current) = i.journal_checked(device, full, latest)? else {
            return Ok(());
        };
        let Some(pos) = current.iter().position(|b| b.to == to) else {
            return Ok(());
        };
        let rest: Vec<Batch> = current.into_iter().skip(pos + 1).collect();
        let mut plain = folded;
        plain.splice(0..0, cursor_bytes(to));
        let path = i.full_path(device);
        i.write_vec(&path, plain)?;
        // From here a crash is harmless: loading skips journal batches before the full copy's cursor.
        if let Some(m) = i.copies.get_mut(&device) {
            m.full_bytes = file_len(&path);
            m.cursors = Some((to, latest));
        }
        i.write_journal(device, &rest)?;
        Ok(())
    }

    /// Drives stored for `host`.
    pub fn device_count(&self, host: Uuid) -> usize {
        self.lock().devices.iter().filter(|r| r.owner == host).count()
    }

    /// Key rotation: re-encrypts every file with `new` while holding the store's lock (every session waits),
    /// then uses only `new`. When it fails halfway, the store keeps reading the files not rewritten yet with
    /// the old key and writes with the new one; `rotate::open_key` finishes the job at the next start.
    /// After such a failure, only that same `new` key is accepted (the rotation is resumed).
    pub fn rekey(&self, new: HubKey) -> Result<usize, StoreError> {
        let mut i = self.lock();
        let from = match i.old_key.take() {
            Some(old) if new.id() == i.key.id() => old,
            Some(old) => {
                i.old_key = Some(old);
                return Err(StoreError::Io(io::Error::other("an unfinished key rotation must be finished first")));
            }
            None => std::mem::replace(&mut i.key, new.clone()),
        };
        let r = crate::rotate::rekey_dir(&i.dir, &from, &new);
        if r.is_err() {
            i.old_key = Some(from);
        }
        r
    }

    pub fn stats(&self) -> HubStats {
        let i = self.lock();
        let per_pc: Vec<(Uuid, u64)> = i.linked.pcs.iter().map(|p| (p.host_id, i.pc_bytes(p.host_id))).collect();
        HubStats {
            pcs: i.linked.pcs.len(),
            devices: i.devices.len(),
            stored_bytes: per_pc.iter().map(|(_, b)| b).sum(),
            per_pc,
        }
    }
}

impl Inner {
    fn full_path(&self, device: Uuid) -> PathBuf {
        self.dir.join(COPIES_DIR).join(format!("{device}.full"))
    }

    fn journal_path(&self, device: Uuid) -> PathBuf {
        self.dir.join(COPIES_DIR).join(format!("{device}.journal"))
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, CryptError> {
        if let Some(e) = self.fault.as_ref().and_then(|f| f(false, path)) {
            return Err(CryptError::Io(e));
        }
        let r = crypt::read(&self.key, path);
        match (&r, &self.old_key) {
            (Err(CryptError::WrongKey), Some(old)) => crypt::read(old, path),
            _ => r,
        }
    }

    /// Every store write goes through here (and so through `crypt::write`).
    fn write(&self, path: &Path, plain: &[u8]) -> io::Result<()> {
        if let Some(e) = self.fault.as_ref().and_then(|f| f(true, path)) {
            return Err(e);
        }
        crypt::write(&self.key, path, plain)
    }

    /// [`Inner::write`] for a buffer that is not needed afterwards (encrypted in place).
    fn write_vec(&self, path: &Path, plain: Vec<u8>) -> io::Result<()> {
        if let Some(e) = self.fault.as_ref().and_then(|f| f(true, path)) {
            return Err(e);
        }
        crypt::write_vec(&self.key, path, plain)
    }

    fn save_json<T: Serialize>(&self, file: &str, value: &T) -> io::Result<()> {
        self.write(&self.dir.join(file), &serde_json::to_vec(value).map_err(io::Error::other)?)
    }

    fn save_pcs(&mut self) -> io::Result<()> {
        self.save_json(PCS_FILE, &self.linked)?;
        self.seen_saved = self.linked.pcs.iter().map(|p| (p.host_id, p.last_seen_ms)).collect();
        Ok(())
    }

    /// Save `linked`, then make it current; on failure nothing changes.
    fn commit_pcs(&mut self, linked: PcsFile) -> io::Result<()> {
        self.save_json(PCS_FILE, &linked)?;
        self.linked = linked;
        self.seen_saved = self.linked.pcs.iter().map(|p| (p.host_id, p.last_seen_ms)).collect();
        self.refresh_known();
        Ok(())
    }

    /// Republish the linked and tombstoned keys for the pre-auth check.
    fn refresh_known(&self) {
        let keys = self
            .linked
            .pcs
            .iter()
            .map(|p| &p.public_key)
            .chain(self.linked.tombstones.iter().map(|t| &t.public_key))
            .filter_map(|k| key_from_b64(k))
            .collect();
        *self.known.write().unwrap_or_else(|p| p.into_inner()) = keys;
    }

    /// Save `devices`, then make it current and delete the copies of devices it no longer has.
    fn commit_devices(&mut self, devices: Vec<DeviceRec>) -> io::Result<()> {
        self.save_json(DEVICES_FILE, &devices)?;
        let kept: HashSet<Uuid> = devices.iter().map(|r| r.device.id).collect();
        let gone: Vec<Uuid> = self.devices.iter().map(|r| r.device.id).filter(|d| !kept.contains(d)).collect();
        self.devices = devices;
        for d in gone {
            self.drop_copy(d);
        }
        Ok(())
    }

    /// Size the stored copies from the files; remove leftovers (`.tmp`, copies of unknown devices).
    fn scan_copies(&mut self) -> io::Result<()> {
        let known: HashSet<Uuid> = self.devices.iter().map(|r| r.device.id).collect();
        for entry in fs::read_dir(self.dir.join(COPIES_DIR))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".full").or_else(|| name.strip_suffix(".journal"));
            if !id.and_then(|s| Uuid::parse_str(s).ok()).is_some_and(|d| known.contains(&d)) {
                tracing::info!("removing stale {}", entry.path().display());
                let _ = fs::remove_file(entry.path());
            }
        }
        for d in known {
            let (full, journal) = (self.full_path(d), self.journal_path(d));
            if full.exists() {
                let meta = CopyMeta { full_bytes: file_len(&full), journal_bytes: file_len(&journal), cursors: None };
                self.copies.insert(d, meta);
            } else {
                let _ = fs::remove_file(journal);
            }
        }
        Ok(())
    }

    fn owner_of(&self, device: Uuid) -> Option<Uuid> {
        self.devices.iter().find(|r| r.device.id == device).map(|r| r.owner)
    }

    /// Only the owner may write, and only devices it listed with `Devices` first.
    fn check_owner(&self, host: Uuid, device: Uuid) -> Result<(), Refusal> {
        ownership::check(|d| self.owner_of(d), host, device)?;
        match self.owner_of(device) {
            Some(_) => Ok(()),
            None => Err(Refusal::NotOwner { device }),
        }
    }

    fn stored_bytes(&self, device: Uuid) -> u64 {
        self.copies.get(&device).map_or(0, |m| m.full_bytes + m.journal_bytes)
    }

    fn pc_bytes(&self, host: Uuid) -> u64 {
        self.devices.iter().filter(|r| r.owner == host).map(|r| self.stored_bytes(r.device.id)).sum()
    }

    fn drop_copy(&mut self, device: Uuid) {
        for p in [self.full_path(device), self.journal_path(device)] {
            if let Err(e) = fs::remove_file(&p) {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!("removing {}: {e}", p.display());
                }
            }
        }
        self.copies.remove(&device);
    }

    /// Log and drop a damaged copy; the owner's next `Devices` gets `Have{none}` and resends it.
    fn corrupt(&mut self, device: Uuid, why: &str) -> Vec<Served> {
        tracing::warn!("copy of {device} is damaged ({why}); dropping it");
        self.drop_copy(device);
        vec![]
    }

    /// A damaged copy is dropped (`None`); a disk error keeps it and is handed back.
    fn read_failed(&mut self, device: Uuid, f: ReadFail) -> Option<io::Error> {
        match f {
            ReadFail::Bad(why) => {
                self.corrupt(device, &why);
                None
            }
            ReadFail::Io(e) => {
                tracing::warn!("reading the copy of {device} failed: {e}; keeping it");
                Some(e)
            }
        }
    }

    fn read_full(&self, device: Uuid) -> Result<(Cursor, Vec<u8>), ReadFail> {
        let mut plain = match self.read(&self.full_path(device)) {
            Ok(p) => p,
            Err(CryptError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                return Err(ReadFail::Bad("full copy is missing".into()))
            }
            Err(e) => return Err(e.into()),
        };
        if plain.len() < CURSOR_LEN {
            return Err(ReadFail::Bad("full copy too short".into()));
        }
        let cursor = get_cursor(&plain);
        plain.drain(..CURSOR_LEN);
        Ok((cursor, plain))
    }

    fn journal_raw(&self, device: Uuid) -> Result<Vec<Batch>, ReadFail> {
        match self.read(&self.journal_path(device)) {
            Ok(plain) => decode_journal(&plain).ok_or_else(|| ReadFail::Bad("journal is malformed".into())),
            Err(CryptError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// `(full cursor, newest cursor)` of a stored copy, reading it on first use. A journal that does not
    /// chain from the full copy is cut back to the part that does. `Ok(None)`: no copy (or a damaged one,
    /// now dropped); `Err`: the disk failed, the copy is kept and nothing is cached.
    fn load(&mut self, device: Uuid) -> io::Result<Option<(Cursor, Cursor)>> {
        let Some(meta) = self.copies.get(&device) else {
            return Ok(None);
        };
        if let Some(c) = meta.cursors {
            return Ok(Some(c));
        }
        let full = match self.read_full(device) {
            Ok((c, _)) => c,
            Err(f) => return self.read_failed(device, f).map_or(Ok(None), Err),
        };
        let (raw, damaged) = match self.journal_raw(device) {
            Ok(raw) => (raw, false),
            Err(f) => match self.read_failed_journal(device, f) {
                Some(e) => return Err(e),
                None => (vec![], true),
            },
        };
        let raw_len = raw.len();
        let batches = chain(raw, full);
        if damaged || raw_len != batches.len() {
            tracing::warn!("journal of {device} cut back to {} batches", batches.len());
            if let Err(e) = self.write_journal(device, &batches) {
                tracing::warn!("rewriting journal of {device}: {e}");
            }
        }
        let latest = batches.last().map_or(full, |b| b.to);
        if let Some(m) = self.copies.get_mut(&device) {
            m.cursors = Some((full, latest));
        }
        Ok(Some((full, latest)))
    }

    /// At load time a damaged journal only loses the deltas: the full copy is still good.
    fn read_failed_journal(&mut self, device: Uuid, f: ReadFail) -> Option<io::Error> {
        match f {
            ReadFail::Bad(why) => {
                tracing::warn!("journal of {device} is damaged ({why}); keeping the full copy");
                None
            }
            ReadFail::Io(e) => {
                tracing::warn!("reading the journal of {device} failed: {e}; keeping it");
                Some(e)
            }
        }
    }

    /// The journal batches of a loaded copy. `Ok(None)`: damaged, or no longer reaching `latest`; the
    /// copy was dropped. `Err`: the disk failed, the copy is kept.
    fn journal_checked(&mut self, device: Uuid, full: Cursor, latest: Cursor) -> io::Result<Option<Vec<Batch>>> {
        let batches = match self.journal_raw(device) {
            Ok(raw) => chain(raw, full),
            Err(f) => return self.read_failed(device, f).map_or(Ok(None), Err),
        };
        if batches.last().map_or(full, |b| b.to) != latest {
            self.corrupt(device, "journal changed on disk");
            return Ok(None);
        }
        Ok(Some(batches))
    }

    /// Write (or, when empty, remove) the journal file.
    fn write_journal(&mut self, device: Uuid, batches: &[Batch]) -> io::Result<()> {
        let path = self.journal_path(device);
        let size = if batches.is_empty() {
            if let Some(e) = self.fault.as_ref().and_then(|f| f(true, &path)) {
                return Err(e);
            }
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => 0,
            }
        } else {
            self.write(&path, &encode_journal(batches))?;
            file_len(&path)
        };
        if let Some(m) = self.copies.get_mut(&device) {
            m.journal_bytes = size;
        }
        Ok(())
    }
}

fn hub_full(e: io::Error) -> Refusal {
    tracing::warn!("hub store write failed: {e}");
    Refusal::HubFull
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

fn load_json<T: DeserializeOwned>(key: &HubKey, path: &Path) -> Result<Option<T>, StoreError> {
    let unreadable = |why: String| {
        tracing::error!("{}: {why}", path.display());
        StoreError::Unreadable(path.to_path_buf())
    };
    let plain = match crypt::read(key, path) {
        Ok(p) => p,
        Err(CryptError::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e.to_string())),
    };
    serde_json::from_slice(&plain).map(Some).map_err(|e| unreadable(e.to_string()))
}

/// Every encrypted file of a store in `dir`: `hub.json`, `host.key`, `pcs.json`, `devices.json` and each
/// copy (leftover `.tmp` files and anything else are not store files).
pub fn files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> =
        [HUB_FILE, HOST_KEY_FILE, PCS_FILE, DEVICES_FILE].iter().map(|f| dir.join(f)).filter(|p| p.is_file()).collect();
    let copies = match fs::read_dir(dir.join(COPIES_DIR)) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in copies {
        let path = entry?.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if (name.ends_with(".full") || name.ends_with(".journal")) && path.is_file() {
            out.push(path);
        }
    }
    Ok(out)
}

/// The settings `init` saved in `<dir>/hub.json` (none when there is no such file yet).
pub fn load_settings(dir: &Path, key: &HubKey) -> Result<Settings, StoreError> {
    Ok(load_json::<HubFile>(key, &dir.join(HUB_FILE))?.map(|h| h.settings).unwrap_or_default())
}

/// Saves `settings` in `<dir>/hub.json`, creating it (a new hub id) when missing; the hub reads them when it
/// starts. The hub itself writes this file only while opening the store and while rotating the key.
pub fn save_settings(dir: &Path, key: &HubKey, settings: &Settings) -> Result<(), StoreError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(HUB_FILE);
    let mut h = load_json::<HubFile>(key, &path)?.unwrap_or_else(|| HubFile {
        id: Uuid::new_v4(),
        name: settings.name.clone().unwrap_or_else(|| "Seek Hub".into()),
        settings: Settings::default(),
    });
    h.settings = settings.clone();
    save_json(key, &path, &h)?;
    Ok(())
}

/// For `open`, before the store's own writer exists.
fn save_json<T: Serialize>(key: &HubKey, path: &Path, value: &T) -> io::Result<()> {
    crypt::write(key, path, &serde_json::to_vec(value).map_err(io::Error::other)?)
}

fn put_cursor(out: &mut Vec<u8>, c: Cursor) {
    out.extend_from_slice(c.generation.as_bytes());
    out.extend_from_slice(&c.seq.to_le_bytes());
}

fn cursor_bytes(c: Cursor) -> [u8; CURSOR_LEN] {
    let mut out = [0u8; CURSOR_LEN];
    out[..16].copy_from_slice(c.generation.as_bytes());
    out[16..].copy_from_slice(&c.seq.to_le_bytes());
    out
}

/// `b` must hold at least `CURSOR_LEN` bytes.
fn get_cursor(b: &[u8]) -> Cursor {
    let generation = Uuid::from_slice(&b[..16]).expect("16 bytes");
    let seq = u64::from_le_bytes(b[16..CURSOR_LEN].try_into().expect("8 bytes"));
    Cursor { generation, seq }
}

fn encode_journal(batches: &[Batch]) -> Vec<u8> {
    let mut out = Vec::with_capacity(batches.iter().map(|b| BATCH_HEAD + b.payload.len()).sum());
    for b in batches {
        put_cursor(&mut out, b.from);
        put_cursor(&mut out, b.to);
        out.extend_from_slice(&(b.payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&b.payload);
    }
    out
}

fn decode_journal(mut b: &[u8]) -> Option<Vec<Batch>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        if b.len() < BATCH_HEAD {
            return None;
        }
        let from = get_cursor(&b[..CURSOR_LEN]);
        let to = get_cursor(&b[CURSOR_LEN..2 * CURSOR_LEN]);
        let len = u64::from_le_bytes(b[2 * CURSOR_LEN..BATCH_HEAD].try_into().ok()?);
        b = &b[BATCH_HEAD..];
        let len = usize::try_from(len).ok().filter(|&n| n <= b.len())?;
        out.push(Batch { from, to, payload: b[..len].to_vec() });
        b = &b[len..];
    }
    Some(out)
}

/// The batches that continue from `full`: leading ones from before it (left by a compaction or full
/// upload interrupted between its two writes) are skipped, and the run stops at the first gap.
fn chain(batches: Vec<Batch>, full: Cursor) -> Vec<Batch> {
    let mut at = full;
    let mut out = Vec::new();
    for b in batches.into_iter().skip_while(|b| b.from != full) {
        if b.from != at {
            break;
        }
        at = b.to;
        out.push(b);
    }
    out
}

/// One delta payload holding every change of `batches`, in order.
fn merge(batches: &[Batch]) -> Result<Vec<u8>, String> {
    let mut changes = Vec::new();
    for b in batches {
        changes.extend(decode_delta(&b.payload).map_err(|e| e.to_string())?);
    }
    Ok(encode_delta(&changes))
}

/// The full copy with every batch applied, re-encoded.
fn fold(full: Vec<u8>, batches: &[Batch]) -> Result<Vec<u8>, String> {
    let decoded = decode_index(&full).map_err(|e| e.to_string());
    drop(full);
    let (header, mut ix) = decoded?;
    for b in batches {
        ix.apply(&decode_delta(&b.payload).map_err(|e| e.to_string())?);
    }
    encode_index(&header, &mut ix).map_err(|e| e.to_string())
}

/// Test helper: a blob that passes `put_full`'s header check, followed by `tail`.
#[cfg(test)]
pub(crate) fn fake_copy(tail: &[u8]) -> Vec<u8> {
    let hdr = seek_core::IndexHeader {
        format_version: 0,
        device_id: Uuid::nil(),
        host_id: Uuid::nil(),
        checkpoint: None,
        saved_at: 0,
        rows: 0,
        sync_cursor: None,
    };
    let full = encode_index(&hdr, &mut seek_core::DeviceIndex::empty(0)).unwrap();
    let head = 16 + u32::from_le_bytes(full[12..16].try_into().unwrap()) as usize;
    let mut out = full[..head].to_vec();
    out.extend_from_slice(tail);
    out
}

#[cfg(test)]
mod tests;
