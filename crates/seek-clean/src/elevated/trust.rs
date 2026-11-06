//! System folders the elevated process can believe. The process environment is not one of them:
//! the elevated copy starts with the user's environment, which the user (or anything running as
//! them) can change, and the known-folder calls follow `%SystemDrive%` and `%ProgramData%` from
//! it. So these come from `GetWindowsDirectoryW` and from HKLM (which a standard user cannot
//! write), are compared with the environment, and if the two disagree nothing is removed. The
//! environment is then overwritten with them, so nothing later can read a poisoned value.

use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;

use crate::paths::{canon, is_inside};
use crate::protect::Places;

pub const NOT_TRUSTED: &str = "Seek's admin step found system folder settings it doesn't trust, so it didn't remove anything.";

const PROFILE_LIST: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";
const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion";

/// The system folders, as Windows and HKLM say they are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedFolders {
    pub windows_dir: String,
    /// `C:`
    pub system_drive: String,
    pub program_data: String,
    /// The folder the profiles are in (`C:\Users`).
    pub profiles_dir: String,
    pub public: Option<String>,
    pub program_files: String,
    pub program_files_x86: Option<String>,
    pub program_w6432: String,
    pub common_files: Option<String>,
    pub common_files_x86: Option<String>,
    pub common_w6432: Option<String>,
}

impl TrustedFolders {
    /// The environment variables these stand for, with their trusted values.
    pub fn variables(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![
            ("SystemDrive", self.system_drive.clone()),
            ("SystemRoot", self.windows_dir.clone()),
            ("windir", self.windows_dir.clone()),
            ("ProgramData", self.program_data.clone()),
            ("ALLUSERSPROFILE", self.program_data.clone()),
            ("ProgramFiles", self.program_files.clone()),
            ("ProgramW6432", self.program_w6432.clone()),
        ];
        let optional = [
            ("PUBLIC", &self.public),
            ("ProgramFiles(x86)", &self.program_files_x86),
            ("CommonProgramFiles", &self.common_files),
            ("CommonProgramFiles(x86)", &self.common_files_x86),
            ("CommonProgramW6432", &self.common_w6432),
        ];
        for (name, value) in optional {
            if let Some(value) = value {
                v.push((name, value.clone()));
            }
        }
        v
    }

    /// The trusted value of a variable (case-insensitive), if it is one of these.
    pub fn get(&self, name: &str) -> Option<String> {
        self.variables().into_iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }
}

/// Replaces `%SystemDrive%` (any case) with `drive` and nothing else. Any other `%` is an error:
/// a value that needs the environment is not one to trust.
pub fn expand_system_drive(raw: &str, drive: &str) -> Result<String, String> {
    let lower = raw.to_ascii_lowercase();
    let mut out = String::new();
    let mut at = 0;
    while let Some(i) = lower[at..].find("%systemdrive%") {
        out.push_str(&raw[at..at + i]);
        out.push_str(drive);
        at += i + "%systemdrive%".len();
    }
    out.push_str(&raw[at..]);
    if out.contains('%') {
        return Err(format!("{raw} needs the environment to be read"));
    }
    Ok(out)
}

/// Works out the trusted folders from what Windows and HKLM gave. Pure: the readers are passed in.
/// `profile_list` and `current_version` read a named value of those two registry keys, unexpanded.
pub fn derive(
    windows_dir: &str,
    profile_list: &dyn Fn(&str) -> Option<String>,
    current_version: &dyn Fn(&str) -> Option<String>,
) -> Result<TrustedFolders, String> {
    let b = windows_dir.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return Err("the Windows folder is not on a drive letter".into());
    }
    let system_drive = windows_dir[..2].to_string();
    let profile = |name: &str| profile_list(name).map(|v| expand_system_drive(&v, &system_drive)).transpose();
    let version = |name: &str| current_version(name).filter(|v| !v.is_empty());
    let need = |what: &str, v: Option<String>| v.filter(|v| !v.is_empty()).ok_or_else(|| format!("{what} could not be read"));
    let program_files = need("Program Files", version("ProgramFilesDir"))?;
    Ok(TrustedFolders {
        windows_dir: windows_dir.to_string(),
        program_data: need("ProgramData", profile("ProgramData")?)?,
        profiles_dir: need("the profiles folder", profile("ProfilesDirectory")?)?,
        public: profile("Public")?,
        program_w6432: version("ProgramW6432Dir").unwrap_or_else(|| program_files.clone()),
        program_files,
        program_files_x86: version("ProgramFilesDir (x86)"),
        common_files: version("CommonFilesDir"),
        common_files_x86: version("CommonFilesDir (x86)"),
        common_w6432: version("CommonW6432Dir"),
        system_drive,
    })
}

