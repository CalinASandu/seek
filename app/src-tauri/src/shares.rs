//! Locations only the user's session can see are scanned here and pushed to the service: the service
//! runs as SYSTEM and cannot see the user's network shares (SMB credentials) or per-user virtual drives
//! such as Google Drive's `G:` or mapped drive letters.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use seek_core::{encode_index, DeviceId, DeviceIdentity, DeviceKind, IndexHeader, INDEX_FORMAT_VERSION};
use seek_ipc::{Request, Response, UserVolume};
use seek_scan::DirWatcher;
use tokio::sync::mpsc;

use crate::pipe::ServiceLink;

/// Full refresh of every share and session-only drive.
const REFRESH_EVERY: Duration = Duration::from_secs(10 * 60);
/// How often drive letters are checked (Google Drive starting/stopping, a drive being mapped).
const VOLUMES_EVERY: Duration = Duration::from_secs(15);
/// Change-triggered re-walks of one location happen at most this often.
const MIN_RESCAN_GAP: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum ShareJob {
    /// Rescan everything.
    All,
    /// Rescan one device now (user request, newly added share).
    One(DeviceId),
    /// The watcher saw changes: rescan, but not more often than `MIN_RESCAN_GAP`.
    Changed(DeviceId),
    /// Look for session-only drives that appeared or went away.
    Volumes,
}

pub type ShareQueue = mpsc::UnboundedSender<ShareJob>;

pub fn start(link: Arc<ServiceLink>) -> ShareQueue {
    let (tx, rx) = mpsc::unbounded_channel();
    let timer_tx = tx.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut last_full = None::<Instant>;
        loop {
            let full = last_full.is_none_or(|t| t.elapsed() >= REFRESH_EVERY);
            let job = if full { ShareJob::All } else { ShareJob::Volumes };
            if full {
                last_full = Some(Instant::now());
            }
            if timer_tx.send(job).is_err() {
                break;
            }
            tokio::time::sleep(VOLUMES_EVERY).await;
        }
    });
    tauri::async_runtime::spawn(worker(link, rx, tx.clone()));
    tx
}

/// Session-only drive letters, as the service should see them.
fn user_volumes() -> Vec<UserVolume> {
    seek_scan::list_volumes()
        .into_iter()
        .map(|v| UserVolume {
            mount_point: v.mount_point,
            identity: v.identity,
            label: v.label,
            filesystem: v.filesystem,
            kind: v.kind,
            total_bytes: v.total_bytes,
            free_bytes: v.free_bytes,
        })
        .collect()
}

struct Worker {
    link: Arc<ServiceLink>,
    tx: ShareQueue,
    watchers: HashMap<DeviceId, DirWatcher>,
    /// Session-only drives currently present → their root (e.g. `G:\`).
    volumes: HashMap<DeviceId, PathBuf>,
    last_scan: HashMap<DeviceId, Instant>,
    /// Devices with a delayed rescan already scheduled.
    delayed: HashSet<DeviceId>,
}

async fn worker(link: Arc<ServiceLink>, mut rx: mpsc::UnboundedReceiver<ShareJob>, tx: ShareQueue) {
    let mut w = Worker {
        link,
        tx,
        watchers: HashMap::new(),
        volumes: HashMap::new(),
        last_scan: HashMap::new(),
        delayed: HashSet::new(),
    };
    while let Some(job) = rx.recv().await {
        match job {
            ShareJob::Volumes => {
                for id in w.refresh_volumes().await {
                    w.scan(id).await;
                }
            }
            ShareJob::All => {
                w.refresh_volumes().await;
                for id in w.targets().await.into_keys() {
                    w.scan(id).await;
                }
            }
            ShareJob::One(id) => {
                w.delayed.remove(&id);
                w.scan(id).await;
            }
            ShareJob::Changed(id) => {
                let wait = w.last_scan.get(&id).map(|t| MIN_RESCAN_GAP.saturating_sub(t.elapsed()));
                match wait {
                    Some(d) if !d.is_zero() => {
                        if w.delayed.insert(id) {
                            let q = w.tx.clone();
                            tauri::async_runtime::spawn(async move {
                                tokio::time::sleep(d).await;
                                let _ = q.send(ShareJob::One(id));
                            });
                        }
                    }
                    _ => w.scan(id).await,
                }
            }
        }
    }
}

