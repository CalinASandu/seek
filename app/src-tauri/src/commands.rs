//! Tauri commands — one per method of `SeekApi` in `app/src/api/types.ts`.

use std::sync::Arc;

use seek_core::{Device, DeviceId, ResultRow, SortKey};
use seek_ipc::{PcInfo, Request, Response, StatusInfo};
use serde_json::{json, Value};
use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt;

use crate::pipe::ServiceLink;
use crate::settings::{self, Settings};
use crate::shares::{ShareJob, ShareQueue};
use crate::{service_ctl, shell};

type Link<'a> = State<'a, Arc<ServiceLink>>;

fn unexpected(r: Response) -> String {
    format!("unexpected reply from service: {r:?}")
}

async fn expect_ok(link: &ServiceLink, req: Request) -> Result<(), String> {
    match link.request(req).await? {
        Response::Ok => Ok(()),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn search(link: Link<'_>, query: String, sort: SortKey, descending: bool) -> Result<Value, String> {
    match link.request(Request::Search { query, sort, descending }).await? {
        Response::SearchStarted { search_id, total, elapsed_ms } => {
            Ok(json!({ "ok": true, "searchId": search_id, "total": total, "elapsedMs": elapsed_ms }))
        }
        Response::SearchError { message, position } => {
            Ok(json!({ "ok": false, "message": message, "position": position }))
        }
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn fetch_rows(link: Link<'_>, search_id: u64, offset: u32, limit: u32) -> Result<Vec<ResultRow>, String> {
    match link.request(Request::FetchRows { search_id, offset, limit }).await? {
        Response::Rows { rows, .. } => Ok(rows),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn list_devices(link: Link<'_>) -> Result<Vec<Device>, String> {
    match link.request(Request::ListDevices).await? {
        Response::Devices { devices } => Ok(devices),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn rename_device(link: Link<'_>, id: DeviceId, name: String) -> Result<(), String> {
    expect_ok(&link, Request::RenameDevice { id, name }).await
}

#[tauri::command]
pub async fn forget_device(link: Link<'_>, id: DeviceId) -> Result<(), String> {
    expect_ok(&link, Request::ForgetDevice { id }).await
}

#[tauri::command]
pub async fn rescan_device(link: Link<'_>, shares: State<'_, ShareQueue>, id: DeviceId) -> Result<(), String> {
    match link.request(Request::RescanDevice { id }).await {
        Ok(Response::Ok) => Ok(()),
        // Network shares and session-only drives (e.g. Google Drive) are scanned by the app.
        Err(e) if e.contains("network shares") => {
            let _ = shares.send(ShareJob::One(id));
            Ok(())
        }
        Ok(other) => Err(unexpected(other)),
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub async fn add_network_share(link: Link<'_>, shares: State<'_, ShareQueue>, unc: String) -> Result<Device, String> {
    match link.request(Request::AddNetworkShare { unc }).await? {
        Response::Device { device } => {
            let _ = shares.send(ShareJob::One(device.id));
            Ok(device)
        }
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn status(link: Link<'_>) -> Result<StatusInfo, String> {
    match link.request(Request::Status).await? {
        Response::Status(s) => Ok(s),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn open_path(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || shell::open_path(&path)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn reveal_path(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || shell::reveal_path(&path)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn service_state(link: Link<'_>) -> Result<&'static str, String> {
    if link.request(Request::Status).await.is_ok() {
        return Ok("connected");
    }
    let installed = tauri::async_runtime::spawn_blocking(service_ctl::is_installed).await.unwrap_or(false);
    Ok(if installed { "connecting" } else { "notInstalled" })
}

#[tauri::command]
pub async fn install_service() -> Result<(), String> {
    service_ctl::install().await
}

#[tauri::command]
pub fn get_settings() -> Settings {
    settings::load()
}

#[tauri::command]
pub fn set_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let autolaunch = app.autolaunch();
    let result = if settings.start_with_windows { autolaunch.enable() } else { autolaunch.disable() };
    if let Err(e) = result {
        tracing::warn!("updating start-with-Windows failed: {e}");
    }
    settings::save(&settings)
}

// ---- linked PCs ----

#[tauri::command]
pub async fn start_pairing(link: Link<'_>) -> Result<Value, String> {
    match link.request(Request::StartPairing).await? {
        Response::PairingCode { code, expires_ms } => Ok(json!({ "code": code, "expiresMs": expires_ms })),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn discovered_pcs(link: Link<'_>) -> Result<Vec<PcInfo>, String> {
    match link.request(Request::DiscoveredPcs).await? {
        Response::Pcs { pcs } => Ok(pcs),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn join_pairing(link: Link<'_>, addr: String, code: String) -> Result<(), String> {
    expect_ok(&link, Request::JoinPairing { addr, code }).await
}

#[tauri::command]
pub async fn list_peers(link: Link<'_>) -> Result<Value, String> {
    match link.request(Request::ListPeers).await? {
        Response::Peers { this_pc, peers, sharing, unshared } => {
            Ok(json!({ "thisPc": this_pc, "peers": peers, "sharing": sharing, "unshared": unshared }))
        }
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn unlink(link: Link<'_>, host_id: uuid::Uuid) -> Result<(), String> {
    expect_ok(&link, Request::Unlink { host_id }).await
}

#[tauri::command]
pub async fn rename_peer(link: Link<'_>, host_id: uuid::Uuid, name: String) -> Result<(), String> {
    expect_ok(&link, Request::RenamePeer { host_id, name }).await
}

#[tauri::command]
pub async fn sync_now(link: Link<'_>) -> Result<(), String> {
    expect_ok(&link, Request::SyncNow).await
}

#[tauri::command]
pub async fn set_sharing(link: Link<'_>, enabled: bool) -> Result<(), String> {
    expect_ok(&link, Request::SetSharing { enabled }).await
}

#[tauri::command]
pub async fn set_device_shared(link: Link<'_>, device_id: DeviceId, shared: bool) -> Result<(), String> {
    expect_ok(&link, Request::SetDeviceShared { device_id, shared }).await
}

#[tauri::command]
pub async fn space_tree(link: Link<'_>, device_id: DeviceId, path: String, depth: u8) -> Result<Value, String> {
    match link.request(Request::SpaceTree { device_id, path, depth }).await? {
        Response::SpaceTree { node } => serde_json::to_value(node).map_err(|e| e.to_string()),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn space_largest(link: Link<'_>, device_id: DeviceId, path: String, limit: u32) -> Result<Value, String> {
    match link.request(Request::SpaceLargest { device_id, path, limit }).await? {
        Response::SpaceFiles { files } => serde_json::to_value(files).map_err(|e| e.to_string()),
        other => Err(unexpected(other)),
    }
}

#[tauri::command]
pub async fn space_types(link: Link<'_>, device_id: DeviceId, path: String) -> Result<Value, String> {
    match link.request(Request::SpaceTypes { device_id, path }).await? {
        Response::SpaceTypes { types } => serde_json::to_value(types).map_err(|e| e.to_string()),
        other => Err(unexpected(other)),
    }
}
