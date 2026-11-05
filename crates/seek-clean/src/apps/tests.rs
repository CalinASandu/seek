use super::*;

fn places() -> Places {
    Places::for_tests(
        r"C:\Windows",
        &[r"C:\Program Files", r"C:\Program Files (x86)"],
        r"C:\Users",
        r"C:\Users\me",
        r"C:\ProgramData",
        &[r"C:\Users\me\AppData\Local", r"C:\Users\me\AppData\Roaming"],
    )
}

/// Local time is UTC+2 for the tests: local midnight is two hours before UTC midnight.
fn plus_two(naive: i64) -> Option<i64> {
    Some(naive - 2 * 3_600_000)
}

fn entry(name: &str) -> RawEntry {
    RawEntry {
        hive: "HKLM".into(),
        view: "64".into(),
        key: format!("{{{name}}}"),
        display_name: name.into(),
        publisher: "Acme".into(),
        uninstall_string: r#""C:\Program Files\Acme\unins000.exe""#.into(),
        ..RawEntry::default()
    }
}

fn names(rows: &[AppRow]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

fn build(entries: Vec<RawEntry>) -> Vec<AppRow> {
    rows(entries, &places(), &[], &plus_two)
}

// ---- which entries are listed ------------------------------------------------------------------

#[test]
fn system_components_parents_and_no_command_are_hidden() {
    let mut sys = entry("Sys");
    sys.system_component = true;
    let mut child = entry("Update KB1");
    child.parent_key_name = "Windows".into();
    let mut no_cmd = entry("NoCommand");
    no_cmd.uninstall_string = "  ".into();
    let mut nameless = entry("x");
    nameless.display_name = " ".into();
    let mut quiet_only = entry("QuietOnly");
    quiet_only.uninstall_string.clear();
    quiet_only.quiet_uninstall_string = r#""C:\q\u.exe" /S"#.into();
    let shown = build(vec![entry("Keep"), sys, child, no_cmd, nameless, quiet_only]);
    assert_eq!(names(&shown), ["Keep", "QuietOnly"]);
}

#[test]
fn seek_itself_is_hidden() {
    let mut machine = entry("Seek");
    machine.publisher = "Seek".into();
    let mut user = machine.clone();
    user.hive = "HKCU".into();
    // Another product that is merely called Seek stays.
    let mut other = entry("Seek");
    other.key = "other".into();
    other.publisher = "Someone Else".into();
    let shown = build(vec![machine, user, other]);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].publisher, "Someone Else");
}

#[test]
fn runtimes_stay_listed_with_a_note() {
    let shown = build(vec![
        entry("Microsoft Visual C++ 2015-2022 Redistributable (x64) - 14.40.33810"),
        entry("Microsoft .NET Runtime - 8.0.10 (x64)"),
        entry("Microsoft Windows Desktop Runtime - 8.0.10 (x64)"),
        entry("Microsoft DirectX SDK"),
        entry("Visual Studio Tools for Applications 2019"),
        entry("NVIDIA Graphics Driver 560.94"),
        entry("Printer driver pack"),
        entry("Firefox"),
    ]);
    assert_eq!(shown.len(), 8);
    for r in &shown {
        let expect = (r.name != "Firefox").then_some("Other apps may need this.");
        assert_eq!(r.note.as_deref(), expect, "{}", r.name);
    }
}

#[test]
fn the_note_table_matches_whole_shapes_not_loose_words() {
    assert!(glob("a*b", "axxb"));
    assert!(glob("a*b*", "axxbyy"));
    assert!(glob("*b", "ab"));
    assert!(!glob("a*b", "axxbc"));
    assert!(!glob("a*b", "xab"));
    assert!(note_for("Microsoft Visual C++ 2013 Redistributable (x86)").is_some());
    assert!(note_for("Microsoft Visual C++ Build Tools").is_none());
    assert!(note_for("Newtonsoft Json Net").is_none());
    assert!(note_for("Paint.NET").is_none());
    for p in RUNTIME_PATTERNS {
        assert_eq!(*p, p.to_lowercase(), "patterns are matched against lowercase names");
    }
}

