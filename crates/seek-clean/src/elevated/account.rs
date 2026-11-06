//! Which Windows account a process runs as, and the check that the elevated copy of Seek is the
//! same account as the window that asked for it. Answering the UAC prompt with another account
//! would put the files in that account's Recycle Bin, where the user can't put them back.

use std::path::PathBuf;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::paths::canon_lexical;

const MISMATCH: &str = "This admin step is not running as the same Windows account as the Seek window that asked for it, so nothing was removed. Answer the prompt with your own account.";

/// Closes the handle once, whatever way out.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess or OpenProcessToken and is closed only here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// What the elevated process learned about itself and about the window that started it.
#[derive(Clone, Debug)]
pub struct AccountFacts {
    pub own_sid: Result<String, String>,
    pub own_image: Result<PathBuf, String>,
    pub window_sid: Result<String, String>,
    pub window_image: Result<PathBuf, String>,
}

/// Everything must line up, or the answer is no: the job's SID, this process's token user, the
/// window's token user, and the window's program file being this very program file.
pub fn check_account(job_sid: &str, f: &AccountFacts) -> Result<(), String> {
    let own = f.own_sid.as_ref().map_err(|_| MISMATCH.to_string())?;
    if job_sid.is_empty() || !job_sid.eq_ignore_ascii_case(own) {
        return Err(MISMATCH.into());
    }
    match &f.window_sid {
        Ok(w) if w.eq_ignore_ascii_case(own) => {}
        _ => return Err(MISMATCH.into()),
    }
    match (&f.own_image, &f.window_image) {
        (Ok(a), Ok(b)) if canon_lexical(a) == canon_lexical(b) => Ok(()),
        _ => Err(MISMATCH.into()),
    }
}

/// Reads the facts for real. `window_pid` is the id from the job.
pub fn read_facts(window_pid: u32) -> AccountFacts {
    let own = open_own();
    let window = open_window(window_pid);
    AccountFacts {
        own_sid: own,
        own_image: std::env::current_exe().map_err(|e| e.to_string()),
        window_sid: window.as_ref().map_err(Clone::clone).and_then(|h| token_user_of_process(h.0)),
        window_image: window.and_then(|h| image_path(h.0)),
    }
}

/// The user SID of this process, as the usual `S-1-5-21-...` text. The real elevator puts it in
/// the job.
pub fn current_user_sid() -> Result<String, String> {
    open_own()
}

fn open_own() -> Result<String, String> {
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    token_user_of_process(unsafe { GetCurrentProcess() })
}

fn open_window(pid: u32) -> Result<Handle, String> {
    if pid == 0 {
        return Err("the job names no window process".into());
    }
    // SAFETY: plain call; the handle is owned by the guard.
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map(Handle)
        .map_err(|e| format!("the window process could not be opened ({e})"))
}

fn token_user_of_process(process: HANDLE) -> Result<String, String> {
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer; the handle is closed by the guard.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(|e| e.to_string())?;
    let token = Handle(token);
    let mut needed = 0u32;
    // SAFETY: the first call only asks for the size.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err("the account of a process could not be read".into());
    }
    // u64 elements keep the buffer aligned for TOKEN_USER.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes.
    unsafe { GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr().cast()), needed, &mut needed) }
        .map_err(|e| e.to_string())?;
    // SAFETY: the call filled the buffer with a TOKEN_USER whose SID points into the same buffer.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    sid_text(sid)
}

/// A SID as the usual `S-1-5-21-...` text. `sid` must be valid for the call.
pub fn sid_text(sid: PSID) -> Result<String, String> {
    let mut text = PWSTR::null();
    // SAFETY: the caller's `sid` is valid; the string is freed with LocalFree below.
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(|e| e.to_string())?;
    // SAFETY: `text` is a valid NUL-terminated string from the call above, freed once here.
    unsafe {
        let s = text.to_string().map_err(|e| e.to_string());
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        s
    }
}

fn image_path(process: HANDLE) -> Result<PathBuf, String> {
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` has `len` elements; the call sets `len` to the number written.
    unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }
        .map_err(|e| e.to_string())?;
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}
