use super::*;
use seek_clean::facts::{ClaimCheck, Recycler, Stat};
use seek_clean::model::{DupCopy, Lock, Outcome};
use seek_core::{DeviceIdentity, DeviceKind, ResultRow};
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

/// Stands in for the real check: every folder counts as claimed, so these tests never depend on
/// what is installed or running on the PC that runs them.
struct ClaimsEveryFolder;

impl ClaimCheck for ClaimsEveryFolder {
    fn is_claimed(&self, _folder: &Path) -> Option<String> {
        Some("a test says so".to_string())
    }
}

const MB: u64 = 1_048_576;

fn id(n: u128) -> DeviceId {
    DeviceId::from_u128(n)
}
fn host(n: u128) -> HostId {
    HostId::from_u128(n)
}
fn mins(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

fn device(n: u128, host_n: u128, state: DeviceState, updated: i64) -> Device {
    Device {
        id: id(n),
        identity: DeviceIdentity::Volume { serial: n as u64, filesystem: "NTFS".into() },
        display_name: format!("Drive {n}"),
        label: String::new(),
        filesystem: "NTFS".into(),
        kind: DeviceKind::Fixed,
        total_bytes: 0,
        free_bytes: 0,
        last_mount_point: "C:\\".into(),
        indexed_by_host: host(host_n),
        first_seen: 0,
        last_seen: 0,
        index_updated_at: updated,
        state,
        entry_count: 0,
        error: None,
    }
}

fn row(dev: u128, path: &str, size: u64) -> ResultRow {
    let name = path.rsplit('\\').next().unwrap().to_string();
    ResultRow {
        name,
        folder: String::new(),
        full_path: path.to_string(),
        size,
        modified: 0,
        created: 0,
        is_dir: false,
        attrs: 0,
        device_id: id(dev),
        device_name: String::new(),
        online: true,
    }
}

fn owners() -> Owners {
    let mut o = Owners::new();
    o.insert(id(1), Owner { local: true, pc_name: "This".into() });
    o.insert(id(2), Owner { local: true, pc_name: "This".into() });
    o.insert(id(9), Owner { local: false, pc_name: "Laptop".into() });
    o
}

fn quiet() -> Box<Emit> {
    Box::new(|_| {})
}

fn counting() -> (Arc<Mutex<Vec<Value>>>, Box<Emit>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    (seen, Box::new(move |v| s2.lock().unwrap().push(v)))
}

fn types(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
    seen.lock().unwrap().iter().map(|v| v["type"].as_str().unwrap().to_string()).collect()
}

/// A recycler for tests that must never recycle anything.
struct NeverRecycles;
impl Recycler for NeverRecycles {
    fn recycle(&self, _: &[PathBuf]) -> Vec<Result<String, String>> {
        panic!("this test must never reach the Recycle Bin");
    }
}

fn fake_places() -> Places {
    Places::for_tests("C:\\Windows", &["C:\\Program Files"], "C:\\Users", "C:\\Users\\me", "C:\\ProgramData", &["C:\\Users\\me\\AppData"])
}

fn idle_core() -> Arc<Core> {
    let places = fake_places();
    let facts: Arc<dyn FileFacts> = Arc::new(RealFacts);
    let engine = Engine::new(places.clone(), Arc::new(NeverRecycles), None, facts.clone(), Arc::new(ClaimsEveryFolder));
    Arc::new(Core { engine, places, facts })
}

fn state_with(core: Arc<Core>) -> CleanState {
    CleanState { core: Mutex::new(Some(core)), listed: Mutex::new(Default::default()), shared: Arc::new(Shared::default()), emit: Mutex::new(None) }
}

fn group(size: u64, paths: &[&str]) -> DupGroup {
    DupGroup {
        id: 1,
        name: String::new(),
        size,
        copies: paths
            .iter()
            .map(|p| DupCopy { path: p.to_string(), device_id: id(1).to_string(), size, modified: 0, lock: None, suggested_keep: false })
            .collect(),
        freeable: size,
    }
}

#[test]
fn split_rows_separates_local_from_linked_pcs() {
    let groups = vec![
        // two local copies and a linked file with the same name: the linked one is kept to be shown
        SizeGroup {
            size: 5 * MB,
            files: vec![row(1, "C:\\a\\Movie.mkv", 5 * MB), row(2, "D:\\b\\movie.MKV", 5 * MB), row(9, "C:\\Users\\x\\Movie.mkv", 5 * MB)],
        },
        // a linked file whose name differs: not kept, and never a candidate
        SizeGroup {
            size: 3 * MB,
            files: vec![row(1, "C:\\a\\one.bin", 3 * MB), row(2, "D:\\a\\one.bin", 3 * MB), row(9, "C:\\other.bin", 3 * MB)],
        },
        // one local file and one linked file: no local pair, so nothing from this size
        SizeGroup { size: 2 * MB, files: vec![row(1, "C:\\only.bin", 2 * MB), row(9, "C:\\only.bin", 2 * MB)] },
        // a drive nobody knows is ignored altogether
        SizeGroup { size: 1_500_000, files: vec![row(1, "C:\\p.bin", 1_500_000), row(1, "C:\\q.bin", 1_500_000), row(77, "Z:\\p.bin", 1_500_000)] },
    ];
    let s = split_rows(groups, &owners());
    assert_eq!(s.batches.len(), 3);
    let paths = |b: &SizeBatch| b.files.iter().map(|f| f.path.to_string_lossy().into_owned()).collect::<Vec<_>>();
    assert_eq!(s.batches[0].size, 5 * MB);
    assert_eq!(paths(&s.batches[0]), vec!["C:\\a\\Movie.mkv", "D:\\b\\movie.MKV"]);
    assert_eq!(s.batches[0].files[0].device_id, id(1).to_string());
    assert_eq!(paths(&s.batches[2]), vec!["C:\\p.bin", "C:\\q.bin"]);
    let all: Vec<String> = s.batches.iter().flat_map(paths).collect();
    assert!(!all.iter().any(|p| p.contains("Users\\x") || p.contains("other.bin") || p.contains("only.bin") || p.starts_with("Z:")));
    assert_eq!(
        s.linked,
        vec![LinkedRow { name: "Movie.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Users\\x\\Movie.mkv".into() }]
    );
}

#[test]
fn linked_files_show_only_next_to_a_confirmed_group_with_the_same_name_and_size() {
    let linked = vec![
        LinkedRow { name: "Movie.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Users\\x\\Movie.mkv".into() },
        LinkedRow { name: "Other.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Other.mkv".into() },
        LinkedRow { name: "Movie.mkv".into(), size: 6 * MB, pc_name: "Laptop".into(), path: "C:\\Other\\Movie.mkv".into() },
    ];
    let groups = vec![group(5 * MB, &["C:\\a\\x.bin", "D:\\b\\movie.MKV"])];
    let m = other_pc_matches(&linked, &groups);
    assert_eq!(m, vec![OtherPcMatch { name: "Movie.mkv".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\Users\\x\\Movie.mkv".into() }]);
    assert!(other_pc_matches(&linked, &[]).is_empty());
}

#[test]
fn prepare_request_with_a_linked_device_becomes_a_refused_candidate() {
    let file = |path: &str, dev: u128| ReqFile { path: path.into(), device_id: id(dev).to_string() };
    let req = vec![vec![file("C:\\a\\x.bin", 1), file("D:\\b\\x.bin", 2), file("C:\\Users\\x\\x.bin", 9), file("Q:\\mystery.bin", 55)]];
    let cands = candidates_from_request(req, &owners());
    assert_eq!(
        cands,
        vec![
            Candidate::Duplicates { copies: vec![PathBuf::from("C:\\a\\x.bin"), PathBuf::from("D:\\b\\x.bin")] },
            Candidate::LinkedPc { path: "C:\\Users\\x\\x.bin".into() },
            Candidate::LinkedPc { path: "Q:\\mystery.bin".into() },
        ]
    );
    let bad = candidates_from_request(vec![vec![ReqFile { path: "C:\\z.bin".into(), device_id: "nope".into() }]], &owners());
    assert_eq!(bad, vec![Candidate::LinkedPc { path: "C:\\z.bin".into() }]);

    // and the engine refuses it, whatever else is chosen
    let engine = Engine::new(fake_places(), Arc::new(NeverRecycles), None, Arc::new(RealFacts), Arc::new(ClaimsEveryFolder));
    let prepared = engine.prepare(cands, &[]);
    let linked: Vec<_> = prepared.items.iter().filter(|i| i.path.contains("Users\\x") || i.path.starts_with("Q:")).collect();
    assert_eq!(linked.len(), 2);
    assert!(linked.iter().all(|i| i.lock == Some(Lock::LinkedPc)));
    let ids: Vec<u32> = linked.iter().map(|i| i.id).collect();
    let r = engine.commit(&prepared.token, &ids, &[]).unwrap();
    assert!(r.items.iter().all(|i| matches!(i.outcome, Outcome::Left { .. })));
    assert_eq!(r.moved_bytes, 0);
}

#[test]
fn the_json_the_window_sends_is_read_and_turned_into_local_and_linked_candidates() {
    let one = id(1).to_string();
    let nine = id(9).to_string();
    let json = format!(
        r#"{{"kind":"duplicates","groups":[[{{"path":"C:\\a\\x.bin","deviceId":"{one}"}},{{"path":"C:\\Users\\x\\x.bin","deviceId":"{nine}"}},{{"path":"D:\\b\\x.bin","deviceId":"{one}"}}]]}}"#
    );
    let req: PrepareRequest = serde_json::from_str(&json).unwrap();
    let PrepareRequest::Duplicates { groups } = req else { panic!("not a duplicates request") };
    let cands = candidates_from_request(groups, &owners());
    assert_eq!(
        cands,
        vec![
            Candidate::Duplicates { copies: vec![PathBuf::from("C:\\a\\x.bin"), PathBuf::from("D:\\b\\x.bin")] },
            Candidate::LinkedPc { path: "C:\\Users\\x\\x.bin".into() },
        ]
    );
}

fn stale(path: &str) -> StaleFile {
    StaleFile { path: path.to_string(), size: 7, modified: 1_700_000_000_000 }
}

fn folder_row(path: &str, level: Level) -> LeftoverRow {
    LeftoverRow {
        path: path.to_string(),
        parent: String::new(),
        size: 1,
        modified: 0,
        needs_admin: false,
        level,
        reasons: vec![],
        contents: vec![],
    }
}

#[test]
fn leftover_and_stale_requests_are_read_as_the_window_sends_them() {
    // The shapes `folderRequest` (LeftoversTab) and the `stale` kind of `PrepareRequest` send.
    let folders = r#"{"kind":"folders","why":"leftover","folders":[{"path":"C:\\x","level":"likely","reasons":[{"text":"t","points":3}]},{"path":"C:\\y","level":null,"reasons":[]}]}"#;
    let PrepareRequest::Folders { why, folders } = serde_json::from_str(folders).unwrap() else { panic!("not folders") };
    assert_eq!(why, Why::Leftover);
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0].level, Some(Level::Likely));
    assert_eq!(folders[0].reasons, vec![Reason { text: "t".into(), points: 3 }]);
    assert_eq!(folders[1].level, None);
    let stale = r#"{"kind":"stale","files":["C:\\ProgramData\\Seek\\indexes\\a.seekidx"]}"#;
    let PrepareRequest::Stale { files } = serde_json::from_str(stale).unwrap() else { panic!("not stale") };
    assert_eq!(files, vec![r"C:\ProgramData\Seek\indexes\a.seekidx".to_string()]);
    assert!(serde_json::from_str::<PrepareRequest>(r#"{"kind":"nonsense"}"#).is_err());
}

#[test]
fn no_service_still_lists_folders() {
    let folders = vec![folder_row(r"C:\Users\me\AppData\Local\Old", Level::Likely), folder_row(r"C:\Program Files\Gone", Level::VeryLikely)];
    let rows = merge_leftovers(folders.clone(), Err("lost connection to Seek service".into()));
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| !r.path.ends_with(".seekidx")));
    // Highest level first.
    assert_eq!(rows[0].path, r"C:\Program Files\Gone");
    // An answer with nothing stale changes nothing either.
    assert_eq!(merge_leftovers(folders, Ok(vec![])).len(), 2);
}

#[test]
fn stale_data_rows_join_the_list_needing_admin() {
    let folders = vec![folder_row(r"C:\a\Likely One", Level::Likely), folder_row(r"C:\a\Not Sure", Level::NotSure)];
    let rows = merge_leftovers(folders, Ok(vec![stale(r"C:\ProgramData\Seek\indexes\old.seekidx")]));
    let order: Vec<Level> = rows.iter().map(|r| r.level).collect();
    assert_eq!(order, vec![Level::VeryLikely, Level::Likely, Level::NotSure]);
    let s = &rows[0];
    assert_eq!(s.path, r"C:\ProgramData\Seek\indexes\old.seekidx");
    assert_eq!(s.parent, r"C:\ProgramData\Seek\indexes");
    assert!(s.needs_admin);
    assert_eq!((s.size, s.modified), (7, 1_700_000_000_000));
    assert_eq!(s.reasons.len(), 1);
    assert_eq!(s.reasons.iter().map(|r| r.points).sum::<i32>(), 5);
    assert!(!s.reasons[0].text.to_lowercase().contains("safe"));
}

#[test]
fn folder_and_stale_requests_become_candidates() {
    let f = |path: &str| ReqFolder { path: path.into(), level: Some(Level::Likely), reasons: vec![] };
    let listed = [stale(r"C:\ProgramData\Seek\indexes\old.seekidx")];
    let cands = folder_candidates(
        Why::Leftover,
        vec![f(r"C:\Users\me\AppData\Local\Old"), f(r"c:\programdata\seek\INDEXES\old.seekidx"), f(r"C:\Users\me\AppData\Local\backup.log")],
        &listed,
    )
    .unwrap();
    assert_eq!(cands.len(), 3);
    assert!(matches!(&cands[0], Candidate::Folder { why: Why::Leftover, level: Some(Level::Likely), .. }));
    assert!(matches!(&cands[1], Candidate::StaleFile { .. }));
    // A folder that merely looks like a log file, and that the service does not list, stays a folder.
    assert!(matches!(&cands[2], Candidate::Folder { .. }));
    // Only leftover folders are removed this way.
    assert!(folder_candidates(Why::Duplicate, vec![f(r"C:\x")], &[]).is_err());
    assert!(folder_candidates(Why::StaleData, vec![f(r"C:\x")], &[]).is_err());
    // A stale request keeps only what the service still calls stale.
    let wanted = vec![r"C:\ProgramData\Seek\indexes\old.seekidx".to_string(), r"C:\ProgramData\Seek\indexes\live.seekidx".to_string()];
    let cands = stale_candidates(wanted, &listed);
    assert_eq!(cands, vec![Candidate::StaleFile { path: PathBuf::from(r"C:\ProgramData\Seek\indexes\old.seekidx") }]);
}

/// Claims nothing, so the engine's own rules are what these tests see.
struct ClaimsNothing;
impl ClaimCheck for ClaimsNothing {
    fn is_claimed(&self, _: &Path) -> Option<String> {
        None
    }
}

/// What the Leftovers tab sends for a Program Files folder, an AppData folder and a stale file goes
/// through `clean_prepare`'s own steps into the real engine: the first and the stale file need the
/// admin prompt, which does not exist yet, so a commit leaves them where they are and says why.
/// No recycler is reachable (it panics) and nothing on disk changes.
#[test]
fn leftover_rows_go_through_the_engines_rules_and_admin_rows_are_left_without_an_elevator() {
    let dir = tempfile::tempdir().unwrap();
    let b = dir.path();
    let s = |p: &str| b.join(p).to_string_lossy().into_owned();
    for d in ["win", "pf/Old Vendor", "pd/Seek/indexes", "users/me/appdata/Old App"] {
        std::fs::create_dir_all(b.join(d)).unwrap();
    }
    std::fs::write(b.join("pf/Old Vendor/a.txt"), b"x").unwrap();
    std::fs::write(b.join("users/me/appdata/Old App/a.txt"), b"x").unwrap();
    let stale_file = b.join("pd/Seek/indexes/00000000-0000-0000-0000-000000000009.seekidx");
    std::fs::write(&stale_file, b"old index").unwrap();
    let places = Places::for_tests(&s("win"), &[&s("pf")], &s("users"), &s("users/me"), &s("pd"), &[&s("users/me/appdata")]);
    let facts: Arc<dyn FileFacts> = Arc::new(DiskOnly);
    let engine = Engine::new(places, Arc::new(NeverRecycles), None, facts, Arc::new(ClaimsNothing));

    let tab = |p: String| ReqFolder {
        path: p,
        level: Some(Level::VeryLikely),
        reasons: vec![Reason { text: "Empty, or nothing left but empty folders".into(), points: 4 }],
    };
    let listed = [stale(&stale_file.to_string_lossy())];
    let mut cands = folder_candidates(
        Why::Leftover,
        vec![tab(s("pf/Old Vendor")), tab(s("users/me/appdata/Old App")), tab(stale_file.to_string_lossy().into_owned())],
        &listed,
    )
    .unwrap();
    // A stale-data row asked for through the `stale` kind reaches the engine the same way.
    cands.extend(stale_candidates(vec![stale_file.to_string_lossy().into_owned()], &listed));
    let prepared = engine.prepare(cands, &[]);
    assert_eq!(prepared.items.len(), 4);

    let pf = &prepared.items[0];
    assert_eq!((pf.why, pf.is_dir, pf.lock, pf.needs_admin), (Why::Leftover, true, None, true));
    assert_eq!(pf.level, Some(Level::VeryLikely), "the level the window showed comes back with the item");
    assert_eq!(pf.reasons.len(), 1);
    let app = &prepared.items[1];
    assert_eq!((app.why, app.lock, app.needs_admin), (Why::Leftover, None, false));
    for st in [&prepared.items[2], &prepared.items[3]] {
        assert_eq!((st.why, st.is_dir, st.lock, st.needs_admin), (Why::StaleData, false, None, true), "{st:?}");
    }

    // Commit only the admin rows: with no elevator each is left with the admin reason.
    let r = engine.commit(&prepared.token, &[0, 2], &[]).unwrap();
    assert_eq!(r.moved_bytes, 0);
    assert_eq!(r.items.len(), 2);
    for item in &r.items {
        assert_eq!(item.outcome, Outcome::Left { reason: "Windows admin rights are needed for this.".into() });
    }
    assert!(b.join("pf/Old Vendor/a.txt").exists() && stale_file.exists());
}

#[test]
fn results_are_dropped_five_minutes_after_the_later_of_hiding_and_finishing() {
    // finished before the window was hidden: counted from hiding
    assert!(should_drop_results(false, Some(mins(60)), Some(mins(5))));
    assert!(!should_drop_results(false, Some(mins(60)), Some(mins(5) - Duration::from_millis(1))));
    // finished after 40 hidden minutes: counted from finishing
    assert!(!should_drop_results(false, Some(mins(1)), Some(mins(40))));
    assert!(should_drop_results(false, Some(mins(5)), Some(mins(40))));
    // no scan has finished (none ran): counted from hiding
    assert!(should_drop_results(false, None, Some(mins(5))));
}

#[test]
fn results_stay_while_a_scan_runs_and_when_the_window_is_showing() {
    assert!(!should_drop_results(true, Some(mins(60)), Some(mins(60))));
    assert!(!should_drop_results(true, None, Some(mins(60))));
    // shown again: the hidden clock is gone, so the next hiding starts from nothing
    assert!(!should_drop_results(false, Some(mins(60)), None));
    assert!(!should_drop_results(false, None, None));
}

#[test]
fn results_dropped_by_the_clock_tell_the_window_once_and_a_late_finish_keeps_them() {
    let state = state_with(idle_core());
    let (seen, emit) = counting();
    let emit: Arc<Emit> = Arc::from(emit);
    *state.emit.lock().unwrap() = Some(emit.clone());
    let (gen, _) = state.shared.begin().unwrap();
    state.shared.on_event(gen, ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*quiet());
    state.shared.finish(gen, &*quiet());

    // the scan only just finished, however long the window was hidden
    state.window_hidden_for(Some(mins(40)));
    assert_eq!(state.shared.state().found.len(), 1);
    assert!(types(&seen).is_empty());

    // finished long ago and hidden since: gone, and the window is told exactly once
    state.shared.lock().finished_at = Some(Instant::now() - mins(6));
    state.window_hidden_for(Some(mins(6)));
    state.window_hidden_for(Some(mins(7)));
    assert!(state.shared.state().found.is_empty());
    assert_eq!(types(&seen), vec!["dropped"]);
}

#[test]
fn dropping_results_clears_them_unless_a_scan_runs() {
    let s = Shared::default();
    let (gen, _) = s.begin().unwrap();
    s.on_event(gen, ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*quiet());
    assert!(!s.drop_results());
    assert_eq!(s.state().found.len(), 1, "a running scan keeps its results");
    s.finish(gen, &*quiet());
    assert!(s.drop_results());
    let st = s.state();
    assert!(!st.running && st.found.is_empty() && st.other_pcs.is_empty());
    assert_eq!((st.checked_bytes, st.total_bytes, st.current), (0, 0, None));
    assert!(!s.drop_results(), "nothing left to drop the second time");
}

fn pg(size: u64, tag: &str, with_linked: bool) -> Vec<SizeGroup> {
    let mut files = vec![row(1, &format!("C:\\{tag}1"), size), row(2, &format!("D:\\{tag}2"), size)];
    if with_linked {
        files.push(row(9, &format!("C:\\Users\\x\\{tag}1"), size));
    }
    vec![SizeGroup { size, files }]
}

#[tokio::test]
async fn a_service_error_mid_paging_ends_the_scan_with_what_was_found_and_says_so() {
    let calls = AtomicUsize::new(0);
    let cursors = Mutex::new(Vec::new());
    let cancel = AtomicBool::new(false);
    let g = gather(
        |cursor| {
            cursors.lock().unwrap().push(cursor);
            let r: Page = match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok((pg(9 * MB, "big", false), Some(4 * MB))),
                1 => Ok((pg(4 * MB, "mid", false), Some(2 * MB))),
                _ => Err("Data could not be read right now, so its files can't be compared".to_string()),
            };
            async move { r }
        },
        &owners(),
        true,
        &cancel,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 3, "no more pages after the error");
    assert_eq!(*cursors.lock().unwrap(), vec![None, Some(4 * MB), Some(2 * MB)]);
    assert_eq!(g.batches.iter().map(|b| b.size).collect::<Vec<_>>(), vec![9 * MB, 4 * MB]);
    assert!(g.notice.expect("the error is said").contains("Data could not be read right now"));
}

#[tokio::test]
async fn paging_stops_at_the_last_page_and_never_loops_on_a_stuck_cursor() {
    let cancel = AtomicBool::new(false);
    let g = gather(|_| async { Ok((pg(9 * MB, "a", false), None)) }, &owners(), true, &cancel).await;
    assert_eq!(g.batches.len(), 1);
    assert!(g.notice.is_none());
    let calls = AtomicUsize::new(0);
    let g = gather(
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok((pg(9 * MB, "a", false), Some(9 * MB))) }
        },
        &owners(),
        true,
        &cancel,
    )
    .await;
    assert!(calls.load(Ordering::SeqCst) <= 2);
    assert!(g.notice.is_some());
}

