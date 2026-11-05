use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::*;
use crate::claims::InstalledNames;
use crate::facts::RealFacts;
use crate::model::Level;
use crate::paths::make_junction;

/// Folders named here (lowercase) are claimed.
#[derive(Default)]
struct Claims(HashSet<String>);

impl Claims {
    fn of(names: &[&str]) -> Claims {
        Claims(names.iter().map(|n| n.to_lowercase()).collect())
    }
}

impl ClaimCheck for Claims {
    fn is_claimed(&self, folder: &Path) -> Option<String> {
        let n = folder.file_name()?.to_string_lossy().to_lowercase();
        self.0.contains(&n).then(|| "The installed app Test".to_string())
    }
}

struct World {
    _dir: tempfile::TempDir,
    base: PathBuf,
    local: PathBuf,
    pf: PathBuf,
    places: Places,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().to_path_buf();
    let s = |p: &str| base.join(p).to_string_lossy().into_owned();
    for d in ["win", "pf", "pd", "users/me/appdata/local"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    let places = Places::for_tests(&s("win"), &[&s("pf")], &s("users"), &s("users/me"), &s("pd"), &[&s("users/me/appdata/local")]);
    World { local: base.join("users/me/appdata/local"), pf: base.join("pf"), base, places, _dir: dir }
}

fn put(dir: &Path, name: &str, bytes: usize) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(name), vec![b'x'; bytes]).unwrap();
}

fn names() -> InstalledNames {
    InstalledNames::default()
}

/// Two years after everything the test makes, so folders count as old.
fn later() -> i64 {
    unix_now_ms() + 2 * YEAR_MS
}

fn run(w: &World, claims: &dyn ClaimCheck, installed: &InstalledNames, now: i64) -> Vec<LeftoverRow> {
    scan_with(&w.places, claims, installed, now, &NoIndex, &walk).rows
}

fn find<'a>(rows: &'a [LeftoverRow], name: &str) -> &'a LeftoverRow {
    rows.iter().find(|r| r.path.ends_with(&format!("\\{name}"))).unwrap_or_else(|| panic!("{name} is not listed: {rows:?}"))
}

fn has_reason(r: &LeftoverRow, text: &str) -> bool {
    r.reasons.iter().any(|x| x.text.contains(text))
}

#[test]
fn a_claimed_folder_is_not_listed() {
    let w = world();
    put(&w.local.join("Alive"), "a.txt", 5);
    put(&w.local.join("Dead"), "a.txt", 5);
    let rows = run(&w, &Claims::of(&["alive"]), &names(), later());
    assert_eq!(rows.len(), 1);
    assert!(rows[0].path.ends_with("Dead"));
}

#[test]
fn only_top_level_folders_are_candidates() {
    let w = world();
    put(&w.local.join("Outer").join("Inner").join("Deepest"), "a.txt", 5);
    put(&w.local, "loose-file.txt", 5);
    let rows = run(&w, &Claims::default(), &names(), later());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].path.ends_with("Outer"));
    assert_eq!(rows[0].parent, w.local.to_string_lossy());
    assert_eq!(rows[0].contents, vec!["Inner".to_string()]);
}

#[test]
fn a_junction_is_not_listed_and_not_followed() {
    let w = world();
    let outside = w.base.join("outside");
    for f in ["one.bin", "two.bin", "three.bin"] {
        put(&outside, f, 1000);
    }
    put(&w.local.join("Real"), "mine.txt", 10);
    assert!(make_junction(&w.local.join("Real").join("jump"), &outside));
    assert!(make_junction(&w.local.join("Linked"), &outside));
    let rows = run(&w, &Claims::default(), &names(), later());
    assert!(!rows.iter().any(|r| r.path.ends_with("Linked")), "a top-level link is listed: {rows:?}");
    let real = find(&rows, "Real");
    assert_eq!(real.size, 10, "the link was followed");
    assert_eq!(real.contents, vec!["jump".to_string(), "mine.txt".to_string()]);
    // The target is untouched.
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 3);
}

