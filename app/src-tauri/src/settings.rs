//! Per-user app settings in `%APPDATA%\Seek\settings.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub start_with_windows: bool,
    pub theme: String,
    pub exclude_folders: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { start_with_windows: true, theme: "system".into(), exclude_folders: Vec::new() }
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
        assert_eq!(json, r#"{"startWithWindows":true,"theme":"system","excludeFolders":[]}"#);
        let partial: Settings = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(partial.theme, "dark");
        assert!(partial.start_with_windows);
    }
}