#[tokio::test]
async fn cancel_stops_the_paging_between_pages() {
    let cancel = AtomicBool::new(false);
    let calls = AtomicUsize::new(0);
    let g = gather(
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            cancel.store(true, Ordering::SeqCst);
            async { Ok((pg(9 * MB, "a", false), Some(4 * MB))) }
        },
        &owners(),
        true,
        &cancel,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(g.batches.len(), 1);
    assert!(g.notice.is_none());
}

fn ready_page(groups: Vec<SizeGroup>) -> std::future::Ready<Page> {
    std::future::ready(Ok((groups, None)))
}

fn two_passes() -> ScanPlan {
    ScanPlan { ask_local: vec![id(1), id(2)], ask_linked: vec![id(9)], owners: owners(), notice: None, nothing_local: false }
}

/// A pager for `collect`: the local pass asks for the local drives only, the second one also for the linked.
fn fake_pager(local: impl Fn(usize) -> Page, both: impl Fn(usize) -> Page) -> impl FnMut(Vec<DeviceId>, Option<u64>) -> std::future::Ready<Page> {
    let (n_local, n_both) = (AtomicUsize::new(0), AtomicUsize::new(0));
    move |ids, _cursor| {
        let r = if ids.contains(&id(9)) { both(n_both.fetch_add(1, Ordering::SeqCst)) } else { local(n_local.fetch_add(1, Ordering::SeqCst)) };
        std::future::ready(r)
    }
}

