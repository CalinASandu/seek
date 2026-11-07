//! The Decimator's Duplicates side in the app process: pages the service's same-size files, hands
//! this PC's files to `seek_clean::dupes::scan`, relays what it finds to the window, and runs the
//! review/commit steps of the removal engine.
//!
//! Rules kept here (the engine enforces the rest):
//! - Only drives indexed by this PC reach the scan. Linked-PC rows never become candidates; they
//!   show up only as "same name and size" notes, and a linked file in a request is refused.
//! - Whether a drive is this PC's is decided from the owner host the service reports, never from a path.
//! - If the protected places can't be worked out, nothing runs.
//! - The app holds no index. The service's rows are turned into path lists page by page, and
//!   released once `dupes::scan` has them.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use seek_clean::apps::{self, ShellLauncher, UninstallPhase, Uninstalls};
use seek_clean::claims::{self, LiveClaims};
use seek_clean::dupes::{self, ScanEvent, SizeBatch, SizeFile, MIN_DUPLICATE_BYTES};
use seek_clean::elevated::{JobElevator, LaunchError, Launcher};
use seek_clean::engine::Engine;
use seek_clean::facts::{Elevator, FileFacts, RealFacts};
use seek_clean::history::{self, History, PutBackResult, ShellBin};
use seek_clean::leftovers;
use seek_clean::model::{AppRow, Candidate, CommitResult, DupGroup, Level, LeftoverRow, Outcome, Prepared, Reason, Why};
use seek_clean::paths::canon;
use seek_clean::protect::Places;
use seek_clean::recycle::ShellRecycler;
use seek_core::{Device, DeviceId, DeviceState, HostId};
use seek_ipc::{Request, Response, SizeGroup, StaleFile};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::pipe::ServiceLink;
use crate::{settings, shell};

pub const CLEAN_EVENT_CHANNEL: &str = "seek://clean";

/// Files per page asked of the service (it caps pages itself too).
const PAGE_LIMIT: u32 = 20_000;
/// Once a scan has finished, results are kept this long while the window is hidden.
const HIDDEN_DROP_AFTER: Duration = Duration::from_secs(5 * 60);

type Emit = dyn Fn(Value) + Send + Sync;

// ---- who owns a drive ----------------------------------------------------------------------

/// Who indexed a drive: this PC, or a linked one.
#[derive(Clone, Debug, PartialEq)]
pub struct Owner {
    pub local: bool,
    pub pc_name: String,
}
pub type Owners = HashMap<DeviceId, Owner>;

/// What the service said about its drives and PCs.
pub struct Context {
    pub devices: Vec<Device>,
    pub this_host: HostId,
    pub pc_names: HashMap<HostId, String>,
}

pub fn owners_of(devices: &[Device], this_host: HostId, pc_names: &HashMap<HostId, String>) -> Owners {
    devices
        .iter()
        .map(|d| {
            let local = d.indexed_by_host == this_host;
            let pc_name = if local {
                String::new()
            } else {
                pc_names.get(&d.indexed_by_host).cloned().unwrap_or_else(|| "Another PC".to_string())
            };
            (d.id, Owner { local, pc_name })
        })
        .collect()
}

/// This PC's host id: from the peers answer, or, when that is unavailable (sync off), the one host
/// all the drives share. Several hosts and no answer means it can't be told, so nothing runs.
pub fn this_host_of(peers: Result<HostId, String>, devices: &[Device]) -> Result<HostId, String> {
    if let Ok(host) = peers {
        return Ok(host);
    }
    let hosts: HashSet<HostId> = devices.iter().map(|d| d.indexed_by_host).collect();
    match hosts.len() {
        0 => Ok(HostId::nil()),
        1 => Ok(hosts.into_iter().next().unwrap_or_default()),
        _ => Err("Seek can't tell which drives are on this PC right now, so nothing was scanned.".to_string()),
    }
}

// ---- planning a scan -----------------------------------------------------------------------

pub struct ScanPlan {
    /// This PC's chosen drives that are ready: the only source of what is scanned.
    pub ask_local: Vec<DeviceId>,
    /// Every linked PC's drives, asked about in a second, best-effort pass that only builds the
    /// "other PCs" notes. Empty when there is nothing local to read.
    pub ask_linked: Vec<DeviceId>,
    pub owners: Owners,
    /// Said right away: drives left out because their first scan isn't done, or why nothing runs.
    pub notice: Option<String>,
    /// True when no drive of this PC is left to read.
    pub nothing_local: bool,
}

const LINKED_ONLY: &str = "Duplicates only looks at this PC's drives, so nothing was scanned.";

/// Which drives to ask the service about. Of the drives asked for, only this PC's count; one still
/// on its first scan is left out (its index is partial) and named, and so is one that is offline
/// (its letter may belong to another volume by now). A linked PC's drive that was
/// picked is not scanned and the notice says why.
pub fn plan_scan(requested: &[DeviceId], devices: &[Device], this_host: HostId, pc_names: &HashMap<HostId, String>) -> ScanPlan {
    let owners = owners_of(devices, this_host, pc_names);
    let mut ask_local: Vec<DeviceId> = Vec::new();
    let mut left_out: Vec<String> = Vec::new();
    let mut offline: Vec<String> = Vec::new();
    let mut picked_linked = false;
    for id in requested {
        let Some(d) = devices.iter().find(|d| d.id == *id) else { continue };
        if !owners.get(&d.id).is_some_and(|o| o.local) {
            picked_linked = true;
            continue;
        }
        if ask_local.contains(id) {
            continue;
        }
        if d.state == DeviceState::Offline {
            offline.push(d.display_name.clone());
        } else if d.state == DeviceState::Scanning && d.index_updated_at == 0 {
            left_out.push(d.display_name.clone());
        } else {
            ask_local.push(*id);
        }
    }
    let nothing_local = ask_local.is_empty();
    let ask_linked: Vec<DeviceId> = if nothing_local { Vec::new() } else { devices.iter().filter(|d| !owners[&d.id].local).map(|d| d.id).collect() };
    let mut notes = Vec::new();
    if !left_out.is_empty() {
        notes.push(format!("Left out, still being indexed for the first time: {}.", left_out.join(", ")));
    }
    notes.extend(offline.iter().map(|name| format!("{name} is offline, so it wasn't scanned.")));
    if nothing_local && picked_linked && left_out.is_empty() && offline.is_empty() {
        notes.push(LINKED_ONLY.to_string());
    }
    ScanPlan { ask_local, ask_linked, owners, notice: (!notes.is_empty()).then(|| notes.join(" ")), nothing_local }
}

