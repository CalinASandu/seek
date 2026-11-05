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
}
