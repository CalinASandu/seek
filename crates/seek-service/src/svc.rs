//! Windows service integration: install, uninstall and the service entry point.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use seek_service::{default_data_dir, default_sync_config, Config, RealVolumes, SYNC_PORT};

pub const SERVICE_NAME: &str = "SeekService";
const DISPLAY_NAME: &str = "Seek indexing service";
const FIREWALL_RULE: &str = "Seek sync";

fn netsh(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("netsh").args(["advfirewall", "firewall"]).args(args).output()
}

/// Lets linked PCs reach the sync port on any network type (Windows marks new networks Public by default),
/// but only from the local subnet, never from the internet. Sessions need a linked PC's key and pairing
/// needs the code shown on screen. A failure is a warning: Seek still works on this PC.
pub fn add_firewall_rule() {
    let _ = netsh(&["delete", "rule", &format!("name={FIREWALL_RULE}")]);
    let exe = match std::env::current_exe() {
        Ok(p) => p.display().to_string(),
        Err(e) => {
            eprintln!("warning: firewall rule not added ({e})");
            return;
        }
    };
    let added = netsh(&[
        "add",
        "rule",
        &format!("name={FIREWALL_RULE}"),
        "dir=in",
        "action=allow",
        &format!("program={exe}"),
        "protocol=TCP",
        &format!("localport={SYNC_PORT}"),
        "profile=any",
        "remoteip=localsubnet",
    ]);
    match added {
        Ok(o) if o.status.success() => println!("firewall rule \"{FIREWALL_RULE}\" added (local network only)"),
        Ok(o) => eprintln!(
            "warning: firewall rule not added: {}",
            String::from_utf8_lossy(&o.stdout).trim()
        ),
        Err(e) => eprintln!("warning: firewall rule not added ({e})"),
    }
}

fn remove_firewall_rule() {
    let _ = netsh(&["delete", "rule", &format!("name={FIREWALL_RULE}")]);
}

pub fn install() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem: needed to read the MFT and USN journal
        account_password: None,
    };
    // Installing over an existing registration (upgrade, repair) updates it instead of failing.
    let access = ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::CHANGE_CONFIG;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(existing) => {
            stop_and_wait(&existing)?;
            existing.change_config(&info)?;
            existing
        }
        Err(_) => manager.create_service(&info, access)?,
    };
    service.set_description("Keeps Seek's file index up to date and answers searches.")?;
    add_firewall_rule();
    service.start::<&str>(&[])?;
    println!("{SERVICE_NAME} installed and started");
    Ok(())
}

fn stop_and_wait(service: &windows_service::service::Service) -> anyhow::Result<()> {
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..50 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    stop_and_wait(&service)?;
    service.delete()?;
    remove_firewall_rule();
    println!("{SERVICE_NAME} removed");
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

/// Hands control to the service control manager; returns when the service stops.
pub fn run_as_service() -> anyhow::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_body() {
        tracing::error!("service failed: {e:#}");
    }
}

fn service_body() -> anyhow::Result<()> {
    let data_dir = default_data_dir();
    crate::init_file_logging(&data_dir);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(true);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let set = |state, accept, exit| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(exit),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
    )?;

    let cfg = Config {
        data_dir,
        pipe_name: seek_ipc::PIPE_NAME.into(),
        sync: Some(default_sync_config()),
    };
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(seek_service::run(cfg, Arc::new(RealVolumes), rx));
    if let Err(e) = &result {
        tracing::error!("service stopped with error: {e:#}");
    }
    set(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        if result.is_ok() { 0 } else { 1 },
    )?;
    Ok(())
}
