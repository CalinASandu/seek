//! Admin removal. Program Files leftovers and Seek's own stale data need admin rights, so the
//! window starts `seek-app.exe --clean-elevated <job file>` behind one UAC prompt. The elevated copy
//! re-checks everything and moves the items to the Recycle Bin, then writes a result beside the job.
//!
//! This is a security boundary. The job file sits in a folder the user can write to, so the
//! elevated process treats it as written by an attacker running as the same user: it takes paths
//! from it and nothing else, and runs them through the removal engine's own rules (not a second
//! set), plus the few rules that only apply here.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use seek_core::{DeviceId, DeviceRegistry};
use serde::{Deserialize, Serialize};

use crate::claims::SystemClaims;
use crate::engine::Engine;
use crate::facts::{ClaimCheck, Elevator, FileFacts, RealFacts, Recycler};
use crate::model::{Candidate, ElevatedJob, ItemOutcome, JobItem, Outcome, Why};
use crate::paths::{canon, canon_lexical, real_location, target_location};
use crate::protect::{is_seek_data_file, Places};
use crate::reasons::{CHANGED, GONE};
use crate::recycle::ShellRecycler;

pub mod account;

pub const JOB_FILE: &str = "job.json";
pub const RESULT_FILE: &str = "result.json";
/// A job bigger than this is refused unread.
pub const MAX_JOB_BYTES: u64 = 1 << 20;
pub const MAX_JOB_ITEMS: usize = 10_000;
/// A result bigger than this is ignored.
const MAX_RESULT_BYTES: u64 = 16 << 20;

pub const DECLINED: &str = "Windows asked for admin and it was declined.";
pub const NO_REPORT: &str = "The admin step didn't report on this item.";
const OUTSIDE: &str = "This is not somewhere the admin step may remove from, so it was left in place.";
const DEVICE_IN_USE: &str = "This belongs to a drive or PC Seek still has, so it was left in place.";

/// What the elevated process writes beside the job. `note` says why everything was refused when
/// the job could not be read item by item.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ElevatedResult {
    pub items: Vec<ItemOutcome>,
    pub note: Option<String>,
}

fn left(j: &JobItem, reason: &str) -> ItemOutcome {
    ItemOutcome { id: j.id, path: j.path.clone(), size: j.size, outcome: Outcome::Left { reason: reason.into() } }
}

// ---- the rules that only exist here ----------------------------------------------------------------

/// The rules that only apply to the elevated side, on top of what the engine checks for every
/// item: what the item is must be one of the two things admin removal is for, and it must be where
/// those live, by the path as written and by where it really is, compared by whole segments.
/// Also refuses a claimed folder, so this can be tested alone.
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
            let ok = spellings.iter().all(|s| places.in_program_files(s) && !places.is_program_files_root(s));
            if !ok {
                return Err(OUTSIDE.into());
            }
            if claims.is_claimed(path).is_some() || claims.is_claimed(Path::new(&real)).is_some() {
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
    pub protected: Result<Vec<String>, String>,
    pub devices: Result<HashSet<DeviceId>, String>,
    pub account: Result<(), String>,
}

/// One outcome per job item, in the job's order. Nothing is removed unless the account matches and
/// the protected list could be read; each item must then pass `check_item`, still match what the
/// window reviewed, and pass every rule of the engine.
pub fn process_job(job: &ElevatedJob, ctx: &ElevatedContext) -> Vec<ItemOutcome> {
    let refuse_all = |why: &str| job.items.iter().map(|j| left(j, why)).collect::<Vec<_>>();
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
    let mut by_path: HashMap<String, u32> = HashMap::new();
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
            by_path.insert(j.path.clone(), j.id);
            Ok(())
        })();
        if let Err(reason) = outcome {
            results.insert(j.id, Outcome::Left { reason });
        }
    }
    if !candidates.is_empty() {
        run_engine(ctx, protected, candidates, &by_path, &mut results);
    }
    job.items
        .iter()
        .map(|j| ItemOutcome {
            id: j.id,
            path: j.path.clone(),
            size: j.size,
            outcome: results.remove(&j.id).unwrap_or(Outcome::Left { reason: GONE.into() }),
        })
        .collect()
}

/// The engine's own prepare and commit, in the mode where admin items are recycled on the spot.
fn run_engine(
    ctx: &ElevatedContext,
    protected: &[String],
    candidates: Vec<Candidate>,
    by_path: &HashMap<String, u32>,
    results: &mut HashMap<u32, Outcome>,
) {
    let engine = Engine::new_elevated(ctx.places.clone(), ctx.recycler.clone(), ctx.facts.clone(), ctx.claims.clone());
    let prepared = engine.prepare(candidates, protected);
    let job_id: HashMap<u32, u32> = prepared.items.iter().filter_map(|i| by_path.get(&i.path).map(|j| (i.id, *j))).collect();
    let ids: Vec<u32> = prepared.items.iter().map(|i| i.id).collect();
    match engine.commit(&prepared.token, &ids, protected) {
        Ok(done) => {
            for item in done.items {
                if let Some(j) = job_id.get(&item.id) {
                    results.insert(*j, item.outcome);
                }
            }
        }
        Err(e) => {
            for j in job_id.values() {
                results.insert(*j, Outcome::Left { reason: e.clone() });
            }
        }
    }
}