// ---- splitting the service's rows ------------------------------------------------------------

/// A file on a linked PC that has the name and size of a file on this PC. Kept only to be shown.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkedRow {
    pub name: String,
    pub size: u64,
    pub pc_name: String,
    pub path: String,
}

#[derive(Default)]
pub struct Split {
    /// This PC's files per size, ready for the scan.
    pub batches: Vec<SizeBatch>,
    pub linked: Vec<LinkedRow>,
}

/// Local rows become scan input; linked rows are never input. A linked row is kept (to be shown)
/// only when a local file of the same size has the same name. A size with fewer than two local
/// files is dropped whole. Rows from drives not in `owners` are ignored.
pub fn split_rows(groups: Vec<SizeGroup>, owners: &Owners) -> Split {
    let mut out = Split::default();
    for g in groups {
        let mut files = Vec::new();
        let mut names = HashSet::new();
        let mut linked = Vec::new();
        for r in g.files {
            if r.is_dir {
                continue;
            }
            match owners.get(&r.device_id) {
                Some(o) if o.local => {
                    names.insert(r.name.to_lowercase());
                    files.push(SizeFile { path: PathBuf::from(r.full_path), device_id: r.device_id.to_string() });
                }
                Some(o) => linked.push((r, o.pc_name.clone())),
                None => {}
            }
        }
        if files.len() < 2 {
            continue;
        }
        let size = g.size;
        out.linked.extend(
            linked
                .into_iter()
                .filter(|(r, _)| names.contains(&r.name.to_lowercase()))
                .map(|(r, pc_name)| LinkedRow { name: r.name, size, pc_name, path: r.full_path }),
        );
        out.batches.push(SizeBatch { size, files });
    }
    out
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherPcMatch {
    pub name: String,
    pub size: u64,
    pub pc_name: String,
    pub path: String,
}

/// The linked-PC files whose name and size match a copy in a confirmed group.
pub fn other_pc_matches(linked: &[LinkedRow], groups: &[DupGroup]) -> Vec<OtherPcMatch> {
    let have: HashSet<(u64, String)> = groups
        .iter()
        .flat_map(|g| {
            g.copies
                .iter()
                .filter_map(move |c| Path::new(&c.path).file_name().map(|n| (g.size, n.to_string_lossy().to_lowercase())))
        })
        .collect();
    let mut out: Vec<OtherPcMatch> = linked
        .iter()
        .filter(|l| have.contains(&(l.size, l.name.to_lowercase())))
        .map(|l| OtherPcMatch { name: l.name.clone(), size: l.size, pc_name: l.pc_name.clone(), path: l.path.clone() })
        .collect();
    out.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.pc_name.cmp(&b.pc_name))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

// ---- paging ----------------------------------------------------------------------------------

#[derive(Default)]
pub struct Gathered {
    pub batches: Vec<SizeBatch>,
    pub linked: Vec<LinkedRow>,
    /// Set when the paging ended on an error: the message names the drive.
    pub notice: Option<String>,
}

pub type Page = Result<(Vec<SizeGroup>, Option<u64>), String>;

/// Fetches pages until the last one, splitting each as it arrives so the rows are released. An
/// error ends the paging with what was gathered so far. Cancel stops it between pages. With
/// `keep_batches` false only the linked-PC notes are kept.
pub async fn gather<F, Fut>(mut fetch: F, owners: &Owners, keep_batches: bool, cancel: &AtomicBool) -> Gathered
where
    F: FnMut(Option<u64>) -> Fut,
    Fut: Future<Output = Page>,
{
    let mut out = Gathered::default();
    let mut cursor: Option<u64> = None;
    while !cancel.load(Ordering::Relaxed) {
        let (groups, next) = match fetch(cursor).await {
            Ok(page) => page,
            Err(message) => {
                out.notice = Some(message);
                break;
            }
        };
        let split = split_rows(groups, owners); // the page's rows end here
        if keep_batches {
            out.batches.extend(split.batches);
        }
        out.linked.extend(split.linked);
        match next {
            None => break,
            // Sizes must keep falling; a service that repeats itself would page for ever.
            Some(n) if cursor.is_some_and(|c| n >= c) => {
                out.notice = Some("The service's pages stopped advancing, so the scan used what it had.".to_string());
                break;
            }
            Some(n) => cursor = Some(n),
        }
    }
    out
}

const OTHER_PCS_LOST: &str = "Files on your other PCs couldn't be compared this time.";

pub struct Collected {
    /// The scan's whole input, from this PC's drives alone.
    pub batches: Vec<SizeBatch>,
    pub linked: Vec<LinkedRow>,
    /// The local pager ended on an error; the message names the user's own drive.
    pub local_notice: Option<String>,
    /// The second pass failed: only the Other PCs section is lost.
    pub linked_notice: Option<String>,
}

/// Two passes. The first pages this PC's drives only and is the sole source of `batches`, so an
/// error in it is about the user's own drive. The second, only if linked drives exist, pages this
/// PC's plus the linked drives (the service's two-or-more rule needs both sides) and keeps only
/// the notes for the Other PCs section; if it fails that section is empty and the scan is untouched.
pub async fn collect<F, Fut>(mut fetch: F, plan: &ScanPlan, cancel: &AtomicBool) -> Collected
where
    F: FnMut(Vec<DeviceId>, Option<u64>) -> Fut,
    Fut: Future<Output = Page>,
{
    let local = gather(|cursor| fetch(plan.ask_local.clone(), cursor), &plan.owners, true, cancel).await;
    let mut out = Collected { batches: local.batches, linked: Vec::new(), local_notice: local.notice, linked_notice: None };
    if plan.ask_linked.is_empty() || cancel.load(Ordering::Relaxed) {
        return out;
    }
    let both: Vec<DeviceId> = plan.ask_local.iter().chain(&plan.ask_linked).copied().collect();
    let second = gather(|cursor| fetch(both.clone(), cursor), &plan.owners, false, cancel).await;
    if second.notice.is_some() {
        out.linked_notice = Some(OTHER_PCS_LOST.to_string());
    } else {
        out.linked = second.linked;
    }
    out
}

// ---- turning a prepare request into candidates -------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReqFile {
    pub path: String,
    pub device_id: String,
}

/// A folder of a `Folders` request. Only its path counts: the confidence the sheet shows comes from
/// the list Seek made (`Listing`), whatever level or reasons the request carries.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReqFolder {
    pub path: String,
}

