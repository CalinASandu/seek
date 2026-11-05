//! Protected places (safety rule 8). One table, in one place. See the spec's table:
//! `docs/superpowers/specs/2026-11-04-seek-cleanup-design.md`.

use std::path::{Path, PathBuf};

use windows::core::GUID;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_ProgramData,
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Profile, FOLDERID_RoamingAppData,
    FOLDERID_UserProfiles, FOLDERID_Windows, KF_FLAG_DEFAULT,
};

use crate::model::{Lock, Why};
use crate::paths::{canon, is_inside};

/// The folder as Windows writes it (real capitalization), for showing and for reading.
fn known_raw(id: &GUID) -> Option<String> {
    // SAFETY: the returned buffer is owned by us and freed with CoTaskMemFree right after copying.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn known(id: &GUID) -> Option<String> {
    known_raw(id).map(|s| canon(Path::new(&s)))
}

/// The system's special folders, in `canon` form. Empty entries match nothing.
#[derive(Clone, Debug)]
pub struct Places {
    windows: String,
    program_files: Vec<String>,
    users_root: String,
    profile: String,
    program_data: String,
    app_data: Vec<String>,
    /// Program Files roots, then the AppData folders, as written (not `canon`): the places a
    /// leftovers scan lists the top-level folders of.
    scan_roots: Vec<PathBuf>,
}

impl Places {
    /// The folders whose top-level folders are leftover candidates: Program Files and its x86
    /// twin, then Local, Roaming and LocalLow AppData. Real capitalization.
    pub fn scan_roots(&self) -> &[PathBuf] {
        &self.scan_roots
    }

    /// Asks Windows for every folder, so nothing depends on a drive letter or a language.
    ///
    /// Fails if the Windows folder, Program Files, the users folder, your profile or ProgramData
    /// can't be found: Cleanup must refuse to run rather than run unprotected.
    pub fn from_system() -> Result<Places, String> {
        let windows = known(&FOLDERID_Windows)
            .or_else(|| std::env::var("SystemRoot").ok().map(|s| canon(Path::new(&s))))
            .unwrap_or_default();
        let program_files = [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86]
            .into_iter()
            .filter_map(known)
            .collect();
        let app_data = [&FOLDERID_LocalAppData, &FOLDERID_RoamingAppData, &FOLDERID_LocalAppDataLow]
            .into_iter()
            .filter_map(known)
            .collect();
        let scan_roots = [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86, &FOLDERID_LocalAppData, &FOLDERID_RoamingAppData, &FOLDERID_LocalAppDataLow]
            .into_iter()
            .filter_map(known_raw)
            .map(PathBuf::from)
            .collect();
        let places = Places {
            scan_roots,
            windows,
            program_files,
            users_root: known(&FOLDERID_UserProfiles).unwrap_or_default(),
            profile: known(&FOLDERID_Profile).unwrap_or_default(),
            program_data: known(&FOLDERID_ProgramData).unwrap_or_default(),
            app_data,
        };
        let missing: Vec<&str> = [
            ("Windows folder", places.windows.is_empty()),
            ("Program Files", places.program_files.is_empty()),
            ("users folder", places.users_root.is_empty()),
            ("user profile", places.profile.is_empty()),
            ("ProgramData", places.program_data.is_empty()),
        ]
        .into_iter()
        .filter(|(_, m)| *m)
        .map(|(n, _)| n)
        .collect();
        if missing.is_empty() {
            Ok(places)
        } else {
            Err(format!("Cannot determine protected places: {}", missing.join(", ")))
        }
    }

    /// True if `canon_path` (in `canon` form) is Program Files or inside it.
    pub fn in_program_files(&self, canon_path: &str) -> bool {
        self.program_files.iter().any(|p| is_inside(canon_path, p))
    }

    /// True if `canon_path` (in `canon` form) is the Windows folder or inside it.
    pub fn in_windows(&self, canon_path: &str) -> bool {
        is_inside(canon_path, &self.windows)
    }

