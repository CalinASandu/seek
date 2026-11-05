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
fn installed_names_come_from_the_uninstall_entries() {
    let mut p = parts();
    for (name, publisher) in [("Docker Desktop", "Docker Inc."), ("Docker Desktop", "docker inc."), ("Nameless Publisher", ""), ("", "Ghost Co")] {
        let mut e = entry(name);
        e.publisher = publisher.into();
        add_uninstall_entry(&mut p, &e, &env);
    }
    let names = InstalledNames::from_parts(&p);
    assert_eq!(names.apps, vec!["Docker Desktop".to_string(), "Nameless Publisher".to_string()]);
    assert_eq!(names.publishers, vec!["Docker Inc.".to_string(), "Ghost Co".to_string()]);
    // Merging the claims of two reads keeps both lists.
    let mut other = parts();
    let mut e = entry("Other");
    e.publisher = "Other Ltd".into();
    add_uninstall_entry(&mut other, &e, &env);
    assert_eq!(InstalledNames::from_parts(&p.merge(other)).publishers.len(), 3);
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
    assert!(whole_words_match("Foo 2", "Foo 2"), "an exact name matches even if it ends in a number");
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

#[test]
fn exact_names_with_a_version_word_match() {
    assert!(whole_words_match("Cubase 13", "Cubase 13"));
    assert!(whole_words_match("Visual Studio 2019", "Visual Studio 2019"));
    assert!(whole_words_match("Visual Studio", "Visual Studio 2019"));
    assert!(!whole_words_match("Cubase 12", "Cubase 13"));
}

#[test]
fn windows_own_folders_are_in_the_table() {
    let c = SystemClaims::from_parts(parts());
    for n in [
        ".IdentityService", "VirtualStore", "PlaceholderTileLogoFolder", "PeerDistRepub", "ToastNotificationManagerCompat",
        "Windows Kits", "Windows Sidebar", "Windows Multimedia Platform", "Windows Mail", "Windows Media Player",
        "Windows Photo Viewer", "Windows Security", "Uninstall Information", "wsl", "ASP.NET",
    ] {
        assert!(c.is_claimed(Path::new(&format!(r"C:\Users\me\AppData\Local\{n}"))).is_some(), "{n}");
    }
    for vendor in ["AMD", "NVIDIA", "Google", "Adobe"] {
        assert!(c.is_claimed(Path::new(&format!(r"C:\Users\me\AppData\Local\{vendor}"))).is_none(), "{vendor}");
    }
}

#[test]
fn short_names_are_expanded_only_on_fixed_drives() {
    let kind = |l: char| match l {
        'C' | 'c' => 3,
        'Z' | 'z' => 4, // a network drive
        _ => 0,
    };
    assert!(should_expand_short_names(r"C:\PROGRA~1\App\u.exe", &mut |l| kind(l)));
    assert!(should_expand_short_names(r"c:\progra~1", &mut |l| kind(l)));
    assert!(!should_expand_short_names(r"Z:\PROGRA~1\App\u.exe", &mut |l| kind(l)));
    assert!(!should_expand_short_names(r"\\server\share\PROGRA~1", &mut |l| kind(l)));
    assert!(!should_expand_short_names(r"C:\Program Files\App", &mut |_| panic!("no short name, no disk question")));
    // And a claim is compared as written without touching the disk.
    let mut p = parts();
    add_process(&mut p, r"Z:\PROGRA~1\App\u.exe");
    let c = SystemClaims::from_parts(p);
    assert!(c.is_claimed(Path::new(r"Z:\PROGRA~1\App")).is_some());
}

#[test]
fn a_program_extension_inside_a_folder_name_still_claims_the_real_folder() {
    let got = command_paths(r"C:\Program Files\Booking.com Helper\h.exe /s", &env);
    assert_eq!(got, vec![r"C:\Program Files\Booking.com", r"C:\Program Files\Booking.com Helper\h.exe"]);
    let mut p = parts();
    add_service(&mut p, "Booking", r"C:\Program Files\Booking.com Helper\h.exe /s", &env);
    assert!(claimed(p, r"C:\Program Files\Booking.com Helper").is_some());
}

#[test]
fn a_later_program_after_an_argument_is_not_glued_on() {
    assert_eq!(
        command_paths(r"C:\a\u.exe /x C:\b c\v.exe", &env),
        vec![r"C:\a\u.exe", r"C:\b c\v.exe"]
    );
}

#[test]
fn no_link_opening_counts_as_a_failed_source() {
    assert!(every_link_failed(&[false, false]));
    assert!(!every_link_failed(&[false, true]));
    assert!(!every_link_failed(&[]), "no shortcuts at all is not a failure");
}

#[test]
fn a_running_program_claims_its_folder_but_not_a_broad_place() {
    let mut p = parts();
    add_process(&mut p, r"C:\Program Files\Foo\foo.exe");
    add_process(&mut p, r"C:\Windows\explorer.exe");
    let c = SystemClaims::from_parts(p);
    assert!(c.is_claimed(Path::new(r"C:\Program Files\Foo\Cache")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Program Files\Foo")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Windows\Anything")).is_none());
    assert!(c.is_claimed(Path::new(r"C:\Program Files\Other")).is_none());
}

