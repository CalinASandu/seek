use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use super::*;

fn moved(n: u32) -> Entry {
    Entry {
        id: format!("e{n}"),
        time_ms: 1_000 + i64::from(n),
        original_path: format!(r"C:\Users\me\docs\file{n}.txt"),
        size: 10 + u64::from(n),
        why: Why::Duplicate,
        bin_path: Some(format!(r"C:\$Recycle.Bin\S\$R{n}.txt")),
        left_reason: None,
        put_back_ms: None,
    }
}

fn left(n: u32, reason: &str) -> Entry {
    Entry { bin_path: None, left_reason: Some(reason.into()), why: Why::Leftover, ..moved(n) }
}

fn file_in(dir: &Path) -> PathBuf {
    dir.join("cleanup-history.json")
}

struct FakeBin {
    restore: Result<(), String>,
    calls: Mutex<Vec<(String, PathBuf, i64)>>,
    /// How many times the whole batch was handed over (the real bin lists once per batch).
    batches: Mutex<usize>,
}

impl FakeBin {
    fn new(restore: Result<(), String>) -> FakeBin {
        FakeBin { restore, calls: Mutex::new(vec![]), batches: Mutex::new(0) }
    }
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl BinRestore for FakeBin {
    fn restore(&self, bin_path: &str, original: &Path, near: i64) -> Result<(), String> {
        self.calls.lock().unwrap().push((bin_path.into(), original.into(), near));
        self.restore.clone()
    }
    fn restore_all(&self, jobs: &[RestoreJob]) -> Vec<Result<(), String>> {
        *self.batches.lock().unwrap() += 1;
        jobs.iter().map(|(b, o, t)| self.restore(b, o, *t)).collect()
    }
}

#[test]
fn every_removal_is_recorded_with_its_reason_and_bin_location() {
    let dir = tempfile::tempdir().unwrap();
    let h = History::open(file_in(dir.path()));
    h.append(&[moved(1), left(2, "Changed since review.")]).unwrap();
    let got = h.list();
    assert_eq!(got.len(), 2);
    // Newest first: the later one in the batch comes first.
    assert_eq!(got[0].id, "e2");
    assert_eq!(got[1].bin_path.as_deref(), Some(r"C:\$Recycle.Bin\S\$R1.txt"));
    assert_eq!(got[1].left_reason, None);
    // A reopened history (another process) sees the same.
    assert_eq!(History::open(file_in(dir.path())).list().len(), 2);
}

#[test]
fn a_failed_item_keeps_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let h = History::open(file_in(dir.path()));
    h.append(&[left(7, "It was in use.")]).unwrap();
    let e = &h.list()[0];
    assert_eq!(e.left_reason.as_deref(), Some("It was in use."));
    assert_eq!(e.bin_path, None);
    assert_eq!(e.why, Why::Leftover);
}

