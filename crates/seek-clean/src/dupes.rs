//! The duplicate finder's reading stage. Takes files that share a size and confirms true copies by
//! content: first and last 64 KB to narrow, then the full BLAKE3 hash of the whole file, which is
//! the only thing that makes two files equal.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::facts::{is_cloud, FileFacts, ATTR_DIRECTORY, ATTR_REPARSE};
use crate::model::{DupCopy, DupGroup, Lock, Why};
use crate::paths::{canon, is_inside};
use crate::protect::{lock_for, Places};

/// The narrowing read: this much from the start and from the end. It only ever narrows; two
/// files are equal only when their whole-file hashes are.
const HEAD_TAIL: usize = 64 * 1024;
/// One buffer per drive worker, reused for every file it reads.
const BUFFER: usize = 1024 * 1024;
/// Progress is sent at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

// Open flags: read the bytes where they are, never ask a cloud provider to download them, and
// never follow a link that appeared after the check.
const FILE_FLAG_OPEN_NO_RECALL: u32 = 0x0010_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

/// Anything smaller is skipped, even if handed in. A fixed minimum: small files are many and save little.
pub const MIN_DUPLICATE_BYTES: u64 = 1_048_576;

#[derive(Clone, Debug)]
pub struct SizeFile {
    pub path: PathBuf,
    pub device_id: String,
}

#[derive(Clone, Debug)]
pub struct SizeBatch {
    pub size: u64,
    pub files: Vec<SizeFile>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScanEvent {
    Group(DupGroup),
    Progress { checked_bytes: u64, total_bytes: u64, current: Option<String> },
    Done,
}

/// Confirms true copies among files that share a size. Batches run one at a time, largest first,
/// and each group is sent as soon as it is confirmed. `Done` is always sent, exactly once, last.
///
/// Reading is one long-lived worker per physical drive, each with one reused 1 MB buffer, in
/// background I/O mode and never more than one file at a time on a drive. The workers stop when
/// this returns; nothing is kept afterwards. Cancel is checked between files and between buffer
/// reads; groups already sent stay valid.
///
/// Returns false when the scan stopped on a panic of its own (`Done` is still sent): what it
/// found is then not the whole answer. Cancel is not a failure.
///
/// Only local files are handed in. Names, sizes and times come from the disk (`facts`), never
/// from the caller.
#[allow(clippy::too_many_arguments)]
pub fn scan(
    input: Vec<SizeBatch>,
    places: &Places,
    protected: &[PathBuf],
    facts: &dyn FileFacts,
    downloads_like: &[String],
    cancel: &AtomicBool,
    mut on_event: impl FnMut(ScanEvent) + Send,
) -> bool {
    let mut meter = Meter::new(&mut on_event);
    let ctx = Ctx { places, protected: protected.iter().map(|p| p.to_string_lossy().into_owned()).collect(), facts, downloads_like, cancel };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run(input, &ctx, &mut meter);
        if !cancel.load(Ordering::Relaxed) {
            meter.flush();
        }
    }));
    match outcome {
        Ok(()) => {
            meter.send(ScanEvent::Done);
            true
        }
        // The receiver itself panicked: stop, without calling it again.
        Err(payload) if meter.in_callback => resume_unwind(payload),
        // Anything else (the disk facts, preparing): the caller still gets its `Done`.
        Err(_) => {
            meter.send(ScanEvent::Done);
            false
        }
    }
}

/// A group after some copies were removed: `removed` says which paths are gone. `None` when fewer
/// than two copies are left or nothing can be freed any more, the same rules as when a group is
/// first shown. The freeable figure and the suggested keep are worked out the way `scan` does.
pub fn without_copies(group: &DupGroup, removed: impl Fn(&str) -> bool, downloads_like: &[String]) -> Option<DupGroup> {
    let mut copies: Vec<DupCopy> = group.copies.iter().filter(|c| !removed(&c.path)).cloned().collect();
    if copies.len() < 2 {
        return None;
    }
    let locks: Vec<Option<Lock>> = copies.iter().map(|c| c.lock).collect();
    let freeable = freeable(group.size, &locks);
    if freeable == 0 {
        return None;
    }
    let items: Vec<(&Path, i64)> = copies.iter().map(|c| (Path::new(c.path.as_str()), c.modified)).collect();
    let keep = keep_index(&items, &locks, downloads_like);
    let name = Path::new(&copies[keep].path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for (i, c) in copies.iter_mut().enumerate() {
        c.suggested_keep = i == keep;
    }
    Some(DupGroup { id: group.id, name, size: group.size, copies, freeable })
}

/// Which copy to suggest keeping: one that stays anyway (a locked one) is the best, so the
/// suggestion comes from the locked copies when there are any, and from all of them otherwise.
fn keep_index(items: &[(&Path, i64)], locks: &[Option<Lock>], downloads_like: &[String]) -> usize {
    let locked: Vec<usize> = (0..items.len()).filter(|&i| locks[i].is_some()).collect();
    let pool: Vec<usize> = if locked.is_empty() { (0..items.len()).collect() } else { locked };
    let picked: Vec<(&Path, i64)> = pool.iter().map(|&i| items[i]).collect();
    pool[suggest_index(&picked, downloads_like)]
}

/// Which copy to suggest keeping: one outside Downloads and temp folders, then the shortest path,
/// then the oldest, then the path itself so the answer never depends on the input order.
/// `downloads_like` entries are folder names (`Downloads`, `Temp`), matched case-insensitively
/// against whole path segments, or full folder paths. Reads each copy's modified time from the disk.
pub fn suggest_keep(copies: &[PathBuf], downloads_like: &[String]) -> usize {
    let items: Vec<(&Path, i64)> = copies.iter().map(|p| (p.as_path(), modified_on_disk(p))).collect();
    suggest_index(&items, downloads_like)
}

/// Space that can really be freed: removable copies, leaving one copy if none is locked.
/// A locked copy stays, so with one the others can all go.
pub fn freeable(size: u64, locks: &[Option<Lock>]) -> u64 {
    let unlocked = locks.iter().filter(|l| l.is_none()).count() as u64;
    let locked = locks.len() as u64 - unlocked;
    let removable = if locked > 0 { unlocked } else { unlocked.saturating_sub(1) };
    size.saturating_mul(removable)
}

struct Ctx<'a> {
    places: &'a Places,
    protected: Vec<String>,
    facts: &'a dyn FileFacts,
    downloads_like: &'a [String],
    cancel: &'a AtomicBool,
}

impl Ctx<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A file that passed the checks on the disk.
struct Cand {
    path: PathBuf,
    canon: String,
    device_id: String,
    modified_ms: i64,
    drive: String,
}

struct Prepared {
    size: u64,
    files: Vec<Cand>,
}

fn modified_on_disk(p: &Path) -> i64 {
    std::fs::symlink_metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(i64::MAX, |d| d.as_millis() as i64)
}

fn suggest_index(items: &[(&Path, i64)], downloads_like: &[String]) -> usize {
    items
        .iter()
        .enumerate()
        .min_by_key(|(i, (p, modified))| {
            let c = canon(p);
            (in_downloads_like(&c, downloads_like), c.chars().count(), *modified, c, *i)
        })
        .map_or(0, |(i, _)| i)
}

