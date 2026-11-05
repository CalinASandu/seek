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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use seek_clean::dupes::{self, ScanEvent, SizeBatch, SizeFile, MIN_DUPLICATE_BYTES};
use seek_clean::engine::Engine;
use seek_clean::facts::{ClaimCheck, FileFacts, RealFacts};
use seek_clean::model::{Candidate, CommitResult, DupGroup, Prepared};
use seek_clean::protect::Places;
use seek_clean::recycle::ShellRecycler;
use seek_core::{Device, DeviceId, DeviceState, HostId};
use seek_ipc::{Request, Response, SizeGroup};
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
    /// Drives to ask the service about: this PC's chosen ones, then every linked PC's.
    pub ask: Vec<DeviceId>,
    pub owners: Owners,
    /// Said in the scan line: drives left out because their first scan isn't done.
    pub notice: Option<String>,
    /// True when no drive of this PC is left to read.
    pub nothing_local: bool,
}

/// Which drives to ask the service about. Of the drives asked for, only this PC's count; one still
/// on its first scan is left out (its index is partial) and named. Linked PCs' drives are added
/// so their same-name files can be noted, but only when there is something local to read.
pub fn plan_scan(requested: &[DeviceId], devices: &[Device], this_host: HostId, pc_names: &HashMap<HostId, String>) -> ScanPlan {
    let owners = owners_of(devices, this_host, pc_names);
    let mut ask: Vec<DeviceId> = Vec::new();
    let mut left_out: Vec<String> = Vec::new();
    for id in requested {
        let Some(d) = devices.iter().find(|d| d.id == *id) else { continue };
        if !owners.get(&d.id).is_some_and(|o| o.local) || ask.contains(id) {
            continue;
        }
        if d.state == DeviceState::Scanning && d.index_updated_at == 0 {
            left_out.push(d.display_name.clone());
        } else {
            ask.push(*id);
        }
    }
    let nothing_local = ask.is_empty();
    if !nothing_local {
        ask.extend(devices.iter().filter(|d| !owners[&d.id].local).map(|d| d.id));
    }
    let notice = (!left_out.is_empty()).then(|| format!("Left out, still being indexed for the first time: {}.", left_out.join(", ")));
    ScanPlan { ask, owners, notice, nothing_local }
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
/// error ends the paging with what was gathered so far. Cancel stops it between pages.
pub async fn gather<F, Fut>(mut fetch: F, owners: &Owners, cancel: &AtomicBool) -> Gathered
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
        out.batches.extend(split.batches);
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

// ---- turning a prepare request into candidates -------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReqFile {
    pub path: String,
    pub device_id: String,
}

/// Same shapes as `PrepareRequest` in `app/src/api/types.ts`. Folders and stale files belong to
/// later tasks; they are read (extra fields ignored) so they can be refused in plain words.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum PrepareRequest {
    Duplicates { groups: Vec<Vec<ReqFile>> },
    Folders {},
    Stale {},
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

// ---- hidden time -----------------------------------------------------------------------------

/// Results go once the scan is over and the window has been hidden for five minutes. Never while
/// a scan runs.
pub fn should_drop_results(scan_running: bool, hidden_for: Option<Duration>) -> bool {
    !scan_running && hidden_for.is_some_and(|d| d >= HIDDEN_DROP_AFTER)
}