#[test]
fn a_corrupt_file_is_set_aside_and_a_new_one_started() {
    let dir = tempfile::tempdir().unwrap();
    let file = file_in(dir.path());
    std::fs::write(&file, b"{ this is not json").unwrap();
    let h = History::open(&file);
    // Reading says so; it does not touch the file.
    assert!(h.try_list().is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"{ this is not json");

    h.append(&[moved(1)]).unwrap();
    assert_eq!(h.list().len(), 1);
    let aside: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("cleanup-history.corrupt-") && n.ends_with(".json"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(std::fs::read(dir.path().join(&aside[0])).unwrap(), b"{ this is not json");
}

#[test]
fn a_removal_still_succeeds_if_history_cant_be_written() {
    // The parent "folder" of the history is a file, so nothing can be created in it.
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let h = History::open(blocker.join("cleanup-history.json"));
    let err = h.append(&[moved(1)]).unwrap_err();
    assert!(!err.is_empty());
    // No APPDATA: no history, and it says why.
    let none = History { target: Err("no appdata".into()), wait: LOCK_WAIT };
    assert_eq!(none.append(&[moved(1)]).unwrap_err(), "no appdata");
    assert!(none.try_list().is_err());
    assert!(none.list().is_empty());
}

#[test]
fn two_writers_lose_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = file_in(dir.path());
    let writers = 4;
    let each = 15;
    let gate = Arc::new(Barrier::new(writers));
    let threads: Vec<_> = (0..writers)
        .map(|w| {
            let file = file.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                // Each writer has its own handle on the file, like separate processes.
                let h = History::open(file);
                gate.wait();
                for i in 0..each {
                    h.append(&[moved((w * 1000 + i) as u32)]).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let got = History::open(&file).list();
    assert_eq!(got.len(), writers * each);
    let mut ids: Vec<_> = got.iter().map(|e| e.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), writers * each);
}

#[test]
fn history_is_capped_at_ten_thousand_entries_keeping_the_newest() {
    let dir = tempfile::tempdir().unwrap();
    let h = History::open(file_in(dir.path()));
    let all: Vec<Entry> = (0..MAX_ENTRIES as u32 + 25).map(moved).collect();
    h.append(&all[..6_000]).unwrap();
    h.append(&all[6_000..]).unwrap();
    let got = h.list();
    assert_eq!(got.len(), MAX_ENTRIES);
    assert_eq!(got[0].id, format!("e{}", MAX_ENTRIES + 24), "the newest is kept");
    assert_eq!(got[MAX_ENTRIES - 1].id, "e25", "the 25 oldest fell off");
}

#[test]
fn put_back_says_no_longer_in_the_recycle_bin_when_it_was_emptied() {
    // The bin says so itself; nothing else is asked first.
    let bin = FakeBin::new(Err("No longer in the Recycle Bin.".into()));
    let r = put_back(&moved(1), &bin);
    assert!(!r.restored);
    assert_eq!(r.reason.as_deref(), Some("No longer in the Recycle Bin."));
    // A bin that can't be listed is never reported as "no longer in the bin".
    let broken = FakeBin::new(Err("Couldn't read the Recycle Bin: The device is not ready.".into()));
    let r = put_back(&moved(1), &broken);
    assert_eq!(r.reason.as_deref(), Some("Couldn't read the Recycle Bin: The device is not ready."));
}

#[test]
fn put_back_refuses_when_something_new_is_at_the_original_path() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("report.txt");
    std::fs::write(&original, b"new").unwrap();
    let e = Entry { original_path: original.to_string_lossy().into_owned(), ..moved(1) };
    let bin = FakeBin::new(Ok(()));
    let r = put_back(&e, &bin);
    assert!(!r.restored);
    assert_eq!(r.reason, Some(format!("Something new is at {}. Move it first.", original.display())));
    assert_eq!(bin.calls(), 0, "restore must not run");
    assert_eq!(std::fs::read(&original).unwrap(), b"new");
}

#[test]
fn put_back_explains_every_other_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("a.txt");
    let e = Entry { original_path: original.to_string_lossy().into_owned(), ..moved(1) };

    assert_eq!(put_back(&left(2, "x"), &FakeBin::new(Ok(()))).reason.as_deref(), Some("It was never moved."));
    let done = Entry { put_back_ms: Some(5), ..e.clone() };
    assert_eq!(put_back(&done, &FakeBin::new(Ok(()))).reason.as_deref(), Some("Already put back."));

    for denied in ["undelete verb failed: Access is denied. (0x80070005)", "the requested operation requires elevation. (0x800702E4)"] {
        let r = put_back(&e, &FakeBin::new(Err(denied.into())));
        assert_eq!(r.reason.as_deref(), Some("Needs admin: restore it from the Recycle Bin."), "{denied}");
    }
    let r = put_back(&e, &FakeBin::new(Err("The item in the Recycle Bin is from a different removal.".into())));
    assert_eq!(r.reason.as_deref(), Some("The item in the Recycle Bin is from a different removal."));

    // Success passes the entry's time on, for the deletion-date check.
    let bin = FakeBin::new(Ok(()));
    let r = put_back(&e, &bin);
    assert!(r.restored && r.reason.is_none());
    assert_eq!(bin.calls.lock().unwrap()[0].2, e.time_ms);
}

#[test]
fn a_put_back_is_recorded_so_the_sheet_shows_it_next_time() {
    let dir = tempfile::tempdir().unwrap();
    let h = History::open(file_in(dir.path()));
    let original = dir.path().join("gone.txt");
    let e = Entry { original_path: original.to_string_lossy().into_owned(), ..moved(1) };
    h.append(&[e.clone(), moved(2)]).unwrap();

    let bin = FakeBin::new(Ok(()));
    let out = put_back_ids(&h, &bin, &["e1".to_string(), "nope".to_string()]);
    assert!(out[0].restored);
    assert!(!out[1].restored && out[1].reason.is_some());
    let list = h.list();
    assert!(list.iter().find(|x| x.id == "e1").unwrap().put_back_ms.is_some());
    assert!(list.iter().find(|x| x.id == "e2").unwrap().put_back_ms.is_none());
    // A second try is refused without asking the bin again.
    let again = put_back_ids(&h, &bin, &["e1".to_string()]);
    assert_eq!(again[0].reason.as_deref(), Some("Already put back."));
    assert_eq!(bin.calls(), 1);
}

#[test]
fn ids_are_unique() {
    let mut ids: Vec<String> = (0..1000).map(|_| new_id()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 1000);
}

