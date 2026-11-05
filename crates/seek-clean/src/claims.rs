//! What claims a folder: an installed app, a service, a startup entry, a shortcut or a running
//! program. A claimed folder is never offered for removal.
//!
//! Everything that reads the machine is in `read()` and the `real` module. The rest is pure
//! (parsing registry values and command lines, matching names), so the tests run anywhere.
//!
//! Two rules shape the design:
//! - Fail closed. If a whole source can't be listed, every folder is claimed and the answer
//!   names the source. One unreadable item (a process of another user, a bad shortcut) is
//!   skipped and counted in [`SystemClaims::skipped`].
//! - A claim that is too broad hides nothing. An uninstall entry that says its install location
//!   is `C:\Program Files` claims that exact place, not what is inside it.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::facts::ClaimCheck;
use crate::paths::{canon, canon_lexical, is_inside};

/// Folders that belong to Windows or are shared by everything, matched on the folder's own name,
/// case-insensitively. One table, with its own tests. No third-party vendor belongs here: an
/// app's folders are claimed because the app is installed, not because of who made it.
pub const ALWAYS_CLAIMED: &[&str] = &[
    "Microsoft",
    "Windows",
    "Common Files",
    "WindowsApps",
    "Packages",
    "Programs",
    "Temp",
    "Seek-dev",
    "Seek",
    ".IdentityService",
    "VirtualStore",
    "PlaceholderTileLogoFolder",
    "PeerDistRepub",
    "ToastNotificationManagerCompat",
    "Windows Kits",
    "Windows Sidebar",
    "Windows Multimedia Platform",
    "Uninstall Information",
    "wsl",
    "ASP.NET",
    // The Seek window's own data folder (the Tauri identifier), and the one an older build used.
    "com.calinsandrei.seek",
    "dev.seek.app",
    "Microsoft.NET",
    "Windows Defender",
    "Windows NT",
    "WindowsPowerShell",
    "Internet Explorer",
    "ModifiableWindowsApps",
    "Reference Assemblies",
    "MSBuild",
    "dotnet",
    "Microsoft OneDrive",
    "ConnectedDevicesPlatform",
    "D3DSCache",
    "CrashDumps",
    "Publishers",
    "Comms",
    "Windows Mail",
    "Windows Media Player",
    "Windows Photo Viewer",
    "Windows Portable Devices",
    "Windows Security",
];

/// How old the list of running programs may get before the next question reads it again.
pub const RUNNING_MAX_AGE: Duration = Duration::from_secs(2);

/// How old the list of installed apps may get. Reading it takes a few hundred milliseconds
/// (every Start Menu shortcut is opened), and installing something while a removal is being
/// checked is rare.
pub const INSTALLED_MAX_AGE: Duration = Duration::from_secs(30);

// ---- the data ------------------------------------------------------------------------------------

/// A path that something points at, and who that is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathClaim {
    pub path: String,
    /// Readable, for the screen: "the installed app Foo", "a running program (C:\...\foo.exe)".
    pub who: String,
    pub source: &'static str,
}

/// An installed app's display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameClaim {
    pub name: String,
    pub source: &'static str,
}

/// A program's own name (the file name of its exe without `.exe`). Apps keep their data in a
/// folder of that name whatever their registered display name says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramName {
    /// Lowercase.
    pub stem: String,
    pub who: String,
    pub source: &'static str,
}

/// Items that could not be read, one count per source. Being refused a process of another user
/// or an elevated one is normal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    pub processes: usize,
    pub shortcuts: usize,
    pub registry_values: usize,
}

/// What `read()` found, before it is indexed. Tests build this by hand.
#[derive(Clone, Debug, Default)]
pub struct ClaimParts {
    pub paths: Vec<PathClaim>,
    pub names: Vec<NameClaim>,
    /// The `Publisher` of every uninstall entry that has one, as registered.
    pub publishers: Vec<String>,
    pub program_names: Vec<ProgramName>,
    /// Places so wide that a claim at or above one of them says nothing about its folders.
    pub broad_roots: Vec<String>,
    /// Sources that could not be listed at all. Any entry claims every folder.
    pub failed: Vec<String>,
    pub skipped: Skipped,
}

impl ClaimParts {
    pub(crate) fn merge(mut self, other: ClaimParts) -> ClaimParts {
        self.paths.extend(other.paths);
        self.names.extend(other.names);
        self.publishers.extend(other.publishers);
        self.program_names.extend(other.program_names);
        self.broad_roots.extend(other.broad_roots);
        self.failed.extend(other.failed);
        self.skipped.processes += other.skipped.processes;
        self.skipped.shortcuts += other.skipped.shortcuts;
        self.skipped.registry_values += other.skipped.registry_values;
        self
    }
}

/// The fields of one uninstall entry that can point at a folder, and the ones the Apps list shows.
#[derive(Clone, Debug, Default)]
pub struct UninstallEntry {
    /// `"HKLM"` or `"HKCU"`. Empty for an entry built by hand.
    pub hive: String,
    /// `"64"` or `"32"`: the registry view the entry was read from.
    pub view: String,
    /// The name of the key under `Uninstall`.
    pub key: String,
    pub display_version: String,
    /// `EstimatedSize`, in KB, as registered.
    pub estimated_size_kb: Option<u64>,
    /// `InstallDate` as registered (`YYYYMMDD`, but anything can be there).
    pub install_date: String,
    pub system_component: bool,
    pub parent_key_name: String,
    pub display_name: String,
    pub publisher: String,
    pub install_location: String,
    pub display_icon: String,
    pub uninstall_string: String,
    pub quiet_uninstall_string: String,
}

struct IndexedPath {
    canon: String,
    /// At or above a broad root: may claim itself and what is inside it, never a folder under it
    /// that merely sits beneath it.
    broad: bool,
    who: String,
}

pub struct SystemClaims {
    paths: Vec<IndexedPath>,
    names: Vec<NameClaim>,
    program_names: Vec<ProgramName>,
    failed: Vec<String>,
    counts: BTreeMap<&'static str, usize>,
    pub skipped: Skipped,
}

