//! The list of installed apps behind the Apps tab: what the three `Uninstall` registry lists
//! say, tidied into rows, with each app's folder and icon. Only reads. Running an uninstaller is
//! a later task; it re-reads an entry by its [`AppRow::id`] and never trusts a command the window
//! sends.
//!
//! The registry reader is the one the claims use (`claims::read_uninstall_entries`). Everything
//! here that decides something (hiding, folders, notes, dates, ids, command parsing) is pure and
//! tested without a registry.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::claims::{self, command_paths, expand_env, is_abs, location_paths, unquoted_lens};
use crate::model::{AppRow, FolderSource};
use crate::paths::{canon_lexical, is_inside};
use crate::protect::Places;

pub use crate::claims::UninstallEntry as RawEntry;

/// An uninstall command, split into what can be run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UninstallCommand {
    /// A program and its arguments as one string.
    Exe { path: String, args: String },
    /// `MsiExec /X{GUID}` or `/I{GUID}`. Both mean "uninstall this product".
    Msi { guid: String },
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn is_msiexec(path: &str) -> bool {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    file.eq_ignore_ascii_case("msiexec.exe") || file.eq_ignore_ascii_case("msiexec")
}

/// `{8-4-4-4-12}` in hex, braces included, and nothing else.
fn is_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 38
        && b[0] == b'{'
        && b[37] == b'}'
        && b[1..37].iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The `{GUID}` after `/X`, `/I` or `/uninstall` in the arguments of `msiexec`.
fn msi_guid(args: &str) -> Option<String> {
    let low = args.to_ascii_lowercase();
    for flag in ["/x", "/i", "-x", "-i", "/uninstall", "/package"] {
        for (at, _) in low.match_indices(flag) {
            let rest = args[at + flag.len()..].trim_start_matches([' ', '=']);
            if let Some(guid) = rest.get(..38).filter(|g| is_guid(g)) {
                return Some(guid.to_string());
            }
        }
    }
    None
}

/// What the parser needs from the machine, so tests can answer for it.
pub struct Ctx<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Writes 8.3 names out in full (`PROGRA~1`). Fixed local drives only, in the real one.
    pub expand: &'a dyn Fn(&str) -> String,
    pub dir_exists: &'a dyn Fn(&str) -> bool,
    pub file_exists: &'a dyn Fn(&str) -> bool,
}

/// Runs `f` with this PC's answers. A folder or file on a drive that is not a fixed local one is
/// never asked for: a folder is taken as there, a file as not there.
pub fn with_real_ctx<R>(f: impl FnOnce(&Ctx) -> R) -> R {
    f(&Ctx {
        env: &process_env,
        expand: &claims::expand_short_spelling,
        dir_exists: &|p| !claims::is_fixed_local(p) || Path::new(p).is_dir(),
        file_exists: &|p| claims::is_fixed_local(p) && Path::new(p).is_file(),
    })
}

const BARE_SYSTEM_PROGRAMS: [&str; 2] = ["rundll32", "msiexec"];

/// An absolute path to an `.exe`: as given, or for a bare `rundll32` / `msiexec` the one in the
/// Windows directory. Any other relative or bare name is something Seek can't pin down.
fn resolve_program(path: &str, ctx: &Ctx) -> Option<String> {
    let path = path.trim();
    if is_abs(path) {
        return path.to_ascii_lowercase().ends_with(".exe").then(|| path.to_string());
    }
    if path.contains(['\\', '/', ':']) {
        return None;
    }
    let low = path.to_ascii_lowercase();
    let stem = low.strip_suffix(".exe").unwrap_or(&low);
    if !BARE_SYSTEM_PROGRAMS.contains(&stem) {
        return None;
    }
    let root = (ctx.env)("SystemRoot").or_else(|| (ctx.env)("windir"))?;
    Some(format!(r"{}\System32\{stem}.exe", root.trim_end_matches('\\')))
}

