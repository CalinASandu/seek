//! `for_app`: what an uninstall left behind. Real temp folders, a fake index, a fake shortcut reader.

use super::*;
use crate::model::{AppRow, FolderSource};

fn app(name: &str, publisher: &str, folder: Option<&Path>, source: Option<FolderSource>) -> AppRow {
    AppRow {
        id: r"HKLM64\{test}".into(),
        name: name.into(),
        publisher: publisher.into(),
        version: "1".into(),
        size: 0,
        install_date: None,
        icon: None,
        folder: folder.map(|f| f.to_string_lossy().into_owned()),
        folder_source: source,
        folder_guessed: source.is_some_and(|s| s != FolderSource::InstallLocation),
        can_uninstall: true,
        note: None,
        running: false,
    }
}

/// The user's Start Menu `Programs` folder, then the all-users one.
fn menus(w: &World) -> (PathBuf, PathBuf) {
    let r = w.places.start_menu_roots();
    (r[0].clone(), r[1].clone())
}

type Links = dyn Fn(&[PathBuf]) -> Vec<Option<String>>;

/// What a shortcut points at, by the shortcut's file name. Any other shortcut can't be read.
fn targets(table: &[(&str, &Path)]) -> impl Fn(&[PathBuf]) -> Vec<Option<String>> {
    let table: Vec<(String, String)> = table.iter().map(|(n, t)| (n.to_lowercase(), t.to_string_lossy().into_owned())).collect();
    move |files| {
        files
            .iter()
            .map(|f| {
                let name = f.file_name().unwrap().to_string_lossy().to_lowercase();
                table.iter().find(|(n, _)| *n == name).map(|(_, t)| t.clone())
            })
            .collect()
    }
}

fn unreadable(files: &[PathBuf]) -> Vec<Option<String>> {
    vec![None; files.len()]
}

fn after(w: &World, app: &AppRow, claims: &dyn ClaimCheck, links: &Links) -> Vec<LeftoverRow> {
    after_with(w, app, claims, &names(), links)
}

fn after_with(w: &World, app: &AppRow, claims: &dyn ClaimCheck, installed: &InstalledNames, links: &Links) -> Vec<LeftoverRow> {
    for_app_with(app, &w.places, claims, installed, later(), &NoIndex, &walk, links)
}

fn depth_reasons(r: &LeftoverRow) -> usize {
    r.reasons.iter().filter(|x| x.text.contains("level below the top")).count()
}

fn listed(rows: &[LeftoverRow], path: &Path) -> bool {
    rows.iter().any(|r| r.path.eq_ignore_ascii_case(&path.to_string_lossy()))
}

fn total(r: &LeftoverRow) -> i32 {
    r.reasons.iter().map(|x| x.points).sum()
}

#[test]
fn for_app_goes_at_most_two_levels_deep() {
    let w = world();
    let a = app("Acme Editor", "Initech", None, None);
    let top = w.pf.join("Acme Editor");
    let one = w.pf.join("Vendor").join("Acme Editor");
    let two = w.local.join("Group").join("Sub").join("Acme Editor");
    let three = w.local.join("Group").join("Sub").join("Deep").join("Acme Editor");
    for p in [&top, &one, &two, &three] {
        put(p, "a.txt", 5);
    }
    let rows = after(&w, &a, &Claims::default(), &unreadable);
    assert!(listed(&rows, &top) && listed(&rows, &one) && listed(&rows, &two), "{rows:?}");
    assert!(!listed(&rows, &three), "three levels down is out of reach: {rows:?}");
    // Folders on the way are not listed just for being on the way.
    assert!(!listed(&rows, &w.pf.join("Vendor")) && !listed(&rows, &w.local.join("Group")));
    let depth = |p: &Path| depth_reasons(rows.iter().find(|r| r.path.eq_ignore_ascii_case(&p.to_string_lossy())).unwrap());
    assert_eq!((depth(&top), depth(&one), depth(&two)), (0, 1, 2));
}

