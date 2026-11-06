//! The engine writes every commit into Recently removed, and a history that can't be written
//! never undoes a removal.

use super::*;
use crate::history::History;

#[test]
fn a_commit_records_one_entry_per_item_moved_or_left() {
    let mut env = Env::new(false);
    let dir = tempfile::tempdir().unwrap();
    let hist = Arc::new(History::open(dir.path().join("cleanup-history.json")));
    env.engine = env.engine.with_history(hist.clone());

    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let folder = env.folder("left/OldApp");
    let folder_path = env.p("left/OldApp");
    env.rec.refuse.lock().unwrap().insert(folder_path.clone());
    let prep = env.engine.prepare(vec![dups(&[&a, &b]), folder], &[]);
    // Items: 0 = a, 1 = b, 2 = the folder. Move a (b stays as its twin); the folder is refused.
    let r = env.engine.commit(&prep.token, &[0, 2], &[]).unwrap();
    assert_eq!(r.history_error, None);
    assert!(is_moved(&r, 0));
    assert!(!left(&r, 2).is_empty());

    let got = hist.list();
    assert_eq!(got.len(), 2, "{got:?}");
    let moved = got.iter().find(|e| e.original_path == a.to_string_lossy()).unwrap();
    assert_eq!(moved.why, Why::Duplicate);
    assert!(moved.bin_path.as_deref().unwrap().contains("$Recycle.Bin"));
    assert_eq!(moved.left_reason, None);
    assert_eq!(moved.size, 2);
    let stayed = got.iter().find(|e| e.original_path == folder_path.to_string_lossy()).unwrap();
    assert_eq!(stayed.why, Why::Leftover);
    assert_eq!(stayed.bin_path, None);
    assert!(stayed.left_reason.as_deref().unwrap().contains("Recycle Bin"));
    assert_ne!(moved.id, stayed.id);
    assert!(moved.time_ms > 0 && moved.put_back_ms.is_none());
}

#[test]
fn a_history_that_cant_be_written_says_so_but_the_items_are_still_moved() {
    let mut env = Env::new(false);
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    env.engine = env.engine.with_history(Arc::new(History::open(blocker.join("cleanup-history.json"))));

    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(is_moved(&r, 0));
    assert!(!a.exists() && b.exists(), "the removal went ahead");
    assert!(r.history_error.as_deref().is_some_and(|e| !e.is_empty()), "{:?}", r.history_error);
}

#[test]
fn an_engine_without_history_records_nothing_and_reports_nothing() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    assert_eq!(env.engine.commit(&prep.token, &[0], &[]).unwrap().history_error, None);
}
