//! Recently removed: every removal Seek makes is written to `%APPDATA%\Seek\cleanup-history.json`,
//! and an item still in the Recycle Bin can be put back from it.
//!
//! The file is `{ "version": 1, "entries": [...] }`, oldest first. Every change is read, merged and
//! written back under a lock file, through a temp file and a rename, so two windows never lose each
//! other's entries and a crash never leaves half a file. Nothing in this module deletes a file: a
//! file that can't be read is renamed aside, never removed.

use std::io::Write as _;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};

use crate::model::Why;

/// The history keeps this many entries; older ones fall off the front.
pub const MAX_ENTRIES: usize = 10_000;

/// How long a writer waits for another writer, and how often it looks again.
const LOCK_WAIT: Duration = Duration::from_secs(3);
const LOCK_RETRY: Duration = Duration::from_millis(20);
/// How long the final rename keeps trying when something (a virus scanner, a reader) holds the file.
const RENAME_WAIT: Duration = Duration::from_secs(1);
const RENAME_RETRY: Duration = Duration::from_millis(50);

const NO_APPDATA: &str = "Windows didn't say where Recently removed is kept (no APPDATA folder).";
const NEWER_WRITE: &str = "Recently removed was written by a newer Seek, so this one won't change it.";
const NEWER_READ: &str = "Recently removed was written by a newer Seek, so this one can't show it.";
const NEEDS_ADMIN: &str = "Needs admin: restore it from the Recycle Bin.";
const BACK_IN_FOLDER: &str = "It's back in its folder.";
const DAMAGED: &str = "Recently removed is damaged and can't be read. It is set aside and started again the next time Seek removes something.";

/// One removal, or one item that was asked for and left in place.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Unique across processes and runs: time, process id and a counter.
    pub id: String,
    /// When the item was moved (for an item left in place, when the commit ended), unix epoch
    /// milliseconds. Put back expects the Recycle Bin's deletion date close to this.
    pub time_ms: i64,
    pub original_path: String,
    pub size: u64,
    pub why: Why,
    /// Where it went in the Recycle Bin; `None` when it was left in place.
    pub bin_path: Option<String>,
    /// Why it was not moved; `None` when it was.
    pub left_reason: Option<String>,
    /// When it was put back, if it was.
    #[serde(default)]
    pub put_back_ms: Option<i64>,
    /// The admin step moved it. The restore verb can't be trusted to do that without a Windows
    /// prompt, so Put back refuses it and points at the Recycle Bin.
    #[serde(default)]
    pub needs_admin: bool,
}

/// What putting one entry back came to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PutBackResult {
    pub id: String,
    pub restored: bool,
    /// Plain-words reason when it could not be put back, or a note when it was put back but
    /// Recently removed could not record that.
    pub reason: Option<String>,
}

/// One item to put back: its Recycle Bin file path, where it goes, and when it was removed.
pub type RestoreJob = (String, PathBuf, i64);

/// The Recycle Bin, as put back needs it. The real one is [`ShellBin`].
pub trait BinRestore {
    /// Puts the bin item at `bin_path` back at `original`. `deleted_near_ms` is when the removal
    /// was recorded: the item must have been deleted close to then. An item the bin no longer
    /// lists is "No longer in the Recycle Bin."; a bin that can't be listed says so.
    fn restore(&self, bin_path: &str, original: &Path, deleted_near_ms: i64) -> Result<(), String>;

    /// For each bin path, whether the bin still lists it, from one listing of the bin. Asked only
    /// for entries whose original path is taken: an item the bin no longer lists is back in its
    /// folder. The default says "listed", which keeps the refusal.
    fn listed(&self, bin_paths: &[String]) -> Result<Vec<bool>, String> {
        Ok(vec![true; bin_paths.len()])
    }

    /// A batch, one answer per job in order. The real one lists the bin once for all of them.
    fn restore_all(&self, jobs: &[RestoreJob]) -> Vec<Result<(), String>> {
        jobs.iter().map(|(b, o, t)| self.restore(b, o, *t)).collect()
    }
}

/// The real Recycle Bin, through the shell's own restore verb.
pub struct ShellBin {
    /// The window (an HWND as a number, 0 for none) any prompt of Windows opens over.
    pub owner: isize,
}

