//! Volume arrival/removal, scanning and live updates.

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use seek_core::{Checkpoint, Device, DeviceId, DeviceState};
use seek_ipc::Event;
use seek_scan::{DirWatcher, FollowError, ScanError, ScanResult, UsnFollower, VolumeInfo};

use crate::state::{now_ms, State};

/// Where volumes come from. The real implementation uses `seek-scan`; tests use fakes.
pub trait VolumeSource: Send + Sync + 'static {
    fn list(&self) -> Vec<VolumeInfo>;
    fn scan(&self, v: &VolumeInfo, progress: &(dyn Fn(u64) + Sync)) -> Result<ScanResult, ScanError>;
    /// Resume following the USN journal; `Err` means a full rescan is needed.
    fn follow(&self, v: &VolumeInfo, cp: Checkpoint) -> Result<UsnFollower, FollowError>;
    /// Watch a non-NTFS volume for changes; `None` when watching isn't possible.
    fn watch(&self, v: &VolumeInfo, on_change: Box<dyn Fn() + Send>) -> Option<DirWatcher>;
}

pub struct RealVolumes;

impl VolumeSource for RealVolumes {
    fn list(&self) -> Vec<VolumeInfo> {
        seek_scan::list_volumes()
    }
    fn scan(&self, v: &VolumeInfo, progress: &(dyn Fn(u64) + Sync)) -> Result<ScanResult, ScanError> {
        seek_scan::scan_volume(v, progress)
    }
    fn follow(&self, v: &VolumeInfo, cp: Checkpoint) -> Result<UsnFollower, FollowError> {
        UsnFollower::open(&v.mount_point, cp)
    }
    fn watch(&self, v: &VolumeInfo, on_change: Box<dyn Fn() + Send>) -> Option<DirWatcher> {
        DirWatcher::new(std::path::Path::new(&v.mount_point), on_change)
            .map_err(|e| tracing::warn!("cannot watch {}: {e}", v.mount_point))
            .ok()
    }
}

fn is_ntfs(v: &VolumeInfo) -> bool {
    v.filesystem.eq_ignore_ascii_case("NTFS")
}

/// One pass: detect new, returning and removed volumes.
pub fn poll_volumes(state: &Arc<State>) {
    let vols = state.source.list();
    let mut present = HashSet::new();
    for v in vols {
        let existing = state.registry.read().unwrap().find_by_identity(&v.identity).map(|d| d.id);
        let id = existing.unwrap_or_else(|| register_volume(state, &v));
        present.insert(id);
        let already = state.mounted.lock().unwrap().insert(id, v.clone()).is_some();
        if !already {
            tracing::info!("volume {} ({}) is online", v.mount_point, v.filesystem);
            state.update_device(id, |d| {
                d.last_mount_point = v.mount_point.clone();
                d.label = v.label.clone();
                d.filesystem = v.filesystem.clone();
                d.kind = v.kind;
                d.total_bytes = v.total_bytes;
                d.free_bytes = v.free_bytes;
                d.last_seen = now_ms();
                d.state = DeviceState::Online;
                d.error = None;
            });
            state.invalidate();
            bring_online(state, id, &v);
        }
    }
    let gone: Vec<DeviceId> = state
        .mounted
        .lock()
        .unwrap()
        .keys()
        .filter(|id| !present.contains(id))
        .copied()
        .collect();
    for id in gone {
        go_offline(state, id);
    }
    start_pending_rescans(state);
}

/// Minimum pause between two watcher-triggered rescans of a device: 10× its last scan time,
/// between 10 s and 15 min (a walked C: of ~60 s rescans at most every 10 min).
fn rescan_cooldown(last_took: Duration) -> Duration {
    (last_took * 10).clamp(Duration::from_secs(10), Duration::from_secs(15 * 60))
}

/// Starts the rescans that watchers asked for, once each device is idle and out of its cooldown.
/// A change during a running scan stays pending, so it is picked up by the next scan.
fn start_pending_rescans(state: &Arc<State>) {
    let pending: Vec<DeviceId> = state.rescan_pending.lock().unwrap().iter().copied().collect();
    for id in pending {
        if state.scanning.lock().unwrap().contains_key(&id) {
            continue;
        }
        if let Some(&(ended, took)) = state.last_scan.lock().unwrap().get(&id) {
            if ended.elapsed() < rescan_cooldown(took) {
                continue;
            }
        }
        state.rescan_pending.lock().unwrap().remove(&id);
        let v = state.mounted.lock().unwrap().get(&id).cloned();
        if let Some(v) = v {
            start_scan(state, id, v);
        }
    }
}

