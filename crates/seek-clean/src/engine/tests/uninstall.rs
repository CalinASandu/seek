//! What an uninstall left (`Why::Uninstall`): the engine holds the rules, whoever asks.

use super::*;

const USER_MENU: &str = "users/me/AppData/Roaming/Microsoft/Windows/Start Menu/Programs";
const ALL_MENU: &str = "pd/Microsoft/Windows/Start Menu/Programs";

fn uninstall_folder(path: PathBuf) -> Candidate {
    Candidate::Folder { path, why: Why::Uninstall, level: None, reasons: vec![] }
}

fn shortcut(path: PathBuf) -> Candidate {
    Candidate::Shortcut { path, level: None, reasons: vec![] }
}

fn lock_of(env: &Env, c: Candidate) -> Option<Lock> {
    env.engine.prepare(vec![c], &[]).items[0].lock
}

/// Left out of the review altogether (nothing there to look at) or listed as locked.
fn refused_or_locked(env: &Env, c: Candidate) -> bool {
    env.engine.prepare(vec![c], &[]).items.first().is_none_or(|i| i.lock.is_some())
}

fn folder_with_data(env: &Env, rel: &str) -> PathBuf {
    let p = env.p(rel);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("data.bin"), b"some data").unwrap();
    p
}

#[test]
fn an_uninstall_folder_goes_to_the_recycle_bin() {
    let env = Env::new(false);
    let p = folder_with_data(&env, "apps/Acme");
    let prep = env.engine.prepare(vec![uninstall_folder(p.clone())], &[]);
    assert_eq!(prep.items[0].lock, None);
    assert_eq!(prep.items[0].why, Why::Uninstall);
    assert!(!prep.items[0].needs_admin);
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(matches!(r.items[0].outcome, Outcome::Moved { .. }), "{r:?}");
    assert_eq!(env.rec.calls(), vec![p]);
}

#[test]
fn an_uninstall_folder_in_program_files_needs_admin() {
    let env = Env::new(false);
    let p = folder_with_data(&env, "pf/Acme");
    let prep = env.engine.prepare(vec![uninstall_folder(p.clone())], &[]);
    assert_eq!(prep.items[0].lock, None);
    assert!(prep.items[0].needs_admin);
    // Without an elevator it is left in place with the admin reason; nothing is recycled.
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(left(&r, 0).contains("admin"), "{r:?}");
    assert!(p.exists() && env.rec.calls().is_empty());

    let env = Env::new(true);
    let p = folder_with_data(&env, "pf/Acme");
    let prep = env.engine.prepare(vec![uninstall_folder(p)], &[]);
    env.engine.commit(&prep.token, &[0], &[]).unwrap();
    let calls = env.elev.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][0].why, Why::Uninstall);
    assert!(env.rec.calls().is_empty(), "the admin step does it, not this process");
}

#[test]
fn a_folder_another_listed_app_still_claims_comes_back_locked() {
    // A language pack's uninstall leaves D:\Apps\Product while the product itself stays installed.
    let env = Env::new(false);
    let p = folder_with_data(&env, "apps/Product");
    env.claims.0.lock().unwrap().insert(p.clone());
    let prep = env.engine.prepare(vec![uninstall_folder(p.clone())], &[]);
    assert_eq!(prep.items[0].lock, Some(Lock::Claimed));
    let r = env.engine.commit(&prep.token, &[0], &[]).unwrap();
    assert!(left(&r, 0).contains("using this folder"), "{r:?}");
    assert!(p.exists() && env.rec.calls().is_empty());
}