// ---- shared scan state -------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    running: bool,
    found: Vec<DupGroup>,
    other_pcs: Vec<OtherPcMatch>,
    linked: Vec<LinkedRow>,
    checked: u64,
    total: u64,
    current: Option<String>,
    notice: Option<String>,
    done_sent: bool,
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

    /// Marks a scan as running, with a fresh slate. One scan at a time.
    pub fn begin(&self, notice: Option<String>) -> Result<Arc<AtomicBool>, String> {
        let mut i = self.lock();
        if i.running {
            return Err("A scan is already running.".to_string());
        }
        *i = Inner { running: true, notice, cancel: Arc::new(AtomicBool::new(false)), ..Inner::default() };
        Ok(i.cancel.clone())
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

    pub fn add_notice(&self, text: &str) {
        let mut i = self.lock();
        i.notice = Some(match i.notice.take() {
            Some(n) => format!("{n} {text}"),
            None => text.to_string(),
        });
    }

    /// The linked-PC files to pair with the groups when the scan ends.
    pub fn keep_linked(&self, rows: Vec<LinkedRow>) {
        self.lock().linked = rows;
    }

    pub fn on_event(&self, e: ScanEvent, emit: &Emit) {
        let message = {
            let mut i = self.lock();
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

    /// Ends the scan: not running, other-PC notes worked out, `done` sent. Safe to call again.
    pub fn finish(&self, emit: &Emit) {
        {
            let mut i = self.lock();
            if i.done_sent {
                return;
            }
            i.done_sent = true;
            i.running = false;
            i.current = None;
            let linked = std::mem::take(&mut i.linked);
            i.other_pcs = other_pc_matches(&linked, &i.found);
        }
        emit(json!({ "type": "done" }));
    }

    /// Forgets the results. Not while a scan runs.
    pub fn drop_results(&self) {
        let mut i = self.lock();
        if i.running {
            return;
        }
        i.found = Vec::new();
        i.other_pcs = Vec::new();
        i.linked = Vec::new();
        i.checked = 0;
        i.total = 0;
        i.current = None;
        i.notice = None;
    }
}

/// Runs `body`, then finishes the scan whatever `body` did, a panic included.
pub fn run_guarded(shared: &Shared, emit: &Emit, body: impl FnOnce() + std::panic::UnwindSafe) {
    let _ = std::panic::catch_unwind(body);
    shared.finish(emit);
}

// ---- managed state -----------------------------------------------------------------------------

/// The engine and everything it needs. Built from the real pieces only: the Recycle Bin recycler
/// and the disk's own facts. There is no other kind outside tests.
pub struct Core {
    pub engine: Engine,
    pub places: Places,
    pub facts: Arc<dyn FileFacts>,
}

/// TEMPORARY, replaced in Task 10 by the real check of what apps claim. Until then every folder
/// counts as claimed, so no folder can be removed; files (duplicates) are not asked about.
pub struct NoFolderClaimsYet;

impl ClaimCheck for NoFolderClaimsYet {
    fn is_claimed(&self, _folder: &Path) -> Option<String> {
        Some("Folder checks are not available yet".to_string())
    }
}

impl Core {
    fn real(places: Places) -> Core {
        let facts: Arc<dyn FileFacts> = Arc::new(RealFacts);
        // No elevator until Task 14: Program Files leftovers are left with the admin reason.
        let engine = Engine::new(places.clone(), Arc::new(ShellRecycler), None, facts.clone(), Arc::new(NoFolderClaimsYet));
        Core { engine, places, facts }
    }
}

pub struct CleanState {
    core: Mutex<Option<Arc<Core>>>,
    shared: Arc<Shared>,
}

impl Default for CleanState {
    fn default() -> Self {
        CleanState { core: Mutex::new(None), shared: Arc::new(Shared::default()) }
    }
}

impl CleanState {
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

    /// Called by the look loop with how long the window has been hidden.
    pub fn window_hidden_for(&self, hidden_for: Option<Duration>) {
        if !should_drop_results(self.shared.is_running(), hidden_for) {
            return;
        }
        self.shared.drop_results();
        let slot = self.core.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(core) = slot.as_ref() {
            core.engine.drop_reviews();
        }
    }
}

// ---- talking to the service --------------------------------------------------------------------

fn unexpected(r: Response) -> String {
    format!("unexpected reply from service: {r:?}")
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

async fn fetch_page(link: &ServiceLink, ids: &[DeviceId], cursor: Option<u64>) -> Page {
    let req = Request::SameSizeFiles { min_size: MIN_DUPLICATE_BYTES, device_ids: ids.to_vec(), cursor, limit: PAGE_LIMIT };
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
async fn scan_job(
    shared: Arc<Shared>,
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
    let gathered = {
        let (conn, ids) = (&*link, &plan.ask[..]);
        gather(|cursor| fetch_page(conn, ids, cursor), &plan.owners, &cancel).await
    };
    drop(link);
    if let Some(n) = &gathered.notice {
        shared.add_notice(n);
    }
    shared.keep_linked(gathered.linked);
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let batches = gathered.batches;
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let sh = shared.clone();
        run_guarded(
            &shared,
            &*emit,
            std::panic::AssertUnwindSafe(|| {
                dupes::scan(batches, &core.places, &protected, &*core.facts, &downloads_like(), &cancel, |e| sh.on_event(e, &*emit));
            }),
        );
    })
    .await;
}

// ---- commands ----------------------------------------------------------------------------------

type Clean<'a> = State<'a, Arc<CleanState>>;

#[tauri::command]
pub async fn clean_scan_start(app: AppHandle, clean: Clean<'_>, device_ids: Vec<DeviceId>) -> Result<(), String> {
    let core = clean.core()?;
    let protected: Vec<PathBuf> = settings::protected_paths_strict()?.into_iter().map(PathBuf::from).collect();
    // The paging has a connection of its own, closed when the paging is over.
    let link = Arc::new(ServiceLink::default());
    let ctx = fetch_context(&link).await?;
    let plan = plan_scan(&device_ids, &ctx.devices, ctx.this_host, &ctx.pc_names);
    let cancel = clean.shared.begin(plan.notice.clone())?;
    let shared = clean.shared.clone();
    let emit: Arc<Emit> = Arc::new(move |v| {
        let _ = app.emit(CLEAN_EVENT_CHANNEL, v);
    });
    tauri::async_runtime::spawn(async move {
        let worker = tauri::async_runtime::spawn(scan_job(shared.clone(), emit.clone(), core, link, plan, protected, cancel));
        // Whatever happened to the worker, a panic included, the window hears `done` and the scan stops running.
        let _ = worker.await;
        shared.finish(&*emit);
    });
    Ok(())
}

#[tauri::command]
pub fn clean_scan_cancel(clean: Clean<'_>) {
    clean.shared.cancel();
}

#[tauri::command]
pub fn clean_scan_state(clean: Clean<'_>) -> ScanState {
    clean.shared.state()
}

#[tauri::command]
pub async fn clean_prepare(clean: Clean<'_>, link: State<'_, Arc<ServiceLink>>, req: PrepareRequest) -> Result<Prepared, String> {
    let core = clean.core()?;
    let groups = match req {
        PrepareRequest::Duplicates { groups } => groups,
        PrepareRequest::Folders {} | PrepareRequest::Stale {} => {
            return Err("Removing leftovers isn't available in this version yet.".to_string());
        }
    };
    let protected = settings::protected_paths_strict()?;
    // Which copies are on this PC is asked of the service now, not remembered from the scan.
    let ctx = fetch_context(&link).await?;
    let owners = owners_of(&ctx.devices, ctx.this_host, &ctx.pc_names);
    let candidates = candidates_from_request(groups, &owners);
    tauri::async_runtime::spawn_blocking(move || core.engine.prepare(candidates, &protected))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clean_commit(clean: Clean<'_>, token: String, ids: Vec<u32>) -> Result<CommitResult, String> {
    let core = clean.core()?;
    let protected = settings::protected_paths_strict()?;
    // Removal happens on a thread of its own; the engine lets one commit run at a time.
    tauri::async_runtime::spawn_blocking(move || core.engine.commit(&token, &ids, &protected))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn open_recycle_bin() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(shell::open_recycle_bin)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|_| "Windows couldn't open the Recycle Bin.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seek_clean::model::{DupCopy, Lock, Outcome};
    use seek_core::{DeviceIdentity, DeviceKind, ResultRow};
    use std::sync::atomic::AtomicUsize;

    const MB: u64 = 1_048_576;

    fn id(n: u128) -> DeviceId {
        DeviceId::from_u128(n)
    }
    fn host(n: u128) -> HostId {
        HostId::from_u128(n)
    }

    fn device(n: u128, host_n: u128, state: DeviceState, updated: i64) -> Device {
        Device {
            id: id(n),
            identity: DeviceIdentity::Volume { serial: n as u64, filesystem: "NTFS".into() },
            display_name: format!("Drive {n}"),
            label: String::new(),
            filesystem: "NTFS".into(),
            kind: DeviceKind::Fixed,
            total_bytes: 0,
            free_bytes: 0,
            last_mount_point: "C:\\".into(),
            indexed_by_host: host(host_n),
            first_seen: 0,
            last_seen: 0,
            index_updated_at: updated,
            state,
            entry_count: 0,
            error: None,
        }
    }

    fn row(dev: u128, path: &str, size: u64) -> ResultRow {
        let name = path.rsplit('\\').next().unwrap().to_string();
        ResultRow {
            name,
            folder: String::new(),
            full_path: path.to_string(),
            size,
            modified: 0,
            created: 0,
            is_dir: false,
            attrs: 0,
            device_id: id(dev),
            device_name: String::new(),
            online: true,
        }
    }

    fn owners() -> Owners {
        let mut o = Owners::new();
        o.insert(id(1), Owner { local: true, pc_name: "This".into() });
        o.insert(id(2), Owner { local: true, pc_name: "This".into() });
        o.insert(id(9), Owner { local: false, pc_name: "Laptop".into() });
        o
    }

    fn quiet() -> Box<Emit> {
        Box::new(|_| {})
    }

    #[test]
    fn split_rows_separates_local_from_linked_pcs() {
        let groups = vec![
            // two local copies and a linked file with the same name: the linked one is kept to be shown
            SizeGroup {
                size: 5 * MB,
                files: vec![row(1, "C:\\a\\Movie.mkv", 5 * MB), row(2, "D:\\b\\movie.MKV", 5 * MB), row(9, "C:\\Users\\x\\Movie.mkv", 5 * MB)],
            },
            // a linked file whose name differs: not kept, and never a candidate
            SizeGroup {
                size: 3 * MB,
                files: vec![row(1, "C:\\a\\one.bin", 3 * MB), row(2, "D:\\a\\one.bin", 3 * MB), row(9, "C:\\other.bin", 3 * MB)],
            },
            // one local file and one linked file: no local pair, so nothing from this size
            SizeGroup { size: 2 * MB, files: vec![row(1, "C:\\only.bin", 2 * MB), row(9, "C:\\only.bin", 2 * MB)] },
            // a drive nobody knows is ignored altogether
            SizeGroup { size: 1_500_000, files: vec![row(1, "C:\\p.bin", 1_500_000), row(1, "C:\\q.bin", 1_500_000), row(77, "Z:\\p.bin", 1_500_000)] },
        ];
        let s = split_rows(groups, &owners());
        assert_eq!(s.batches.len(), 3);
        let paths = |b: &SizeBatch| b.files.iter().map(|f| f.path.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert_eq!(s.batches[0].size, 5 * MB);
        assert_eq!(paths(&s.batches[0]), vec!["C:\\a\\Movie.mkv", "D:\\b\\movie.MKV"]);
        assert_eq!(s.batches[0].files[0].device_id, id(1).to_string());
        assert_eq!(paths(&s.batches[2]), vec!["C:\\p.bin", "C:\\q.bin"]);
        // no linked path is in any batch
        let all: Vec<String> = s.batches.iter().flat_map(paths).collect();
        assert!(!all.iter().any(|p| p.contains("Users\\x") || p.contains("other.bin") || p.contains("only.bin") || p.starts_with("Z:")));
        assert_eq!(
            s.linked,
            vec![LinkedRow { name: "Movie.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Users\\x\\Movie.mkv".into() }]
        );
    }

    fn group(size: u64, paths: &[&str]) -> DupGroup {
        DupGroup {
            id: 1,
            name: String::new(),
            size,
            copies: paths
                .iter()
                .map(|p| DupCopy { path: p.to_string(), device_id: id(1).to_string(), size, modified: 0, lock: None, suggested_keep: false })
                .collect(),
            freeable: size,
        }
    }

    #[test]
    fn linked_files_show_only_next_to_a_confirmed_group_with_the_same_name_and_size() {
        let linked = vec![
            LinkedRow { name: "Movie.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Users\\x\\Movie.mkv".into() },
            LinkedRow { name: "Other.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Other.mkv".into() },
            LinkedRow { name: "Movie.mkv".into(), size: 6 * MB, pc_name: "Laptop".into(), path: "C:\\Other\\Movie.mkv".into() },
        ];
        // the group's second copy is named differently: the match is on any copy's name
        let groups = vec![group(5 * MB, &["C:\\a\\x.bin", "D:\\b\\movie.MKV"])];
        let m = other_pc_matches(&linked, &groups);
        assert_eq!(m, vec![OtherPcMatch { name: "Movie.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Users\\x\\Movie.mkv".into() }]);
        assert!(other_pc_matches(&linked, &[]).is_empty());
    }

    #[test]
    fn prepare_request_with_a_linked_device_becomes_a_refused_candidate() {
        let file = |path: &str, dev: u128| ReqFile { path: path.into(), device_id: id(dev).to_string() };
        let req = vec![vec![file("C:\\a\\x.bin", 1), file("D:\\b\\x.bin", 2), file("C:\\Users\\x\\x.bin", 9), file("Q:\\mystery.bin", 55)]];
        let cands = candidates_from_request(req, &owners());
        assert_eq!(
            cands,
            vec![
                Candidate::Duplicates { copies: vec![PathBuf::from("C:\\a\\x.bin"), PathBuf::from("D:\\b\\x.bin")] },
                Candidate::LinkedPc { path: "C:\\Users\\x\\x.bin".into() },
                Candidate::LinkedPc { path: "Q:\\mystery.bin".into() },
            ]
        );
        // an unreadable device id is treated like an unknown drive
        let bad = candidates_from_request(vec![vec![ReqFile { path: "C:\\z.bin".into(), device_id: "nope".into() }]], &owners());
        assert_eq!(bad, vec![Candidate::LinkedPc { path: "C:\\z.bin".into() }]);

        // and the engine refuses it, whatever else is chosen
        struct NeverRecycles;
        impl seek_clean::facts::Recycler for NeverRecycles {
            fn recycle(&self, _: &[PathBuf]) -> Vec<Result<String, String>> {
                panic!("a linked file must never reach the Recycle Bin");
            }
        }
        let places = Places::for_tests("C:\\Windows", &["C:\\Program Files"], "C:\\Users", "C:\\Users\\me", "C:\\ProgramData", &["C:\\Users\\me\\AppData"]);
        let engine = Engine::new(places, Arc::new(NeverRecycles), None, Arc::new(RealFacts), Arc::new(NoFolderClaimsYet));
        let prepared = engine.prepare(cands, &[]);
        let linked: Vec<_> = prepared.items.iter().filter(|i| i.path.contains("Users\\x") || i.path.starts_with("Q:")).collect();
        assert_eq!(linked.len(), 2);
        assert!(linked.iter().all(|i| i.lock == Some(Lock::LinkedPc)));
        let ids: Vec<u32> = linked.iter().map(|i| i.id).collect();
        let r = engine.commit(&prepared.token, &ids, &[]).unwrap();
        assert!(r.items.iter().all(|i| matches!(i.outcome, Outcome::Left { .. })));
        assert_eq!(r.moved_bytes, 0);
    }

    #[test]
    fn results_are_dropped_after_five_minutes_hidden_once_finished() {
        let five = Duration::from_secs(300);
        assert!(should_drop_results(false, Some(five)));
        assert!(should_drop_results(false, Some(five + Duration::from_secs(1))));
        assert!(!should_drop_results(false, Some(five - Duration::from_millis(1))));
        assert!(!should_drop_results(false, None)); // the window is showing
    }

    #[test]
    fn results_stay_while_the_scan_runs() {
        assert!(!should_drop_results(true, Some(Duration::from_secs(3600))));
        assert!(!should_drop_results(true, Some(Duration::from_secs(300))));
    }

    #[test]
    fn dropping_results_clears_them_unless_a_scan_runs() {
        let s = Shared::default();
        s.begin(None).unwrap();
        s.on_event(ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*quiet());
        s.drop_results();
        assert_eq!(s.state().found.len(), 1, "a running scan keeps its results");
        s.finish(&*quiet());
        s.drop_results();
        let st = s.state();
        assert!(!st.running && st.found.is_empty() && st.other_pcs.is_empty());
        assert_eq!((st.checked_bytes, st.total_bytes, st.current), (0, 0, None));
    }

    fn page(sizes: &[(u64, &str)], next: Option<u64>) -> Page {
        Ok((
            sizes
                .iter()
                .map(|(sz, p)| SizeGroup { size: *sz, files: vec![row(1, &format!("C:\\{p}1"), *sz), row(2, &format!("D:\\{p}2"), *sz)] })
                .collect(),
            next,
        ))
    }

    #[tokio::test]
    async fn a_service_error_mid_paging_ends_the_scan_with_what_was_found_and_says_so() {
        let calls = AtomicUsize::new(0);
        let cursors = Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let g = gather(
            |cursor| {
                cursors.lock().unwrap().push(cursor);
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let r = match n {
                    0 => page(&[(9 * MB, "big")], Some(4 * MB)),
                    1 => page(&[(4 * MB, "mid")], Some(2 * MB)),
                    _ => Err("Data could not be read right now, so its files can't be compared".to_string()),
                };
                async move { r }
            },
            &owners(),
            &cancel,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 3, "no more pages after the error");
        assert_eq!(*cursors.lock().unwrap(), vec![None, Some(4 * MB), Some(2 * MB)]);
        assert_eq!(g.batches.iter().map(|b| b.size).collect::<Vec<_>>(), vec![9 * MB, 4 * MB]);
        let notice = g.notice.expect("the error is said");
        assert!(notice.contains("Data could not be read right now"), "{notice}");
    }

    #[tokio::test]
    async fn paging_stops_at_the_last_page_and_never_loops_on_a_stuck_cursor() {
        let cancel = AtomicBool::new(false);
        let g = gather(|_| async { page(&[(9 * MB, "a")], None) }, &owners(), &cancel).await;
        assert_eq!(g.batches.len(), 1);
        assert!(g.notice.is_none());
        // a service that keeps answering with the same cursor ends the paging with a notice
        let calls = AtomicUsize::new(0);
        let g = gather(
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { page(&[(9 * MB, "a")], Some(9 * MB)) }
            },
            &owners(),
            &cancel,
        )
        .await;
        assert!(calls.load(Ordering::SeqCst) <= 2);
        assert!(g.notice.is_some());
    }

    #[tokio::test]
    async fn cancel_stops_the_paging_between_pages() {
        let cancel = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        let g = gather(
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                cancel.store(true, Ordering::SeqCst);
                async { page(&[(9 * MB, "a")], Some(4 * MB)) }
            },
            &owners(),
            &cancel,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(g.batches.len(), 1);
        assert!(g.notice.is_none());
    }

    fn counting() -> (Arc<Mutex<Vec<Value>>>, Box<Emit>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        (seen, Box::new(move |v| s2.lock().unwrap().push(v)))
    }

    #[test]
    fn done_is_sent_once_and_the_scan_stops_running_even_if_the_scan_panics() {
        let s = Shared::default();
        let (seen, emit) = counting();
        s.begin(None).unwrap();
        assert!(s.is_running());
        run_guarded(&s, &*emit, || panic!("the scan blew up"));
        assert!(!s.is_running());
        assert!(!s.state().running);
        s.finish(&*emit); // a second finish says nothing more
        let dones = seen.lock().unwrap().iter().filter(|v| v["type"] == "done").count();
        assert_eq!(dones, 1);
        // a new scan can start afterwards
        assert!(s.begin(None).is_ok());
    }

    #[test]
    fn done_follows_a_cancel_and_the_groups_found_stay() {
        let s = Shared::default();
        let (seen, emit) = counting();
        let cancel = s.begin(None).unwrap();
        s.on_event(ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*emit);
        s.on_event(ScanEvent::Progress { checked_bytes: 3, total_bytes: 9, current: Some("C:\\a".into()) }, &*emit);
        s.cancel();
        assert!(cancel.load(Ordering::SeqCst));
        run_guarded(&s, &*emit, || {});
        let st = s.state();
        assert!(!st.running);
        assert_eq!(st.found.len(), 1);
        let types: Vec<String> = seen.lock().unwrap().iter().map(|v| v["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(types, vec!["group", "progress", "done"]);
        assert_eq!(seen.lock().unwrap()[0]["group"]["copies"][0]["suggestedKeep"], false);
    }

    #[test]
    fn a_second_scan_cannot_start_while_one_runs_and_the_notice_survives_to_the_state() {
        let s = Shared::default();
        s.begin(Some("Left out: Data".into())).unwrap();
        assert!(s.begin(None).is_err());
        s.add_notice("Backup could not be read right now");
        let st = s.state();
        assert_eq!(st.notice.as_deref(), Some("Left out: Data Backup could not be read right now"));
        s.finish(&*quiet());
        assert!(s.state().notice.is_some(), "the notice stays after the scan ends");
    }

    #[test]
    fn finishing_pairs_linked_files_with_the_groups_that_were_confirmed() {
        let s = Shared::default();
        s.begin(None).unwrap();
        s.keep_linked(vec![
            LinkedRow { name: "x.bin".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\x.bin".into() },
            LinkedRow { name: "y.bin".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\y.bin".into() },
        ]);
        s.on_event(ScanEvent::Group(group(5 * MB, &["C:\\a\\x.bin", "C:\\b\\x.bin"])), &*quiet());
        assert!(s.state().other_pcs.is_empty(), "worked out when the scan ends");
        s.finish(&*quiet());
        let names: Vec<String> = s.state().other_pcs.into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["x.bin"]);
    }

    #[test]
    fn a_scan_leaves_out_drives_still_on_their_first_scan_and_says_so() {
        let devices = vec![
            device(1, 100, DeviceState::Online, 5),
            device(2, 100, DeviceState::Scanning, 0), // first scan
            device(3, 100, DeviceState::Scanning, 7), // a rescan of an index that exists
            device(9, 200, DeviceState::Online, 5),   // a linked PC's drive
        ];
        let names = HashMap::from([(host(200), "Laptop".to_string())]);
        let all = [id(1), id(2), id(3)];
        let plan = plan_scan(&all, &devices, host(100), &names);
        assert_eq!(plan.ask, vec![id(1), id(3), id(9)], "this PC's ready drives, then the linked PC's");
        assert!(!plan.nothing_local);
        let notice = plan.notice.expect("the left-out drive is named");
        assert!(notice.contains("Drive 2") && !notice.contains("Drive 3"), "{notice}");
        assert!(plan.owners[&id(1)].local && !plan.owners[&id(9)].local);
        assert_eq!(plan.owners[&id(9)].pc_name, "Laptop");
        // a linked drive that was asked for is still a linked drive
        let plan = plan_scan(&[id(9)], &devices, host(100), &names);
        assert!(plan.nothing_local);
        assert!(plan.ask.is_empty());
        // everything on its first scan: nothing to read
        let plan = plan_scan(&[id(2)], &devices, host(100), &names);
        assert!(plan.nothing_local);
    }

    #[test]
    fn this_pc_is_found_from_peers_or_from_a_single_host_and_otherwise_not_at_all() {
        let one = vec![device(1, 100, DeviceState::Online, 1), device(2, 100, DeviceState::Online, 1)];
        let two = vec![device(1, 100, DeviceState::Online, 1), device(9, 200, DeviceState::Online, 1)];
        assert_eq!(this_host_of(Ok(host(200)), &two), Ok(host(200)));
        assert_eq!(this_host_of(Err("sync is disabled".into()), &one), Ok(host(100)));
        assert!(this_host_of(Err("sync is disabled".into()), &two).unwrap_err().contains("which drives"));
    }

    #[test]
    fn a_stale_core_drops_its_reviews_with_the_results() {
        // the engine behind a CleanState forgets reviews when results go
        let places = Places::for_tests("C:\\Windows", &["C:\\Program Files"], "C:\\Users", "C:\\Users\\me", "C:\\ProgramData", &[]);
        let facts: Arc<dyn FileFacts> = Arc::new(RealFacts);
        let engine = Engine::new(places.clone(), Arc::new(seek_clean::recycle::ShellRecycler), None, facts.clone(), Arc::new(NoFolderClaimsYet));
        let token = engine.prepare(vec![], &[]).token;
        let state = CleanState { core: Mutex::new(Some(Arc::new(Core { engine, places, facts }))), shared: Arc::new(Shared::default()) };
        state.window_hidden_for(Some(Duration::from_secs(299)));
        state.window_hidden_for(None);
        let core = state.core().unwrap();
        assert!(core.engine.commit(&token, &[], &[]).is_ok(), "a review is still good before five minutes");
        let token = core.engine.prepare(vec![], &[]).token;
        state.window_hidden_for(Some(Duration::from_secs(301)));
        assert!(core.engine.commit(&token, &[], &[]).unwrap_err().contains("no longer valid"));
    }

    #[test]
    fn a_running_scan_keeps_its_results_and_reviews_while_hidden() {
        let places = Places::for_tests("C:\\Windows", &["C:\\Program Files"], "C:\\Users", "C:\\Users\\me", "C:\\ProgramData", &[]);
        let facts: Arc<dyn FileFacts> = Arc::new(RealFacts);
        let engine = Engine::new(places.clone(), Arc::new(seek_clean::recycle::ShellRecycler), None, facts.clone(), Arc::new(NoFolderClaimsYet));
        let token = engine.prepare(vec![], &[]).token;
        let shared = Arc::new(Shared::default());
        shared.begin(None).unwrap();
        shared.on_event(ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*quiet());
        let state = CleanState { core: Mutex::new(Some(Arc::new(Core { engine, places, facts }))), shared: shared.clone() };
        state.window_hidden_for(Some(Duration::from_secs(3600)));
        assert_eq!(shared.state().found.len(), 1);
        assert!(state.core().unwrap().engine.commit(&token, &[], &[]).is_ok());
    }
}
