//! Who is asking: the user and elevation of a pipe client, and the folders they may not see.
//!
//! Rule: an elevated client ("Run as administrator") sees every file. Anyone else sees everything except
//! other users' profile folders (`C:\Users\<someone else>`), which Windows would not let them open anyway.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

use tokio::net::windows::named_pipe::NamedPipeServer;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// Running elevated ("Run as administrator", or the service itself).
    pub elevated: bool,
    /// The user's SID (`S-1-5-21-…`); empty when unknown.
    pub sid: String,
}

impl Caller {
    /// Used when the client cannot be identified: sees the least.
    pub fn unknown() -> Self {
        Self {
            elevated: false,
            sid: String::new(),
        }
    }
}

/// Profile folders of every user except `caller`; empty for an elevated caller.
pub fn hidden_folders(caller: &Caller, profiles: Vec<(String, String)>) -> Vec<String> {
    if caller.elevated {
        return Vec::new();
    }
    profiles
        .into_iter()
        .filter(|(sid, path)| !path.is_empty() && !sid.eq_ignore_ascii_case(&caller.sid))
        .map(|(_, path)| path)
        .collect()
}

/// Identifies the process at the other end of `pipe` from its access token.
pub fn identify(pipe: &NamedPipeServer) -> Caller {
    // SAFETY: the handle is a live pipe owned by `pipe` for the duration of the call.
    unsafe { identify_handle(pipe.as_raw_handle() as HANDLE) }.unwrap_or_else(Caller::unknown)
}

unsafe fn identify_handle(pipe: HANDLE) -> Option<Caller> {
    let mut pid = 0u32;
    if GetNamedPipeClientProcessId(pipe, &mut pid) == 0 {
        return None;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if process.is_null() {
        return None;
    }
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
    CloseHandle(process);
    if opened == 0 {
        return None;
    }
    let caller = token_caller(token);
    CloseHandle(token);
    caller
}

unsafe fn token_caller(token: HANDLE) -> Option<Caller> {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let ok = GetTokenInformation(
        token,
        TokenElevation,
        &mut elevation as *mut _ as *mut c_void,
        std::mem::size_of::<TOKEN_ELEVATION>() as u32,
        &mut len,
    );
    if ok == 0 {
        return None;
    }
    // TOKEN_USER is followed by the SID it points to; u64s keep the buffer aligned.
    let mut buf = vec![0u64; 64];
    if GetTokenInformation(
        token,
        TokenUser,
        buf.as_mut_ptr() as *mut c_void,
        (buf.len() * 8) as u32,
        &mut len,
    ) == 0
    {
        buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 8) as u32,
            &mut len,
        ) == 0
        {
            return None;
        }
    }
    let user = &*(buf.as_ptr() as *const TOKEN_USER);
    let mut s: *mut u16 = std::ptr::null_mut();
    if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
        return None;
    }
    let sid = wide_to_string(s);
    LocalFree(s as _);
    Some(Caller {
        elevated: elevation.TokenIsElevated != 0,
        sid,
    })
}

unsafe fn wide_to_string(p: *const u16) -> String {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `(SID, profile folder)` of every account with a profile on this PC (from the registry's ProfileList).
pub fn profiles() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let path = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList");
    let value = wide("ProfileImagePath");
    // SAFETY: every buffer passed below is sized as declared; the key is closed before returning.
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS
        {
            return out;
        }
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let r = RegEnumKeyExW(
                key,
                i,
                name.as_mut_ptr(),
                &mut name_len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if r != ERROR_SUCCESS {
                break;
            }
            let mut data = [0u16; 1024];
            let mut cb = (data.len() * 2) as u32;
            // REG_EXPAND_SZ values (`%SystemDrive%\Users\x`) come back expanded.
            let r = RegGetValueW(
                key,
                name.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr() as *mut c_void,
                &mut cb,
            );
            if r == ERROR_SUCCESS {
                let sid = String::from_utf16_lossy(&name[..name_len as usize]);
                out.push((sid, wide_to_string(data.as_ptr())));
            }
        }
        RegCloseKey(key);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_pcs_profiles() {
        let p = profiles();
        assert!(
            p.iter().any(|(sid, _)| sid == "S-1-5-18"),
            "LocalSystem is always listed: {p:?}"
        );
        assert!(
            p.iter()
                .all(|(sid, path)| sid.starts_with("S-1-") && path.contains('\\')),
            "{p:?}"
        );
    }

    #[test]
    fn only_other_users_folders_are_hidden_and_only_from_non_elevated_callers() {
        let profiles = vec![
            ("S-1-5-21-me".to_string(), r"C:\Users\me".to_string()),
            ("S-1-5-21-bob".to_string(), r"C:\Users\bob".to_string()),
            (
                "S-1-5-18".to_string(),
                r"C:\Windows\system32\config\systemprofile".to_string(),
            ),
        ];
        let me = Caller {
            elevated: false,
            sid: "S-1-5-21-me".into(),
        };
        assert_eq!(
            hidden_folders(&me, profiles.clone()),
            vec![
                r"C:\Users\bob".to_string(),
                r"C:\Windows\system32\config\systemprofile".to_string()
            ]
        );
        assert!(hidden_folders(
            &Caller {
                elevated: true,
                ..me
            },
            profiles.clone()
        )
        .is_empty());
        assert_eq!(hidden_folders(&Caller::unknown(), profiles).len(), 3);
    }
}