impl SystemClaims {
    pub fn from_parts(parts: ClaimParts) -> SystemClaims {
        let broad_roots: Vec<String> = parts.broad_roots.iter().map(|r| canon_lexical(Path::new(r))).collect();
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut paths = Vec::with_capacity(parts.paths.len());
        let mut seen = HashSet::new();
        for c in parts.paths {
            *counts.entry(c.source).or_default() += 1;
            let canon = canon_lexical(Path::new(&c.path));
            if !seen.insert((canon.clone(), c.who.clone())) {
                continue;
            }
            let broad = broad_roots.iter().any(|r| is_inside(r, &canon));
            paths.push(IndexedPath { canon, broad, who: c.who });
        }
        for n in &parts.names {
            *counts.entry(n.source).or_default() += 1;
        }
        for n in &parts.program_names {
            *counts.entry(n.source).or_default() += 1;
        }
        SystemClaims { paths, names: parts.names, program_names: parts.program_names, failed: parts.failed, counts, skipped: parts.skipped }
    }

    /// Reads this PC: the uninstall lists, services, startup entries, App Paths, Start Menu
    /// shortcuts and running programs. Only reads.
    pub fn read() -> SystemClaims {
        SystemClaims::from_parts(real::read_installed_parts().merge(real::read_running_parts()))
    }

    /// Installed apps, services, startup entries and shortcuts, without the running programs.
    pub fn read_installed() -> SystemClaims {
        SystemClaims::from_parts(real::read_installed_parts())
    }

    /// Only the running programs.
    pub fn read_running() -> SystemClaims {
        SystemClaims::from_parts(real::read_running_parts())
    }

    /// How many claims each source gave.
    pub fn counts(&self) -> &BTreeMap<&'static str, usize> {
        &self.counts
    }

    pub fn failed_sources(&self) -> &[String] {
        &self.failed
    }
}

impl ClaimCheck for SystemClaims {
    fn is_claimed(&self, folder: &Path) -> Option<String> {
        if !self.failed.is_empty() {
            return Some(format!(
                "Couldn't check {}, so Seek can't tell whether a program uses this folder",
                self.failed.join(" and ")
            ));
        }
        let name = folder.file_name().map(|n| n.to_string_lossy().into_owned());
        if let Some(n) = &name {
            if ALWAYS_CLAIMED.iter().any(|a| a.eq_ignore_ascii_case(n)) {
                return Some("A Windows or shared folder".to_string());
            }
        }
        let c = canon(folder);
        for p in &self.paths {
            let inside_folder = is_inside(&p.canon, &c);
            if inside_folder || (!p.broad && is_inside(&c, &p.canon)) {
                return Some(p.who.clone());
            }
        }
        if let Some(n) = &name {
            let lower = n.to_lowercase();
            if let Some(p) = self.program_names.iter().find(|p| p.stem == lower) {
                return Some(p.who.clone());
            }
            if let Some(app) = self.names.iter().find(|a| whole_words_match(n, &a.name)) {
                return Some(format!("The installed app {}", app.name));
            }
        }
        None
    }
}

// ---- names ---------------------------------------------------------------------------------------

/// Words are runs of letters and digits; `+` counts as a letter so `Notepad++` is not `Notepad`.
pub(crate) fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '+'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A trailing token of an app name that only says which build it is.
fn is_build_token(token: &str) -> bool {
    let t = token
        .trim_matches(|c| matches!(c, '(' | ')' | '[' | ']' | ',' | ';'))
        .to_lowercase();
    if t.is_empty() {
        return true;
    }
    if matches!(
        t.as_str(),
        "x64" | "x86" | "x32" | "arm64" | "amd64" | "win64" | "win32" | "64-bit" | "32-bit" | "64bit" | "32bit"
    ) {
        return true;
    }
    let v = t.strip_prefix('v').unwrap_or(&t);
    if v.starts_with(|c: char| c.is_ascii_digit()) && v.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '_')) {
        return true;
    }
    // A locale such as en-US.
    let b = t.as_bytes();
    b.len() == 5 && (b[2] == b'-' || b[2] == b'_') && b[..2].iter().chain(&b[3..]).all(u8::is_ascii_alphabetic)
}

/// True if the two names are the same words in the same order, ignoring case and punctuation.
/// Trailing version and architecture tokens of `app_name` are ignored. Never true for no words.
pub fn whole_words_match(folder_name: &str, app_name: &str) -> bool {
    let folder = words(folder_name);
    if folder.is_empty() {
        return false;
    }
    // The name as registered ("Cubase 13") or without its trailing build words ("Foo (x64)").
    if words(app_name) == folder {
        return true;
    }
    let mut tokens: Vec<&str> = app_name.split_whitespace().collect();
    while tokens.len() > 1 && tokens.last().is_some_and(|t| is_build_token(t)) {
        tokens.pop();
    }
    words(&tokens.join(" ")) == folder
}

// ---- values --------------------------------------------------------------------------------------

/// True if there were shortcuts and none of them could be opened (`opened` per link): then the
/// reader itself is broken, and the source counts as failed rather than as empty.
pub(crate) fn every_link_failed(opened: &[bool]) -> bool {
    !opened.is_empty() && opened.iter().all(|o| !o)
}

/// `GetDriveTypeW`'s answer for a fixed local disk.
const DRIVE_FIXED: u32 = 3;

/// Whether an 8.3 spelling in `path` may be looked up on the disk: only on a fixed local drive.
/// `drive_type` gives the drive type of a letter. Anywhere else the path stays as written.
pub(crate) fn should_expand_short_names(path: &str, drive_type: &mut dyn FnMut(char) -> u32) -> bool {
    if !path.contains('~') {
        return false;
    }
    let b = path.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && drive_type(b[0] as char) == DRIVE_FIXED
}

/// True if `path` starts with a drive letter whose type `drive_type` says is a fixed local disk.
/// UNC paths, relative paths and removable, optical, network or unknown drives are not.
pub(crate) fn on_fixed_drive(path: &str, drive_type: &mut dyn FnMut(char) -> u32) -> bool {
    let b = path.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && drive_type((b[0] as char).to_ascii_uppercase()) == DRIVE_FIXED
}