// ---- the process entry ----------------------------------------------------------------------------

/// Checks that `job_file` is `<base>\<32 hex digits>\job.json` for real: not through a link, not a
/// link itself. Returns its folder.
fn job_folder(job_file: &Path, base: &Path) -> Result<PathBuf, String> {
    let bad = || "The job file is not in the folder Seek makes for admin steps.".to_string();
    let dir = job_file.parent().ok_or_else(bad)?;
    let hex = dir.file_name().and_then(|n| n.to_str()).ok_or_else(bad)?;
    let named = job_file.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(JOB_FILE));
    if !named || hex.len() != 32 || !hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        return Err(bad());
    }
    if canon_lexical(dir.parent().ok_or_else(bad)?) != canon_lexical(base) {
        return Err(bad());
    }
    // Where it really is must be the same place: no junction in any parent.
    let real = real_location(job_file).ok_or_else(bad)?;
    let expected = format!("{}\\{hex}\\{JOB_FILE}", real_location(base).ok_or_else(bad)?);
    if real != expected || !std::fs::symlink_metadata(job_file).map_err(|_| bad())?.file_type().is_file() {
        return Err(bad());
    }
    Ok(dir.to_path_buf())
}

/// Reads and parses a job, within the size and item caps.
fn read_job(job_file: &Path) -> Result<ElevatedJob, String> {
    let f = std::fs::File::open(job_file).map_err(|e| format!("The job file couldn't be read ({e})."))?;
    let mut bytes = Vec::new();
    f.take(MAX_JOB_BYTES + 1).read_to_end(&mut bytes).map_err(|e| format!("The job file couldn't be read ({e})."))?;
    if bytes.len() as u64 > MAX_JOB_BYTES {
        return Err("The job file is too big, so nothing was removed.".into());
    }
    let job: ElevatedJob = serde_json::from_slice(&bytes).map_err(|_| "The job file is malformed, so nothing was removed.".to_string())?;
    if job.items.len() > MAX_JOB_ITEMS {
        return Err("The job lists too many items, so nothing was removed.".into());
    }
    Ok(job)
}

/// Writes the result beside the job without following anything already there: a new file with a
/// random name, then renamed over `result.json`.
fn write_result(dir: &Path, result: &ElevatedResult) -> Result<(), String> {
    let bytes = serde_json::to_vec(result).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("result-{:032x}.tmp", rand::random::<u128>()));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(&bytes).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, dir.join(RESULT_FILE)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// The whole elevated run, with the way the facts are read passed in. Returns the exit code:
/// 0 when the job was processed and the result written, 2 when the job was refused as a whole
/// (a result saying so is written if it can be), 3 when no result could be written.
pub fn run_main_with(
    job_file: &Path,
    base: &Path,
    context: &dyn Fn(&ElevatedJob) -> Result<ElevatedContext, String>,
) -> i32 {
    let Ok(dir) = job_folder(job_file, base) else {
        return 2;
    };
    let refuse = |note: String, items: Vec<ItemOutcome>| {
        let _ = write_result(&dir, &ElevatedResult { items, note: Some(note) });
        2
    };
    let job = match read_job(job_file) {
        Ok(j) => j,
        Err(note) => return refuse(note, vec![]),
    };
    let ctx = match context(&job) {
        Ok(c) => c,
        Err(note) => return refuse(note.clone(), job.items.iter().map(|j| left(j, &note)).collect()),
    };
    let items = process_job(&job, &ctx);
    match write_result(&dir, &ElevatedResult { items, note: None }) {
        Ok(()) => 0,
        Err(_) => 3,
    }
}

/// `%LOCALAPPDATA%\Seek\elevated`, where the window puts one folder per run.
pub fn jobs_base() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join("Seek").join("elevated"))
}

/// `seek-app.exe --clean-elevated <job file>`. `protected` is the user's "Always protect" list as
/// read by the same strict reader the window uses (an error means nothing is removed).
pub fn run_main(job_file: &Path, protected: Result<Vec<String>, String>) -> i32 {
    let Some(base) = jobs_base() else {
        return 2;
    };
    run_main_with(job_file, &base, &|job| real_context(job, protected.clone()))
}