#[tokio::test]
async fn a_failure_of_the_local_pager_is_about_the_users_drive_and_the_linked_pass_still_runs() {
    let fetch = fake_pager(
        |n| match n {
            0 => Ok((pg(9 * MB, "big", false), Some(4 * MB))),
            _ => Err("Data could not be read right now, so its files can't be compared".to_string()),
        },
        |_| Ok((pg(9 * MB, "big", true), None)),
    );
    let c = collect(fetch, &two_passes(), &AtomicBool::new(false)).await;
    assert_eq!(c.batches.len(), 1, "what the local pager found is scanned");
    assert!(c.local_notice.unwrap().contains("Data could not be read"));
    assert!(c.linked_notice.is_none());
    assert_eq!(c.linked.len(), 1);
    assert_eq!(c.linked[0].pc_name, "Laptop");
}

#[tokio::test]
async fn a_failure_of_the_linked_pass_loses_only_the_other_pcs_section() {
    let fetch = fake_pager(
        |_| Ok((pg(9 * MB, "big", false), None)),
        |n| match n {
            0 => Ok((pg(9 * MB, "big", true), Some(4 * MB))),
            _ => Err("Laptop could not be read right now".to_string()),
        },
    );
    let c = collect(fetch, &two_passes(), &AtomicBool::new(false)).await;
    assert_eq!(c.batches.len(), 1, "the scan's input does not depend on the linked pass");
    assert!(c.local_notice.is_none(), "an error here is not about the user's drive");
    assert!(c.linked.is_empty(), "a half-read linked pass shows nothing");
    assert!(c.linked_notice.unwrap().contains("other PCs"));
}