#[test]
fn empty_folder_scores_high() {
    let w = world();
    std::fs::create_dir_all(w.local.join("Hollow").join("also-empty")).unwrap();
    std::fs::create_dir_all(w.local.join("Bare")).unwrap();
    let rows = run(&w, &Claims::default(), &names(), later());
    let bare = find(&rows, "Bare");
    assert_eq!(bare.level, Level::VeryLikely);
    assert!(has_reason(bare, "Empty"));
    assert!(has_reason(bare, "no subfolders"));
    assert!(has_reason(bare, "year"));
    assert_eq!(bare.size, 0);
    // Nothing but empty folders still counts as empty, but it does have a subfolder.
    let hollow = find(&rows, "Hollow");
    assert!(has_reason(hollow, "Empty"));
    assert!(!has_reason(hollow, "no subfolders"));
    assert_eq!(hollow.level, Level::VeryLikely);
}

#[test]
fn a_folder_that_just_changed_scores_lower() {
    let w = world();
    std::fs::create_dir_all(w.local.join("Fresh")).unwrap();
    let rows = run(&w, &Claims::default(), &names(), unix_now_ms());
    let fresh = find(&rows, "Fresh");
    assert!(has_reason(fresh, "last 30 days"));
    assert!(!has_reason(fresh, "year"));
    assert_eq!(fresh.level, Level::Likely); // 4 + 2 - 3
}

#[test]
fn a_folder_of_programs_scores_low() {
    let w = world();
    put(&w.local.join("Tool"), "tool.EXE", 10);
    put(&w.local.join("Tool"), "helper.dll", 10);
    std::fs::create_dir_all(w.local.join("Tool").join("sub")).unwrap();
    let rows = run(&w, &Claims::default(), &names(), later());
    let tool = find(&rows, "Tool");
    assert!(has_reason(tool, "Contains programs"));
    assert_eq!(tool.level, Level::ProbablyInUse); // 2 - 4
    assert_eq!(tool.size, 20);
}

#[test]
fn a_big_folder_has_many_files_and_only_a_few_names_are_kept() {
    let w = world();
    let big = w.local.join("Big");
    std::fs::create_dir_all(&big).unwrap();
    for i in 0..(MANY_FILES + 5) {
        std::fs::write(big.join(format!("f{i:05}.txt")), b"").unwrap();
    }
    let rows = run(&w, &Claims::default(), &names(), later());
    let big = find(&rows, "Big");
    assert!(has_reason(big, "many files"));
    assert_eq!(big.contents.len(), CONTENTS_SHOWN);
    assert_eq!(big.contents[0], "f00000.txt");
    assert_eq!(big.contents[CONTENTS_SHOWN - 1], format!("f{:05}.txt", CONTENTS_SHOWN - 1));
}

#[test]
fn a_generic_name_scores_lower() {
    let w = world();
    put(&w.local.join("Common"), "a.txt", 1);
    put(&w.local.join("Zeta Thing"), "a.txt", 1);
    let rows = run(&w, &Claims::default(), &names(), later());
    assert!(has_reason(find(&rows, "Common"), "Generic"));
    assert!(!has_reason(find(&rows, "Zeta Thing"), "Generic"));
}

#[test]
fn size_and_date_come_from_what_is_inside() {
    let w = world();
    put(&w.local.join("Sized").join("a").join("b"), "x.bin", 700);
    put(&w.local.join("Sized"), "y.bin", 300);
    let rows = run(&w, &Claims::default(), &names(), later());
    let r = find(&rows, "Sized");
    assert_eq!(r.size, 1000);
    assert!(r.modified > 1_600_000_000_000 && r.modified <= unix_now_ms() + 1000, "{}", r.modified);
}

