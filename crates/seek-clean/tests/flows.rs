//! End-to-end flows: the real engine, the real Recycle Bin and the real put back, on temp folders
//! only. Run on purpose: `cargo test -p seek-clean --test flows -- --ignored --nocapture`.
//! Everything a flow recycles is put back before it ends (also when it fails), and nothing but
//! files these tests created is ever touched.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use seek_clean::engine::Engine;
use seek_clean::facts::{ClaimCheck, RealFacts};
use seek_clean::history::{put_back_ids, History, ShellBin};
use seek_clean::model::{Candidate, Level, Lock, Outcome, Reason, Why};
use seek_clean::protect::Places;
use seek_clean::recycle::ShellRecycler;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetDriveTypeW;

const DRIVE_FIXED: u32 = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A scratch dir on a fixed local drive, or a clear failure.
fn fixed_temp_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new().prefix("seek-clean-flow-").tempdir().expect("temp dir");
    let s = dir.path().to_string_lossy().into_owned();
    let root = format!("{}\\", &s[..2]);
    let kind = unsafe { GetDriveTypeW(PCWSTR(wide(&root).as_ptr())) };
    assert_eq!(kind, DRIVE_FIXED, "temp dir {s} is not on a fixed drive ({root})");
    dir
}

struct NoClaims;
impl ClaimCheck for NoClaims {
    fn is_claimed(&self, _folder: &Path) -> Option<String> {
        None
    }
}

/// Everything a flow needs. Field order matters: `_cleanup` drops before `dir`, so what is still
/// in the bin is put back while its folder still exists.
struct Flow {
    engine: Engine,
    history: Arc<History>,
    _cleanup: Cleanup,
    dir: tempfile::TempDir,
}

/// On drop (a failed flow included): puts back every entry of this flow still in the bin, and
/// prints the names of any it could not, so they can be found by hand.
struct Cleanup(Arc<History>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let entries = self.0.list();
        let ids: Vec<String> = entries
            .iter()
            .filter(|e| e.bin_path.is_some() && e.put_back_ms.is_none())
            .map(|e| e.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        for r in put_back_ids(&self.0, &ShellBin { owner: 0 }, &ids) {
            if !r.restored {
                let e = entries.iter().find(|e| e.id == r.id).unwrap();
                println!(
                    "CLEANUP FAILED, still in the Recycle Bin, remove by hand: {} (bin {:?}): {:?}",
                    e.original_path, e.bin_path, r.reason
                );
            }
        }
    }
}

fn flow(scan_root: Option<&str>) -> Flow {
    let dir = fixed_temp_dir();
    // Fake protected places elsewhere, so nothing under the temp dir counts as protected.
    let fake = tempfile::tempdir().unwrap();
    let f = |s: &str| fake.path().join(s).to_string_lossy().into_owned();
    let mut places = Places::for_tests(&f("win"), &[&f("pf")], &f("users"), &f("users/me"), &f("pd"), &[&f("ad")]);
    if let Some(root) = scan_root {
        places = places.with_scan_root_for_tests(root);
    }
    let history = Arc::new(History::open(dir.path().join("history").join("cleanup-history.json")));
    let engine = Engine::new(places, Arc::new(ShellRecycler), None, Arc::new(RealFacts), Arc::new(NoClaims))
        .with_history(history.clone());
    Flow { engine, history: history.clone(), _cleanup: Cleanup(history), dir }
}

#[test]
#[ignore]
fn duplicate_group_of_three_remove_two_put_both_back() {
    let fl = flow(None);
    let body = b"seek flows: three identical files";
    let copies: Vec<PathBuf> = (0..3).map(|i| fl.dir.path().join(format!("flow-dup-{i}.txt"))).collect();
    for c in &copies {
        std::fs::write(c, body).unwrap();
    }

    let prepared = fl.engine.prepare(vec![Candidate::Duplicates { copies: copies.clone() }], &[]);
    assert_eq!(prepared.items.len(), 3);
    for i in &prepared.items {
        assert_eq!(i.lock, None, "{} locked: {:?}", i.path, i.lock);
    }
    let result = fl.engine.commit(&prepared.token, &[0, 1], &[]).expect("commit");
    println!("DUP: commit = {result:#?}");
    assert_eq!(result.history_error, None);
    assert!(result.items.iter().all(|i| matches!(i.outcome, Outcome::Moved { .. })), "both must move");
    assert!(!copies[0].exists() && !copies[1].exists(), "two copies must be gone");
    assert!(copies[2].exists(), "one copy must stay");

    let entries = fl.history.list();
    assert_eq!(entries.len(), 2, "history must hold exactly two entries");
    for e in &entries {
        assert!(e.bin_path.as_deref().is_some_and(|b| !b.is_empty()), "entry without a bin path: {e:?}");
        assert_eq!(e.why, Why::Duplicate);
    }

    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    let back = put_back_ids(&fl.history, &ShellBin { owner: 0 }, &ids);
    println!("DUP: put back = {back:#?}");
    assert!(back.iter().all(|r| r.restored), "put back failed: {back:#?}");
    for c in &copies {
        assert_eq!(std::fs::read(c).unwrap(), body, "{} differs", c.display());
    }
    let after = fl.history.list();
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|e| e.put_back_ms.is_some()), "history must mark both as put back");
}