#[tokio::test]
async fn the_linked_pass_adds_no_scan_input_and_is_skipped_with_no_linked_drives() {
    let seen_both = AtomicUsize::new(0);
    let fetch = |ids: Vec<DeviceId>, _c: Option<u64>| {
        if ids.contains(&id(9)) {
            seen_both.fetch_add(1, Ordering::SeqCst);
        }
        ready_page(pg(9 * MB, "big", true))
    };
    let c = collect(fetch, &two_passes(), &AtomicBool::new(false)).await;
    assert_eq!(c.batches.len(), 1, "one batch from the local pass, none from the second");
    assert_eq!(seen_both.load(Ordering::SeqCst), 1);

    let mut plan = two_passes();
    plan.ask_linked.clear();
    seen_both.store(0, Ordering::SeqCst);
    let c = collect(|_ids: Vec<DeviceId>, _c: Option<u64>| ready_page(pg(9 * MB, "big", false)), &plan, &AtomicBool::new(false)).await;
    assert_eq!(c.batches.len(), 1);
    assert!(c.linked.is_empty() && c.linked_notice.is_none());
}

#[tokio::test]
async fn cancel_in_the_window_before_the_service_answers_stops_the_scan() {
    let s = Shared::default();
    let (gen, cancel) = s.begin().unwrap();
    assert!(s.is_running(), "running from the moment it is asked for");
    s.cancel(); // while the drives are still being listed
    assert!(cancel.load(Ordering::SeqCst));
    let calls = AtomicUsize::new(0);
    let c = collect(
        |_ids: Vec<DeviceId>, _c: Option<u64>| {
            calls.fetch_add(1, Ordering::SeqCst);
            ready_page(pg(9 * MB, "big", false))
        },
        &two_passes(),
        &cancel,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(c.batches.is_empty());
    let (seen, emit) = counting();
    s.finish(gen, &*emit);
    assert!(!s.is_running());
    assert_eq!(types(&seen), vec!["done"]);
}

#[test]
fn a_scan_whose_service_calls_fail_stops_running_and_can_be_started_again() {
    let s = Shared::default();
    let (gen, _) = s.begin().unwrap();
    assert!(s.begin().is_err());
    s.abort(gen);
    assert!(!s.is_running());
    let (seen, emit) = counting();
    s.finish(gen, &*emit);
    assert!(types(&seen).is_empty(), "an aborted start sends no done: the command itself failed");
    assert!(s.begin().is_ok());
}

#[test]
fn done_is_sent_once_and_the_scan_stops_running_even_if_the_scan_panics() {
    let s = Shared::default();
    let (seen, emit) = counting();
    let (gen, _) = s.begin().unwrap();
    run_guarded(&s, gen, &*emit, || panic!("the scan blew up"));
    assert!(!s.is_running());
    assert!(!s.state().running);
    s.finish(gen, &*emit);
    assert_eq!(types(&seen).iter().filter(|t| *t == "done").count(), 1);
    assert!(s.begin().is_ok());
}

#[test]
fn a_scan_that_panicked_says_its_results_may_be_incomplete() {
    let s = Shared::default();
    let (seen, emit) = counting();
    let (gen, _) = s.begin().unwrap();
    run_guarded(&s, gen, &*emit, || panic!("the scan blew up"));
    let notice = s.state().notice.expect("the stop is said");
    assert!(notice.contains("stopped early") && notice.contains("incomplete"), "{notice}");
    // the window hears the notice before the done
    assert_eq!(types(&seen), vec!["notice", "done"]);

    // a scan that ends normally says nothing of the kind
    let (gen, _) = s.begin().unwrap();
    run_guarded(&s, gen, &*quiet(), || {});
    assert!(s.state().notice.is_none());
}

#[test]
fn a_late_call_of_an_old_scan_does_not_touch_the_new_one() {
    let s = Shared::default();
    let (seen, emit) = counting();
    let (old, _) = s.begin().unwrap();
    s.finish(old, &*emit);
    let (new, _) = s.begin().unwrap();
    assert_ne!(old, new);

    s.finish(old, &*emit);
    s.on_event(old, ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*emit);
    s.on_event(old, ScanEvent::Progress { checked_bytes: 1, total_bytes: 2, current: None }, &*emit);
    s.add_notice(old, "old news", &*emit);
    s.keep_linked(old, vec![LinkedRow { name: "x".into(), size: 1, pc_name: "p".into(), path: "q".into() }]);
    s.abort(old);

    assert!(s.is_running(), "the new scan is still running");
    let st = s.state();
    assert!(st.found.is_empty() && st.notice.is_none() && st.total_bytes == 0);
    assert_eq!(types(&seen), vec!["done"], "only the old scan's own done was ever sent");
    s.finish(new, &*emit);
    assert_eq!(types(&seen), vec!["done", "done"]);
}

#[test]
fn done_follows_a_cancel_and_the_groups_found_stay() {
    let s = Shared::default();
    let (seen, emit) = counting();
    let (gen, cancel) = s.begin().unwrap();
    s.on_event(gen, ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*emit);
    s.on_event(gen, ScanEvent::Progress { checked_bytes: 3, total_bytes: 9, current: Some("C:\\a".into()) }, &*emit);
    s.cancel();
    assert!(cancel.load(Ordering::SeqCst));
    run_guarded(&s, gen, &*emit, || {});
    let st = s.state();
    assert!(!st.running);
    assert_eq!(st.found.len(), 1);
    assert_eq!(types(&seen), vec!["group", "progress", "done"]);
    assert_eq!(seen.lock().unwrap()[0]["group"]["copies"][0]["suggestedKeep"], false);
}

#[test]
fn a_second_scan_cannot_start_while_one_runs_and_a_notice_is_sent_and_kept() {
    let s = Shared::default();
    let (seen, emit) = counting();
    let (gen, _) = s.begin().unwrap();
    s.add_notice(gen, "Left out: Data", &*emit);
    assert!(s.begin().is_err());
    s.add_notice(gen, "Backup could not be read right now", &*emit);
    assert_eq!(s.state().notice.as_deref(), Some("Left out: Data Backup could not be read right now"));
    // each notice event carries the whole text, so the window shows it as soon as it is raised
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["type"], "notice");
    assert_eq!(last["text"], "Left out: Data Backup could not be read right now");
    s.finish(gen, &*quiet());
    assert!(s.state().notice.is_some(), "the notice stays after the scan ends");
}