/// [`on_fixed_drive`] asked of this PC. Nothing is read from the path itself.
pub fn is_fixed_local(path: &str) -> bool {
    on_fixed_drive(path, &mut real::drive_type)
}

/// `path` with 8.3 names (`PROGRA~1`) written out, in the disk's own case, when it is on a fixed
/// local drive; otherwise as given. This is the rule the claims use (`should_expand_short_names`).
pub fn expand_short_spelling(path: &str) -> String {
    if should_expand_short_names(path, &mut real::drive_type) {
        crate::paths::expand_short(&path.replace('/', "\\"))
    } else {
        path.to_string()
    }
}

/// Replaces `%NAME%` with the value `env` gives. Unknown names and stray `%` stay as they are.
pub fn expand_env(s: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => match env(&after[..end]) {
                Some(v) => {
                    out.push_str(&v);
                    rest = &after[end + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            },
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn is_abs(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || s.starts_with("\\\\")
}

/// `\??\C:\x` and `\SystemRoot\x`, the shapes a service's image path uses.
fn fix_prefix(s: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let s = s.trim();
    if let Some(r) = s.strip_prefix("\\??\\") {
        return r.to_string();
    }
    if let Some(head) = s.get(..11) {
        if head.eq_ignore_ascii_case("\\SystemRoot") && matches!(s.as_bytes().get(11), None | Some(b'\\')) {
            if let Some(root) = env("SystemRoot") {
                return format!("{}{}", root.trim_end_matches('\\'), &s[11..]);
            }
        }
    }
    s.to_string()
}

fn clean(path: &str) -> Option<String> {
    let p = path.trim().trim_end_matches(['\\', '/', ' ']);
    (is_abs(p) && p.contains(['\\', '/']) && p.len() > 2).then(|| p.to_string())
}

fn strip_icon_index(token: &str) -> &str {
    match token.rfind(',') {
        Some(i) if token[i + 1..].trim_start_matches('-').chars().all(|c| c.is_ascii_digit()) && i + 1 < token.len() => &token[..i],
        _ => token,
    }
}

const EXTENSIONS: &[&str] = &[
    ".exe", ".dll", ".bat", ".cmd", ".msi", ".com", ".ico", ".lnk", ".cpl", ".sys", ".scr", ".msc", ".vbs", ".ps1",
];

/// Possible lengths of the unquoted path at the start of `s`. Such a path may contain spaces, so
/// it can end after any file extension followed by nothing, a space, a comma or a quote. The
/// first is the likely one; later ones are also returned (up to the next argument marker) because
/// a folder may be called `Booking.com Helper`. Claiming a longer, nonexistent candidate costs
/// nothing; missing the real folder would.
pub(crate) fn unquoted_lens(s: &str) -> Vec<usize> {
    let low = s.to_ascii_lowercase(); // same byte length as `s`
    let mut ends: Vec<usize> = Vec::new();
    for ext in EXTENSIONS {
        for (i, _) in low.match_indices(ext) {
            let end = i + ext.len();
            if matches!(low.as_bytes().get(end), None | Some(b' ' | b',' | b'"' | b'\t')) {
                ends.push(end);
            }
        }
    }
    ends.sort_unstable();
    ends.dedup();
    let mut out: Vec<usize> = Vec::new();
    for end in ends {
        match out.last() {
            None => out.push(end),
            Some(&prev) => {
                let gap = &low[prev..end];
                // A gap holding an argument, a second path or a quote is not part of a folder name.
                let argument = gap.contains(" /") || gap.contains(" -") || gap.contains([',', '"']);
                if !argument && !gap.contains(":\\") && !gap.contains(":/") {
                    out.push(end);
                }
            }
        }
    }
    if out.is_empty() {
        let cut = low.find([',', '"']).unwrap_or(low.len());
        out.push([" /", " -"].iter().filter_map(|m| low.find(m)).fold(cut, usize::min));
    }
    out
}

/// Every absolute path a command line or icon location names, in order: the program, and any
/// quoted or plain path among its arguments (`rundll32.exe "C:\a\b.dll",Uninstall`). A program
/// with no directory (`MsiExec.exe`) names nothing. Icon indexes, `%VAR%`, `\??\` and
/// `\SystemRoot\` are understood. Trailing backslashes are dropped.
pub fn command_paths(cmd: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let full = fix_prefix(&expand_env(cmd, env), env);
    let mut rest: &str = &full;
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: Option<String>| {
        if let Some(p) = p {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    };
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        let Some(first) = rest.chars().next() else { break };
        if first == '"' {
            let inner = &rest[1..];
            let end = inner.find('"').unwrap_or(inner.len());
            let token = fix_prefix(strip_icon_index(&inner[..end]), env);
            push(clean(&token));
            rest = inner.get(end + 1..).unwrap_or("");
        } else if is_abs(rest) {
            let lens = unquoted_lens(rest);
            for &n in &lens {
                push(clean(strip_icon_index(&rest[..n])));
            }
            rest = &rest[lens.last().copied().unwrap_or(0).max(1)..];
        } else {
            let n = rest.find([' ', '\t', ',', '"']).unwrap_or(rest.len()).max(first.len_utf8());
            // `/D=C:\Program Files\App`: the path after the `=` is an argument worth reading.
            match rest[..n].find('=') {
                Some(eq) if is_abs(&rest[eq + 1..]) => rest = &rest[eq + 1..],
                _ => rest = &rest[n..],
            }
        }
    }
    out
}

/// Folders named by a location value (`InstallLocation`, the `Path` of App Paths, which may hold
/// several separated by `;`).
pub fn location_paths(value: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    expand_env(value, env)
        .split(';')
        .filter_map(|p| clean(p.trim().trim_matches('"')))
        .collect()
}

fn push_paths(parts: &mut ClaimParts, paths: Vec<String>, who: &str, source: &'static str) {
    for path in paths {
        parts.paths.push(PathClaim { path, who: who.to_string(), source });
    }
}

pub fn add_uninstall_entry(parts: &mut ClaimParts, e: &UninstallEntry, env: &dyn Fn(&str) -> Option<String>) {
    let label = if e.display_name.trim().is_empty() { e.key.trim() } else { e.display_name.trim() };
    let who = format!("The installed app {label}");
    push_paths(parts, location_paths(&e.install_location, env), &who, "uninstall locations");
    push_paths(parts, command_paths(&e.display_icon, env), &who, "uninstall icons");
    push_paths(parts, command_paths(&e.uninstall_string, env), &who, "uninstall commands");
    push_paths(parts, command_paths(&e.quiet_uninstall_string, env), &who, "uninstall commands");
    if !e.display_name.trim().is_empty() {
        parts.names.push(NameClaim { name: e.display_name.trim().to_string(), source: "app names" });
    }
    if !e.publisher.trim().is_empty() {
        parts.publishers.push(e.publisher.trim().to_string());
    }
}

/// The names of the installed apps and of their publishers, as the uninstall lists give them.
/// The Leftovers scan (and later the Apps tab) uses them to tell a dead folder from one that
/// belongs to a longer-named app or to a company that still has software installed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstalledNames {
    pub apps: Vec<String>,
    pub publishers: Vec<String>,
}

impl InstalledNames {
    /// Distinct names, in the order first seen (two spellings that differ only in case are one).
    pub fn from_parts(parts: &ClaimParts) -> InstalledNames {
        fn distinct<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
            let mut seen = HashSet::new();
            names.filter(|n| seen.insert(n.to_lowercase())).map(str::to_string).collect()
        }
        InstalledNames {
            apps: distinct(parts.names.iter().map(|n| n.name.as_str())),
            publishers: distinct(parts.publishers.iter().map(String::as_str)),
        }
    }
}

/// Every entry of the three uninstall lists (both views of HKLM and HKCU), as registered. The
/// second value is the (hive, view) of each list that could not be read, as in
/// [`UninstallEntry::hive`] and [`UninstallEntry::view`]. Only reads.
pub fn read_uninstall_entries() -> (Vec<UninstallEntry>, Vec<(String, String)>) {
    real::read_entries()
}

/// One uninstall entry by where it lives. `None` if the key is gone or `hive`/`view` is not one
/// this reader uses.
pub fn read_uninstall_entry(hive: &str, view: &str, key: &str) -> Option<UninstallEntry> {
    real::read_one_entry(hive, view, key)
}

/// Whether a shortcut's target should be taken as still there, by the rule the claims use: it is,
/// unless its readable fixed drive shows it missing in both the 64-bit and x86 Program Files spellings.
pub(crate) fn target_present(target: &str) -> bool {
    real::target_exists(target)
}

/// Where each Start Menu shortcut points, as written in the shortcut (`%VAR%` not yet expanded).
/// `None` for one that can't be opened. Only reads.
pub(crate) fn link_targets(files: &[std::path::PathBuf]) -> Vec<Option<String>> {
    real::link_targets(files)
}

/// The full path of every running program's exe, as Windows reports it (programs of other users
/// or elevated ones may be missing). Only reads.
pub fn running_exes() -> Vec<String> {
    real::read_running_parts()
        .paths
        .into_iter()
        .filter(|c| c.source == "running programs" && c.path.to_ascii_lowercase().ends_with(".exe"))
        .map(|c| c.path)
        .collect()
}

/// Reads the three uninstall lists (the same reader the claims use) for names only. Quick: it
/// opens no shortcut and lists no process. If a list can't be read, what it gave is still returned,
/// and the names can only lower a score.
pub fn read_installed_names() -> InstalledNames {
    InstalledNames::from_parts(&real::read_uninstall_parts())
}

pub fn add_service(parts: &mut ClaimParts, name: &str, image_path: &str, env: &dyn Fn(&str) -> Option<String>) {
    push_paths(parts, command_paths(image_path, env), &format!("The service {name}"), "services");
}

pub fn add_run_entry(parts: &mut ClaimParts, name: &str, command: &str, env: &dyn Fn(&str) -> Option<String>) {
    push_paths(parts, command_paths(command, env), &format!("The startup entry {name}"), "startup entries");
}

/// Records the program name of an exe path, unless it is a generic one.
fn add_program_name(parts: &mut ClaimParts, exe_path: &str, source: &'static str) {
    let file = exe_path.trim().trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or("");
    let Some(stem) = file.len().checked_sub(4).and_then(|i| file.get(..i).filter(|_| file[i..].eq_ignore_ascii_case(".exe"))) else {
        return;
    };
    let lower = stem.to_lowercase();
    if lower.is_empty() || GENERIC_PROGRAM_NAMES.contains(&lower.as_str()) {
        return;
    }
    parts.program_names.push(ProgramName { stem: lower, who: format!("The program {file}"), source });
}

/// Exe names too common to say anything about whose data a folder holds.
const GENERIC_PROGRAM_NAMES: &[&str] = &[
    "uninstall", "uninst", "unins000", "setup", "install", "update", "updater", "launcher", "helper", "service", "app", "main",
    "run", "start",
];

pub fn add_app_path(parts: &mut ClaimParts, exe: &str, default: &str, path: &str, env: &dyn Fn(&str) -> Option<String>) {
    let who = format!("The program {exe}");
    let programs = command_paths(default, env);
    for p in &programs {
        add_program_name(parts, p, "app paths");
    }
    if programs.is_empty() {
        add_program_name(parts, exe, "app paths");
    }
    push_paths(parts, programs, &who, "app paths");
    push_paths(parts, location_paths(path, env), &who, "app paths");
}

pub fn add_shortcut(parts: &mut ClaimParts, name: &str, target: &str, env: &dyn Fn(&str) -> Option<String>) {
    let paths = command_paths(target, env);
    if let Some(p) = paths.first() {
        add_program_name(parts, p, "shortcuts");
    }
    push_paths(parts, paths, &format!("The Start Menu shortcut {name}"), "shortcuts");
}

/// [`add_shortcut`] for a shortcut that works: one whose target is gone (`exists` says so) claims
/// nothing, so the folder of an uninstalled app is not held by its own dead shortcut.
pub fn add_shortcut_if_there(
    parts: &mut ClaimParts,
    name: &str,
    target: &str,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&str) -> bool,
) {
    if exists(target) {
        add_shortcut(parts, name, target, env);
    }
}

