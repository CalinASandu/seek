//! Tauri commands — one per method of `SeekApi` in `app/src/api/types.ts`.

use std::sync::Arc;

use seek_core::{Device, DeviceId, ResultRow, SortKey};
use seek_ipc::{Request, Response, StatusInfo};
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
        // Network shares are scanned by the app, not the service.
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
