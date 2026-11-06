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

use crate::model::Why;

/// The history keeps this many entries; older ones fall off the front.
pub const MAX_ENTRIES: usize = 10_000;

/// How long a writer waits for another writer, and how often it looks again.
const LOCK_WAIT: Duration = Duration::from_secs(3);
const LOCK_RETRY: Duration = Duration::from_millis(20);

const NO_APPDATA: &str = "Windows didn't say where Recently removed is kept (no APPDATA folder).";

/// One removal, or one item that was asked for and left in place.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Unique across processes and runs: time, process id and a counter.
    pub id: String,
    /// When the commit finished, unix epoch milliseconds.
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
}

/// What putting one entry back came to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PutBackResult {
    pub id: String,
    pub restored: bool,
    /// Plain-words reason when it could not be put back.
    pub reason: Option<String>,
}

/// The Recycle Bin, as put back needs it. The real one is [`ShellBin`].
pub trait BinRestore {
    /// Puts the bin item at `bin_path` back at `original`. `deleted_near_ms` is when the removal
    /// was recorded: the item must have been deleted close to then.
    fn restore(&self, bin_path: &str, original: &Path, deleted_near_ms: i64) -> Result<(), String>;
    fn in_bin(&self, bin_path: &str) -> bool;
}

/// The real Recycle Bin, through the shell's own restore verb.
pub struct ShellBin;

impl BinRestore for ShellBin {
    fn restore(&self, bin_path: &str, original: &Path, deleted_near_ms: i64) -> Result<(), String> {
        crate::recycle::restore_near(bin_path, original, deleted_near_ms)
    }
    fn in_bin(&self, bin_path: &str) -> bool {
        crate::recycle::in_bin(bin_path)
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

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    entries: &'a [Entry],
}

#[derive(Deserialize)]
struct FileIn {
    version: u32,
    entries: Vec<Entry>,
}

/// The history file. Holds only where the file is, so it is cheap to share.
pub struct History {
    target: Result<PathBuf, String>,
}

impl History {
    pub fn open(path: impl Into<PathBuf>) -> History {
        History { target: Ok(path.into()) }
    }

    /// The history in `%APPDATA%\Seek`. With no APPDATA there is no history: every call says why.
    pub fn open_default() -> History {
        let target = std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(v).join("Seek").join("cleanup-history.json"))
            .ok_or_else(|| NO_APPDATA.to_string());
        History { target }
    }

    fn path(&self) -> Result<&Path, String> {
        self.target.as_deref().map_err(Clone::clone)
    }

    /// Adds entries (oldest first), keeping the newest [`MAX_ENTRIES`]. A file that can't be read
    /// as history is set aside first and a new one started.
    pub fn append(&self, entries: &[Entry]) -> Result<(), String> {
        let path = self.path()?;
        let _lock = lock(path)?;
        let mut all = load(path, true)?;
        all.extend_from_slice(entries);
        if all.len() > MAX_ENTRIES {
            all.drain(..all.len() - MAX_ENTRIES);
        }
        store(path, &all)
    }

    /// Newest first. An error says why the history can't be read; the file is left alone.
    pub fn try_list(&self) -> Result<Vec<Entry>, String> {
        let mut all = load(self.path()?, false)?;
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
        let _lock = lock(path)?;
        let mut all = load(path, false)?;
        let entry = all.iter_mut().find(|e| e.id == id).ok_or("That entry is no longer in Recently removed.")?;
        entry.put_back_ms = Some(when_ms);
        store(path, &all)
    }
}

/// Takes the lock file `cleanup-history.lock` beside the history, opened so nobody else can open it
/// (share mode 0), and holds it until the returned file drops. Waits for up to [`LOCK_WAIT`]. The
/// lock file itself is never deleted: an empty file left behind is the point.
fn lock(history: &Path) -> Result<std::fs::File, String> {
    if let Some(dir) = history.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Recently removed's folder couldn't be made: {e}"))?;
    }
    let lock_path = history.with_extension("lock");
    let start = Instant::now();
    loop {
        let opened = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(&lock_path);
        match opened {
            Ok(f) => return Ok(f),
            // 32/33 sharing and lock violations, 5 access denied (another writer mid-open).
            Err(e) if matches!(e.raw_os_error(), Some(32 | 33 | 5)) => {
                if start.elapsed() >= LOCK_WAIT {
                    return Err("Recently removed is busy: another Seek window is writing it. Try again in a moment.".into());
                }
                std::thread::sleep(LOCK_RETRY);
            }
            Err(e) => return Err(format!("Recently removed couldn't be locked for writing: {e}")),
        }
    }
}