/// Same shapes as `PrepareRequest` in `app/src/api/types.ts`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum PrepareRequest {
    Duplicates { groups: Vec<Vec<ReqFile>> },
    /// `app_id` names the app whose uninstall left the folders (`Why::Uninstall` only).
    Folders {
        why: Why,
        folders: Vec<ReqFolder>,
        #[serde(default)]
        app_id: Option<String>,
    },
    Stale { files: Vec<String> },
}

/// Each group becomes one `Duplicates` of its local copies (locked ones included: they count as
/// survivors). A file on a linked PC, or on a drive nobody knows, becomes `LinkedPc`, which the
/// engine refuses and never counts as a surviving copy.
pub fn candidates_from_request(groups: Vec<Vec<ReqFile>>, owners: &Owners) -> Vec<Candidate> {
    let mut out = Vec::new();
    for group in groups {
        let (mut copies, mut linked) = (Vec::new(), Vec::new());
        for f in group {
            let local = f.device_id.parse::<DeviceId>().ok().and_then(|id| owners.get(&id)).is_some_and(|o| o.local);
            if local {
                copies.push(PathBuf::from(f.path));
            } else {
                linked.push(f.path);
            }
        }
        if !copies.is_empty() {
            out.push(Candidate::Duplicates { copies });
        }
        out.extend(linked.into_iter().map(|path| Candidate::LinkedPc { path }));
    }
    out
}

// ---- leftovers: the list, and turning their requests into candidates ----------------------------

/// What the Leftovers list says about one of Seek's own stale files. It is a fact the service
/// reported, not a guess, so it is the top level, with the one reason to show.
const STALE_REASON: &str = "No drive Seek knows about uses this file";
const STALE_POINTS: i32 = 5;

/// The index and journal file names the service can report as stale.
fn looks_stale(path: &str) -> bool {
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    ext.eq_ignore_ascii_case("seekidx") || ext.eq_ignore_ascii_case("log")
}

pub fn stale_row(f: &StaleFile) -> LeftoverRow {
    let parent = Path::new(&f.path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    LeftoverRow {
        path: f.path.clone(),
        parent,
        size: f.size,
        modified: f.modified,
        needs_admin: true,
        level: seek_clean::confidence::level(STALE_POINTS),
        reasons: vec![Reason { text: STALE_REASON.to_string(), points: STALE_POINTS }],
        contents: vec![stale_line(&f.path).to_string()],
    }
}

/// What a stale file is, in the place where the window lists what is inside a row.
fn stale_line(path: &str) -> &'static str {
    if path.to_lowercase().ends_with(".log") {
        "Seek's change log for a drive it no longer knows"
    } else {
        "Seek's index for a drive it no longer knows"
    }
}

/// The folder rows with Seek's own stale files added. If the service could not be asked, the
/// folders are still the answer and there are simply no stale rows. Highest level first; within
/// a level the biggest first, as the window lists them.
pub fn merge_leftovers(folders: Vec<LeftoverRow>, stale: Result<Vec<StaleFile>, String>) -> Vec<LeftoverRow> {
    let mut rows = folders;
    match stale {
        Ok(files) => rows.extend(files.iter().map(stale_row)),
        Err(e) => tracing::warn!("stale Seek data not listed, the service could not be asked: {e}"),
    }
    let rank = |l: Level| match l {
        Level::VeryLikely => 0,
        Level::Likely => 1,
        Level::NotSure => 2,
        Level::ProbablyInUse => 3,
    };
    rows.sort_by_key(|r| (rank(r.level), std::cmp::Reverse(r.size)));
    rows
}

/// Facts about folders from Seek's own index, asked of the service in one request. Called from a
/// thread that may block. If the service can't be reached, it knows nothing and the folders are walked.
struct ServiceIndex(Arc<ServiceLink>);

pub fn summary_of(f: seek_core::FolderFact) -> leftovers::Summary {
    leftovers::Summary {
        size: f.size,
        files: f.files,
        newest_ms: f.newest_modified,
        subfolders: f.has_subfolders,
        has_files: f.has_files,
        programs: f.has_programs,
        top: f.top_names,
        cloud: f.has_cloud,
    }
}

impl leftovers::IndexFacts for ServiceIndex {
    fn summaries(&self, folders: &[PathBuf]) -> Vec<Option<leftovers::Summary>> {
        let paths: Vec<String> = folders.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        match tauri::async_runtime::block_on(self.0.request(Request::FolderFacts { paths })) {
            Ok(Response::FolderFacts { facts }) if facts.len() == folders.len() => {
                facts.into_iter().map(|f| f.map(summary_of)).collect()
            }
            other => {
                tracing::warn!("folder facts not available, walking the folders: {other:?}");
                vec![None; folders.len()]
            }
        }
    }
}

/// The folders on this PC that nothing claims, scored. Slow when it has to walk (it reads every
/// folder's metadata), so only ever called from a thread of its own. It builds its own claims, so it
/// shares no lock with the removal path.
fn scan_folders(core: &Core, link: Arc<ServiceLink>) -> Vec<LeftoverRow> {
    let claims = LiveClaims::new();
    let installed = claims::read_installed_names();
    leftovers::scan(&core.places, &claims, &installed, &*core.facts, &ServiceIndex(link))
}

/// What the last list showed for one row: the confidence Seek worked out, kept so the review sheet
/// shows it and not what a request says.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedRow {
    pub level: Level,
    pub reasons: Vec<Reason>,
}

/// The last list `clean_leftovers` (or an uninstall's "what's left") gave, by path in `canon` form
/// (lowercased).
pub type Listing = HashMap<String, ListedRow>;

pub fn listing_of(rows: &[LeftoverRow]) -> Listing {
    rows.iter()
        .map(|r| (canon(Path::new(&r.path)), ListedRow { level: r.level, reasons: r.reasons.clone() }))
        .collect()
}

/// Refuses a leftover or stale request for a path that was not in the last list. The window only
/// asks for what it was shown; anything else is not a request Seek made.
pub fn check_listed<'a>(paths: impl IntoIterator<Item = &'a str>, listed: &Listing) -> Result<(), String> {
    match paths.into_iter().find(|p| !listed.contains_key(&canon(Path::new(p)))) {
        None => Ok(()),
        Some(p) => Err(format!("{p} isn't in the list of leftovers Seek last showed, so it can't be removed from here. Look again, then review it.")),
    }
}