    /// True if `canon_path` (in `canon` form) is a Program Files root itself, not a folder in it.
    pub fn is_program_files_root(&self, canon_path: &str) -> bool {
        self.program_files.iter().any(|p| p == canon_path.trim_end_matches('\\'))
    }

    /// True if `canon_path` (in `canon` form) is inside `<ProgramData>\Package Cache`.
    pub fn in_package_cache(&self, canon_path: &str) -> bool {
        !self.program_data.is_empty() && is_inside(canon_path, &format!("{}\\package cache", self.program_data.trim_end_matches('\\')))
    }

    /// True if `canon_path` (in `canon` form) is, or contains, any known place: Windows, a
    /// Program Files root, the users folder, your profile, ProgramData or an AppData folder.
    /// A folder like that is never a leftover.
    pub fn is_or_contains_place(&self, canon_path: &str) -> bool {
        [&self.windows, &self.users_root, &self.profile, &self.program_data]
            .into_iter()
            .chain(&self.program_files)
            .chain(&self.app_data)
            .any(|p| !p.is_empty() && is_inside(p, canon_path))
    }

    /// Tests only: one more folder whose top-level folders count as leftover candidates.
    pub fn with_scan_root_for_tests(mut self, root: &str) -> Places {
        self.scan_roots.push(PathBuf::from(root));
        self
    }

    /// Fixed places for tests. Paths are put through `canon`.
    pub fn for_tests(
        windows: &str,
        program_files: &[&str],
        users_root: &str,
        profile: &str,
        program_data: &str,
        app_data: &[&str],
    ) -> Places {
        let c = |s: &str| canon(Path::new(s));
        Places {
            scan_roots: program_files.iter().chain(app_data).map(PathBuf::from).collect(),
            windows: c(windows),
            program_files: program_files.iter().map(|s| c(s)).collect(),
            users_root: c(users_root),
            profile: c(profile),
            program_data: c(program_data),
            app_data: app_data.iter().map(|s| c(s)).collect(),
        }
    }
}

/// Why `canon_path` can't be removed for this purpose, if it can't. `canon_path` must be in
/// `canon` form. The pre-check results (`Lock::NoRecycleBin`, `BinFull`, `PathTooLong`, `CloudPlaceholder`)
/// and `LinkedPc` are not decided here.
pub fn lock_for(
    places: &Places,
    canon_path: &str,
    why: Why,
    user_protected: &[String],
) -> Option<Lock> {
    let dup = why == Why::Duplicate;
    if !is_supported(canon_path) {
        return Some(Lock::UnsupportedPath);
    }
    // Second guard: with no Windows folder known, nothing can be judged.
    if places.windows.is_empty() || is_system(places, canon_path) {
        return Some(Lock::SystemFolder);
    }
    if is_other_user(places, canon_path) {
        return Some(Lock::OtherUser);
    }
    if dup && places.program_files.iter().any(|p| is_inside(canon_path, p)) {
        return Some(Lock::ProgramFiles);
    }
    // ProgramData: duplicates are locked, leftovers are never listed (so locked here as a
    // second line of defence). Only Seek's own stale files (`StaleData`) may be touched.
    if why != Why::StaleData && is_inside(canon_path, &places.program_data) {
        return Some(Lock::AppData);
    }
    if dup && places.app_data.iter().any(|p| is_inside(canon_path, p)) {
        return Some(Lock::AppData);
    }
    if dup && in_tool_folder(canon_path) {
        return Some(Lock::ToolFolder);
    }
    if user_protected.iter().any(|p| is_inside(canon_path, &canon(Path::new(p)))) {
        return Some(Lock::Protected);
    }
    None
}