impl BinRestore for ShellBin {
    fn restore(&self, bin_path: &str, original: &Path, deleted_near_ms: i64) -> Result<(), String> {
        crate::recycle::restore_near(bin_path, original, deleted_near_ms, self.owner)
    }
    fn restore_all(&self, jobs: &[RestoreJob]) -> Vec<Result<(), String>> {
        crate::recycle::restore_many(jobs, self.owner)
    }
    fn listed(&self, bin_paths: &[String]) -> Result<Vec<bool>, String> {
        crate::recycle::bin_lists(bin_paths)
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// A fresh entry id: time, process id and a counter, so no two are alike across runs and processes.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}-{}", now_ms(), std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// An OS error in plain words: Rust's text without its trailing "(os error N)".
pub fn plain_io(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text,
    }
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    entries: &'a [Entry],
}

#[derive(Deserialize)]
struct FileIn {
    entries: Vec<Entry>,
}

enum Parsed {
    Entries(Vec<Entry>),
    /// A version this Seek doesn't know: never read as damaged, never touched.
    Newer,
    Damaged,
}

fn parse(bytes: &[u8]) -> Parsed {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Parsed::Damaged;
    };
    match v.get("version").and_then(serde_json::Value::as_u64) {
        Some(1) => serde_json::from_value::<FileIn>(v).map_or(Parsed::Damaged, |f| Parsed::Entries(f.entries)),
        Some(n) if n > 1 => Parsed::Newer,
        _ => Parsed::Damaged,
    }
}

/// What a load is for.
#[derive(Clone, Copy, PartialEq)]
enum Load {
    Read,
    /// A change to the file; a file that isn't history is an error.
    Write,
    /// A change to the file; a file that isn't history is set aside and a new one started.
    WriteSettingAside,
}

/// The history file. Holds only where the file is, so it is cheap to share.
pub struct History {
    target: Result<PathBuf, String>,
    /// How long a change waits for another writer.
    wait: Duration,
}

impl History {
    pub fn open(path: impl Into<PathBuf>) -> History {
        History { target: Ok(path.into()), wait: LOCK_WAIT }
    }

    /// The history in `%APPDATA%\Seek`. With no APPDATA there is no history: every call says why.
    pub fn open_default() -> History {
        let target = std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(v).join("Seek").join("cleanup-history.json"))
            .ok_or_else(|| NO_APPDATA.to_string());
        History { target, wait: LOCK_WAIT }
    }

    fn path(&self) -> Result<&Path, String> {
        self.target.as_deref().map_err(Clone::clone)
    }

    /// Adds entries (oldest first), keeping the newest [`MAX_ENTRIES`]. A file that can't be read
    /// as history is set aside first and a new one started: the entries are still recorded and
    /// the answer is a note saying so. A file from a newer Seek is never changed (an error).
    pub fn append(&self, entries: &[Entry]) -> Result<Option<String>, String> {
        let path = self.path()?;
        let _lock = lock(path, self.wait)?;
        let (mut all, note) = load(path, Load::WriteSettingAside)?;
        all.extend_from_slice(entries);
        if all.len() > MAX_ENTRIES {
            all.drain(..all.len() - MAX_ENTRIES);
        }
        store(path, &all)?;
        Ok(note)
    }

    /// Newest first. An error says why the history can't be read; the file is left alone.
    pub fn try_list(&self) -> Result<Vec<Entry>, String> {
        let (mut all, _) = load(self.path()?, Load::Read)?;
        all.reverse();
        Ok(all)
    }

    /// Newest first. Empty when the history can't be read; `try_list` says why.
    pub fn list(&self) -> Vec<Entry> {
        self.try_list().unwrap_or_default()
    }

    /// Records that the entry was put back.
    pub fn mark_put_back(&self, id: &str, when_ms: i64) -> Result<(), String> {
        let path = self.path()?;
        let _lock = lock(path, self.wait)?;
        let (mut all, _) = load(path, Load::Write)?;
        let entry = all.iter_mut().find(|e| e.id == id).ok_or("That entry is no longer in Recently removed.")?;
        entry.put_back_ms = Some(when_ms);
        store(path, &all)
    }
}

/// Takes the lock file `cleanup-history.lock` beside the history, opened so nobody else can open it
/// (share mode 0), and holds it until the returned file drops. Waits up to `wait` while someone
/// else holds it (a sharing violation); any other failure, access denied included, is final. The
/// lock file itself is never deleted: an empty file left behind is the point.
fn lock(history: &Path, wait: Duration) -> Result<std::fs::File, String> {
    if let Some(dir) = history.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Recently removed's folder couldn't be made ({}).", plain_io(&e)))?;
    }
    let lock_path = history.with_extension("lock");
    let start = Instant::now();
    loop {
        let opened = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(&lock_path);
        match opened {
            Ok(f) => return Ok(f),
            // 32 sharing violation, 33 lock violation: another writer has it.
            Err(e) if matches!(e.raw_os_error(), Some(32 | 33)) => {
                if start.elapsed() >= wait {
                    return Err("Recently removed is busy: another Seek window is writing it.".into());
                }
                std::thread::sleep(LOCK_RETRY);
            }
            Err(e) => return Err(format!("Recently removed couldn't be locked for writing ({}).", plain_io(&e))),
        }
    }
}

