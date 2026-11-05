//! Wire protocol between `seek-service` and the app.
//!
//! Every frame is a `u32` little-endian byte length followed by a UTF-8 JSON [`Envelope`].
//! Responses carry the `id` of the request they answer; pushed events use `id = 0`.

use serde::{Deserialize, Serialize};

pub use seek_core::{Device, DeviceId, ResultRow, SortKey, SpaceNode, TypeTotal};

pub const PIPE_NAME: &str = r"\\.\pipe\seek-v1";
pub const PROTOCOL_VERSION: u32 = 3;
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
    /// The window is showing: load the indexes (if they are not in memory) and keep them there a while.
    Wake,
    /// What the service's memory is made of (`seek memory`).
    Memory,
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
    /// Links this PC to the hub at `addr` (e.g. `tls://seek-hub.tailnet.ts.net`) that shows `code` (8 digits).
    /// Fails with an error message that starts with one of the `HUB_ERR_*` prefixes when the cause is known.
    LinkHub { addr: String, code: String },
    /// Unlinks from the hub, or dismisses the *hub unlinked this PC* notice.
    UnlinkHub,
    HubStatus,
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
    /// Files that share a size (at least `min_size` bytes) across `device_ids`, largest size first; only
    /// sizes held by two or more files. `cursor` is the largest size still to return (`None` = from the top).
    /// A page is whole size groups, about `limit` files, and always at least one group.
    SameSizeFiles { min_size: u64, device_ids: Vec<DeviceId>, cursor: Option<u64>, limit: u32 },
    /// Seek's own index and journal files that no known drive owns. Read-only.
    StaleData,
}

/// A file in the service's data folder that no known drive owns. `modified` is unix epoch milliseconds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StaleFile {
    pub path: String,
    pub size: u64,
    pub modified: i64,
}

/// The files of one size, see [`Request::SameSizeFiles`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SizeGroup {
    pub size: u64,
    pub files: Vec<ResultRow>,
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
    /// Answer to `LinkHub`: the hub's name.
    HubLinked { name: String },
    /// Answer to `HubStatus`: `None` when this PC is not linked to a hub.
    Hub { hub: Option<HubStatus> },
    Peers { this_pc: PcSummary, peers: Vec<PcSummary>, sharing: bool, unshared: Vec<DeviceId> },
    SpaceTree { node: SpaceNode },
    SpaceFiles { files: Vec<ResultRow> },
    SpaceTypes { types: Vec<TypeTotal> },
    /// `next` = the size to pass as `cursor` for the following page; `None` = that was the last page.
    SameSizeFiles { groups: Vec<SizeGroup>, next: Option<u64> },
    StaleData { files: Vec<StaleFile> },
    Memory { report: MemoryReport },
}

/// What the service's memory is made of. Sizes are in bytes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryReport {
    /// What Task Manager's Memory column shows.
    pub working_set: u64,
    pub private_bytes: u64,
    pub peak_working_set: u64,
    /// Bytes handed out by the allocator right now, and the most ever.
    pub heap_live: u64,
    pub heap_peak: u64,
    /// `None` = loaded indexes are never unloaded.
    pub idle_unload_secs: Option<u64>,
    /// How many times idle unloading has run.
    pub idle_checks: u64,
    pub devices: Vec<DeviceMemory>,
    /// Which code asked for indexes, and how often, since the service started.
    pub touches: Vec<(String, u64)>,
    pub journal_bytes: u64,
    pub mask_bytes: u64,
    pub totals_bytes: u64,
    pub stored_result_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceMemory {
    pub name: String,
    pub in_memory: bool,
    pub on_disk_only: bool,
    pub rows: u64,
    pub bytes: u64,
    pub columns: Vec<(String, u64)>,
    /// Seconds since something last used this index; `None` = not since the service started.
    pub idle_secs: Option<u64>,
    pub last_used_by: Option<String>,
    pub loads: u32,
    pub unloads: u32,
    /// Why idle unloading could not take it out of memory.
    pub refused: Option<String>,
}

