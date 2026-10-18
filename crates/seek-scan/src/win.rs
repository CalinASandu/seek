//! Small Win32 helpers shared by the NTFS scanner and the USN follower.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows_sys::Win32::System::IO::DeviceIoControl;

pub(crate) const ERROR_FILE_NOT_FOUND: i32 = 2;
pub(crate) const ERROR_PATH_NOT_FOUND: i32 = 3;
pub(crate) const ERROR_ACCESS_DENIED: i32 = 5;
pub(crate) const ERROR_NOT_READY: i32 = 21;

pub(crate) const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Opens `\.\X:` for raw reading. `mount_point` is e.g. `C:\` or `C:`.
pub(crate) fn open_volume(mount_point: &str) -> io::Result<File> {
    let letter = mount_point
        .chars()
        .next()
        .filter(char::is_ascii_alphabetic)
        .ok_or_else(|| io::Error::from_raw_os_error(ERROR_PATH_NOT_FOUND))?;
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(format!(r"\\.\{}:", letter.to_ascii_uppercase()))
}

/// # Safety
/// `inp`/`out` must be valid for `in_len`/`out_len` bytes (or null with length 0).
pub(crate) unsafe fn ioctl(
    file: &File,
    code: u32,
    inp: *const core::ffi::c_void,
    in_len: u32,
    out: *mut core::ffi::c_void,
    out_len: u32,
) -> io::Result<u32> {
    let mut returned = 0u32;
    let ok = DeviceIoControl(
        file.as_raw_handle() as HANDLE,
        code,
        inp,
        in_len,
        out,
        out_len,
        &mut returned,
        core::ptr::null_mut(),
    );
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(returned)
    }
}

pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