#[test]
fn awkward_command_lines_give_exact_claims() {
    assert_eq!(
        command_paths(r"C:\Program Files\App Name\u.exe /x C:\Other\", &env),
        vec![r"C:\Program Files\App Name\u.exe", r"C:\Other"]
    );
    assert_eq!(
        command_paths(r#""C:\Setup\setup.exe" /D=C:\Program Files"#, &env),
        vec![r"C:\Setup\setup.exe", r"C:\Program Files"]
    );
    assert_eq!(command_paths(r#""C:\Program Files\App\" /x"#, &env), vec![r"C:\Program Files\App"]);
    assert_eq!(command_paths(r"rundll32.exe C:\a\b.dll,Uninstall", &env), vec![r"C:\a\b.dll"]);
    assert_eq!(command_paths(r"%NOPE%\App\u.exe", &env), Vec::<String>::new());
    assert_eq!(command_paths(r"C:\%NOPE%\u.exe", &env), vec![r"C:\%NOPE%\u.exe"]);
}

#[test]
fn live_claims_says_it_could_not_check_when_a_source_fails() {
    let live = LiveClaims::with_readers(
        Box::new(|| {
            let mut p = parts();
            p.failed.push("the HKLM 64-bit uninstall list".into());
            SystemClaims::from_parts(p)
        }),
        Duration::from_secs(30),
        Box::new(|| SystemClaims::from_parts(parts())),
        Duration::from_secs(2),
        Box::new(|| Duration::ZERO),
    );
    let who = live.is_claimed(Path::new(r"C:\Users\me\AppData\Local\Anything")).expect("fails closed");
    assert!(who.starts_with("Couldn't check") && who.contains("uninstall list"), "{who}");
}

#[test]
fn the_read_time_is_when_the_read_finished() {
    let now_ms = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let (n, r) = (now_ms.clone(), reads.clone());
    let live = LiveClaims::with_readers(
        Box::new(|| SystemClaims::from_parts(parts())),
        Duration::from_secs(30),
        Box::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
            n.fetch_add(5000, Ordering::SeqCst); // a slow read
            SystemClaims::from_parts(parts())
        }),
        Duration::from_secs(2),
        Box::new({
            let n = now_ms.clone();
            move || Duration::from_millis(n.load(Ordering::SeqCst))
        }),
    );
    let f = Path::new(r"C:\Users\me\AppData\Local\X");
    live.is_claimed(f);
    now_ms.fetch_add(1000, Ordering::SeqCst);
    live.is_claimed(f);
    assert_eq!(reads.load(Ordering::SeqCst), 1, "the snapshot was one second old, not six");
}

#[test]
fn an_installed_programs_exe_name_claims_its_data_folder() {
    let mut p = parts();
    add_shortcut(&mut p, "Visual Studio Code", r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe", &env);
    add_app_path(&mut p, "ollama app.exe", r"C:\Users\me\AppData\Local\Programs\Ollama\ollama app.exe", "", &env);
    add_process(&mut p, r"C:\Program Files\AnyDesk\AnyDesk.exe");
    let c = SystemClaims::from_parts(p);
    let who = c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\Code")).expect("claimed");
    assert!(who.contains("Code.exe"), "{who}");
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Local\ollama app")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\ANYDESK")).is_some());
    assert!(c.is_claimed(Path::new(r"C:\Users\me\AppData\Roaming\Cod")).is_none());
}

#[test]
fn a_generic_exe_name_claims_nothing() {
    let mut p = parts();
    for exe in ["Setup", "unins000", "Updater", "Launcher", "Helper", "Uninstall", "app", "run", "start", "main", "service", "install", "update", "uninst"] {
        add_shortcut(&mut p, "X", &format!(r"C:\Program Files\X\{exe}.exe"), &env);
    }
    let c = SystemClaims::from_parts(p);
    for n in ["Setup", "unins000", "Updater", "Launcher", "Helper", "Uninstall", "app", "run", "start", "main", "service", "install", "update", "uninst"] {
        assert!(c.is_claimed(Path::new(&format!(r"C:\Users\me\AppData\Roaming\{n}"))).is_none(), "{n}");
    }
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