/// Prefixes of the `Response::Error` message of `LinkHub` when the cause is one the app explains in its own words.
/// The rest of the message is a plain-English text for other clients.
pub const HUB_ERR_WRONG_CODE: &str = "[hub:wrong-code] ";
pub const HUB_ERR_NO_WINDOW: &str = "[hub:no-window] ";
pub const HUB_ERR_UNTRUSTED: &str = "[hub:untrusted] ";

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

/// This PC's link to a self-hosted hub.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HubStatus {
    pub name: String,
    /// The address the PC was linked with (as typed, canonicalised).
    pub addr: String,
    pub online: bool,
    /// Unix ms of the last update exchanged with the hub; 0 = never.
    pub last_sync_ms: i64,
    /// Why the last attempt failed, or what the hub refused or did (e.g. *The hub unlinked this PC*).
    pub last_error: Option<String>,
    /// The other PCs of the hub, as far as the last catalog told.
    pub pcs: Vec<PcSummary>,
    /// The hub removed this PC's link: this is only the notice, and the PC is back in LAN mode.
    #[serde(default)]
    pub unlinked: bool,
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
    fn hub_json_shapes() {
        let l = Request::LinkHub { addr: "tls://h.example".into(), code: "12345678".into() };
        assert_eq!(serde_json::to_string(&l).unwrap(), r#"{"type":"linkHub","addr":"tls://h.example","code":"12345678"}"#);
        assert_eq!(serde_json::to_string(&Request::UnlinkHub).unwrap(), r#"{"type":"unlinkHub"}"#);
        assert_eq!(serde_json::to_string(&Request::HubStatus).unwrap(), r#"{"type":"hubStatus"}"#);
        let n = Response::HubLinked { name: "Seek Hub".into() };
        assert_eq!(serde_json::to_string(&n).unwrap(), r#"{"type":"hubLinked","name":"Seek Hub"}"#);
        assert_eq!(serde_json::to_string(&Response::Hub { hub: None }).unwrap(), r#"{"type":"hub","hub":null}"#);
        let hub = HubStatus {
            name: "H".into(),
            addr: "tls://h.example".into(),
            online: true,
            last_sync_ms: 9,
            last_error: None,
            pcs: vec![],
            unlinked: false,
        };
        let r = Response::Hub { hub: Some(hub) };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(
            js,
            r#"{"type":"hub","hub":{"name":"H","addr":"tls://h.example","online":true,"lastSyncMs":9,"lastError":null,"pcs":[],"unlinked":false}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), r);
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

    #[test]
    fn stale_data_json_shapes() {
        assert_eq!(serde_json::to_string(&Request::StaleData).unwrap(), r#"{"type":"staleData"}"#);
        let resp = Response::StaleData { files: vec![StaleFile { path: r"C:\a.seekidx".into(), size: 3, modified: 4 }] };
        let js = serde_json::to_string(&resp).unwrap();
        assert_eq!(js, r#"{"type":"staleData","files":[{"path":"C:\\a.seekidx","size":3,"modified":4}]}"#);
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), resp);
    }

    #[test]
    fn same_size_json_shapes() {
        let r = Request::SameSizeFiles { min_size: 1_048_576, device_ids: vec![uuid::Uuid::nil()], cursor: None, limit: 200 };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(
            js,
            r#"{"type":"sameSizeFiles","minSize":1048576,"deviceIds":["00000000-0000-0000-0000-000000000000"],"cursor":null,"limit":200}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
        let resp = Response::SameSizeFiles { groups: vec![SizeGroup { size: 5, files: vec![] }], next: Some(4) };
        let js = serde_json::to_string(&resp).unwrap();
        assert_eq!(js, r#"{"type":"sameSizeFiles","groups":[{"size":5,"files":[]}],"next":4}"#);
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), resp);
    }
}