// ---- the folder -----------------------------------------------------------------------------------------

#[test]
fn install_location_wins() {
    let mut e = entry("App");
    e.install_location = r"D:\Games\App\".into();
    e.display_icon = r"C:\Program Files\Other\other.exe,0".into();
    assert_eq!(install_folder(&e, &places()), Some(PathBuf::from(r"D:\Games\App")));
}

#[test]
fn folder_falls_back_to_the_icon_then_the_uninstaller() {
    let mut e = entry("App");
    e.display_icon = r#""C:\Program Files\Acme App\app.exe",1"#.into();
    assert_eq!(install_folder(&e, &places()), Some(PathBuf::from(r"C:\Program Files\Acme App")));
    e.display_icon.clear();
    assert_eq!(install_folder(&e, &places()), Some(PathBuf::from(r"C:\Program Files\Acme")));
    // An icon in a rejected place does not stop the uninstaller from being tried.
    e.display_icon = r"C:\Windows\Installer\{G}\icon.ico".into();
    assert_eq!(install_folder(&e, &places()), Some(PathBuf::from(r"C:\Program Files\Acme")));
}

#[test]
fn derived_folder_rejected_under_windows_installer_temp_package_cache_and_pf_root() {
    let bad = [
        r"C:\Windows\Installer\{G}\app.ico",
        r"C:\Windows\System32\drivers\x.sys",
        r"C:\Users\me\AppData\Local\Temp\{G}\setup.exe",
        r"C:\ProgramData\Package Cache\{G}\vc_redist.x64.exe",
        r"C:\Program Files\setup.exe",
        r"C:\Program Files (x86)\setup.exe",
        r"C:\Users\me\AppData\Local\unins.exe",
    ];
    for icon in bad {
        let mut e = entry("App");
        e.uninstall_string = format!(r#""{icon}""#);
        e.display_icon = icon.to_string();
        assert_eq!(install_folder(&e, &places()), None, "{icon}");
    }
    // The same rules hold for a registered InstallLocation that is a root.
    let mut e = entry("App");
    e.uninstall_string = "MsiExec.exe /X{AAAA-BBBB}".into();
    e.install_location = r"C:\Program Files".into();
    assert_eq!(install_folder(&e, &places()), None);
}

#[test]
fn no_folder_is_still_listed() {
    let mut e = entry("Mystery");
    e.uninstall_string = "MsiExec.exe /X{AAAA-BBBB}".into();
    let shown = build(vec![e]);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].folder, None);
    assert!(!shown[0].running);
}

// ---- the command --------------------------------------------------------------------------------------------