/// Refuses a folder request for a path the matching list did not show. Leftovers are checked against
/// the last Leftovers list; an uninstall's against the last list given for that same app, so one
/// app's list can't vouch for another's, and a request that names no app gets nothing.
pub fn check_folder_request<'a>(
    why: Why,
    app_id: Option<&str>,
    paths: impl IntoIterator<Item = &'a str>,
    leftovers: &Listing,
    uninstalls: &HashMap<String, Listing>,
) -> Result<(), String> {
    match why {
        Why::Uninstall => {
            let app_id = app_id.ok_or("Seek needs to know which app these leftovers came from. Look again, then review them.")?;
            let listed = uninstalls
                .get(app_id)
                .ok_or("Seek hasn't listed what that app left behind. Look again, then review it.")?;
            check_listed(paths, listed)
        }
        _ => check_listed(paths, leftovers),
    }
}

fn same_place(a: &str, b: &str) -> bool {
    canon(Path::new(a)) == canon(Path::new(b))
}

/// Candidates for a request to remove leftover folders. A path the service reports as stale is
/// one of Seek's own files (`StaleFile`); anything else is a folder, and the engine refuses
/// whatever is not one. `why` is `Leftover` or `Uninstall`. What an uninstall left is a folder or a
/// Start Menu shortcut (a `.lnk`); the engine refuses anything else of either kind, and none of it
/// is one of Seek's own stale files.
///
/// The level and reasons of each come from `listing`, the list Seek itself last made, by path; the
/// request's own are not read.
pub fn folder_candidates(why: Why, folders: Vec<ReqFolder>, stale: &[StaleFile], listing: &Listing) -> Result<Vec<Candidate>, String> {
    let shown = |path: &str| match listing.get(&canon(Path::new(path))) {
        Some(row) => (Some(row.level), row.reasons.clone()),
        None => (None, Vec::new()),
    };
    if why == Why::Uninstall {
        return Ok(folders
            .into_iter()
            .map(|f| {
                let (level, reasons) = shown(&f.path);
                if Path::new(&f.path).extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                    Candidate::Shortcut { path: PathBuf::from(f.path), level, reasons }
                } else {
                    Candidate::Folder { path: PathBuf::from(f.path), why, level, reasons }
                }
            })
            .collect());
    }
    if why != Why::Leftover {
        return Err("Seek can only remove folders from the Leftovers list this way.".to_string());
    }
    Ok(folders
        .into_iter()
        .map(|f| {
            if looks_stale(&f.path) && stale.iter().any(|s| same_place(&s.path, &f.path)) {
                Candidate::StaleFile { path: PathBuf::from(f.path) }
            } else {
                let (level, reasons) = shown(&f.path);
                Candidate::Folder { path: PathBuf::from(f.path), why, level, reasons }
            }
        })
        .collect())
}

/// Candidates for a request to remove Seek's own stale files: only what the service still calls
/// stale. A file a drive has since taken over, or that is gone, is left out.
pub fn stale_candidates(files: Vec<String>, stale: &[StaleFile]) -> Vec<Candidate> {
    files
        .into_iter()
        .filter(|f| stale.iter().any(|s| same_place(&s.path, f)))
        .map(|path| Candidate::StaleFile { path: PathBuf::from(path) })
        .collect()
}

// ---- hidden time -----------------------------------------------------------------------------

/// Results go once the scan is over and five minutes have passed since the later of two things: the
/// window being hidden and the scan finishing (a scan that ends after 40 hidden minutes keeps its
/// results for 5 more). `finished_ago` is `None` when no scan has finished; `hidden_for` is `None`
/// while the window shows, which also resets the clock. Never while a scan runs.
pub fn should_drop_results(scan_running: bool, finished_ago: Option<Duration>, hidden_for: Option<Duration>) -> bool {
    if scan_running {
        return false;
    }
    let Some(hidden) = hidden_for else { return false };
    finished_ago.map_or(hidden, |f| f.min(hidden)) >= HIDDEN_DROP_AFTER
}

// ---- shared scan state -------------------------------------------------------------------------

const SCAN_STOPPED: &str = "The scan stopped early, so these results may be incomplete.";

#[derive(Default)]
struct Inner {
    /// Counts scans. Every call that belongs to one scan carries its number and does nothing for an older one.
    generation: u64,
    running: bool,
    found: Vec<DupGroup>,
    other_pcs: Vec<OtherPcMatch>,
    linked: Vec<LinkedRow>,
    checked: u64,
    total: u64,
    current: Option<String>,
    notice: Option<String>,
    done_sent: bool,
    finished_at: Option<Instant>,
    cancel: Arc<AtomicBool>,
}

/// What `clean_scan_state` returns; the shape of `DupScanState` in `types.ts`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanState {
    pub running: bool,
    pub found: Vec<DupGroup>,
    pub other_pcs: Vec<OtherPcMatch>,
    pub checked_bytes: u64,
    pub total_bytes: u64,
    pub current: Option<String>,
    pub notice: Option<String>,
}

#[derive(Default)]
pub struct Shared {
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks a scan as running, with a fresh slate and its cancel flag, before anything else is
    /// asked of the service. One scan at a time. Returns the scan's number.
    pub fn begin(&self) -> Result<(u64, Arc<AtomicBool>), String> {
        let mut i = self.lock();
        if i.running {
            return Err("A scan is already running.".to_string());
        }
        let generation = i.generation + 1;
        *i = Inner { generation, running: true, cancel: Arc::new(AtomicBool::new(false)), ..Inner::default() };
        Ok((generation, i.cancel.clone()))
    }

    /// The scan could not even start (the service did not answer): it stops running, silently,
    /// because the command that started it reports the failure.
    pub fn abort(&self, gen: u64) {
        let mut i = self.lock();
        if i.generation == gen && i.running {
            *i = Inner { generation: gen, done_sent: true, ..Inner::default() };
        }
    }

    pub fn state(&self) -> ScanState {
        let i = self.lock();
        ScanState {
            running: i.running,
            found: i.found.clone(),
            other_pcs: i.other_pcs.clone(),
            checked_bytes: i.checked,
            total_bytes: i.total,
            current: i.current.clone(),
            notice: i.notice.clone(),
        }
    }

    pub fn cancel(&self) {
        self.lock().cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.lock().running
    }

    /// How long ago the last scan finished.
    pub fn finished_ago(&self) -> Option<Duration> {
        self.lock().finished_at.map(|t| t.elapsed())
    }

    /// Adds a sentence to the scan's notice and tells the window the whole notice at once.
    pub fn add_notice(&self, gen: u64, text: &str, emit: &Emit) {
        let full = {
            let mut i = self.lock();
            if i.generation != gen {
                return;
            }
            let full = match i.notice.take() {
                Some(n) => format!("{n} {text}"),
                None => text.to_string(),
            };
            i.notice = Some(full.clone());
            full
        };
        emit(json!({ "type": "notice", "text": full }));
    }