/// Splits an `UninstallString` into something that can be run, or `None` when Seek can't tell
/// what to run (it will not guess). A quoted program ends at the closing quote (unclosed is
/// `None`). An unquoted absolute one may contain spaces: of the candidates ending in `.exe`
/// (the claims' own rule for where a path can end) the shortest that exists wins, else the
/// shortest. `%VAR%` is expanded. `MsiExec` with a product code becomes [`UninstallCommand::Msi`].
pub fn parse_uninstall_with(cmd: &str, ctx: &Ctx) -> Option<UninstallCommand> {
    let full = expand_env(cmd.trim(), ctx.env);
    let full = full.trim();
    if full.is_empty() {
        return None;
    }
    let (path, args) = if let Some(inner) = full.strip_prefix('"') {
        let end = inner.find('"')?;
        (inner[..end].to_string(), inner[end + 1..].trim().to_string())
    } else if is_abs(full) {
        let ends: Vec<usize> = unquoted_lens(full)
            .into_iter()
            .filter(|&n| full.is_char_boundary(n) && full[..n].to_ascii_lowercase().ends_with(".exe"))
            .collect();
        let n = ends.iter().copied().find(|&n| (ctx.file_exists)(full[..n].trim())).or_else(|| ends.first().copied())?;
        (full[..n].trim().to_string(), full[n..].trim().to_string())
    } else {
        let (first, rest) = full.split_once(char::is_whitespace).unwrap_or((full, ""));
        (first.to_string(), rest.trim().to_string())
    };
    let path = resolve_program(&path, ctx)?;
    if is_msiexec(&path) {
        if let Some(guid) = msi_guid(&args) {
            return Some(UninstallCommand::Msi { guid });
        }
    }
    Some(UninstallCommand::Exe { path, args })
}

/// [`parse_uninstall_with`] on this PC.
pub fn parse_uninstall(cmd: &str) -> Option<UninstallCommand> {
    with_real_ctx(|ctx| parse_uninstall_with(cmd, ctx))
}

// ---- the id ---------------------------------------------------------------------------------------

const HIVES: [&str; 2] = ["HKLM", "HKCU"];
const VIEWS: [&str; 2] = ["64", "32"];

/// `HKLM64\{GUID}`: hive, view and key name. The same between reads.
pub fn app_id(e: &RawEntry) -> String {
    format!("{}{}\\{}", e.hive, e.view, e.key)
}

/// Hive, view and key name of an id made by [`app_id`]. Anything else (a key name holding a
/// backslash would reach into another key) gives `None`.
pub fn parse_app_id(id: &str) -> Option<(&str, &str, &str)> {
    let (head, key) = id.split_once('\\')?;
    if key.is_empty() || key.contains('\\') || key.chars().any(char::is_control) || head.len() != 6 || !head.is_char_boundary(4) {
        return None;
    }
    let (hive, view) = head.split_at(4);
    (HIVES.contains(&hive) && VIEWS.contains(&view)).then_some((hive, view, key))
}

/// The registry entry behind an id, read now. `None` if the id is malformed, the key is gone, or
/// the entry is one the list does not show (Seek, a system component, a child entry, an app with
/// no normal uninstaller): an id can't reach what the list hides.
pub fn entry_by_id(id: &str) -> Option<RawEntry> {
    let (hive, view, key) = parse_app_id(id)?;
    claims::read_uninstall_entry(hive, view, key).filter(is_listed)
}

// ---- which entries are listed -----------------------------------------------------------------

/// Seek's own installer entry (Tauri's NSIS installer writes the product name and the
/// `bundle.publisher` of `tauri.conf.json`), per-machine or per-user.
const SEEK_NAME: &str = "Seek";
const SEEK_PUBLISHER: &str = "Seek";

fn is_seek(e: &RawEntry) -> bool {
    e.display_name.trim().eq_ignore_ascii_case(SEEK_NAME) && e.publisher.trim().eq_ignore_ascii_case(SEEK_PUBLISHER)
}

