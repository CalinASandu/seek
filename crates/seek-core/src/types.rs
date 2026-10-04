//! Search result types shared with the IPC protocol and the UI.

use serde::{Deserialize, Serialize};

use crate::model::DeviceId;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "camelCase")]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Device,
}

/// One result row as sent to the UI.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResultRow {
    pub name: String,
    /// Containing folder, e.g. `C:\Users\me`.
    pub folder: String,
    pub full_path: String,
    pub size: u64,
    pub modified: i64,
    pub created: i64,
    pub is_dir: bool,
    pub attrs: u32,
    pub device_id: DeviceId,
    pub device_name: String,
    pub online: bool,
}