#[test]
fn rows_are_sorted_by_confidence_then_size() {
    let w = world();
    std::fs::create_dir_all(w.local.join("Hollow")).unwrap(); // very likely, 0 bytes
    put(&w.local.join("Small"), "a.txt", 10); // likely
    put(&w.local.join("Large"), "a.txt", 5000); // likely, bigger
    put(&w.local.join("Prog"), "a.exe", 10);
    std::fs::create_dir_all(w.local.join("Prog").join("s")).unwrap(); // probably in use
    let rows = run(&w, &Claims::default(), &names(), later());
    let order: Vec<&str> = rows.iter().map(|r| r.path.rsplit('\\').next().unwrap()).collect();
    assert_eq!(order, vec!["Hollow", "Large", "Small", "Prog"], "{rows:?}");
}

#[test]
fn a_program_files_folder_needs_admin() {
    let w = world();
    put(&w.pf.join("Old Vendor"), "a.txt", 1);
    put(&w.local.join("Old Vendor"), "a.txt", 1);
    let rows = run(&w, &Claims::default(), &names(), later());
    let admin: Vec<bool> = rows.iter().map(|r| r.needs_admin).collect();
    assert_eq!(admin.iter().filter(|a| **a).count(), 1);
    assert!(rows.iter().find(|r| r.needs_admin).unwrap().path.starts_with(&*w.pf.to_string_lossy()));
}

#[test]
fn an_unreadable_folder_does_not_fail_the_scan() {
    let w = world();
    for n in ["Good", "Bad", "Also Good"] {
        put(&w.local.join(n), "a.txt", 1);
    }
    let walker = |p: &Path| if p.ends_with("Bad") { None } else { walk(p) };
    let out = scan_with(&w.places, &Claims::default(), &names(), later(), &NoIndex, &walker);
    assert_eq!(out.rows.len(), 2);
    assert_eq!(out.left_out, 1);
    assert!(!out.rows.iter().any(|r| r.path.ends_with("Bad")));
}

#[test]
fn a_folder_that_vanishes_mid_scan_is_dropped() {
    let w = world();
    put(&w.local.join("Stays"), "a.txt", 1);
    put(&w.local.join("Goes"), "a.txt", 1);
    // The folder is deleted after it was listed and before it is walked.
    let gone = w.local.join("Goes");
    let walker = |p: &Path| {
        if p.ends_with("Goes") {
            std::fs::remove_dir_all(&gone).unwrap();
        }
        walk(p)
    };
    let out = scan_with(&w.places, &Claims::default(), &names(), later(), &NoIndex, &walker);
    assert_eq!(out.rows.len(), 1);
    assert!(out.rows[0].path.ends_with("Stays"));
}

#[test]
fn a_missing_root_is_skipped() {
    let w = world();
    std::fs::remove_dir_all(&w.pf).unwrap();
    put(&w.local.join("Left"), "a.txt", 1);
    assert_eq!(run(&w, &Claims::default(), &names(), later()).len(), 1);
}

#[test]
fn cloud_and_link_entries_are_never_entered() {
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    for a in [OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS] {
        assert_eq!(kind_of(a), Kind::Cloud);
        assert_eq!(kind_of(a | ATTR_DIRECTORY | ATTR_REPARSE), Kind::Cloud);
    }
    assert_eq!(kind_of(ATTR_DIRECTORY | ATTR_REPARSE), Kind::Link);
    assert_eq!(kind_of(ATTR_REPARSE), Kind::Link);
    assert_eq!(kind_of(ATTR_DIRECTORY), Kind::Dir);
    assert_eq!(kind_of(0x20), Kind::File);
}

#[test]
fn a_folder_named_inside_an_installed_apps_name_scores_lower() {
    let w = world();
    put(&w.local.join("Docker"), "a.txt", 1);
    put(&w.local.join("Zork"), "a.txt", 1);
    let plain = run(&w, &Claims::default(), &names(), later());
    let installed = InstalledNames { apps: vec!["Docker Desktop".into(), "Zork Deluxe".into()], publishers: vec![] };
    let with = run(&w, &Claims::default(), &installed, later());
    let (before, after) = (find(&plain, "Docker"), find(&with, "Docker"));
    assert!(has_reason(after, "similar name"));
    assert!(!has_reason(before, "similar name"));
    let pts = |r: &LeftoverRow| r.reasons.iter().map(|x| x.points).sum::<i32>();
    assert_eq!(pts(after), pts(before) - 2);
    // A name of four letters or fewer is never matched.
    assert!(!has_reason(find(&with, "Zork"), "similar name"));
}

