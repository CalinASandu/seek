use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::facts::RealFacts;
use crate::model::{Level, Reason};
use crate::paths::make_junction;

/// Recycles by really deleting (inside the temp folder only), so later checks see the effect.
#[derive(Default)]
struct FakeRecycler {
    calls: Mutex<Vec<PathBuf>>,
    /// Err, file stays (in use, access denied).
    refuse: Mutex<HashSet<PathBuf>>,
    /// On this call number (1-based): delete the file and return Err, as a silent permanent delete would.
    destroy_at: Mutex<Option<usize>>,
    /// On this call number: delete one file inside the folder and return Err (a half-done folder).
    partial_at: Mutex<Option<usize>>,
    /// The first call waits for this flag, then lingers, so a second commit can overlap it.
    gate: Mutex<Option<Arc<AtomicBool>>>,
}

impl FakeRecycler {
    fn calls(&self) -> Vec<PathBuf> {
        self.calls.lock().unwrap().clone()
    }
}

impl Recycler for FakeRecycler {
    fn recycle(&self, paths: &[PathBuf]) -> Vec<Result<String, String>> {
        let mut out = Vec::new();
        for p in paths {
            let n = {
                let mut c = self.calls.lock().unwrap();
                c.push(p.clone());
                c.len()
            };
            let remove = |p: &Path| {
                if p.is_dir() {
                    std::fs::remove_dir_all(p).unwrap();
                } else {
                    std::fs::remove_file(p).unwrap();
                }
            };
            let gate = self.gate.lock().unwrap().clone();
            if let (1, Some(flag)) = (n, gate) {
                let start = Instant::now();
                while !flag.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(3) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            if *self.partial_at.lock().unwrap() == Some(n) {
                let first = std::fs::read_dir(p)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                std::fs::remove_file(first).unwrap();
                out.push(Err("half done".to_string()));
                break;
            }
            if *self.destroy_at.lock().unwrap() == Some(n) {
                remove(p);
                out.push(Err("lost".to_string()));
                break;
            }
            if self.refuse.lock().unwrap().contains(p) {
                out.push(Err("in use".to_string()));
                continue;
            }
            remove(p);
            out.push(Ok(format!(r"C:\$Recycle.Bin\S\$R{n}")));
        }
        out
    }
}

#[derive(Default)]
struct FakeClaims(Mutex<HashSet<PathBuf>>);
impl ClaimCheck for FakeClaims {
    fn is_claimed(&self, folder: &Path) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .contains(folder)
            .then(|| "SomeApp".to_string())
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum ElevMode {
    #[default]
    All,
    Fail,
    OnlyFirst,
}

#[derive(Default)]
struct FakeElevator {
    calls: Mutex<Vec<Vec<JobItem>>>,
    mode: Mutex<ElevMode>,
}
impl Elevator for FakeElevator {
    fn run(&self, items: &[JobItem]) -> Result<Vec<ItemOutcome>, String> {
        self.calls.lock().unwrap().push(items.to_vec());
        let mode = *self.mode.lock().unwrap();
        if mode == ElevMode::Fail {
            return Err("prompt refused".into());
        }
        let take = if mode == ElevMode::OnlyFirst {
            1
        } else {
            items.len()
        };
        Ok(items
            .iter()
            .take(take)
            .map(|j| ItemOutcome {
                id: j.id,
                path: j.path.clone(),
                size: j.size,
                outcome: Outcome::Moved {
                    bin_path: "bin".into(),
                },
            })
            .collect())
    }
}

/// The real `FileFacts` with hooks, so branches the real one rarely takes can be run.
#[derive(Default)]
struct Wrap {
    /// `recycle_block` answers `NoRecycleBin` for these paths.
    block: Mutex<HashSet<PathBuf>>,
    /// The `batch_bytes` each `recycle_block` call was given.
    batches: Mutex<Vec<u64>>,
    same_bytes_fails: AtomicBool,
    after_compare: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl FileFacts for Wrap {
    fn stat(&self, p: &Path) -> Option<Stat> {
        RealFacts.stat(p)
    }
    fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String> {
        if self.same_bytes_fails.load(Ordering::SeqCst) {
            return Err("cannot read".into());
        }
        let r = RealFacts.same_bytes(a, b);
        if let Some(f) = self.after_compare.lock().unwrap().as_ref() {
            f();
        }
        r
    }
    fn file_id(&self, p: &Path) -> Option<(u32, u64)> {
        RealFacts.file_id(p)
    }
    fn recycle_block(&self, p: &Path, batch_bytes: u64) -> Option<Lock> {
        self.batches.lock().unwrap().push(batch_bytes);
        self.block
            .lock()
            .unwrap()
            .contains(p)
            .then_some(Lock::NoRecycleBin)
    }
}

struct Env {
    dir: tempfile::TempDir,
    rec: Arc<FakeRecycler>,
    claims: Arc<FakeClaims>,
    elev: Arc<FakeElevator>,
    wrap: Arc<Wrap>,
    engine: Engine,
}

impl Env {
    fn new(with_elevator: bool) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path();
        let s = |p: &str| b.join(p).to_string_lossy().into_owned();
        for d in [
            "win",
            "pf",
            "pd",
            "users/me/docs",
            "users/me/appdata",
            "users/other",
            "left",
        ] {
            std::fs::create_dir_all(b.join(d)).unwrap();
        }
        let places = Places::for_tests(
            &s("win"),
            &[&s("pf")],
            &s("users"),
            &s("users/me"),
            &s("pd"),
            &[&s("users/me/appdata")],
        );
        let rec = Arc::new(FakeRecycler::default());
        let claims = Arc::new(FakeClaims::default());
        let elev = Arc::new(FakeElevator::default());
        let elevator: Option<Arc<dyn Elevator>> = if with_elevator {
            Some(elev.clone() as Arc<dyn Elevator>)
        } else {
            None
        };
        let wrap = Arc::new(Wrap::default());
        let engine = Engine::new(places, rec.clone(), elevator, wrap.clone(), claims.clone());
        Env {
            dir,
            rec,
            claims,
            elev,
            wrap,
            engine,
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        // Backslashes only: `mklink` reads a `/` as a switch.
        self.dir.path().join(rel.replace('/', "\\"))
    }

    fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = self.p(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn folder(&self, rel: &str) -> Candidate {
        let p = self.p(rel);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("data.bin"), b"some data").unwrap();
        Candidate::Folder {
            path: p,
            why: Why::Leftover,
            level: Some(Level::Likely),
            reasons: vec![Reason {
                text: "x".into(),
                points: 1,
            }],
        }
    }
}

fn dups(paths: &[&PathBuf]) -> Candidate {
    Candidate::Duplicates {
        copies: paths.iter().map(|p| (*p).clone()).collect(),
    }
}

fn left(r: &CommitResult, id: u32) -> String {
    match &r
        .items
        .iter()
        .find(|i| i.id == id)
        .expect("item in result")
        .outcome
    {
        Outcome::Left { reason } => reason.clone(),
        Outcome::Moved { .. } => panic!("item {id} was moved"),
    }
}

fn is_moved(r: &CommitResult, id: u32) -> bool {
    matches!(
        r.items.iter().find(|i| i.id == id).unwrap().outcome,
        Outcome::Moved { .. }
    )
}

#[test]
fn commit_without_a_token_is_refused() {
    let env = Env::new(false);
    assert!(env.engine.commit("no-such-token", &[0], &[]).is_err());
    assert!(env.engine.commit("", &[], &[]).is_err());
}

#[test]
fn a_token_works_only_once() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    assert!(env.engine.commit(&prep.token, &[0], &[]).is_ok());
    assert!(env.engine.commit(&prep.token, &[0], &[]).is_err());
}

#[test]
fn tokens_are_random_and_only_eight_stay_valid() {
    let env = Env::new(false);
    let first = env.engine.prepare(vec![], &[]).token;
    let second = env.engine.prepare(vec![], &[]).token;
    assert_ne!(first, second);
    assert!(first.len() >= 32 && first.chars().all(|c| c.is_ascii_hexdigit()));
    for _ in 0..7 {
        env.engine.prepare(vec![], &[]);
    }
    // 9 prepared so far: the oldest is gone, the second is still there.
    assert!(env.engine.commit(&first, &[], &[]).is_err());
    assert!(env.engine.commit(&second, &[], &[]).is_ok());
}

#[test]
fn an_item_not_in_the_token_is_refused() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"aa");
    let b = env.file("users/me/docs/b.bin", b"aa");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    assert!(env.engine.commit(&prep.token, &[0, 99], &[]).is_err());
    assert!(env.rec.calls().is_empty());
    assert!(a.exists() && b.exists());
}

