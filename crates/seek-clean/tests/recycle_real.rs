//! Real Recycle Bin spike tests. Run on purpose only:
//! `cargo test -p seek-clean --test recycle_real --features spike -- --ignored --nocapture`
//! They only ever touch temp files the test itself created, on a fixed drive.
//! Tests that observe a real permanent delete need the `spike` feature; the rest run without it.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use seek_clean::engine::Engine;
use seek_clean::facts::{ClaimCheck, FileFacts, RealFacts, Recycler};
use seek_clean::model::{Candidate, Lock};
use seek_clean::protect::Places;
use seek_clean::recycle::{probe_pre_delete, recycle_one, restore};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeNameForVolumeMountPointW,
};

const DRIVE_FIXED: u32 = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A scratch dir on a fixed local drive, or a clear failure.
fn fixed_temp_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new().prefix("seek-clean-spike-").tempdir().expect("temp dir");
    let root = drive_root(dir.path());
    let kind = unsafe { GetDriveTypeW(PCWSTR(wide(&root).as_ptr())) };
    assert_eq!(kind, DRIVE_FIXED, "temp dir {} is not on a fixed drive ({root})", dir.path().display());
    dir
}

fn drive_root(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("{}\\", &s[..2])
}

#[test]
#[ignore]
fn real_recycle_reports_a_location_and_restores() {
    let dir = fixed_temp_dir();
    let file: PathBuf = dir.path().join(format!("seek-spike-{}.txt", std::process::id()));
    let body = b"seek decimator spike: this file is recycled and restored";
    std::fs::write(&file, body).unwrap();

    let report = recycle_one(&file).expect("recycle_one");
    println!("RECYCLE REPORT: {report:#?}");

    assert!(!file.exists(), "original must be gone after recycling");
    let bin = report.bin_location().expect("sink gave no new location at all").to_string();
    assert!(
        bin.to_ascii_lowercase().contains("$recycle.bin"),
        "reported location is not under $Recycle.Bin: {bin}"
    );

    if let Err(e) = restore(&bin, &file) {
        panic!(
            "RESTORE FAILED, the test file is still in the Recycle Bin and must be restored or \
             removed by hand: original {} / bin {bin} / reason: {e}",
            file.display()
        );
    }
    assert_eq!(std::fs::read(&file).unwrap(), body, "restored contents differ");
}

/// Recycle a temp file, then put it back through the real `BinRestore` (the one the app uses,
/// with the deletion-date check), and read the content again.
#[test]
#[ignore]
fn real_put_back_restores_to_its_folder() {
    use seek_clean::history::{now_ms, put_back, Entry, ShellBin};
    use seek_clean::model::Why;

    let dir = fixed_temp_dir();
    let file: PathBuf = dir.path().join(format!("seek-putback-{}.txt", std::process::id()));
    let body = b"seek decimator put back: recycled, then put back through history::put_back";
    std::fs::write(&file, body).unwrap();

    let report = recycle_one(&file).expect("recycle_one");
    let bin = report.bin_location().expect("sink gave no new location").to_string();
    assert!(!file.exists());
    assert!(seek_clean::recycle::in_bin(&bin), "the bin does not list {bin}");

    let entry = Entry {
        id: "real-test".into(),
        time_ms: now_ms(),
        original_path: file.to_string_lossy().into_owned(),
        size: body.len() as u64,
        why: Why::Duplicate,
        bin_path: Some(bin.clone()),
        left_reason: None,
        put_back_ms: None,
        needs_admin: false,
    };
    let result = put_back(&entry, &ShellBin { owner: 0 });
    assert!(
        result.restored,
        "PUT BACK FAILED, the test file is still in the Recycle Bin and must be restored or \
         removed by hand: original {} / bin {bin} / reason: {:?}",
        file.display(),
        result.reason
    );
    assert_eq!(std::fs::read(&file).unwrap(), body, "restored contents differ");
    assert!(!seek_clean::recycle::in_bin(&bin), "still listed in the bin after the restore");
}