impl Worker {
    /// Reports this session's drive letters to the service; returns devices that just appeared here.
    /// Drives that disappeared are marked offline (their index stays searchable).
    async fn refresh_volumes(&mut self) -> Vec<DeviceId> {
        let vols = tokio::task::spawn_blocking(user_volumes).await.unwrap_or_default();
        let mut present = HashMap::new();
        for v in vols {
            let root = PathBuf::from(&v.mount_point);
            // `Ok` = the service sees this volume itself; an error = an older service without this request.
            if let Ok(Response::Device { device }) = self.link.request(Request::AddUserVolume { volume: v }).await {
                present.insert(device.id, root);
            }
        }
        let gone: Vec<DeviceId> = self.volumes.keys().filter(|id| !present.contains_key(id)).copied().collect();
        for id in gone {
            self.watchers.remove(&id);
            let _ = self.link.request(Request::SetShareState { device_id: id, online: false }).await;
        }
        let new = present.keys().filter(|id| !self.volumes.contains_key(id)).copied().collect();
        self.volumes = present;
        new
    }

    /// Everything this worker feeds: network shares plus session-only drives, by device.
    async fn targets(&self) -> HashMap<DeviceId, PathBuf> {
        let mut out = self.volumes.clone();
        if let Ok(Response::Devices { devices }) = self.link.request(Request::ListDevices).await {
            for d in devices {
                if let (DeviceKind::Network, DeviceIdentity::Share { unc }) = (&d.kind, &d.identity) {
                    out.insert(d.id, PathBuf::from(unc));
                }
            }
        }
        out
    }

    async fn scan(&mut self, id: DeviceId) {
        let Some(root) = self.targets().await.remove(&id) else { return };
        let probe = root.clone();
        let reachable = tokio::task::spawn_blocking(move || probe.is_dir()).await.unwrap_or(false);
        if !reachable {
            self.watchers.remove(&id);
            let _ = self.link.request(Request::SetShareState { device_id: id, online: false }).await;
            return;
        }
        self.last_scan.insert(id, Instant::now());
        let walk_root = root.clone();
        let encoded = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let r = seek_scan::scan_walk(&walk_root, &|_| {}).map_err(|e| e.to_string())?;
            let mut ix = r.index;
            let header = IndexHeader {
                format_version: INDEX_FORMAT_VERSION,
                device_id: id,
                host_id: uuid::Uuid::nil(),
                checkpoint: None,
                saved_at: 0,
                rows: 0,
            sync_cursor: None,
            };
            let bytes = encode_index(&header, &mut ix).map_err(|e| e.to_string())?;
            Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
        })
        .await;
        let shown = root.display();
        match encoded {
            Ok(Ok(data_b64)) => {
                if let Err(e) = self.link.request(Request::PushShareIndex { device_id: id, data_b64 }).await {
                    tracing::warn!("pushing index for {shown} failed: {e}");
                }
            }
            Ok(Err(e)) => tracing::warn!("scanning {shown} failed: {e}"),
            Err(e) => tracing::warn!("scan task for {shown} panicked: {e}"),
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = self.watchers.entry(id) {
            let q = self.tx.clone();
            // Shares are re-walked whole and pushed (the service holds the index), so which folders
            // changed does not matter here.
            if let Ok(w) = DirWatcher::new(&root, move |_| {
                let _ = q.send(ShareJob::Changed(id));
            }) {
                slot.insert(w);
            }
        }
    }
}
