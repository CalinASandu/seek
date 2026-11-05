use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

fn env(name: &str) -> Option<String> {
    match name.to_ascii_lowercase().as_str() {
        "systemroot" | "windir" => Some(r"C:\Windows".into()),
        "programfiles" => Some(r"C:\Program Files".into()),
        "localappdata" => Some(r"C:\Users\me\AppData\Local".into()),
        "appdata" => Some(r"C:\Users\me\AppData\Roaming".into()),
        _ => None,
    }
}

fn broad() -> Vec<String> {
    [
        "c:",
        r"c:\windows",
        r"c:\program files",
        r"c:\program files (x86)",
        r"c:\programdata",
        r"c:\users",
        r"c:\users\me",
        r"c:\users\me\appdata\local",
        r"c:\users\me\appdata\roaming",
        r"c:\users\me\appdata\locallow",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn parts() -> ClaimParts {
    ClaimParts { broad_roots: broad(), ..ClaimParts::default() }
}

fn claimed(p: ClaimParts, folder: &str) -> Option<String> {
    SystemClaims::from_parts(p).is_claimed(Path::new(folder))
}

fn entry(name: &str) -> UninstallEntry {
    UninstallEntry { key: "k".into(), display_name: name.into(), ..UninstallEntry::default() }
}

#[test]
fn an_uninstall_location_claims() {
    let mut p = parts();
    let mut e = entry("Foo");
    e.install_location = r"C:\Program Files\Foo\".into();
    add_uninstall_entry(&mut p, &e, &env);
    let who = claimed(p, r"C:\Program Files\foo").expect("claimed");
    assert!(who.contains("Foo"), "{who}");
}

#[test]
fn an_icon_path_claims() {
    let mut p = parts();
    let mut e = entry("");
    e.display_icon = r"C:\Users\me\AppData\Local\Bar\bar.exe,0".into();
    add_uninstall_entry(&mut p, &e, &env);
    assert!(claimed(p, r"C:\Users\me\AppData\Local\Bar").is_some());
}

#[test]
fn an_uninstall_command_claims() {
    let mut p = parts();
    let mut e = entry("");
    e.uninstall_string = r#""C:\Program Files\Baz\unins000.exe" /SILENT"#.into();
    add_uninstall_entry(&mut p, &e, &env);
    assert!(claimed(p, r"C:\Program Files\Baz").is_some());
}

#[test]
fn a_display_name_claims_as_whole_words() {
    let mut p = parts();
    add_uninstall_entry(&mut p, &entry("Visual Studio Code"), &env);
    let c = SystemClaims::from_parts(p);
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\Code")).is_none());
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\visual-studio-code")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\VISUAL STUDIO CODE")).is_some());
}

#[test]
fn a_partial_word_does_not_claim() {
    assert!(!whole_words_match("AppX", "AppX Extended"));
    assert!(!whole_words_match("AppX Extended", "AppX"));
    assert!(!whole_words_match("App", "AppX"));
    assert!(whole_words_match("AppX", "appx"));
}

#[test]
fn version_and_architecture_words_are_ignored() {
    assert!(whole_words_match("Notepad++", "Notepad++ (64-bit x64)"));
    assert!(whole_words_match("Foo", "Foo 1.2.3"));
    assert!(whole_words_match("Foo", "Foo v2"));
    assert!(whole_words_match("Foo", "Foo (x64 en-US)"));
    assert!(whole_words_match("Foo Bar", "Foo Bar x86"));
    assert!(whole_words_match("Foo", "Foo 64-bit"));
    assert!(!whole_words_match("Foo 2", "Foo 2"), "a folder name is not trimmed, so only the app name loses its version");
}

#[test]
fn notepad_plus_plus_is_not_notepad() {
    assert!(!whole_words_match("Notepad", "Notepad++"));
    assert!(!whole_words_match("Notepad++", "Notepad"));
    assert!(whole_words_match("Notepad++", "Notepad++"));
    assert!(!whole_words_match("", ""));
    assert!(!whole_words_match("---", "..."));
}

#[test]
fn notepad_plus_plus_settings_in_roaming_are_claimed_by_name_alone() {
    let mut p = parts();
    add_uninstall_entry(&mut p, &entry("Notepad++ (64-bit x64)"), &env);
    let c = SystemClaims::from_parts(p);
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\Notepad++")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\Notepad")).is_none());
}