fn register_volume(state: &Arc<State>, v: &VolumeInfo) -> DeviceId {
    let now = now_ms();
    let name = if v.label.is_empty() {
        format!("Local Disk ({})", v.mount_point.trim_end_matches('\\'))
    } else {
        format!("{} ({})", v.label, v.mount_point.trim_end_matches('\\'))
    };
    let d = Device {
        id: uuid::Uuid::new_v4(),
        identity: v.identity.clone(),
        display_name: name,
        label: v.label.clone(),
        filesystem: v.filesystem.clone(),
        kind: v.kind,
        total_bytes: v.total_bytes,
        free_bytes: v.free_bytes,
        last_mount_point: v.mount_point.clone(),
        indexed_by_host: state.host_id(),
        first_seen: now,
        last_seen: now,
        index_updated_at: 0,
        state: DeviceState::Offline,
        entry_count: 0,
        error: None,
    };
    let id = d.id;
    tracing::info!("new device {} ({})", d.display_name, d.filesystem);
    state.registry.write().unwrap().add(d);
    state.save_registry();
    id
}

fn bring_online(state: &Arc<State>, id: DeviceId, v: &VolumeInfo) {
    let has_index = state.indexes.read().unwrap().contains_key(&id);
    let forced = state.needs_rescan.lock().unwrap().remove(&id);
    if !has_index || forced {
        start_scan(state, id, v.clone());
        return;
    }
    // The stored index stays searchable while we catch up.
    if is_ntfs(v) {
        let cp = state.indexes.read().unwrap().get(&id).and_then(|ix| ix.read().unwrap().checkpoint);
        if let Some(cp) = cp {
            match state.source.follow(v, cp) {
                Ok(f) => {
                    tracing::info!("{}: resuming USN journal", v.mount_point);
                    state.followers.lock().unwrap().insert(id, f);
                    return;
                }
                Err(e) => tracing::info!("{}: cannot resume journal ({e}); rescanning", v.mount_point),
            }
        }
    }
    start_scan(state, id, v.clone());
}

/// Live updates for a volume without a USN journal (non-NTFS, or NTFS scanned without admin rights):
/// a directory watcher marks the device for a rescan, which `start_pending_rescans` rate-limits.
fn attach_watcher(state: &Arc<State>, id: DeviceId, v: &VolumeInfo) {
    if state.watchers.lock().unwrap().contains_key(&id) {
        return;
    }
    let weak = Arc::downgrade(state);
    let on_change = Box::new(move || {
        if let Some(st) = weak.upgrade() {
            st.rescan_pending.lock().unwrap().insert(id);
        }
    });
    if let Some(w) = state.source.watch(v, on_change) {
        state.watchers.lock().unwrap().insert(id, w);
    }
}

fn go_offline(state: &Arc<State>, id: DeviceId) {
    state.mounted.lock().unwrap().remove(&id);
    state.followers.lock().unwrap().remove(&id);
    state.watchers.lock().unwrap().remove(&id);
    state.rescan_pending.lock().unwrap().remove(&id);
    if state.dirty.lock().unwrap().contains(&id) {
        state.save_index(id);
    }
    state.update_device(id, |d| {
        d.state = DeviceState::Offline;
        d.last_seen = now_ms();
    });
    state.invalidate();
    tracing::info!("device {id} went offline");
}

