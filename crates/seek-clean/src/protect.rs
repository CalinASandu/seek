//! Protected places (safety rule 8). One table, in one place. See the spec's table:
//! `docs/superpowers/specs/2026-11-04-seek-cleanup-design.md`.

use std::path::{Path, PathBuf};

use windows::core::GUID;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_ProgramData,
    FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Favorites, FOLDERID_Music,
    FOLDERID_Pictures, FOLDERID_Programs, FOLDERID_SavedGames, FOLDERID_Videos, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86,
    FOLDERID_Profile, FOLDERID_ProgramFilesCommon, FOLDERID_ProgramFilesCommonX86, FOLDERID_RoamingAppData, FOLDERID_UserProfiles, FOLDERID_Windows, KF_FLAG_DEFAULT,
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
    /// The `Programs` folder of the current user's Start Menu, then the all-users one, as written.
    start_menus: Vec<PathBuf>,
    /// `FOLDERID_CommonPrograms` alone: the all-users Start Menu `Programs` folder. Kept apart
    /// from the user's `Programs`, which can be moved anywhere (even into ProgramData).
    common_programs: Option<PathBuf>,
    /// Where Windows says Desktop, Documents, Downloads, Pictures, Music, Videos, Saved Games and
    /// Favorites are, which may not be in the profile (`D:\Documents`, a OneDrive move).
    known_dirs: Vec<String>,
}

/// What Windows says a folder-valued environment variable is, for the variables a program could
/// poison to make a claim point somewhere else: Program Files, Common Files, ProgramData,
/// the Windows folder and its drive. Read from the system, never from the environment. `None` for
/// any other name.
pub fn trusted_folder_env(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "programfiles" | "programw6432" => known_raw(&FOLDERID_ProgramFiles),
        "programfiles(x86)" => known_raw(&FOLDERID_ProgramFilesX86),
        "commonprogramfiles" | "commonprogramw6432" => known_raw(&FOLDERID_ProgramFilesCommon),
        "commonprogramfiles(x86)" => known_raw(&FOLDERID_ProgramFilesCommonX86),
        "programdata" | "allusersprofile" => known_raw(&FOLDERID_ProgramData),
        "systemroot" | "windir" => windows_dir_trusted(),
        "systemdrive" => windows_dir_trusted().map(|w| w.chars().take(2).collect()),
        _ => None,
    }
}

/// The users folder (`C:\Users`) from Windows.
pub fn users_folder() -> Option<String> {
    known_raw(&FOLDERID_UserProfiles)
}

/// True for the names `trusted_folder_env` answers for.
pub fn is_trusted_env_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "programfiles" | "programw6432" | "programfiles(x86)" | "commonprogramfiles" | "commonprogramw6432" | "commonprogramfiles(x86)"
            | "programdata" | "allusersprofile" | "systemroot" | "windir" | "systemdrive"
    )
}

/// The Windows folder from Windows itself, never the environment.
fn windows_dir_trusted() -> Option<String> {
    known_raw(&FOLDERID_Windows)
}

/// The Windows folder as Windows spells it (`C:\Windows`). The uninstall launcher builds the path
/// of `msiexec.exe` from it, so it never depends on the registry's text.
pub fn windows_dir() -> Option<String> {
    known_raw(&FOLDERID_Windows).or_else(|| std::env::var("SystemRoot").ok().filter(|s| !s.is_empty()))
}

impl Places {
    /// The two Start Menu `Programs` folders (the user's, then all users'), real capitalization.
    pub fn start_menu_roots(&self) -> &[PathBuf] {
        &self.start_menus
    }

    /// True if `canon_path` (in `canon` form) is inside `<ProgramData>`.
    pub fn in_program_data(&self, canon_path: &str) -> bool {
        !self.program_data.is_empty() && is_inside(canon_path, &self.program_data)
    }

    /// True if `canon_path` (in `canon` form) is strictly inside a Start Menu `Programs` folder.
    pub fn in_start_menu(&self, canon_path: &str) -> bool {
        self.start_menus.iter().map(|r| canon(r)).any(|r| canon_path.len() > r.len() && is_inside(canon_path, &r))
    }

