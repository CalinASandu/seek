//! Messages of a sync session (the JSON part of a [`crate::Frame`]; payloads noted per message).

use serde::{Deserialize, Serialize};

use crate::journal::Cursor;
use crate::pairing::MemberInfo;

/// Bumped on incompatible changes; peers with another version do not sync.
pub const SYNC_PROTOCOL: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Msg {
    /// First message each way.
    Hello { protocol: u32, host_id: uuid::Uuid, name: String },
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
}
