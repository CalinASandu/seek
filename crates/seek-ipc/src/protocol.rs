//! Wire protocol between `seek-service` and the app.
//!
//! Every frame is a `u32` little-endian byte length followed by a UTF-8 JSON [`Envelope`].
//! Responses carry the `id` of the request they answer; pushed events use `id = 0`.

use serde::{Deserialize, Serialize};

pub use seek_core::{Device, DeviceId, ResultRow, SortKey};

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
    }
}