#[test]
fn finishing_pairs_linked_files_with_the_groups_that_were_confirmed() {
    let s = Shared::default();
    let (gen, _) = s.begin().unwrap();
    s.keep_linked(
        gen,
        vec![
            LinkedRow { name: "x.bin".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\x.bin".into() },
            LinkedRow { name: "y.bin".into(), size: 5 * MB, pc_name: "Laptop".into(), path: "C:\\y.bin".into() },
        ],
    );
    s.on_event(gen, ScanEvent::Group(group(5 * MB, &["C:\\a\\x.bin", "C:\\b\\x.bin"])), &*quiet());
    assert!(s.state().other_pcs.is_empty(), "worked out when the scan ends");
    s.finish(gen, &*quiet());
    let names: Vec<String> = s.state().other_pcs.into_iter().map(|m| m.name).collect();
    assert_eq!(names, vec!["x.bin"]);
}

#[test]
fn a_scan_leaves_out_drives_still_on_their_first_scan_and_says_so() {
    let devices = vec![
        device(1, 100, DeviceState::Online, 5),
        device(2, 100, DeviceState::Scanning, 0), // first scan
        device(3, 100, DeviceState::Scanning, 7), // a rescan of an index that exists
        device(9, 200, DeviceState::Online, 5),   // a linked PC's drive
    ];
    let names = HashMap::from([(host(200), "Laptop".to_string())]);
    let plan = plan_scan(&[id(1), id(2), id(3)], &devices, host(100), &names);
    assert_eq!(plan.ask_local, vec![id(1), id(3)]);
    assert_eq!(plan.ask_linked, vec![id(9)]);
    assert!(!plan.nothing_local);
    let notice = plan.notice.expect("the left-out drive is named");
    assert!(notice.contains("Drive 2") && !notice.contains("Drive 3"), "{notice}");
    assert!(plan.owners[&id(1)].local && !plan.owners[&id(9)].local);
    assert_eq!(plan.owners[&id(9)].pc_name, "Laptop");
    let plan = plan_scan(&[id(2)], &devices, host(100), &names);
    assert!(plan.nothing_local && plan.ask_linked.is_empty());
}

#[test]
fn a_linked_drive_picked_in_the_sidebar_gets_a_plain_notice_not_a_silent_empty_scan() {
    let devices = vec![device(1, 100, DeviceState::Online, 5), device(9, 200, DeviceState::Online, 5)];
    let plan = plan_scan(&[id(9)], &devices, host(100), &HashMap::new());
    assert!(plan.nothing_local);
    assert!(plan.ask_local.is_empty() && plan.ask_linked.is_empty());
    let notice = plan.notice.expect("it says why nothing was scanned");
    assert!(notice.contains("this PC's drives"), "{notice}");
    assert!(!notice.contains("Another PC"), "{notice}");
}

#[test]
fn this_pc_is_found_from_peers_or_from_a_single_host_and_otherwise_not_at_all() {
    let one = vec![device(1, 100, DeviceState::Online, 1), device(2, 100, DeviceState::Online, 1)];
    let two = vec![device(1, 100, DeviceState::Online, 1), device(9, 200, DeviceState::Online, 1)];
    assert_eq!(this_host_of(Ok(host(200)), &two), Ok(host(200)));
    assert_eq!(this_host_of(Err("sync is disabled".into()), &one), Ok(host(100)));
    assert!(this_host_of(Err("sync is disabled".into()), &two).unwrap_err().contains("which drives"));
}

#[test]
fn an_unexpected_reply_is_logged_and_shown_as_a_plain_sentence() {
    let s = unexpected(Response::Ok);
    assert!(!s.contains("Ok") && !s.contains("Response") && !s.contains('{'), "{s}");
    assert!(s.ends_with('.'));
}

#[test]
fn reviews_are_dropped_with_the_results_and_kept_before_that() {
    let core = idle_core();
    let state = state_with(core.clone());
    let token = core.engine.prepare(vec![], &[]).token;
    state.window_hidden_for(Some(Duration::from_secs(299)));
    state.window_hidden_for(None);
    assert!(core.engine.commit(&token, &[], &[]).is_ok(), "a review is still good before five minutes");
    let token = core.engine.prepare(vec![], &[]).token;
    state.window_hidden_for(Some(Duration::from_secs(301)));
    assert!(core.engine.commit(&token, &[], &[]).unwrap_err().contains("no longer valid"));
}

#[test]
fn a_running_scan_keeps_its_results_and_reviews_while_hidden() {
    let core = idle_core();
    let state = state_with(core.clone());
    let token = core.engine.prepare(vec![], &[]).token;
    let (gen, _) = state.shared.begin().unwrap();
    state.shared.on_event(gen, ScanEvent::Group(group(5 * MB, &["C:\\a", "C:\\b"])), &*quiet());
    state.window_hidden_for(Some(mins(600)));
    assert_eq!(state.shared.state().found.len(), 1);
    assert!(core.engine.commit(&token, &[], &[]).is_ok());
}

// ---- removed files do not come back ---------------------------------------------------------------

fn found_group(s: &Shared, gen: u64, id_: u32, size: u64, copies: &[(&str, i64, Option<Lock>)]) {
    let g = DupGroup {
        id: id_,
        name: "x".into(),
        size,
        copies: copies
            .iter()
            .map(|(p, m, l)| DupCopy { path: p.to_string(), device_id: "d".into(), size, modified: *m, lock: *l, suggested_keep: false })
            .collect(),
        freeable: size,
    };
    s.on_event(gen, ScanEvent::Group(g), &*quiet());
}

#[test]
fn pruning_drops_moved_copies_and_groups_that_are_no_longer_groups() {
    let s = Shared::default();
    let (gen, _) = s.begin().unwrap();
    found_group(&s, gen, 1, 10, &[("D:\\Downloads\\a.bin", 1, None), ("D:\\keep\\a.bin", 9, None), ("D:\\other\\deeper\\a.bin", 2, None)]);
    found_group(&s, gen, 2, 10, &[("D:\\p\\b.bin", 1, None), ("D:\\q\\b.bin", 1, None)]);
    found_group(&s, gen, 3, 10, &[("C:\\Program Files\\c.bin", 1, Some(Lock::ProgramFiles)), ("D:\\r\\c.bin", 1, None)]);
    s.finish(gen, &*quiet());

    // spelled differently (case, slashes) from how the scan saw them
    s.prune(&["d:/downloads/A.bin".to_string(), "D:\\p\\b.bin".to_string()], &["downloads".to_string()]);
    let st = s.state();
    let ids: Vec<u32> = st.found.iter().map(|g| g.id).collect();
    assert_eq!(ids, vec![1, 3], "group 2 has one copy left and is gone");
    let g1 = &st.found[0];
    assert_eq!((g1.copies.len(), g1.freeable), (2, 10));
    assert_eq!(g1.copies.iter().filter(|c| c.suggested_keep).count(), 1);
    assert!(g1.copies[0].suggested_keep && g1.copies[0].path.contains("keep"), "the shorter path is now suggested");

    // the locked copy stays, so the other can still go; once it has gone nothing is left to free
    s.prune(&["D:\\r\\c.bin".to_string()], &[]);
    assert_eq!(s.state().found.iter().map(|g| g.id).collect::<Vec<_>>(), vec![1]);
}

/// Deletes the file it is given (temp files only) and reports a bin location, like the engine tests' fake.
struct DeletesTempFiles;
impl Recycler for DeletesTempFiles {
    fn recycle(&self, paths: &[PathBuf]) -> Vec<Result<String, String>> {
        paths
            .iter()
            .enumerate()
            .map(|(n, p)| {
                std::fs::remove_file(p).map_err(|e| e.to_string())?;
                Ok(format!("C:\\$Recycle.Bin\\S\\$R{n}"))
            })
            .collect()
    }
}

/// The real disk facts, but with no Recycle Bin pre-checks (the temp folder's drive is this PC's).
struct DiskOnly;
impl FileFacts for DiskOnly {
    fn stat(&self, p: &Path) -> Option<Stat> {
        RealFacts.stat(p)
    }
    fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String> {
        RealFacts.same_bytes(a, b)
    }
    fn file_id(&self, p: &Path) -> Option<(u32, u64)> {
        RealFacts.file_id(p)
    }
    fn recycle_block(&self, _: &Path, _: u64) -> Option<Lock> {
        None
    }
}

#[test]
fn after_a_commit_the_scan_state_no_longer_lists_what_was_moved() {
    let dir = tempfile::tempdir().unwrap();
    let b = dir.path();
    let s = |p: &str| b.join(p).to_string_lossy().into_owned();
    for d in ["win", "pf", "pd", "users/me/docs", "users/me/appdata"] {
        std::fs::create_dir_all(b.join(d)).unwrap();
    }
    let places = Places::for_tests(&s("win"), &[&s("pf")], &s("users"), &s("users/me"), &s("pd"), &[&s("users/me/appdata")]);
    let facts: Arc<dyn FileFacts> = Arc::new(DiskOnly);
    let engine = Engine::new(places.clone(), Arc::new(DeletesTempFiles), None, facts.clone(), Arc::new(ClaimsEveryFolder));
    let state = state_with(Arc::new(Core { engine, places, facts }));
    let core = state.core().unwrap();
    let names = ["a.bin", "b.bin", "c.bin"];
    let paths: Vec<PathBuf> = names.iter().map(|n| b.join("users/me/docs").join(n)).collect();
    for p in &paths {
        std::fs::write(p, b"same bytes").unwrap();
    }
    let (gen, _) = state.shared.begin().unwrap();
    let copies: Vec<(String, i64, Option<Lock>)> = paths.iter().map(|p| (p.to_string_lossy().into_owned(), 1, None)).collect();
    let refs: Vec<(&str, i64, Option<Lock>)> = copies.iter().map(|(p, m, l)| (p.as_str(), *m, *l)).collect();
    found_group(&state.shared, gen, 1, 10, &refs);
    state.shared.finish(gen, &*quiet());

    let prepared = core.engine.prepare(vec![Candidate::Duplicates { copies: paths.clone() }], &[]);
    let r = state.commit(&core, &prepared.token, &[0], &[]).unwrap();
    assert!(matches!(r.items[0].outcome, Outcome::Moved { .. }));
    assert!(!paths[0].exists());
    let after = state.shared.state().found;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].copies.len(), 2);
    assert!(after[0].copies.iter().all(|c| !c.path.ends_with("a.bin")));

    let prepared = core.engine.prepare(vec![Candidate::Duplicates { copies: paths[1..].to_vec() }], &[]);
    state.commit(&core, &prepared.token, &[0], &[]).unwrap();
    assert!(state.shared.state().found.is_empty(), "one copy left is no group");
    assert!(paths[2].exists(), "the last copy stays");

    // a rejected commit is a plain error and prunes nothing
    assert!(state.commit(&core, "no-such-token", &[0], &[]).is_err());
}

