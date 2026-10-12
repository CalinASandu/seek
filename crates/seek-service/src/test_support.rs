//! Fakes shared by the service's unit tests.

use std::path::PathBuf;

use seek_core::{Change, Checkpoint, DeviceIdentity, DeviceIndex, DeviceKind};
use seek_scan::{Changed, DirWatcher, FollowError, ScanError, ScanResult, UsnFollower, VolumeInfo};

use crate::monitor::VolumeSource;

/// A PC without volumes (devices are added by hand in tests).
pub struct NoVolumes;

impl VolumeSource for NoVolumes {
    fn list(&self) -> Vec<VolumeInfo> {
        Vec::new()
    }
    fn scan(&self, _v: &VolumeInfo, _p: &(dyn Fn(u64) + Sync)) -> Result<ScanResult, ScanError> {
        unimplemented!("tests add indexes directly")
    }
    fn follow(&self, _v: &VolumeInfo, _cp: Checkpoint) -> Result<UsnFollower, FollowError> {
        Err(FollowError::AccessDenied)
    }
    fn watch(&self, _v: &VolumeInfo, _f: Box<dyn Fn(Changed) + Send>) -> Option<DirWatcher> {
        None
    }
    fn rescan_dirs(
        &self,
        _v: &VolumeInfo,
        _ix: &DeviceIndex,
        _d: &[PathBuf],
    ) -> Result<Vec<Change>, ScanError> {
        Ok(Vec::new())
    }
    fn profiles(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

pub fn volume() -> VolumeInfo {
    VolumeInfo {
        mount_point: "T:\\".into(),
        identity: DeviceIdentity::Volume {
            serial: 1,
            filesystem: "NTFS".into(),
        },
        label: String::new(),
        filesystem: "NTFS".into(),
        kind: DeviceKind::Fixed,
        total_bytes: 0,
        free_bytes: 0,
    }
}