/// Only the normal uninstaller counts: Seek runs that one, never the quiet one.
fn has_command(e: &RawEntry) -> bool {
    !e.uninstall_string.trim().is_empty()
}

fn is_listed(e: &RawEntry) -> bool {
    !e.display_name.trim().is_empty()
        && !e.system_component
        && e.parent_key_name.trim().is_empty()
        && has_command(e)
        && !is_seek(e)
}

/// Name patterns of runtimes and drivers that other apps depend on. `*` matches anything;
/// matching ignores case. One table, with its own test.
pub const RUNTIME_PATTERNS: &[&str] = &[
    "microsoft visual c++ *redistributable*",
    "microsoft .net *",
    "*.net runtime*",
    "*.net sdk*",
    "*.net framework*",
    "*.net core*",
    "*windows desktop runtime*",
    "*directx*",
    "visual studio tools for*",
    "*driver*",
];

pub const RUNTIME_NOTE: &str = "Other apps may need this.";
pub const CANT_START_NOTE: &str = "Seek can't start this app's uninstaller.";

fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = text.strip_prefix(first) else { return false };
    let mut tail: Vec<&str> = parts.collect();
    let Some(last) = tail.pop() else { return rest.is_empty() };
    for mid in tail {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

pub fn note_for(name: &str) -> Option<&'static str> {
    let low = name.to_lowercase();
    RUNTIME_PATTERNS.iter().any(|p| glob(p, &low)).then_some(RUNTIME_NOTE)
}

// ---- the folder -----------------------------------------------------------------------------------

/// Whether `canon` (lowercase, lexical) has `appdata\local\temp` as consecutive whole segments.
fn in_temp(canon: &str) -> bool {
    let temp = canon_lexical(&std::env::temp_dir());
    let segs: Vec<&str> = canon.split('\\').collect();
    is_inside(canon, &temp) || segs.windows(3).any(|w| w == ["appdata", "local", "temp"])
}

/// A drive root (`c:`) or a UNC share root (`\\server\share`).
fn is_root(canon: &str) -> bool {
    canon.len() <= 2 || (canon.starts_with("\\\\") && canon[2..].matches('\\').count() <= 1)
}

/// A folder an app may be said to live in, in its expanded spelling (so `C:\PROGRA~1\x` is judged
/// as `C:\Program Files\x`). Rejected: one that is not there, Windows (with `Installer`), temp
/// folders, the package cache, a drive or share root, a Program Files root, the user profile or a
/// folder directly in it, any other place Seek knows and, for a guess, one that is or holds the
/// folder of another listed app (`others`, in lowercase lexical form). That last rule holds for an
/// `InstallLocation` too: a vendor folder that holds other apps is not one app's folder.
fn acceptable_folder(places: &Places, ctx: &Ctx, raw: &str, guessed: bool, others: &[String]) -> Option<PathBuf> {
    let expanded = (ctx.expand)(raw.trim()).replace('/', "\\");
    let spelled = if expanded.len() > 3 { expanded.trim_end_matches('\\') } else { expanded.as_str() };
    let c = canon_lexical(Path::new(spelled));
    let bad = is_root(&c)
        || places.in_windows(&c)
        || places.is_or_contains_place(&c)
        || places.in_package_cache(&c)
        || places.is_program_files_root(&c)
        || places.is_profile_or_child(&c)
        || in_temp(&c)
        || (guessed && places.in_common_files(&c))
        || others.iter().any(|o| is_inside(o, &c))
        || !(ctx.dir_exists)(spelled);
    (!bad).then(|| PathBuf::from(spelled))
}

fn parent_folder(file: &str) -> Option<String> {
    let parent = Path::new(file).parent()?.to_str()?;
    (!parent.is_empty()).then(|| parent.to_string())
}

/// An app's folder and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub source: FolderSource,
}