/// Spike: what does the bin's raw `undelete` verb do when a different file already sits at the
/// original path? Bypasses Seek's own existence check on purpose, to record what Windows does.
/// Needs `--features spike`. Only temp files this test made are involved.
#[cfg(feature = "spike")]
#[test]
#[ignore]
fn real_undelete_with_a_name_conflict() {
    use seek_clean::recycle::undelete_unchecked;

    let dir = fixed_temp_dir();
    let a: PathBuf = dir.path().join(format!("seek-conflict-{}.txt", std::process::id()));
    std::fs::write(&a, b"ORIGINAL").unwrap();
    let report = recycle_one(&a).expect("recycle_one");
    let bin = report.bin_location().expect("sink gave no new location").to_string();
    assert!(!a.exists());
    std::fs::write(&a, b"NEWER-FILE").unwrap();

    // The verb may wait for a prompt that nobody sees: give it 20 seconds.
    let (tx, rx) = std::sync::mpsc::channel();
    let bin_for_verb = bin.clone();
    std::thread::spawn(move || {
        let _ = tx.send(undelete_unchecked(&bin_for_verb));
    });
    let Ok(verb) = rx.recv_timeout(std::time::Duration::from_secs(20)) else {
        println!("CONFLICT RESULT: HANGS (no answer in 20 s, even with CMIC_MASK_FLAG_NO_UI; the item is still in the bin and the verb is blocked)");
        println!("CONFLICT: clean up by hand: temp file {} (newer) and the bin item {bin}", a.display());
        std::mem::forget(dir);
        return;
    };
    println!("CONFLICT: undelete verb returned {verb:?}");
    let mut seen = Vec::new();
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let e = e.unwrap();
        let body = String::from_utf8_lossy(&std::fs::read(e.path()).unwrap_or_default()).to_string();
        println!("CONFLICT: in the folder afterwards: {} = {body:?}", e.file_name().to_string_lossy());
        seen.push((e.file_name().to_string_lossy().to_string(), body));
    }
    let still_in_bin = seek_clean::recycle::in_bin(&bin);
    println!("CONFLICT: item still in the bin: {still_in_bin}");
    let outcome = match (&verb, still_in_bin, std::fs::read(&a).ok().as_deref()) {
        (Ok(()), false, Some(b"ORIGINAL")) => "OVERWROTE the newer file with the restored one",
        (Ok(()), false, Some(b"NEWER-FILE")) => "RENAMED (restored copy went under another name; the newer file is untouched)",
        (Ok(()), true, _) => "NO-OP (verb reported success, item still in the bin)",
        (Err(_), true, _) => "REFUSED (verb failed, item still in the bin, newer file untouched)",
        _ => "OTHER, see lines above",
    };
    println!("CONFLICT RESULT: {outcome}");

    // Put things right: remove this test's own newer file, then bring back what is still in the bin.
    let _ = std::fs::remove_file(&a);
    if still_in_bin {
        if let Err(e) = restore(&bin, &a) {
            panic!("the test file is still in the Recycle Bin and must be removed by hand: {bin} ({e})");
        }
    }
}