/// Full scan on a background thread. The previous index (if any) stays searchable until the
/// new one replaces it; on failure it is kept.
pub fn start_scan(state: &Arc<State>, id: DeviceId, v: VolumeInfo) {
    {
        let mut scanning = state.scanning.lock().unwrap();
        if scanning.contains_key(&id) {
            return;
        }
        scanning.insert(id, 0);
    }
    state.update_device(id, |d| d.state = DeviceState::Scanning);
    let state = state.clone();
    std::thread::spawn(move || {
        tracing::info!("scanning {}", v.mount_point);
        let started = std::time::Instant::now();
        let last_emit = AtomicI64::new(0);
        let progress = |n: u64| {
            let now = now_ms();
            let prev = last_emit.load(Ordering::Relaxed);
            if now - prev >= 250
                && last_emit.compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
            {
                state.scanning.lock().unwrap().insert(id, n);
                state.emit(Event::IndexProgress { device_id: id, scanned: n, done: false });
            }
        };
        let result = state.source.scan(&v, &progress);
        let still_mounted = state.mounted.lock().unwrap().contains_key(&id);
        match result {
            Ok(r) => {
                let mut ix = r.index;
                ix.checkpoint = r.checkpoint;
                let count = ix.live_count() as u64;
                tracing::info!(
                    "{}: {count} entries in {:.1}s ({} skipped, journal: {})",
                    v.mount_point,
                    started.elapsed().as_secs_f64(),
                    r.skipped,
                    r.checkpoint.is_some()
                );
                state.indexes.write().unwrap().insert(id, Arc::new(RwLock::new(ix)));
                state.mark_dirty(id);
                state.save_index(id);
                // Read before update_device: it holds the registry write lock while running the closure.
                let host = state.host_id();
                state.update_device(id, |d| {
                    d.entry_count = count;
                    d.index_updated_at = now_ms();
                    d.indexed_by_host = host;
                    d.state = if still_mounted { DeviceState::Online } else { DeviceState::Offline };
                    d.error = None;
                });
                // The previous follower tracked the old index (and, after a walker fallback, the ids no
                // longer are file reference numbers at all): never let it touch the new one.
                state.followers.lock().unwrap().remove(&id);
                if still_mounted {
                    let follower = r.checkpoint.and_then(|cp| {
                        state
                            .source
                            .follow(&v, cp)
                            .map_err(|e| tracing::info!("{}: no journal ({e}); watching instead", v.mount_point))
                            .ok()
                    });
                    match follower {
                        Some(f) => {
                            state.watchers.lock().unwrap().remove(&id);
                            state.followers.lock().unwrap().insert(id, f);
                        }
                        None => attach_watcher(&state, id, &v),
                    }
                }
                state.invalidate();
            }
            Err(e) => {
                tracing::error!("scan of {} failed: {e}", v.mount_point);
                state.update_device(id, |d| {
                    if still_mounted {
                        d.state = DeviceState::Error;
                        d.error = Some(e.to_string());
                    } else {
                        d.state = DeviceState::Offline;
                    }
                });
            }
        }
        state.last_scan.lock().unwrap().insert(id, (Instant::now(), started.elapsed()));
        state.scanning.lock().unwrap().remove(&id);
        let scanned = state.device(id).map(|d| d.entry_count).unwrap_or(0);
        state.emit(Event::IndexProgress { device_id: id, scanned, done: true });
    });
}

/// One pass over every USN follower: apply changes, handle journal resets.
pub fn poll_journals(state: &Arc<State>) {
    let mut rescan = Vec::new();
    let mut changed = Vec::new();
    {
        let mut followers = state.followers.lock().unwrap();
        followers.retain(|&id, f| match f.poll() {
            Ok(changes) => {
                if !changes.is_empty() {
                    changed.push((id, changes));
                }
                true
            }
            Err(FollowError::JournalReset) => {
                rescan.push(id);
                false
            }
            Err(e) => {
                tracing::warn!("journal follow for {id} stopped: {e}");
                false
            }
        });
    }
    for (id, changes) in changed {
        let Some(ix) = state.indexes.read().unwrap().get(&id).cloned() else { continue };
        let count = {
            let mut ix = ix.write().unwrap();
            ix.apply(&changes);
            if let Some(cp) = state.followers.lock().unwrap().get(&id).map(|f| f.checkpoint()) {
                ix.checkpoint = Some(cp);
            }
            ix.live_count() as u64
        };
        state.mark_dirty(id);
        let mut reg = state.registry.write().unwrap();
        if let Some(d) = reg.get_mut(id) {
            d.entry_count = count;
            d.index_updated_at = now_ms();
        }
        drop(reg);
        state.invalidate();
    }
    for id in rescan {
        let v = state.mounted.lock().unwrap().get(&id).cloned();
        if let Some(v) = v {
            start_scan(state, id, v);
        }
    }
}

/// Rescan a present local volume on request.
pub fn rescan(state: &Arc<State>, id: DeviceId) -> Result<(), String> {
    let v = state.mounted.lock().unwrap().get(&id).cloned();
    match v {
        Some(v) => {
            start_scan(state, id, v);
            Ok(())
        }
        None => Err("device is not connected".into()),
    }
}