#[test]
fn a_folder_named_after_a_publisher_with_an_installed_app_scores_low() {
    let w = world();
    put(&w.local.join("Google"), "a.txt", 1);
    put(&w.local.join("NVIDIA Corporation"), "a.txt", 1);
    put(&w.local.join("Unrelated Studio"), "a.txt", 1);
    let installed = InstalledNames { apps: vec![], publishers: vec!["Google LLC".into(), "NVIDIA Corporation".into()] };
    let rows = run(&w, &Claims::default(), &installed, later());
    for n in ["Google", "NVIDIA Corporation"] {
        let r = find(&rows, n);
        assert!(has_reason(r, "publisher still has an app"), "{n}");
        assert!(r.reasons.iter().any(|x| x.points == -4));
    }
    assert!(!has_reason(find(&rows, "Unrelated Studio"), "publisher still has an app"));
    // -4 against "no subfolders" (+1) and "a year" (+2) is below zero.
    assert_eq!(find(&rows, "Google").level, Level::ProbablyInUse); // 1 + 2 - 4
}

#[test]
fn nothing_is_called_safe() {
    let w = world();
    std::fs::create_dir_all(w.local.join("Hollow")).unwrap();
    put(&w.local.join("Tool"), "a.exe", 1);
    put(&w.local.join("Docker"), "a.txt", 1);
    let installed = InstalledNames { apps: vec!["Docker Desktop".into()], publishers: vec!["Docker Inc".into()] };
    let rows = run(&w, &Claims::default(), &installed, later());
    let mut seen: Vec<String> = rows.iter().flat_map(|r| r.reasons.iter().map(|x| x.text.clone())).collect();
    seen.extend(crate::confidence::POINTS.iter().map(|(_, _, t)| t.to_string()));
    for level in [Level::VeryLikely, Level::Likely, Level::NotSure, Level::ProbablyInUse] {
        seen.push(serde_json::to_string(&level).unwrap());
    }
    assert!(seen.len() > 20);
    for t in &seen {
        assert!(!t.to_lowercase().contains("safe"), "{t}");
    }
    // The facts argument is part of the signature even though the walk reads the disk itself.
    let all = scan(&w.places, &Claims::default(), &installed, &RealFacts, &NoIndex);
    assert_eq!(all.len(), rows.len());
}