#[test]
fn parse_quoted_path_with_args() {
    assert_eq!(
        parse_uninstall(r#""C:\Program Files\App\unins000.exe" /SILENT"#),
        UninstallCommand::Exe { path: r"C:\Program Files\App\unins000.exe".into(), args: "/SILENT".into() }
    );
}

#[test]
fn parse_unquoted_path_with_spaces() {
    assert_eq!(
        parse_uninstall(r"C:\Program Files\My App\uninstall.exe /S /D=C:\x"),
        UninstallCommand::Exe { path: r"C:\Program Files\My App\uninstall.exe".into(), args: r"/S /D=C:\x".into() }
    );
    assert_eq!(
        parse_uninstall(r"C:\Program Files\Booking.com Helper\un.exe"),
        UninstallCommand::Exe { path: r"C:\Program Files\Booking.com Helper\un.exe".into(), args: String::new() }
    );
}

#[test]
fn parse_msiexec_x_and_i_forms_become_uninstall() {
    let want = UninstallCommand::Msi { guid: "{AAAA-BBBB}".into() };
    assert_eq!(parse_uninstall("MsiExec.exe /I{AAAA-BBBB}"), want);
    assert_eq!(parse_uninstall("MsiExec.exe /X{AAAA-BBBB}"), want);
    assert_eq!(parse_uninstall("msiexec /x {AAAA-BBBB} /qn"), want);
    assert_eq!(parse_uninstall(r"C:\Windows\System32\msiexec.exe /X{AAAA-BBBB}"), want);
    // No product code: left as a plain command.
    assert_eq!(
        parse_uninstall("MsiExec.exe /?"),
        UninstallCommand::Exe { path: "MsiExec.exe".into(), args: "/?".into() }
    );
}

#[test]
fn parse_expands_environment_variables() {
    std::env::set_var("SEEK_APPS_TEST_DIR", r"C:\Tools");
    assert_eq!(
        parse_uninstall(r"%SEEK_APPS_TEST_DIR%\u.exe -y"),
        UninstallCommand::Exe { path: r"C:\Tools\u.exe".into(), args: "-y".into() }
    );
}

// ---- the id -----------------------------------------------------------------------------------------------------

#[test]
fn id_round_trips_and_rejects_anything_else() {
    for (hive, view) in [("HKLM", "64"), ("HKLM", "32"), ("HKCU", "64"), ("HKCU", "32")] {
        let e = RawEntry { hive: hive.into(), view: view.into(), key: "{12345678-ABCD}".into(), ..RawEntry::default() };
        let id = app_id(&e);
        assert_eq!(id, format!(r"{hive}{view}\{{12345678-ABCD}}"));
        assert_eq!(parse_app_id(&id), Some((hive, view, "{12345678-ABCD}")));
        assert_eq!(app_id(&e), id, "stable between reads");
    }
    let e = RawEntry { hive: "HKLM".into(), view: "64".into(), key: "Foo Bar 1.2".into(), ..RawEntry::default() };
    assert_eq!(parse_app_id(&app_id(&e)), Some(("HKLM", "64", "Foo Bar 1.2")));
    for bad in ["", "HKLM64", r"HKLM64\", r"HKLM64\a\b", r"HKLM99\x", r"HKXX64\x", r"hklm64\x", r"HKLM6\x", "ünï\\x", r"HKLM64\..\x"] {
        assert_eq!(parse_app_id(bad), None, "{bad:?}");
    }
}

#[test]
fn entry_by_id_refuses_a_malformed_id_and_a_missing_key() {
    assert!(entry_by_id("nonsense").is_none());
    assert!(entry_by_id(r"HKCU64\Seek-test-no-such-key-5f3a").is_none());
}

#[test]
fn the_same_app_in_both_hklm_views_is_listed_once_from_the_64_bit_view() {
    let mut a32 = entry("Both");
    a32.view = "32".into();
    a32.display_version = "1.0-32".into();
    let mut a64 = entry("Both");
    a64.display_version = "1.0-64".into();
    for order in [vec![a32.clone(), a64.clone()], vec![a64.clone(), a32.clone()]] {
        let shown = build(order);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].version, "1.0-64");
        assert_eq!(shown[0].id, r"HKLM64\{Both}");
    }
    // Per-user and per-machine copies are different registrations.
    let mut user = a64.clone();
    user.hive = "HKCU".into();
    assert_eq!(build(vec![a64, user]).len(), 2);
}

// ---- running ------------------------------------------------------------------------------------------------------

