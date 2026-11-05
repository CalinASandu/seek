//! Real Recycle Bin spike tests. Run on purpose only:
//! `cargo test -p seek-clean --test recycle_real -- --ignored --nocapture`
//! They only ever touch temp files the test itself created, on a fixed drive.

#![cfg(windows)]

use std::path::{Path, PathBuf};

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
    if let Ok(r) = result {
        let loc = r.bin_location().expect("Ok must carry a location").to_string();
        assert!(loc.to_ascii_lowercase().contains("$recycle.bin"));
        println!("STRICT: it really was recycled to {loc}; leaving it for the shell to restore is not possible, remove by hand");
    }
}

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