/// The folder an app is installed in: `InstallLocation`, else the folder of its `DisplayIcon`,
/// else the folder of its uninstaller. A candidate that fails [`acceptable_folder`] (including a
/// stale `InstallLocation` that no longer exists) falls through to the next. `others` are the
/// canonical folders of the other listed apps. The path is returned in expanded spelling: 8.3 names
/// written out in the disk's case on fixed drives, otherwise as registered, with `\` separators
/// and no trailing `\`. `None` when nothing is left; the app is still listed.
pub fn find_folder(e: &RawEntry, places: &Places, ctx: &Ctx, others: &[String]) -> Option<Found> {
    if let Some(path) = location_paths(&e.install_location, ctx.env).iter().find_map(|p| acceptable_folder(places, ctx, p, false, others)) {
        return Some(Found { path, source: FolderSource::InstallLocation });
    }
    if let Some(path) = command_paths(&e.display_icon, ctx.env)
        .first()
        .and_then(|icon| parent_folder(icon))
        .and_then(|p| acceptable_folder(places, ctx, &p, true, others))
    {
        return Some(Found { path, source: FolderSource::Icon });
    }
    match parse_uninstall_with(&e.uninstall_string, ctx) {
        Some(UninstallCommand::Exe { path, .. }) => parent_folder(&path)
            .and_then(|p| acceptable_folder(places, ctx, &p, true, others))
            .map(|path| Found { path, source: FolderSource::Uninstaller }),
        _ => None,
    }
}

/// [`find_folder`] on this PC, knowing nothing of other apps.
pub fn install_folder(e: &RawEntry, places: &Places) -> Option<PathBuf> {
    with_real_ctx(|ctx| find_folder(e, places, ctx, &[])).map(|f| f.path)
}

/// True if a running program's exe is inside `folder`.
pub fn is_running(folder: Option<&Path>, running_exes: &[String]) -> bool {
    let Some(folder) = folder else { return false };
    let folder = canon_lexical(folder);
    running_exes.iter().any(|exe| is_inside(&canon_lexical(Path::new(exe)), &folder))
}

// ---- dates and sizes ----------------------------------------------------------------------------------

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

const DAY_MS: i64 = 86_400_000;

/// `YYYYMMDD` as epoch milliseconds at local midnight. `local_to_utc` turns "milliseconds of
/// local wall-clock time, counted as if it were UTC" into real UTC milliseconds. Anything that
/// is not a real date gives `None`.
pub fn install_date_ms(value: &str, local_to_utc: &dyn Fn(i64) -> Option<i64>) -> Option<i64> {
    let v = value.trim();
    if v.len() != 8 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (v[..4].parse().ok()?, v[4..6].parse().ok()?, v[6..].parse().ok()?);
    if !(1980..=2200).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    local_to_utc(days_from_civil(y, m, d) * DAY_MS)
}

fn local_to_utc_ms(naive_ms: i64) -> Option<i64> {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::TzSpecificLocalTimeToSystemTime;
    let days = naive_ms.div_euclid(DAY_MS);
    let ms_of_day = naive_ms.rem_euclid(DAY_MS);
    let (y, m, d) = civil_from_days(days);
    let local = SYSTEMTIME {
        wYear: y as u16,
        wMonth: m as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: (ms_of_day / 3_600_000) as u16,
        wMinute: (ms_of_day / 60_000 % 60) as u16,
        wSecond: (ms_of_day / 1000 % 60) as u16,
        wMilliseconds: (ms_of_day % 1000) as u16,
    };
    let mut utc = SYSTEMTIME::default();
    // SAFETY: both pointers are valid SYSTEMTIMEs; a null zone means the current one.
    unsafe { TzSpecificLocalTimeToSystemTime(None, &local, &mut utc) }.ok()?;
    let day = days_from_civil(i64::from(utc.wYear), i64::from(utc.wMonth), i64::from(utc.wDay));
    Some(
        day * DAY_MS
            + i64::from(utc.wHour) * 3_600_000
            + i64::from(utc.wMinute) * 60_000
            + i64::from(utc.wSecond) * 1000
            + i64::from(utc.wMilliseconds),
    )
}

