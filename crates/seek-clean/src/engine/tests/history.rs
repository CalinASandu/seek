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
    let said = r.history_error.as_deref().unwrap();
    assert!(said.starts_with("Moved to the Recycle Bin, but they weren't recorded."), "{said}");
    assert!(said.contains("Put them back from the Recycle Bin if needed."), "{said}");
    assert!(!said.contains("os error") && !said.to_lowercase().contains("try again"), "{said}");
}

#[test]
fn an_engine_without_history_records_nothing_and_reports_nothing() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    assert_eq!(env.engine.commit(&prep.token, &[0], &[]).unwrap().history_error, None);
}

/// An elevator that looks at the history when it is asked, then moves everything.
struct Peeking {
    history: Arc<History>,
    seen: Mutex<Option<usize>>,
}

impl Elevator for Peeking {
    fn run(&self, items: &[JobItem]) -> Result<crate::facts::AdminAnswer, String> {
        *self.seen.lock().unwrap() = Some(self.history.list().len());
        let answered = items
            .iter()
            .map(|j| ItemOutcome {
                id: j.id,
                path: j.path.clone(),
                size: j.size,
                outcome: Outcome::Moved { bin_path: r"C:\$Recycle.Bin\S\$RADMIN".into() },
            })
            .collect();
        Ok(crate::facts::AdminAnswer { items: answered, stopped_early: None })
    }
}

#[test]
fn admin_items_are_recorded_after_the_prompt_and_what_moved_before_it_is_kept_first() {
    let mut env = Env::new(true);
    let dir = tempfile::tempdir().unwrap();
    let hist = Arc::new(History::open(dir.path().join("cleanup-history.json")));
    let peek = Arc::new(Peeking { history: hist.clone(), seen: Mutex::new(None) });
    env.engine = env.engine.with_history(hist.clone());
    env.engine.elevator = Some(peek.clone());

    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let admin_folder = env.folder("pf/OldApp");
    let admin_path = env.p("pf/OldApp");
    let prep = env.engine.prepare(vec![dups(&[&a, &b]), admin_folder], &[]);
    let r = env.engine.commit(&prep.token, &[0, 2], &[]).unwrap();
    assert!(is_moved(&r, 0) && is_moved(&r, 2));
    assert_eq!(r.history_error, None);

    // When the prompt opened, the ordinary removal was already written down.
    assert_eq!(*peek.seen.lock().unwrap(), Some(1));
    let got = hist.list();
    assert_eq!(got.len(), 2);
    let admin = got.iter().find(|e| e.original_path == admin_path.to_string_lossy()).unwrap();
    assert_eq!(admin.bin_path.as_deref(), Some(r"C:\$Recycle.Bin\S\$RADMIN"));
    assert_eq!(admin.why, Why::Leftover);
    let plain = got.iter().find(|e| e.original_path == a.to_string_lossy()).unwrap();
    assert!(admin.time_ms >= plain.time_ms, "each item has its own time, the admin one stamped when the answer came back");
}

#[test]
fn only_the_items_the_admin_step_moved_are_marked_needs_admin() {
    let mut env = Env::new(true);
    let dir = tempfile::tempdir().unwrap();
    let hist = Arc::new(History::open(dir.path().join("cleanup-history.json")));
    env.engine = env.engine.with_history(hist.clone());
    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let admin_folder = env.folder("pf/OldApp");
    let admin_path = env.p("pf/OldApp");
    let prep = env.engine.prepare(vec![dups(&[&a, &b]), admin_folder], &[]);
    env.engine.commit(&prep.token, &[0, 2], &[]).unwrap();
    let got = hist.list();
    assert_eq!(got.len(), 2);
    for e in &got {
        assert_eq!(e.needs_admin, e.original_path == admin_path.to_string_lossy(), "{}", e.original_path);
    }
}

#[test]
fn a_damaged_history_set_aside_is_a_note_not_an_error() {
    let mut env = Env::new(false);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cleanup-history.json");
    std::fs::write(&file, b"this is not json").unwrap();
    env.engine = env.engine.with_history(Arc::new(History::open(&file)));
    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(is_moved(&r, 0));
    assert_eq!(r.history_error, None, "the removal was recorded");
    assert!(r.history_note.as_deref().unwrap().contains("started again"), "{:?}", r.history_note);
}

/// An elevator that makes the history unwritable (a newer Seek's file) before it answers.
struct Spoiling {
    file: std::path::PathBuf,
}

impl Elevator for Spoiling {
    fn run(&self, items: &[JobItem]) -> Result<crate::facts::AdminAnswer, String> {
        std::fs::write(&self.file, br#"{"version":2,"entries":[]}"#).unwrap();
        let answered = items
            .iter()
            .map(|j| ItemOutcome { id: j.id, path: j.path.clone(), size: j.size, outcome: Outcome::Moved { bin_path: "bin".into() } })
            .collect();
        Ok(crate::facts::AdminAnswer { items: answered, stopped_early: None })
    }
}

#[test]
fn a_failed_admin_only_append_is_an_error_and_leaves_the_ordinary_entries_in_the_history() {
    let mut env = Env::new(true);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cleanup-history.json");
    let hist = Arc::new(History::open(&file));
    env.engine = env.engine.with_history(hist.clone());
    env.engine.elevator = Some(Arc::new(Spoiling { file: file.clone() }));
    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let admin_folder = env.folder("pf/OldApp");
    let prep = env.engine.prepare(vec![dups(&[&a, &b]), admin_folder], &[]);
    let r = env.engine.commit(&prep.token, &[0, 2], &[]).unwrap();
    assert!(is_moved(&r, 0) && is_moved(&r, 2));
    assert!(r.history_error.as_deref().unwrap().contains("weren't recorded"), "{:?}", r.history_error);
    assert_eq!(r.history_note, None);
}