#[test]
fn leftovers_after_uninstall_include_the_install_folder_and_shortcuts_and_nothing_is_ticked() {
    let w = world();
    let install = w.base.join("apps").join("Acme Editor");
    put(&install, "editor.exe", 20);
    put(&install.join("docs"), "manual.pdf", 20);
    let a = app("Acme Editor", "Initech", Some(&install), Some(FolderSource::InstallLocation));
    let (user_menu, all_menu) = menus(&w);
    put(&user_menu, "Acme Editor.lnk", 1);
    put(&all_menu.join("Acme Editor"), "Manual.lnk", 1);
    put(&user_menu, "Unrelated.lnk", 1);
    let elsewhere = w.base.join("elsewhere.exe");
    std::fs::write(&elsewhere, b"x").unwrap();
    let links = targets(&[
        ("Acme Editor.lnk", &install.join("gone.exe")),
        ("Manual.lnk", &install.join("docs").join("gone.pdf")),
        ("Unrelated.lnk", &elsewhere),
    ]);
    let rows = after(&w, &a, &Claims::default(), &links);

    let folder = rows.iter().find(|r| r.path.eq_ignore_ascii_case(&install.to_string_lossy())).expect("the install folder is a row");
    assert!(has_reason(folder, "install folder"), "{folder:?}");
    assert!(folder.size >= 40);
    assert!(!folder.needs_admin, "it is not in Program Files");
    let name_and_target = rows.iter().find(|r| r.path.ends_with("Acme Editor.lnk")).expect("shortcut by name and target");
    assert_eq!(name_and_target.level, Level::VeryLikely, "{name_and_target:?}");
    let by_target = rows.iter().find(|r| r.path.ends_with("Manual.lnk")).expect("shortcut by target");
    assert!(by_target.needs_admin, "the all-users Start Menu is in ProgramData");
    assert!(!name_and_target.needs_admin);
    assert!(!rows.iter().any(|r| r.path.ends_with("Unrelated.lnk")), "a shortcut to something that works is not a leftover");
    assert_eq!(rows.len(), 3, "{rows:?}");
    // Most likely first.
    let scores: Vec<i32> = rows.iter().map(total).collect();
    assert!(scores.windows(2).all(|p| p[0] >= p[1]), "{scores:?}");
    // A row is only a row. It carries no selection, so the sheet ticks nothing.
    let json = serde_json::to_string(&rows).unwrap();
    assert!(!json.contains("ticked") && !json.contains("selected") && !json.contains("checked"));
}

#[test]
fn a_guessed_folder_is_never_a_row_but_a_registered_one_in_program_files_needs_admin() {
    let w = world();
    let guessed = w.base.join("apps").join("Initech Tools");
    put(&guessed, "a.exe", 5);
    let a = app("Acme Editor", "Initech", Some(&guessed), Some(FolderSource::Icon));
    assert!(after(&w, &a, &Claims::default(), &unreadable).is_empty());

    let registered = w.pf.join("Initech Tools");
    put(&registered, "a.exe", 5);
    let a = app("Acme Editor", "Initech", Some(&registered), Some(FolderSource::InstallLocation));
    let rows = after(&w, &a, &Claims::default(), &unreadable);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].needs_admin);
}

#[test]
fn an_install_folder_that_is_gone_is_no_row_and_the_shortcuts_still_go_by_its_path() {
    let w = world();
    let install = w.pf.join("Acme Editor");
    let a = app("Acme Editor", "Initech", Some(&install), Some(FolderSource::InstallLocation));
    let (user_menu, _) = menus(&w);
    put(&user_menu, "Editor.lnk", 1);
    let links = targets(&[("Editor.lnk", &install.join("editor.exe"))]);
    let rows = after(&w, &a, &Claims::default(), &links);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].path.ends_with("Editor.lnk"));
}

