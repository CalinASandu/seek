//! Core data model shared by every Seek crate.

use serde::{Deserialize, Serialize};

/// Seek-generated, stable-forever identifier of a device.
pub type DeviceId = uuid::Uuid;
/// Persistent identifier of the PC that produced an index (for future sync).
pub type HostId = uuid::Uuid;

pub const ATTR_READONLY: u32 = 0x1;
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_DIRECTORY: u32 = 0x10;

/// How a physical volume or share is recognised when it shows up again.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DeviceIdentity {
    /// Local volume. `serial` is the volume serial number (changes on reformat).
    Volume { serial: u64, filesystem: String },
    /// Network share, normalised with [`normalize_unc`].
    Share { unc: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum DeviceKind {
    Fixed,
    Removable,
    Network,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum DeviceState {
    Online,
    Offline,
    Scanning,
    Error,
}

/// One indexed root: a local volume or a network share.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: DeviceId,
    pub identity: DeviceIdentity,
    /// User-editable name; defaults to the volume label or share name.
    pub display_name: String,
    pub label: String,
    pub filesystem: String,
    pub kind: DeviceKind,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// e.g. `C:\` or `\\server\share\` — used to build paths, also while offline.
    pub last_mount_point: String,
    pub indexed_by_host: HostId,
    /// Unix epoch milliseconds.
    pub first_seen: i64,
    pub last_seen: i64,
    pub index_updated_at: i64,
    pub state: DeviceState,
    pub entry_count: u64,
    pub error: Option<String>,
}

/// One file or folder as produced by a scanner, before it is packed into a `DeviceIndex`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEntry {
    /// NTFS file reference number (low 48 bits) or a synthetic id from the walker.
    pub id: u64,
    pub parent_id: u64,
    pub name: String,
    pub size: u64,
    /// Unix epoch milliseconds.
    pub modified: i64,
    pub created: i64,
    /// Windows file attributes (`ATTR_*`).
    pub attrs: u32,
}

/// Position in an NTFS USN change journal.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// Incremental change applied to a `DeviceIndex`, keyed by entry id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Upsert(RawEntry),
    Delete { id: u64 },
}

/// Normalise a UNC share path: backslashes, lowercase, no trailing separator.
/// `"//NAS/Media/"` → `"\\\\nas\\media"`.
pub fn normalize_unc(s: &str) -> String {
    let mut out = s.trim().replace('/', "\\").to_lowercase();
    while out.ends_with('\\') && out.len() > 2 {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_normalisation() {
        assert_eq!(normalize_unc("\\\\NAS\\Media\\"), "\\\\nas\\media");
        assert_eq!(normalize_unc("//nas/media"), "\\\\nas\\media");
    }

    #[test]
    fn device_serde_roundtrip_camel_case() {
        let d = Device {
            id: uuid::Uuid::nil(),
            identity: DeviceIdentity::Volume {
                serial: 42,
                filesystem: "NTFS".into(),
            },
            display_name: "System".into(),
            label: "".into(),
            filesystem: "NTFS".into(),
            kind: DeviceKind::Fixed,
            total_bytes: 1,
            free_bytes: 0,
            last_mount_point: "C:\\".into(),
            indexed_by_host: uuid::Uuid::nil(),
            first_seen: 1,
            last_seen: 2,
            index_updated_at: 3,
            state: DeviceState::Online,
            entry_count: 0,
            error: None,
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"displayName\":\"System\""));
        assert!(json.contains("\"identity\":{\"type\":\"volume\",\"serial\":42"));
        assert!(json.contains("\"state\":\"online\""));
        assert_eq!(serde_json::from_str::<Device>(&json).unwrap(), d);
    }
}
