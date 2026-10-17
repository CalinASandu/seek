//! Wire protocol between `seek-service` and the app.
//!
//! Every frame is a `u32` little-endian byte length followed by a UTF-8 JSON [`Envelope`].
//! Responses carry the `id` of the request they answer; pushed events use `id = 0`.

use serde::{Deserialize, Serialize};

pub use seek_core::{Device, DeviceId, ResultRow, SortKey, SpaceNode, TypeTotal};

pub const PIPE_NAME: &str = r"\\.\pipe\seek-v1";
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest accepted frame (share index uploads can be big).
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Envelope<T> {
    pub id: u64,
    pub body: T,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Request {
    Hello { protocol_version: u32 },
    Search { query: String, sort: SortKey, descending: bool },
    FetchRows { search_id: u64, offset: u32, limit: u32 },
    ListDevices,
    RenameDevice { id: DeviceId, name: String },
    ForgetDevice { id: DeviceId },
    RescanDevice { id: DeviceId },
    AddNetworkShare { unc: String },
    /// Base64 of an encoded `.seekidx` (see `seek_core::persist::encode_index`).
    PushShareIndex { device_id: DeviceId, data_b64: String },
    SetShareState { device_id: DeviceId, online: bool },
    /// A drive the app sees in the user's session (e.g. Google Drive's virtual `G:`, a mapped drive).
    /// The service answers `Ok` when it indexes that volume itself, or `Device` when it cannot see it:
    /// then the app scans it and feeds it with `PushShareIndex` / `SetShareState`, like a share.
    AddUserVolume { volume: UserVolume },
    Status,
    /// Switches the connection into an event stream of [`Response::Event`].
    Subscribe,
    /// Shows a 6-digit code another PC can type to link with this one (replaces any previous code).
    StartPairing,
    /// Seek PCs seen on the local network that are not linked yet.
    DiscoveredPcs,
    /// Links with the PC at `addr` (`ip:port`) that shows `code`.
    JoinPairing { addr: String, code: String },
    ListPeers,
    /// Removes the link with a PC on both sides, and everything copied from it.
    Unlink { host_id: uuid::Uuid },
    RenamePeer { host_id: uuid::Uuid, name: String },
    /// Pushes changes to every linked PC now and asks them to do the same.
    SyncNow,
    /// "Share this PC's files with linked PCs".
    SetSharing { enabled: bool },
    SetDeviceShared { device_id: DeviceId, shared: bool },
    /// What fills `path` (a folder's full path; the drive root is its mount point, e.g. `D:\`), `depth`
    /// levels deep, largest first.
    SpaceTree { device_id: DeviceId, path: String, depth: u8 },
    /// The `limit` largest files below `path`.
    SpaceLargest { device_id: DeviceId, path: String, limit: u32 },
    /// Bytes below `path` by kind of file.
    SpaceTypes { device_id: DeviceId, path: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Response {
    Hello { protocol_version: u32, service_version: String },
    SearchStarted { search_id: u64, total: u64, elapsed_ms: f64 },
    /// `position` is a char index into the query string.
    SearchError { message: String, position: usize },
    Rows { search_id: u64, offset: u32, rows: Vec<ResultRow> },
    Devices { devices: Vec<Device> },
    Device { device: Device },
    Ok,
    Error { message: String },
    Status(StatusInfo),
    Event { event: Event },
    PairingCode { code: String, expires_ms: i64 },
    Pcs { pcs: Vec<PcInfo> },
    Peers { this_pc: PcSummary, peers: Vec<PcSummary>, sharing: bool, unshared: Vec<DeviceId> },
    SpaceTree { node: SpaceNode },
    SpaceFiles { files: Vec<ResultRow> },
    SpaceTypes { types: Vec<TypeTotal> },
}

/// A Seek PC on the local network.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PcInfo {
    pub host_id: uuid::Uuid,
    pub name: String,
    pub addr: String,
}

/// This PC or a linked one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PcSummary {
    pub host_id: uuid::Uuid,
    pub name: String,
    pub online: bool,
    /// Unix ms of the last update exchanged with this PC; 0 = never.
    pub last_sync_ms: i64,
    /// The PC came back with a different identity (reinstalled) and must be linked again.
    pub needs_relink: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserVolume {
    /// e.g. `G:\`
    pub mount_point: String,
    pub identity: seek_core::DeviceIdentity,
    pub label: String,
    pub filesystem: String,
    pub kind: seek_core::DeviceKind,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatusInfo {
    pub service_version: String,
    pub total_entries: u64,
    pub devices_online: u32,
    pub devices_offline: u32,
    pub indexing: Vec<IndexProgress>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgress {
    pub device_id: DeviceId,
    pub scanned: u64,
    pub done: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Event {
    DevicesChanged,
    ResultsInvalidated,
    IndexProgress { device_id: DeviceId, scanned: u64, done: bool },
    /// Linked PCs changed (linked, unlinked, connected, synced).
    PeersChanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shapes() {
        let r = Request::Search { query: "a".into(), sort: SortKey::Size, descending: true };
        assert_eq!(
            serde_json::to_string(&Envelope { id: 7, body: r }).unwrap(),
            r#"{"id":7,"body":{"type":"search","query":"a","sort":"size","descending":true}}"#
        );
        let f = Request::FetchRows { search_id: 1, offset: 0, limit: 50 };
        assert_eq!(serde_json::to_string(&f).unwrap(), r#"{"type":"fetchRows","searchId":1,"offset":0,"limit":50}"#);
        let e = Response::Event { event: Event::ResultsInvalidated };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"type":"event","event":{"type":"resultsInvalidated"}}"#);
        let s = Response::SearchStarted { search_id: 3, total: 10, elapsed_ms: 1.5 };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"type":"searchStarted","searchId":3,"total":10,"elapsedMs":1.5}"#);
        let j = Request::JoinPairing { addr: "1.2.3.4:47321".into(), code: "123456".into() };
        assert_eq!(serde_json::to_string(&j).unwrap(), r#"{"type":"joinPairing","addr":"1.2.3.4:47321","code":"123456"}"#);
        let pc = PcSummary { host_id: uuid::Uuid::nil(), name: "A".into(), online: true, last_sync_ms: 5, needs_relink: false };
        let p = Response::Peers { this_pc: pc.clone(), peers: vec![pc], sharing: true, unshared: vec![] };
        let js = serde_json::to_string(&p).unwrap();
        assert!(js.starts_with(r#"{"type":"peers","thisPc":{"hostId":"#), "{js}");
        assert!(js.contains(r#""lastSyncMs":5,"needsRelink":false"#), "{js}");
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), p);
        let e = Response::Event { event: Event::PeersChanged };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"type":"event","event":{"type":"peersChanged"}}"#);
    }

    #[test]
    fn space_json_shapes() {
        let r = Request::SpaceTree { device_id: uuid::Uuid::nil(), path: r"D:\Games".into(), depth: 3 };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"type":"spaceTree","deviceId":"00000000-0000-0000-0000-000000000000","path":"D:\\Games","depth":3}"#
        );
        let leaf = |name: &str, kind| seek_core::SpaceNode {
            name: name.into(),
            kind,
            bytes: 2,
            files: 1,
            modified: 0,
            cloud: false,
            children: vec![],
        };
        let mut node = leaf("Games", seek_core::SpaceKind::Folder);
        node.children = vec![
            leaf("a.iso", seek_core::SpaceKind::File),
            leaf("", seek_core::SpaceKind::Smaller { count: 3 }),
            leaf("Other users", seek_core::SpaceKind::Hidden),
        ];
        let js = serde_json::to_string(&Response::SpaceTree { node: node.clone() }).unwrap();
        assert!(js.starts_with(r#"{"type":"spaceTree","node":{"name":"Games","kind":"folder","bytes":2"#), "{js}");
        assert!(js.contains(r#""kind":{"smaller":{"count":3}}"#), "{js}");
        assert!(js.contains(r#""kind":"hidden""#), "{js}");
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), Response::SpaceTree { node });
        let t = Response::SpaceTypes {
            types: vec![seek_core::TypeTotal { category: seek_core::TypeCategory::Video, bytes: 9, files: 1 }],
        };
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"{"type":"spaceTypes","types":[{"category":"video","bytes":9,"files":1}]}"#
        );
        let l = Request::SpaceLargest { device_id: uuid::Uuid::nil(), path: r"D:\".into(), limit: 50 };
        assert!(serde_json::to_string(&l).unwrap().starts_with(r#"{"type":"spaceLargest","deviceId":"#));
        assert_eq!(serde_json::to_string(&Response::SpaceFiles { files: vec![] }).unwrap(), r#"{"type":"spaceFiles","files":[]}"#);
    }
}