fn parse(bytes: &[u8]) -> Option<Vec<Entry>> {
    serde_json::from_slice::<FileIn>(bytes).ok().filter(|f| f.version == 1).map(|f| f.entries)
}

/// The entries on disk, oldest first. No file is none. A file that isn't history is an error, or
/// with `set_aside` renamed to `cleanup-history.corrupt-<ms>.json` (never overwriting an earlier
/// one) so a new one can start.
fn load(path: &Path, set_aside: bool) -> Result<Vec<Entry>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("Recently removed couldn't be read: {e}")),
    };
    if let Some(entries) = parse(&bytes) {
        return Ok(entries);
    }
    if !set_aside {
        return Err("Recently removed is damaged and can't be read. It will be set aside and started again the next time Seek removes something.".into());
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut ms = now_ms();
    let aside = loop {
        let candidate = dir.join(format!("cleanup-history.corrupt-{ms}.json"));
        if std::fs::symlink_metadata(&candidate).is_err() {
            break candidate;
        }
        ms += 1;
    };
    std::fs::rename(path, &aside)
        .map_err(|e| format!("Recently removed is damaged, and setting it aside failed: {e}"))?;
    Ok(vec![])
}

/// Writes the entries to the fixed temp file, flushes it to disk, then renames it over the history.
fn store(path: &Path, entries: &[Entry]) -> Result<(), String> {
    let bytes = serde_json::to_vec(&FileOut { version: 1, entries })
        .map_err(|e| format!("Recently removed couldn't be written: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| format!("Recently removed couldn't be written: {e}"))
}

fn is_denied(text: &str) -> bool {
    let t = text.to_lowercase();
    ["access is denied", "0x80070005", "requires elevation", "0x800702e4"].iter().any(|m| t.contains(m))
}

/// Puts one entry back. Never touches the history; the caller records a success.
pub fn put_back(entry: &Entry, bin: &dyn BinRestore) -> PutBackResult {
    let say = |restored: bool, reason: Option<String>| PutBackResult { id: entry.id.clone(), restored, reason };
    let no = |reason: String| say(false, Some(reason));
    let Some(bin_path) = entry.bin_path.as_deref() else {
        return no("It was never moved.".into());
    };
    if entry.put_back_ms.is_some() {
        return no("Already put back.".into());
    }
    let original = Path::new(&entry.original_path);
    // Anything at all there, even a broken link: restoring over it could lose it.
    if std::fs::symlink_metadata(original).is_ok() {
        return no(format!("Something new is at {}. Move it first.", original.display()));
    }
    if !bin.in_bin(bin_path) {
        return no("No longer in the Recycle Bin".into());
    }
    match bin.restore(bin_path, original, entry.time_ms) {
        Ok(()) => say(true, None),
        Err(e) if is_denied(&e) => no("Needs admin: restore it from the Recycle Bin.".into()),
        Err(e) => no(e),
    }
}

/// Puts back every listed entry, one at a time, and records each success.
pub fn put_back_ids(history: &History, bin: &dyn BinRestore, ids: &[String]) -> Vec<PutBackResult> {
    let entries = match history.try_list() {
        Ok(e) => e,
        Err(why) => {
            return ids.iter().map(|id| PutBackResult { id: id.clone(), restored: false, reason: Some(why.clone()) }).collect();
        }
    };
    ids.iter()
        .map(|id| {
            let Some(entry) = entries.iter().find(|e| &e.id == id) else {
                return PutBackResult { id: id.clone(), restored: false, reason: Some("That entry is no longer in Recently removed.".into()) };
            };
            let mut result = put_back(entry, bin);
            if result.restored {
                if let Err(e) = history.mark_put_back(id, now_ms()) {
                    result.reason = Some(format!("Put back, but Recently removed couldn't record it: {e}"));
                }
            }
            result
        })
        .collect()
}

#[cfg(test)]
mod tests;