#[test]
fn a_service_image_path_claims() {
    let mut p = parts();
    add_service(&mut p, "FooSvc", r#""C:\Program Files\Foo\svc.exe" -k run"#, &env);
    let who = claimed(p, r"C:\Program Files\Foo").expect("claimed");
    assert!(who.contains("FooSvc"), "{who}");
}

#[test]
fn service_path_prefixes_are_understood() {
    assert_eq!(command_paths(r"\??\C:\Windows\system32\drivers\x.sys", &env), vec![r"C:\Windows\system32\drivers\x.sys"]);
    assert_eq!(command_paths(r"\SystemRoot\System32\drivers\y.sys", &env), vec![r"C:\Windows\System32\drivers\y.sys"]);
    assert_eq!(command_paths(r#""\??\C:\Program Files\A B\z.sys""#, &env), vec![r"C:\Program Files\A B\z.sys"]);
    assert_eq!(command_paths(r"System32\drivers\w.sys", &env), Vec::<String>::new());
}

#[test]
fn a_run_key_claims() {
    let mut p = parts();
    add_run_entry(&mut p, "Updater", r#""C:\Users\me\AppData\Local\Qux\up.exe" --background"#, &env);
    let who = claimed(p, r"C:\Users\me\AppData\Local\Qux").expect("claimed");
    assert!(who.contains("Updater"), "{who}");
}

#[test]
fn an_app_paths_entry_claims() {
    let mut p = parts();
    add_app_path(&mut p, "quux.exe", r"C:\Program Files\Quux\quux.exe", r"C:\Tools\QuuxLib;C:\Tools\More\", &env);
    let c = SystemClaims::from_parts(p);
    assert!(c.is_claimed(Path::new(r"C:\Program Files\Quux")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Tools\QuuxLib")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Tools\More")).is_some());
}

#[test]
fn a_start_menu_shortcut_target_claims() {
    let mut p = parts();
    add_shortcut(&mut p, "Corge", r"C:\Program Files\Corge\corge.exe", &env);
    let who = claimed(p, r"C:\Program Files\Corge").expect("claimed");
    assert!(who.contains("Corge"), "{who}");
}

#[test]
fn a_running_process_claims() {
    let mut p = parts();
    add_process(&mut p, r"C:\Users\me\AppData\Local\Grault\grault.exe");
    let who = claimed(p, r"C:\Users\me\AppData\Local\Grault").expect("claimed");
    assert!(who.contains("grault.exe"), "{who}");
}

#[test]
fn the_always_claimed_table_claims() {
    let c = SystemClaims::from_parts(parts());
    for n in ["Microsoft", "Windows", "Common Files", "WindowsApps", "Packages", "Programs", "Temp", "Seek-dev", "Seek", "microsoft", "MSBuild", "dotnet"] {
        assert!(c.is_claimed(Path::new(&format!(r"C:\Users\me\AppData\Local\{n}"))).is_some(), "{n}");
    }
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Foo")).is_none());
}

#[test]
fn the_always_claimed_table_has_no_duplicates_and_no_vendors() {
    let mut seen = HashSet::new();
    for n in ALWAYS_CLAIMED {
        assert!(seen.insert(n.to_lowercase()), "duplicate {n}");
    }
    for vendor in ["google", "adobe", "mozilla", "jetbrains", "nvidia", "discord", "spotify", "steam"] {
        assert!(!seen.contains(vendor), "{vendor}");
    }
}

#[test]
fn quoted_and_argument_paths_are_understood() {
    assert_eq!(command_paths(r#""C:\Program Files\App\u.exe" /x"#, &env), vec![r"C:\Program Files\App\u.exe"]);
    assert_eq!(command_paths(r"C:\Program Files\App\u.exe /x", &env), vec![r"C:\Program Files\App\u.exe"]);
    assert_eq!(command_paths(r#"cmd /c "C:\a b\u.bat""#, &env), vec![r"C:\a b\u.bat"]);
}

#[test]
fn an_icon_index_is_ignored() {
    assert_eq!(command_paths(r"C:\a\b.exe,0", &env), vec![r"C:\a\b.exe"]);
    assert_eq!(command_paths(r"C:\a\b.exe,-12", &env), vec![r"C:\a\b.exe"]);
    assert_eq!(command_paths(r#""C:\a b\c.ico",3"#, &env), vec![r"C:\a b\c.ico"]);
    assert_eq!(command_paths(r#""C:\a b\c.ico,3""#, &env), vec![r"C:\a b\c.ico"]);
}

#[test]
fn rundll32_arguments_are_understood() {
    assert_eq!(command_paths(r#"rundll32.exe "C:\a\b.dll",Uninstall"#, &env), vec![r"C:\a\b.dll"]);
    assert_eq!(
        command_paths(r#"C:\Windows\System32\rundll32.exe C:\Program Files\App\b.dll,Uninstall"#, &env),
        vec![r"C:\Windows\System32\rundll32.exe", r"C:\Program Files\App\b.dll"]
    );
}

#[test]
fn msiexec_claims_nothing_beyond_system32() {
    assert!(command_paths("MsiExec.exe /X{0A1B2C3D-0000-1111-2222-333344445555}", &env).is_empty());
    let mut p = parts();
    let mut e = entry("");
    e.uninstall_string = "MsiExec.exe /I{0A1B2C3D-0000-1111-2222-333344445555}".into();
    add_uninstall_entry(&mut p, &e, &env);
    assert!(p.paths.is_empty());
}

#[test]
fn environment_variables_are_expanded() {
    assert_eq!(expand_env(r"%ProgramFiles%\App", &env), r"C:\Program Files\App");
    assert_eq!(expand_env(r"%LOCALAPPDATA%\x\%nope%\y", &env), r"C:\Users\me\AppData\Local\x\%nope%\y");
    assert_eq!(expand_env("100%", &env), "100%");
    assert_eq!(command_paths(r#""%ProgramFiles%\App\a.exe" /s"#, &env), vec![r"C:\Program Files\App\a.exe"]);
    assert_eq!(location_paths(r"%APPDATA%\Foo", &env), vec![r"C:\Users\me\AppData\Roaming\Foo"]);
}

#[test]
fn trailing_backslashes_do_not_matter() {
    let mut p = parts();
    let mut e = entry("");
    e.install_location = r"C:\Program Files\Foo\\".into();
    add_uninstall_entry(&mut p, &e, &env);
    let c = SystemClaims::from_parts(p);
    assert!(c.is_claimed(Path::new(r"C:\Program Files\Foo\")).is_some());
    assert!(c.is_claimed(Path::new(r"c:\program files\foo")).is_some());
    assert!(c.is_claimed(Path::new(r"c:\program files\foo2")).is_none());
}

#[test]
fn broken_values_claim_nothing_and_do_not_panic() {
    for v in ["", " ", "\"", "\"\"", ",", "%", "%%", "\"C:\\unterminated", "C:", "C:\\", "\\\\", "\u{1F600} \u{e9}\u{e9}.exe", "relative\\path.exe", ",,,\"", "%X"] {
        let _ = command_paths(v, &env);
        let _ = location_paths(v, &env);
        let _ = expand_env(v, &env);
        let mut p = parts();
        add_service(&mut p, "s", v, &env);
        add_run_entry(&mut p, "r", v, &env);
        add_shortcut(&mut p, "x", v, &env);
        add_process(&mut p, v);
        let c = SystemClaims::from_parts(p);
        let _ = c.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Zzz"));
        let _ = whole_words_match(v, v);
    }
    assert!(command_paths("", &env).is_empty());
    assert!(command_paths("C:\\", &env).is_empty());
    assert!(claimed(parts(), r"C:\Users\me\AppData\Local\Zzz").is_none());
}

#[test]
fn a_source_that_could_not_be_read_claims_every_folder() {
    let mut p = parts();
    p.failed.push("the uninstall list".into());
    let who = claimed(p, r"C:\Users\me\AppData\Local\Anything").expect("fails closed");
    assert!(who.starts_with("Couldn't check"), "{who}");
    assert!(who.contains("the uninstall list"), "{who}");
}

#[test]
fn one_unreadable_item_is_counted_and_does_not_claim_everything() {
    let mut p = parts();
    p.skipped.processes = 3;
    let c = SystemClaims::from_parts(p);
    assert_eq!(c.skipped.processes, 3);
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Anything")).is_none());
}

#[test]
fn an_install_location_of_program_files_does_not_claim_everything_in_it() {
    for loc in [r"C:\Program Files", r"C:\Program Files\", "C:\\", r"C:\Users\me\AppData\Local", r"C:\Users\me\AppData"] {
        let mut p = parts();
        let mut e = entry("");
        e.install_location = loc.into();
        add_uninstall_entry(&mut p, &e, &env);
        let c = SystemClaims::from_parts(p);
        assert!(c.is_claimed(Path::new(r"C:\Program Files\Anything")).is_none(), "{loc}");
        assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Anything")).is_none(), "{loc}");
    }
}

#[test]
fn a_claim_inside_a_folder_still_claims_it_even_in_a_broad_place() {
    let mut p = parts();
    add_process(&mut p, r"C:\Program Files\Tool\deep\tool.exe");
    add_shortcut(&mut p, "Other", r"C:\Users\me\AppData\Local\Other\deep\o.exe", &env);
    let c = SystemClaims::from_parts(p);
    assert!(c.is_claimed(Path::new(r"C:\Program Files\Tool")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Other")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Program Files\Tool\deep")).is_some());
}

#[test]
fn a_claim_above_a_leftover_subfolder_claims_it() {
    let mut p = parts();
    let mut e = entry("Foo");
    e.install_location = r"C:\Program Files\Foo".into();
    add_uninstall_entry(&mut p, &e, &env);
    assert!(claimed(p, r"C:\Program Files\Foo\Cache").is_some());
}

struct Harness {
    installed_reads: Arc<AtomicUsize>,
    running_reads: Arc<AtomicUsize>,
    now_ms: Arc<AtomicU64>,
    live: LiveClaims,
}

fn harness() -> Harness {
    let installed_reads = Arc::new(AtomicUsize::new(0));
    let running_reads = Arc::new(AtomicUsize::new(0));
    let now_ms = Arc::new(AtomicU64::new(0));
    let (i, r, n) = (installed_reads.clone(), running_reads.clone(), now_ms.clone());
    let live = LiveClaims::with_readers(
        Box::new(move || {
            i.fetch_add(1, Ordering::SeqCst);
            SystemClaims::from_parts(parts())
        }),
        Duration::from_secs(30),
        Box::new(move || {
            // The "program" is running only from the second read on.
            let seen = r.fetch_add(1, Ordering::SeqCst);
            let mut p = parts();
            if seen >= 1 {
                add_process(&mut p, r"C:\Users\me\AppData\Local\Late\late.exe");
            }
            SystemClaims::from_parts(p)
        }),
        Duration::from_secs(2),
        Box::new(move || Duration::from_millis(n.load(Ordering::SeqCst))),
    );
    Harness { installed_reads, running_reads, now_ms, live }
}

#[test]
fn live_claims_reads_lazily_and_not_per_folder() {
    let h = harness();
    assert_eq!(h.running_reads.load(Ordering::SeqCst) + h.installed_reads.load(Ordering::SeqCst), 0, "nothing is read before the first question");
    for _ in 0..50 {
        h.live.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Late"));
    }
    assert_eq!(h.running_reads.load(Ordering::SeqCst), 1);
    h.now_ms.store(1999, Ordering::SeqCst);
    h.live.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Late"));
    assert_eq!(h.running_reads.load(Ordering::SeqCst), 1);
}

#[test]
fn live_claims_reads_again_after_the_interval() {
    let h = harness();
    assert!(h.live.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Late")).is_none());
    h.now_ms.store(2000, Ordering::SeqCst);
    let who = h.live.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Late")).expect("the program started later");
    assert!(who.contains("late.exe"), "{who}");
    assert_eq!(h.running_reads.load(Ordering::SeqCst), 2);
    assert_eq!(h.installed_reads.load(Ordering::SeqCst), 1, "the installed apps are not read that often");
    h.now_ms.store(30_000, Ordering::SeqCst);
    h.live.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Late"));
    assert_eq!(h.installed_reads.load(Ordering::SeqCst), 2);
}

#[test]
fn a_poisoned_lock_does_not_panic() {
    let h = harness();
    let live = Arc::new(h.live);
    let l2 = live.clone();
    let _ = std::thread::spawn(move || l2.poison_for_test()).join();
    assert!(live.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Late")).is_none());
}

#[test]
#[ignore = "reads the real registry, services, shortcuts and processes"]
fn real_read_reports_counts_and_time() {
    // Only reads. Run with: cargo test -p seek-clean claims::tests::real_read -- --ignored --nocapture
    let t = std::time::Instant::now();
    let c = SystemClaims::read();
    let ms = t.elapsed().as_millis();
    println!("read took {ms} ms");
    for (source, n) in c.counts() {
        println!("  {source}: {n}");
    }
    println!("  skipped: {:?}", c.skipped);
    println!("  failed sources: {:?}", c.failed_sources());
    for var in ["LOCALAPPDATA", "APPDATA"] {
        let Ok(root) = std::env::var(var) else { continue };
        let mut open = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) && c.is_claimed(&e.path()).is_none() {
                    open.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        open.sort();
        println!("unclaimed in %{var}%: {open:?}");
    }
}