/// Program Files spellings that name the same place: the 64-bit and x86 expansions of each.
const PROGRAM_FILES_GROUPS: [&[&str]; 2] = [
    &["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"],
    &["CommonProgramFiles", "CommonProgramW6432", "CommonProgramFiles(x86)"],
];

/// Whether a shortcut's target should be taken as still there. It is, unless it can be shown
/// missing: a target on a drive that isn't a readable fixed one (`readable`) is taken as there,
/// and so is a Program Files target that exists under the other (64-bit or x86) expansion.
pub(crate) fn target_looks_present(
    target: &str,
    env: &dyn Fn(&str) -> Option<String>,
    readable: &dyn Fn(&str) -> bool,
    exists: &dyn Fn(&str) -> bool,
) -> bool {
    let Some(path) = command_paths(target, env).into_iter().next() else { return true };
    if !readable(&path) || exists(&path) {
        return true;
    }
    let low = path.to_lowercase();
    PROGRAM_FILES_GROUPS.iter().any(|group| {
        let roots: Vec<String> = group.iter().filter_map(|v| env(v)).map(|r| r.trim_end_matches('\\').to_string()).collect();
        roots.iter().filter(|r| is_inside(&low, &r.to_lowercase())).any(|from| {
            let rest = &path[from.len()..];
            roots.iter().filter(|to| !to.eq_ignore_ascii_case(from)).any(|to| exists(&format!("{to}{rest}")))
        })
    })
}