/// True if a folder on the way to the file (never the file's own name) is one of the entries.
fn in_downloads_like(canon_path: &str, entries: &[String]) -> bool {
    let mut segs: Vec<&str> = canon_path.split('\\').collect();
    segs.pop();
    entries.iter().any(|e| {
        let e = e.trim();
        if e.is_empty() {
            false
        } else if e.contains(['\\', '/', ':']) {
            is_inside(canon_path, &canon(Path::new(e))) && canon_path != canon(Path::new(e))
        } else {
            let e = e.to_lowercase();
            segs.iter().any(|s| *s == e)
        }
    })
}

/// A folder the recycle bin or the system keeps for itself, on any drive, in any case.
fn in_system_store(canon_path: &str) -> bool {
    canon_path
        .split('\\')
        .any(|s| s == "$recycle.bin" || s == "system volume information")
}

/// The physical disk a drive letter sits on, so that two letters on one disk share a worker.
/// A drive whose number can't be read falls back to its own volume.
fn physical_key(letter: char) -> String {
    let fallback = || format!("volume:{}", letter.to_ascii_lowercase());
    let wide: Vec<u16> = format!("\\\\.\\{letter}:").encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call. Access 0: the volume is only asked a question.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };
    let Ok(handle) = handle else {
        return fallback();
    };
    let mut number = STORAGE_DEVICE_NUMBER::default();
    let mut returned = 0u32;
    // SAFETY: `handle` is valid; the out buffer is `number`, sized exactly.
    let asked = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some(&mut number as *mut STORAGE_DEVICE_NUMBER as *mut c_void),
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&mut returned),
            None,
        )
    };
    // SAFETY: closing the handle opened above, once.
    unsafe {
        let _ = CloseHandle(handle);
    }
    match asked {
        Ok(()) => format!("disk:{}:{}", number.DeviceType, number.DeviceNumber),
        Err(_) => fallback(),
    }
}

fn drive_key(canon_path: &str, cache: &mut HashMap<char, String>) -> String {
    let b = canon_path.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let letter = b[0] as char;
        cache.entry(letter).or_insert_with(|| physical_key(letter)).clone()
    } else {
        // A network share or another kind of path: one worker per server and share.
        format!("path:{}", canon_path.split('\\').take(4).collect::<Vec<_>>().join("\\"))
    }
}

/// Everything decided from the disk's metadata alone: the largest sizes first, no system stores,
/// no folders, links or cloud placeholders and no file whose size isn't the batch's. A batch left
/// with fewer than two files is dropped. (Hard-linked files are refused later, on open.)
fn prepare(input: Vec<SizeBatch>, ctx: &Ctx) -> Vec<Prepared> {
    let mut input: Vec<SizeBatch> = input.into_iter().filter(|b| b.size >= MIN_DUPLICATE_BYTES).collect();
    input.sort_by(|a, b| b.size.cmp(&a.size));
    let mut merged: Vec<SizeBatch> = Vec::new();
    for b in input {
        match merged.last_mut() {
            Some(last) if last.size == b.size => last.files.extend(b.files),
            _ => merged.push(b),
        }
    }
    let mut drives = HashMap::new();
    let mut out = Vec::new();
    for batch in merged {
        let mut files = Vec::new();
        for f in batch.files {
            if ctx.cancelled() {
                return out;
            }
            let c = canon(&f.path);
            if in_system_store(&c) {
                continue;
            }
            let Some(st) = ctx.facts.stat(&f.path) else { continue };
            if st.is_dir || st.reparse || st.cloud || st.size != batch.size {
                continue;
            }
            // Hard links are refused when the file is opened (see `handle_ok`), where the link count is read.
            let drive = drive_key(&c, &mut drives);
            files.push(Cand { path: f.path, canon: c, device_id: f.device_id, modified_ms: st.modified_ms, drive });
        }
        if files.len() >= 2 {
            out.push(Prepared { size: batch.size, files });
        }
    }
    out
}

/// Progress, throttled, plus the way out for every event.
struct Meter<'a> {
    checked: u64,
    total: u64,
    current: Option<String>,
    last: Option<Instant>,
    dirty: bool,
    /// True while the receiver is running, so a panic can be told to be its own.
    in_callback: bool,
    on_event: &'a mut dyn FnMut(ScanEvent),
}

impl<'a> Meter<'a> {
    fn new(on_event: &'a mut dyn FnMut(ScanEvent)) -> Self {
        Meter { checked: 0, total: 0, current: None, last: None, dirty: false, in_callback: false, on_event }
    }

    fn send(&mut self, e: ScanEvent) {
        self.in_callback = true;
        (self.on_event)(e);
        self.in_callback = false;
    }