#[test]
fn names_of_four_letters_or_fewer_never_match_and_a_rough_match_scores_lower() {
    let w = world();
    put(&w.local.join("Zoom"), "a.txt", 5);
    let short = app("Zoom", "Zoom Video", None, None);
    assert!(after(&w, &short, &Claims::default(), &unreadable).is_empty());

    put(&w.local.join("Acme Editor"), "a.txt", 5);
    put(&w.local.join("Acme Editor Plus"), "a.txt", 5);
    let a = app("Acme Editor", "Initech", None, None);
    let rows = after(&w, &a, &Claims::default(), &unreadable);
    let exact = find(&rows, "Acme Editor");
    let rough = find(&rows, "Acme Editor Plus");
    assert!(has_reason(exact, "exactly"));
    assert!(has_reason(rough, "roughly"));
    assert!(!has_reason(rough, "exactly"));
    assert_eq!(total(exact) - total(rough), 4, "exact is +2, rough is -2, the rest is equal");
}

#[test]
fn roughly_means_one_name_holds_the_other_as_whole_words() {
    assert_eq!(match_name("Acme Editor Plus", "Acme Editor"), NameMatch::Rough);
    assert_eq!(match_name("Acme", "Acme Editor"), NameMatch::None, "four letters never match");
    assert_eq!(match_name("Acme Edit", "Acme Editor"), NameMatch::None, "half a word is no match");
    assert_eq!(match_name("acme-editor", "Acme Editor"), NameMatch::Exact);
    assert_eq!(match_name("Acme Editor", "Acme Editor (x64)"), NameMatch::Rough, "a match that needed build words trimmed is rough");
    assert_eq!(match_name("Python", "Python 3.12.4 (64-bit)"), NameMatch::Rough);
    assert_eq!(match_name("Anything", ""), NameMatch::None);
    assert_eq!(match_name("", "Acme Editor"), NameMatch::None);
}

#[test]
fn a_folder_under_the_publishers_folder_needs_its_own_name_match_and_the_publishers_folder_is_never_a_row() {
    let w = world();
    // Adobe/Premiere Pro matches; Adobe/CameraRaw is another Adobe product's and does not.
    put(&w.local.join("Adobe").join("Premiere Pro"), "a.txt", 5);
    put(&w.local.join("Adobe").join("CameraRaw"), "a.txt", 5);
    let a = app("Premiere Pro", "Adobe Systems", None, None);
    let rows = after(&w, &a, &Claims::default(), &unreadable);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = find(&rows, "Premiere Pro");
    assert!(has_reason(r, "publisher"), "{r:?}");
    assert!(has_reason(r, "exactly"));
    assert_eq!(depth_reasons(r), 1);
    assert!(!listed(&rows, &w.local.join("Adobe")), "the publisher's own folder may hold other apps");
}

#[test]
fn google_chrome_is_found_under_google_and_google_is_not() {
    let w = world();
    put(&w.local.join("Google").join("Chrome"), "a.txt", 5);
    put(&w.local.join("Google").join("Drive"), "a.txt", 5);
    let a = app("Google Chrome", "Google LLC", None, None);
    let rows = after(&w, &a, &Claims::default(), &unreadable);
    assert!(listed(&rows, &w.local.join("Google").join("Chrome")), "{rows:?}");
    assert!(!listed(&rows, &w.local.join("Google")), "{rows:?}");
    assert_eq!(rows.len(), 1, "{rows:?}");
}

#[test]
fn a_microsoft_apps_leftovers_never_reach_shared_microsoft_folders() {
    let w = world();
    for n in ["Protect", "Credentials", "Vault", "SystemCertificates", "Templates", "Signatures", "Edge"] {
        put(&w.local.join("Microsoft").join(n), "a.txt", 5);
    }
    put(&w.local.join("Microsoft").join("Windows").join("PowerShell"), "a.txt", 5);
    put(&w.pf.join("Microsoft").join("EdgeCore"), "a.exe", 5);
    put(&w.pf.join("PowerShell 7"), "a.exe", 5);
    put(&w.local.join("WindowsPowerShell"), "a.txt", 5);
    for (name, publisher) in [("Microsoft Edge", "Microsoft Corporation"), ("PowerShell 7-x64", "Microsoft Corporation"), ("Microsoft Corporation Thing", "Microsoft Corporation")] {
        let rows = after(&w, &app(name, publisher, None, None), &Claims::default(), &unreadable);
        for r in &rows {
            let p = r.path.to_lowercase();
            assert!(!p.contains("microsoft") && !p.contains("windowspowershell"), "{name}: {}", r.path);
        }
    }
}

