//! Seek desktop app: Tauri shell around the React UI, talking to seek-service over a named pipe.

mod cleanup;
mod commands;
mod pipe;
mod service_ctl;
mod settings;
mod shares;
mod shell;

use std::sync::Arc;
use std::time::{Duration, Instant};

use seek_ipc::Request;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

use cleanup::CleanState;
use pipe::ServiceLink;

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_main(app);
        }
    }
}

/// Tells the service while the window is showing, so its indexes are in memory before the first search.
/// Once the window is hidden the service unloads them.
async fn keep_service_awake(app: AppHandle, link: Arc<ServiceLink>, clean: Arc<CleanState>) {
    const LOOK_EVERY: Duration = Duration::from_millis(500);
    const REPEAT_EVERY: Duration = Duration::from_secs(20);
    let mut told: Option<Instant> = None;
    let mut hidden_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        let showing = app
            .get_webview_window("main")
            .is_some_and(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false));
        if !showing {
            told = None;
            hidden_since.get_or_insert_with(Instant::now);
        } else {
            hidden_since = None;
            if told.is_none_or(|t| t.elapsed() >= REPEAT_EVERY) {
                told = Some(Instant::now());
                let _ = link.request(Request::Wake).await;
            }
        }
        // Finished duplicate results do not outlast a long absence (see `cleanup`).
        clean.window_hidden_for(hidden_since.map(|t| t.elapsed()));
    }
}

/// Quit: the window goes away at once, then a removal in flight (and its history write) gets up to two
/// minutes to finish before the process exits. On a thread of its own, so the tray menu stays alive.
fn quit_when_idle(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let clean: Arc<CleanState> = (*app.state::<Arc<CleanState>>()).clone();
        clean.finish_in_flight(Duration::from_secs(120));
        app.exit(0);
    });
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Seek", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Seek", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut tray = TrayIconBuilder::with_id("seek")
        .tooltip("Seek")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| match e.id().as_ref() {
            "open" => show_main(app),
            "quit" => quit_when_idle(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                toggle_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// `SetDefaultDllDirectories(System32 | application directory)` for this process. True if it took.
fn restrict_dll_search() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{
        SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: a plain call with two documented flags.
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR) != 0 }
}

/// The first thing `main` does: `--clean-elevated <pipe name>` is the admin step of Cleanup, started
/// behind one UAC prompt. It needs no Tauri, no window and no single-instance plugin, so it is
/// handled here and the process exits with the returned code. Anything else after the flag, or
/// less, is refused (non-zero). `None` means this is an ordinary start.
pub fn clean_elevated_dispatch(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.first()? != "--clean-elevated" {
        return None;
    }
    // Before anything else is loaded or read: from now on a DLL is found only in System32 or next to
    // this exe, never in the current directory or on PATH, which a user (or anything running as them)
    // can plant in. If that can't be set, nothing is done.
    if !restrict_dll_search() {
        return Some(2);
    }
    // Only the right shape of arguments gets any further (a refusal reads nothing and changes nothing).
    let [_, pipe_name] = args else { return Some(2) };
    let Some(name) = pipe_name.to_str().filter(|n| seek_clean::elevated::pipe::name_ok(n)) else {
        return Some(2);
    };
    // First, before any place, claim or setting is read: the system folders from Windows and HKLM,
    // checked against the environment, which is then overwritten with them.
    let trusted = seek_clean::elevated::trust::establish();
    let protected = match &trusted {
        Ok(_) => settings::protected_paths_strict(),
        Err(e) => Err(e.clone()),
    };
    Some(seek_clean::elevated::run_main(name, protected, trusted))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let link = Arc::new(ServiceLink::default());
    let clean = Arc::new(CleanState::default());
    tauri::Builder::default()
        // First, as the plugin requires. A second process would have its own settings cache and engine
        // (and could drop a protected path), so a second launch only shows this window and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .manage(link.clone())
        .manage(clean.clone())
        .setup(move |app| {
            clean.set_app(app.handle().clone());
            // The admin prompt opens in front of this window.
            if let Some(Ok(h)) = app.get_webview_window("main").map(|w| w.hwnd()) {
                cleanup::set_main_hwnd(h.0 as isize);
            }
            let handle = app.handle().clone();
            let events_link = link.clone();
            tauri::async_runtime::spawn(async move { events_link.run_events(handle).await });
            let (handle, wake_link) = (app.handle().clone(), link.clone());
            tauri::async_runtime::spawn(keep_service_awake(handle, wake_link, clean.clone()));
            // Shares get their own connection: a multi-megabyte index upload must not sit in front of
            // the window's searches on the shared one.
            app.manage(shares::start(Arc::new(ServiceLink::default())));
            build_tray(app.handle())?;
            if std::env::args().any(|a| a == "--minimized") {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps Seek in the tray; Quit lives in the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::fetch_rows,
            commands::list_devices,
            commands::rename_device,
            commands::forget_device,
            commands::rescan_device,
            commands::add_network_share,
            commands::status,
            commands::open_path,
            commands::reveal_path,
            commands::service_state,
            commands::install_service,
            commands::get_settings,
            commands::set_settings,
            commands::settings_problem,
            commands::start_pairing,
            commands::discovered_pcs,
            commands::join_pairing,
            commands::link_hub,
            commands::unlink_hub,
            commands::hub_status,
            commands::list_peers,
            commands::unlink,
            commands::rename_peer,
            commands::sync_now,
            commands::set_sharing,
            commands::set_device_shared,
            commands::space_tree,
            commands::space_largest,
            commands::space_types,
            cleanup::clean_scan_start,
            cleanup::clean_scan_cancel,
            cleanup::clean_scan_faster,
            cleanup::clean_remembered,
            cleanup::clean_forget,
            cleanup::clean_scan_state,
            cleanup::clean_leftovers,
            cleanup::clean_apps,
            cleanup::clean_prepare,
            cleanup::clean_commit,
            cleanup::clean_history,
            cleanup::clean_put_back,
            cleanup::open_recycle_bin,
            cleanup::uninstall_start,
            cleanup::uninstall_state,
            cleanup::uninstall_leftovers,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Seek");
}

#[cfg(test)]
mod dispatch_tests {
    use super::clean_elevated_dispatch;
    use std::ffi::OsString;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn ordinary_starts_are_not_the_admin_step() {
        assert_eq!(clean_elevated_dispatch(&args(&[])), None);
        assert_eq!(clean_elevated_dispatch(&args(&["--minimized"])), None);
        assert_eq!(clean_elevated_dispatch(&args(&["x", "--clean-elevated"])), None);
    }

    #[test]
    fn the_flag_with_anything_but_one_job_file_refuses_and_exits_non_zero() {
        assert_eq!(clean_elevated_dispatch(&args(&["--clean-elevated"])), Some(2));
        assert_eq!(clean_elevated_dispatch(&args(&["--clean-elevated", "a", "b"])), Some(2));
        assert_eq!(clean_elevated_dispatch(&args(&["--clean-elevated", "a", "--minimized"])), Some(2));
    }

    #[test]
    fn a_name_that_is_not_the_windows_pipe_shape_is_refused_before_anything_is_opened() {
        for name in ["job.json", r"C:\Temp\job.json", r"\\.\pipe\other", r"\\.\pipe\seek-clean-ABCDEFABCDEFABCDEFABCDEFABCDEFAB"] {
            assert_eq!(clean_elevated_dispatch(&args(&["--clean-elevated", name])), Some(2), "{name}");
        }
    }
}