    /// Something changed; send it if it has been long enough since the last one.
    fn touch(&mut self) {
        self.dirty = true;
        if self.last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
            self.emit();
        }
    }

    fn emit(&mut self) {
        self.dirty = false;
        self.last = Some(Instant::now());
        let e = ScanEvent::Progress {
            checked_bytes: self.checked.min(self.total),
            total_bytes: self.total,
            current: self.current.clone(),
        };
        self.send(e);
    }

    /// Sends the last change that the throttle held back, once it is allowed.
    fn flush(&mut self) {
        if self.dirty {
            if let Some(t) = self.last {
                if let Some(wait) = PROGRESS_EVERY.checked_sub(t.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
            self.emit();
        }
    }
}

fn run(input: Vec<SizeBatch>, ctx: &Ctx, meter: &mut Meter) {
    let batches = prepare(input, ctx);
    meter.total = batches.iter().map(|b| b.size * b.files.len() as u64).sum();
    if meter.total > 0 {
        meter.touch();
    }
    std::thread::scope(|scope| {
        let mut workers: HashMap<String, Sender<Job>> = HashMap::new();
        let mut next_id = 1u32;
        for batch in &batches {
            if ctx.cancelled() {
                break;
            }
            let mut dispatch = |job: Job, drive: &str| {
                let tx = workers.entry(drive.to_string()).or_insert_with(|| {
                    let (tx, rx) = channel::<Job>();
                    let cancel = ctx.cancel;
                    let spawned = std::thread::Builder::new()
                        .name("seek-dupes-reader".into())
                        .spawn_scoped(scope, move || worker(rx, cancel));
                    if spawned.is_err() {
                        // Nothing to read with: the jobs below fail and their files are dropped.
                        let (dead, _) = channel::<Job>();
                        return dead;
                    }
                    tx
                });
                if let Err(e) = tx.send(job) {
                    let job = e.0;
                    let _ = job.reply.send(Msg::Finished { idx: job.idx, result: Err(Fail::Unreadable) });
                }
            };
            process(batch, ctx, meter, &mut next_id, &mut dispatch);
        }
        drop(workers);
    });
}

/// Narrow, then hash, then confirm, one batch.
fn process(
    batch: &Prepared,
    ctx: &Ctx,
    meter: &mut Meter,
    next_id: &mut u32,
    dispatch: &mut dyn FnMut(Job, &str),
) {
    let n = batch.files.len();
    // Nothing here could ever be removed (every file is locked): don't spend any reading on it.
    if !batch.files.iter().any(|c| lock_of(c, batch.size, ctx).is_none()) {
        meter.total -= batch.size * n as u64;
        meter.touch();
        return;
    }
    let mut counted = vec![0u64; n];

    // 1. First and last 64 KB. Files whose ends differ are not copies of each other.
    let ends = read_phase(batch, &(0..n).collect::<Vec<_>>(), Kind::Narrow, meter, &mut counted, dispatch);
    if ctx.cancelled() {
        return;
    }
    let mut buckets: HashMap<blake3::Hash, Vec<usize>> = HashMap::new();
    for (i, h) in ends.iter().enumerate() {
        if let Some(h) = h {
            buckets.entry(*h).or_default().push(i);
        }
    }
    let mut survivors: Vec<usize> = buckets.into_values().filter(|v| v.len() >= 2).flatten().collect();
    survivors.sort_unstable();
    let keep: HashSet<usize> = survivors.iter().copied().collect();
    for i in (0..n).filter(|i| !keep.contains(i)) {
        meter.total -= batch.size - counted[i];
    }
    meter.touch();

    // 2. The whole file. This is the only thing that makes two files equal.
    let hashes = read_phase(batch, &survivors, Kind::Full, meter, &mut counted, dispatch);
    if ctx.cancelled() {
        return;
    }
    let mut confirmed: HashMap<blake3::Hash, Vec<usize>> = HashMap::new();
    for &i in &survivors {
        let Some(h) = hashes[i] else {
            meter.total -= batch.size - counted[i]; // could not be read to the end
            continue;
        };
        // Changed while we were reading it? Then what we hashed may not be what is there now.
        let c = &batch.files[i];
        let same = ctx
            .facts
            .stat(&c.path)
            .is_some_and(|s| !s.is_dir && !s.reparse && !s.cloud && s.size == batch.size && s.modified_ms == c.modified_ms);
        if same {
            confirmed.entry(h).or_default().push(i);
        }
    }
    meter.touch();

    let mut sets: Vec<Vec<usize>> = confirmed.into_values().filter(|v| v.len() >= 2).collect();
    for s in &mut sets {
        s.sort_by(|a, b| batch.files[*a].canon.cmp(&batch.files[*b].canon));
    }
    sets.sort_by(|a, b| batch.files[a[0]].canon.cmp(&batch.files[b[0]].canon));
    for set in sets {
        if ctx.cancelled() {
            return;
        }
        if let Some(g) = build_group(batch, &set, *next_id, ctx) {
            *next_id += 1;
            meter.send(ScanEvent::Group(g));
        }
    }
}

/// Sends one read job per file to its drive's worker and waits for every answer, keeping the
/// meter up to date as bytes are read. `None` for a file that couldn't be read.
fn read_phase(
    batch: &Prepared,
    which: &[usize],
    kind: Kind,
    meter: &mut Meter,
    counted: &mut [u64],
    dispatch: &mut dyn FnMut(Job, &str),
) -> Vec<Option<blake3::Hash>> {
    let mut results = vec![None; batch.files.len()];
    let (tx, rx) = channel::<Msg>();
    for &idx in which {
        let c = &batch.files[idx];
        dispatch(Job { idx, path: c.path.clone(), size: batch.size, kind, reply: tx.clone() }, &c.drive);
    }
    // Only the jobs hold a sender now, so a worker that vanished ends the wait instead of hanging it.
    drop(tx);
    let mut pending = which.len();
    while pending > 0 {
        match rx.recv() {
            Ok(Msg::Read { idx, n }) => {
                counted[idx] += n;
                meter.checked += n;
                meter.current = Some(batch.files[idx].path.to_string_lossy().into_owned());
                meter.touch();
            }
            Ok(Msg::Finished { idx, result }) => {
                pending -= 1;
                results[idx] = result.ok();
            }
            Err(_) => break,
        }
    }
    results
}

/// Why this copy can't be removed, if it can't: the protected places first, then the recycle pre-checks.
fn lock_of(c: &Cand, size: u64, ctx: &Ctx) -> Option<Lock> {
    lock_for(ctx.places, &c.canon, Why::Duplicate, &ctx.protected).or_else(|| ctx.facts.recycle_block(&c.path, size))
}

fn build_group(batch: &Prepared, set: &[usize], id: u32, ctx: &Ctx) -> Option<DupGroup> {
    let size = batch.size;
    let files: Vec<&Cand> = set.iter().map(|&i| &batch.files[i]).collect();
    let locks: Vec<Option<Lock>> = files.iter().map(|c| lock_of(c, size, ctx)).collect();
    let freeable = freeable(size, &locks);
    if freeable == 0 {
        return None;
    }
    let items: Vec<(&Path, i64)> = files.iter().map(|c| (c.path.as_path(), c.modified_ms)).collect();
    let keep = keep_index(&items, &locks, ctx.downloads_like);
    let name = files[keep].path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let copies = files
        .iter()
        .zip(&locks)
        .enumerate()
        .map(|(i, (c, lock))| DupCopy {
            path: c.path.to_string_lossy().into_owned(),
            device_id: c.device_id.clone(),
            size,
            modified: c.modified_ms,
            lock: *lock,
            suggested_keep: i == keep,
        })
        .collect();
    Some(DupGroup { id, name, size, copies, freeable })
}

#[derive(Clone, Copy)]
enum Kind {
    Narrow,
    Full,
}

struct Job {
    idx: usize,
    path: PathBuf,
    size: u64,
    kind: Kind,
    reply: Sender<Msg>,
}

enum Msg {
    Read { idx: usize, n: u64 },
    Finished { idx: usize, result: Result<blake3::Hash, Fail> },
}

enum Fail {
    Cancelled,
    Unreadable,
}

/// One per physical drive, alive for the whole scan: takes jobs one at a time, so a drive is never
/// asked for two files at once. Ends when the scan drops its sender.
fn worker(jobs: Receiver<Job>, cancel: &AtomicBool) {
    // SAFETY: only changes the I/O and memory priority of this thread, which ends it with the loop.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
    let mut buf = vec![0u8; BUFFER];
    for job in jobs {
        let result = catch_unwind(AssertUnwindSafe(|| match job.kind {
            Kind::Narrow => read_ends(&job, &mut buf, cancel),
            Kind::Full => read_all(&job, &mut buf, cancel),
        }))
        .unwrap_or(Err(Fail::Unreadable));
        let _ = job.reply.send(Msg::Finished { idx: job.idx, result });
    }
    // SAFETY: as above.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
    }
}

/// What an opened handle must look like for the file to be read: an ordinary local file of the
/// size expected and with exactly one name. With more names, removing one frees nothing, so such a
/// file is never a copy at all.
fn handle_ok(attrs: u32, len: u64, expected: u64, links: u32) -> bool {
    attrs & (ATTR_DIRECTORY | ATTR_REPARSE) == 0 && !is_cloud(attrs) && len == expected && links == 1
}