    /// The linked-PC files to pair with the groups when the scan ends.
    pub fn keep_linked(&self, gen: u64, rows: Vec<LinkedRow>) {
        let mut i = self.lock();
        if i.generation == gen {
            i.linked = rows;
        }
    }

    pub fn on_event(&self, gen: u64, e: ScanEvent, emit: &Emit) {
        let message = {
            let mut i = self.lock();
            if i.generation != gen {
                return;
            }
            match e {
                ScanEvent::Group(g) => {
                    let m = json!({ "type": "group", "group": g });
                    i.found.push(g);
                    Some(m)
                }
                ScanEvent::Progress { checked_bytes, total_bytes, current } => {
                    i.checked = checked_bytes;
                    i.total = total_bytes;
                    i.current = current.clone();
                    Some(json!({ "type": "progress", "checkedBytes": checked_bytes, "totalBytes": total_bytes, "current": current }))
                }
                // `finish` sends the one `done`, after the notes for the other PCs are ready.
                ScanEvent::Done => None,
            }
        };
        if let Some(m) = message {
            emit(m);
        }
    }

    /// Ends the scan: not running, other-PC notes worked out, `done` sent. Safe to call again, and
    /// a no-op for a scan that is no longer the current one.
    pub fn finish(&self, gen: u64, emit: &Emit) {
        {
            let mut i = self.lock();
            if i.generation != gen || i.done_sent {
                return;
            }
            i.done_sent = true;
            i.running = false;
            i.current = None;
            i.finished_at = Some(Instant::now());
            let linked = std::mem::take(&mut i.linked);
            i.other_pcs = other_pc_matches(&linked, &i.found);
        }
        emit(json!({ "type": "done" }));
    }

    /// Forgets the results. Not while a scan runs. True if there was anything to forget.
    pub fn drop_results(&self) -> bool {
        let mut i = self.lock();
        if i.running {
            return false;
        }
        let had = !i.found.is_empty() || !i.other_pcs.is_empty() || i.notice.is_some() || i.total > 0 || i.checked > 0;
        i.found = Vec::new();
        i.other_pcs = Vec::new();
        i.linked = Vec::new();
        i.checked = 0;
        i.total = 0;
        i.current = None;
        i.notice = None;
        had
    }

    /// Takes moved files out of the results with the rules the window uses: a group with fewer than
    /// two copies left, or with nothing left to free, goes. Figures and the suggested keep are
    /// worked out again as `dupes` does.
    pub fn prune(&self, moved: &[String], downloads_like: &[String]) {
        let gone: HashSet<String> = moved.iter().map(|p| canon(Path::new(p))).collect();
        let is_gone = |p: &str| gone.contains(&canon(Path::new(p)));
        let mut i = self.lock();
        let found = std::mem::take(&mut i.found);
        i.found = found
            .into_iter()
            .filter_map(|g| if g.copies.iter().any(|c| is_gone(&c.path)) { dupes::without_copies(&g, is_gone, downloads_like) } else { Some(g) })
            .collect();
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Runs `body`, then finishes the scan whatever `body` did. A panic is logged, and the scan says
/// its results may be incomplete.
pub fn run_guarded(shared: &Shared, gen: u64, emit: &Emit, body: impl FnOnce() + std::panic::UnwindSafe) {
    if let Err(payload) = std::panic::catch_unwind(body) {
        tracing::error!("the duplicate scan panicked: {}", panic_text(&*payload));
        shared.add_notice(gen, SCAN_STOPPED, emit);
    }
    shared.finish(gen, emit);
}

// ---- managed state -----------------------------------------------------------------------------

/// The engine and everything it needs. Built from the real pieces only: the Recycle Bin recycler
/// and the disk's own facts. There is no other kind outside tests.
pub struct Core {
    pub engine: Engine,
    pub places: Places,
    pub facts: Arc<dyn FileFacts>,
    /// The engine's claims, kept so a listing can make them read again.
    pub claims: Option<Arc<LiveClaims>>,
}

impl Core {
    fn real(places: Places) -> Core {
        let facts: Arc<dyn FileFacts> = Arc::new(RealFacts);
        let claims = Arc::new(LiveClaims::new());
        // Program Files leftovers and Seek's stale data go through one UAC prompt per commit.
        // Every commit is written to Recently removed. Only this engine has a history; the elevated process reports to it.
        let engine = Engine::new(places.clone(), Arc::new(ShellRecycler), Some(Arc::new(UacElevator)), facts.clone(), claims.clone())
            .with_history(Arc::new(History::open_default()));
        Core { engine, places, facts, claims: Some(claims) }
    }
}

/// Asks Windows for admin, once, to run `seek-app.exe --clean-elevated <pipe name>`. Built only in
/// `Core::real`. Everything else about the admin step (the pipe, matching the answers) is
/// `seek_clean::elevated`.
pub struct UacElevator;

impl Elevator for UacElevator {
    fn run(&self, items: &[seek_clean::model::JobItem]) -> Result<seek_clean::facts::AdminAnswer, String> {
        JobElevator { launcher: Arc::new(ShellLaunch) }.run(items)
    }
}

/// The main window's handle, so the UAC prompt opens in front of it. 0 until the window exists.
static MAIN_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn set_main_hwnd(hwnd: isize) {
    MAIN_HWND.store(hwnd, Ordering::Relaxed);
}

/// `ShellExecuteExW` with the `runas` verb, then waits for the elevated copy to exit.
struct ShellLaunch;

/// Closes the process handle once, whatever way out.
struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from ShellExecuteExW with SEE_MASK_NOCLOSEPROCESS and is closed only here.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// COM on the launching thread, as ShellExecute asks; balanced on every path.
struct ComApartment(bool);

impl ComApartment {
    fn enter() -> ComApartment {
        use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        // SAFETY: balanced by the CoUninitialize in Drop, on this same thread, only if this succeeded.
        ComApartment(unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) } >= 0)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: the CoInitializeEx in `enter` succeeded on this thread.
            unsafe { windows_sys::Win32::System::Com::CoUninitialize() };
        }
    }
}