fn mklink_junction(link: &Path, target: &Path) {
    let out = std::process::Command::new("cmd")
        .arg("/c")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .expect("cmd mklink");
    assert!(out.status.success(), "mklink /J failed: {}", String::from_utf8_lossy(&out.stdout));
}

#[test]
#[ignore]
fn leftover_with_inner_junction_goes_and_comes_back() {
    let scan = fixed_temp_dir();
    let scan_root = scan.path().join("scanroot");
    std::fs::create_dir_all(&scan_root).unwrap();
    let fl = flow(Some(&scan_root.to_string_lossy()));

    let leftover = scan_root.join("SeekFlowLeftover");
    std::fs::create_dir_all(leftover.join("sub")).unwrap();
    std::fs::write(leftover.join("sub").join("nested.txt"), b"nested file").unwrap();

    let target = fixed_temp_dir();
    let marker = target.path().join("marker.txt");
    std::fs::write(&marker, b"junction target marker").unwrap();
    let junction = leftover.join("link");
    mklink_junction(&junction, target.path());
    assert!(marker.exists() && junction.join("marker.txt").exists());

    let cand = Candidate::Folder {
        path: leftover.clone(),
        why: Why::Leftover,
        level: Some(Level::Likely),
        reasons: vec![Reason { text: "flow".into(), points: 1 }],
    };
    let prepared = fl.engine.prepare(vec![cand], &[]);
    assert_eq!(prepared.items.len(), 1);
    let lock = prepared.items[0].lock;
    println!("LEFTOVER: lock at prepare = {lock:?}");
    let result = fl.engine.commit(&prepared.token, &[0], &[]).expect("commit");
    println!("LEFTOVER: commit = {result:#?}");

    // Never allowed to touch the junction's target, whatever the engine decided.
    assert_eq!(std::fs::read(&marker).unwrap(), b"junction target marker", "target marker changed");

    match lock {
        Some(l) => {
            println!("LEFTOVER BEHAVIOUR: locked with {l:?}, nothing moved");
            assert_eq!(l, Lock::ReparsePoint);
            assert!(matches!(result.items[0].outcome, Outcome::Left { .. }));
            assert!(leftover.join("sub").join("nested.txt").exists(), "a locked folder must not move");
            assert!(junction.join("marker.txt").exists());
        }
        None => {
            println!("LEFTOVER BEHAVIOUR: not locked, the folder was moved");
            assert!(matches!(result.items[0].outcome, Outcome::Moved { .. }), "{result:#?}");
            assert!(!leftover.exists(), "the folder must be gone from its place");
            assert_eq!(std::fs::read(&marker).unwrap(), b"junction target marker");

            let ids: Vec<String> = fl.history.list().iter().map(|e| e.id.clone()).collect();
            assert_eq!(ids.len(), 1);
            let back = put_back_ids(&fl.history, &ShellBin { owner: 0 }, &ids);
            println!("LEFTOVER: put back = {back:#?}");
            assert!(back.iter().all(|r| r.restored), "put back failed: {back:#?}");

            assert_eq!(std::fs::read(leftover.join("sub").join("nested.txt")).unwrap(), b"nested file");
            let md = std::fs::symlink_metadata(&junction).expect("junction restored");
            assert!(md.file_type().is_symlink() || {
                use std::os::windows::fs::MetadataExt;
                md.file_attributes() & 0x400 != 0
            }, "restored junction is not a reparse point");
            let link_to = std::fs::canonicalize(&junction).unwrap();
            assert_eq!(link_to, std::fs::canonicalize(target.path()).unwrap(), "junction points elsewhere");
            assert_eq!(std::fs::read(&marker).unwrap(), b"junction target marker");
        }
    }
    // Remove the junction itself first (never its target's contents) so the temp drop is clean.
    if std::fs::symlink_metadata(&junction).is_ok() {
        let _ = std::fs::remove_dir(&junction);
    }
}
