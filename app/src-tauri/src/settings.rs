//! Per-user app settings in `%APPDATA%\Seek\settings.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub start_with_windows: bool,
    pub theme: String,
    pub exclude_folders: Vec<String>,
    /// Files and folders Cleanup never touches ("Always protect"). Settings files from before this
    /// existed have no such key and load with none.
    pub protected_paths: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { start_with_windows: true, theme: "system".into(), exclude_folders: Vec::new(), protected_paths: Vec::new() }
    }
}

fn path() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join("Seek").join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The "Always protect" list for a scan or a removal, read from disk each time. A missing file means
/// no list; a file that exists but can't be read means no removal, since the list may be in it.
pub fn protected_paths_strict() -> Result<Vec<String>, String> {
    protected_from(std::fs::read(path()))
}

fn protected_from(read: std::io::Result<Vec<u8>>) -> Result<Vec<String>, String> {
    const UNREADABLE: &str = "Your settings file couldn't be read, so Cleanup won't remove anything until it can.";
    match read {
        Ok(bytes) => serde_json::from_slice::<Settings>(&bytes).map(|s| s.protected_paths).map_err(|_| UNREADABLE.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(_) => Err(UNREADABLE.to_string()),
    }
}

pub fn save(s: &Settings) -> Result<(), String> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(p, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_matches_ui_shape_and_tolerates_missing_fields() {
        let s = Settings::default();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"startWithWindows":true,"theme":"system","excludeFolders":[],"protectedPaths":[]}"#);
        let partial: Settings = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(partial.theme, "dark");
        assert!(partial.start_with_windows);
        assert!(partial.protected_paths.is_empty());
    }

    #[test]
    fn the_protected_list_fails_closed_when_the_file_cannot_be_read() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(protected_from(Err(missing)), Ok(vec![]));
        assert!(protected_from(Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))).is_err());
        assert!(protected_from(Ok(b"{ not json".to_vec())).is_err());
        assert_eq!(protected_from(Ok(br#"{"theme":"dark"}"#.to_vec())), Ok(vec![]));
        assert_eq!(protected_from(Ok(br#"{"protectedPaths":["D:\\keep"]}"#.to_vec())), Ok(vec!["D:\\keep".to_string()]));
    }

    #[test]
    fn a_settings_file_from_before_protected_paths_still_loads() {
        let old = r#"{"startWithWindows":false,"theme":"light","excludeFolders":["D:\\x"]}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert!(!s.start_with_windows);
        assert_eq!(s.exclude_folders, vec!["D:\\x".to_string()]);
        assert!(s.protected_paths.is_empty());
        let kept: Settings = serde_json::from_str(r#"{"protectedPaths":["D:\\keep"]}"#).unwrap();
        assert_eq!(kept.protected_paths, vec!["D:\\keep".to_string()]);
    }
}