impl Launcher for ShellLaunch {
    fn launch_and_wait(&self, pipe_name: &str, started: &dyn Fn(u32)) -> Result<u32, LaunchError> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_CANCELLED};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, GetProcessId, WaitForSingleObject, INFINITE};
        use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let _com = ComApartment::enter();
        let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(std::iter::once(0)).collect() };
        let exe = std::env::current_exe().map_err(|e| LaunchError::Failed(e.to_string()))?;
        let file = wide(exe.as_os_str());
        let verb = wide(std::ffi::OsStr::new("runas"));
        let params = wide(std::ffi::OsStr::new(&format!("--clean-elevated \"{pipe_name}\"")));
        // SAFETY: an all-zero SHELLEXECUTEINFOW is valid (null pointers, zero flags); fields are set below.
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC;
        info.hwnd = MAIN_HWND.load(Ordering::Relaxed) as _;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        // SAFETY: `info` and the strings it points to outlive the call.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            // SAFETY: reads the calling thread's last error, straight after the failed call.
            let err = unsafe { GetLastError() };
            return Err(if err == ERROR_CANCELLED { LaunchError::Declined } else { LaunchError::Failed(std::io::Error::from_raw_os_error(err as i32).to_string()) });
        }
        if info.hProcess.is_null() {
            return Err(LaunchError::Failed("Windows did not give a process to wait for".into()));
        }
        let process = ProcessHandle(info.hProcess);
        // The pipe serves this process and no other.
        // SAFETY: a valid process handle owned by the guard.
        started(unsafe { GetProcessId(process.0) });
        // No timeout: the prompt can stay open as long as the user likes.
        // SAFETY: a valid process handle owned by the guard.
        unsafe { WaitForSingleObject(process.0, INFINITE) };
        let mut code = 0u32;
        // SAFETY: a valid process handle and out pointer.
        if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
            // It ran, and it may have moved things: an unknown code is read as "something went wrong".
            return Ok(u32::MAX);
        }
        Ok(code)
    }
}

pub struct CleanState {
    core: Mutex<Option<Arc<Core>>>,
    /// What the last Leftovers list held, with the confidence it gave each row.
    listed: Mutex<Listing>,
    /// What the last "what's left" list of each uninstalled app held, by app id.
    uninstall_listed: Mutex<HashMap<String, Listing>>,
    /// The uninstallers this run has started. Lives as long as the app, hidden window or not.
    uninstalls: Uninstalls,
    shared: Arc<Shared>,
    /// Sends events to the window; set once the app exists.
    emit: Mutex<Option<Arc<Emit>>>,
    /// Removals and put-backs in flight, so Quit can wait for them (and their history write).
    busy: Mutex<Busy>,
    idle: Condvar,
}

#[derive(Default)]
struct Busy {
    running: u32,
    quitting: bool,
}

/// Counts one removal or put-back in flight until it drops.
struct InFlight<'a>(&'a CleanState);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut b = self.0.busy.lock().unwrap_or_else(|e| e.into_inner());
        b.running = b.running.saturating_sub(1);
        if b.running == 0 {
            self.0.idle.notify_all();
        }
    }
}

impl Default for CleanState {
    fn default() -> Self {
        CleanState {
            core: Mutex::new(None),
            listed: Mutex::new(Listing::new()),
            uninstall_listed: Mutex::new(HashMap::new()),
            uninstalls: Uninstalls::default(),
            shared: Arc::new(Shared::default()),
            emit: Mutex::new(None),
            busy: Mutex::new(Busy::default()),
            idle: Condvar::new(),
        }
    }
}

impl CleanState {
    /// Lets the state talk to the window on `seek://clean`.
    pub fn set_app(&self, app: AppHandle) {
        let emit: Arc<Emit> = Arc::new(move |v| {
            let _ = app.emit(CLEAN_EVENT_CHANNEL, v);
        });
        *self.emit.lock().unwrap_or_else(|e| e.into_inner()) = Some(emit);
    }

    fn emitter(&self) -> Arc<Emit> {
        self.emit.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| Arc::new(|_| {}))
    }

    /// The engine, built on first use. If Windows can't name the protected places this is a plain
    /// error and nothing can be scanned or removed; it is tried again next time.
    fn core(&self) -> Result<Arc<Core>, String> {
        let mut slot = self.core.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let places = Places::from_system().map_err(|e| format!("Cleanup can't run safely on this PC: {e}"))?;
        let core = Arc::new(Core::real(places));
        *slot = Some(core.clone());
        Ok(core)
    }

    /// Called by the look loop with how long the window has been hidden (`None` while it shows).
    /// When the results are due to go they are dropped, the engine forgets its reviews, and a
    /// window that is listening is told so it can let go of the groups.
    pub fn window_hidden_for(&self, hidden_for: Option<Duration>) {
        if !should_drop_results(self.shared.is_running(), self.shared.finished_ago(), hidden_for) {
            return;
        }
        let had_results = self.shared.drop_results();
        // The app icons go with the results: they are cheap to extract again.
        apps::clear_icon_cache();
        if let Some(core) = self.core.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            core.engine.drop_reviews();
            // The snapshot of installed apps and running programs goes too; the next use reads it again.
            if let Some(claims) = &core.claims {
                claims.refresh();
            }
        }
        if had_results {
            (*self.emitter())(json!({ "type": "dropped" }));
        }
    }

    /// Counts a removal or put-back as in flight. Refused once Quit has begun: it would be cut off.
    fn begin(&self) -> Result<InFlight<'_>, String> {
        let mut b = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if b.quitting {
            return Err("Seek is closing. Nothing was changed.".into());
        }
        b.running += 1;
        Ok(InFlight(self))
    }

    /// Quit: refuses new removals, then waits up to `limit` for the ones running (including their
    /// history write). True when none is left. Sleeps on a condition variable; it never spins.
    pub fn finish_in_flight(&self, limit: Duration) -> bool {
        let mut b = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        b.quitting = true;
        let deadline = Instant::now() + limit;
        while b.running > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return false;
            };
            b = self.idle.wait_timeout(b, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// Removes the chosen items of a review, then takes what was moved out of the results so it
    /// cannot come back through `clean_scan_state`.
    fn commit(&self, core: &Core, token: &str, ids: &[u32], protected: &[String]) -> Result<CommitResult, String> {
        let _flying = self.begin()?;
        let result = core.engine.commit(token, ids, protected)?;
        let moved: Vec<String> = result.items.iter().filter(|i| matches!(i.outcome, Outcome::Moved { .. })).map(|i| i.path.clone()).collect();
        if !moved.is_empty() {
            self.shared.prune(&moved, &downloads_like());
        }
        Ok(result)
    }
}

// ---- talking to the service --------------------------------------------------------------------