pub fn add_process(parts: &mut ClaimParts, image: &str) {
    let who = format!("A running program ({image})");
    let Some(exe) = clean(image) else { return };
    // The program's folder too, so its cache and data next to it count as in use.
    let folder = exe.rfind(['\\', '/']).and_then(|i| clean(&exe[..i]));
    add_program_name(parts, &exe, "running programs");
    push_paths(parts, std::iter::once(exe).chain(folder).collect(), &who, "running programs");
}

// ---- keeping it fresh ----------------------------------------------------------------------------

type Reader = Box<dyn Fn() -> SystemClaims + Send + Sync>;
type Clock = Box<dyn Fn() -> Duration + Send + Sync>;

/// A list that is read on first use and again once it is older than `max_age`.
struct Cached {
    read: Reader,
    max_age: Duration,
    slot: Mutex<Option<(Duration, Arc<SystemClaims>)>>,
}

impl Cached {
    fn new(read: Reader, max_age: Duration) -> Cached {
        Cached { read, max_age, slot: Mutex::new(None) }
    }

    fn current(&self, clock: &dyn Fn() -> Duration) -> Arc<SystemClaims> {
        // A panic elsewhere must not stop folders being checked: the data is only a snapshot.
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        // Taken after any wait for the lock, so it is not older than the read that just finished.
        let now = clock();
        if let Some((at, s)) = slot.as_ref() {
            if now.saturating_sub(*at) < self.max_age {
                return s.clone();
            }
        }
        let fresh = Arc::new((self.read)());
        // Stamped when the read finished, not when it began: a slow read is not born old.
        *slot = Some((clock(), fresh.clone()));
        fresh
    }
}

/// `SystemClaims` that stay recent. Nothing is read until the first question, so a long prepare
/// reads once and not per folder. Running programs are read again after 2 seconds, so a program
/// started after Seek opened is not missed and a commit sees a recent list; installed apps, which
/// cost far more to read and change far less, after 30.
pub struct LiveClaims {
    /// Time since some fixed start; only differences matter.
    clock: Clock,
    installed: Cached,
    running: Cached,
}

impl LiveClaims {
    pub fn new() -> LiveClaims {
        let start = Instant::now();
        LiveClaims::with_readers(
            Box::new(SystemClaims::read_installed),
            INSTALLED_MAX_AGE,
            Box::new(SystemClaims::read_running),
            RUNNING_MAX_AGE,
            Box::new(move || start.elapsed()),
        )
    }

    /// For tests: readers and a clock they control.
    pub fn with_readers(installed: Reader, installed_age: Duration, running: Reader, running_age: Duration, clock: Clock) -> LiveClaims {
        LiveClaims { clock, installed: Cached::new(installed, installed_age), running: Cached::new(running, running_age) }
    }