/// The entries on disk, oldest first, and a note when a damaged file was set aside. No file is no
/// entries. See [`Load`] for what a file that isn't history comes to.
fn load(path: &Path, mode: Load) -> Result<(Vec<Entry>, Option<String>), String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], None)),
        Err(e) => return Err(format!("Recently removed couldn't be read ({}).", plain_io(&e))),
    };
    match parse(&bytes) {
        Parsed::Entries(entries) => Ok((entries, None)),
        Parsed::Newer => Err(if mode == Load::Read { NEWER_READ } else { NEWER_WRITE }.into()),
        Parsed::Damaged if mode != Load::WriteSettingAside => Err(DAMAGED.into()),
        Parsed::Damaged => {
            let name = set_aside(path)?;
            Ok((vec![], Some(format!("Recently removed couldn't be read, so it was started again. The old file was kept as {name}."))))
        }
    }
}

/// Renames the damaged file to `cleanup-history.corrupt-<ms>.json` and returns that name. The
/// rename fails if the name is taken (no replace flag), and then the next millisecond is tried, so
/// an earlier set-aside file is never overwritten.
fn set_aside(path: &Path) -> Result<String, String> {
    const EXISTS: [i32; 2] = [0x8007_0050_u32 as i32, 0x8007_00B7_u32 as i32]; // file exists, already exists
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut ms = now_ms();
    for _ in 0..1000 {
        let name = format!("cleanup-history.corrupt-{ms}.json");
        let (from, to) = (HSTRING::from(path.as_os_str()), HSTRING::from(dir.join(&name).as_os_str()));
        // SAFETY: plain call on two NUL-terminated strings that live through it.
        match unsafe { MoveFileExW(&from, &to, MOVE_FILE_FLAGS(0)) } {
            Ok(()) => return Ok(name),
            Err(e) if EXISTS.contains(&e.code().0) => ms += 1,
            Err(e) => return Err(format!("Recently removed is damaged, and setting it aside failed ({}).", e.message().trim())),
        }
    }
    Err("Recently removed is damaged, and no name was free to set it aside under.".into())
}

fn rename_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    let start = Instant::now();
    loop {
        match std::fs::rename(from, to) {
            // 32 sharing violation, 5 access denied: something holds the file for a moment.
            Err(e) if matches!(e.raw_os_error(), Some(32 | 5)) && start.elapsed() < RENAME_WAIT => std::thread::sleep(RENAME_RETRY),
            other => return other,
        }
    }
}

/// Writes the entries to the fixed temp file, flushes it to disk, then renames it over the history.
fn store(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let failed = |e: String| format!("Recently removed couldn't be written ({e}).");
    let bytes = serde_json::to_vec(&FileOut { version: 1, entries }).map_err(|e| failed(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        rename_retrying(&tmp, path)
    };
    write().map_err(|e| failed(plain_io(&e)))
}

/// An error as the user reads it: without the raw code Windows ends its messages with
/// (`... is not ready. (0x80070015)` becomes `... is not ready.`). Text that doesn't end that way is as it was.
pub fn plain_hresult(text: &str) -> String {
    let trimmed = text.trim_end();
    if let Some(open) = trimmed.rfind(" (0x") {
        let code = &trimmed[open + 4..];
        if let Some(hex) = code.strip_suffix(')') {
            if hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return trimmed[..open].to_string();
            }
        }
    }
    text.to_string()
}

fn is_denied(text: &str) -> bool {
    let t = text.to_lowercase();
    ["access is denied", "0x80070005", "requires elevation", "0x800702e4"].iter().any(|m| t.contains(m))
}

fn refuse(entry: &Entry, reason: String) -> PutBackResult {
    PutBackResult { id: entry.id.clone(), restored: false, reason: Some(reason) }
}

/// What can be said about an entry without asking the bin.
enum Ready {
    /// What to restore.
    Go(RestoreJob),
    /// The answer already.
    Refused(PutBackResult),
    /// Something is at the original path. If the entry's bin item is gone too, it is back in its
    /// folder (a put back that finished after its deadline); otherwise it is something new.
    Taken(String),
}

