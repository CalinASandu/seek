// Prevents an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // First act, before anything else starts: the admin step of Cleanup has no window.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(code) = seek_app_lib::clean_elevated_dispatch(&args) {
        std::process::exit(code);
    }
    seek_app_lib::run()
}