    /// Forgets both snapshots: the next question reads installed apps and running programs again.
    pub fn refresh(&self) {
        for list in [&self.installed, &self.running] {
            *list.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    #[cfg(test)]
    fn poison_for_test(&self) {
        let _guard = self.running.slot.lock();
        panic!("poisoning the lock on purpose");
    }
}

impl Default for LiveClaims {
    fn default() -> Self {
        LiveClaims::new()
    }
}

impl ClaimCheck for LiveClaims {
    fn is_claimed(&self, folder: &Path) -> Option<String> {
        self.installed
            .current(&*self.clock)
            .is_claimed(folder)
            .or_else(|| self.running.current(&*self.clock).is_claimed(folder))
    }
}

// ---- the machine ---------------------------------------------------------------------------------

mod real {
    use std::path::Path;

    use windows::core::{Interface, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, WIN32_FIND_DATAW};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
    };
    use windows::Win32::System::ProcessStatus::K32EnumProcesses;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_DWORD, REG_EXPAND_SZ, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

    use super::*;

    const WIN: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion";

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn text(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by RegOpenKeyExW and is closed exactly once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// Ok(None) = the key does not exist, which is normal (HKCU has no 32-bit view of most things).
    fn open(hive: HKEY, path: &str, view: REG_SAM_FLAGS) -> Result<Option<Key>, String> {
        let mut h = HKEY::default();
        // SAFETY: the path is NUL-terminated and `h` a valid out-pointer.
        let rc = unsafe { RegOpenKeyExW(hive, PCWSTR(wide(path).as_ptr()), None, KEY_READ | view, &mut h) };
        if rc == ERROR_SUCCESS {
            Ok(Some(Key(h)))
        } else if rc == ERROR_FILE_NOT_FOUND || rc == ERROR_PATH_NOT_FOUND {
            Ok(None)
        } else {
            Err(format!("error {}", rc.0))
        }
    }

    fn subkeys(key: &Key) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            // SAFETY: `buf` and `len` describe a valid buffer.
            let rc = unsafe { RegEnumKeyExW(key.0, i, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc != ERROR_SUCCESS {
                return Err(format!("error {}", rc.0));
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        Ok(out)
    }

    fn value_names(key: &Key) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 16384];
            let mut len = buf.len() as u32;
            // SAFETY: `buf` and `len` describe a valid buffer.
            let rc = unsafe { RegEnumValueW(key.0, i, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc != ERROR_SUCCESS {
                return Err(format!("error {}", rc.0));
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        Ok(out)
    }

    /// Ok(None) = absent. Err = present but not text, or unreadable.
    fn value(key: &Key, name: &str) -> Result<Option<String>, ()> {
        let n = wide(name);
        let mut kind = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        // SAFETY: first call only asks for the size.
        let rc = unsafe { RegQueryValueExW(key.0, PCWSTR(n.as_ptr()), None, Some(&mut kind), None, Some(&mut size)) };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS || (kind != REG_SZ && kind != REG_EXPAND_SZ) {
            return Err(());
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        let mut size2 = (buf.len() * 2) as u32;
        // SAFETY: `buf` has `size2` bytes.
        let rc = unsafe {
            RegQueryValueExW(key.0, PCWSTR(n.as_ptr()), None, Some(&mut kind), Some(buf.as_mut_ptr().cast()), Some(&mut size2))
        };
        if rc != ERROR_SUCCESS {
            return Err(());
        }
        Ok(Some(text(&buf)))
    }

    /// A value that may be absent; a bad one is skipped and counted.
    fn value_or_skip(key: &Key, name: &str, skipped: &mut Skipped) -> String {
        match value(key, name) {
            Ok(v) => v.unwrap_or_default(),
            Err(()) => {
                skipped.registry_values += 1;
                String::new()
            }
        }
    }

    fn hives() -> [(&'static str, HKEY); 2] {
        [("HKLM", HKEY_LOCAL_MACHINE), ("HKCU", HKEY_CURRENT_USER)]
    }

    fn views() -> [(&'static str, REG_SAM_FLAGS); 2] {
        [("64-bit", KEY_WOW64_64KEY), ("32-bit", KEY_WOW64_32KEY)]
    }

    /// Lists the keys under `base` and calls `each` with every one that opens. A root that cannot
    /// be opened or listed is recorded as a failed source (a missing one too, if `required`); a
    /// child that cannot be opened is skipped and counted.
    fn for_each_child(
        parts: &mut ClaimParts,
        (hive, view): (HKEY, REG_SAM_FLAGS),
        base: &str,
        label: &str,
        required: bool,
        mut each: impl FnMut(&mut ClaimParts, &str, &Key),
    ) {
        let root = match open(hive, base, view) {
            Ok(Some(k)) => k,
            Ok(None) => {
                if required {
                    parts.failed.push(format!("{label} (missing)"));
                }
                return;
            }
            Err(e) => {
                parts.failed.push(format!("{label} ({e})"));
                return;
            }
        };
        let names = match subkeys(&root) {
            Ok(n) => n,
            Err(e) => {
                parts.failed.push(format!("{label} ({e})"));
                return;
            }
        };
        for name in names {
            match open(hive, &format!(r"{base}\{name}"), view) {
                Ok(Some(child)) => each(parts, &name, &child),
                _ => parts.skipped.registry_values += 1,
            }
        }
    }

    fn dword(key: &Key, name: &str) -> Option<u64> {
        let n = wide(name);
        let mut kind = REG_VALUE_TYPE(0);
        let mut buf = [0u8; 4];
        let mut size = 4u32;
        // SAFETY: `buf` has `size` bytes.
        let rc = unsafe { RegQueryValueExW(key.0, PCWSTR(n.as_ptr()), None, Some(&mut kind), Some(buf.as_mut_ptr()), Some(&mut size)) };
        if rc == ERROR_SUCCESS && kind == REG_DWORD {
            return Some(u32::from_le_bytes(buf) as u64);
        }
        // Some installers write numbers as text.
        value(key, name).ok().flatten().and_then(|t| t.trim().parse().ok())
    }

    fn read_entry(name: &str, k: &Key, hive: &str, view: &str, skipped: &mut Skipped) -> UninstallEntry {
        let mut get = |v: &str| value_or_skip(k, v, skipped);
        UninstallEntry {
            hive: hive.to_string(),
            view: view.to_string(),
            key: name.to_string(),
            display_name: get("DisplayName"),
            publisher: get("Publisher"),
            display_version: get("DisplayVersion"),
            install_date: get("InstallDate"),
            parent_key_name: get("ParentKeyName"),
            install_location: get("InstallLocation"),
            display_icon: get("DisplayIcon"),
            uninstall_string: get("UninstallString"),
            quiet_uninstall_string: get("QuietUninstallString"),
            estimated_size_kb: dword(k, "EstimatedSize"),
            system_component: dword(k, "SystemComponent") == Some(1),
        }
    }

    fn view_tag(vn: &str) -> &'static str {
        if vn == "64-bit" {
            "64"
        } else {
            "32"
        }
    }

    pub(super) fn drive_type(letter: char) -> u32 {
        // SAFETY: a NUL-terminated root path.
        unsafe { GetDriveTypeW(PCWSTR(wide(&format!("{}:\\", letter.to_ascii_uppercase())).as_ptr())) }
    }

    /// Calls `each` for every entry. Returns the (hive, view) of every list that could not be read.
    fn each_entry(parts: &mut ClaimParts, mut each: impl FnMut(&mut ClaimParts, UninstallEntry)) -> Vec<(String, String)> {
        let mut failed = Vec::new();
        for (hn, hive) in hives() {
            for (vn, view) in views() {
                // The 64-bit machine list exists on every Windows install.
                let required = hn == "HKLM" && vn == "64-bit";
                let label = format!("the {hn} {vn} uninstall list");
                let before = parts.failed.len();
                for_each_child(parts, (hive, view), &format!(r"{WIN}\Uninstall"), &label, required, |parts, name, k| {
                    let e = read_entry(name, k, hn, view_tag(vn), &mut parts.skipped);
                    each(parts, e);
                });
                if parts.failed.len() > before {
                    failed.push((hn.to_string(), view_tag(vn).to_string()));
                }
            }
        }
        failed
    }

    fn uninstall(parts: &mut ClaimParts) {
        let _ = each_entry(parts, |parts, e| add_uninstall_entry(parts, &e, &env));
    }

    pub(super) fn read_entries() -> (Vec<UninstallEntry>, Vec<(String, String)>) {
        let mut parts = ClaimParts::default();
        let mut out = Vec::new();
        let failed = each_entry(&mut parts, |_, e| out.push(e));
        (out, failed)
    }

    pub(super) fn read_one_entry(hn: &str, vn: &str, key: &str) -> Option<UninstallEntry> {
        let hive = hives().into_iter().find(|(n, _)| *n == hn)?.1;
        let view = views().into_iter().find(|(n, _)| view_tag(n) == vn)?.1;
        let k = open(hive, &format!(r"{WIN}\Uninstall\{key}"), view).ok().flatten()?;
        let mut skipped = Skipped::default();
        Some(read_entry(key, &k, hn, vn, &mut skipped))
    }

    fn run_keys(parts: &mut ClaimParts) {
        for (hn, hive) in hives() {
            for (vn, view) in views() {
                for sub in ["Run", "RunOnce"] {
                    let key = match open(hive, &format!(r"{WIN}\{sub}"), view) {
                        Ok(Some(k)) => k,
                        Ok(None) => continue,
                        Err(e) => {
                            parts.failed.push(format!("the {hn} {vn} {sub} list ({e})"));
                            continue;
                        }
                    };
                    let names = match value_names(&key) {
                        Ok(n) => n,
                        Err(e) => {
                            parts.failed.push(format!("the {hn} {vn} {sub} list ({e})"));
                            continue;
                        }
                    };
                    for n in names {
                        let cmd = value_or_skip(&key, &n, &mut parts.skipped);
                        add_run_entry(parts, &n, &cmd, &env);
                    }
                }
            }
        }
    }

    fn app_paths(parts: &mut ClaimParts) {
        for (hn, hive) in hives() {
            for (vn, view) in views() {
                let label = format!("the {hn} {vn} App Paths list");
                for_each_child(parts, (hive, view), &format!(r"{WIN}\App Paths"), &label, false, |parts, name, k| {
                    let default = value_or_skip(k, "", &mut parts.skipped);
                    let dirs = value_or_skip(k, "Path", &mut parts.skipped);
                    add_app_path(parts, name, &default, &dirs, &env);
                });
            }
        }
    }

    /// Services and drivers keep their image path in the registry; the service manager reads the
    /// same values.
    fn services(parts: &mut ClaimParts) {
        let hive = (HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY);
        for_each_child(parts, hive, r"SYSTEM\CurrentControlSet\Services", "the list of services", true, |parts, name, k| {
            let image = value_or_skip(k, "ImagePath", &mut parts.skipped);
            add_service(parts, name, &image, &env);
        });
    }

    fn processes(parts: &mut ClaimParts) {
        let mut pids = vec![0u32; 4096];
        let count = loop {
            let mut needed = 0u32;
            let bytes = (pids.len() * 4) as u32;
            // SAFETY: `pids` is `bytes` long.
            if !unsafe { K32EnumProcesses(pids.as_mut_ptr(), bytes, &mut needed) }.as_bool() {
                parts.failed.push("the list of running programs".into());
                return;
            }
            if needed < bytes {
                break needed as usize / 4;
            }
            pids.resize(pids.len() * 2, 0);
        };
        let mut seen = HashSet::new();
        for &pid in &pids[..count] {
            if pid == 0 {
                continue;
            }
            // SAFETY: a limited query handle, closed below.
            let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
                parts.skipped.processes += 1;
                continue;
            };
            let mut buf = vec![0u16; 32768];
            let mut len = buf.len() as u32;
            // SAFETY: `buf` and `len` describe a valid buffer.
            let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.is_ok();
            // SAFETY: `h` came from OpenProcess.
            unsafe {
                let _ = CloseHandle(h);
            }
            if !ok {
                parts.skipped.processes += 1;
                continue;
            }
            let image = String::from_utf16_lossy(&buf[..len as usize]);
            if seen.insert(image.to_lowercase()) {
                add_process(parts, &image);
            }
        }
    }

    fn shortcut_files(dir: &Path, depth: u32, out: &mut Vec<std::path::PathBuf>, skipped: &mut Skipped) -> Result<(), String> {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if depth > 0 => {
                skipped.shortcuts += 1;
                return Ok(());
            }
            Err(e) => return Err(e.to_string()),
        };
        for entry in rd {
            let Ok(entry) = entry else {
                skipped.shortcuts += 1;
                continue;
            };
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                skipped.shortcuts += 1;
                continue;
            };
            if ft.is_dir() && depth < 8 {
                shortcut_files(&path, depth + 1, out, skipped)?;
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                out.push(path);
            }
        }
        Ok(())
    }

    /// # Safety
    /// COM must be initialised on this thread.
    unsafe fn link_target(path: &Path) -> Option<String> {
        use std::os::windows::ffi::OsStrExt;
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        let w: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        file.Load(PCWSTR(w.as_ptr()), STGM_READ).ok()?;
        let mut buf = [0u16; 1024];
        let mut data = WIN32_FIND_DATAW::default();
        link.GetPath(&mut buf, &mut data, SLGP_RAWPATH.0 as u32).ok()?;
        let t = text(&buf);
        (!t.is_empty()).then_some(t)
    }

    /// Whether a shortcut's target is still there. A drive that may stall is not asked: its
    /// shortcuts are taken as working.
    pub(super) fn target_exists(target: &str) -> bool {
        target_looks_present(target, &env, &drive_readable, &|p| Path::new(p).exists())
    }

    /// A fixed local drive whose volume can be read. A locked (BitLocker) or failing one can't be
    /// asked about its files.
    fn drive_readable(path: &str) -> bool {
        if !is_fixed_local(path) {
            return false;
        }
        let root: String = path.chars().take(2).collect::<String>() + "\\";
        // SAFETY: a NUL-terminated root path; every optional out-parameter is left out.
        unsafe { GetVolumeInformationW(PCWSTR(wide(&root).as_ptr()), None, None, None, None, None) }.is_ok()
    }

    /// The raw target of each shortcut (`None` for one that can't be opened), in order. Opens
    /// them on a thread of its own: IShellLink wants its own apartment.
    pub(super) fn link_targets(files: &[std::path::PathBuf]) -> Vec<Option<String>> {
        let files = files.to_vec();
        let count = files.len();
        std::thread::spawn(move || {
            // SAFETY: balanced with CoUninitialize below.
            if files.is_empty() || unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_err() {
                return vec![None; files.len()];
            }
            // SAFETY: COM is initialised on this thread.
            let found = files.iter().map(|f| unsafe { link_target(f) }).collect();
            // SAFETY: balanced with the CoInitializeEx above.
            unsafe { CoUninitialize() };
            found
        })
        .join()
        .unwrap_or_else(|_| vec![None; count])
    }

    fn shortcuts(parts: &mut ClaimParts) {
        let mut files = Vec::new();
        for var in ["ProgramData", "APPDATA"] {
            let Some(base) = env(var) else { continue };
            let dir = Path::new(&base).join(r"Microsoft\Windows\Start Menu\Programs");
            if let Err(e) = shortcut_files(&dir, 0, &mut files, &mut parts.skipped) {
                parts.failed.push(format!("the Start Menu shortcuts ({e})"));
                return;
            }
        }
        // IShellLink wants a single-threaded apartment; a fresh thread owns its own.
        let done = std::thread::spawn(move || {
            // SAFETY: balanced with CoUninitialize below.
            if unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_err() {
                return None;
            }
            let found: Vec<(String, Option<String>)> = files
                .iter()
                .map(|f| {
                    let name = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    // SAFETY: COM is initialised on this thread.
                    (name, unsafe { link_target(f) })
                })
                .collect();
            // SAFETY: balanced with the CoInitializeEx above.
            unsafe { CoUninitialize() };
            Some(found)
        })
        .join();
        match done {
            Ok(Some(found)) => {
                // Links exist and not one opened: the reader is broken, not the links.
                let opened: Vec<bool> = found.iter().map(|(_, t)| t.is_some()).collect();
                if every_link_failed(&opened) {
                    parts.failed.push("the Start Menu shortcuts (none could be opened)".into());
                    return;
                }
                for (name, target) in found {
                    match target {
                        Some(t) => add_shortcut_if_there(parts, &name, &t, &env, &target_exists),
                        None => parts.skipped.shortcuts += 1,
                    }
                }
            }
            _ => parts.failed.push("the Start Menu shortcuts (COM could not start)".into()),
        }
    }

    fn broad_roots() -> Vec<String> {
        let mut roots = Vec::new();
        // SAFETY: no arguments.
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) != 0 {
                roots.push(format!("{}:\\", (b'A' + i as u8) as char));
            }
        }
        for var in ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData", "USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
            roots.extend(env(var));
        }
        if let Some(profile) = env("USERPROFILE") {
            let p = Path::new(&profile);
            roots.extend(p.parent().map(|u| u.to_string_lossy().into_owned()));
            roots.push(p.join(r"AppData\LocalLow").to_string_lossy().into_owned());
            roots.push(p.join("AppData").to_string_lossy().into_owned());
        }
        roots
    }

    /// The uninstall lists alone.
    pub(super) fn read_uninstall_parts() -> ClaimParts {
        let mut parts = ClaimParts { broad_roots: broad_roots(), ..ClaimParts::default() };
        uninstall(&mut parts);
        parts
    }

    /// Installed apps, services, startup entries and shortcuts: slow to read (shortcuts alone
    /// open every `.lnk`) and slow to change.
    pub(super) fn read_installed_parts() -> ClaimParts {
        let mut parts = ClaimParts { broad_roots: broad_roots(), ..ClaimParts::default() };
        uninstall(&mut parts);
        run_keys(&mut parts);
        app_paths(&mut parts);
        services(&mut parts);
        shortcuts(&mut parts);
        expand_short_names(&mut parts);
        parts
    }

    /// Running programs: quick, and the one thing that changes from second to second.
    pub(super) fn read_running_parts() -> ClaimParts {
        let mut parts = ClaimParts { broad_roots: broad_roots(), ..ClaimParts::default() };
        processes(&mut parts);
        expand_short_names(&mut parts);
        parts
    }

    /// Writes `PROGRA~1` spellings out in full, but only on fixed local drives: asking a
    /// disconnected network or removable drive can take many seconds, with every caller waiting.
    fn expand_short_names(parts: &mut ClaimParts) {
        let mut kinds: std::collections::HashMap<char, u32> = std::collections::HashMap::new();
        let mut kind_of = |letter: char| -> u32 {
            *kinds.entry(letter.to_ascii_uppercase()).or_insert_with(|| {
                // SAFETY: a NUL-terminated root path.
                unsafe { GetDriveTypeW(PCWSTR(wide(&format!("{}:\\", letter.to_ascii_uppercase())).as_ptr())) }
            })
        };
        for c in &mut parts.paths {
            if should_expand_short_names(&c.path, &mut kind_of) {
                c.path = canon(Path::new(&c.path));
            }
        }
    }
}

#[cfg(test)]
mod tests;