/// The one place the elevated side builds its real pieces: the disk's own facts, the Recycle Bin
/// recycler, and fresh claims read now (never a cache).
fn real_context(job: &ElevatedJob, protected: Result<Vec<String>, String>) -> Result<ElevatedContext, String> {
    let places = Places::from_system().map_err(|e| format!("The admin step can't run safely on this PC: {e}"))?;
    let devices = places
        .seek_data_dir()
        .ok_or_else(|| "Windows didn't say where Seek's data is kept.".to_string())
        .and_then(|d| read_live_devices(&d));
    let account = account::check_account(&job.user_sid, &account::read_facts(job.window_pid));
    Ok(ElevatedContext {
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

/// Starts the elevated copy on `job_file` and waits for it to exit. The real one is
/// `cleanup::ShellLaunch`; tests use fakes.
pub trait Launcher: Send + Sync {
    fn launch_and_wait(&self, job_file: &Path) -> Result<(), LaunchError>;
}

/// Removes the run folder when it goes out of scope, on every path.
struct RunFolder(PathBuf);

impl Drop for RunFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn new_run_folder(base: &Path) -> std::io::Result<RunFolder> {
    std::fs::create_dir_all(base)?;
    let dir = base.join(format!("{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&dir)?; // new: fails if it exists
    Ok(RunFolder(dir))
}

/// Every item, left in place for one reason.
fn all_left(items: &[JobItem], reason: &str) -> Vec<ItemOutcome> {
    items.iter().map(|j| left(j, reason)).collect()
}

/// One elevated run for `items`: job folder, launch, wait, read what came back, delete the
/// folder. Every item gets an outcome whatever happens.
pub fn run_elevated(items: &[JobItem], user_sid: &str, window_pid: u32, base: &Path, launcher: &dyn Launcher) -> Vec<ItemOutcome> {
    let prepare_failed = |e: String| all_left(items, &format!("Seek couldn't prepare the admin step ({e}), so this was left in place."));
    let folder = match new_run_folder(base) {
        Ok(f) => f,
        Err(e) => return prepare_failed(e.to_string()),
    };
    let job = ElevatedJob { user_sid: user_sid.to_string(), window_pid, items: items.to_vec() };
    let job_file = folder.0.join(JOB_FILE);
    let written = serde_json::to_vec(&job).map_err(|e| e.to_string()).and_then(|b| std::fs::write(&job_file, b).map_err(|e| e.to_string()));
    if let Err(e) = written {
        return prepare_failed(e);
    }
    match launcher.launch_and_wait(&job_file) {
        Err(LaunchError::Declined) => all_left(items, DECLINED),
        Err(LaunchError::Failed(e)) => all_left(items, &format!("Windows couldn't start the admin step ({e}), so this was left in place.")),
        Ok(()) => {
            let bytes = read_capped(&folder.0.join(RESULT_FILE));
            match_results(items, bytes.as_deref())
        }
    }
}

fn read_capped(file: &Path) -> Option<Vec<u8>> {
    let f = std::fs::File::open(file).ok()?;
    let mut bytes = Vec::new();
    f.take(MAX_RESULT_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_RESULT_BYTES).then_some(bytes)
}

/// What came back, matched to what was sent: only ids that were sent count, the first answer for
/// an id wins, and the path and size are the ones sent. A missing, malformed or partial result
/// leaves each item without an answer in place (with the elevated side's own reason, if it gave one).
pub fn match_results(sent: &[JobItem], result: Option<&[u8]>) -> Vec<ItemOutcome> {
    let parsed: Option<ElevatedResult> = result.and_then(|b| serde_json::from_slice(b).ok());
    let silent = parsed.as_ref().and_then(|r| r.note.clone()).unwrap_or_else(|| NO_REPORT.to_string());
    sent.iter()
        .map(|j| {
            let answer = parsed.as_ref().and_then(|r| r.items.iter().find(|a| a.id == j.id));
            match answer {
                Some(a) => ItemOutcome { id: j.id, path: j.path.clone(), size: j.size, outcome: a.outcome.clone() },
                None => left(j, &silent),
            }
        })
        .collect()
}

/// The real elevator's shape: asks `launcher` for admin once per `run`. Built in `cleanup::Core::real`.
pub struct JobElevator {
    pub launcher: Arc<dyn Launcher>,
    /// Where the run folders go; `None` means `%LOCALAPPDATA%\Seek\elevated`.
    pub base: Option<PathBuf>,
}

impl Elevator for JobElevator {
    fn run(&self, items: &[JobItem]) -> Result<Vec<ItemOutcome>, String> {
        // Nothing has started yet, so a failure here leaves every item in place (not an Err, which
        // the engine reads as "the admin step may have removed some items").
        let sid = match account::current_user_sid() {
            Ok(s) => s,
            Err(e) => return Ok(all_left(items, &format!("Seek couldn't tell which Windows account this is ({e}), so nothing was sent to admin."))),
        };
        let Some(base) = self.base.clone().or_else(jobs_base) else {
            return Ok(all_left(items, "Windows didn't say where Seek's local data is kept, so nothing was sent to admin."));
        };
        Ok(run_elevated(items, &sid, std::process::id(), &base, self.launcher.as_ref()))
    }
}

#[cfg(test)]
mod tests;