#[test]
fn every_row_is_less_likely_while_the_publisher_still_has_an_app() {
    let w = world();
    put(&w.local.join("Adobe").join("Premiere Pro"), "a.txt", 5);
    let a = app("Premiere Pro", "Adobe Systems", None, None);
    let alone = after(&w, &a, &Claims::default(), &unreadable);
    let installed = InstalledNames { apps: vec!["Photoshop".into()], publishers: vec!["Adobe Systems Incorporated".into(), "Adobe Inc.".into()] };
    let with = after_with(&w, &a, &Claims::default(), &installed, &unreadable);
    assert!(has_reason(&with[0], "publisher still has an app"), "{:?}", with[0]);
    assert_eq!(total(&alone[0]) - total(&with[0]), 4);
    // Shortcuts too.
    let (user_menu, _) = menus(&w);
    put(&user_menu, "Premiere Pro.lnk", 1);
    let dead = w.base.join("gone.exe");
    let rows = after_with(&w, &a, &Claims::default(), &installed, &targets(&[("Premiere Pro.lnk", &dead)]));
    let lnk = rows.iter().find(|r| r.path.ends_with(".lnk")).unwrap();
    assert!(has_reason(lnk, "publisher still has an app"));
    // A different publisher doesn't count.
    let other = InstalledNames { apps: vec![], publishers: vec!["Contoso".into()] };
    assert!(!has_reason(&after_with(&w, &a, &Claims::default(), &other, &unreadable)[0], "publisher still has an app"));
}

#[test]
fn a_shortcut_to_something_that_still_works_is_not_listed_whatever_its_name_or_place() {
    let w = world();
    let install = w.base.join("apps").join("Acme Editor");
    put(&install, "editor.exe", 5);
    let a = app("Acme Editor", "Initech", Some(&install), Some(FolderSource::InstallLocation));
    let (user_menu, _) = menus(&w);
    put(&user_menu, "Acme Editor.lnk", 1);
    put(&user_menu, "Acme Help.lnk", 1);
    let rows = after(&w, &a, &Claims::default(), &targets(&[("Acme Editor.lnk", &install.join("editor.exe")), ("Acme Help.lnk", &install.join("editor.exe"))]));
    assert!(rows.iter().all(|r| !r.path.ends_with(".lnk")), "{rows:?}");
}

#[test]
fn a_guessed_folder_gives_no_install_folder_points_to_a_shortcut() {
    let w = world();
    let guessed = w.base.join("apps").join("Initech Tools");
    put(&guessed, "a.exe", 5);
    let a = app("Acme Editor", "Initech", Some(&guessed), Some(FolderSource::Icon));
    let (user_menu, _) = menus(&w);
    put(&user_menu, "Acme Editor.lnk", 1);
    put(&user_menu, "Other Thing.lnk", 1);
    let dead = guessed.join("gone.exe");
    let rows = after(&w, &a, &Claims::default(), &targets(&[("Acme Editor.lnk", &dead), ("Other Thing.lnk", &dead)]));
    assert_eq!(rows.len(), 1, "a guessed folder alone lists nothing: {rows:?}");
    assert!(!has_reason(&rows[0], "install folder"), "{:?}", rows[0]);
    assert!(has_reason(&rows[0], "exactly"));
}

#[test]
fn another_listed_apps_folder_is_skipped_but_the_install_folder_stays() {
    let w = world();
    put(&w.local.join("Acme Editor"), "a.txt", 5);
    let install = w.pf.join("Acme Editor");
    put(&install, "a.exe", 5);
    let a = app("Acme Editor", "Initech", Some(&install), Some(FolderSource::InstallLocation));
    let rows = after(&w, &a, &Claims::of(&["acme editor"]), &unreadable);
    assert!(listed(&rows, &install), "its own dead shortcuts claim it, so the engine decides: {rows:?}");
    assert!(!listed(&rows, &w.local.join("Acme Editor")), "{rows:?}");
}