#[test]
fn a_missing_item_is_left_out_of_prepare() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"aa");
    let prep = env
        .engine
        .prepare(vec![dups(&[&a, &env.p("users/me/docs/none.bin")])], &[]);
    assert_eq!(prep.items.len(), 1);
    assert_eq!(prep.items[0].path, a.to_string_lossy());
    assert_eq!(prep.items[0].size, 2);
}

#[test]
fn a_repeated_id_is_handled_once_and_only_moved_items_count() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"aaaa");
    let b = env.file("users/me/docs/b.bin", b"aaaa");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let r = env.engine.commit(&prep.token, &[1, 1], &[]).unwrap();
    assert_eq!(r.items.len(), 1);
    assert!(is_moved(&r, 1));
    assert_eq!(r.moved_bytes, 4);
    assert_eq!(env.rec.calls().len(), 1);
}

#[test]
fn the_last_copy_stays() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    assert_eq!(
        r.items
            .iter()
            .filter(|i| matches!(i.outcome, Outcome::Moved { .. }))
            .count(),
        1
    );
    // The first in the group's order is the one kept, every time.
    assert!(a.exists() && !b.exists());
    assert!(left(&r, 0).contains("last copy"));
    assert_eq!(r.moved_bytes, 4);
}

#[test]
fn a_locked_copy_counts_as_the_surviving_copy() {
    let env = Env::new(false);
    let keeper = env.file("pf/app/doc.bin", b"same");
    let copy = env.file("users/me/docs/doc.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&keeper, &copy])], &[]);
    assert_eq!(prep.items[0].lock, Some(Lock::ProgramFiles));
    assert_eq!(prep.items[1].lock, None);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    assert!(left(&r, 0).contains("Program Files"));
    assert!(is_moved(&r, 1));
    assert!(keeper.exists() && !copy.exists());
}

#[test]
fn a_copy_changed_since_review_stays() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    std::fs::write(&b, b"same but longer now").unwrap();
    let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
    assert!(left(&r, 1).contains("changed"));
    assert!(b.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_copy_whose_contents_differ_from_the_survivor_stays() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"aaaa");
    let b = env.file("users/me/docs/b.bin", b"aaab");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
    assert!(left(&r, 1).contains("identical"));
    assert!(a.exists() && b.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_copy_stays_if_the_keeper_was_deleted_after_review() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    std::fs::remove_file(&a).unwrap();
    let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
    assert!(left(&r, 1).contains("identical and unchanged"));
    assert!(b.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_copy_stays_if_the_keeper_changed_after_review() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    std::fs::write(&a, b"diff").unwrap();
    let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
    assert!(left(&r, 1).contains("identical and unchanged"));
    assert!(b.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_hard_link_is_not_a_surviving_copy() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.p("users/me/docs/b.bin");
    std::fs::hard_link(&a, &b).unwrap();
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
    assert!(left(&r, 1).contains("identical and unchanged"));
    assert!(a.exists() && b.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_protected_path_is_refused_even_if_added_after_prepare() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let protect = vec![env.p("users/me/docs").to_string_lossy().into_owned()];
    let r = env.engine.commit(&prep.token, &[1], &protect).unwrap();
    assert!(left(&r, 1).contains("always protected"));
    assert!(b.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_linked_pc_file_is_refused() {
    let env = Env::new(false);
    let prep = env.engine.prepare(
        vec![Candidate::LinkedPc {
            path: r"\\other-pc\share\f.bin".into(),
        }],
        &[],
    );
    assert_eq!(prep.items[0].lock, Some(Lock::LinkedPc));
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(!is_moved(&r, 0));
    assert!(env.rec.calls().is_empty());
}

#[test]
fn program_files_without_an_elevator_is_refused() {
    let env = Env::new(false);
    let prep = env.engine.prepare(vec![env.folder("pf/oldapp")], &[]);
    assert!(prep.items[0].needs_admin);
    assert_eq!(prep.items[0].lock, None);
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert_eq!(left(&r, 0), "Windows admin rights are needed for this.");
    assert!(env.rec.calls().is_empty());
    assert!(env.p("pf/oldapp").exists());
}

#[test]
fn admin_items_go_through_the_elevator_in_one_call() {
    let env = Env::new(true);
    let prep = env
        .engine
        .prepare(vec![env.folder("pf/one"), env.folder("pf/two")], &[]);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    assert!(is_moved(&r, 0) && is_moved(&r, 1));
    let calls = env.elev.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 2);
    assert!(
        env.rec.calls().is_empty(),
        "admin items never go through the user-level recycler"
    );
}

#[test]
fn a_stale_file_needs_admin() {
    let env = Env::new(false);
    let f = env.file("pd/Seek/old.dat", b"x");
    let prep = env
        .engine
        .prepare(vec![Candidate::StaleFile { path: f.clone() }], &[]);
    assert!(prep.items[0].needs_admin);
    assert_eq!(prep.items[0].why, Why::StaleData);
    assert_eq!(prep.items[0].lock, None);
}

#[test]
fn a_folder_claimed_at_review_is_locked() {
    let env = Env::new(false);
    let Candidate::Folder { path, .. } = env.folder("left/app") else {
        unreachable!()
    };
    env.claims.0.lock().unwrap().insert(path.clone());
    let prep = env.engine.prepare(
        vec![Candidate::Folder {
            path,
            why: Why::Leftover,
            level: None,
            reasons: vec![],
        }],
        &[],
    );
    assert_eq!(prep.items[0].lock, Some(Lock::Claimed));
}

#[test]
fn a_folder_claimed_after_review_stays() {
    let env = Env::new(false);
    let cand = env.folder("left/app");
    let Candidate::Folder { path, .. } = &cand else {
        unreachable!()
    };
    let path = path.clone();
    let prep = env.engine.prepare(vec![cand], &[]);
    env.claims.0.lock().unwrap().insert(path.clone());
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(!is_moved(&r, 0));
    assert!(path.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_folder_with_a_new_file_stays() {
    let env = Env::new(false);
    let cand = env.folder("left/app");
    let Candidate::Folder { path, .. } = &cand else {
        unreachable!()
    };
    let path = path.clone();
    let prep = env.engine.prepare(vec![cand], &[]);
    assert_eq!(prep.items[0].files, 1);
    std::fs::write(path.join("new.txt"), b"hello").unwrap();
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(left(&r, 0).contains("changed"));
    assert!(path.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_reparse_point_candidate_is_locked_and_its_target_untouched() {
    let env = Env::new(false);
    let target = env.file("left/target/precious.txt", b"keep me");
    let link = env.p("left/link");
    assert!(make_junction(&link, target.parent().unwrap()));
    let prep = env.engine.prepare(
        vec![Candidate::Folder {
            path: link.clone(),
            why: Why::Leftover,
            level: None,
            reasons: vec![],
        }],
        &[],
    );
    assert_eq!(prep.items[0].lock, Some(Lock::ReparsePoint));
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(!is_moved(&r, 0));
    assert!(env.rec.calls().is_empty());
    assert!(target.exists());
}

#[test]
fn a_junction_inside_a_folder_is_not_counted_or_followed() {
    let env = Env::new(false);
    let outside = env.file("left/outside/huge.bin", &vec![1u8; 9000]);
    let cand = env.folder("left/app");
    let Candidate::Folder { path, .. } = &cand else {
        unreachable!()
    };
    assert!(make_junction(&path.join("jump"), outside.parent().unwrap()));
    let prep = env.engine.prepare(vec![cand], &[]);
    assert_eq!((prep.items[0].files, prep.items[0].size), (1, 9));
}

#[test]
fn a_file_reached_through_a_junction_into_the_windows_folder_is_locked() {
    let env = Env::new(false);
    env.file("win/system32/x.dll", b"dll");
    let jump = env.p("users/me/docs/jump");
    assert!(make_junction(&jump, &env.p("win")));
    let via = jump.join("system32").join("x.dll");
    let other = env.file("users/me/docs/x.dll", b"dll");
    let prep = env.engine.prepare(vec![dups(&[&via, &other])], &[]);
    assert_eq!(prep.items[0].lock, Some(Lock::SystemFolder));
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(!is_moved(&r, 0));
    assert!(env.rec.calls().is_empty());
    assert!(env.p("win/system32/x.dll").exists());
}

#[test]
fn a_leftover_reached_through_a_junction_into_program_files_needs_admin() {
    let env = Env::new(false);
    env.folder("pf/app");
    let jump = env.p("left/jump");
    assert!(make_junction(&jump, &env.p("pf")));
    let prep = env.engine.prepare(
        vec![Candidate::Folder {
            path: jump.join("app"),
            why: Why::Leftover,
            level: None,
            reasons: vec![],
        }],
        &[],
    );
    assert!(prep.items[0].needs_admin);
}

#[test]
fn the_rest_of_the_batch_stops_when_one_item_cant_be_confirmed() {
    let env = Env::new(false);
    let cands = vec![
        env.folder("left/a"),
        env.folder("left/b"),
        env.folder("left/c"),
    ];
    let prep = env.engine.prepare(cands, &[]);
    *env.rec.destroy_at.lock().unwrap() = Some(2);
    let r = env.engine.commit(&prep.token, &[0, 1, 2], &[]).unwrap();
    assert!(r.stopped_early.is_some());
    assert_eq!(r.items[0].id, 1, "the unconfirmed item is reported first");
    assert!(left(&r, 1).contains("deleted for good"));
    assert!(is_moved(&r, 0));
    assert!(!is_moved(&r, 2));
    assert!(env.p("left/c").exists(), "the rest is untouched");
    assert_eq!(env.rec.calls().len(), 2);
    assert_eq!(r.items.len(), 3);
}

#[test]
fn an_item_that_fails_but_stays_does_not_stop_the_run() {
    let env = Env::new(false);
    let cands = vec![
        env.folder("left/a"),
        env.folder("left/b"),
        env.folder("left/c"),
    ];
    let prep = env.engine.prepare(cands, &[]);
    env.rec.refuse.lock().unwrap().insert(env.p("left/b"));
    let r = env.engine.commit(&prep.token, &[0, 1, 2], &[]).unwrap();
    assert!(r.stopped_early.is_none());
    assert!(is_moved(&r, 0) && is_moved(&r, 2));
    assert!(left(&r, 1).contains("in use"));
    assert!(env.p("left/b").exists());
    assert_eq!(env.rec.calls().len(), 3, "one path per call");
}

fn leftover(p: &Path) -> Candidate {
    Candidate::Folder {
        path: p.to_path_buf(),
        why: Why::Leftover,
        level: None,
        reasons: vec![],
    }
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn a_protected_item_inside_a_listed_folder_locks_the_folder() {
    let env = Env::new(false);
    env.folder("users/me/appdata/OldApp");
    let key = env.file("users/me/appdata/OldApp/license.key", b"k");
    let prot = vec![s(&key)];
    let folder = env.p("users/me/appdata/OldApp");
    let prep = env.engine.prepare(vec![leftover(&folder)], &prot);
    assert_eq!(prep.items[0].lock, Some(Lock::Protected));
    // Protected only after review: commit refuses it too.
    let prep = env.engine.prepare(vec![leftover(&folder)], &[]);
    assert_eq!(prep.items[0].lock, None);
    let r = env.engine.commit(&prep.token, &[0], &prot).unwrap();
    assert!(left(&r, 0).contains("always protected"));
    assert!(key.exists() && env.rec.calls().is_empty());
}

#[test]
fn protecting_a_junction_protects_what_it_points_at() {
    let env = Env::new(false);
    let a = env.file("left/real/a.bin", b"same");
    let docs = env.file("users/me/docs/a.bin", b"same");
    assert!(make_junction(&env.p("left/jump"), &env.p("left/real")));
    let prot = vec![s(&env.p("left/jump"))];
    let prep = env.engine.prepare(vec![dups(&[&a, &docs])], &prot);
    assert_eq!(prep.items[0].lock, Some(Lock::Protected));
    assert_eq!(prep.items[1].lock, None);
}

#[test]
fn a_folder_that_is_or_holds_a_known_place_is_never_a_leftover() {
    let env = Env::new(false);
    for rel in ["users/me", "users/me/appdata", "pf"] {
        let prep = env.engine.prepare(vec![leftover(&env.p(rel))], &[]);
        assert_eq!(prep.items[0].lock, Some(Lock::SystemFolder), "{rel}");
    }
    for rel in ["users", "win", "pd"] {
        let prep = env.engine.prepare(vec![leftover(&env.p(rel))], &[]);
        assert!(prep.items[0].lock.is_some(), "{rel}");
    }
    // Inside AppData or Program Files still works as before.
    env.folder("users/me/appdata/OldApp");
    env.folder("pf/OldApp");
    let prep = env.engine.prepare(
        vec![
            leftover(&env.p("users/me/appdata/OldApp")),
            leftover(&env.p("pf/OldApp")),
        ],
        &[],
    );
    assert_eq!(prep.items[0].lock, None);
    assert_eq!(
        (prep.items[1].lock, prep.items[1].needs_admin),
        (None, true)
    );
}

#[test]
fn a_folder_candidate_must_be_a_folder_with_a_folder_purpose() {
    let env = Env::new(false);
    let Candidate::Folder { path, .. } = env.folder("left/x") else {
        unreachable!()
    };
    for why in [Why::StaleData, Why::Duplicate] {
        let cand = Candidate::Folder {
            path: path.clone(),
            why,
            level: None,
            reasons: vec![],
        };
        let prep = env.engine.prepare(vec![cand], &[]);
        assert_eq!(prep.items[0].lock, Some(Lock::UnsupportedPath), "{why:?}");
    }
    let file = env.file("left/f.txt", b"x");
    let prep = env.engine.prepare(vec![leftover(&file)], &[]);
    assert_eq!(prep.items[0].lock, Some(Lock::UnsupportedPath));
    let dir_as_dup = env.engine.prepare(vec![dups(&[&path])], &[]);
    assert_eq!(dir_as_dup.items[0].lock, Some(Lock::UnsupportedPath));
}

#[test]
fn a_selected_folder_cannot_hold_the_surviving_copy() {
    let env = Env::new(false);
    let old = env.file("left/old/a.bin", b"same");
    let docs = env.file("users/me/docs/a.bin", b"same");
    let prep = env.engine.prepare(
        vec![dups(&[&old, &docs]), leftover(&env.p("left/old"))],
        &[],
    );
    let r = env.engine.commit(&prep.token, &[1, 2], &[]).unwrap();
    assert!(left(&r, 1).contains("last copy"));
    assert!(is_moved(&r, 2));
    assert!(docs.exists());
}

#[test]
fn a_folder_going_through_the_elevator_cannot_hold_the_surviving_copy_either() {
    let env = Env::new(true);
    let old = env.file("pf/old/a.bin", b"same");
    let docs = env.file("users/me/docs/a.bin", b"same");
    let prep = env
        .engine
        .prepare(vec![dups(&[&old, &docs]), leftover(&env.p("pf/old"))], &[]);
    assert!(prep.items[2].needs_admin);
    let r = env.engine.commit(&prep.token, &[1, 2], &[]).unwrap();
    assert!(left(&r, 1).contains("last copy"));
    assert!(is_moved(&r, 2));
    assert!(docs.exists() && env.rec.calls().is_empty());
}

#[test]
fn an_item_inside_another_selected_folder_is_left_and_not_counted_twice() {
    let env = Env::new(false);
    let inside = env.file("left/old/a.bin", b"same");
    let docs = env.file("users/me/docs/a.bin", b"same");
    let prep = env.engine.prepare(
        vec![dups(&[&inside, &docs]), leftover(&env.p("left/old"))],
        &[],
    );
    let folder_size = prep.items[2].size;
    let r = env.engine.commit(&prep.token, &[0, 2], &[]).unwrap();
    assert!(left(&r, 0).contains("inside a folder"));
    assert!(is_moved(&r, 2));
    assert_eq!(r.moved_bytes, folder_size);
    assert_eq!(env.rec.calls().len(), 1);
}

#[test]
fn two_commits_at_once_never_count_each_other_as_the_survivor() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let one = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let two = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    let flag = Arc::new(AtomicBool::new(false));
    *env.rec.gate.lock().unwrap() = Some(flag.clone());
    std::thread::scope(|sc| {
        let t1 = sc.spawn(|| env.engine.commit(&one.token, &[0], &[]).unwrap());
        let t2 = sc.spawn(|| {
            flag.store(true, Ordering::SeqCst);
            env.engine.commit(&two.token, &[1], &[]).unwrap()
        });
        t1.join().unwrap();
        t2.join().unwrap();
    });
    assert_eq!(
        a.exists() as u8 + b.exists() as u8,
        1,
        "exactly one copy must survive"
    );
}

#[test]
fn a_keeper_that_changes_during_the_compare_is_caught_before_removal() {
    for change_keeper in [true, false] {
        let env = Env::new(false);
        let a = env.file("users/me/docs/a.bin", b"same");
        let b = env.file("users/me/docs/b.bin", b"same");
        let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
        let victim = if change_keeper { a.clone() } else { b.clone() };
        *env.wrap.after_compare.lock().unwrap() = Some(Box::new(move || {
            std::fs::write(&victim, b"changed while comparing").unwrap()
        }));
        let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
        assert!(
            left(&r, 1).contains("changed"),
            "keeper changed: {change_keeper}"
        );
        assert!(env.rec.calls().is_empty());
    }
}

#[test]
fn queued_admin_bytes_count_towards_the_drive_total() {
    let env = Env::new(true);
    let prep = env
        .engine
        .prepare(vec![env.folder("pf/one"), env.folder("left/a")], &[]);
    env.wrap.batches.lock().unwrap().clear();
    env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    // "some data" is 9 bytes: the second item is judged with the queued first one added.
    assert_eq!(*env.wrap.batches.lock().unwrap(), vec![9, 18]);
}

#[test]
fn a_copy_in_the_recycle_bin_is_not_a_surviving_copy() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let in_bin = env.file("$Recycle.Bin/S-1-5-21/$Rabc123.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &in_bin])], &[]);
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(left(&r, 0).contains("last copy"));
    assert!(a.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_claim_on_the_real_location_counts() {
    let env = Env::new(false);
    env.folder("left/realdir/app");
    assert!(make_junction(
        &env.p("left/jumpdir"),
        &env.p("left/realdir")
    ));
    env.claims
        .0
        .lock()
        .unwrap()
        .insert(PathBuf::from(canon(&env.p("left/realdir/app"))));
    let prep = env
        .engine
        .prepare(vec![leftover(&env.p("left/jumpdir/app"))], &[]);
    assert_eq!(prep.items[0].lock, Some(Lock::Claimed));
}

#[test]
fn a_folder_that_is_half_removed_stops_the_run() {
    let env = Env::new(false);
    let prep = env
        .engine
        .prepare(vec![env.folder("left/a"), env.folder("left/b")], &[]);
    *env.rec.partial_at.lock().unwrap() = Some(1);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    assert!(r.stopped_early.is_some());
    assert_eq!(r.items[0].id, 0);
    assert!(left(&r, 0).contains("deleted for good"));
    assert!(left(&r, 1).contains("stopped"));
    assert!(env.p("left/b").exists());
    assert_eq!(env.rec.calls().len(), 1);
}

#[test]
fn a_block_from_the_pre_checks_holds_at_prepare_and_at_commit() {
    let env = Env::new(false);
    let Candidate::Folder { path: blocked, .. } = env.folder("left/blocked") else {
        unreachable!()
    };
    env.wrap.block.lock().unwrap().insert(blocked.clone());
    let prep = env.engine.prepare(vec![leftover(&blocked)], &[]);
    assert_eq!(prep.items[0].lock, Some(Lock::NoRecycleBin));

    let Candidate::Folder { path: later, .. } = env.folder("left/later") else {
        unreachable!()
    };
    let prep = env.engine.prepare(vec![leftover(&later)], &[]);
    assert_eq!(prep.items[0].lock, None);
    env.wrap.block.lock().unwrap().insert(later.clone());
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(left(&r, 0).contains("no Recycle Bin"));
    assert!(later.exists() && env.rec.calls().is_empty());
}

#[test]
fn a_failed_byte_comparison_is_not_a_match() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    env.wrap.same_bytes_fails.store(true, Ordering::SeqCst);
    let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
    assert!(left(&r, 1).contains("identical and unchanged"));
    assert!(b.exists() && env.rec.calls().is_empty());
}

#[test]
fn admin_items_do_not_run_after_a_stop() {
    let env = Env::new(true);
    let prep = env
        .engine
        .prepare(vec![env.folder("pf/one"), env.folder("left/a")], &[]);
    *env.rec.destroy_at.lock().unwrap() = Some(1);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    assert!(r.stopped_early.is_some());
    assert!(env.elev.calls.lock().unwrap().is_empty());
    assert!(left(&r, 0).contains("stopped"));
    assert!(env.p("pf/one").exists());
}

#[test]
fn a_failed_elevator_run_does_not_claim_everything_stayed() {
    let env = Env::new(true);
    *env.elev.mode.lock().unwrap() = ElevMode::Fail;
    let prep = env
        .engine
        .prepare(vec![env.folder("pf/one"), env.folder("pf/two")], &[]);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    for id in [0, 1] {
        let why = left(&r, id);
        assert!(
            why.contains("may have removed") && !why.contains("left in place"),
            "{why}"
        );
    }
    assert_eq!(r.moved_bytes, 0);
}

#[test]
fn a_partial_elevator_answer_is_reported_item_by_item() {
    let env = Env::new(true);
    *env.elev.mode.lock().unwrap() = ElevMode::OnlyFirst;
    let prep = env
        .engine
        .prepare(vec![env.folder("pf/one"), env.folder("pf/two")], &[]);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    assert!(is_moved(&r, 0));
    assert!(left(&r, 1).contains("no answer"));
    assert_eq!(r.moved_bytes, 9);
    assert_eq!(r.items.len(), 2);
}

#[test]
fn a_file_swapped_for_a_look_alike_stays() {
    let env = Env::new(false);
    let a = env.file("users/me/docs/a.bin", b"same");
    let b = env.file("users/me/docs/b.bin", b"same");
    let prep = env.engine.prepare(vec![dups(&[&a, &b])], &[]);
    // Same size, same modified time, different file on disk: only the file ID can tell.
    let mtime = std::fs::metadata(&b).unwrap().modified().unwrap();
    let c = env.file("users/me/docs/c.bin", b"same");
    std::fs::File::options()
        .write(true)
        .open(&c)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    std::fs::remove_file(&b).unwrap();
    std::fs::rename(&c, &b).unwrap();
    assert_eq!(
        RealFacts.stat(&b).map(|s| (s.size, s.modified_ms)),
        Some((4, prep.items[1].modified_ms))
    );
    let r = env.engine.commit(&prep.token, &[1], &[]).unwrap();
    assert!(left(&r, 1).contains("changed"));
    assert!(b.exists() && env.rec.calls().is_empty());
}
