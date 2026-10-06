//! Opening files, revealing them in Explorer and elevated launches.

use std::os::windows::process::CommandExt;
use std::path::Path;

use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn shell_execute(verb: &str, file: &str, params: Option<&str>) -> Result<(), String> {
    let (verb, file) = (wide(verb), wide(file));
    let params = params.map(wide);
    // SAFETY: all strings are NUL-terminated UTF-16 buffers that outlive the call.
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    // ShellExecute returns a value > 32 on success.
    if code > 32 {
        Ok(())
    } else if code == 5 {
        Err("access denied".into())
    } else {
        Err(format!("could not open {} (error {code})", String::from_utf16_lossy(&file[..file.len() - 1])))
    }
}

pub fn open_path(path: &str) -> Result<(), String> {
    if !Path::new(path).exists() {
        return Err(format!("{path} is not available — the device may be offline"));
    }
    shell_execute("open", path, None)
}

pub fn reveal_path(path: &str) -> Result<(), String> {
    let target = Path::new(path);
    if target.exists() {
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{path}\""))
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    } else if let Some(parent) = target.parent().filter(|p| p.exists()) {
        shell_execute("open", &parent.to_string_lossy(), None)
    } else {
        Err(format!("{path} is not available — the device may be offline"))
    }
}

pub fn run_elevated(exe: &Path, args: &str) -> Result<(), String> {
    shell_execute("runas", &exe.to_string_lossy(), Some(args))
}