fn ready(entry: &Entry) -> Ready {
    let Some(bin_path) = entry.bin_path.as_deref() else {
        return Ready::Refused(refuse(entry, "It was never moved.".into()));
    };
    if entry.put_back_ms.is_some() {
        return Ready::Refused(refuse(entry, "Already put back.".into()));
    }
    if entry.needs_admin {
        return Ready::Refused(refuse(entry, NEEDS_ADMIN.into()));
    }
    let original = Path::new(&entry.original_path);
    // Anything at all there, even a broken link: restoring over it could lose it.
    if std::fs::symlink_metadata(original).is_ok() {
        return Ready::Taken(bin_path.to_string());
    }
    Ready::Go((bin_path.to_string(), original.to_path_buf(), entry.time_ms))
}

fn something_new(entry: &Entry) -> PutBackResult {
    refuse(entry, crate::recycle::something_new_at(Path::new(&entry.original_path)))
}

fn back_in_folder(entry: &Entry) -> PutBackResult {
    PutBackResult { id: entry.id.clone(), restored: true, reason: Some(BACK_IN_FOLDER.into()) }
}

fn answer(entry: &Entry, restored: Result<(), String>) -> PutBackResult {
    match restored {
        Ok(()) => PutBackResult { id: entry.id.clone(), restored: true, reason: None },
        Err(e) if is_denied(&e) => refuse(entry, NEEDS_ADMIN.into()),
        Err(e) => refuse(entry, plain_hresult(&e)),
    }
}

/// Puts one entry back. Never touches the history; the caller records a success.
pub fn put_back(entry: &Entry, bin: &dyn BinRestore) -> PutBackResult {
    match ready(entry) {
        Ready::Refused(refusal) => refusal,
        Ready::Taken(bin_path) => match bin.listed(&[bin_path]) {
            Ok(listed) if listed == [false] => back_in_folder(entry),
            _ => something_new(entry),
        },
        Ready::Go((bin_path, original, near)) => answer(entry, bin.restore(&bin_path, &original, near)),
    }
}

/// Puts back every listed entry and records each success. The bin is listed once for the batch.
pub fn put_back_ids(history: &History, bin: &dyn BinRestore, ids: &[String]) -> Vec<PutBackResult> {
    let entries = match history.try_list() {
        Ok(e) => e,
        Err(why) => {
            return ids.iter().map(|id| PutBackResult { id: id.clone(), restored: false, reason: Some(why.clone()) }).collect();
        }
    };
    // Per id: the entry (if it is still there) and what to do with it.
    let mut plan: Vec<(String, Option<&Entry>, Ready)> = Vec::new();
    for id in ids {
        plan.push(match entries.iter().find(|e| &e.id == id) {
            None => {
                let gone = PutBackResult { id: id.clone(), restored: false, reason: Some("That entry is no longer in Recently removed.".into()) };
                (id.clone(), None, Ready::Refused(gone))
            }
            Some(entry) => (id.clone(), Some(entry), ready(entry)),
        });
    }
    // Entries whose original path is taken: one listing of the bin settles which are back in their folder.
    let taken: Vec<String> = plan.iter().filter_map(|(_, _, r)| if let Ready::Taken(b) = r { Some(b.clone()) } else { None }).collect();
    let mut listed = if taken.is_empty() { vec![].into_iter() } else { bin.listed(&taken).unwrap_or_default().into_iter() };
    let jobs: Vec<RestoreJob> = plan.iter().filter_map(|(_, _, r)| if let Ready::Go(j) = r { Some(j.clone()) } else { None }).collect();
    let mut restored = if jobs.is_empty() { vec![] } else { bin.restore_all(&jobs) }.into_iter();
    let record = |id: &str, mut result: PutBackResult| {
        if result.restored {
            if let Err(e) = history.mark_put_back(id, now_ms()) {
                result.reason = Some(format!("Recently removed couldn't record it. {e}"));
            }
        }
        result
    };
    plan.into_iter()
        .map(|(id, entry, step)| match (entry, step) {
            (_, Ready::Refused(refusal)) => refusal,
            (Some(entry), Ready::Taken(_)) => match listed.next() {
                Some(false) => record(&id, back_in_folder(entry)),
                _ => something_new(entry),
            },
            (Some(entry), Ready::Go(_)) => {
                record(&id, answer(entry, restored.next().unwrap_or_else(|| Err("The Recycle Bin gave no answer.".into()))))
            }
            (None, _) => PutBackResult { id, restored: false, reason: Some("That entry is no longer in Recently removed.".into()) },
        })
        .collect()
}

#[cfg(test)]
mod tests;
