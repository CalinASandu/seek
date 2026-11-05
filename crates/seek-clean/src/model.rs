//! Types shared by the engine, the Tauri commands and (mirrored field for field) the UI.
//! Timestamps are unix epoch milliseconds. Everything serializes as camelCase.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Why an item is up for removal.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum Why {
    Duplicate,
    Leftover,
    Uninstall,
    StaleData,
}

/// Why an item cannot be removed. `NoRecycleBin` and `CloudPlaceholder` come from the pre-checks,
/// `LinkedPc` from the engine; the rest from `protect::lock_for`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum Lock {
    SystemFolder,
    ProgramFiles,
    OtherUser,
    AppData,
    ToolFolder,
    NoRecycleBin,
    CloudPlaceholder,
    Protected,
    LinkedPc,
    /// Not an absolute drive-letter path (relative, UNC, device, volume-GUID...). Fails closed.
    UnsupportedPath,
    /// The candidate is itself a link (junction or symlink). Never followed, never removed.
    ReparsePoint,
    /// A folder an app owns, found when the engine looked (at review or at removal).
    Claimed,
}

/// How sure Seek is that a leftover folder is dead.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    VeryLikely,
    Likely,
    NotSure,
    ProbablyInUse,
}

/// One line of the "why we think so" list.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Reason {
    pub text: String,
    pub points: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Candidate {
    /// Every copy of one group, removable or not. Locked copies still count as copies.
    #[serde(rename_all = "camelCase")]
    Duplicates { copies: Vec<PathBuf> },
    #[serde(rename_all = "camelCase")]
    Folder {
        path: PathBuf,
        why: Why,
        level: Option<Level>,
        reasons: Vec<Reason>,
    },
    /// Seek's own stale file in ProgramData. Needs admin.
    #[serde(rename_all = "camelCase")]
    StaleFile { path: PathBuf },
    /// A file on a linked PC. Always refused, kept here so the rule has one place.
    #[serde(rename_all = "camelCase")]
    LinkedPc { path: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItem {
    pub id: u32,
    pub path: String,
    pub size: u64,
    pub modified_ms: i64,
    pub is_dir: bool,
    pub files: u32,
    pub why: Why,
    pub lock: Option<Lock>,
    pub needs_admin: bool,
    pub group: Option<u32>,
    pub level: Option<Level>,
    pub reasons: Vec<Reason>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    pub token: String,
    pub items: Vec<ReviewItem>,
}

/// Serialized with a `kind` tag: `{ "kind": "moved", "binPath": ... }`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Outcome {
    Moved { bin_path: String },
    Left { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemOutcome {
    pub id: u32,
    pub path: String,
    pub size: u64,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommitResult {
    pub items: Vec<ItemOutcome>,
    pub moved_bytes: u64,
    pub stopped_early: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobItem {
    pub id: u32,
    pub path: String,
    pub size: u64,
    pub modified_ms: i64,
    pub why: Why,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ElevatedJob {
    pub user_sid: String,
    pub items: Vec<JobItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DupCopy {
    pub path: String,
    pub device_id: String,
    pub size: u64,
    pub modified: i64,
    pub lock: Option<Lock>,
    pub suggested_keep: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DupGroup {
    pub id: u32,
    pub name: String,
    pub size: u64,
    pub copies: Vec<DupCopy>,
    pub freeable: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LeftoverRow {
    pub path: String,
    pub parent: String,
    pub size: u64,
    pub modified: i64,
    pub needs_admin: bool,
    pub level: Level,
    pub reasons: Vec<Reason>,
    pub contents: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppRow {
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub size: u64,
    pub install_date: Option<i64>,
    pub icon: Option<String>,
    pub folder: Option<String>,
    pub note: Option<String>,
    pub running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_and_level_serialize_as_camel_case_strings() {
        assert_eq!(serde_json::to_string(&Lock::NoRecycleBin).unwrap(), "\"noRecycleBin\"");
        assert_eq!(serde_json::to_string(&Level::ProbablyInUse).unwrap(), "\"probablyInUse\"");
        assert_eq!(serde_json::to_string(&Why::StaleData).unwrap(), "\"staleData\"");
    }

    #[test]
    fn dup_copy_fields_are_camel_case() {
        let c = DupCopy {
            path: "c:\\a".into(),
            device_id: "d".into(),
            size: 1,
            modified: 2,
            lock: Some(Lock::ProgramFiles),
            suggested_keep: true,
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["deviceId"], "d");
        assert_eq!(v["suggestedKeep"], true);
        assert_eq!(v["lock"], "programFiles");
        assert_eq!(serde_json::from_value::<DupCopy>(v).unwrap(), c);
    }

    #[test]
    fn outcome_is_tagged() {
        let o = Outcome::Moved { bin_path: "x".into() };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["kind"], "moved");
        assert_eq!(v["binPath"], "x");
        assert_eq!(serde_json::from_value::<Outcome>(v).unwrap(), o);
    }
}