/// Opens an ordinary local file for reading without asking for cloud content. The handle is
/// checked again, since the file may have been swapped since the stat.
fn open_for_read(job: &Job) -> Result<File, Fail> {
    let f = File::options()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_NO_RECALL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN)
        .open(&job.path)
        .map_err(|_| Fail::Unreadable)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is valid for as long as `f` lives and `info` is a valid out-structure.
    unsafe { GetFileInformationByHandle(HANDLE(f.as_raw_handle()), &mut info) }.map_err(|_| Fail::Unreadable)?;
    let len = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
    if !handle_ok(info.dwFileAttributes, len, job.size, info.nNumberOfLinks) {
        return Err(Fail::Unreadable);
    }
    Ok(f)
}

fn read_ends(job: &Job, buf: &mut [u8], cancel: &AtomicBool) -> Result<blake3::Hash, Fail> {
    let _ = job.reply.send(Msg::Read { idx: job.idx, n: 0 }); // names the file being read
    if cancel.load(Ordering::Relaxed) {
        return Err(Fail::Cancelled);
    }
    let mut f = open_for_read(job)?;
    let mut hasher = blake3::Hasher::new();
    let part = &mut buf[..HEAD_TAIL];
    f.read_exact(part).map_err(|_| Fail::Unreadable)?;
    hasher.update(part);
    if cancel.load(Ordering::Relaxed) {
        return Err(Fail::Cancelled);
    }
    f.seek(SeekFrom::Start(job.size - HEAD_TAIL as u64)).map_err(|_| Fail::Unreadable)?;
    f.read_exact(part).map_err(|_| Fail::Unreadable)?;
    hasher.update(part);
    Ok(hasher.finalize())
}