/// Read-only run on this PC: lists what the scan finds and writes it to a file. Removes nothing.
#[test]
#[ignore = "reads this PC's Program Files and AppData; writes only the list file"]
fn real_leftovers_list() {
    use std::fmt::Write as _;
    let places = Places::from_system().expect("places");
    let t = std::time::Instant::now();
    let claims = crate::claims::LiveClaims::new();
    let installed = crate::claims::read_installed_names();
    let t_names = t.elapsed();
    // Warm the claims so their cost is not charged to the walk.
    let _ = claims.is_claimed(Path::new(r"C:\warm-up"));
    let t_claims = t.elapsed();
    let per_cell: std::cell::RefCell<Vec<(std::time::Duration, String)>> = std::cell::RefCell::new(Vec::new());
    let timed = |p: &Path| {
        let t0 = std::time::Instant::now();
        let r = walk(p);
        (t0.elapsed(), r)
    };
    let cell = &per_cell;
    let walker = |p: &Path| {
        let (d, r) = timed(p);
        cell.borrow_mut().push((d, p.to_string_lossy().into_owned()));
        r
    };
    let started = std::time::Instant::now();
    let out = scan_with(&places, &claims, &installed, unix_now_ms(), &NoIndex, &walker);
    let elapsed = started.elapsed();
    let mut per = per_cell.into_inner();

    let total = |r: &LeftoverRow| r.reasons.iter().map(|x| x.points).sum::<i32>();
    let mut text = String::new();
    writeln!(text, "elapsed_ms: {}", elapsed.as_millis()).unwrap();
    writeln!(text, "installed names read in ms: {}; claims warmed by ms: {}", t_names.as_millis(), t_claims.as_millis()).unwrap();
    writeln!(text, "installed apps: {}, publishers: {}", installed.apps.len(), installed.publishers.len()).unwrap();
    writeln!(text, "rows: {}, left out (unreadable, vanished or cloud): {}", out.rows.len(), out.left_out).unwrap();
    for level in [Level::VeryLikely, Level::Likely, Level::NotSure, Level::ProbablyInUse] {
        writeln!(text, "  {level:?}: {}", out.rows.iter().filter(|r| r.level == level).count()).unwrap();
    }
    writeln!(text, "\nlevel | total | size | name | reasons").unwrap();
    for r in &out.rows {
        let reasons: Vec<String> = r.reasons.iter().map(|x| format!("{:+} {}", x.points, x.text)).collect();
        writeln!(text, "{:?} | {} | {} | {} | {}", r.level, total(r), r.size, r.path, reasons.join("; ")).unwrap();
    }
    per.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
    writeln!(text, "\nslowest walks:").unwrap();
    for (d, p) in per.iter().take(10) {
        writeln!(text, "  {} ms  {p}", d.as_millis()).unwrap();
    }
    let file = r"D:\Programming\seek\.superpowers\sdd\2026-11-05-seek-decimator\task-11-real-list.txt";
    std::fs::write(file, text).unwrap();
}

/// Answers for folders by name; `None` for the rest. Remembers what it was asked.
struct FakeIndex {
    known: Vec<(&'static str, Summary)>,
    asked: std::sync::Mutex<Vec<Vec<String>>>,
}

impl IndexFacts for FakeIndex {
    fn summaries(&self, folders: &[PathBuf]) -> Vec<Option<Summary>> {
        self.asked.lock().unwrap().push(folders.iter().map(|p| p.to_string_lossy().into_owned()).collect());
        folders.iter().map(|p| self.known.iter().find(|(n, _)| p.ends_with(n)).map(|(_, s)| s.clone())).collect()
    }
}

#[test]
fn index_facts_are_used_and_the_walk_runs_only_for_none() {
    let w = world();
    for n in ["InIndex", "NotInIndex", "Online"] {
        put(&w.local.join(n), "real.txt", 3);
    }
    let from_index = Summary { size: 12_345, files: 4, newest_ms: 1_000, subfolders: true, has_files: true, top: vec!["from-index".into()], ..Summary::default() };
    let index = FakeIndex {
        known: vec![("InIndex", from_index), ("Online", Summary { cloud: true, size: 1, ..Summary::default() })],
        asked: Default::default(),
    };
    let walked = std::sync::Mutex::new(Vec::new());
    let walker = |p: &Path| {
        walked.lock().unwrap().push(p.file_name().unwrap().to_string_lossy().into_owned());
        walk(p)
    };
    let out = scan_with(&w.places, &Claims::default(), &names(), later(), &index, &walker);
    // One question for every candidate, not one per folder.
    let asked = index.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].len(), 3);
    // The walk ran for the folder the index didn't know, and only for it.
    assert_eq!(*walked.lock().unwrap(), vec!["NotInIndex".to_string()]);
    let r = find(&out.rows, "InIndex");
    assert_eq!((r.size, r.contents.clone()), (12_345, vec!["from-index".to_string()]));
    assert_eq!(find(&out.rows, "NotInIndex").size, 3);
    // A folder holding something online-only is left out, as the walk leaves it out.
    assert!(!out.rows.iter().any(|r| r.path.ends_with("Online")));
    assert_eq!(out.left_out, 1);
}