/// The extra rules for what may be a leftover folder or a stale file of Seek's: a leftover is a
/// direct child of a scan root that the scan would list; stale data is one of Seek's own index or
/// journal files. Anything else is `UnsupportedPath`. Judged on the path as written.
pub fn candidate_lock(places: &Places, canon_path: &str, why: Why) -> Option<Lock> {
    let ok = match why {
        Why::Leftover => is_scan_root_child(places, canon_path) && !never_listed(places, canon_path),
        Why::StaleData => is_seek_data_file(places, canon_path),
        Why::Duplicate | Why::Uninstall => true,
    };
    (!ok).then_some(Lock::UnsupportedPath)
}

/// True if `p` (in `canon` form) is a direct child of one of the leftover scan roots.
fn is_scan_root_child(places: &Places, p: &str) -> bool {
    places.scan_roots.iter().any(|r| {
        let r = canon(r);
        let r = r.trim_end_matches('\\');
        p.len() > r.len() + 1 && p.starts_with(r) && p.as_bytes()[r.len()] == b'\\' && !p[r.len() + 1..].contains('\\')
    })
}

/// True if `p` (in `canon` form) is one of Seek's own index or journal files: directly in
/// `<ProgramData>\Seek\indexes` as `.seekidx`, or in `<ProgramData>\Seek\journal` as `.log`.
fn is_seek_data_file(places: &Places, p: &str) -> bool {
    if places.program_data.is_empty() {
        return false;
    }
    [("indexes", ".seekidx"), ("journal", ".log")].iter().any(|(dir, ext)| {
        let prefix = format!("{}\\seek\\{dir}\\", places.program_data.trim_end_matches('\\'));
        p.strip_prefix(&prefix).is_some_and(|name| name.len() > ext.len() && name.ends_with(ext) && !name.contains('\\'))
    })
}

/// Leftover scans never list these at all.
pub fn never_listed(places: &Places, canon_path: &str) -> bool {
    !is_supported(canon_path)
        || places.windows.is_empty()
        || is_system(places, canon_path)
        || is_other_user(places, canon_path)
        || is_inside(canon_path, &places.program_data)
}

/// Only an absolute path on a drive letter (`x:\...`) or a drive root (`x:`) can be removed.
/// Relative, drive-relative, UNC, `\\?\Volume{}`, GLOBALROOT and device paths all fail closed.
fn is_supported(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || b[2] == b'\\')
}

fn is_drive_root(p: &str) -> bool {
    p.len() == 2 && p.as_bytes()[1] == b':'
}

/// Windows folder, `$Recycle.Bin` and `System Volume Information` at a drive root, drive roots.
fn is_system(places: &Places, p: &str) -> bool {
    if is_drive_root(p) || is_inside(p, &places.windows) {
        return true;
    }
    let b = p.as_bytes();
    if b.len() > 2 && b[1] == b':' {
        let first = p.split('\\').nth(1).unwrap_or("");
        return first == "$recycle.bin" || first == "system volume information";
    }
    false
}

/// Under the users folder but not under your own profile. The users folder itself counts.
fn is_other_user(places: &Places, p: &str) -> bool {
    is_inside(p, &places.users_root) && !is_inside(p, &places.profile)
}

