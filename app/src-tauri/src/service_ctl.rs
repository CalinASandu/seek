//! Installing and locating the Seek Windows service.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub const SERVICE_NAME: &str = "SeekService";

pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// `seek-service.exe` next to the app (installed / release build), else `SEEK_SERVICE_PATH`.
pub fn service_exe() -> Option<PathBuf> {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        for name in ["seek-service.exe", "seek-service-x86_64-pc-windows-msvc.exe"] {
            let p = dir.join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    std::env::var_os("SEEK_SERVICE_PATH").map(PathBuf::from).filter(|p| p.exists())
}

/// Runs `seek-service.exe install` elevated (one UAC prompt) and waits for the pipe to answer.
pub async fn install() -> Result<(), String> {
    let exe = service_exe().ok_or("seek-service.exe not found next to Seek (set SEEK_SERVICE_PATH in development)")?;
    crate::shell::run_elevated(&exe, "install")?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        if seek_ipc::Client::connect().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("the Seek service did not start within 30 seconds".into())
}