#[test]
fn running_is_an_exe_inside_the_app_folder() {
    let exes = vec![r"C:\Program Files\Acme\bin\acme.exe".to_string(), r"C:\Program Files\Acme Extra\x.exe".to_string()];
    assert!(is_running(Some(Path::new(r"C:\Program Files\Acme")), &exes));
    assert!(is_running(Some(Path::new(r"c:\program files\ACME\")), &exes));
    assert!(!is_running(Some(Path::new(r"C:\Program Files\Other")), &exes));
    // A folder that merely starts with the same letters is another folder.
    assert!(!is_running(Some(Path::new(r"C:\Program Files\Acme E")), &exes));
    assert!(!is_running(None, &exes));
    assert!(!is_running(Some(Path::new(r"C:\Program Files\Acme")), &[]));

    let mut e = entry("Acme");
    e.install_location = r"C:\Program Files\Acme".into();
    let shown = rows(vec![e], &places(), &exes, &plus_two);
    assert!(shown[0].running);
}

// ---- dates and sizes ------------------------------------------------------------------------------------------------

#[test]
fn install_date_is_local_midnight_in_epoch_milliseconds() {
    // 2024-03-05 00:00 at UTC+2 is 2024-03-04 22:00 UTC.
    assert_eq!(install_date_ms("20240305", &plus_two), Some(1_709_589_600_000));
    assert_eq!(install_date_ms("19800101", &|n| Some(n)), Some(315_532_800_000));
    assert_eq!(install_date_ms(" 20240229 ", &|n| Some(n)), Some(1_709_164_800_000));
}

#[test]
fn a_bad_install_date_is_null_not_a_panic() {
    for bad in ["", "2024", "2024-03-05", "20241301", "20240230", "20230229", "00000000", "20240300", "abcdefgh", "2024030é", "999999999", "-2024030"] {
        assert_eq!(install_date_ms(bad, &|n| Some(n)), None, "{bad:?}");
    }
    assert_eq!(install_date_ms("20240305", &|_| None), None);
}

#[test]
fn civil_date_arithmetic_round_trips() {
    for days in [-1000i64, 0, 59, 60, 365, 11_017, 19_782, 47_482] {
        let (y, m, d) = civil_from_days(days);
        assert_eq!(days_from_civil(y, m, d), days);
    }
}

#[test]
fn the_real_local_time_conversion_is_within_a_day_of_the_naive_one() {
    let naive = days_from_civil(2024, 7, 1) * DAY_MS;
    let utc = local_to_utc_ms(naive).expect("the time zone can be read");
    assert!((utc - naive).abs() <= 15 * 3_600_000);
}

#[test]
fn size_comes_from_kb_and_a_missing_one_is_zero() {
    let mut e = entry("Sized");
    e.estimated_size_kb = Some(2048);
    let mut none = entry("Unsized");
    none.estimated_size_kb = None;
    let shown = build(vec![e, none]);
    assert_eq!(shown[0].size, 2048 * 1024);
    assert_eq!(shown[1].size, 0);
    assert_eq!(size_bytes(Some(u64::MAX)), u64::MAX);
}

// ---- icons ------------------------------------------------------------------------------------------------------------

#[test]
fn icon_locations_are_split_into_path_and_index() {
    assert_eq!(split_icon(r"C:\x\app.exe,2"), Some((r"C:\x\app.exe".into(), 2)));
    assert_eq!(split_icon(r#""C:\Program Files\x\app.exe",-5"#), Some((r"C:\Program Files\x\app.exe".into(), -5)));
    assert_eq!(split_icon(r"C:\x\app.ico"), Some((r"C:\x\app.ico".into(), 0)));
    assert_eq!(split_icon(r"C:\a,b\app.ico"), Some((r"C:\a,b\app.ico".into(), 0)));
    assert_eq!(split_icon("app.exe,0"), None);
    assert_eq!(split_icon(""), None);
    let mut e = entry("App");
    assert_eq!(icon_source(&e), Some((r"C:\Program Files\Acme\unins000.exe".into(), 0)));
    e.display_icon = r"C:\x\app.exe,3".into();
    assert_eq!(icon_source(&e), Some((r"C:\x\app.exe".into(), 3)));
    e.display_icon.clear();
    e.uninstall_string = "MsiExec.exe /X{A-B}".into();
    assert_eq!(icon_source(&e), None);
}

#[test]
fn a_cached_icon_is_not_extracted_again_until_the_file_changes() {
    use std::cell::Cell;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.exe");
    std::fs::write(&file, b"one").unwrap();
    let path = file.to_str().unwrap();
    let cache = IconCache::default();
    let calls = Cell::new(0);
    let extract = |_: &str, _: i32| {
        calls.set(calls.get() + 1);
        Some(format!("icon{}", calls.get()))
    };
    assert_eq!(cache.get(path, 0, extract).as_deref(), Some("icon1"));
    assert_eq!(cache.get(path, 0, extract).as_deref(), Some("icon1"));
    assert_eq!(calls.get(), 1);
    // Another index of the same file is another icon.
    assert_eq!(cache.get(path, 1, extract).as_deref(), Some("icon2"));
    // A changed file (new modified time) is asked again.
    let f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(120)).unwrap();
    drop(f);
    assert_eq!(cache.get(path, 0, extract).as_deref(), Some("icon3"));
    assert_eq!(calls.get(), 3);
}

#[test]
fn a_failed_icon_is_remembered_as_none() {
    let cache = IconCache::default();
    let mut calls = 0;
    for _ in 0..3 {
        assert_eq!(
            cache.get(r"C:\no\such\file.exe", 0, |_, _| {
                calls += 1;
                None
            }),
            None
        );
    }
    assert_eq!(calls, 1);
}

#[test]
fn a_missing_file_has_no_icon_and_a_real_exe_gives_a_png() {
    assert_eq!(icons_win::extract_png(r"C:\no\such\file-7d1e.exe", 0), None);
    assert_eq!(icons_win::extract_png(r"\\nohost\share\a.exe", 0), None);
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let url = icons_win::extract_png(&format!(r"{windir}\System32\notepad.exe"), 0).expect("notepad has an icon");
    assert!(url.starts_with("data:image/png;base64,"));
    assert!(url.len() < 32 * 1024);
}

// ---- this PC -------------------------------------------------------------------------------------------------------------

/// Read-only. `cargo test -p seek-clean real_app_list -- --ignored --nocapture`.
#[test]
#[ignore]
fn real_app_list() {
    use std::fmt::Write as _;
    let t = std::time::Instant::now();
    let cold_rows = try_list().expect("the app list can be read");
    let cold = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let warm_rows = try_list().expect("the app list can be read");
    let warm = t.elapsed().as_millis();
    assert_eq!(cold_rows.len(), warm_rows.len());

    let no_folder = warm_rows.iter().filter(|r| r.folder.is_none()).count();
    let no_icon = warm_rows.iter().filter(|r| r.icon.is_none()).count();
    let running = warm_rows.iter().filter(|r| r.running).count();
    let seek = warm_rows.iter().any(|r| r.name.trim().eq_ignore_ascii_case("seek"));
    let mut out = String::new();
    let _ = writeln!(out, "apps listed: {}", warm_rows.len());
    let _ = writeln!(out, "cold (icons extracted): {cold} ms");
    let _ = writeln!(out, "warm (icons cached): {warm} ms");
    let _ = writeln!(out, "no folder: {no_folder}");
    let _ = writeln!(out, "no icon: {no_icon}");
    let _ = writeln!(out, "running: {running}");
    let _ = writeln!(out, "Seek appears: {seek}");
    let _ = writeln!(out, "\nfirst 15 rows (name | publisher | version | size | folder | note):");
    for r in warm_rows.iter().take(15) {
        let _ = writeln!(
            out,
            "{} | {} | {} | {} | {} | {}",
            r.name,
            r.publisher,
            r.version,
            r.size,
            r.folder.as_deref().unwrap_or("-"),
            r.note.as_deref().unwrap_or("-")
        );
    }
    println!("{out}");
    std::fs::write(r"D:\Programming\seek\.superpowers\sdd\2026-11-05-seek-decimator\task-12-real-list.txt", out).unwrap();
}