fn unexpected(r: Response) -> String {
    tracing::warn!("unexpected reply from the service: {r:?}");
    "The Seek service gave an answer the app didn't expect.".to_string()
}

/// The drives and PCs the service knows, and which of them is this PC.
async fn fetch_context(link: &ServiceLink) -> Result<Context, String> {
    let devices = match link.request(Request::ListDevices).await? {
        Response::Devices { devices } => devices,
        other => return Err(unexpected(other)),
    };
    let mut pc_names = HashMap::new();
    let peers = match link.request(Request::ListPeers).await {
        Ok(Response::Peers { this_pc, peers, .. }) => {
            pc_names.extend(peers.into_iter().map(|p| (p.host_id, p.name)));
            Ok(this_pc.host_id)
        }
        Ok(other) => Err(unexpected(other)),
        Err(e) => Err(e),
    };
    if let Ok(Response::Hub { hub: Some(hub) }) = link.request(Request::HubStatus).await {
        pc_names.extend(hub.pcs.into_iter().map(|p| (p.host_id, p.name)));
    }
    let this_host = this_host_of(peers, &devices)?;
    Ok(Context { devices, this_host, pc_names })
}

async fn fetch_stale(link: &ServiceLink) -> Result<Vec<StaleFile>, String> {
    match link.request(Request::StaleData).await? {
        Response::StaleData { files } => Ok(files),
        other => Err(unexpected(other)),
    }
}

async fn fetch_page(link: &ServiceLink, ids: Vec<DeviceId>, cursor: Option<u64>) -> Page {
    let req = Request::SameSizeFiles { min_size: MIN_DUPLICATE_BYTES, device_ids: ids, cursor, limit: PAGE_LIMIT, same_name: false };
    match link.request(req).await? {
        Response::SameSizeFiles { groups, next } => Ok((groups, next)),
        other => Err(unexpected(other)),
    }
}

/// Folders (by name or full path) that hold things people download or throw away; a copy outside
/// them is the better one to keep.
fn downloads_like() -> Vec<String> {
    vec!["Downloads".to_string(), "Temp".to_string(), "Tmp".to_string(), std::env::temp_dir().to_string_lossy().into_owned()]
}

/// The scan itself: all the paging first, on its own connection, then the reading, which needs
/// nothing from the service. The connection is closed before the first file is read.
#[allow(clippy::too_many_arguments)]
async fn scan_job(
    shared: Arc<Shared>,
    gen: u64,
    emit: Arc<Emit>,
    core: Arc<Core>,
    link: Arc<ServiceLink>,
    plan: ScanPlan,
    protected: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
) {
    if plan.nothing_local {
        return;
    }
    let collected = {
        let conn = &*link;
        collect(|ids, cursor| fetch_page(conn, ids, cursor), &plan, &cancel).await
    };
    drop(link);
    for notice in [&collected.local_notice, &collected.linked_notice].into_iter().flatten() {
        shared.add_notice(gen, notice, &*emit);
    }
    shared.keep_linked(gen, collected.linked);
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let batches = collected.batches;
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let sh = shared.clone();
        run_guarded(
            &shared,
            gen,
            &*emit,
            std::panic::AssertUnwindSafe(|| {
                let complete = dupes::scan(batches, &core.places, &protected, &*core.facts, &downloads_like(), &cancel, |e| {
                    sh.on_event(gen, e, &*emit)
                });
                if !complete {
                    tracing::error!("the duplicate scan stopped on a panic of its own");
                    sh.add_notice(gen, SCAN_STOPPED, &*emit);
                }
            }),
        );
    })
    .await;
}

// ---- commands ----------------------------------------------------------------------------------

type Clean<'a> = State<'a, Arc<CleanState>>;

#[tauri::command]
pub async fn clean_scan_start(clean: Clean<'_>, device_ids: Vec<DeviceId>) -> Result<(), String> {
    let core = clean.core()?;
    let protected: Vec<PathBuf> = settings::protected_paths_strict()?.into_iter().map(PathBuf::from).collect();
    let emit = clean.emitter();
    // Running, with its cancel flag, from now: a Cancel while the service is being asked stops the scan.
    let (gen, cancel) = clean.shared.begin()?;
    // The paging has a connection of its own, closed when the paging is over.
    let link = Arc::new(ServiceLink::default());
    let plan = match fetch_context(&link).await {
        Ok(ctx) => plan_scan(&device_ids, &ctx.devices, ctx.this_host, &ctx.pc_names),
        Err(e) => {
            clean.shared.abort(gen);
            return Err(e);
        }
    };
    if let Some(n) = &plan.notice {
        clean.shared.add_notice(gen, n, &*emit);
    }
    let shared = clean.shared.clone();
    tauri::async_runtime::spawn(async move {
        let worker = tauri::async_runtime::spawn(scan_job(shared.clone(), gen, emit.clone(), core, link, plan, protected, cancel));
        // Whatever happened to the worker, a panic included, the window hears `done` and the scan stops running.
        if let Err(e) = worker.await {
            tracing::error!("the duplicate scan job failed: {e}");
            shared.add_notice(gen, SCAN_STOPPED, &*emit);
        }
        shared.finish(gen, &*emit);
    });
    Ok(())
}

#[tauri::command(async)]
pub fn clean_scan_cancel(clean: Clean<'_>) {
    clean.shared.cancel();
}

#[tauri::command(async)]
pub fn clean_scan_state(clean: Clean<'_>) -> ScanState {
    clean.shared.state()
}

/// Leftover folders on this PC plus Seek's own stale files. The folder walk runs on a thread of
/// its own, so the window stays alive, and the service is asked meanwhile.
#[tauri::command]
pub async fn clean_leftovers(clean: Clean<'_>, link: State<'_, Arc<ServiceLink>>) -> Result<Vec<LeftoverRow>, String> {
    let core = clean.core()?;
    let own_link: Arc<ServiceLink> = (*link).clone();
    let walk = tauri::async_runtime::spawn_blocking(move || scan_folders(&core, own_link));
    let stale = fetch_stale(&link).await;
    let folders = walk.await.map_err(|e| format!("Looking for leftovers stopped unexpectedly: {e}"))?;
    let rows = merge_leftovers(folders, stale);
    *clean.listed.lock().unwrap_or_else(|e| e.into_inner()) = listing_of(&rows);
    Ok(rows)
}