#[test]
fn an_uninstall_folder_request_is_refused_for_now() {
    let f = ReqFolder { path: r"C:\Users\me\AppData\Local\Old".into(), level: None, reasons: vec![] };
    let e = folder_candidates(Why::Uninstall, vec![f], &[]).unwrap_err();
    assert!(e.contains("Leftovers list"), "{e}");
}

#[test]
fn a_leftover_path_not_in_the_last_list_is_refused() {
    let shown = vec![folder_row(r"C:\Users\me\AppData\Local\Old", Level::Likely), stale_row(&stale(r"C:\ProgramData\Seek\indexes\a.seekidx"))];
    let listed = listed_set(&shown);
    // What was shown is accepted, however it is spelled.
    assert!(check_listed([r"c:\users\ME\appdata\local\old", r"C:\ProgramData\Seek\indexes\A.SEEKIDX"], &listed).is_ok());
    // Anything else is not, even a real folder one level up or somewhere else entirely.
    for p in [r"C:\Users\me\Documents\Photos", r"D:\Projects", r"C:\Users\me\AppData\Local", r"C:\Users\me\AppData\Local\Old\Sub"] {
        let e = check_listed([r"C:\Users\me\AppData\Local\Old", p], &listed).unwrap_err();
        assert!(e.contains(p), "{e}");
    }
    // Before any list was shown, nothing is accepted.
    assert!(check_listed([r"C:\x"], &listed_set(&[])).is_err());
}

