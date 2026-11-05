//! Per-user app settings in `%APPDATA%\Seek\settings.json`.

use std::path::{Path, PathBuf};

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

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join("Seek").join("settings.json"))
}

const NO_APPDATA: &str = "Windows didn't say where your settings are kept (no APPDATA folder), so they can't be read or saved.";

/// What a read of the settings file says: no file is `None`; a file that can't be read or parsed is
/// an error naming it, since the protected list may be inside.
fn read_settings(read: std::io::Result<Vec<u8>>, file: &Path) -> Result<Option<Settings>, String> {
    let unreadable = || format!("Your settings file ({}) couldn't be read.", file.display());
    match read {
        Ok(bytes) => serde_json::from_slice::<Settings>(&bytes).map(Some).map_err(|_| unreadable()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(unreadable()),
    }
}

/// The settings, or defaults when there is no file or it can't be read (so the app still opens;
/// `problem` tells the window why the defaults are showing).
pub fn load() -> Settings {
    path().and_then(|p| read_settings(std::fs::read(&p), &p).ok().flatten()).unwrap_or_default()
}

/// Why the saved settings can't be used, if they can't: no APPDATA, or a file that won't read.
pub fn problem() -> Option<String> {
    match path() {
        None => Some(NO_APPDATA.to_string()),
        Some(p) => read_settings(std::fs::read(&p), &p).err(),
    }
}

/// The "Always protect" list for a scan or a removal, read from disk each time. A missing file means
/// no list; no APPDATA, or a file that exists but can't be read, means no removal, since the list
/// may be in it.
pub fn protected_paths_strict() -> Result<Vec<String>, String> {
    protected_at(path())
}

fn protected_at(file: Option<PathBuf>) -> Result<Vec<String>, String> {
    let p = file.ok_or_else(|| NO_APPDATA.to_string())?;
    protected_from(std::fs::read(&p), &p)
}

fn protected_from(read: std::io::Result<Vec<u8>>, file: &Path) -> Result<Vec<String>, String> {
    match read_settings(read, file) {
        Ok(s) => Ok(s.map(|s| s.protected_paths).unwrap_or_default()),
        Err(e) => Err(format!("{e} Cleanup won't remove anything until it is fixed or deleted.")),
    }
}

/// Saves the settings, unless the file there exists and can't be read: overwriting it could lose
/// the protected list. Written to a temp file in the same folder and renamed over the old one.
pub fn save(s: &Settings) -> Result<(), String> {
    save_to(&path().ok_or_else(|| NO_APPDATA.to_string())?, s)
}

fn save_to(p: &Path, s: &Settings) -> Result<(), String> {
    read_settings(std::fs::read(p), p).map_err(|e| format!("{e} It wasn't overwritten; fix or delete it first."))?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    let tmp = p.with_file_name("settings.json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
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

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("seek-settings-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("settings.json")
    }

    #[test]
    fn the_protected_list_fails_closed_when_the_file_cannot_be_read() {
        let f = Path::new("X:\\Seek\\settings.json");
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(protected_from(Err(missing), f), Ok(vec![]));
        let e = protected_from(Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)), f).unwrap_err();
        assert!(e.contains("X:\\Seek\\settings.json") && e.contains("won't remove anything"), "{e}");
        assert!(protected_from(Ok(b"{ not json".to_vec()), f).is_err());
        assert_eq!(protected_from(Ok(br#"{"theme":"dark"}"#.to_vec()), f), Ok(vec![]));
        assert_eq!(protected_from(Ok(br#"{"protectedPaths":["D:\\keep"]}"#.to_vec()), f), Ok(vec!["D:\\keep".to_string()]));
    }

    #[test]
    fn no_appdata_is_an_error_not_an_empty_list() {
        let e = protected_at(None).unwrap_err();
        assert!(e.contains("APPDATA"), "{e}");
        let f = scratch("missing");
        assert_eq!(protected_at(Some(f)), Ok(vec![]), "a missing file is no list");
    }

    #[test]
    fn saving_refuses_to_overwrite_a_file_it_cannot_read_and_says_which() {
        let f = scratch("refuse");
        std::fs::write(&f, b"{ \"protectedPaths\": [\"D:\\\\keep\"").unwrap(); // cut off half way
        let before = std::fs::read(&f).unwrap();
        let e = save_to(&f, &Settings::default()).unwrap_err();
        assert!(e.contains(&*f.display().to_string()) && e.contains("wasn't overwritten"), "{e}");
        assert_eq!(std::fs::read(&f).unwrap(), before, "the file is untouched");
        // an unreadable-but-present file is a problem the window can show; a missing one is not
        assert!(read_settings(std::fs::read(&f), &f).is_err());
        std::fs::remove_file(&f).unwrap();
        assert_eq!(read_settings(std::fs::read(&f), &f), Ok(None));
    }

    #[test]
    fn saving_writes_a_temp_file_beside_it_and_renames_leaving_nothing_behind() {
        let f = scratch("rename");
        let mut s = Settings { protected_paths: vec!["D:\\keep".to_string()], ..Settings::default() };
        save_to(&f, &s).unwrap();
        assert_eq!(read_settings(std::fs::read(&f), &f).unwrap(), Some(s.clone()));
        s.theme = "dark".into();
        save_to(&f, &s).unwrap(); // over a good file
        assert_eq!(read_settings(std::fs::read(&f), &f).unwrap().unwrap().theme, "dark");
        let names: Vec<_> = std::fs::read_dir(f.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("settings.json")], "no temp file left");
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
