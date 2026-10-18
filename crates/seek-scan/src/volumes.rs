//! Drive-letter enumeration and the NTFS-with-fallback scan entry point.

use std::path::Path;

use seek_core::{DeviceIdentity, DeviceKind};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};

use crate::win::to_wide;
use crate::{ScanError, ScanResult};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct VolumeInfo {
    /// e.g. `C:\`
    pub mount_point: String,
    pub identity: DeviceIdentity,
    pub label: String,
    pub filesystem: String,
    pub kind: DeviceKind,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Fixed and removable drive letters that currently have a readable volume.
pub fn list_volumes() -> Vec<VolumeInfo> {
    // SAFETY: plain Win32 queries with valid, correctly sized buffers.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let mount_point = format!("{}:\\", (b'A' + i as u8) as char);
        let root = to_wide(&mount_point);
        let kind = match unsafe { GetDriveTypeW(root.as_ptr()) } {
            DRIVE_FIXED => DeviceKind::Fixed,
            DRIVE_REMOVABLE => DeviceKind::Removable,
            _ => continue,
        };
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        let mut serial = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue; // empty card reader, locked volume, ...
        }
        let (mut total, mut free) = (0u64, 0u64);
        unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
        let from_wide = |b: &[u16]| {
            String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())])
        };
        let filesystem = from_wide(&fs);
        out.push(VolumeInfo {
            mount_point,
            identity: DeviceIdentity::Volume {
                serial: serial as u64,
                filesystem: filesystem.clone(),
            },
            label: from_wide(&label),
            filesystem,
            kind,
            total_bytes: total,
            free_bytes: free,
        });
    }
    out
}

/// NTFS volumes use the raw MFT scanner; on any error (typically access denied) or for other
/// filesystems the generic walker is used.
pub fn scan_volume(
    v: &VolumeInfo,
    progress: &(dyn Fn(u64) + Sync),
) -> Result<ScanResult, ScanError> {
    if v.filesystem.eq_ignore_ascii_case("NTFS") {
        match crate::ntfs::scan_ntfs(&v.mount_point, progress) {
            Ok(r) => return Ok(r),
            Err(e) => tracing::info!(
                "NTFS scan of {} failed ({e}); falling back to walk",
                v.mount_point
            ),
        }
    }
    crate::walk::scan_walk(Path::new(&v.mount_point), progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_c_drive() {
        let vols = list_volumes();
        let c = vols
            .iter()
            .find(|v| v.mount_point == "C:\\")
            .expect("C: present");
        assert_eq!(c.filesystem, "NTFS");
        assert_eq!(c.kind, DeviceKind::Fixed);
        assert!(c.total_bytes > 0);
        assert!(matches!(c.identity, DeviceIdentity::Volume { .. }));
    }
}