#[test]
fn a_batch_asks_the_bin_once_for_everything_that_can_be_restored() {
    let dir = tempfile::tempdir().unwrap();
    let h = History::open(file_in(dir.path()));
    let at = |n: u32| Entry { original_path: dir.path().join(format!("f{n}.txt")).to_string_lossy().into_owned(), ..moved(n) };
    // Entry 3 has something new at its path, so it never reaches the bin.
    std::fs::write(dir.path().join("f3.txt"), b"new").unwrap();
    h.append(&[at(1), at(2), at(3)]).unwrap();
    let bin = FakeBin::new(Ok(()));
    let ids: Vec<String> = ["e1", "e2", "e3"].map(String::from).to_vec();
    let out = put_back_ids(&h, &bin, &ids);
    assert_eq!(*bin.batches.lock().unwrap(), 1);
    assert_eq!(bin.calls(), 2);
    assert!(out[0].restored && out[1].restored && !out[2].restored);
}

#[test]
fn a_put_back_that_could_not_be_recorded_keeps_its_note() {
    let dir = tempfile::tempdir().unwrap();
    let h = History::open(file_in(dir.path()));
    let e = Entry { original_path: dir.path().join("x.txt").to_string_lossy().into_owned(), ..moved(1) };
    h.append(&[e]).unwrap();
    // The history turns damaged between listing and recording.
    struct Spoil(PathBuf);
    impl BinRestore for Spoil {
        fn restore(&self, _: &str, _: &Path, _: i64) -> Result<(), String> {
            std::fs::write(&self.0, b"garbage").unwrap();
            Ok(())
        }
    }
    let out = put_back_ids(&h, &Spoil(file_in(dir.path())), &["e1".to_string()]);
    assert!(out[0].restored);
    assert!(out[0].reason.as_deref().unwrap().starts_with("Recently removed couldn't record it."));
}

#[test]
fn a_file_from_a_newer_seek_is_never_changed_or_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let file = file_in(dir.path());
    let body = br#"{"version":2,"entries":[{"future":true}]}"#;
    std::fs::write(&file, body).unwrap();
    let h = History::open(&file);
    let want = "Recently removed was written by a newer Seek, so this one won't change it.";
    assert_eq!(h.append(&[moved(1)]).unwrap_err(), want);
    assert_eq!(h.mark_put_back("e1", 5).unwrap_err(), want);
    assert!(h.try_list().is_err());
    assert_eq!(std::fs::read(&file).unwrap(), body);
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.contains("corrupt")), "{names:?}");
}

#[test]
fn setting_a_damaged_file_aside_is_reported_with_the_new_name() {
    let dir = tempfile::tempdir().unwrap();
    let file = file_in(dir.path());
    std::fs::write(&file, b"nope").unwrap();
    let h = History::open(&file);
    let note = h.append(&[moved(1)]).unwrap().expect("a note");
    assert!(note.starts_with("Recently removed couldn't be read, so it was started again. The old file was kept as cleanup-history.corrupt-"), "{note}");
    assert!(note.ends_with(".json."), "{note}");
    assert_eq!(h.list().len(), 1);
    // A healthy file gives no note.
    assert_eq!(h.append(&[moved(2)]).unwrap(), None);
}

#[test]
fn a_busy_file_is_not_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let file = file_in(dir.path());
    std::fs::write(&file, b"nope").unwrap();
    let mut h = History::open(&file);
    h.wait = Duration::from_millis(100);
    let held = lock(&file, Duration::from_secs(1)).unwrap();
    let err = h.append(&[moved(1)]).unwrap_err();
    assert!(err.contains("busy"), "{err}");
    assert!(!err.to_lowercase().contains("try again"), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), b"nope");
    drop(held);
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.contains("corrupt")), "{names:?}");
}

#[test]
fn an_existing_set_aside_name_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let file = file_in(dir.path());
    std::fs::write(&file, b"nope").unwrap();
    // Every name the next few hundred milliseconds could pick is taken already.
    let start = now_ms() - 10;
    for ms in start..start + 600 {
        std::fs::write(dir.path().join(format!("cleanup-history.corrupt-{ms}.json")), b"keep").unwrap();
    }
    History::open(&file).append(&[moved(1)]).unwrap();
    let mut fresh = 0;
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let e = e.unwrap();
        if e.file_name().to_string_lossy().starts_with("cleanup-history.corrupt-") {
            match std::fs::read(e.path()).unwrap().as_slice() {
                b"keep" => {}
                b"nope" => fresh += 1,
                other => panic!("{:?} changed to {other:?}", e.path()),
            }
        }
    }
    assert_eq!(fresh, 1);
}

#[test]
fn errors_read_in_plain_words() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let err = History::open(blocker.join("cleanup-history.json")).append(&[moved(1)]).unwrap_err();
    assert!(!err.contains("os error"), "{err}");
    let raw = std::io::Error::from_raw_os_error(5);
    assert!(raw.to_string().contains("os error 5"));
    assert_eq!(plain_io(&raw), "Access is denied.");
}
