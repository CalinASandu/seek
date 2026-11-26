//! Messages of a sync session (the JSON part of a [`crate::Frame`]; payloads noted per message).

use serde::{Deserialize, Serialize};

use crate::journal::Cursor;
use crate::pairing::MemberInfo;

/// Bumped on incompatible changes; peers with another version do not sync.
pub const SYNC_PROTOCOL: u32 = 2;

/// What the sender of a `Hello` is: a PC (LAN or hub client) or a hub.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    #[default]
    Pc,
    Hub,
}

/// One PC of a hub's catalog.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPc {
    pub host_id: uuid::Uuid,
    pub name: String,
    pub user_sid: String,
    pub user_name: String,
    /// `(SID, profile folder)` list.
    pub profiles: Vec<(String, String)>,
    pub last_seen_ms: i64,
}

/// Why a hub did not take or serve something.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Refusal {
    /// The device was claimed by another PC.
    NotOwner { device: uuid::Uuid },
    HubFull,
    QuotaFull,
    TooManyDevices,
    NotSupported,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Msg {
    /// First message each way.
    Hello {
        protocol: u32,
        host_id: uuid::Uuid,
        name: String,
        /// Absent in a v1 peer's hello (it is a PC).
        #[serde(default)]
        role: Role,
    },
    /// Hub only: "send me the devices of the other PCs; these are the cursors of my copies."
    Pull { cursors: Vec<(uuid::Uuid, Cursor)> },
    /// Hub only: the hub's PCs and their devices, answering `Pull`.
    Catalog { pcs: Vec<CatalogPc>, devices: Vec<seek_core::Device> },
    /// Hub only: everything a `Pull` asked for was sent.
    PullDone,
    /// Hub only: a request was not served.
    Refused { reason: Refusal },
    /// Reserved: open a relay channel to another PC.
    Open { channel: u16, to: uuid::Uuid },
    /// Reserved: relay channel data (payload).
    Data { channel: u16 },
    /// Reserved: close a relay channel.
    Close { channel: u16 },
    /// The sender's shared devices and its `(SID, profile folder)` list; sent at start and when they change.
    Devices { devices: Vec<seek_core::Device>, profiles: Vec<(String, String)> },
    /// Cursors of the receiver's devices held by the sender. Devices not listed are sent in full.
    /// Also sent later for single devices to ask for a fresh full copy (`Cursor::none()`).
    Have { cursors: Vec<(uuid::Uuid, Cursor)> },
    /// Payload: zstd(postcard(Vec<Change>)).
    Delta { device: uuid::Uuid, from: Cursor, to: Cursor },
    /// Payload: an encoded `.seekidx` (zstd level 1).
    Full { device: uuid::Uuid, cursor: Cursor },
    Ack { device: uuid::Uuid, cursor: Cursor },
    /// "Send me what you have now."
    SyncNow,
    /// The sender removed the link.
    Unlinked,
    /// A PC joined the group through the sender.
    MemberJoined { member: MemberInfo },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape() {
        let m = Msg::Ack { device: uuid::Uuid::nil(), cursor: Cursor::none() };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.starts_with(r#"{"t":"ack","device":"#), "{s}");
        assert_eq!(serde_json::from_str::<Msg>(&s).unwrap(), m);
    }

    fn round_trip(m: Msg, tag: &str) {
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.starts_with(&format!(r#"{{"t":"{tag}""#)), "{s}");
        assert_eq!(serde_json::from_str::<Msg>(&s).unwrap(), m);
    }

    #[test]
    fn hello_without_role_parses_as_pc() {
        let s = r#"{"t":"hello","protocol":1,"hostId":"00000000-0000-0000-0000-000000000000","name":"old"}"#;
        match serde_json::from_str::<Msg>(s).unwrap() {
            Msg::Hello { protocol, role, .. } => assert!(protocol == 1 && role == Role::Pc),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn v2_shapes() {
        let hello = Msg::Hello { protocol: SYNC_PROTOCOL, host_id: uuid::Uuid::nil(), name: "pc".into(), role: Role::Pc };
        let s = serde_json::to_string(&hello).unwrap();
        assert!(s.contains(r#""role":"pc""#), "{s}");
        assert_eq!(serde_json::from_str::<Msg>(&s).unwrap(), hello);
        assert_eq!(SYNC_PROTOCOL, 2);

        round_trip(Msg::Pull { cursors: vec![(uuid::Uuid::nil(), Cursor::none())] }, "pull");
        let pc = CatalogPc {
            host_id: uuid::Uuid::nil(),
            name: "pc".into(),
            user_sid: "S-1".into(),
            user_name: "me".into(),
            profiles: vec![("S-1".into(), "C:\\Users\\me".into())],
            last_seen_ms: 5,
        };
        round_trip(Msg::Catalog { pcs: vec![pc], devices: vec![] }, "catalog");
        round_trip(Msg::PullDone, "pullDone");
        round_trip(Msg::Refused { reason: Refusal::NotOwner { device: uuid::Uuid::nil() } }, "refused");
    }
}