/// The installed apps, with their icons. Reading the registry and pulling icons out of exe files
/// runs on a thread of its own so the window stays alive; icons are cached after the first call.
#[tauri::command]
pub async fn clean_apps() -> Result<Vec<AppRow>, String> {
    tauri::async_runtime::spawn_blocking(apps::try_list)
        .await
        .map_err(|e| format!("Listing the installed apps stopped unexpectedly: {e}"))?
}

#[tauri::command]
pub async fn clean_prepare(clean: Clean<'_>, link: State<'_, Arc<ServiceLink>>, req: PrepareRequest) -> Result<Prepared, String> {
    let core = clean.core()?;
    let protected = settings::protected_paths_strict()?;
    let candidates = match req {
        PrepareRequest::Duplicates { groups } => {
            // Which copies are on this PC is asked of the service now, not remembered from the scan.
            let ctx = fetch_context(&link).await?;
            let owners = owners_of(&ctx.devices, ctx.this_host, &ctx.pc_names);
            candidates_from_request(groups, &owners)
        }
        PrepareRequest::Folders { why, folders, app_id } => {
            // One snapshot of the lists, under both locks: the check and the confidence lookup below
            // use the same list, so a path that passed the check always has its row (a new listing
            // arriving in between changes nothing here). The confidence shown comes from this list,
            // not from the request.
            let listing = {
                let leftovers = clean.listed.lock().unwrap_or_else(|e| e.into_inner());
                let uninstalls = clean.uninstall_listed.lock().unwrap_or_else(|e| e.into_inner());
                check_folder_request(why, app_id.as_deref(), folders.iter().map(|f| f.path.as_str()), &leftovers, &uninstalls)?;
                match (why, app_id.as_deref()) {
                    (Why::Uninstall, Some(app)) => uninstalls.get(app).cloned().unwrap_or_default(),
                    _ => leftovers.clone(),
                }
            };
            // Which files are stale is also asked now. Only a leftovers request that names one needs the answer.
            let stale = if why == Why::Leftover && folders.iter().any(|f| looks_stale(&f.path)) { fetch_stale(&link).await? } else { Vec::new() };
            folder_candidates(why, folders, &stale, &listing)?
        }
        PrepareRequest::Stale { files } => {
            check_listed(files.iter().map(String::as_str), &clean.listed.lock().unwrap_or_else(|e| e.into_inner()))?;
            stale_candidates(files, &fetch_stale(&link).await?)
        }
    };
    tauri::async_runtime::spawn_blocking(move || core.engine.prepare(candidates, &protected))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clean_commit(clean: Clean<'_>, token: String, ids: Vec<u32>) -> Result<CommitResult, String> {
    let core = clean.core()?;
    let protected = settings::protected_paths_strict()?;
    let state: Arc<CleanState> = (*clean).clone();
    // Removal happens on a thread of its own; the engine lets one commit run at a time.
    tauri::async_runtime::spawn_blocking(move || state.commit(&core, &token, &ids, &protected))
        .await
        .map_err(|e| e.to_string())?
}

/// Past removals, newest first. A history that can't be read is an error with its reason; a damaged
/// file is set aside the next time Seek removes something.
#[tauri::command]
pub async fn clean_history() -> Result<Vec<history::Entry>, String> {
    tauri::async_runtime::spawn_blocking(|| History::open_default().try_list())
        .await
        .map_err(|e| format!("Reading Recently removed stopped unexpectedly: {e}"))?
}

/// Puts the listed entries back from the Recycle Bin. Restoring is COM on a thread of its own.
#[tauri::command]
pub async fn clean_put_back(clean: Clean<'_>, entry_ids: Vec<String>) -> Result<Vec<PutBackResult>, String> {
    let state: Arc<CleanState> = (*clean).clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _flying = state.begin()?;
        // Windows' own prompt, if one ever shows, opens over Seek's window.
        let bin = ShellBin { owner: MAIN_HWND.load(Ordering::Relaxed) };
        Ok(history::put_back_ids(&History::open_default(), &bin, &entry_ids))
    })
    .await
    .map_err(|e| format!("Putting back stopped unexpectedly: {e}"))?
}

// ---- uninstalling -------------------------------------------------------------------------------------

/// Starts the app's own uninstaller. The command is read from the registry here, by the app id
/// alone: nothing the window sends is ever run. The real launcher is made only in this function.
#[tauri::command]
pub async fn uninstall_start(clean: Clean<'_>, app_id: String) -> Result<(), String> {
    let state: Arc<CleanState> = (*clean).clone();
    // Reading the registry, and an admin prompt if Windows asks for one, can take a while.
    tauri::async_runtime::spawn_blocking(move || state.uninstalls.start(&ShellLauncher, apps::found_by_id(&app_id)))
        .await
        .map_err(|e| format!("Starting the uninstaller stopped unexpectedly: {e}"))?
}

/// Where the uninstall of this app stands: running, finished (the process ended and the registry
/// entry is gone) or unresolved (it ended, and the entry was still there five minutes later).
#[tauri::command(async)]
pub fn uninstall_state(clean: Clean<'_>, app_id: String) -> Result<UninstallPhase, String> {
    clean.uninstalls.state(&app_id)
}

/// What the uninstaller of this app left behind. The list is remembered as the only thing a
/// review for this app may ask to remove.
#[tauri::command]
pub async fn uninstall_leftovers(clean: Clean<'_>, link: State<'_, Arc<ServiceLink>>, app_id: String) -> Result<Vec<LeftoverRow>, String> {
    let core = clean.core()?;
    let state: Arc<CleanState> = (*clean).clone();
    let own_link: Arc<ServiceLink> = (*link).clone();
    let id = app_id.clone();
    let rows = tauri::async_runtime::spawn_blocking(move || {
        // Refused while Windows still lists the app: nothing is "left" of something still installed.
        let app = state.uninstalls.leftovers_for(&id)?;
        // The engine's own claims are read afresh too, so the review that follows agrees with this list.
        if let Some(c) = &core.claims {
            c.refresh();
        }
        let claims = LiveClaims::new();
        let installed = claims::read_installed_names();
        Ok::<_, String>(leftovers::for_app(&app, &core.places, &claims, &installed, &ServiceIndex(own_link)))
    })
    .await
    .map_err(|e| format!("Looking for what was left behind stopped unexpectedly: {e}"))??;
    clean.uninstall_listed.lock().unwrap_or_else(|e| e.into_inner()).insert(app_id, listing_of(&rows));
    Ok(rows)
}

#[tauri::command]
pub async fn open_recycle_bin() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(shell::open_recycle_bin)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|_| "Windows couldn't open the Recycle Bin.".to_string())
}

#[cfg(test)]
mod tests;