/// `EstimatedSize` is in KB. Missing is 0.
fn size_bytes(kb: Option<u64>) -> u64 {
    kb.unwrap_or(0).saturating_mul(1024)
}

// ---- the rows -------------------------------------------------------------------------------------------

/// The rows for these entries: hidden ones dropped, the same app in both views once (the 64-bit
/// view wins), sorted by name. `icon` stays empty. Folders are found twice: once alone, then
/// again knowing every other app's folder, so a guess that is or holds another app's folder
/// (`C:\Program Files\Adobe`) is dropped. A row whose uninstall command can't be understood stays,
/// with a note saying so. Pure but for `ctx`.
pub fn rows(
    entries: Vec<RawEntry>,
    places: &Places,
    running_exes: &[String],
    ctx: &Ctx,
    local_to_utc: &dyn Fn(i64) -> Option<i64>,
) -> Vec<AppRow> {
    let mut entries: Vec<RawEntry> = entries.into_iter().filter(is_listed).collect();
    // "64" sorts after "32", so reversing puts the 64-bit view first; the sort is stable.
    entries.sort_by(|a, b| b.view.cmp(&a.view));
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert((e.hive.clone(), e.key.to_lowercase(), e.display_name.trim().to_lowercase())));

    let alone: Vec<Option<String>> =
        entries.iter().map(|e| find_folder(e, places, ctx, &[]).map(|f| canon_lexical(&f.path))).collect();
    let mut out: Vec<AppRow> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let others: Vec<String> = alone.iter().enumerate().filter(|(j, _)| *j != i).filter_map(|(_, f)| f.clone()).collect();
        let found = find_folder(e, places, ctx, &others);
        let name = e.display_name.trim().to_string();
        let mut notes: Vec<&str> = note_for(&name).into_iter().collect();
        if parse_uninstall_with(&e.uninstall_string, ctx).is_none() {
            notes.push(CANT_START_NOTE);
        }
        out.push(AppRow {
            id: app_id(e),
            note: (!notes.is_empty()).then(|| notes.join(" ")),
            name,
            publisher: e.publisher.trim().to_string(),
            version: e.display_version.trim().to_string(),
            size: size_bytes(e.estimated_size_kb),
            install_date: install_date_ms(&e.install_date, local_to_utc),
            icon: None,
            running: is_running(found.as_ref().map(|f| f.path.as_path()), running_exes),
            folder_guessed: found.as_ref().is_some_and(|f| f.source != FolderSource::InstallLocation),
            folder_source: found.as_ref().map(|f| f.source),
            folder: found.map(|f| f.path.to_string_lossy().into_owned()),
        });
    }
    out.sort_by_key(|r| r.name.to_lowercase());
    out
}

/// The rows for these entries on this PC: no icons, and nothing counted as running (the caller
/// has no process list).
pub fn from_entries(entries: Vec<RawEntry>) -> Vec<AppRow> {
    match Places::from_system() {
        Ok(places) => with_real_ctx(|ctx| rows(entries, &places, &[], ctx, &local_to_utc_ms)),
        Err(_) => Vec::new(),
    }
}