/// Strictly inside one of the tool folders (the last segment is the item itself).
fn in_tool_folder(p: &str) -> bool {
    let mut segs: Vec<&str> = p.split('\\').collect();
    segs.pop();
    segs.iter().any(|s| matches!(*s, ".git" | "node_modules" | "target" | ".venv"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Places {
        Places::for_tests(
            r"C:\Windows",
            &[r"C:\Program Files", r"C:\Program Files (x86)"],
            r"C:\Users",
            r"C:\Users\me",
            r"C:\ProgramData",
            &[
                r"C:\Users\me\AppData\Local",
                r"C:\Users\me\AppData\Roaming",
                r"C:\Users\me\AppData\LocalLow",
            ],
        )
    }

    fn lock(p: &str, why: Why) -> Option<Lock> {
        lock_p(p, why, &[])
    }

    fn lock_p(p: &str, why: Why, prot: &[String]) -> Option<Lock> {
        lock_for(&places(), &canon(Path::new(p)), why, prot)
    }

    fn hidden(p: &str) -> bool {
        never_listed(&places(), &canon(Path::new(p)))
    }

    #[test]
    fn spellings_of_windows_all_lock() {
        for p in [
            r"c:\WINDOWS\system32\x",
            r"C:\Users\me\..\..\Windows\x",
            r"\\?\C:\Windows\x",
            r"C:\Windows\",
        ] {
            assert_eq!(lock(p, Why::Duplicate), Some(Lock::SystemFolder), "{p}");
            assert_eq!(lock(p, Why::Leftover), Some(Lock::SystemFolder), "{p}");
            assert!(hidden(p), "{p}");
        }
    }

    #[test]
    fn recycle_bin_and_volume_information_and_drive_roots() {
        for p in [
            r"C:\$Recycle.Bin\S-1-5\x",
            r"d:\System Volume Information\x",
            r"D:\$RECYCLE.BIN",
            r"C:\",
            r"E:",
        ] {
            assert_eq!(lock(p, Why::Duplicate), Some(Lock::SystemFolder), "{p}");
            assert_eq!(lock(p, Why::Leftover), Some(Lock::SystemFolder), "{p}");
            assert!(hidden(p), "{p}");
        }
        assert_eq!(lock(r"D:\Photos\a.jpg", Why::Duplicate), None);
        assert!(!hidden(r"D:\Photos"));
    }

    #[test]
    fn alternate_data_stream_spellings_lock_like_the_folder() {
        for p in [
            r"C:\Windows:x",
            r"C:\Windows::$INDEX_ALLOCATION",
            r"C:\Windows::$INDEX_ALLOCATION\system32\x",
            r"C:\$Recycle.Bin::$INDEX_ALLOCATION",
            r"C:\Windows\system32:evil:$DATA",
        ] {
            assert_eq!(lock(p, Why::Duplicate), Some(Lock::SystemFolder), "{p}");
            assert!(hidden(p), "{p}");
        }
        assert_eq!(lock(r"C:\Program Files:x", Why::Duplicate), Some(Lock::ProgramFiles));
        assert_eq!(lock(r"C:\Program Files::$INDEX_ALLOCATION", Why::Duplicate), Some(Lock::ProgramFiles));
        assert_eq!(lock(r"C:\Program Files:x", Why::Leftover), None);
        assert_eq!(canon(Path::new(r"D:\a\b.txt:stream:$DATA")), r"d:\a\b.txt");
    }

    #[test]
    fn anything_but_an_absolute_drive_path_is_unsupported() {
        for p in [
            "",
            r"Windows\x",
            r"C:Windows\x",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\Windows\x",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Windows\x",
            r"\\localhost\c$\Windows\x",
            r"\\server\share\x",
            r"\\?\UNC\server\share\x",
            r"\\.\PhysicalDrive0",
            r"\\.\GLOBALROOT\Device\x",
            r"\x",
        ] {
            assert_eq!(lock(p, Why::Duplicate), Some(Lock::UnsupportedPath), "{p:?}");
            assert_eq!(lock(p, Why::Leftover), Some(Lock::UnsupportedPath), "{p:?}");
            assert_eq!(lock(p, Why::StaleData), Some(Lock::UnsupportedPath), "{p:?}");
            assert!(hidden(p), "{p:?}");
        }
        // The device-prefixed drive spelling is a plain drive path once canonical.
        assert_eq!(lock(r"\\.\C:\Windows\x", Why::Duplicate), Some(Lock::SystemFolder));
    }

    #[test]
    fn empty_windows_place_locks_everything() {
        let p = Places::for_tests("", &[], "", "", "", &[]);
        assert_eq!(lock_for(&p, r"d:\photos\a.jpg", Why::Duplicate, &[]), Some(Lock::SystemFolder));
        assert!(never_listed(&p, r"d:\photos"));
    }

    #[test]
    fn a_lookalike_folder_is_not_program_files() {
        assert_eq!(lock(r"C:\Program Files Extra\x", Why::Duplicate), None);
        assert_eq!(lock(r"C:\Windows Old\x", Why::Duplicate), None);
        assert_eq!(lock(r"C:\Users\me2\x", Why::Duplicate), Some(Lock::OtherUser));
    }

    #[test]
    fn program_files_locks_duplicates_but_not_leftovers() {
        for pf in [r"C:\Program Files", r"C:\Program Files (x86)"] {
            let dup = format!(r"{pf}\App\a.bin");
            let app = format!(r"{pf}\App");
            assert_eq!(lock(&dup, Why::Duplicate), Some(Lock::ProgramFiles));
            assert_eq!(lock(&app, Why::Leftover), None);
            assert_eq!(lock(&app, Why::Uninstall), None);
            assert!(!hidden(&app));
        }
    }

    #[test]
    fn inside_a_tool_folder_locks_at_any_depth() {
        for p in [
            r"D:\code\proj\.git\objects\ab\cd",
            r"D:\code\proj\node_modules\left-pad\a\b\c\x.js",
            r"D:\code\proj\Target\debug\x.exe",
            r"D:\code\.venv\Lib\x.py",
        ] {
            assert_eq!(lock(p, Why::Duplicate), Some(Lock::ToolFolder), "{p}");
        }
        assert_eq!(lock(r"D:\code\target", Why::Duplicate), None, "a file called target");
        assert_eq!(lock(r"D:\code\targets\x", Why::Duplicate), None);
        assert_eq!(lock(r"D:\code\my.git\x", Why::Duplicate), None);
    }

    #[test]
    fn user_protected_folder_locks_everything_inside_it() {
        let prot = vec![r"D:\Keep".to_string(), r"E:\Thesis\final.docx".to_string()];
        for why in [Why::Duplicate, Why::Leftover, Why::Uninstall, Why::StaleData] {
            assert_eq!(lock_p(r"d:\keep\a\b.txt", why, &prot), Some(Lock::Protected));
            assert_eq!(lock_p(r"D:\KEEP", why, &prot), Some(Lock::Protected));
            assert_eq!(lock_p(r"E:\Thesis\final.docx", why, &prot), Some(Lock::Protected));
            assert_eq!(lock_p(r"D:\Keeper\a.txt", why, &prot), None);
            assert_eq!(lock_p(r"E:\Thesis\other.docx", why, &prot), None);
        }
        // A protected Program Files app is locked for leftovers too.
        let pf = vec![r"C:\Program Files\App".to_string()];
        assert_eq!(lock_p(r"C:\Program Files\App", Why::Leftover, &pf), Some(Lock::Protected));
        // Protected entries are compared in canonical form.
        let odd = vec![r"d:/keep/../Keep2/".to_string()];
        assert_eq!(lock_p(r"D:\Keep2\x", Why::Duplicate, &odd), Some(Lock::Protected));
    }

    #[test]
    fn other_users_profile_locks_and_your_own_does_not() {
        assert_eq!(lock(r"C:\Users\bob\Documents\a.txt", Why::Duplicate), Some(Lock::OtherUser));
        assert_eq!(lock(r"C:\Users\Public\a.txt", Why::Duplicate), Some(Lock::OtherUser));
        assert_eq!(lock(r"C:\Users\bob", Why::Leftover), Some(Lock::OtherUser));
        assert_eq!(lock(r"C:\Users", Why::Leftover), Some(Lock::OtherUser));
        assert!(hidden(r"C:\Users\bob\AppData\Local\Thing"));
        assert!(hidden(r"C:\Users"));
        assert_eq!(lock(r"C:\Users\me\Documents\a.txt", Why::Duplicate), None);
        assert_eq!(lock(r"C:\Users\ME\Documents\a.txt", Why::Duplicate), None);
        assert!(!hidden(r"C:\Users\me\Documents\Thing"));
    }

    #[test]
    fn app_data_locks_duplicates_and_is_listed_for_leftovers() {
        for p in [
            r"C:\Users\me\AppData\Local\App\cache.bin",
            r"C:\Users\me\AppData\Roaming\App\cfg.json",
            r"C:\Users\me\AppData\LocalLow\App\x",
        ] {
            assert_eq!(lock(p, Why::Duplicate), Some(Lock::AppData), "{p}");
        }
        for p in [r"C:\Users\me\AppData\Local\OldApp", r"C:\Users\me\AppData\Roaming\OldApp"] {
            assert_eq!(lock(p, Why::Leftover), None, "{p}");
            assert!(!hidden(p), "{p}");
        }
        // Your own Documents next to AppData is not AppData.
        assert_eq!(lock(r"C:\Users\me\AppDataX\a", Why::Duplicate), None);
    }

    #[test]
    fn program_data_is_never_listed_apart_from_seeks_stale_files() {
        assert_eq!(lock(r"C:\ProgramData\Vendor\x.bin", Why::Duplicate), Some(Lock::AppData));
        assert_eq!(lock(r"C:\ProgramData\Vendor", Why::Leftover), Some(Lock::AppData));
        assert!(hidden(r"C:\ProgramData\Vendor"));
        assert!(hidden(r"C:\ProgramData"));
        assert_eq!(lock(r"C:\ProgramData\Seek\old.json", Why::StaleData), None);
    }

    #[test]
    fn drive_root_is_never_listed() {
        assert!(hidden(r"C:\"));
        assert!(hidden(r"d:"));
        assert!(hidden(r"\\?\E:\"));
        assert!(!hidden(r"D:\x"));
    }

    #[test]
    fn real_system_places_agree_with_the_environment() {
        let places = Places::from_system().expect("system places");
        let win = std::env::var("SystemRoot").unwrap();
        let cw = canon(Path::new(&win));
        assert_eq!(lock_for(&places, &format!(r"{cw}\system32\x"), Why::Duplicate, &[]), Some(Lock::SystemFolder));
        let pf = canon(Path::new(&std::env::var("ProgramFiles").unwrap()));
        assert_eq!(lock_for(&places, &format!(r"{pf}\a\b"), Why::Duplicate, &[]), Some(Lock::ProgramFiles));
        assert_eq!(lock_for(&places, &format!(r"{pf}\a"), Why::Leftover, &[]), None);
        let profile = canon(Path::new(&std::env::var("USERPROFILE").unwrap()));
        assert_eq!(lock_for(&places, &format!(r"{profile}\documents\a.txt"), Why::Duplicate, &[]), None);
        let local = canon(Path::new(&std::env::var("LOCALAPPDATA").unwrap()));
        assert_eq!(lock_for(&places, &format!(r"{local}\app\a"), Why::Duplicate, &[]), Some(Lock::AppData));
        let roaming = canon(Path::new(&std::env::var("APPDATA").unwrap()));
        assert_eq!(lock_for(&places, &format!(r"{roaming}\app\a"), Why::Duplicate, &[]), Some(Lock::AppData));
        let low = format!(r"{profile}\appdata\locallow\app\a");
        assert_eq!(lock_for(&places, &low, Why::Duplicate, &[]), Some(Lock::AppData));
        let pd = canon(Path::new(&std::env::var("ProgramData").unwrap()));
        assert!(never_listed(&places, &format!(r"{pd}\vendor")));
        assert!(!never_listed(&places, &format!(r"{local}\oldapp")));
        let users = canon(Path::new(profile.rsplit_once('\\').unwrap().0));
        assert!(never_listed(&places, &format!(r"{users}\someone-else\x")));
    }
}
