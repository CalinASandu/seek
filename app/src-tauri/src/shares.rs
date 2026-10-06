//! Network shares are scanned here, in the user's session: the service runs as SYSTEM and cannot
//! see the user's mapped drives or SMB credentials. Finished indexes are pushed to the service.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use seek_core::{encode_index, DeviceId, DeviceIdentity, DeviceKind, IndexHeader, INDEX_FORMAT_VERSION};
use seek_ipc::{Request, Response};
use seek_scan::DirWatcher;
use tokio::sync::mpsc;

use crate::pipe::ServiceLink;

const REFRESH_EVERY: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
pub enum ShareJob {
    All,
    One(DeviceId),
}

pub type ShareQueue = mpsc::UnboundedSender<ShareJob>;

pub fn start(link: Arc<ServiceLink>) -> ShareQueue {
    let (tx, rx) = mpsc::unbounded_channel();
    let timer_tx = tx.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            if timer_tx.send(ShareJob::All).is_err() {
                break;
            }
            tokio::time::sleep(REFRESH_EVERY).await;
        }
    });
    tauri::async_runtime::spawn(worker(link, rx, tx.clone()));
    tx
}

async fn worker(link: Arc<ServiceLink>, mut rx: mpsc::UnboundedReceiver<ShareJob>, tx: ShareQueue) {
    let mut watchers: HashMap<DeviceId, DirWatcher> = HashMap::new();
    while let Some(job) = rx.recv().await {
        let Ok(Response::Devices { devices }) = link.request(Request::ListDevices).await else { continue };
        let shares = devices.into_iter().filter_map(|d| match (&d.kind, &d.identity) {
            (DeviceKind::Network, DeviceIdentity::Share { unc }) => Some((d.id, unc.clone())),
            _ => None,
        });
        for (id, unc) in shares {
            if let ShareJob::One(want) = job {
                if want != id {
                    continue;
                }
            }
            let root = PathBuf::from(&unc);
            let probe = root.clone();
            let reachable = tokio::task::spawn_blocking(move || probe.is_dir()).await.unwrap_or(false);
            if !reachable {
                watchers.remove(&id);
                let _ = link.request(Request::SetShareState { device_id: id, online: false }).await;
                continue;
            }
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
                };
                let bytes = encode_index(&header, &mut ix).map_err(|e| e.to_string())?;
                Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
            })
            .await;
            match encoded {
                Ok(Ok(data_b64)) => {
                    if let Err(e) = link.request(Request::PushShareIndex { device_id: id, data_b64 }).await {
                        tracing::warn!("pushing index for {unc} failed: {e}");
                    }
                }
                Ok(Err(e)) => tracing::warn!("scanning {unc} failed: {e}"),
                Err(e) => tracing::warn!("scan task for {unc} panicked: {e}"),
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = watchers.entry(id) {
                let q = tx.clone();
                if let Ok(w) = DirWatcher::new(&root, move || {
                    let _ = q.send(ShareJob::One(id));
                }) {
                    slot.insert(w);
                }
            }
        }
    }
}