fn same(a: &str, b: &str) -> bool {
    let n = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    n(a) == n(b)
}

/// The variables the environment gives a different value for (case-insensitive, trailing slash
/// ignored). A variable it does not set is not a disagreement.
pub fn mismatches(t: &TrustedFolders, env: &dyn Fn(&str) -> Option<String>) -> Vec<&'static str> {
    t.variables().into_iter().filter(|(name, value)| env(name).is_some_and(|e| !same(&e, value))).map(|(name, _)| name).collect()
}

/// Do the places Windows named agree with the trusted folders? The users folder is the profiles
/// folder, ProgramData and the Windows folder are the trusted ones, Program Files is among the
/// trusted Program Files folders, and the all-users Start Menu is inside ProgramData.
pub fn check_places(places: &Places, t: &TrustedFolders) -> Result<(), String> {
    let c = |s: &str| canon(std::path::Path::new(s));
    let pf: Vec<String> = [Some(&t.program_files), t.program_files_x86.as_ref()].into_iter().flatten().map(|s| c(s)).collect();
    let ok = places.windows_canon() == c(&t.windows_dir)
        && places.program_data_canon() == c(&t.program_data)
        && places.users_root_canon() == c(&t.profiles_dir)
        && !places.program_files_canon().is_empty()
        && places.program_files_canon().iter().all(|p| pf.contains(p))
        && places.program_files_canon().contains(&c(&t.program_files))
        && places.common_programs_canon().is_none_or(|m| is_inside(&m, &c(&t.program_data)));
    if ok {
        Ok(())
    } else {
        Err(NOT_TRUSTED.into())
    }
}

// ---- the machine ---------------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// One string value of an HKLM key, as stored (not expanded), in the 64-bit view.
fn hklm_string(key: &str, name: &str) -> Option<String> {
    let (k, n) = (wide(key), wide(name));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND | RRF_SUBKEY_WOW6464KEY;
    let mut size = 0u32;
    // SAFETY: NUL-terminated names; the first call only asks for the size.
    let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(k.as_ptr()), PCWSTR(n.as_ptr()), flags, None, None, Some(&mut size)) };
    if rc != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf` holds `size` bytes.
    let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(k.as_ptr()), PCWSTR(n.as_ptr()), flags, None, Some(buf.as_mut_ptr().cast()), Some(&mut size)) };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn windows_directory() -> Option<String> {
    let mut buf = vec![0u16; 512];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
}

/// Reads the trusted folders from this machine, without the environment.
pub fn read_trusted() -> Result<TrustedFolders, String> {
    let dir = windows_directory().ok_or("Windows didn't say where it is")?;
    derive(&dir, &|n| hklm_string(PROFILE_LIST, n), &|n| hklm_string(CURRENT_VERSION, n))
}

static TRUSTED: OnceLock<TrustedFolders> = OnceLock::new();

/// The folders `establish` settled on, once it has run in this process.
pub fn global() -> Option<&'static TrustedFolders> {
    TRUSTED.get()
}

/// The first thing the elevated process does, before it reads any place, claim or setting:
/// reads the trusted folders, notes whether the environment disagrees, then overwrites the
/// environment with them so nothing later reads a poisoned value. An error (unreadable, or the
/// environment disagreed) means nothing may be removed.
pub fn establish() -> Result<TrustedFolders, String> {
    let t = read_trusted().map_err(|e| format!("Seek's admin step couldn't read the system folders ({e}), so it didn't remove anything."))?;
    let disagree = mismatches(&t, &|n| std::env::var(n).ok());
    for (name, value) in t.variables() {
        std::env::set_var(name, value);
    }
    let _ = TRUSTED.set(t.clone());
    if disagree.is_empty() {
        Ok(t)
    } else {
        Err(NOT_TRUSTED.into())
    }
}