#[test]
fn an_uninstall_request_for_the_users_own_folders_is_locked() {
    let env = Env::new(false);
    for rel in ["users/me/Documents", "users/me/Desktop", "users/me/OneDrive - Contoso", "users/me/Dropbox", "users/me/source"] {
        let p = folder_with_data(&env, rel);
        assert!(refused_or_locked(&env, uninstall_folder(p)), "{rel} is not locked");
    }
    // The profile, the users folder, Windows, a drive root and the places themselves.
    for rel in ["users/me", "users", "win/system32", "pf", "pd", "users/me/appdata"] {
        let p = env.p(rel);
        std::fs::create_dir_all(&p).unwrap();
        assert!(refused_or_locked(&env, uninstall_folder(p)), "{rel} is not locked");
    }
    assert!(refused_or_locked(&env, uninstall_folder(PathBuf::from(r"C:\"))));
    assert!(env.rec.calls().is_empty());
}

#[test]
fn another_users_folder_and_program_data_are_locked_for_uninstall_too() {
    let env = Env::new(false);
    let other = folder_with_data(&env, "users/other/Acme");
    assert_eq!(lock_of(&env, uninstall_folder(other)), Some(Lock::OtherUser));
    let data = folder_with_data(&env, "pd/Acme");
    assert_eq!(lock_of(&env, uninstall_folder(data)), Some(Lock::AppData));
}

#[test]
fn a_start_menu_shortcut_may_go_and_the_all_users_one_needs_admin() {
    let env = Env::new(false);
    let mine = env.file(&format!("{USER_MENU}/Acme/Acme.lnk"), b"lnk");
    let all = env.file(&format!("{ALL_MENU}/Acme.lnk"), b"lnk");
    let prep = env.engine.prepare(vec![shortcut(mine.clone()), shortcut(all.clone())], &[]);
    assert_eq!(prep.items[0].lock, None, "{:?}", prep.items[0]);
    assert!(!prep.items[0].needs_admin && !prep.items[0].is_dir);
    assert_eq!(prep.items[1].lock, None, "{:?}", prep.items[1]);
    assert!(prep.items[1].needs_admin, "ProgramData's Start Menu can't be changed without admin");
    assert_eq!(prep.items[0].why, Why::Uninstall);
    let r = env.engine.commit(&prep.token, &[0, 1], &[]).unwrap();
    assert!(matches!(r.items.iter().find(|i| i.id == 0).unwrap().outcome, Outcome::Moved { .. }));
    assert!(left(&r, 1).contains("admin"));
    assert!(!mine.exists() && all.exists());
}

#[test]
fn an_uninstall_file_that_is_not_a_start_menu_shortcut_is_locked() {
    let env = Env::new(false);
    let cases = [
        env.file("users/me/docs/Acme.lnk", b"x"),
        env.file("users/me/Documents/report.docx", b"x"),
        env.file("pf/Acme/app.exe", b"x"),
        env.file(&format!("{USER_MENU}/Acme/readme.txt"), b"x"),
        env.file(&format!("{ALL_MENU}/Acme.exe"), b"x"),
        env.file("pd/Microsoft/Windows/Acme.lnk", b"x"),
        env.file("users/me/AppData/Roaming/Microsoft/Windows/Start Menu/Acme.lnk", b"x"),
    ];
    for p in cases {
        // ProgramData's own rule may speak first; either way it is locked.
        let lock = lock_of(&env, shortcut(p.clone()));
        assert!(matches!(lock, Some(Lock::UnsupportedPath | Lock::AppData)), "{}: {lock:?}", p.display());
    }
    let plain = lock_of(&env, shortcut(env.p("users/me/docs/Acme.lnk")));
    assert_eq!(plain, Some(Lock::UnsupportedPath));
    // The same files named as folders are wrong in the other direction.
    let doc = env.file("users/me/Documents/report.docx", b"x");
    assert_eq!(lock_of(&env, uninstall_folder(doc)), Some(Lock::UnsupportedPath));
    assert!(env.rec.calls().is_empty());
}

#[test]
fn a_folder_called_lnk_in_the_start_menu_and_the_start_menu_itself_are_locked() {
    let env = Env::new(false);
    for rel in [format!("{ALL_MENU}/Acme.lnk"), format!("{USER_MENU}/Acme.lnk")] {
        let p = env.p(&rel);
        std::fs::create_dir_all(&p).unwrap();
        assert!(lock_of(&env, shortcut(p.clone())).is_some(), "{rel} as a shortcut");
        assert!(lock_of(&env, uninstall_folder(p)).is_some(), "{rel} as a folder");
    }
    for rel in [ALL_MENU, USER_MENU, "pd/Microsoft/Windows/Start Menu"] {
        let p = env.p(rel);
        std::fs::create_dir_all(&p).unwrap();
        assert!(lock_of(&env, uninstall_folder(p)).is_some(), "{rel}");
    }
}

#[test]
fn a_junction_cannot_carry_a_shortcut_out_of_the_start_menu() {
    let env = Env::new(false);
    let outside = env.file("users/me/docs/secret/Acme.lnk", b"x");
    let menu = env.p(USER_MENU);
    std::fs::create_dir_all(&menu).unwrap();
    assert!(make_junction(&menu.join("Jump"), outside.parent().unwrap()));
    let via = menu.join("Jump").join("Acme.lnk");
    assert_eq!(lock_of(&env, shortcut(via)), Some(Lock::UnsupportedPath));
}

#[test]
fn leftover_rules_are_unchanged_by_uninstall_ones() {
    // A Leftover candidate still has to be a direct child of a scan root, and a shortcut is not one.
    let env = Env::new(false);
    let mine = env.file(&format!("{USER_MENU}/Acme.lnk"), b"x");
    let as_leftover = Candidate::Folder { path: mine, why: Why::Leftover, level: None, reasons: vec![] };
    assert!(lock_of(&env, as_leftover).is_some());
}
