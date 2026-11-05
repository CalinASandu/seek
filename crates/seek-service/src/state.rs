//! Shared service state: device registry, in-memory indexes and live bookkeeping.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use seek_core::{
    load_index, read_index_head, save_index, Device, DeviceId, DeviceIndex, DeviceKind, DeviceRegistry,
    DeviceState, FolderTotals, HostId, IndexHeader, INDEX_FORMAT_VERSION,
};
use seek_ipc::Event;
use seek_scan::watch::MAX_CHANGED_DIRS;
use seek_scan::{Changed, DirWatcher, UsnFollower, VolumeInfo};
use tokio::sync::broadcast;

use crate::monitor::VolumeSource;
use crate::parking::Parked;

pub struct Config {
    pub data_dir: PathBuf,
    pub pipe_name: String,
    /// Sync with linked PCs; `None` disables it.
    pub sync: Option<SyncConfig>,
    /// Unload an index this long after its last use; `None` keeps loaded indexes in memory.
    pub idle_unload: Option<Duration>,
}

/// Network settings for sync between linked PCs.
#[derive(Clone, Debug)]
pub struct SyncConfig {
    /// Address to listen on (`0.0.0.0`; tests use `127.0.0.1` so no firewall prompt appears).
    pub bind: std::net::IpAddr,
    /// TCP port to listen on (47321; 0 = any, for tests).
    pub port: u16,
    /// Advertise and discover PCs on the LAN with mDNS.
    pub mdns: bool,
    /// This PC's name as shown on linked PCs.
    pub name: String,
    /// How often changes are pushed to linked PCs, and the hub is polled and redialed.
    pub interval: Duration,
    /// TLS trust for a `tls://` hub address; `None` = the public web roots (tests trust their own root).
    pub hub_tls: Option<seek_sync::transport::TlsConfig>,
}

pub type SharedIndex = Arc<RwLock<DeviceIndex>>;
/// Cached folder totals of one device for one user (SID, or "" when nothing is hidden).
pub type SpaceEntry = ((DeviceId, String), Arc<FolderTotals>);

pub struct State {
    pub cfg: Config,
    pub source: Arc<dyn VolumeSource>,
    pub registry: RwLock<DeviceRegistry>,
    /// Indexes in memory. A device with an index is either here or in `parked`.
    pub indexes: RwLock<HashMap<DeviceId, SharedIndex>>,
    /// Indexes that are only on disk (see `parking`).
    pub parked: Mutex<HashMap<DeviceId, Parked>>,
    /// Only one thread loads parked indexes at a time; the others wait for what it brings in.
    pub load_lock: Mutex<()>,
    /// When each index was last needed.
    pub last_used: Mutex<HashMap<DeviceId, Instant>>,
    /// Who needed it, for `seek memory`.
    pub usage: Mutex<crate::memory::UsageLog>,
    /// Bumped when indexes leave memory: connections drop the results they stored.
    pub parked_gen: tokio::sync::watch::Sender<u64>,
    pub dirty: Mutex<HashSet<DeviceId>>,
    /// Local volumes currently present, by device.
    pub mounted: Mutex<HashMap<DeviceId, VolumeInfo>>,
    pub followers: Mutex<HashMap<DeviceId, UsnFollower>>,
    pub watchers: Mutex<HashMap<DeviceId, DirWatcher>>,
    /// Devices with a scan in progress → entries scanned so far.
    pub scanning: Mutex<HashMap<DeviceId, u64>>,
    /// Devices whose stored index was unusable; scanned as soon as they are present.
    pub needs_rescan: Mutex<HashSet<DeviceId>>,
    /// Watched devices that changed since their last scan or update started.
    pub rescan_pending: Mutex<HashMap<DeviceId, Pending>>,
    /// Devices with a folder update (partial rescan) in progress.
    pub updating: Mutex<HashSet<DeviceId>>,
    /// When each device's last scan finished and how long it took.
    pub last_scan: Mutex<HashMap<DeviceId, (Instant, Duration)>>,
    /// Volumes only the user's session can see (reported by the app, which scans and pushes them).
    pub user_volumes: Mutex<HashSet<DeviceId>>,
    pub events: broadcast::Sender<Event>,
    pub invalidated: AtomicBool,
    /// Rows hidden from a user on a device, by (device, user SID), with the index epoch and row count
    /// they were computed for. `None` = nothing to hide there.
    pub hidden_masks: Mutex<HashMap<(DeviceId, String), HiddenMask>>,
    /// Folder totals for the Space view, by (device, SID or "" when nothing is hidden), most recent last.
    /// Only the last few are kept: one costs ~21 bytes per row.
    pub space_totals: Mutex<Vec<SpaceEntry>>,
    /// Change journals of own devices (see `journal_ops`).
    pub journals: Mutex<HashMap<DeviceId, seek_sync::DeviceJournal>>,
    /// Linked PCs, sharing settings (`peers.json`).
    pub peers: Mutex<seek_sync::peers::PeerStore>,
    /// Where each copy of another PC's device is in that device's journal.
    pub remote_cursors: Mutex<HashMap<DeviceId, seek_sync::Cursor>>,
    /// Linked PCs with a live session.
    pub connected: Mutex<HashSet<HostId>>,
    /// Network side of sync (listener, dialer, pairing); unset when sync is disabled.
    pub peers_rt: std::sync::OnceLock<Arc<crate::peers::PeersRuntime>>,
}

