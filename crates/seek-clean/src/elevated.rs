//! Admin removal. Program Files leftovers and Seek's own stale data need admin rights, so the
//! window starts `seek-app.exe --clean-elevated <pipe name>` behind one UAC prompt. The job goes
//! to the elevated copy, and its result comes back, over a named pipe the window made: no file
//! holds either, and the elevated copy writes nothing to the filesystem but the Recycle Bin.
//!
//! This is a security boundary. Whoever is on the other end of the pipe is treated as a same-user
//! attacker: the elevated process takes paths from the job and nothing else, and runs them through
//! the removal engine's own rules (not a second set), plus the few rules that only apply here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use seek_core::{DeviceId, DeviceRegistry};
use serde::{Deserialize, Serialize};

use crate::claims::SystemClaims;
use crate::engine::{Engine, Guard};
use crate::facts::{AdminAnswer, ClaimCheck, Elevator, FileFacts, RealFacts, Recycler};
use crate::model::{Candidate, ElevatedJob, ItemOutcome, JobItem, Outcome, Why};
use crate::paths::{canon, real_location, target_location};
use crate::protect::{is_seek_data_file, uninstall_lock, Places};
use crate::reasons::{ADMIN_SILENT, CHANGED, GONE};
use crate::recycle::ShellRecycler;

pub mod account;
pub mod guard;
pub mod pipe;
pub mod trust;

/// A job bigger than this is refused unread.
pub const MAX_JOB_BYTES: u64 = 1 << 20;
pub const MAX_JOB_ITEMS: usize = 10_000;
/// A result bigger than this is ignored.
const MAX_RESULT_BYTES: u64 = 16 << 20;
/// How long the elevated copy waits for the job to arrive.
const JOB_TIMEOUT: Duration = Duration::from_secs(30);
/// Once the elevated process has exited, its serving thread has this long to end by itself, then
/// is cancelled, and is let go after the deadline: the window never waits longer than that.
const SERVE_GRACE: Duration = Duration::from_millis(500);
const SERVE_DEADLINE: Duration = Duration::from_secs(5);

pub const DECLINED: &str = "Windows asked for admin and it was declined.";
/// The exit code of a run that refused the job as a whole and did nothing.
pub const EXIT_REFUSED: i32 = 2;
/// The exit code of a run that processed the job but could not send the result.
pub const EXIT_NO_RESULT: i32 = 3;
const OUTSIDE: &str = "This is not somewhere the admin step may remove from, so it was left in place.";
const DEVICE_IN_USE: &str = "This belongs to a drive or PC Seek still has, so it was left in place.";
const REFUSED_RUN: &str = "The admin step refused to run, so nothing was removed.";

/// What the elevated process sends back. `note` says why everything was refused when the job could
/// not be taken item by item; `stopped_early` is the engine's own sentence when a move could not
/// be confirmed and the rest was left untouched.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ElevatedResult {
    pub items: Vec<ItemOutcome>,
    pub note: Option<String>,
    pub stopped_early: Option<String>,
}

fn left(j: &JobItem, reason: &str) -> ItemOutcome {
    ItemOutcome { id: j.id, path: j.path.clone(), size: j.size, outcome: Outcome::Left { reason: reason.into() } }
}

// ---- the rules that only exist here ----------------------------------------------------------------