/// The installed apps on this PC. The registry is read once. With `with_icons`, icons come from a
/// cache (path, index and modified time), so a second call extracts none again; only files on
/// fixed local drives are touched.
fn list_with(with_icons: bool) -> Result<Vec<AppRow>, String> {
    let places = Places::from_system()?;
    let (entries, failed) = claims::read_uninstall_entries();
    if failed.iter().any(|(hive, view)| hive == "HKLM" && view == "64") {
        let lists: Vec<String> = failed.iter().map(|(h, v)| format!("{h} {v}-bit")).collect();
        return Err(format!("Seek couldn't read the list of installed apps ({}).", lists.join(", ")));
    }
    let running = claims::running_exes();
    with_real_ctx(|ctx| {
        let sources: HashMap<String, (String, i32)> = if with_icons {
            entries.iter().filter(|e| is_listed(e)).filter_map(|e| icon_source(e, ctx).map(|s| (app_id(e), s))).collect()
        } else {
            HashMap::new()
        };
        let mut out = rows(entries, &places, &running, ctx, &local_to_utc_ms);
        for r in &mut out {
            r.icon = sources
                .get(&r.id)
                .filter(|(path, _)| claims::is_fixed_local(path))
                .and_then(|(path, idx)| icon_cache().get(path, *idx, icons_win::extract_png));
        }
        Ok(out)
    })
}

/// The installed apps on this PC, with icons.
pub fn try_list() -> Result<Vec<AppRow>, String> {
    list_with(true)
}

/// The installed apps on this PC, without icons.
pub fn try_list_plain() -> Result<Vec<AppRow>, String> {
    list_with(false)
}

/// [`try_list`], or nothing if the list can't be read.
pub fn list() -> Vec<AppRow> {
    try_list().unwrap_or_default()
}

/// Lets go of every extracted icon (the window has been hidden long enough for Cleanup to drop
/// its results).
pub fn clear_icon_cache() {
    icon_cache().clear();
}

// ---- icons ----------------------------------------------------------------------------------------------

/// `C:\x\app.exe,2` or `"C:\x\app.exe",0` as a path and an icon index.
pub fn split_icon(value: &str) -> Option<(String, i32)> {
    let expanded = expand_env(value.trim(), &process_env);
    let v = expanded.trim();
    let (path, index) = match v.strip_prefix('"') {
        Some(inner) => {
            let end = inner.find('"')?;
            (&inner[..end], inner[end + 1..].trim_start_matches(',').trim())
        }
        None => match v.rfind(',') {
            Some(i) if v[i + 1..].trim().trim_start_matches('-').chars().all(|c| c.is_ascii_digit()) => (&v[..i], v[i + 1..].trim()),
            _ => (v, ""),
        },
    };
    let path = path.trim();
    is_abs(path).then(|| (path.to_string(), index.parse().unwrap_or(0)))
}

/// Where an app's icon comes from: its `DisplayIcon`, else its uninstaller's own icon.
fn icon_source(e: &RawEntry, ctx: &Ctx) -> Option<(String, i32)> {
    split_icon(&e.display_icon).or_else(|| match parse_uninstall_with(&e.uninstall_string, ctx) {
        Some(UninstallCommand::Exe { path, .. }) => Some((path, 0)),
        _ => None,
    })
}

/// Extracted icons by file, icon index and modified time. A failure is remembered too.
#[derive(Default)]
pub struct IconCache {
    map: Mutex<HashMap<(String, i32, u64), Option<String>>>,
}

/// The most icons kept. A PC has a few hundred apps; this only stops runaway growth.
const ICON_CACHE_MAX: usize = 4096;

fn modified_ms(path: &str) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

impl IconCache {
    pub fn clear(&self) {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// The icon for `path`, extracted by `extract` only if this file at this modified time has
    /// not been asked for.
    pub fn get(&self, path: &str, index: i32, extract: impl FnOnce(&str, i32) -> Option<String>) -> Option<String> {
        let key = (path.to_lowercase(), index, modified_ms(path));
        if let Some(hit) = self.map.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return hit.clone();
        }
        let icon = extract(path, index);
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= ICON_CACHE_MAX {
            map.clear();
        }
        map.insert(key, icon.clone());
        icon
    }
}

fn icon_cache() -> &'static IconCache {
    static CACHE: std::sync::OnceLock<IconCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(IconCache::default)
}