/// MaxCapacity in MB for the volume holding `root`, from the user's bin settings (read only).
fn max_capacity_mb(root: &str) -> Option<u64> {
    let mut guid = [0u16; 64];
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(wide(root).as_ptr()), &mut guid).ok()? };
    let end = guid.iter().position(|&c| c == 0)?;
    let name = String::from_utf16_lossy(&guid[..end]); // \\?\Volume{guid}\
    let g = name.split('{').nth(1)?.split('}').next()?.to_string();
    let out = std::process::Command::new("reg")
        .args([
            "query",
            &format!("HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\BitBucket\\Volume\\{{{g}}}"),
            "/v",
            "MaxCapacity",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let hex = text.split("0x").nth(1)?.split_whitespace().next()?.to_string();
    u64::from_str_radix(&hex, 16).ok()
}

#[test]
#[ignore]
fn real_oversized_file_is_visible_to_the_sink() {
    let dir = fixed_temp_dir();
    let root = drive_root(dir.path());
    let Some(cap_mb) = max_capacity_mb(&root) else {
        println!("OVERSIZED: not reproducible here, no MaxCapacity value stored for {root}");
        return;
    };
    let mut free = 0u64;
    unsafe {
        let _ = GetDiskFreeSpaceExW(PCWSTR(wide(&root).as_ptr()), Some(&mut free), None, None);
    }
    let want = (cap_mb + 1) * 1024 * 1024;
    println!("OVERSIZED: MaxCapacity = {cap_mb} MB, free = {} MB, file = {} MB", free >> 20, want >> 20);
    if free < want + (2u64 << 30) {
        println!("OVERSIZED: not reproducible here, not enough free space to exceed MaxCapacity");
        return;
    }
    let file = dir.path().join("seek-spike-oversized.bin");
    let f = std::fs::File::create(&file).unwrap();
    f.set_len(want).unwrap();
    drop(f);

    // Probe mode refuses the item in PreDeleteItem, so nothing is deleted and the bin is untouched.
    let report = probe_pre_delete(&file).expect("probe");
    println!("OVERSIZED PRE-DELETE REPORT: {report:#?}");
    assert!(file.exists(), "probe must not delete the file");
    assert!(!report.pre_flags.is_empty(), "sink never saw PreDeleteItem");
    // A refused item reports E_ABORT and no new item, and so has no bin location.
    assert!(!report.posts.is_empty(), "sink never saw PostDeleteItem");
    for p in &report.posts {
        assert_eq!(p.hr_delete as u32, 0x8000_4004, "refused item must report E_ABORT");
        assert!(p.new_item_missing, "refused item must have no new item");
    }
    assert_eq!(report.bin_location(), None);
}

/// Creates a sparse file far bigger than the drive's bin `MaxCapacity`. A sparse file takes no
/// real disk space, so Windows sees an item that cannot fit the bin without the test needing it.
fn make_sparse_oversized(dir: &Path) -> PathBuf {
    let root = drive_root(dir);
    let cap_mb = max_capacity_mb(&root).unwrap_or(1024 * 1024);
    let len = (cap_mb * 2 + 1024) * 1024 * 1024;
    let file = dir.join("seek-spike-sparse.bin");
    std::fs::File::create(&file).unwrap();
    let out = std::process::Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(&file)
        .output()
        .expect("fsutil");
    assert!(out.status.success(), "fsutil sparse setflag failed: {}", String::from_utf8_lossy(&out.stdout));
    let f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
    f.set_len(len).unwrap();
    println!("SPARSE: {} is {} MB logical (bin MaxCapacity {cap_mb} MB)", file.display(), len >> 20);
    file
}

/// Exits the whole process if the test is still running after 60 seconds (a dialog may be up).
struct Watchdog(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Watchdog {
    fn start(label: &'static str) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let d = done.clone();
        std::thread::spawn(move || {
            for _ in 0..60 {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if d.load(Ordering::SeqCst) {
                    return;
                }
            }
            println!("WATCHDOG: {label} still running after 60 s, a dialog is probably up. Exiting.");
            std::process::exit(2);
        });
        Watchdog(done)
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Logs what happened to a test-created file after an observed recycle. If it landed in the bin,
/// puts it back and removes it, so nothing is left behind.
#[cfg(feature = "spike")]
fn settle(label: &str, file: &Path, report: &seek_clean::recycle::RecycleReport) {
    println!("{label} REPORT: {report:#?}");
    println!("{label}: original still exists = {}", file.exists());
    match report.bin_location() {
        Some(loc) => {
            println!("{label}: sink reported bin location {loc}");
            if !file.exists() {
                match restore(loc, file) {
                    Ok(()) => println!("{label}: restored from the bin"),
                    Err(e) => println!("{label}: RESTORE FAILED, remove from the bin by hand: {e}"),
                }
            }
        }
        None => println!("{label}: sink reported NO bin location"),
    }
    if file.exists() {
        std::fs::remove_file(file).expect("remove test file");
    }
}

#[cfg(feature = "spike")]
#[test]
#[ignore]
fn real_sparse_oversized_file_outcome() {
    let _wd = Watchdog::start("sparse oversized");
    let dir = fixed_temp_dir();
    let file = make_sparse_oversized(dir.path());
    let report = seek_clean::recycle::observe_recycle(&file, 0).expect("observe");
    let gone = !file.exists();
    settle("SPARSE", &file, &report);
    assert!(!report.pre_flags.is_empty(), "sink never saw PreDeleteItem");
    assert_destroyed_silently(&report, gone);
    // The strict API must refuse whatever happened if no bin location was reported.
    if gone && report.bin_location().is_none() {
        println!("SPARSE: file was destroyed without a bin location, strict recycle_one would return Err");
    }
}

#[cfg(feature = "spike")]
#[test]
#[ignore]
fn real_sparse_oversized_with_nuke_warning_outcome() {
    let _wd = Watchdog::start("sparse oversized with FOF_WANTNUKEWARNING");
    let dir = fixed_temp_dir();
    let file = make_sparse_oversized(dir.path());
    const FOF_WANTNUKEWARNING: u32 = 0x4000;
    let report = seek_clean::recycle::observe_recycle(&file, FOF_WANTNUKEWARNING).expect("observe");
    let gone = !file.exists();
    settle("SPARSE+NUKEWARNING", &file, &report);
    assert!(!report.pre_flags.is_empty(), "sink never saw PreDeleteItem");
    // Observed over 8 runs: 5 destroyed silently, 2 cancelled by Windows (0x80270000) with the
    // file intact, 1 left a dialog up until the watchdog fired. So the flag is not a reliable
    // guard. Only the invariant is asserted: a cancel never also destroys, and never reports a
    // bin location.
    assert_eq!(report.bin_location(), None);
    if report.aborted || report.perform_error.is_some() {
        assert!(!gone, "operation was cancelled but the file is gone");
    } else {
        assert_destroyed_silently(&report, gone);
    }
}

#[test]
#[ignore]
fn real_strict_recycle_one_refuses_when_no_location() {
    let _wd = Watchdog::start("strict oversized");
    let dir = fixed_temp_dir();
    let file = make_sparse_oversized(dir.path());
    let result = recycle_one(&file);
    println!("STRICT: result = {result:?}, original exists = {}", file.exists());
    if file.exists() {
        std::fs::remove_file(&file).unwrap();
    }
    // Windows destroys this file without naming a bin location, so the strict API must say Err.
    let err = result.expect_err("no bin location was reported, recycle_one must be an Err");
    assert!(!err.is_empty());
}

/// Holds `file` open with no sharing, so nothing else can open, move or delete it.
fn hold_exclusively(file: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(file)
        .expect("open with no sharing")
}

#[test]
#[ignore]
fn real_locked_file_stays_with_a_reason() {
    let _wd = Watchdog::start("locked file");
    let dir = fixed_temp_dir();
    let file = dir.path().join("seek-locked.txt");
    std::fs::write(&file, b"seek locked file").unwrap();
    let held = hold_exclusively(&file);

    let result = recycle_one(&file);
    println!("LOCKED: result = {result:?}");
    let still_there = file.exists();
    drop(held);
    let content = std::fs::read(&file).ok();
    if file.exists() {
        std::fs::remove_file(&file).unwrap();
    }

    let err = result.expect_err("a file held open with no sharing must not be reported as recycled");
    assert!(still_there, "the locked file vanished");
    assert_eq!(content.as_deref(), Some(&b"seek locked file"[..]), "contents changed");
    assert!(err.contains("seek-locked.txt"), "reason does not name the file: {err}");
    assert!(err.contains("in use"), "reason is not a plain 'in use' sentence: {err}");
    assert!(!err.contains("0x") && !err.contains("left in place"), "reason is not just a plain cause: {err}");
}

#[test]
#[ignore]
fn real_read_only_file_outcome() {
    let _wd = Watchdog::start("read-only file");
    let dir = fixed_temp_dir();
    let file = dir.path().join("seek-readonly.txt");
    std::fs::write(&file, b"seek read only").unwrap();
    let mut perms = std::fs::metadata(&file).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&file, perms).unwrap();

    let result = recycle_one(&file);
    println!("READONLY: result = {result:?}, original exists = {}", file.exists());
    let exists = file.exists();
    match &result {
        Ok(report) => {
            let loc = report.bin_location().expect("Ok carries a location").to_string();
            println!("READONLY OUTCOME: recycled to {loc}");
            assert!(!exists, "reported recycled but the original is still there");
            if let Err(e) = restore(&loc, &file) {
                println!("READONLY: RESTORE FAILED, remove from the bin by hand: {e}");
            }
        }
        Err(e) => {
            println!("READONLY OUTCOME: refused with: {e}");
            assert!(exists, "refused but the original is gone");
        }
    }
    if file.exists() {
        let mut p = std::fs::metadata(&file).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        std::fs::set_permissions(&file, p).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"seek read only", "contents changed");
        std::fs::remove_file(&file).unwrap();
    }
}

struct CountingRecycler(std::sync::atomic::AtomicUsize);
impl Recycler for CountingRecycler {
    fn recycle(&self, paths: &[PathBuf]) -> Vec<Result<String, String>> {
        self.0.fetch_add(paths.len(), std::sync::atomic::Ordering::SeqCst);
        paths.iter().map(|_| Err("should never be called".to_string())).collect()
    }
}

struct NoClaims;
impl ClaimCheck for NoClaims {
    fn is_claimed(&self, _folder: &Path) -> Option<String> {
        None
    }
}

#[test]
#[ignore]
fn real_oversized_sparse_file_is_refused_before_the_recycler() {
    let _wd = Watchdog::start("oversized end to end");
    let dir = fixed_temp_dir();
    let file = make_sparse_oversized(dir.path());
    let size = std::fs::metadata(&file).unwrap().len();

    // Fake protected places elsewhere, so only the recycle pre-checks can refuse this file.
    let fake = tempfile::tempdir().unwrap();
    let f = |s: &str| fake.path().join(s).to_string_lossy().into_owned();
    let places = Places::for_tests(&f("win"), &[&f("pf")], &f("users"), &f("users/me"), &f("pd"), &[&f("ad")]);
    let recycler = Arc::new(CountingRecycler(Default::default()));
    let engine = Engine::new(places, recycler.clone(), None, Arc::new(RealFacts), Arc::new(NoClaims));

    let blocked = RealFacts.recycle_block(&file, size);
    let prepared = engine.prepare(vec![Candidate::Duplicates { copies: vec![file.clone()] }], &[]);
    let lock = prepared.items.first().map(|i| i.lock);
    let committed = engine.commit(&prepared.token, &[0], &[]);
    let calls = recycler.0.load(std::sync::atomic::Ordering::SeqCst);
    let still_there = file.exists();
    if still_there {
        std::fs::remove_file(&file).unwrap();
    }

    println!("E2E: recycle_block = {blocked:?}, lock = {lock:?}, commit ok = {}, recycler calls = {calls}", committed.is_ok());
    assert_eq!(blocked, Some(Lock::NoRecycleBin));
    assert_eq!(lock, Some(Some(Lock::NoRecycleBin)), "the review must show the item locked");
    assert_eq!(calls, 0, "the oversized file reached the recycler");
    assert!(still_there, "the oversized file was removed");
}

#[cfg(feature = "spike")]
#[test]
#[ignore]
fn real_unc_path_outcome() {
    let _wd = Watchdog::start("unc path");
    let dir = fixed_temp_dir();
    let file = dir.path().join("seek-spike-unc.txt");
    std::fs::write(&file, b"seek spike unc").unwrap();
    let s = file.to_string_lossy().to_string();
    let unc = PathBuf::from(format!("\\\\localhost\\{}${}", &s[..1], &s[2..]));
    if std::fs::metadata(&unc).is_err() {
        println!("UNC: not reproducible here, {} is not reachable without admin", unc.display());
        return;
    }
    let report = seek_clean::recycle::observe_recycle(&unc, 0).expect("observe");
    let gone = !file.exists();
    settle("UNC", &file, &report);
    assert_destroyed_silently(&report, gone);
    // Here Windows does say so beforehand: TSF_DELETE_RECYCLE_IF_POSSIBLE (0x80) is absent.
    assert!(report.pre_flags.iter().all(|f| f & 0x80 == 0), "UNC pre flags: {:?}", report.pre_flags);
}

/// A permanent delete looks like a success to the sink: hrDelete >= 0, but no new item and so no
/// bin location, and the original is gone.
#[cfg(feature = "spike")]
fn assert_destroyed_silently(report: &seek_clean::recycle::RecycleReport, gone: bool) {
    assert!(gone, "expected the original to be destroyed");
    assert_eq!(report.bin_location(), None);
    assert!(report.perform_error.is_none() && !report.aborted);
    assert!(!report.posts.is_empty());
    for p in &report.posts {
        assert!(p.hr_delete >= 0, "hrDelete was a failure: {:#x}", p.hr_delete);
        assert!(p.new_item_missing, "a new item was reported");
    }
}

#[test]
#[ignore]
fn real_folder_with_children_is_confirmed_by_every_post() {
    let _wd = Watchdog::start("folder with children");
    let dir = fixed_temp_dir();
    let folder = dir.path().join("seek-folder");
    std::fs::create_dir_all(folder.join("nested").join("deeper")).unwrap();
    std::fs::write(folder.join("a.txt"), b"a").unwrap();
    std::fs::write(folder.join("nested").join("b.txt"), b"b").unwrap();
    std::fs::write(folder.join("nested").join("deeper").join("c.txt"), b"c").unwrap();

    let result = recycle_one(&folder);
    println!("FOLDER: result = {result:?}, original exists = {}", folder.exists());
    let exists = folder.exists();
    if let Ok(report) = &result {
        let loc = report.confirmed_location().expect("Ok is confirmed").to_string();
        println!("FOLDER: {} post(s), confirmed at {loc}", report.posts.len());
        if let Err(e) = restore(&loc, &folder) {
            println!("FOLDER: RESTORE FAILED, remove from the bin by hand: {e}");
        }
    }
    let restored = folder.join("nested").join("deeper").join("c.txt").exists();
    if folder.exists() {
        std::fs::remove_dir_all(&folder).unwrap();
    }
    assert!(result.is_ok(), "a real folder must be confirmed, or the rule needs rethinking: {result:?}");
    assert!(!exists, "reported recycled but the folder is still there");
    assert!(restored, "the folder did not come back whole");
}

/// Builds `<dir>\<name>` holding a chain of folders (50 units each at most) and a small file at
/// the end, so the longest path inside, relative to it, is `suffix` units. Returns that length.
#[cfg(feature = "spike")]
fn folder_with_inner_path(dir: &Path, name: &str, suffix: usize) -> (PathBuf, usize) {
    let folder = dir.join(name);
    let mut inner = PathBuf::new();
    let mut len = 0usize;
    let mut n = 0;
    // The directories fill `suffix - 6` units, so `<dirs>\x.txt` is exactly `suffix` long.
    let dirs_budget = suffix - r"\x.txt".len();
    while len < dirs_budget {
        let sep = if len == 0 { 0 } else { 1 };
        let seg_len = (dirs_budget - len - sep).min(50);
        inner.push(format!("{:0>seg_len$}", format!("s{n}")));
        len += sep + seg_len;
        n += 1;
    }
    let file = folder.join(&inner).join("x.txt");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"seek long inner path").unwrap();
    (folder, len + r"\x.txt".len())
}

#[cfg(feature = "spike")]
#[test]
#[ignore]
fn real_folder_with_too_long_inner_path_outcome() {
    let _wd = Watchdog::start("long inner path");
    for target in [241usize, 378] {
        let dir = fixed_temp_dir();
        let (folder, suffix) = folder_with_inner_path(dir.path(), "seek-long", target);
        let top = folder.to_string_lossy().encode_utf16().count();
        println!("LONGINNER: target {target}: folder is {top} units, longest inner path {suffix} units, full {}", top + 1 + suffix);
        let result = seek_clean::recycle::observe_recycle(&folder, 0);
        let survived = std::fs::metadata(&folder).is_ok();
        match &result {
            Ok(report) => {
                println!("LONGINNER {target}: posts = {:#?}", report.posts);
                println!("LONGINNER {target}: perform_error = {:?}, aborted = {}", report.perform_error, report.aborted);
                println!("LONGINNER {target}: bin location = {:?}", report.bin_location());
                if let Some(loc) = report.bin_location() {
                    if !survived {
                        match restore(loc, &folder) {
                            Ok(()) => println!("LONGINNER {target}: restored from the bin"),
                            Err(e) => println!("LONGINNER {target}: RESTORE FAILED, remove from the bin by hand: {e}"),
                        }
                    }
                }
            }
            Err(e) => println!("LONGINNER {target}: observe failed: {e}"),
        }
        println!("LONGINNER {target}: folder survived = {survived}, exists now = {}", std::fs::metadata(&folder).is_ok());
        if std::fs::metadata(&folder).is_ok() {
            std::fs::remove_dir_all(&folder).unwrap();
        }
    }
}

/// The size `SHQueryRecycleBinW` reports for the bin of `root` (`C:\`).
fn bin_size(root: &str) -> i64 {
    use windows::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    unsafe { SHQueryRecycleBinW(PCWSTR(wide(root).as_ptr()), &mut info) }.expect("SHQueryRecycleBinW");
    info.i64Size
}

#[test]
#[ignore]
fn real_bin_size_counts_a_moved_item_straight_away() {
    let _wd = Watchdog::start("bin size after a recycle");
    let dir = fixed_temp_dir();
    let root = drive_root(dir.path());
    let file = dir.path().join("seek-binsize.bin");
    let size = 3 * 1024 * 1024u64;
    std::fs::write(&file, vec![7u8; size as usize]).unwrap();

    let before = bin_size(&root);
    let result = recycle_one(&file);
    let after = bin_size(&root);
    println!("BINSIZE: before = {before}, after = {after}, rise = {}, file = {size}", after - before);
    // Put the file back and remove it, whatever the numbers say.
    if let Ok(report) = &result {
        let loc = report.confirmed_location().unwrap().to_string();
        if let Err(e) = restore(&loc, &file) {
            println!("BINSIZE: RESTORE FAILED, remove from the bin by hand: {e}");
        }
    }
    if file.exists() {
        std::fs::remove_file(&file).unwrap();
    }
    result.expect("recycle");
    assert!(after - before >= size as i64, "the bin size lags: rose {} for a {size} byte file", after - before);
}