/// The rules that only apply to the elevated side, on top of what the engine checks for every
/// item: what the item is must be one of the things admin removal is for, and it must be where
/// those live, by the path as written and by where it really is, compared by whole segments.
/// Those are: a Leftover or Uninstall folder in Program Files, a `.lnk` an uninstall left in the
/// all-users Start Menu, and one of Seek's own stale files. Also refuses a claimed folder, so
/// that can be tested alone.
pub fn check_item(
    item: &JobItem,
    places: &Places,
    claims: &dyn ClaimCheck,
    live_devices: &HashSet<DeviceId>,
) -> Result<(), String> {
    let path = Path::new(&item.path);
    if item.path.split(['\\', '/']).any(|s| s == ".." || s == ".") {
        return Err(OUTSIDE.into());
    }
    let written = canon(path);
    let Some(real) = real_location(path) else {
        return Err(if std::fs::symlink_metadata(path).is_err() { GONE } else { OUTSIDE }.into());
    };
    // A link in the last segment is followed too: it may not point out of the place it sits in.
    let target = target_location(path).unwrap_or_else(|| real.clone());
    let spellings = [written.as_str(), real.as_str(), target.as_str()];
    match item.why {
        Why::Leftover | Why::Uninstall => {
            let in_pf = spellings.iter().all(|s| places.in_program_files(s) && !places.is_program_files_root(s));
            // Nothing else in ProgramData: only a shortcut the engine's own Start Menu rule accepts.
            let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
            let menu_link = item.why == Why::Uninstall
                && !is_dir
                && spellings.iter().all(|s| places.in_all_users_start_menu(s) && uninstall_lock(places, s, false).is_none());
            if !in_pf && !menu_link {
                return Err(OUTSIDE.into());
            }
            if in_pf && (claims.is_claimed(path).is_some() || claims.is_claimed(Path::new(&real)).is_some()) {
                return Err("An app or a running program is using this folder, so it was left in place.".into());
            }
        }
        Why::StaleData => {
            if !spellings.iter().all(|s| is_seek_data_file(places, s)) {
                return Err(OUTSIDE.into());
            }
            for s in spellings {
                let name = s.rsplit('\\').next().unwrap_or("");
                let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
                let id = hex_of_id(stem).ok_or_else(|| "This is not named like one of Seek's index or journal files, so it was left in place.".to_string())?;
                if live_devices.iter().any(|d| d.as_simple().to_string() == id) {
                    return Err(DEVICE_IN_USE.into());
                }
            }
        }
        Why::Duplicate => return Err(OUTSIDE.into()),
    }
    Ok(())
}