#[test]
fn what_is_inside_a_listed_folder_is_not_listed_again() {
    let w = world();
    let install = w.pf.join("Acme Editor");
    put(&install.join("Acme Editor"), "a.txt", 5);
    let a = app("Acme Editor", "Initech", Some(&install), Some(FolderSource::InstallLocation));
    let rows = after(&w, &a, &Claims::default(), &unreadable);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].path.eq_ignore_ascii_case(&install.to_string_lossy()));
}

#[test]
fn a_shortcut_named_like_the_app_with_a_dead_target_is_listed_and_an_unreadable_one_is_not() {
    let w = world();
    let a = app("Acme Editor", "Initech", None, None);
    let (user_menu, _) = menus(&w);
    put(&user_menu, "Acme Editor.lnk", 1);
    put(&user_menu, "Acme.lnk", 1);
    put(&user_menu, "Acme Editor Updater.lnk", 1);
    let dead = w.base.join("gone").join("editor.exe");
    // A short name or a longer one never counts, and the exact one's target can't be read here, so it is not listed.
    let rows = after(&w, &a, &Claims::default(), &targets(&[("Acme.lnk", &dead), ("Acme Editor Updater.lnk", &dead)]));
    assert!(rows.is_empty(), "{rows:?}");
    let rows = after(&w, &a, &Claims::default(), &targets(&[("Acme Editor.lnk", &dead)]));
    assert_eq!(rows.len(), 1);
}

#[test]
fn folder_facts_come_from_the_index_first_and_the_walk_fills_the_rest() {
    let w = world();
    let install = w.base.join("apps").join("Acme Editor");
    put(&install, "a.exe", 5);
    put(&w.local.join("Acme Editor"), "b.txt", 7);
    put(&w.local.join("Acme Editor Plus"), "b.txt", 11);
    let a = app("Acme Editor", "Initech", Some(&install), Some(FolderSource::InstallLocation));
    let known = Summary { size: 999, files: 1, newest_ms: 1_000, has_files: true, top: vec!["from-index".into()], ..Summary::default() };
    let index = FakeIndex { known: vec![("Acme Editor", known)], asked: Default::default() };
    let walked = std::sync::Mutex::new(Vec::new());
    let walker = |p: &Path| {
        walked.lock().unwrap().push(p.file_name().unwrap().to_string_lossy().into_owned());
        walk(p)
    };
    let rows = for_app_with(&a, &w.places, &Claims::default(), &names(), later(), &index, &walker, &unreadable);
    assert_eq!(rows.len(), 3, "{rows:?}");
    // One question for all the candidates; the walk only for the one the index didn't know.
    assert_eq!(index.asked.lock().unwrap().len(), 1);
    assert_eq!(*walked.lock().unwrap(), vec!["Acme Editor Plus".to_string()]);
    assert_eq!(rows.iter().filter(|r| r.size == 999).count(), 2);
    assert_eq!(find(&rows, "Acme Editor Plus").size, 11);
}

#[test]
fn an_inner_exact_match_wins_over_an_outer_rough_one() {
    let w = world();
    let suite = w.local.join("Acme Editor Suite");
    put(&suite.join("Acme Editor"), "a.txt", 5);
    put(&suite.join("Acme Paint"), "a.txt", 5);
    let a = app("Acme Editor", "Initech", None, None);
    let rows = after(&w, &a, &Claims::default(), &unreadable);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(listed(&rows, &suite.join("Acme Editor")));
    // With no exact match inside, the outer rough one still stands.
    let w = world();
    put(&w.local.join("Acme Editor Suite").join("Acme Paint"), "a.txt", 5);
    assert!(listed(&after(&w, &a, &Claims::default(), &unreadable), &w.local.join("Acme Editor Suite")));
}

#[test]
fn a_shortcut_that_cannot_be_read_is_never_listed() {
    let w = world();
    let a = app("Acme Editor", "Initech", None, None);
    let (user_menu, _) = menus(&w);
    put(&user_menu, "Acme Editor.lnk", 1);
    assert!(after(&w, &a, &Claims::default(), &unreadable).is_empty());
}