pub struct HiddenMask {
    pub epoch: u64,
    pub rows: usize,
    pub mask: Option<Arc<Vec<bool>>>,
}

/// Work a watcher queued for a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Rescan only these folders (non-recursively; new subfolders are walked whole).
    Dirs(HashSet<PathBuf>),
    /// Rescan the whole volume (rate-limited by a cooldown).
    Full,
}

impl Pending {
    /// Adds a watcher report; too many folders turn into a full rescan.
    pub fn merge(slot: Option<Pending>, changed: Changed) -> Pending {
        match (slot, changed) {
            (Some(Pending::Full), _) | (_, Changed::Everything) => Pending::Full,
            (Some(Pending::Dirs(mut a)), Changed::Dirs(b)) => {
                a.extend(b);
                if a.len() > MAX_CHANGED_DIRS {
                    Pending::Full
                } else {
                    Pending::Dirs(a)
                }
            }
            (None, Changed::Dirs(b)) => Pending::Dirs(b),
        }
    }
}

/// One device as seen by a search: a snapshot of its record plus its index.
pub struct Snapshot {
    pub device: Device,
    pub index: SharedIndex,
    pub online: bool,
    /// Name of the PC the device belongs to.
    pub pc: String,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl State {
    /// Loads the registry and notes every stored index, which stay on disk until they are needed. Unreadable
    /// indexes are dropped and their devices queued for a rescan; an unreadable registry is moved aside and
    /// recreated.
    pub fn load(cfg: Config, source: Arc<dyn VolumeSource>) -> anyhow::Result<Arc<State>> {
        std::fs::create_dir_all(cfg.data_dir.join("indexes"))?;
        std::fs::create_dir_all(cfg.data_dir.join("journal"))?;
        let mut registry = match DeviceRegistry::load_or_create(&cfg.data_dir) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("device registry unreadable ({e}); starting a new one");
                let path = cfg.data_dir.join("devices.json");
                let _ = std::fs::rename(&path, cfg.data_dir.join("devices.json.bad"));
                DeviceRegistry::load_or_create(&cfg.data_dir)?
            }
        };
        let mut indexes = HashMap::new();
        let mut parked = HashMap::new();
        let mut dirty = HashSet::new();
        let mut needs_rescan = HashSet::new();
        let mut journals = HashMap::new();
        let mut remote_cursors = HashMap::new();
        let host_id = registry.host_id;
        for d in &mut registry.devices {
            d.state = DeviceState::Offline;
            let path = index_path(&cfg.data_dir, d.id);
            match read_index_head(&path) {
                Ok(head) if head.device_id == d.id => {
                    let mut stale = false;
                    if d.indexed_by_host == host_id {
                        // The journal must reach back to the saved index, whose missing changes it holds;
                        // otherwise (older index file, damaged journal) linked PCs need a full copy.
                        let jpath = cfg.data_dir.join("journal").join(format!("{}.log", d.id));
                        let mut j =
                            seek_sync::DeviceJournal::open(jpath, seek_sync::JOURNAL_MAX_BYTES);
                        stale = !head
                            .sync_cursor
                            .is_some_and(|(generation, seq)| j.covers(&seek_sync::Cursor { generation, seq }));
                        if stale {
                            j.reset();
                        }
                        journals.insert(d.id, j);
                    } else if let Some((generation, seq)) = head.sync_cursor {
                        remote_cursors.insert(d.id, seek_sync::Cursor { generation, seq });
                    }
                    if !stale {
                        parked.insert(d.id, Parked::from_head(&head));
                        continue;
                    }
                    // Its file no longer matches the new journal: kept in memory until it is saved.
                    match load_index(&path) {
                        Ok((_, ix)) => {
                            indexes.insert(d.id, Arc::new(RwLock::new(ix)));
                            dirty.insert(d.id);
                        }
                        Err(e) => {
                            tracing::warn!("index for {} unreadable ({e}); will rescan", d.display_name);
                            let _ = std::fs::remove_file(&path);
                            needs_rescan.insert(d.id);
                        }
                    }
                }
                Ok(_) => {
                    tracing::warn!(
                        "index file for {} belongs to another device; rescanning",
                        d.display_name
                    );
                    needs_rescan.insert(d.id);
                }
                Err(seek_core::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!("index for {} unreadable ({e}); will rescan", d.display_name);
                    let _ = std::fs::remove_file(&path);
                    needs_rescan.insert(d.id);
                }
            }
        }
        let (events, _) = broadcast::channel(256);
        let cfg_dir = cfg.data_dir.clone();
        let state = Arc::new(State {
            cfg,
            source,
            registry: RwLock::new(registry),
            indexes: RwLock::new(indexes),
            parked: Mutex::new(parked),
            load_lock: Mutex::new(()),
            last_used: Mutex::new(HashMap::new()),
            usage: Mutex::new(crate::memory::UsageLog::default()),
            parked_gen: tokio::sync::watch::channel(0).0,
            dirty: Mutex::new(dirty),
            mounted: Mutex::new(HashMap::new()),
            followers: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            scanning: Mutex::new(HashMap::new()),
            needs_rescan: Mutex::new(needs_rescan),
            rescan_pending: Mutex::new(HashMap::new()),
            updating: Mutex::new(HashSet::new()),
            last_scan: Mutex::new(HashMap::new()),
            user_volumes: Mutex::new(HashSet::new()),
            events,
            invalidated: AtomicBool::new(false),
            hidden_masks: Mutex::new(HashMap::new()),
            space_totals: Mutex::new(Vec::new()),
            journals: Mutex::new(journals),
            peers: Mutex::new(seek_sync::peers::PeerStore::load(&cfg_dir)),
            remote_cursors: Mutex::new(remote_cursors),
            connected: Mutex::new(HashSet::new()),
            peers_rt: std::sync::OnceLock::new(),
        });
        state.save_registry();
        Ok(state)
    }

    /// This PC's name as shown to linked PCs.
    pub fn pc_name(&self) -> String {
        match &self.cfg.sync {
            Some(s) => s.name.clone(),
            None => std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
        }
    }

    /// Name of the PC a device belongs to, as shown in results and the `pc:` filter.
    pub fn owner_name(&self, host: HostId) -> String {
        if host == self.host_id() {
            return self.pc_name();
        }
        self.peers.lock().unwrap().get(host).map(|p| p.name.clone()).unwrap_or_else(|| "Linked PC".into())
    }

    /// `Some(PC name)` when the device is a copy of another PC's device.
    pub fn remote_owner(&self, d: &Device) -> Option<String> {
        (d.indexed_by_host != self.host_id()).then(|| self.owner_name(d.indexed_by_host))
    }

    /// Devices as the user sees them: one record per physical device (see `hidden_duplicates`).
    pub fn visible_devices(&self) -> Vec<Device> {
        let reg = self.registry.read().unwrap();
        let hidden = seek_core::hidden_duplicates(&reg.devices, reg.host_id);
        reg.devices.iter().filter(|d| !hidden.contains(&d.id)).cloned().collect()
    }

    /// Folders `sid` may not see on `device`. Own devices: `own_hidden` (other local users' profiles).
    /// Another PC's device: that PC's profile folders except those of users linked with `sid`; an elevated
    /// caller sees everything.
    pub fn hidden_for(&self, device: &Device, sid: &str, elevated: bool, own_hidden: &[String]) -> Vec<String> {
        if device.indexed_by_host == self.host_id() {
            return own_hidden.to_vec();
        }
        if elevated {
            return Vec::new();
        }
        let peers = self.peers.lock().unwrap();
        let Some(p) = peers.get(device.indexed_by_host) else { return Vec::new() };
        p.profiles
            .iter()
            .filter(|(rsid, path)| {
                !path.is_empty()
                    && !p.linked_users.iter().any(|(l, r)| l.eq_ignore_ascii_case(sid) && r.eq_ignore_ascii_case(rsid))
            })
            .map(|(_, path)| path.clone())
            .collect()
    }

    pub fn peers_changed(&self) {
        self.emit(Event::PeersChanged);
    }

    pub fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    /// Marks results stale; a ticker turns this into at most one `ResultsInvalidated` per second.
    pub fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Relaxed);
    }

    pub fn host_id(&self) -> uuid::Uuid {
        self.registry.read().unwrap().host_id
    }

    pub fn device(&self, id: DeviceId) -> Option<Device> {
        self.registry.read().unwrap().get(id).cloned()
    }

    pub fn save_registry(&self) {
        let reg = self.registry.read().unwrap();
        if let Err(e) = reg.save(&self.cfg.data_dir) {
            tracing::error!("saving device registry failed: {e}");
        }
    }

    /// Applies `f` to a device record, persists the registry and notifies clients.
    pub fn update_device(&self, id: DeviceId, f: impl FnOnce(&mut Device)) -> bool {
        let found = {
            let mut reg = self.registry.write().unwrap();
            match reg.get_mut(id) {
                Some(d) => {
                    f(d);
                    true
                }
                None => false,
            }
        };
        if found {
            self.save_registry();
            self.emit(Event::DevicesChanged);
        }
        found
    }

    pub fn index_path(&self, id: DeviceId) -> PathBuf {
        index_path(&self.cfg.data_dir, id)
    }

    pub fn mark_dirty(&self, id: DeviceId) {
        self.dirty.lock().unwrap().insert(id);
    }

    /// Writes one device's index to disk. Saving compacts the index, which renumbers rows, so
    /// open result sets are invalidated afterwards.
    pub fn save_index(&self, id: DeviceId) {
        let Some(ix) = self.indexes.read().unwrap().get(&id).cloned() else {
            return;
        };
        let header = IndexHeader {
            format_version: INDEX_FORMAT_VERSION,
            device_id: id,
            host_id: self.host_id(),
            checkpoint: None,
            saved_at: now_ms(),
            rows: 0,
            sync_cursor: None,
        };
        let own = self.is_own_device(id);
        let mut ix = ix.write().unwrap();
        let sync_cursor = if own {
            let c = self.journal_cursor(id);
            Some((c.generation, c.seq))
        } else {
            self.remote_cursors
                .lock()
                .unwrap()
                .get(&id)
                .map(|c| (c.generation, c.seq))
        };
        let header = IndexHeader {
            checkpoint: ix.checkpoint,
            sync_cursor,
            ..header
        };
        let compacts = ix.live_count() != ix.len();
        match save_index(&self.index_path(id), &header, &mut ix) {
            Ok(()) => {
                self.dirty.lock().unwrap().remove(&id);
                if compacts {
                    self.invalidate();
                }
            }
            Err(e) => tracing::error!("saving index {id} failed: {e}"),
        }
    }

    pub fn save_dirty(&self) {
        let ids: Vec<DeviceId> = self.dirty.lock().unwrap().iter().copied().collect();
        for id in ids {
            self.save_index(id);
        }
    }

    /// Rows of `ix` (the index of `device`) that a user must not see: everything inside `hidden` folders
    /// on that device. Cached until the index changes; `None` when nothing is hidden there.
    pub fn hidden_mask(
        &self,
        device: &Device,
        ix: &DeviceIndex,
        sid: &str,
        hidden: &[String],
    ) -> Option<Arc<Vec<bool>>> {
        if hidden.is_empty() || device.kind == DeviceKind::Network {
            return None;
        }
        let mount = device
            .last_mount_point
            .trim_end_matches('\\')
            .to_lowercase();
        let paths: Vec<&str> = hidden
            .iter()
            .filter(|p| p.to_lowercase().starts_with(&format!("{mount}\\")))
            .map(String::as_str)
            .collect();
        if paths.is_empty() {
            return None;
        }
        let key = (device.id, sid.to_string());
        if let Some(m) = self.hidden_masks.lock().unwrap().get(&key) {
            if m.epoch == ix.epoch() && m.rows == ix.len() {
                return m.mask.clone();
            }
        }
        let roots: Vec<u32> = ix
            .find_folders(&device.last_mount_point, &paths)
            .into_iter()
            .flatten()
            .collect();
        let mask = (!roots.is_empty()).then(|| Arc::new(ix.subtree_mask(&roots)));
        self.hidden_masks.lock().unwrap().insert(
            key,
            HiddenMask {
                epoch: ix.epoch(),
                rows: ix.len(),
                mask: mask.clone(),
            },
        );
        mask
    }

    /// Folder totals of `ix` (the index of `device`) as `sid` may see them (`mask` = rows hidden from them).
    /// Cached until the index changes; only the most recently used few are kept.
    pub fn space_totals(
        &self,
        device: &Device,
        ix: &DeviceIndex,
        sid: &str,
        mask: Option<&[bool]>,
    ) -> Arc<FolderTotals> {
        const KEPT: usize = 2;
        let key = (device.id, if mask.is_some() { sid.to_string() } else { String::new() });
        let mut cache = self.space_totals.lock().unwrap();
        if let Some(i) = cache.iter().position(|(k, t)| *k == key && t.is_current(ix)) {
            let hit = cache.remove(i);
            let t = hit.1.clone();
            cache.push(hit);
            return t;
        }
        cache.retain(|(k, _)| *k != key);
        drop(cache);
        let t = Arc::new(FolderTotals::compute(ix, mask));
        let mut cache = self.space_totals.lock().unwrap();
        cache.push((key, t.clone()));
        let excess = cache.len().saturating_sub(KEPT);
        cache.drain(..excess);
        t
    }

    /// Every device that has an index, in registry order, loaded.
    pub fn snapshot(&self, why: &'static str) -> Vec<Snapshot> {
        let devices = self.visible_devices();
        let ids: Vec<DeviceId> = devices.iter().map(|d| d.id).collect();
        self.ensure_loaded(&ids, why);
        let names: HashMap<HostId, String> = {
            let peers = self.peers.lock().unwrap();
            peers.file.peers.iter().map(|p| (p.host_id, p.name.clone())).collect()
        };
        let (me, my_name) = (self.host_id(), self.pc_name());
        let indexes = self.indexes.read().unwrap();
        devices
            .into_iter()
            .filter_map(|d| {
                let pc = if d.indexed_by_host == me {
                    my_name.clone()
                } else {
                    names.get(&d.indexed_by_host).cloned().unwrap_or_else(|| "Linked PC".into())
                };
                indexes.get(&d.id).map(|ix| Snapshot {
                    online: matches!(d.state, DeviceState::Online | DeviceState::Scanning),
                    index: ix.clone(),
                    device: d,
                    pc,
                })
            })
            .collect()
    }
}

impl State {
    /// `indexes/*.seekidx` and `journal/*.log` whose name is not the id of any device in the
    /// registry. Only reads. The folder is listed before the registry, so a drive added while this
    /// runs is never mistaken for a stranger.
    pub fn stale_data(&self) -> Vec<seek_ipc::StaleFile> {
        let mut found = Vec::new();
        for (sub, ext) in [("indexes", "seekidx"), ("journal", "log")] {
            let Ok(rd) = std::fs::read_dir(self.cfg.data_dir.join(sub)) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                let is_ours = path.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext));
                let id = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<DeviceId>().ok());
                let (true, Some(id)) = (is_ours, id) else { continue };
                let Ok(md) = entry.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let modified = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64);
                found.push((id, seek_ipc::StaleFile { path: path.to_string_lossy().into_owned(), size: md.len(), modified }));
            }
        }
        let reg = self.registry.read().unwrap();
        let mut out: Vec<seek_ipc::StaleFile> =
            found.into_iter().filter(|(id, _)| reg.get(*id).is_none()).map(|(_, f)| f).collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }
}

fn index_path(dir: &Path, id: DeviceId) -> PathBuf {
    dir.join("indexes").join(format!("{id}.seekidx"))
}