    /// True if `canon_path` (in `canon` form) is a `.lnk` file name inside a Start Menu `Programs`
    /// folder, at any depth. Whether it is a file is for the caller to know.
    pub fn is_start_menu_shortcut(&self, canon_path: &str) -> bool {
        canon_path.ends_with(".lnk") && self.in_start_menu(canon_path)
    }

    /// True if `canon_path` (in `canon` form) is, holds, or is inside a Start Menu `Programs` folder.
    fn touches_start_menu(&self, canon_path: &str) -> bool {
        self.start_menus.iter().map(|r| canon(r)).any(|r| is_inside(&r, canon_path) || is_inside(canon_path, &r))
    }

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
        let start_menus = [&FOLDERID_Programs, &FOLDERID_CommonPrograms].into_iter().filter_map(known_raw).map(PathBuf::from).collect();
        let common_programs = known_raw(&FOLDERID_CommonPrograms).map(PathBuf::from);
        let known_dirs = [
            &FOLDERID_Desktop,
            &FOLDERID_Documents,
            &FOLDERID_Downloads,
            &FOLDERID_Pictures,
            &FOLDERID_Music,
            &FOLDERID_Videos,
            &FOLDERID_SavedGames,
            &FOLDERID_Favorites,
        ]
        .into_iter()
        .filter_map(known)
        .collect();
        let places = Places {
            scan_roots,
            start_menus,
            common_programs,
            known_dirs,
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

    /// `<ProgramData>\Seek` (in `canon` form): where the service keeps `devices.json`, the indexes
    /// and the journals. `None` when ProgramData is unknown.
    pub fn seek_data_dir(&self) -> Option<PathBuf> {
        (!self.program_data.is_empty()).then(|| Path::new(&self.program_data).join("Seek"))
    }

    /// The roots whose contents the elevated process removes from, in `canon` form: Program Files
    /// and the Start Menu `Programs` folders. The folders above an item in one of them are held
    /// while it is recycled.
    pub fn guarded_roots(&self) -> Vec<String> {
        self.program_files.iter().cloned().chain(self.start_menus.iter().map(|m| canon(m))).collect()
    }

    /// True if `canon_path` (in `canon` form) is inside the all-users Start Menu `Programs`
    /// folder, which is `FOLDERID_CommonPrograms` and nothing else.
    pub fn in_all_users_start_menu(&self, canon_path: &str) -> bool {
        self.common_programs.as_ref().is_some_and(|m| is_inside(canon_path, &canon(m)))
    }

    /// Tests only: where the user's and the all-users Start Menu `Programs` folders are.
    pub fn with_start_menus_for_tests(mut self, user: &str, all_users: &str) -> Places {
        self.start_menus = vec![PathBuf::from(user), PathBuf::from(all_users)];
        self.common_programs = Some(PathBuf::from(all_users));
        self
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

    /// True if `canon_path` (in `canon` form) is `Common Files` of a Program Files root, or inside it.
    pub fn in_common_files(&self, canon_path: &str) -> bool {
        self.program_files.iter().any(|p| is_inside(canon_path, &format!("{}\\common files", p.trim_end_matches('\\'))))
    }

    /// True if `canon_path` (in `canon` form) is the user profile itself or one of its well-known
    /// folders (Desktop, Downloads and the like). Other folders in the profile, where tools such
    /// as miniconda or scoop live, are not.
    pub fn is_profile_or_known_folder(&self, canon_path: &str) -> bool {
        const KNOWN: [&str; 20] = [
            "desktop", "documents", "downloads", "pictures", "music", "videos", "favorites", "links", "saved games", "contacts",
            "searches", "appdata", "dropbox", "google drive", "icloud drive", "iclouddrive", "box", "creative cloud files",
            "3d objects", "source",
        ];
        // `OneDrive - Contoso` and `OneDrive - Personal` are sync roots too, and so are
        // `Dropbox (Personal)` style names.
        const KNOWN_PREFIXES: [&str; 2] = ["onedrive", "dropbox"];
        let at = canon_path.trim_end_matches('\\');
        if self.known_dirs.iter().any(|k| !k.is_empty() && k.trim_end_matches('\\') == at) {
            return true;
        }
        let profile = self.profile.trim_end_matches('\\');
        !profile.is_empty()
            && (canon_path == profile
                || canon_path.rsplit_once('\\').is_some_and(|(parent, name)| {
                    parent == profile && (KNOWN.contains(&name) || KNOWN_PREFIXES.iter().any(|p| name.starts_with(p)))
                }))
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

    /// Tests only: where Windows says the user's known folders are.
    pub fn with_known_dirs_for_tests(mut self, dirs: &[&str]) -> Places {
        self.known_dirs = dirs.iter().map(|d| canon(Path::new(d))).collect();
        self
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
        // Where Windows keeps the Start Menu under a profile and under ProgramData.
        let menu = |base: &str, tail: &str| (!base.is_empty()).then(|| Path::new(base).join(tail));
        let start_menus = [
            menu(profile, r"AppData\Roaming\Microsoft\Windows\Start Menu\Programs"),
            menu(program_data, r"Microsoft\Windows\Start Menu\Programs"),
        ]
        .into_iter()
        .flatten()
        .collect();
        Places {
            common_programs: menu(program_data, r"Microsoft\Windows\Start Menu\Programs"),
            start_menus,
            known_dirs: Vec::new(),
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
    // The one exception: a Start Menu shortcut an uninstall left (all users' shortcuts live there).
    let start_menu_link = why == Why::Uninstall && places.is_start_menu_shortcut(canon_path);
    if why != Why::StaleData && !start_menu_link && is_inside(canon_path, &places.program_data) {
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

/// The extra rules for what an uninstall may have left (`Why::Uninstall`). `is_dir` is what the
/// path is on disk.
/// - A folder may be anywhere an app can live, but never the user's profile or one of its
///   well-known folders, ProgramData, the package cache, or a Start Menu `Programs` folder or
///   anything above or inside it.
/// - A file must be a `.lnk` in or below a Start Menu `Programs` folder. Anything else is
///   `UnsupportedPath`.
pub fn uninstall_lock(places: &Places, canon_path: &str, is_dir: bool) -> Option<Lock> {
    if !is_dir {
        return (!places.is_start_menu_shortcut(canon_path)).then_some(Lock::UnsupportedPath);
    }
    if never_listed(places, canon_path) {
        return Some(Lock::UnsupportedPath);
    }
    if places.is_profile_or_known_folder(canon_path) || places.in_package_cache(canon_path) || places.touches_start_menu(canon_path) {
        return Some(Lock::SystemFolder);
    }
    None
}

/// `Local\Programs\<App>` is where per-user installers put apps, so the `Programs` folder alone
/// does not make what is in it shared.
const NOT_SHARED: &str = "programs";

/// True if `canon_path` (in `canon` form) lies below a scan root with any segment named in
/// `ALWAYS_CLAIMED` (`Microsoft`, `Windows`, `Common Files`, `WindowsPowerShell` and the rest):
/// such a folder is shared with Windows and other apps, and an uninstall never offers anything
/// inside it.
pub fn in_shared_folder(places: &Places, canon_path: &str) -> bool {
    places.scan_roots.iter().map(|r| canon(r)).filter(|r| canon_path.len() > r.len() && is_inside(canon_path, r)).any(|r| {
        canon_path[r.len()..]
            .split('\\')
            .filter(|s| !s.is_empty() && *s != NOT_SHARED)
            .any(|seg| crate::claims::ALWAYS_CLAIMED.iter().any(|a| a.eq_ignore_ascii_case(seg)))
    })
}

/// True if an uninstall must neither list nor enter this folder: anything `lock_for` or
/// `uninstall_lock` would lock, or anything inside a shared folder.
pub fn uninstall_blocked(places: &Places, canon_path: &str) -> bool {
    lock_for(places, canon_path, Why::Uninstall, &[]).is_some()
        || uninstall_lock(places, canon_path, true).is_some()
        || in_shared_folder(places, canon_path)
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
pub fn is_seek_data_file(places: &Places, p: &str) -> bool {
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
    fn the_profiles_well_known_folders_match_by_name_and_sync_roots_by_prefix() {
        let p = places();
        let known = |s: &str| p.is_profile_or_known_folder(&canon(Path::new(s)));
        for name in [
            "Documents", "Desktop", "OneDrive", "OneDrive - Contoso", "OneDrive - Personal", "ONEDRIVE for Business", "Dropbox",
            "Dropbox (Personal)", "Google Drive", "iCloudDrive", "iCloud Drive", "Box", "Creative Cloud Files", "3D Objects", "source",
        ] {
            assert!(known(&format!(r"C:\Users\me\{name}")), "{name}");
        }
        assert!(known(r"C:\Users\me"), "the profile itself");
        // Only a direct child of the profile, and only those names.
        assert!(!known(r"C:\Users\me\source\repos"));
        assert!(!known(r"C:\Users\me\Documents\Thing"));
        assert!(!known(r"C:\Users\me\Boxes"));
        assert!(!known(r"C:\Users\me\Sources"));
        assert!(!known(r"C:\Users\me\MyOneDrive"));
        assert!(!known(r"C:\Users\me\miniconda3"));
        assert!(!known(r"C:\Users\other\Documents"));
    }

    #[test]
    fn relocated_known_folders_are_locked_like_the_defaults() {
        let p = places().with_known_dirs_for_tests(&[r"D:\Documents", r"C:\Users\me\OneDrive - Contoso\Desktop", r"E:\Media\Pictures"]);
        for dir in [r"D:\Documents", r"d:\DOCUMENTS\", r"C:\Users\me\OneDrive - Contoso\Desktop", r"E:\Media\Pictures"] {
            assert!(p.is_profile_or_known_folder(&canon(Path::new(dir))), "{dir}");
            assert!(uninstall_lock(&p, &canon(Path::new(dir)), true).is_some(), "{dir}");
        }
        // Only the folder itself; what is inside, and the neighbours, are not known folders.
        for dir in [r"D:\Documents\Thing", r"D:\Docs", r"E:\Media"] {
            assert!(!p.is_profile_or_known_folder(&canon(Path::new(dir))), "{dir}");
        }
    }

    #[test]
    fn uninstall_folders_and_shortcuts_have_their_own_rules() {
        let p = places();
        let menu = r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs";
        let user_menu = r"C:\Users\me\AppData\Roaming\Microsoft\Windows\Start Menu\Programs";
        let c = |s: &str| canon(Path::new(s));
        // Shortcuts: a .lnk in or below either Programs folder, and nothing else.
        for ok in [format!(r"{menu}\Acme.lnk"), format!(r"{menu}\Acme\Acme.LNK"), format!(r"{user_menu}\A\B\c.lnk")] {
            assert_eq!(uninstall_lock(&p, &c(&ok), false), None, "{ok}");
            assert_eq!(lock_for(&p, &c(&ok), Why::Uninstall, &[]), None, "{ok}: ProgramData's rule gives way for a shortcut");
        }
        for bad in [format!(r"{menu}\Acme.exe"), format!(r"{menu}.lnk"), r"C:\Users\me\Documents\a.lnk".to_string(), r"C:\ProgramData\a.lnk".to_string()] {
            assert_eq!(uninstall_lock(&p, &c(&bad), false), Some(Lock::UnsupportedPath), "{bad}");
        }
        // Only an uninstall's shortcut gets past ProgramData.
        assert_eq!(lock_for(&p, &c(&format!(r"{menu}\Acme.lnk")), Why::Leftover, &[]), Some(Lock::AppData));
        assert_eq!(lock_for(&p, &c(r"C:\ProgramData\Vendor\a.lnk"), Why::Uninstall, &[]), Some(Lock::AppData));
        // Folders: the user's own, the Start Menu and what holds it, and ProgramData are out.
        for bad in [r"C:\Users\me\Documents", r"C:\Users\me\OneDrive - Contoso", menu, user_menu, r"C:\Users\me\AppData\Roaming\Microsoft\Windows", r"C:\ProgramData\Package Cache\x"] {
            assert!(uninstall_lock(&p, &c(bad), true).is_some() || lock_for(&p, &c(bad), Why::Uninstall, &[]).is_some(), "{bad}");
        }
        for ok in [r"D:\Apps\Acme", r"C:\Program Files\Acme", r"C:\Users\me\AppData\Local\Acme", r"C:\Users\me\miniconda3"] {
            assert_eq!(uninstall_lock(&p, &c(ok), true), None, "{ok}");
        }
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