/// A device id in any spelling (dashes, braces, any case) as 32 lowercase hex digits, or `None`
/// when the text is not shaped like one.
fn hex_of_id(stem: &str) -> Option<String> {
    let hex: String = stem.chars().filter(|c| !matches!(c, '-' | '{' | '}')).collect::<String>().to_lowercase();
    (hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(hex)
}

/// The devices Seek still has, from `devices.json` in its ProgramData folder. A file that is
/// missing, unreadable or not what the service writes is an error: no stale file is then removed.
pub fn read_live_devices(data_dir: &Path) -> Result<HashSet<DeviceId>, String> {
    let file = data_dir.join("devices.json");
    let bytes = std::fs::read(&file).map_err(|_| format!("Seek's device list ({}) couldn't be read, so its stale files were left in place.", file.display()))?;
    let reg: DeviceRegistry = serde_json::from_slice(&bytes).map_err(|_| format!("Seek's device list ({}) isn't in the form expected, so its stale files were left in place.", file.display()))?;
    Ok(reg.devices.iter().map(|d| d.id).collect())
}

// ---- the elevated run ---------------------------------------------------------------------------

/// Everything the elevated side reads for itself. The real one is `real_context`.
pub struct ElevatedContext {
    pub places: Places,
    pub facts: Arc<dyn FileFacts>,
    pub recycler: Arc<dyn Recycler>,
    pub claims: Arc<dyn ClaimCheck>,
    /// Held around each item's last look and recycle.
    pub guard: Arc<dyn Guard>,
    pub protected: Result<Vec<String>, String>,
    pub devices: Result<HashSet<DeviceId>, String>,
    pub account: Result<(), String>,
}

/// One outcome per job item, in the job's order. Nothing is removed unless the account matches and
/// the protected list could be read; each item must then pass `check_item`, still match what the
/// window reviewed (twice: before and after the engine looks), and pass every rule of the engine.
pub fn process_job(job: &ElevatedJob, ctx: &ElevatedContext) -> ElevatedResult {
    let refuse_all = |why: &str| ElevatedResult { items: job.items.iter().map(|j| left(j, why)).collect(), ..ElevatedResult::default() };
    if let Err(e) = &ctx.account {
        return refuse_all(e);
    }
    let protected = match &ctx.protected {
        Ok(p) => p,
        Err(e) => return refuse_all(e),
    };
    let mut seen_ids = HashSet::new();
    if !job.items.iter().all(|j| seen_ids.insert(j.id)) {
        return refuse_all("The job lists the same item twice, so nothing was removed.");
    }
    let no_devices = HashSet::new();
    let mut results: HashMap<u32, Outcome> = HashMap::new();
    let mut candidates = Vec::new();
    let mut by_path: HashMap<String, &JobItem> = HashMap::new();
    let mut seen_paths = HashSet::new();
    for j in &job.items {
        let outcome = (|| -> Result<(), String> {
            let devices = match (&ctx.devices, j.why) {
                (Ok(d), _) => d,
                (Err(e), Why::StaleData) => return Err(e.clone()),
                (Err(_), _) => &no_devices,
            };
            check_item(j, &ctx.places, ctx.claims.as_ref(), devices)?;
            if !seen_paths.insert(canon(Path::new(&j.path))) {
                return Err("The job lists this path twice, so it was left in place.".into());
            }
            // UAC can sit open for minutes: it must still be what the window reviewed.
            let now = ctx.facts.stat(Path::new(&j.path)).ok_or(GONE)?;
            if now.size != j.size || now.modified_ms != j.modified_ms {
                return Err(CHANGED.into());
            }
            candidates.push(match (j.why, now.is_dir) {
                (Why::StaleData, _) => Candidate::StaleFile { path: PathBuf::from(&j.path) },
                (Why::Uninstall, false) => Candidate::Shortcut { path: PathBuf::from(&j.path), level: None, reasons: vec![] },
                (why, _) => Candidate::Folder { path: PathBuf::from(&j.path), why, level: None, reasons: vec![] },
            });
            by_path.insert(j.path.clone(), j);
            Ok(())
        })();
        if let Err(reason) = outcome {
            results.insert(j.id, Outcome::Left { reason });
        }
    }
    let mut stopped_early = None;
    if !candidates.is_empty() {
        stopped_early = run_engine(ctx, protected, candidates, &by_path, &mut results);
    }
    let items = job
        .items
        .iter()
        .map(|j| ItemOutcome {
            id: j.id,
            path: j.path.clone(),
            size: j.size,
            outcome: results.remove(&j.id).unwrap_or(Outcome::Left { reason: GONE.into() }),
        })
        .collect();
    ElevatedResult { items, note: None, stopped_early }
}

/// The engine's own prepare and commit, in the mode where admin items are recycled on the spot.
/// Returns why it stopped early, if it did.
fn run_engine(
    ctx: &ElevatedContext,
    protected: &[String],
    candidates: Vec<Candidate>,
    by_path: &HashMap<String, &JobItem>,
    results: &mut HashMap<u32, Outcome>,
) -> Option<String> {
    let engine = Engine::new_elevated(ctx.places.clone(), ctx.recycler.clone(), ctx.facts.clone(), ctx.claims.clone()).with_guard(ctx.guard.clone());
    let prepared = engine.prepare(candidates, protected);
    let mut job_id: HashMap<u32, u32> = HashMap::new();
    let mut ids = Vec::new();
    for i in &prepared.items {
        let Some(j) = by_path.get(&i.path) else { continue };
        job_id.insert(i.id, j.id);
        // What the engine just read must still be what the window reviewed.
        if i.size != j.size || i.modified_ms != j.modified_ms {
            results.insert(j.id, Outcome::Left { reason: CHANGED.into() });
        } else {
            ids.push(i.id);
        }
    }
    match engine.commit(&prepared.token, &ids, protected) {
        Ok(done) => {
            for item in done.items {
                if let Some(j) = job_id.get(&item.id) {
                    results.insert(*j, item.outcome);
                }
            }
            done.stopped_early
        }
        Err(e) => {
            for i in ids {
                results.insert(job_id[&i], Outcome::Left { reason: e.clone() });
            }
            None
        }
    }
}

// ---- the process entry ----------------------------------------------------------------------------

/// The whole elevated run, with the way the facts are read passed in. `pipe_name` must be exactly
/// the shape the window makes. Returns the exit code: 0 when the job was processed and the
/// result sent, `EXIT_REFUSED` when it was refused as a whole (a result saying so is sent if it
/// can be; nothing was removed), `EXIT_NO_RESULT` when it was processed but the result could not
/// be sent.
pub fn run_main_with(pipe_name: &str, context: &dyn Fn(&ElevatedJob) -> Result<ElevatedContext, String>) -> i32 {
    if !pipe::name_ok(pipe_name) {
        return EXIT_REFUSED;
    }
    let Ok(client) = pipe::Client::connect(pipe_name) else {
        return EXIT_REFUSED;
    };
    let server_pid = client.server_pid;
    let bytes = match client.read_job(MAX_JOB_BYTES, JOB_TIMEOUT) {
        Ok(b) => b,
        Err(note) => return refuse(client, note, vec![]),
    };
    let job: ElevatedJob = match serde_json::from_slice(&bytes) {
        Ok(j) => j,
        Err(_) => return refuse(client, "The job is malformed, so nothing was removed.".into(), vec![]),
    };
    if job.items.len() > MAX_JOB_ITEMS {
        return refuse(client, "The job lists too many items, so nothing was removed.".into(), vec![]);
    }
    let refuse_items = |note: &str| job.items.iter().map(|j| left(j, note)).collect::<Vec<_>>();
    // The job must come from the window it names: the process serving this pipe is that window.
    if server_pid != job.window_pid {
        let note = "This admin step was not asked for by the Seek window the job names, so nothing was removed.";
        return refuse(client, note.into(), refuse_items(note));
    }
    let ctx = match context(&job) {
        Ok(c) => c,
        Err(note) => return refuse(client, note.clone(), refuse_items(&note)),
    };
    let result = process_job(&job, &ctx);
    let mut bytes = serde_json::to_vec(&result).unwrap_or_default();
    if bytes.len() as u64 > MAX_RESULT_BYTES {
        // More than the window reads: say so, without the items.
        let note = "The answer was too big to send, so check the Recycle Bin.".to_string();
        bytes = serde_json::to_vec(&ElevatedResult { note: Some(note), ..ElevatedResult::default() }).unwrap_or_default();
    }
    match client.reply_within(&bytes, MAX_RESULT_BYTES, pipe::REPLY_TIMEOUT) {
        Ok(()) => 0,
        Err(_) => EXIT_NO_RESULT,
    }
}

/// Sends a result that says everything was refused, if it can.
fn refuse(client: pipe::Client, note: String, items: Vec<ItemOutcome>) -> i32 {
    if let Ok(b) = serde_json::to_vec(&ElevatedResult { items, note: Some(note), stopped_early: None }) {
        let _ = client.reply_within(&b, MAX_RESULT_BYTES, pipe::REPLY_TIMEOUT);
    }
    EXIT_REFUSED
}

/// `seek-app.exe --clean-elevated <pipe name>`. `protected` is the user's "Always protect" list as
/// read by the same strict reader the window uses (an error means nothing is removed).
pub fn run_main(pipe_name: &str, protected: Result<Vec<String>, String>, trusted: Result<trust::TrustedFolders, String>) -> i32 {
    run_main_with(pipe_name, &|job| real_context(job, protected.clone(), trusted.clone()))
}

/// The one place the elevated side builds its real pieces: the disk's own facts, the Recycle Bin
/// recycler, and fresh claims read now (never a cache) that take the system folders from Windows
/// and not from the environment.
fn real_context(job: &ElevatedJob, protected: Result<Vec<String>, String>, trusted: Result<trust::TrustedFolders, String>) -> Result<ElevatedContext, String> {
    let trusted = trusted?;
    let places = Places::from_system().map_err(|e| format!("The admin step can't run safely on this PC: {e}"))?;
    // What Windows named must be what HKLM says; otherwise something moved the system folders.
    trust::check_places(&places, &trusted)?;
    let devices = places
        .seek_data_dir()
        .ok_or_else(|| "Windows didn't say where Seek's data is kept.".to_string())
        .and_then(|d| read_live_devices(&d));
    let account = account::check_account(&job.user_sid, &account::read_facts(job.window_pid));
    crate::claims::trust_system_folders_only();
    Ok(ElevatedContext {
        guard: Arc::new(guard::ParentChainGuard::new(places.guarded_roots())),
        places,
        facts: Arc::new(RealFacts),
        recycler: Arc::new(ShellRecycler),
        claims: Arc::new(SystemClaims::read()),
        protected,
        devices,
        account,
    })
}

// ---- the window's side -----------------------------------------------------------------------------

/// Why the admin step could not be started.
#[derive(Clone, Debug, PartialEq)]
pub enum LaunchError {
    /// The user said no at the UAC prompt (ERROR_CANCELLED).
    Declined,
    Failed(String),
}

/// Starts the elevated copy on `pipe_name`, tells `started` its process id as soon as it exists
/// (before anything else: the pipe serves no one else), waits for it to exit and returns its exit
/// code. The real one is `cleanup::ShellLaunch`; tests use fakes.
pub trait Launcher: Send + Sync {
    fn launch_and_wait(&self, pipe_name: &str, started: &dyn Fn(u32)) -> Result<u32, LaunchError>;
}

/// Every item, left in place for one reason.
fn all_left(items: &[JobItem], reason: &str) -> AdminAnswer {
    AdminAnswer { items: items.iter().map(|j| left(j, reason)).collect(), stopped_early: None }
}

/// One elevated run for `items`: the pipe, one launch, wait, read what came back. Every item gets
/// an outcome whatever happens.
pub fn run_elevated(items: &[JobItem], user_sid: &str, window_pid: u32, launcher: &dyn Launcher) -> AdminAnswer {
    let prepare_failed = |e: String| all_left(items, &format!("Seek couldn't prepare the admin step ({e}), so this was left in place."));
    if items.len() > MAX_JOB_ITEMS {
        return prepare_failed("too many items for one admin step".into());
    }
    let job = ElevatedJob { user_sid: user_sid.to_string(), window_pid, items: items.to_vec() };
    let bytes = match serde_json::to_vec(&job) {
        Ok(b) if b.len() as u64 <= MAX_JOB_BYTES => b,
        Ok(_) => return prepare_failed("the job is too big".into()),
        Err(e) => return prepare_failed(e.to_string()),
    };
    let name = pipe::new_name();
    let server = match pipe::Server::create(&name) {
        Ok(s) => s,
        Err(e) => return prepare_failed(e.to_string()),
    };
    let (tell, started) = std::sync::mpsc::channel();
    let serving = std::thread::spawn(move || server.serve(&bytes, MAX_RESULT_BYTES, started));
    let launched = launcher.launch_and_wait(&name, &|pid| {
        let _ = tell.send(pid);
    });
    drop(tell);
    // The process has exited (or never started): the serving thread ends now, or is cancelled, or
    // is let go. This never blocks for long, and never holds up the commit.
    let reply = pipe::finish(serving, SERVE_GRACE, SERVE_DEADLINE).and_then(Result::ok);
    match launched {
        Err(LaunchError::Declined) => all_left(items, DECLINED),
        Err(LaunchError::Failed(e)) => all_left(items, &format!("Windows couldn't start the admin step ({e}), so this was left in place.")),
        Ok(code) => match_results(items, reply.as_deref(), code),
    }
}

/// What came back, matched to what was sent: only ids that were sent count, the first answer for
/// an id wins, and the path and size are the ones sent. Exit code 2 means nothing was done, so an
/// item without an answer is left, with the elevated side's own reason if it gave one. For any
/// other exit code an item without an answer may have been moved before the process ended, so it
/// gets the engine's "check the Recycle Bin" outcome and never one that says it was left alone.
pub fn match_results(sent: &[JobItem], result: Option<&[u8]>, exit_code: u32) -> AdminAnswer {
    let mut parsed: Option<ElevatedResult> = result.and_then(|b| serde_json::from_slice(b).ok());
    let silent = if exit_code == EXIT_REFUSED as u32 {
        // Nothing was done, so nothing in the reply but its reason counts.
        let note = parsed.take().and_then(|r| r.note);
        note.unwrap_or_else(|| REFUSED_RUN.to_string())
    } else {
        ADMIN_SILENT.to_string()
    };
    let items = sent
        .iter()
        .map(|j| match parsed.as_ref().and_then(|r| r.items.iter().find(|a| a.id == j.id)) {
            Some(a) => ItemOutcome { id: j.id, path: j.path.clone(), size: j.size, outcome: a.outcome.clone() },
            None => left(j, &silent),
        })
        .collect();
    AdminAnswer { items, stopped_early: parsed.and_then(|r| r.stopped_early) }
}

/// The real elevator's shape: asks `launcher` for admin once per `run`. Built in `cleanup::UacElevator`.
pub struct JobElevator {
    pub launcher: Arc<dyn Launcher>,
}

impl Elevator for JobElevator {
    fn run(&self, items: &[JobItem]) -> Result<AdminAnswer, String> {
        // Nothing has started yet, so a failure here leaves every item in place (not an Err, which
        // the engine reads as "the admin step may have removed some items").
        let sid = match account::current_user_sid() {
            Ok(s) => s,
            Err(e) => return Ok(all_left(items, &format!("Seek couldn't tell which Windows account this is ({e}), so nothing was sent to admin."))),
        };
        Ok(run_elevated(items, &sid, std::process::id(), self.launcher.as_ref()))
    }
}

#[cfg(test)]
mod tests;