pub(crate) mod icons_win {
    use base64::Engine as _;
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
        HGDIOBJ,
    };
    use windows::Win32::UI::Shell::ExtractIconExW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    /// Longest data URL sent to the window. A 32x32 icon is a few KB.
    const MAX_DATA_URL: usize = 32 * 1024;
    const MAX_SIDE: i32 = 256;

    struct Icon(HICON);
    impl Drop for Icon {
        fn drop(&mut self) {
            if !self.0 .0.is_null() {
                // SAFETY: the handle came from ExtractIconExW and is destroyed exactly once.
                unsafe {
                    let _ = DestroyIcon(self.0);
                }
            }
        }
    }

    struct Bitmap(HBITMAP);
    impl Drop for Bitmap {
        fn drop(&mut self) {
            if !self.0 .0.is_null() {
                // SAFETY: GetIconInfo gave us these bitmaps to delete, once each.
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(self.0 .0));
                }
            }
        }
    }

    struct ScreenDc(HDC);
    impl Drop for ScreenDc {
        fn drop(&mut self) {
            // SAFETY: from GetDC(None), released once.
            unsafe {
                ReleaseDC(None, self.0);
            }
        }
    }

    /// 32-bit top-down BGRA pixels of a bitmap, `w * h * 4` bytes.
    fn pixels(dc: &ScreenDc, bmp: &Bitmap, w: i32, h: i32) -> Option<Vec<u8>> {
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        // SAFETY: `buf` holds w*h 32-bit pixels, as `info` asks for.
        let lines = unsafe { GetDIBits(dc.0, bmp.0, 0, h as u32, Some(buf.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS) };
        (lines == h).then_some(buf)
    }

    fn png_data_url(rgba: &[u8], w: u32, h: u32) -> Option<String> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().ok()?.write_image_data(rgba).ok()?;
        }
        let url = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(out));
        (url.len() <= MAX_DATA_URL).then_some(url)
    }

    /// The large icon `index` of `path` as a PNG data URL. `None` for anything that goes wrong.
    /// Every handle is released by a guard, on every path out.
    pub fn extract_png(path: &str, index: i32) -> Option<String> {
        if !crate::claims::is_fixed_local(path) {
            // A network, removable or optical drive can stall for a long time; the letter tile does fine.
            return None;
        }
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut large = HICON::default();
        let mut small = HICON::default();
        // SAFETY: `wide` is NUL-terminated; the out-pointers are valid for one icon each.
        let n = unsafe { ExtractIconExW(PCWSTR(wide.as_ptr()), index, Some(&mut large), Some(&mut small), 1) };
        let large = Icon(large);
        let _small = Icon(small);
        if n == 0 || n == u32::MAX || large.0 .0.is_null() {
            return None;
        }
        let mut info = ICONINFO::default();
        // SAFETY: `info` is a valid out-pointer and `large` a live icon.
        unsafe { GetIconInfo(large.0, &mut info) }.ok()?;
        let color = Bitmap(info.hbmColor);
        let mask = Bitmap(info.hbmMask);
        if color.0 .0.is_null() {
            return None; // a black-and-white icon: not worth a special case
        }
        let mut bm = BITMAP::default();
        // SAFETY: `bm` is a BITMAP and the size says so.
        let got = unsafe { GetObjectW(HGDIOBJ(color.0 .0), std::mem::size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast())) };
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if got == 0 || w <= 0 || h <= 0 || w > MAX_SIDE || h > MAX_SIDE {
            return None;
        }
        // SAFETY: a screen DC for GetDIBits, released by the guard.
        let dc = ScreenDc(unsafe { GetDC(None) });
        let mut px = pixels(&dc, &color, w, h)?;
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            // An old icon without an alpha channel: the mask says which pixels are see-through.
            let m = pixels(&dc, &mask, w, h)?;
            for (p, m) in px.chunks_exact_mut(4).zip(m.chunks_exact(4)) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA to RGBA
        }
        png_data_url(&px, w as u32, h as u32)
    }
}

#[cfg(test)]
mod tests;