/// The hash of exactly `job.size` bytes. A file that ends early or keeps going has changed.
fn read_all(job: &Job, buf: &mut [u8], cancel: &AtomicBool) -> Result<blake3::Hash, Fail> {
    let _ = job.reply.send(Msg::Read { idx: job.idx, n: 0 });
    let mut f = open_for_read(job)?;
    let mut hasher = blake3::Hasher::new();
    let mut read = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Fail::Cancelled);
        }
        let n = match f.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Fail::Unreadable),
        };
        read += n as u64;
        if read > job.size {
            return Err(Fail::Unreadable);
        }
        hasher.update(&buf[..n]);
        let _ = job.reply.send(Msg::Read { idx: job.idx, n: n as u64 });
    }
    if read != job.size {
        return Err(Fail::Unreadable);
    }
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use crate::facts::{RealFacts, Stat};

    const MB: usize = 1024 * 1024;

    /// Varied bytes, so two seeds differ everywhere.
    fn data(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + usize::from(seed) * 7) % 251) as u8).collect()
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn batch(size: usize, paths: &[&PathBuf]) -> SizeBatch {
        SizeBatch {
            size: size as u64,
            files: paths
                .iter()
                .map(|p| SizeFile { path: (*p).clone(), device_id: "dev1".into() })
                .collect(),
        }
    }

    /// Nothing under a temp folder is protected by these places (they point at a drive that isn't there).
    fn places() -> Places {
        Places::for_tests(
            "q:\\windows",
            &["q:\\program files"],
            "q:\\users",
            "q:\\users\\me",
            "q:\\programdata",
            &["q:\\appdata"],
        )
    }

    /// Real disk facts, except where a test says otherwise. Never runs the real recycle check.
    #[derive(Default)]
    struct Fake {
        reparse: HashSet<PathBuf>,
        cloud: HashSet<PathBuf>,
        no_bin: HashSet<PathBuf>,
        /// Deleted right after the first look at it: gone before it is read.
        vanish: HashSet<PathBuf>,
        /// Reports a different modified time from the second stat on: it changed while being read.
        change: HashSet<PathBuf>,
        /// Panics when asked about these.
        panics: HashSet<PathBuf>,
        /// How often each path was asked about. A path read by the scan is asked about twice.
        stats: Mutex<HashMap<PathBuf, usize>>,
    }

    impl FileFacts for Fake {
        fn stat(&self, p: &Path) -> Option<Stat> {
            assert!(!self.panics.contains(p), "fake facts panic");
            let mut s = RealFacts.stat(p)?;
            if self.vanish.contains(p) {
                let _ = std::fs::remove_file(p);
            }
            s.reparse |= self.reparse.contains(p);
            s.cloud |= self.cloud.contains(p);
            let n = {
                let mut m = self.stats.lock().unwrap();
                let c = m.entry(p.to_path_buf()).or_default();
                *c += 1;
                *c
            };
            if self.change.contains(p) && n >= 2 {
                s.modified_ms += 5000;
            }
            Some(s)
        }
        fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String> {
            RealFacts.same_bytes(a, b)
        }
        fn file_id(&self, p: &Path) -> Option<(u32, u64)> {
            RealFacts.file_id(p)
        }
        fn recycle_block(&self, p: &Path, _batch_bytes: u64) -> Option<Lock> {
            self.no_bin.contains(p).then_some(Lock::NoRecycleBin)
        }
    }

    fn run_full(
        batches: Vec<SizeBatch>,
        places: &Places,
        protected: &[PathBuf],
        facts: &dyn FileFacts,
        cancel: &AtomicBool,
        mut on_event: impl FnMut(&ScanEvent) + Send,
    ) -> Vec<ScanEvent> {
        let mut events = Vec::new();
        scan(batches, places, protected, facts, &["downloads".into(), "temp".into()], cancel, |e| {
            on_event(&e);
            events.push(e);
        });
        events
    }

    fn run(batches: Vec<SizeBatch>, facts: &dyn FileFacts) -> Vec<ScanEvent> {
        run_full(batches, &places(), &[], facts, &AtomicBool::new(false), |_| {})
    }

    fn groups(events: &[ScanEvent]) -> Vec<&DupGroup> {
        events
            .iter()
            .filter_map(|e| if let ScanEvent::Group(g) = e { Some(g) } else { None })
            .collect()
    }

    fn done_count(events: &[ScanEvent]) -> usize {
        events.iter().filter(|e| matches!(e, ScanEvent::Done)).count()
    }

    fn paths_of(g: &DupGroup) -> HashSet<String> {
        g.copies.iter().map(|c| c.path.clone()).collect()
    }

    #[test]
    fn the_minimum_is_one_megabyte() {
        assert_eq!(MIN_DUPLICATE_BYTES, 1_048_576);
    }

    #[test]
    fn identical_files_form_a_group_with_the_fields_the_ui_needs() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 1);
        let (a, b) = (write(d.path(), "a.bin", &bytes), write(d.path(), "sub/b.bin", &bytes));
        let mut fake = Fake::default();
        fake.no_bin.insert(b.clone()); // a USB-like copy: locked, with its reason, at scan time
        let ev = run(vec![batch(2 * MB, &[&a, &b])], &fake);
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        let g = gs[0];
        assert_eq!(g.size, 2 * MB as u64);
        assert_eq!(g.copies.len(), 2);
        assert_eq!(paths_of(g), [a.to_string_lossy(), b.to_string_lossy()].map(|s| s.into_owned()).into());
        let ca = g.copies.iter().find(|c| Path::new(&c.path) == a).unwrap();
        let cb = g.copies.iter().find(|c| Path::new(&c.path) == b).unwrap();
        assert_eq!((ca.lock, cb.lock), (None, Some(Lock::NoRecycleBin)));
        assert_eq!(ca.device_id, "dev1");
        assert_eq!(ca.size, 2 * MB as u64);
        assert!(ca.modified > 1_600_000_000_000, "unix epoch milliseconds");
        assert_eq!(g.copies.iter().filter(|c| c.suggested_keep).count(), 1);
        let kept = g.copies.iter().find(|c| c.suggested_keep).unwrap();
        assert_eq!(g.name, Path::new(&kept.path).file_name().unwrap().to_string_lossy());
        // The locked copy stays, so the other one can go.
        assert_eq!(g.freeable, 2 * MB as u64);
        assert_eq!(done_count(&ev), 1);
        assert!(matches!(ev.last(), Some(ScanEvent::Done)));
    }

    #[test]
    fn same_size_different_contents_is_not_a_group() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (write(d.path(), "a", &data(2 * MB, 1)), write(d.path(), "b", &data(2 * MB, 2)));
        let ev = run(vec![batch(2 * MB, &[&a, &b])], &Fake::default());
        assert!(groups(&ev).is_empty());
        assert_eq!(done_count(&ev), 1);
    }

    #[test]
    fn matching_start_and_end_but_different_middle_is_not_a_group() {
        let d = tempfile::tempdir().unwrap();
        let mut one = data(3 * MB, 1);
        let two = one.clone();
        one[MB + MB / 2] ^= 0xff; // far from the first and last 64 KB
        let (a, b) = (write(d.path(), "a", &one), write(d.path(), "b", &two));
        let ev = run(vec![batch(3 * MB, &[&a, &b])], &Fake::default());
        assert!(groups(&ev).is_empty());
    }

    #[test]
    fn hard_links_are_not_duplicates() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 3);
        let a = write(d.path(), "a", &bytes);
        let link = d.path().join("link");
        std::fs::hard_link(&a, &link).unwrap();
        let ev = run(vec![batch(2 * MB, &[&a, &link])], &Fake::default());
        assert!(groups(&ev).is_empty(), "two names for one file free nothing");

        // Removing one name leaves the data, so the linked file is no copy at all: with one
        // separate identical file beside it, that file is alone and there is no group.
        let c = write(d.path(), "c", &bytes);
        let ev = run(vec![batch(2 * MB, &[&a, &link, &c])], &Fake::default());
        assert!(groups(&ev).is_empty());

        // A second separate copy makes a group of the two separate files only.
        let e = write(d.path(), "e", &bytes);
        let ev = run(vec![batch(2 * MB, &[&a, &link, &c, &e])], &Fake::default());
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(paths_of(gs[0]), [c.to_string_lossy(), e.to_string_lossy()].map(|s| s.into_owned()).into());
    }

    #[test]
    fn a_file_with_a_link_outside_the_input_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 3);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes));
        std::fs::hard_link(&a, d.path().join("elsewhere")).unwrap(); // not handed in
        let ev = run(vec![batch(2 * MB, &[&a, &b])], &Fake::default());
        assert!(groups(&ev).is_empty());
        let c = write(d.path(), "c", &bytes);
        let gs_ev = run(vec![batch(2 * MB, &[&a, &b, &c])], &Fake::default());
        let gs = groups(&gs_ev);
        assert_eq!(gs.len(), 1);
        assert!(!paths_of(gs[0]).contains(&*a.to_string_lossy()));
    }

    #[test]
    fn the_handle_check_refuses_each_unsafe_case() {
        assert!(handle_ok(0x20, 100, 100, 1), "an ordinary file passes");
        assert!(!handle_ok(0x20 | 0x400, 100, 100, 1), "reparse point");
        assert!(!handle_ok(0x10, 100, 100, 1), "folder");
        for attr in [0x1000, 0x4_0000, 0x40_0000] {
            assert!(!handle_ok(0x20 | attr, 100, 100, 1), "offline or recall attribute {attr:#x}");
        }
        assert!(!handle_ok(0x20, 101, 100, 1), "wrong size");
        assert!(!handle_ok(0x20, 100, 100, 2), "more than one link");
        assert!(!handle_ok(0x20, 100, 100, 0), "an unreadable link count is not trusted");
    }

    fn make_junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("could not run cmd to make a junction");
        assert!(
            out.status.success(),
            "mklink /J failed, so the junction tests prove nothing: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn open_for_read_refuses_a_real_junction() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("real")).unwrap();
        let junction = d.path().join("j");
        make_junction(&junction, &d.path().join("real"));
        let (tx, _rx) = channel();
        let job = Job { idx: 0, path: junction, size: 0, kind: Kind::Full, reply: tx };
        assert!(open_for_read(&job).is_err());
    }

    #[test]
    fn open_for_read_refuses_a_hard_linked_file_and_accepts_a_plain_one() {
        let d = tempfile::tempdir().unwrap();
        let a = write(d.path(), "a", &data(2 * MB, 1));
        let (tx, _rx) = channel();
        let job = |p: &Path| Job { idx: 0, path: p.to_path_buf(), size: 2 * MB as u64, kind: Kind::Full, reply: tx.clone() };
        assert!(open_for_read(&job(&a)).is_ok());
        std::fs::hard_link(&a, d.path().join("l")).unwrap();
        assert!(open_for_read(&job(&a)).is_err());
    }

    #[test]
    fn a_batch_with_nothing_removable_is_not_read() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 90);
        let (a, b, c) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes), write(d.path(), "c", &bytes));
        let mut fake = Fake::default();
        fake.no_bin.extend([a.clone(), b.clone(), c.clone()]);
        let ev = run(vec![batch(2 * MB, &[&a, &b, &c])], &fake);
        assert!(groups(&ev).is_empty());
        // Reading would stat each file a second time to check it didn't change.
        let stats = fake.stats.lock().unwrap();
        assert!(stats.values().all(|n| *n == 1), "nothing was read: {stats:?}");
        assert!(!ev.iter().any(|e| matches!(e, ScanEvent::Progress { checked_bytes, .. } if *checked_bytes > 0)));
        drop(stats);

        // Locked by the protected list as well.
        let (x, y) = (write(d.path(), "keep/x", &bytes), write(d.path(), "keep/y", &bytes));
        let fake = Fake::default();
        run_full(vec![batch(2 * MB, &[&x, &y])], &places(), &[d.path().join("keep")], &fake, &AtomicBool::new(false), |_| {});
        assert!(fake.stats.lock().unwrap().values().all(|n| *n == 1));
    }

    #[test]
    fn a_locked_copy_that_cannot_be_read_does_not_count_as_the_one_that_stays() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 91);
        let kept = write(d.path(), "keep/a", &bytes);
        let (b, c) = (write(d.path(), "b", &bytes), write(d.path(), "c", &bytes));
        let _held = std::fs::File::options().read(true).share_mode(0).open(&kept).unwrap();
        let ev = run_full(
            vec![batch(2 * MB, &[&kept, &b, &c])],
            &places(),
            &[d.path().join("keep")],
            &Fake::default(),
            &AtomicBool::new(false),
            |_| {},
        );
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].copies.len(), 2);
        assert!(gs[0].copies.iter().all(|c| c.lock.is_none()));
        assert_eq!(gs[0].freeable, 2 * MB as u64, "one of the two stays; the unreadable locked one is no copy");
    }

    fn keep_run(d: &Path, protected: &[&str], names: &[&str]) -> DupGroup {
        let bytes = data(2 * MB, 92);
        let paths: Vec<PathBuf> = names.iter().map(|n| write(d, n, &bytes)).collect();
        let refs: Vec<&PathBuf> = paths.iter().collect();
        let prot: Vec<PathBuf> = protected.iter().map(|p| d.join(p)).collect();
        let ev = run_full(vec![batch(2 * MB, &refs)], &places(), &prot, &Fake::default(), &AtomicBool::new(false), |_| {});
        groups(&ev).into_iter().next().expect("a group").clone()
    }

    #[test]
    fn the_suggested_keep_is_a_locked_copy_when_there_is_one() {
        let size = 2 * MB as u64;
        let d = tempfile::tempdir().unwrap();
        // [Locked, None, None]: the locked copy has the longest path, yet it is the one that stays.
        let g = keep_run(d.path(), &["keep"], &["keep/deeper/folder/a.bin", "b.bin", "c.bin"]);
        let kept: Vec<_> = g.copies.iter().filter(|c| c.suggested_keep).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].lock, Some(Lock::Protected));
        let tickable = g.copies.iter().filter(|c| !c.suggested_keep).count() as u64;
        assert_eq!(tickable * size, g.freeable, "ticking the others frees exactly the figure");

        // [Locked, Locked, None]: a locked copy stays; the unlocked one is tickable.
        let d = tempfile::tempdir().unwrap();
        let g = keep_run(d.path(), &["keep"], &["keep/x/a.bin", "keep/b.bin", "c.bin"]);
        let kept: Vec<_> = g.copies.iter().filter(|c| c.suggested_keep).collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].lock.is_some());
        assert!(kept[0].path.ends_with("b.bin"), "the shorter of the locked ones: {}", kept[0].path);
        let tickable: Vec<_> = g.copies.iter().filter(|c| !c.suggested_keep && c.lock.is_none()).collect();
        assert_eq!(tickable.len(), 1);
        assert_eq!(g.freeable, size);

        // [None, None]: the spec's order, here the shorter path.
        let d = tempfile::tempdir().unwrap();
        let g = keep_run(d.path(), &[], &["long/folder/a.bin", "b.bin"]);
        let kept = g.copies.iter().find(|c| c.suggested_keep).unwrap();
        assert!(kept.path.ends_with("b.bin"));
        assert_eq!(g.freeable, size);
    }

    #[test]
    fn a_panic_in_the_scan_still_ends_in_one_done() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 93);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes));
        let mut fake = Fake::default();
        fake.panics.insert(b.clone());
        let ev = run(vec![batch(2 * MB, &[&a, &b])], &fake);
        assert!(groups(&ev).is_empty());
        assert_eq!(done_count(&ev), 1);
        assert!(matches!(ev.last(), Some(ScanEvent::Done)));
    }

    #[test]
    fn a_panic_inside_on_event_does_not_send_a_second_done() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 94);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes));
        let mut seen = Vec::new();
        let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
            scan(vec![batch(2 * MB, &[&a, &b])], &places(), &[], &Fake::default(), &[], &AtomicBool::new(false), |e| {
                let is_group = matches!(e, ScanEvent::Group(_));
                seen.push(e);
                assert!(!is_group, "the receiver gave up");
            });
        }));
        assert!(r.is_err(), "the receiver's panic is not swallowed");
        assert!(seen.iter().all(|e| !matches!(e, ScanEvent::Done)), "no Done after the receiver panicked");
        assert_eq!(seen.iter().filter(|e| matches!(e, ScanEvent::Group(_))).count(), 1, "not called again");
    }

    #[test]
    fn under_the_minimum_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(MB - 1, 4);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes));
        let ev = run(vec![batch(MB - 1, &[&a, &b])], &Fake::default());
        assert!(groups(&ev).is_empty());
        assert_eq!(done_count(&ev), 1);
        // Exactly the minimum is read.
        let bytes = data(MB, 4);
        let (a, b) = (write(d.path(), "c", &bytes), write(d.path(), "d", &bytes));
        assert_eq!(groups(&run(vec![batch(MB, &[&a, &b])], &Fake::default())).len(), 1);
    }

    #[test]
    fn reparse_points_and_cloud_placeholders_are_never_read() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 5);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes));
        let (link, cloud) = (write(d.path(), "link", &bytes), write(d.path(), "cloud", &bytes));
        let mut fake = Fake::default();
        fake.reparse.insert(link.clone());
        fake.cloud.insert(cloud.clone());
        let ev = run(vec![batch(2 * MB, &[&a, &b, &link, &cloud])], &fake);
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].copies.len(), 2, "only the ordinary local files");
        // Were they read, one of them would make a group with a lone ordinary file.
        let ev = run(vec![batch(2 * MB, &[&a, &link, &cloud])], &fake);
        assert!(groups(&ev).is_empty());
    }

    #[test]
    fn a_real_junction_is_never_followed() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 6);
        let a = write(d.path(), "real/a", &bytes);
        let b = write(d.path(), "real/b", &bytes);
        let junction = d.path().join("j");
        make_junction(&junction, &d.path().join("real"));
        // The link itself is handed in beside two real copies and must never become a copy.
        let ev = run(vec![batch(2 * MB, &[&a, &b, &junction])], &RealFacts);
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].copies.len(), 2);
    }

    #[test]
    fn a_drive_maps_to_one_stable_worker_key() {
        let mut cache = HashMap::new();
        let k = drive_key("c:\\a\\b", &mut cache);
        assert!(!k.is_empty());
        assert_eq!(k, drive_key("c:\\other", &mut cache));
        assert!(drive_key("\\\\server\\share\\x\\y", &mut cache).starts_with("path:"));
    }

    #[test]
    fn copies_in_a_recycle_bin_or_system_volume_information_never_count() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 7);
        let a = write(d.path(), "a", &bytes);
        let in_bin = write(d.path(), "$RECYCLE.BIN/S-1-5-21/$Rabc.bin", &bytes);
        let in_svi = write(d.path(), "System Volume Information/x/copy.bin", &bytes);
        let ev = run(vec![batch(2 * MB, &[&a, &in_bin, &in_svi])], &Fake::default());
        assert!(groups(&ev).is_empty(), "a lone real file is not a group");

        let b = write(d.path(), "b", &bytes);
        let ev = run(vec![batch(2 * MB, &[&a, &in_bin, &b, &in_svi])], &Fake::default());
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].copies.len(), 2);
        assert!(gs[0].copies.iter().all(|c| !c.path.to_lowercase().contains("recycle") && !c.path.to_lowercase().contains("volume information")));
    }

    #[test]
    fn a_group_with_nothing_removable_is_not_shown() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 8);
        let (a, b) = (write(d.path(), "keep/a", &bytes), write(d.path(), "keep/b", &bytes));
        let ev = run_full(
            vec![batch(2 * MB, &[&a, &b])],
            &places(),
            &[d.path().join("keep")],
            &Fake::default(),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(groups(&ev).is_empty(), "both copies are protected");
        assert_eq!(done_count(&ev), 1);

        // Both locked by the pre-checks: still nothing removable.
        let (c, e) = (write(d.path(), "c", &bytes), write(d.path(), "e", &bytes));
        let mut fake = Fake::default();
        fake.no_bin.extend([c.clone(), e.clone()]);
        assert!(groups(&run(vec![batch(2 * MB, &[&c, &e])], &fake)).is_empty());
    }

    #[test]
    fn one_unlocked_copy_beside_a_locked_one_can_go() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 9);
        let (a, b) = (write(d.path(), "keep/a", &bytes), write(d.path(), "b", &bytes));
        let ev = run_full(
            vec![batch(2 * MB, &[&a, &b])],
            &places(),
            &[d.path().join("keep")],
            &Fake::default(),
            &AtomicBool::new(false),
            |_| {},
        );
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].freeable, 2 * MB as u64);
        let locked = gs[0].copies.iter().find(|c| Path::new(&c.path) == a).unwrap();
        assert_eq!(locked.lock, Some(Lock::Protected));
    }

    #[test]
    fn freeable_counts_only_what_can_really_be_freed() {
        assert_eq!(freeable(100, &[None, None, None]), 200); // one copy stays
        assert_eq!(freeable(100, &[Some(Lock::ProgramFiles), None, None]), 200); // the locked one stays
        assert_eq!(freeable(100, &[Some(Lock::ProgramFiles), Some(Lock::AppData), None]), 100);
        assert_eq!(freeable(100, &[Some(Lock::ProgramFiles), Some(Lock::AppData)]), 0);
        assert_eq!(freeable(100, &[None]), 0, "a lone copy is not a group");
        assert_eq!(freeable(100, &[]), 0);
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }
    fn dl() -> Vec<String> {
        vec!["Downloads".into(), "Temp".into()]
    }

    #[test]
    fn suggestion_prefers_outside_downloads_then_shortest_then_oldest() {
        // Outside Downloads beats a shorter path inside it.
        let c = [p("C:\\Users\\me\\Downloads\\a.bin"), p("C:\\Users\\me\\Documents\\projects\\a.bin")];
        assert_eq!(suggest_keep(&c, &dl()), 1);
        // Both outside: the shortest path.
        let c = [p("C:\\Users\\me\\Documents\\projects\\a.bin"), p("C:\\Users\\me\\Docs\\a.bin")];
        assert_eq!(suggest_keep(&c, &dl()), 1);
        // Same length: the oldest.
        let d = tempfile::tempdir().unwrap();
        let (x, y) = (write(d.path(), "x1.bin", b"1"), write(d.path(), "y1.bin", b"1"));
        let old = std::time::SystemTime::now() - Duration::from_secs(86_400 * 30);
        std::fs::File::options().write(true).open(&y).unwrap().set_modified(old).unwrap();
        assert_eq!(suggest_keep(&[x.clone(), y.clone()], &dl()), 1);
        assert_eq!(suggest_keep(&[y, x], &dl()), 0);
    }

    #[test]
    fn suggestion_matches_whole_folder_names_case_insensitively() {
        // A folder merely containing the word is not Downloads.
        let c = [p("C:\\MyDownloadsArchive\\x.bin"), p("C:\\Users\\me\\Downloads\\x.bin")];
        assert_eq!(suggest_keep(&c, &dl()), 0);
        let c = [p("C:\\users\\ME\\DOWNLOADS\\x.bin"), p("C:\\Users\\me\\Documents\\deep\\deeper\\x.bin")];
        assert_eq!(suggest_keep(&c, &dl()), 1);
        // A temp folder counts too, at any depth.
        let c = [p("C:\\Users\\me\\AppData\\Local\\Temp\\sub\\x.bin"), p("C:\\Users\\me\\Documents\\deep\\deeper\\x.bin")];
        assert_eq!(suggest_keep(&c, &dl()), 1);
        // A file called Downloads is not a folder.
        let c = [p("C:\\Users\\me\\Downloads"), p("C:\\Users\\me\\Documents\\x\\y")];
        assert_eq!(suggest_keep(&c, &dl()), 0);
        // A full folder path works as well as a bare name.
        let full = vec!["D:\\Stuff\\Incoming".to_string()];
        let c = [p("D:\\Stuff\\Incoming\\a.bin"), p("D:\\Stuff\\Incoming2\\a.bin")];
        assert_eq!(suggest_keep(&c, &full), 1);
    }

    #[test]
    fn suggestion_is_deterministic_when_everything_ties() {
        let a = [p("C:\\b\\x.bin"), p("C:\\a\\x.bin")];
        let b = [p("C:\\a\\x.bin"), p("C:\\b\\x.bin")];
        assert_eq!(a[suggest_keep(&a, &dl())], b[suggest_keep(&b, &dl())]);
        assert_eq!(suggest_keep(&[], &dl()), 0);
    }

    #[test]
    fn cancel_keeps_what_was_found() {
        let d = tempfile::tempdir().unwrap();
        let mut batches = Vec::new();
        for (i, size) in [4 * MB, 3 * MB, 2 * MB].into_iter().enumerate() {
            let bytes = data(size, i as u8 + 10);
            let (a, b) = (write(d.path(), &format!("a{i}"), &bytes), write(d.path(), &format!("b{i}"), &bytes));
            batches.push(batch(size, &[&a, &b]));
        }
        let cancel = AtomicBool::new(false);
        let ev = run_full(batches, &places(), &[], &Fake::default(), &cancel, |e| {
            if matches!(e, ScanEvent::Group(_)) {
                cancel.store(true, Ordering::SeqCst);
            }
        });
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1, "the group already sent stays, nothing after it");
        assert_eq!(gs[0].size, 4 * MB as u64);
        assert_eq!(done_count(&ev), 1);
        assert!(matches!(ev.last(), Some(ScanEvent::Done)));
    }

    #[test]
    fn done_is_sent_exactly_once_even_when_cancelled_before_the_start() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 20);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes));
        let cancel = AtomicBool::new(true);
        let ev = run_full(vec![batch(2 * MB, &[&a, &b])], &places(), &[], &Fake::default(), &cancel, |_| {});
        assert!(groups(&ev).is_empty());
        assert_eq!(done_count(&ev), 1);
        assert!(matches!(ev.last(), Some(ScanEvent::Done)));
        // And with nothing to do at all.
        let ev = run(Vec::new(), &Fake::default());
        assert_eq!(ev, vec![ScanEvent::Done]);
    }

    #[test]
    fn largest_groups_come_first() {
        let d = tempfile::tempdir().unwrap();
        let mut batches = Vec::new();
        // Handed in smallest first, on purpose.
        for (i, size) in [2 * MB, 5 * MB, 3 * MB, 4 * MB].into_iter().enumerate() {
            let bytes = data(size, i as u8 + 30);
            let (a, b) = (write(d.path(), &format!("a{i}"), &bytes), write(d.path(), &format!("b{i}"), &bytes));
            batches.push(batch(size, &[&a, &b]));
        }
        let ev = run(batches, &Fake::default());
        let sizes: Vec<u64> = groups(&ev).iter().map(|g| g.size).collect();
        let want: Vec<u64> = [5, 4, 3, 2].iter().map(|m| (m * MB) as u64).collect();
        assert_eq!(sizes, want);
        let ids: Vec<u32> = groups(&ev).iter().map(|g| g.id).collect();
        assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len(), "ids are unique");
    }

    #[test]
    fn a_file_that_vanishes_mid_scan_is_dropped_without_failing_the_scan() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 40);
        let (a, b, gone) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes), write(d.path(), "gone", &bytes));
        let mut fake = Fake::default();
        fake.vanish.insert(gone.clone());
        let ev = run(vec![batch(2 * MB, &[&a, &gone, &b]), batch(2 * MB + 1, &[&d.path().join("never-existed")])], &fake);
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].copies.len(), 2);
        assert!(!gone.exists());
        assert_eq!(done_count(&ev), 1);
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_dropped_without_failing_the_scan() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 41);
        let (a, b, held) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes), write(d.path(), "held", &bytes));
        let _lock = std::fs::File::options().read(true).share_mode(0).open(&held).unwrap();
        let ev = run(vec![batch(2 * MB, &[&a, &held, &b])], &Fake::default());
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].copies.len(), 2);
        assert_eq!(done_count(&ev), 1);
    }

    #[test]
    fn a_file_that_changes_while_being_read_is_dropped_from_its_group() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 50);
        let (a, b, c) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes), write(d.path(), "c", &bytes));
        let mut fake = Fake::default();
        fake.change.insert(c.clone());
        let ev = run(vec![batch(2 * MB, &[&a, &b, &c])], &fake);
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        assert!(!paths_of(gs[0]).contains(&*c.to_string_lossy()));
        assert_eq!(gs[0].copies.len(), 2);

        // Two files, one of them changed: no group is left.
        let mut fake = Fake::default();
        fake.change.insert(b.clone());
        assert!(groups(&run(vec![batch(2 * MB, &[&a, &b])], &fake)).is_empty());
    }

    #[test]
    fn a_file_whose_size_no_longer_matches_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 51);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &data(2 * MB + 5, 51)));
        assert!(groups(&run(vec![batch(2 * MB, &[&a, &b])], &Fake::default())).is_empty());
    }

    #[test]
    fn progress_is_throttled_bounded_and_finishes_at_the_total() {
        let d = tempfile::tempdir().unwrap();
        let mut batches = Vec::new();
        for (i, size) in [3 * MB, 2 * MB].into_iter().enumerate() {
            let bytes = data(size, i as u8 + 60);
            let (a, b) = (write(d.path(), &format!("a{i}"), &bytes), write(d.path(), &format!("b{i}"), &bytes));
            batches.push(batch(size, &[&a, &b]));
        }
        // A same-size pair that differs: dropped by narrowing, so it must leave the total.
        let (x, y) = (write(d.path(), "x", &data(4 * MB, 70)), write(d.path(), "y", &data(4 * MB, 71)));
        batches.push(batch(4 * MB, &[&x, &y]));
        let start = Instant::now();
        let mut times = Vec::new();
        let ev = run_full(batches, &places(), &[], &Fake::default(), &AtomicBool::new(false), |e| {
            if matches!(e, ScanEvent::Progress { .. }) {
                times.push(Instant::now());
            }
        });
        let elapsed = start.elapsed();
        let progress: Vec<(u64, u64, Option<String>)> = ev
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Progress { checked_bytes, total_bytes, current } => Some((*checked_bytes, *total_bytes, current.clone())),
                _ => None,
            })
            .collect();
        assert!(!progress.is_empty());
        assert!(progress.iter().all(|(c, t, _)| c <= t), "checked never passes total");
        let (c, t, _) = progress.last().unwrap();
        assert_eq!(c, t, "ends at 100%");
        assert_eq!(*t, (2 * (3 + 2)) as u64 * MB as u64, "total is what is actually read after the drops");
        assert!(progress.iter().any(|(_, _, cur)| cur.is_some()), "current names the file being read");
        for w in times.windows(2) {
            assert!(w[1] - w[0] >= Duration::from_millis(95), "no more than about ten a second");
        }
        assert!(progress.len() as u128 <= elapsed.as_millis() / 100 + 2);
        assert_eq!(groups(&ev).len(), 2);
    }

    #[test]
    fn each_copy_keeps_the_device_id_it_was_handed_in_with() {
        // Files sit on one real drive here; the device ids differ, which must not matter.
        let d = tempfile::tempdir().unwrap();
        let bytes = data(2 * MB, 80);
        let (a, b) = (write(d.path(), "a", &bytes), write(d.path(), "b", &bytes));
        let b2 = SizeBatch {
            size: 2 * MB as u64,
            files: vec![
                SizeFile { path: a, device_id: "one".into() },
                SizeFile { path: b, device_id: "two".into() },
            ],
        };
        let ev = run(vec![b2], &Fake::default());
        let gs = groups(&ev);
        assert_eq!(gs.len(), 1);
        let ids: HashSet<&str> = gs[0].copies.iter().map(|c| c.device_id.as_str()).collect();
        assert_eq!(ids, HashSet::from(["one", "two"]));
    }

    fn copy(path: &str, modified: i64, lock: Option<Lock>) -> DupCopy {
        DupCopy { path: path.into(), device_id: "d".into(), size: 5, modified, lock, suggested_keep: false }
    }

    #[test]
    fn a_group_after_removals_is_gone_below_two_copies_or_with_nothing_to_free() {
        let (dl, keep, other) = ("D:\\Downloads\\x.bin", "D:\\keep\\x.bin", "D:\\other\\longer\\x.bin");
        let g = DupGroup {
            id: 7,
            name: "x".into(),
            size: 5,
            copies: vec![copy(dl, 1, None), copy(keep, 9, None), copy(other, 2, None)],
            freeable: 10,
        };
        let same = without_copies(&g, |_| false, &["downloads".into()]).unwrap();
        assert_eq!((same.copies.len(), same.freeable), (3, 10));
        assert_eq!(same.copies.iter().filter(|c| c.suggested_keep).count(), 1);
        assert!(same.copies[1].suggested_keep, "outside Downloads, then the shortest path");

        // the suggested copy is the one removed: another takes over, and one fewer copy can be freed
        let less = without_copies(&g, |p| p == keep, &["downloads".into()]).unwrap();
        assert_eq!((less.id, less.copies.len(), less.freeable), (7, 2, 5));
        assert_eq!(less.copies.iter().filter(|c| c.suggested_keep).count(), 1);
        assert!(less.copies[1].suggested_keep && less.copies[1].path.contains("other"));

        // one copy left is no group
        assert!(without_copies(&g, |p| p != keep, &[]).is_none());

        // when every copy left is locked nothing can be freed, so there is no group to show
        let locked = DupGroup {
            copies: vec![copy("C:\\a", 1, Some(Lock::ProgramFiles)), copy("C:\\b", 1, Some(Lock::AppData)), copy("D:\\c", 1, None)],
            ..g.clone()
        };
        assert!(without_copies(&locked, |p| p == "D:\\c", &[]).is_none());
        // with the unlocked copy still there, the locked ones stay and it can go
        assert_eq!(without_copies(&locked, |_| false, &[]).unwrap().freeable, 5);
    }
}