#[test]
fn stale_rows_sort_by_size_within_their_level() {
    let mut small = folder_row(r"C:\a\small", Level::VeryLikely);
    small.size = 10;
    let mut big = folder_row(r"C:\a\big", Level::VeryLikely);
    big.size = 9_000;
    let mut file = stale(r"C:\ProgramData\Seek\indexes\mid.seekidx");
    file.size = 500;
    let rows = merge_leftovers(vec![small, big], Ok(vec![file]));
    let sizes: Vec<u64> = rows.iter().map(|r| r.size).collect();
    assert_eq!(sizes, vec![9_000, 500, 10]);
}

#[test]
fn index_facts_become_a_summary() {
    let fact = seek_core::FolderFact {
        size: 5,
        files: 2,
        newest_modified: 77,
        has_files: true,
        has_subfolders: true,
        has_programs: true,
        has_link: false,
        has_cloud: true,
        top_names: vec!["a".into()],
    };
    let s = summary_of(fact);
    assert_eq!((s.size, s.files, s.newest_ms), (5, 2, 77));
    assert!(s.has_files && s.subfolders && s.programs && s.cloud);
    assert_eq!(s.top, vec!["a".to_string()]);
}

#[test]
fn a_stale_row_says_what_the_file_is() {
    assert_eq!(stale_row(&stale(r"C:\ProgramData\Seek\indexes\a.seekidx")).contents, vec!["Seek's index for a drive it no longer knows".to_string()]);
    assert_eq!(stale_row(&stale(r"C:\ProgramData\Seek\journal\a.log")).contents, vec!["Seek's change log for a drive it no longer knows".to_string()]);
}

