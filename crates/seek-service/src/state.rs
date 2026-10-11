//! Shared service state: device registry, in-memory indexes and live bookkeeping.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use seek_core::{
    load_index, save_index, Device, DeviceId, DeviceIndex, DeviceRegistry, DeviceState, IndexHeader,
    INDEX_FORMAT_VERSION,
};
use seek_ipc::Event;
use seek_scan::{DirWatcher, UsnFollower, VolumeInfo};
use tokio::sync::broadcast;

use crate::monitor::VolumeSource;

pub struct Config {
    pub data_dir: PathBuf,
    pub pipe_name: String,
}

pub type SharedIndex = Arc<RwLock<DeviceIndex>>;

pub struct State {
    pub cfg: Config,
    pub source: Arc<dyn VolumeSource>,
    pub registry: RwLock<DeviceRegistry>,
    pub indexes: RwLock<HashMap<DeviceId, SharedIndex>>,
    pub dirty: Mutex<HashSet<DeviceId>>,
    /// Local volumes currently present, by device.
    pub mounted: Mutex<HashMap<DeviceId, VolumeInfo>>,
    pub followers: Mutex<HashMap<DeviceId, UsnFollower>>,
    pub watchers: Mutex<HashMap<DeviceId, DirWatcher>>,
    /// Devices with a scan in progress → entries scanned so far.
    pub scanning: Mutex<HashMap<DeviceId, u64>>,
    /// Devices whose stored index was unusable; scanned as soon as they are present.
    pub needs_rescan: Mutex<HashSet<DeviceId>>,
    /// Watched devices that changed since their last scan started; rescanned once their cooldown ends.
    pub rescan_pending: Mutex<HashSet<DeviceId>>,
    /// When each device's last scan finished and how long it took.
    pub last_scan: Mutex<HashMap<DeviceId, (Instant, Duration)>>,
    /// Volumes only the user's session can see (reported by the app, which scans and pushes them).
    pub user_volumes: Mutex<HashSet<DeviceId>>,
    pub events: broadcast::Sender<Event>,
    pub invalidated: AtomicBool,
}

/// One device as seen by a search: a snapshot of its record plus its index.
pub struct Snapshot {
    pub device: Device,
    pub index: SharedIndex,
    pub online: bool,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl State {
    /// Loads the registry and every stored index. Unreadable indexes are dropped and their
    /// devices queued for a rescan; an unreadable registry is moved aside and recreated.
    pub fn load(cfg: Config, source: Arc<dyn VolumeSource>) -> anyhow::Result<Arc<State>> {
        std::fs::create_dir_all(cfg.data_dir.join("indexes"))?;
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
        let mut needs_rescan = HashSet::new();
        for d in &mut registry.devices {
            d.state = DeviceState::Offline;
            let path = index_path(&cfg.data_dir, d.id);
            match load_index(&path) {
                Ok((hdr, ix)) if hdr.device_id == d.id => {
                    indexes.insert(d.id, Arc::new(RwLock::new(ix)));
                }
                Ok(_) => {
                    tracing::warn!("index file for {} belongs to another device; rescanning", d.display_name);
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
        let state = Arc::new(State {
            cfg,
            source,
            registry: RwLock::new(registry),
            indexes: RwLock::new(indexes),
            dirty: Mutex::new(HashSet::new()),
            mounted: Mutex::new(HashMap::new()),
            followers: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            scanning: Mutex::new(HashMap::new()),
            needs_rescan: Mutex::new(needs_rescan),
            rescan_pending: Mutex::new(HashSet::new()),
            last_scan: Mutex::new(HashMap::new()),
            user_volumes: Mutex::new(HashSet::new()),
            events,
            invalidated: AtomicBool::new(false),
        });
        state.save_registry();
        Ok(state)
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
        let Some(ix) = self.indexes.read().unwrap().get(&id).cloned() else { return };
        let header = IndexHeader {
            format_version: INDEX_FORMAT_VERSION,
            device_id: id,
            host_id: self.host_id(),
            checkpoint: None,
            saved_at: now_ms(),
            rows: 0,
        };
        let mut ix = ix.write().unwrap();
        let header = IndexHeader { checkpoint: ix.checkpoint, ..header };
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

    /// Every device that has an index, in registry order.
    pub fn snapshot(&self) -> Vec<Snapshot> {
        let reg = self.registry.read().unwrap();
        let indexes = self.indexes.read().unwrap();
        reg.devices
            .iter()
            .filter_map(|d| {
                indexes.get(&d.id).map(|ix| Snapshot {
                    device: d.clone(),
                    index: ix.clone(),
                    online: matches!(d.state, DeviceState::Online | DeviceState::Scanning),
                })
            })
            .collect()
    }
}

fn index_path(dir: &Path, id: DeviceId) -> PathBuf {
    dir.join("indexes").join(format!("{id}.seekidx"))
}
