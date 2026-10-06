//! Seek desktop app: Tauri shell around the React UI, talking to seek-service over a named pipe.

mod commands;
mod pipe;
mod service_ctl;
mod settings;
mod shares;
mod shell;

use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

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
            "quit" => app.exit(0),
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let link = Arc::new(ServiceLink::default());
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .manage(link.clone())
        .setup(move |app| {
            let handle = app.handle().clone();
            let events_link = link.clone();
            tauri::async_runtime::spawn(async move { events_link.run_events(handle).await });
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running Seek");
}