/// Read-only run on this PC against the running service: the scan with the index's facts, then 10
/// folders compared with what a walk of the disk says. Writes the list file; removes nothing.
/// If the running service does not know `FolderFacts` (an older build), it says so and stops.
#[test]
#[ignore = "reads this PC's folders and talks to the running service; writes only the list file"]
fn real_leftovers_list_with_index() {
    use leftovers::IndexFacts;
    use std::fmt::Write as _;
    let link = Arc::new(ServiceLink::default());
    let index = ServiceIndex(link.clone());
    let probe = tauri::async_runtime::block_on(link.request(Request::FolderFacts { paths: vec![r"C:\Windows".to_string()] }));
    match probe {
        Ok(Response::FolderFacts { .. }) => {}
        other => {
            println!("NO INDEX FACTS: the running service did not answer FolderFacts: {other:?}");
            return;
        }
    }
    let places = Places::from_system().expect("places");
    let claims = LiveClaims::new();
    let installed = claims::read_installed_names();
    let t = Instant::now();
    let rows = leftovers::scan(&places, &claims, &installed, &RealFacts, &index);
    let elapsed = t.elapsed();

    // Which folders did the index know? Ask again for the same rows.
    let paths: Vec<PathBuf> = rows.iter().map(|r| PathBuf::from(&r.path)).collect();
    let known = index.summaries(&paths);
    let in_index = known.iter().filter(|k| k.is_some()).count();

    let mut text = String::new();
    writeln!(text, "elapsed_ms: {} (with the service's index)", elapsed.as_millis()).unwrap();
    writeln!(text, "rows: {}, of which the index knew {}", rows.len(), in_index).unwrap();
    for level in [Level::VeryLikely, Level::Likely, Level::NotSure, Level::ProbablyInUse] {
        writeln!(text, "  {level:?}: {}", rows.iter().filter(|r| r.level == level).count()).unwrap();
    }
    // Ten folders of different sizes, index against walk.
    let mut by_size: Vec<usize> = (0..rows.len()).filter(|&i| known[i].is_some()).collect();
    by_size.sort_by_key(|&i| rows[i].size);
    let picks: Vec<usize> = (0..10).filter_map(|k| by_size.get(k * by_size.len().max(1) / 10).copied()).collect();
    writeln!(text, "\nindex vs walk (size, files, newest change):").unwrap();
    for i in picks {
        let (idx, walked) = (known[i].as_ref().unwrap(), leftovers::walk(&paths[i]));
        let w = match &walked {
            Some(w) => format!("size {} files {} newest {}", w.size, w.files, w.newest_ms),
            None => "walk failed".to_string(),
        };
        writeln!(text, "{} | index: size {} files {} newest {} | walk: {w}", rows[i].path, idx.size, idx.files, idx.newest_ms).unwrap();
    }
    writeln!(text, "\nlevel | size | name | reasons").unwrap();
    for r in &rows {
        let reasons: Vec<String> = r.reasons.iter().map(|x| format!("{:+} {}", x.points, x.text)).collect();
        writeln!(text, "{:?} | {} | {} | {}", r.level, r.size, r.path, reasons.join("; ")).unwrap();
    }
    std::fs::write(r"D:\Programming\seek\.superpowers\sdd\2026-11-05-seek-decimator\task-11-real-list-index.txt", text).unwrap();
}
